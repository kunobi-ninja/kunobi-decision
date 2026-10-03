//! Retry policy, delay calculation and `Retry-After` parsing.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::header::HeaderMap;

use crate::error::{Error, Result};

/// Default timeout per attempt.
///
/// Jev answers fast: measured against the live API, a three-question request
/// took 292 ms at the median and 728 ms at the worst of twenty calls, network
/// included. Five seconds is roughly nine times the p90, enough for a cold
/// connection or a bad network, and short enough that a stuck attempt is
/// retried instead of holding the caller for ten seconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default upper bound for a whole call, retries and backoff included.
///
/// Ten seconds is two attempts and the backoff between them: one full attempt,
/// 500 ms, then a second cut to what is left. A decision this model answers in
/// under a second should not keep a caller waiting half a minute because a
/// retry ladder ran to its end.
///
/// The trade-off is rate limits: a 429 whose `Retry-After` is longer than the
/// budget now returns the error instead of waiting it out. That is what an
/// interactive caller wants. Batch work should raise both this and
/// [`RetryPolicy::max_retry_after`], or remove the bound with
/// `total_timeout(None)`.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A caller's rule for retrying an error the built-in ones decline.
pub type RetryPredicate = Arc<dyn Fn(&Error) -> bool + Send + Sync>;

/// When and how to retry a failed attempt.
///
/// To change one field, start from an existing policy:
///
/// ```
/// use kunobi_decision::RetryPolicy;
///
/// let policy = RetryPolicy { max_retries: 5, ..RetryPolicy::default() };
/// ```
#[derive(Clone)]
pub struct RetryPolicy {
    /// Retries after the initial attempt; `0` disables retries. Default: 2.
    pub max_retries: u32,
    /// First backoff delay, doubled on each retry up to `backoff_max`. Default: 500 ms.
    pub backoff_initial: Duration,
    /// Longest backoff delay. Default: 5 s.
    pub backoff_max: Duration,
    /// Fraction of each backoff delay randomly subtracted, from 0 to 1. Default: 0.25.
    pub backoff_jitter: f64,
    /// HTTP statuses to retry. Default: 408, 429 and 500–599.
    pub http_statuses: BTreeSet<u16>,
    /// Honor `retry-after-ms` and `Retry-After` up to `max_retry_after`. Default: true.
    pub respect_retry_after: bool,
    /// Longest server delay to honor; longer ones fall back to backoff. Default: 60 s.
    pub max_retry_after: Duration,
    /// Retry connection failures, including interrupted response bodies. Default: true.
    pub retry_connection_errors: bool,
    /// Retry attempts that timed out. Default: true.
    pub retry_timeouts: bool,
    /// Retry an error the fields above decline. Default: none.
    ///
    /// The fields decide first; this only ever widens what is retried, so a
    /// predicate cannot disable the defaults. Use it for an error only the
    /// caller can classify, such as a specific API message worth another try.
    ///
    /// ```
    /// use kunobi_decision::{Error, RetryPolicy};
    ///
    /// let policy = RetryPolicy::default()
    ///     .retry_if(|error: &Error| error.status().is_some_and(|status| status == 409));
    /// ```
    pub retry_if: Option<RetryPredicate>,
}

impl fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("max_retries", &self.max_retries)
            .field("backoff_initial", &self.backoff_initial)
            .field("backoff_max", &self.backoff_max)
            .field("backoff_jitter", &self.backoff_jitter)
            .field("http_statuses", &self.http_statuses)
            .field("respect_retry_after", &self.respect_retry_after)
            .field("max_retry_after", &self.max_retry_after)
            .field("retry_connection_errors", &self.retry_connection_errors)
            .field("retry_timeouts", &self.retry_timeouts)
            .field("retry_if", &self.retry_if.as_ref().map(|_| "<predicate>"))
            .finish()
    }
}

