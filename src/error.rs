//! Error types.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::retry::parse_retry_after;

/// Header carrying the server's request ID.
pub const REQUEST_ID_HEADER: &str = "x-typesafe-request-id";

/// Longest raw body quoted in an [`ApiError`] message, in characters.
const MAX_RAW_BODY_IN_MESSAGE: usize = 200;

/// A `Result` whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything a client call can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The client configuration is missing or invalid.
    #[error("{0}")]
    Config(String),

    /// The request was rejected before it was sent.
    #[error("{0}")]
    InvalidRequest(String),

    /// The server answered with a non-2xx status after retries.
    #[error(transparent)]
    Api(Box<ApiError>),

    /// The request or the response body failed in transit (DNS, TLS, reset).
    #[error("Connection error: {source}")]
    Connection {
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },

    /// The full response did not arrive within the per-attempt timeout.
    #[error("Request timed out after {}ms.", .timeout.as_millis())]
    Timeout {
        /// The per-attempt timeout that elapsed.
        timeout: Duration,
    },

    /// A 2xx response body did not have the expected shape.
    #[error("{message}")]
    Decode {
        /// What was wrong with the body.
        message: String,
        /// Request ID from `x-typesafe-request-id`.
        request_id: Option<String>,
        /// The JSON parse error, when there was one.
        #[source]
        source: Option<serde_json::Error>,
    },

    /// The credential provider failed, timed out, or returned an unusable token.
    ///
    /// The message never includes the token. Not retried.
    #[error("Could not get credentials: {source}")]
    Credentials {
        /// What went wrong.
        #[source]
        source: crate::credentials::BoxError,
    },

    /// An answer is missing, has another type, or uses a label the typed key doesn't know.
    #[error("Unexpected answer \"{name}\": {reason}.")]
    UnexpectedAnswer {
        /// The question name.
        name: String,
        /// What didn't match.
        reason: String,
    },
}

impl Error {
    /// The HTTP status, for [`Error::Api`].
    pub fn status(&self) -> Option<StatusCode> {
        self.api_error().map(ApiError::status)
    }

    /// The server's request ID, when the response carried one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Api(err) => err.request_id(),
            Error::Decode { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    /// The API error, for [`Error::Api`].
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api(err) => Some(err),
            _ => None,
        }
    }

    /// Whether the call timed out.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout { .. })
    }

    /// Whether the call failed in transit. Timeouts count as connection errors.
    pub fn is_connection(&self) -> bool {
        matches!(self, Error::Connection { .. } | Error::Timeout { .. })
    }
}

impl From<ApiError> for Error {
    fn from(err: ApiError) -> Self {
        Error::Api(Box::new(err))
    }
}

/// The class of an unsuccessful HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApiErrorKind {
    /// HTTP 400: the request is invalid.
    BadRequest,
    /// HTTP 401: authentication failed.
    Authentication,
    /// HTTP 403: access is denied.
    PermissionDenied,
    /// HTTP 404: the resource was not found.
    NotFound,
    /// HTTP 422: request validation failed.
    UnprocessableEntity,
    /// HTTP 429: the rate limit was exceeded.
    RateLimit,
    /// HTTP 529: TypeSafe is temporarily overloaded. Documented as "retry after
    /// a short delay", which the default policy already does.
    Overloaded,
    /// HTTP 5xx: the server failed to handle the request.
    InternalServer,
    /// Any other non-2xx status.
    Other,
}

impl ApiErrorKind {
    /// Classify an HTTP status.
    pub fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            400 => Self::BadRequest,
            401 => Self::Authentication,
            403 => Self::PermissionDenied,
            404 => Self::NotFound,
            422 => Self::UnprocessableEntity,
            429 => Self::RateLimit,
            529 => Self::Overloaded,
            500.. => Self::InternalServer,
            _ => Self::Other,
        }
    }
}

/// A response body kept for diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorBody {
    /// The body was empty.
    Empty,
    /// The body parsed as JSON.
    Json(Value),
    /// The body was not JSON.
    Text(String),
}

impl ErrorBody {
    /// Parse a body leniently: JSON when it parses, text otherwise.
    ///
    /// The content type is ignored because servers and proxies don't always set it.
    pub fn parse(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::Empty;
        }
        match serde_json::from_slice(bytes) {
            Ok(value) => Self::Json(value),
            Err(_) => Self::Text(String::from_utf8_lossy(bytes).into_owned()),
        }
    }
}

/// An unsuccessful HTTP response from the API.
#[derive(Debug, Clone)]
pub struct ApiError {
    kind: ApiErrorKind,
    status: StatusCode,
    headers: HeaderMap,
    body: ErrorBody,
    request_id: Option<String>,
    message: String,
}

impl ApiError {
    /// Build an error from a raw response, deriving its kind and message.
    pub fn from_response(status: StatusCode, headers: HeaderMap, body: &[u8]) -> Self {
        let body = ErrorBody::parse(body);
        let message = describe(status, &body);
        Self {
            kind: ApiErrorKind::from_status(status),
            request_id: request_id_from(&headers),
            status,
            headers,
            body,
            message,
        }
    }

