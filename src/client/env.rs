//! Environment variables for client configuration.

/// Required API key, used when [`ClientBuilder::api_key`](super::ClientBuilder::api_key) is not set.
pub const ENV_API_KEY: &str = "TYPESAFE_API_KEY";

/// API root; defaults to `https://api.typesafe.ai`.
pub const ENV_BASE_URL: &str = "TYPESAFE_BASE_URL";

/// Default model; defaults to `jev-latest`.
pub const ENV_DEFAULT_MODEL: &str = "TYPESAFE_DEFAULT_MODEL";

/// Read a trimmed variable, treating missing, blank and non-UTF-8 values as unset.
pub(crate) fn read_env(name: &str) -> Option<String> {
    non_blank(std::env::var(name).ok())
}

pub(crate) fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