impl PartialEq for RetryPolicy {
    /// Predicates compare by identity: two closures with the same behaviour are
    /// still two closures, and pretending otherwise would make `assert_eq!` lie.
    fn eq(&self, other: &Self) -> bool {
        self.max_retries == other.max_retries
            && self.backoff_initial == other.backoff_initial
            && self.backoff_max == other.backoff_max
            && self.backoff_jitter == other.backoff_jitter
            && self.http_statuses == other.http_statuses
            && self.respect_retry_after == other.respect_retry_after
            && self.max_retry_after == other.max_retry_after
            && self.retry_connection_errors == other.retry_connection_errors
            && self.retry_timeouts == other.retry_timeouts
            && match (&self.retry_if, &other.retry_if) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(5),
            backoff_jitter: 0.25,
            http_statuses: [408, 429].into_iter().chain(500..600).collect(),
            respect_retry_after: true,
            max_retry_after: Duration::from_secs(60),
            retry_connection_errors: true,
            retry_timeouts: true,
            retry_if: None,
        }
    }
}

impl RetryPolicy {
    /// The default policy with retries disabled.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    /// Reject a jitter outside 0..=1 and statuses outside 100..=999.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.backoff_jitter) {
            return Err(Error::Config(format!(
                "`retry.backoff_jitter` must be between 0 and 1, got {}.",
                self.backoff_jitter
            )));
        }
        if let Some(status) = self
            .http_statuses
            .iter()
            .find(|status| !(100..=999).contains(*status))
        {
            return Err(Error::Config(format!(
                "`retry.http_statuses` must contain HTTP status codes, got {status}."
            )));
        }
        Ok(())
    }

    /// Whether this policy retries an HTTP status.
    pub fn is_retryable_status(&self, status: u16) -> bool {
        self.http_statuses.contains(&status)
    }

    /// Retry errors the built-in rules decline, as decided by `predicate`.
    ///
    /// Builder form of [`RetryPolicy::retry_if`], so a policy can be written in
    /// one expression.
    pub fn retry_if(mut self, predicate: impl Fn(&Error) -> bool + Send + Sync + 'static) -> Self {
        self.retry_if = Some(Arc::new(predicate));
        self
    }

    /// Whether this policy retries a transport error.
    pub(crate) fn is_retryable_error(&self, err: &Error) -> bool {
        let built_in = match err {
            Error::Timeout { .. } => self.retry_timeouts,
            Error::Connection { .. } => self.retry_connection_errors,
            _ => false,
        };
        built_in || self.asks_to_retry(err)
    }

    /// Whether the caller's predicate wants this error retried.
    pub(crate) fn asks_to_retry(&self, err: &Error) -> bool {
        self.retry_if
            .as_ref()
            .is_some_and(|predicate| predicate(err))
    }

    /// The wait before retrying zero-based `attempt`.
    ///
    /// Uses an allowed server delay from `headers`; otherwise capped
    /// exponential backoff, reduced by `random * backoff_jitter`.
    /// `random` is expected in `0.0..=1.0`.
    pub fn delay(&self, attempt: u32, headers: Option<&HeaderMap>, random: f64) -> Duration {
        if self.respect_retry_after
            && let Some(headers) = headers
            && let Some(server) = parse_retry_after(headers, SystemTime::now())
            && server <= self.max_retry_after
        {
            return server;
        }
        let factor = 2u32.checked_pow(attempt).unwrap_or(u32::MAX);
        let exponential = self
            .backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_max);
        let keep = (1.0 - random.clamp(0.0, 1.0) * self.backoff_jitter).clamp(0.0, 1.0);
        exponential.mul_f64(keep)
    }
}

/// Parse `retry-after-ms` or `Retry-After`, preferring `retry-after-ms`.
///
/// `Retry-After` may hold seconds or an HTTP date; a date in the past yields
/// zero. Returns `None` when neither header holds a valid delay.
pub fn parse_retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    if let Some(ms) = header_str(headers, "retry-after-ms").and_then(parse_non_negative)
        && let Ok(delay) = Duration::try_from_secs_f64(ms / 1000.0)
    {
        return Some(delay);
    }
    let raw = header_str(headers, "retry-after")?;
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::try_from_secs_f64(seconds).ok())
            .flatten();
    }
    let date = httpdate::parse_http_date(raw).ok()?;
    Some(date.duration_since(now).unwrap_or(Duration::ZERO))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

