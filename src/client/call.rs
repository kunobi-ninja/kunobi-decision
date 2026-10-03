//! A pending API call with per-call options.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use super::Client;
use super::builder::validate_timeout;
use super::transport::Resolved;
use crate::error::{Error, Result};
use crate::retry::RetryPolicy;

/// Why a 2xx body could not be turned into the expected value.
pub(crate) struct DecodeFailure {
    pub(crate) message: String,
    pub(crate) source: Option<serde_json::Error>,
}

pub(crate) type Parser<T> = fn(&[u8]) -> std::result::Result<T, DecodeFailure>;

pub(crate) fn parse_json<T: DeserializeOwned>(
    body: &[u8],
) -> std::result::Result<T, DecodeFailure> {
    serde_json::from_slice(body).map_err(|err| DecodeFailure {
        message: format!("Could not parse the response body: {err}"),
        source: Some(err),
    })
}

/// A pending call. Await it for the parsed result, or set per-call options first.
///
/// Nothing is sent until the call is awaited. Dropping the future cancels the
/// request and any pending retry.
///
/// ```no_run
/// # async fn example(client: kunobi_decision::Client) -> kunobi_decision::Result<()> {
/// use std::time::Duration;
///
/// let models = client
///     .models()
///     .list()
///     .timeout(Duration::from_secs(2))
///     .max_retries(0)
///     .with_response()
///     .await?;
/// println!("{:?} {:?}", models.request_id, models.data);
/// # Ok(()) }
/// ```
#[must_use = "a call does nothing until it is awaited"]
pub struct Call<T> {
    client: Client,
    method: Method,
    path: String,
    body: Result<Option<Bytes>>,
    timeout: Option<Duration>,
    /// Outer `None` means "not set, use the client's"; inner `None` means this
    /// call removed the bound.
    total_timeout: Option<Option<Duration>>,
    retry: Option<RetryPolicy>,
    max_retries: Option<u32>,
    headers: HeaderMap,
    parse: Parser<T>,
}

/// Parsed data with its HTTP status, headers and request ID.
#[derive(Debug, Clone)]
pub struct WithResponse<T> {
    /// The parsed response body.
    pub data: T,
    /// The HTTP status.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// Request ID from `x-typesafe-request-id`.
    pub request_id: Option<String>,
}

/// A successful response with its body unparsed.
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// The HTTP status, always 2xx.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The full response body.
    pub body: Bytes,
    /// Request ID from `x-typesafe-request-id`.
    pub request_id: Option<String>,
}

impl<T> Call<T> {
    pub(crate) fn new(
        client: Client,
        method: Method,
        path: impl Into<String>,
        body: Result<Option<Vec<u8>>>,
        parse: Parser<T>,
    ) -> Self {
        Self {
            client,
            method,
            path: path.into(),
            body: body.map(|body| body.map(Bytes::from)),
            timeout: None,
            total_timeout: None,
            retry: None,
            max_retries: None,
            headers: HeaderMap::new(),
            parse,
        }
    }

    /// Timeout per attempt for this call.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Upper bound for the whole call, including credentials, retries and backoff.
    /// Pass `None` to remove the client's bound for this call.
    ///
    /// Attempts are shortened to fit the time left. A retry is skipped when its backoff
    /// would end past the bound, and a retry cut short by the bound returns the
    /// previous attempt's error, such as the 503 that caused the retry.
    pub fn total_timeout(mut self, total_timeout: impl Into<Option<Duration>>) -> Self {
        self.total_timeout = Some(total_timeout.into());
        self
    }

    /// Retry policy for this call, replacing the client's.
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Retries after the initial attempt for this call; `0` disables retries.
    ///
    /// Applies on top of the client's policy or the one set with [`Call::retry`].
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Add a header for this call, replacing a default header with the same name.
    ///
    /// Authentication, content negotiation and SDK headers cannot be overridden.
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Add headers for this call, replacing earlier values for the same names.
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Send the call and return the body without parsing it.
    pub async fn raw(self) -> Result<RawResponse> {
        let client = self.client.clone();
        let resolved = self.resolve()?;
        client.execute(resolved).await
    }

    /// Send the call and return the parsed result with response metadata.
    pub async fn with_response(self) -> Result<WithResponse<T>> {
        let parse = self.parse;
        let raw = self.raw().await?;
        match parse(&raw.body) {
            Ok(data) => Ok(WithResponse {
                data,
                status: raw.status,
                headers: raw.headers,
                request_id: raw.request_id,
            }),
            Err(failure) => Err(Error::Decode {
                message: failure.message,
                request_id: raw.request_id,
                source: failure.source,
            }),
        }
    }

    fn resolve(self) -> Result<Resolved> {
        let body = self.body?;
        let timeout = validate_timeout(self.timeout.unwrap_or(self.client.timeout()))?;
        let total_timeout = match self.total_timeout.unwrap_or(self.client.total_timeout()) {
            Some(total) => Some(validate_timeout(total).map_err(|_| {
                Error::Config("`total_timeout` must be a positive duration, got 0.".into())
            })?),
            None => None,
        };
        let mut retry = self.retry.unwrap_or_else(|| self.client.retry().clone());
        if let Some(max_retries) = self.max_retries {
            retry.max_retries = max_retries;
        }
        retry.validate()?;

        let mut headers = self.client.default_headers().clone();
        headers.extend(self.headers);
        Ok(Resolved {
            method: self.method,
            path: self.path,
            body,
            headers,
            timeout,
            retry,
            total_timeout,
        })
    }
}

impl<T: Send + 'static> IntoFuture for Call<T> {
    type Output = Result<T>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move { self.with_response().await.map(|response| response.data) })
    }
}
