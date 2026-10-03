//! Property tests for response parsing and retry arithmetic.

use std::time::{Duration, SystemTime};

use kunobi_decision::reqwest::StatusCode;
use kunobi_decision::reqwest::header::{HeaderMap, HeaderValue};
use kunobi_decision::{Answer, ApiError, RetryPolicy, parse_retry_after};
use proptest::prelude::*;

fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        if let Ok(value) = HeaderValue::from_str(value) {
            map.insert(*name, value);
        }
    }
    map
}

proptest! {
    /// Any body and status yields an error whose message starts with the status
    /// and never panics on odd bytes or multi-byte text.
    #[test]
    fn api_error_messages_never_panic(status in 400u16..600, body in proptest::collection::vec(any::<u8>(), 0..600)) {
        let err = ApiError::from_response(StatusCode::from_u16(status).unwrap(), HeaderMap::new(), &body);
        let prefix = format!("{status} ");
        prop_assert!(err.message().starts_with(&prefix));
    }

    /// JSON bodies without a recognized message are quoted, truncated to 200 characters.
    #[test]
    fn raw_json_bodies_are_truncated(text in "\\PC{0,400}") {
        let body = serde_json::json!({ "unrecognized": text }).to_string();
        let err = ApiError::from_response(StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), body.as_bytes());
        let quoted = err.message().trim_start_matches("500 ");
        let limit = 200 + "…".chars().count();
        prop_assert!(quoted.chars().count() <= limit);
    }

    /// Retry-After parsing never panics, and numeric values round-trip.
    #[test]
    fn retry_after_parsing_never_panics(ms in "\\PC{0,30}", after in "\\PC{0,40}") {
        let _ = parse_retry_after(
            &headers(&[("retry-after-ms", ms), ("retry-after", after)]),
            SystemTime::now(),
        );
    }

    #[test]
    fn retry_after_seconds_round_trip(seconds in 0u32..100_000) {
        let parsed = parse_retry_after(&headers(&[("retry-after", seconds.to_string())]), SystemTime::now());
        prop_assert_eq!(parsed, Some(Duration::from_secs(seconds.into())));
    }

    /// Backoff stays within [cap * (1 - jitter), cap] for every attempt and random draw.
    #[test]
    fn backoff_stays_within_bounds(
        attempt in any::<u32>(),
        initial_ms in 0u64..10_000,
        max_ms in 0u64..60_000,
        jitter in 0.0f64..=1.0,
        random in 0.0f64..=1.0,
    ) {
        let policy = RetryPolicy {
            backoff_initial: Duration::from_millis(initial_ms),
            backoff_max: Duration::from_millis(max_ms),
            backoff_jitter: jitter,
            ..RetryPolicy::default()
        };
        let delay = policy.delay(attempt, None, random);
        let factor = 2u32.checked_pow(attempt).unwrap_or(u32::MAX);
        let exponential = policy.backoff_initial.saturating_mul(factor).min(policy.backoff_max);
        prop_assert!(delay <= exponential);
        prop_assert!(delay + Duration::from_micros(1) >= exponential.mul_f64(1.0 - jitter));
    }

    /// Server delays are honored only up to the configured cap.
    #[test]
    fn server_delays_respect_the_cap(server_ms in 0u64..120_000, cap_ms in 0u64..120_000) {
        let policy = RetryPolicy {
            max_retry_after: Duration::from_millis(cap_ms),
            backoff_jitter: 0.0,
            ..RetryPolicy::default()
        };
        let delay = policy.delay(0, Some(&headers(&[("retry-after-ms", server_ms.to_string())])), 0.0);
        if server_ms <= cap_ms {
            prop_assert_eq!(delay, Duration::from_millis(server_ms));
        } else {
            prop_assert_eq!(delay, policy.backoff_initial);
        }
    }

    /// Answer parsing never panics on arbitrary JSON.
    #[test]
    fn answer_parsing_never_panics(body in "\\PC{0,200}") {
        let _ = serde_json::from_str::<Answer>(&body);
    }
}
