//! Sending requests: headers, timeouts, retries and logging.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::Instrument;
use tracing::field::Empty;

use bytes::Bytes;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use reqwest::{Method, StatusCode};

use super::Client;
use super::call::RawResponse;
use crate::error::{ApiError, Error, Result, request_id_from};
use crate::redact::redact_headers;
use crate::retry::RetryPolicy;

const SDK_HEADER: &str = "x-typesafe-sdk";
const RUNTIME_HEADER: &str = "x-typesafe-runtime";
const RETRY_COUNT_HEADER: &str = "x-typesafe-retry-count";

/// A call with every option resolved against the client.
pub(crate) struct Resolved {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) body: Option<Bytes>,
    pub(crate) headers: HeaderMap,
    pub(crate) timeout: Duration,
    pub(crate) retry: RetryPolicy,
    pub(crate) total_timeout: Option<Duration>,
}

fn sdk_identity() -> HeaderValue {
    HeaderValue::from_static(concat!(
        env!("CARGO_PKG_NAME"),
        "/",
        env!("CARGO_PKG_VERSION")
    ))
}

fn runtime_description() -> HeaderValue {
    let runtime = format!(
        "rust ({}; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    HeaderValue::from_str(&runtime).unwrap_or_else(|_| HeaderValue::from_static("rust"))
}

impl Client {
    /// Send a resolved call, retrying eligible failures.
    ///
    /// Each call runs in a `typesafe.request` span with OpenTelemetry-style fields.
    /// Attempt summaries log at `info`; redacted headers, and bodies when enabled, at `debug`.
    pub(crate) async fn execute(&self, req: Resolved) -> Result<RawResponse> {
        let number = self.inner.request_count.fetch_add(1, Ordering::Relaxed) + 1;
        let span = tracing::info_span!(
            "typesafe.request",
            otel.kind = "client",
            http.request.method = %req.method,
            url.path = req.path.as_str(),
            typesafe.request.number = number,
            http.request.resend_count = Empty,
            http.response.status_code = Empty,
            typesafe.request_id = Empty,
            error.type = Empty,
        );
        let result = self
            .execute_attempts(req, number)
            .instrument(span.clone())
            .await;
        if let Err(err) = &result {
            span.record("error.type", error_type(err));
        }
        result
    }

    async fn execute_attempts(&self, mut req: Resolved, number: u64) -> Result<RawResponse> {
        let inner = &self.inner;
        let tag = format!("#{number} {} {}", req.method, req.path);
        let url = format!("{}{}", inner.base_url, req.path);
        let span = tracing::Span::current();
        let deadline = req.total_timeout.map(|total| Instant::now() + total);

        // Caller headers go first so they can't clobber auth or the JSON content type.
        let mut headers = std::mem::take(&mut req.headers);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, sdk_identity());
        headers.insert(HeaderName::from_static(SDK_HEADER), sdk_identity());
        headers.insert(
            HeaderName::from_static(RUNTIME_HEADER),
            runtime_description(),
        );
        headers.remove(RETRY_COUNT_HEADER);
        if req.body.is_some() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        } else {
            headers.remove(CONTENT_TYPE);
        }

        // The previous attempt's error, returned when the total timeout ends a retry early:
        // "503, then out of time" says more than a bare timeout.
        let mut previous: Option<Error> = None;
        let total = req.total_timeout.unwrap_or(req.timeout);
        let mut attempt: u32 = 0;
        loop {
            let retries_left = req.retry.max_retries - attempt;
            if attempt > 0 {
                span.record("http.request.resend_count", attempt);
            }
            let Ok(slot) =
                acquire_slot(inner.limiter.as_ref().map(|(slots, _)| slots), deadline).await
            else {
                return Err(previous.unwrap_or(Error::Timeout { timeout: total }));
            };
            let Some(budget) = attempt_budget(req.timeout, deadline) else {
                return Err(previous.unwrap_or(Error::Timeout { timeout: total }));
            };

            // Asked per attempt so a provider can refresh an expiring token between retries.
            let authorization = inner.credentials.authorization(budget).await?;
            // The provider's time comes out of the total budget.
            let Some(timeout) = attempt_budget(req.timeout, deadline) else {
                return Err(previous.unwrap_or(Error::Timeout { timeout: total }));
            };
            let mut attempt_headers = headers.clone();
            attempt_headers.insert(AUTHORIZATION, authorization);
            if attempt > 0 {
                attempt_headers.insert(
                    HeaderName::from_static(RETRY_COUNT_HEADER),
                    HeaderValue::from(attempt),
                );
            }
            if inner.log_bodies {
                tracing::debug!(
                    attempt,
                    headers = ?redact_headers(&attempt_headers),
                    body = %req.body.as_deref().map(String::from_utf8_lossy).unwrap_or_default(),
                    "{tag} -> {url}",
                );
            } else {
                tracing::debug!(attempt, headers = ?redact_headers(&attempt_headers), "{tag} -> {url}");
            }

            let started = Instant::now();
            let outcome = self
                .attempt(&tag, &url, &req, attempt_headers, timeout)
                .await;
            // Free the slot before any backoff so waiting calls can use it.
            drop(slot);
            let (status, response_headers, body) = match outcome {
                Ok(response) => response,
                Err(err) => {
                    let cut_by_total = err.is_timeout() && timeout < req.timeout;
                    if cut_by_total && let Some(previous) = previous {
                        return Err(previous);
                    }
                    if retries_left == 0 || !req.retry.is_retryable_error(&err) {
                        return Err(err);
                    }
                    let delay = req.retry.delay(attempt, None, fastrand::f64());
                    if !fits_before(deadline, delay) {
                        return Err(err);
                    }
                    backoff(&tag, attempt, retries_left, &err.to_string(), delay).await;
                    previous = Some(err);
                    attempt += 1;
                    continue;
                }
            };

            let request_id = request_id_from(&response_headers);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            tracing::info!(
                attempt,
                http.response.status_code = status.as_u16(),
                elapsed_ms,
                typesafe.request_id = request_id.as_deref(),
                "{tag} <- {} in {elapsed_ms}ms{}",
                status.as_u16(),
                request_id
                    .as_deref()
                    .map(|id| format!(" (request {id})"))
                    .unwrap_or_default(),
            );
            if inner.log_bodies {
                tracing::debug!(body = %String::from_utf8_lossy(&body), "{tag} <- body");
            }

            if status.is_success() {
                record_response(&span, status, request_id.as_deref());
                return Ok(RawResponse {
                    status,
                    headers: response_headers,
                    body,
                    request_id,
                });
            }

            let error_body = if matches!(self.provider(), crate::Provider::Cloudflare { .. }) {
                &b"{}"[..]
            } else {
                &body
            };
            let error = ApiError::from_response(status, response_headers, error_body);
            let error = Error::from(error);
            if retries_left == 0
                || !(req.retry.is_retryable_status(status.as_u16())
                    || req.retry.asks_to_retry(&error))
            {
                record_response(&span, status, request_id.as_deref());
                return Err(error);
            }
            let headers = error
                .api_error()
                .map(ApiError::headers)
                .cloned()
                .unwrap_or_default();
            let delay = req.retry.delay(attempt, Some(&headers), fastrand::f64());
            if !fits_before(deadline, delay) {
                record_response(&span, status, request_id.as_deref());
                return Err(error);
            }
            let reason = status.as_u16().to_string();
            backoff(&tag, attempt, retries_left, &reason, delay).await;
            previous = Some(error);
            attempt += 1;
        }
    }

    /// One round trip, including the full body, bounded by `timeout`.
    async fn attempt(
        &self,
        tag: &str,
        url: &str,
        req: &Resolved,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<(StatusCode, HeaderMap, Bytes)> {
        let mut builder = self
            .inner
            .http
            .request(req.method.clone(), url)
            .headers(headers);
        if let Some(body) = &req.body {
            builder = builder.body(body.clone());
        }
        let round_trip = async {
            let response = builder.send().await?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.bytes().await?;
            Ok::<_, reqwest::Error>((status, headers, body))
        };

        let started = Instant::now();
        match tokio::time::timeout(timeout, round_trip).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(source)) => {
                tracing::info!(
                    error.type = "connection",
                    "{tag} connection error after {}ms: {source}",
                    started.elapsed().as_millis()
                );
                Err(Error::Connection { source })
            }
            Err(_) => {
                tracing::info!(
                    error.type = "timeout",
                    "{tag} timed out after {}ms",
                    started.elapsed().as_millis()
                );
                Err(Error::Timeout { timeout })
            }
        }
    }
}

