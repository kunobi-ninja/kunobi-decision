//! Retry-After headers drive how long a client sleeps; parsing and the delay
//! calculation must never panic, whatever the server sends.

#![no_main]

use std::time::{Duration, SystemTime};

use kunobi_decision::reqwest::header::{HeaderMap, HeaderValue};
use kunobi_decision::{RetryPolicy, parse_retry_after};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mid = data.len() / 2;
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_bytes(&data[..mid]) {
        headers.insert("retry-after-ms", value);
    }
    if let Ok(value) = HeaderValue::from_bytes(&data[mid..]) {
        headers.insert("retry-after", value);
    }
    let _ = parse_retry_after(&headers, SystemTime::now());

    let policy = RetryPolicy::default();
    let attempt = data.first().copied().unwrap_or_default().into();
    let delay = policy.delay(attempt, Some(&headers), 0.5);
    assert!(delay <= policy.max_retry_after.max(policy.backoff_max) + Duration::from_millis(1));
});
