//! Successful responses are parsed into typed answers; malformed bodies must
//! produce errors, never panics.

#![no_main]

use kunobi_decision::SystemOneResult;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(result) = serde_json::from_slice::<SystemOneResult>(data) {
        // Anything that parses must serialize back.
        serde_json::to_vec(&result).expect("parsed results serialize");
    }
});
