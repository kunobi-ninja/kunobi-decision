//! Error bodies come from the server or any proxy in between; message
//! extraction and truncation must never panic.

#![no_main]

use kunobi_decision::ApiError;
use kunobi_decision::reqwest::StatusCode;
use kunobi_decision::reqwest::header::HeaderMap;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((status, body)) = data.split_first_chunk::<2>() else {
        return;
    };
    let code = 400 + u16::from_le_bytes(*status) % 200;
    let status = StatusCode::from_u16(code).expect("400..600 is a valid status");
    let err = ApiError::from_response(status, HeaderMap::new(), body);
    assert!(err.message().starts_with(&code.to_string()));
});