    /// The class of the status code.
    pub fn kind(&self) -> ApiErrorKind {
        self.kind
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The parsed response body.
    pub fn body(&self) -> &ErrorBody {
        &self.body
    }

    /// The request ID from `x-typesafe-request-id`.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// The server's retry delay from `retry-after-ms` or `Retry-After`.
    ///
    /// Usually present on [`ApiErrorKind::RateLimit`], sometimes on 503.
    pub fn retry_after(&self) -> Option<Duration> {
        parse_retry_after(&self.headers, std::time::SystemTime::now())
    }

    /// The error message, `"<status> <detail>"`.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub(crate) fn request_id_from(headers: &HeaderMap) -> Option<String> {
    [REQUEST_ID_HEADER, "cf-ray", "x-request-id", "x-vercel-id"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `"<status> <detail>"`, falling back to a truncated raw body.
fn describe(status: StatusCode, body: &ErrorBody) -> String {
    let status = status.as_u16();
    // An empty message is not a message; quote the body instead.
    if let Some(detail) = extract_message(body).filter(|detail| !detail.is_empty()) {
        return format!("{status} {detail}");
    }
    let raw = match body {
        ErrorBody::Empty => return format!("{status} status code (no body)"),
        ErrorBody::Text(text) | ErrorBody::Json(Value::String(text)) => text.clone(),
        ErrorBody::Json(value) => value.to_string(),
    };
    match raw.char_indices().nth(MAX_RAW_BODY_IN_MESSAGE) {
        Some((cut, _)) => format!("{status} {}…", &raw[..cut]),
        None => format!("{status} {raw}"),
    }
}

/// Pull a message out of a text, error, or validation response body.
fn extract_message(body: &ErrorBody) -> Option<String> {
    let value = match body {
        ErrorBody::Empty => return None,
        ErrorBody::Text(text) => return non_empty(text),
        ErrorBody::Json(value) => value,
    };
    if let Value::String(text) = value {
        return non_empty(text);
    }
    let object = value.as_object()?;
    let error = object.get("error");
    let detail = object.get("detail");
    if let Some(Value::String(text)) = error {
        return Some(text.clone());
    }
    if let Some(text) = error.and_then(Value::as_object).and_then(nested_message) {
        return Some(text);
    }
    if let Some(Value::String(text)) = object.get("message") {
        return Some(text.clone());
    }
    match detail {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(inner)) => nested_message(inner),
        Some(Value::Array(errors)) => describe_validation_errors(errors),
        _ => None,
    }
}

/// The readable part of a nested error object.
///
/// `message` when the server wrote one. Otherwise `error_type`, which is all
/// TypeSafe sends for some failures: an oversized request answers with
/// `{"detail":{"error_type":"max_tokens_exceeded"}}`, and quoting that JSON at
/// the caller is worse than naming the error.
fn nested_message(object: &serde_json::Map<String, Value>) -> Option<String> {
    ["message", "error_type"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .and_then(non_empty)
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// Format validation errors as `path: message` entries joined by `"; "`.
fn describe_validation_errors(errors: &[Value]) -> Option<String> {
    let parts: Vec<String> = errors
        .iter()
        .filter_map(|error| {
            let msg = error.get("msg")?.as_str()?;
            let loc = error
                .get("loc")
                .and_then(Value::as_array)
                .map(|segments| {
                    segments
                        .iter()
                        .filter(|segment| segment.as_str() != Some("body"))
                        .map(loc_segment)
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            Some(if loc.is_empty() {
                msg.to_owned()
            } else {
                format!("{loc}: {msg}")
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn loc_segment(segment: &Value) -> String {
    match segment {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    fn message(status: u16, body: &[u8]) -> String {
        ApiError::from_response(
            StatusCode::from_u16(status).unwrap(),
            HeaderMap::new(),
            body,
        )
        .to_string()
    }

    #[test]
    fn classifies_statuses() {
        let kind = |s| ApiErrorKind::from_status(StatusCode::from_u16(s).unwrap());
        assert_eq!(kind(400), ApiErrorKind::BadRequest);
        assert_eq!(kind(401), ApiErrorKind::Authentication);
        assert_eq!(kind(403), ApiErrorKind::PermissionDenied);
        assert_eq!(kind(404), ApiErrorKind::NotFound);
        assert_eq!(kind(422), ApiErrorKind::UnprocessableEntity);
        assert_eq!(kind(429), ApiErrorKind::RateLimit);
        assert_eq!(kind(500), ApiErrorKind::InternalServer);
        assert_eq!(kind(529), ApiErrorKind::Overloaded);
        assert_eq!(kind(599), ApiErrorKind::InternalServer);
        assert_eq!(kind(409), ApiErrorKind::Other);
        assert_eq!(kind(302), ApiErrorKind::Other);
    }

    #[test]
    fn extracts_messages_from_common_shapes() {
        assert_eq!(message(400, br#"{"error":"bad"}"#), "400 bad");
        assert_eq!(
            message(400, br#"{"error":{"message":"nested"}}"#),
            "400 nested"
        );
        assert_eq!(message(400, br#"{"message":"plain"}"#), "400 plain");
        assert_eq!(message(400, br#"{"detail":"why"}"#), "400 why");
        assert_eq!(
            message(400, br#"{"detail":{"message":"deep"}}"#),
            "400 deep"
        );
        assert_eq!(message(400, br#""quoted""#), "400 quoted");
        assert_eq!(message(502, b"Bad Gateway"), "502 Bad Gateway");
    }

    /// The live API answers an oversized request with this exact body, and the
    /// error_type is the only thing in it worth reading.
    #[test]
    fn error_type_is_used_when_there_is_no_message() {
        assert_eq!(
            message(400, br#"{"detail":{"error_type":"max_tokens_exceeded"}}"#),
            "400 max_tokens_exceeded"
        );
        assert_eq!(
            message(400, br#"{"error":{"error_type":"rate_limited"}}"#),
            "400 rate_limited"
        );
        // A message still wins when both are present.
        assert_eq!(
            message(
                400,
                br#"{"detail":{"error_type":"bad","message":"be specific"}}"#
            ),
            "400 be specific"
        );
    }

    #[test]
    fn prefers_error_over_message_and_detail() {
        assert_eq!(
            message(400, br#"{"detail":"d","message":"m","error":"e"}"#),
            "400 e"
        );
        assert_eq!(message(400, br#"{"detail":"d","message":"m"}"#), "400 m");
    }

    #[test]
    fn formats_validation_errors_without_the_body_prefix() {
        let body = json!({"detail": [
            {"loc": ["body", "questions", "x", 0], "msg": "too short"},
            {"loc": [], "msg": "bad state"},
            {"msg": 3},
        ]});
        assert_eq!(
            message(422, body.to_string().as_bytes()),
            "422 questions.x.0: too short; bad state"
        );
    }

    #[test]
    fn falls_back_to_raw_body_or_no_body() {
        assert_eq!(message(500, b""), "500 status code (no body)");
        assert_eq!(message(500, br#"{"other":1}"#), r#"500 {"other":1}"#);
        assert_eq!(message(500, br#"{"detail":[]}"#), r#"500 {"detail":[]}"#);
    }

    #[test]
    fn empty_extracted_messages_fall_back_to_the_raw_body() {
        // The first matching field wins even when empty, like the JS SDK, and an
        // empty detail is never used as the message.
        assert_eq!(
            message(400, br#"{"error":"","message":"m"}"#),
            r#"400 {"error":"","message":"m"}"#
        );
        assert_eq!(message(400, br#"{"message":""}"#), r#"400 {"message":""}"#);
        assert_eq!(
            message(400, br#"{"error":{"message":""}}"#),
            r#"400 {"error":{"message":""}}"#
        );
        assert_eq!(message(400, br#"{"detail":""}"#), r#"400 {"detail":""}"#);
    }

    #[test]
    fn json_string_bodies_are_not_quoted() {
        assert_eq!(message(400, br#""""#), "400 ");
    }

    #[test]
    fn truncates_long_json_bodies_on_char_boundaries() {
        let body = json!({ "x": "é".repeat(250) }).to_string();
        let expected: String = body.chars().take(200).collect();
        assert_eq!(message(500, body.as_bytes()), format!("500 {expected}…"));

        let exact = json!({ "x": "a".repeat(192) }).to_string();
        assert_eq!(exact.chars().count(), 200);
        assert_eq!(message(500, exact.as_bytes()), format!("500 {exact}"));
    }

    #[test]
    fn text_bodies_are_the_message_in_full() {
        let long = "é".repeat(250);
        assert_eq!(message(502, long.as_bytes()), format!("502 {long}"));
    }

    #[test]
    fn exposes_request_id_and_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_1"));
        headers.insert("retry-after", HeaderValue::from_static("7"));
        let err = ApiError::from_response(StatusCode::TOO_MANY_REQUESTS, headers, b"{}");
        assert_eq!(err.request_id(), Some("req_1"));
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(err.kind(), ApiErrorKind::RateLimit);

        let wrapped = Error::from(err);
        assert_eq!(wrapped.request_id(), Some("req_1"));
        assert_eq!(wrapped.status(), Some(StatusCode::TOO_MANY_REQUESTS));
        assert!(!wrapped.is_connection());
    }

    #[test]
    fn timeouts_are_connection_errors() {
        let err = Error::Timeout {
            timeout: Duration::from_millis(1000),
        };
        assert!(err.is_timeout());
        assert!(err.is_connection());
        assert_eq!(err.to_string(), "Request timed out after 1000ms.");
    }
}