/// The timeout for the next attempt: the per-attempt timeout, cut to what is left of the
/// total. `None` once the total timeout has passed.
fn attempt_budget(per_attempt: Duration, deadline: Option<Instant>) -> Option<Duration> {
    let Some(deadline) = deadline else {
        return Some(per_attempt);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then(|| per_attempt.min(remaining))
}

/// Wait for a concurrency slot when the client has a limit. `Err` when the total timeout
/// passes first.
async fn acquire_slot(
    limiter: Option<&Semaphore>,
    deadline: Option<Instant>,
) -> std::result::Result<Option<SemaphorePermit<'_>>, ()> {
    let Some(limiter) = limiter else {
        return Ok(None);
    };
    let permit = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), limiter.acquire())
            .await
            .map_err(|_| ())?,
        None => limiter.acquire().await,
    };
    Ok(Some(
        permit.expect("the concurrency limiter is never closed"),
    ))
}

/// Record the response that ends the call. Retried responses are not recorded, so a call
/// that ends in a timeout doesn't carry an earlier attempt's status.
fn record_response(span: &tracing::Span, status: StatusCode, request_id: Option<&str>) {
    span.record("http.response.status_code", status.as_u16());
    if let Some(id) = request_id {
        span.record("typesafe.request_id", id);
    }
}

/// Whether waiting `delay` still leaves time for another attempt before the deadline.
fn fits_before(deadline: Option<Instant>, delay: Duration) -> bool {
    deadline.is_none_or(|deadline| Instant::now() + delay < deadline)
}

/// A low-cardinality `error.type` value for a failed call.
fn error_type(err: &Error) -> String {
    match err {
        Error::Api(api) => api.status().as_u16().to_string(),
        Error::Timeout { .. } => "timeout".into(),
        Error::Connection { .. } => "connection".into(),
        Error::Credentials { .. } => "credentials".into(),
        Error::Decode { .. } => "decode".into(),
        Error::Config(_) => "config".into(),
        Error::InvalidRequest(_) => "invalid_request".into(),
        Error::UnexpectedAnswer { .. } => "unexpected_answer".into(),
    }
}

async fn backoff(tag: &str, attempt: u32, retries_left: u32, reason: &str, delay: Duration) {
    tracing::info!(
        attempt,
        delay_ms = delay.as_millis() as u64,
        "{tag} retrying in {}ms (retry {}/{}) after {reason}",
        delay.as_millis(),
        attempt + 1,
        attempt + retries_left,
    );
    tokio::time::sleep(delay).await;
}