fn parse_non_negative(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn default_policy_matches_the_documented_values() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 2);
        assert!(policy.is_retryable_status(408));
        assert!(policy.is_retryable_status(429));
        assert!(policy.is_retryable_status(500));
        assert!(policy.is_retryable_status(599));
        assert!(!policy.is_retryable_status(600));
        assert!(!policy.is_retryable_status(409));
        assert_eq!(policy.http_statuses.len(), 102);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let policy = RetryPolicy::default();
        let ms = |attempt| policy.delay(attempt, None, 0.0).as_millis();
        assert_eq!(
            [ms(0), ms(1), ms(2), ms(3), ms(4)],
            [500, 1000, 2000, 4000, 5000]
        );
        assert_eq!(ms(40), 5000);
        assert_eq!(ms(u32::MAX), 5000);
    }

    #[test]
    fn jitter_subtracts_at_most_the_configured_fraction() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay(0, None, 1.0), Duration::from_millis(375));
        assert_eq!(policy.delay(0, None, 0.5), Duration::from_micros(437_500));
    }

    #[test]
    fn honors_retry_after_within_the_cap() {
        let policy = RetryPolicy::default();
        let server = headers(&[("retry-after", "2")]);
        assert_eq!(policy.delay(0, Some(&server), 1.0), Duration::from_secs(2));

        let ignoring = RetryPolicy {
            respect_retry_after: false,
            ..RetryPolicy::default()
        };
        assert_eq!(
            ignoring.delay(0, Some(&server), 0.0),
            Duration::from_millis(500)
        );

        let capped = RetryPolicy {
            max_retry_after: Duration::from_secs(1),
            ..RetryPolicy::default()
        };
        assert_eq!(
            capped.delay(0, Some(&server), 0.0),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn parses_retry_after_forms() {
        let now = SystemTime::now();
        let parse = |pairs: &[(&'static str, &str)]| parse_retry_after(&headers(pairs), now);
        assert_eq!(
            parse(&[("retry-after-ms", "250")]),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse(&[("retry-after-ms", "1.5")]),
            Some(Duration::from_micros(1500))
        );
        assert_eq!(parse(&[("retry-after", "3")]), Some(Duration::from_secs(3)));
        assert_eq!(
            parse(&[("retry-after", " 0.5 ")]),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse(&[("retry-after-ms", "100"), ("retry-after", "9")]),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            parse(&[("retry-after-ms", "-1"), ("retry-after", "9")]),
            Some(Duration::from_secs(9))
        );
        assert_eq!(parse(&[("retry-after", "-3")]), None);
        assert_eq!(parse(&[("retry-after", "inf")]), None);
        assert_eq!(parse(&[("retry-after", "soon")]), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn parses_http_dates_relative_to_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let later = httpdate::fmt_http_date(now + Duration::from_secs(30));
        let earlier = httpdate::fmt_http_date(now - Duration::from_secs(30));
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", &later)]), now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", &earlier)]), now),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn validation_names_the_bad_field() {
        let bad = |policy: RetryPolicy| policy.validate().unwrap_err().to_string();
        assert!(
            bad(RetryPolicy {
                backoff_jitter: 1.5,
                ..RetryPolicy::default()
            })
            .contains("retry.backoff_jitter")
        );
        assert!(
            bad(RetryPolicy {
                backoff_jitter: -0.1,
                ..RetryPolicy::default()
            })
            .contains("retry.backoff_jitter")
        );
        assert!(
            bad(RetryPolicy {
                backoff_jitter: f64::NAN,
                ..RetryPolicy::default()
            })
            .contains("retry.backoff_jitter")
        );
        assert!(
            bad(RetryPolicy {
                http_statuses: [503, 42].into(),
                ..RetryPolicy::default()
            })
            .contains("retry.http_statuses")
        );
        assert!(
            RetryPolicy {
                backoff_jitter: 1.0,
                ..RetryPolicy::none()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn retryable_errors_follow_the_policy_flags() {
        let timeout = Error::Timeout {
            timeout: Duration::from_secs(1),
        };
        let policy = RetryPolicy {
            retry_timeouts: false,
            ..RetryPolicy::default()
        };
        assert!(!policy.is_retryable_error(&timeout));
        assert!(RetryPolicy::default().is_retryable_error(&timeout));
        assert!(!RetryPolicy::default().is_retryable_error(&Error::Config("x".into())));
    }
}
