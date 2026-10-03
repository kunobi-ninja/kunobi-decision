//! Client construction.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::SecretString;
use tokio::sync::Semaphore;

use super::env::read_env;
use super::{Client, Inner, Provider};
use crate::credentials::{BoxError, CredentialProvider, Credentials, FnProvider, bearer_header};
use crate::error::{Error, Result};
use crate::retry::{DEFAULT_TIMEOUT, DEFAULT_TOTAL_TIMEOUT, RetryPolicy};

/// Builder for [`Client`].
///
/// Values set here take precedence over environment variables, then defaults.
/// Blank environment values are ignored.
#[derive(Default)]
pub struct ClientBuilder {
    provider: Provider,
    credentials: Option<Credentials>,
    base_url: Option<String>,
    allow_insecure_http: bool,
    log_bodies: bool,
    default_model: Option<String>,
    retry: Option<RetryPolicy>,
    timeout: Option<Duration>,
    /// Outer `None` means "not set, use the default"; inner `None` means the
    /// caller removed the bound.
    total_timeout: Option<Option<Duration>>,
    max_concurrent_requests: Option<usize>,
    default_headers: HeaderMap,
    http_client: Option<reqwest::Client>,
}

impl ClientBuilder {
    /// Select the access provider. TypeSafe is the default.
    pub fn provider(mut self, provider: Provider) -> Self {
        self.provider = provider;
        self
    }

    /// Authenticate with an API key; falls back to the selected provider's key environment variable.
    ///
    /// The key is held as a [`SecretString`] and wiped from memory when the client
    /// is dropped. Don't embed an API key in software that runs on user machines;
    /// use [`ClientBuilder::credential_provider`] with your own backend instead.
    pub fn api_key(mut self, api_key: impl Into<SecretString>) -> Self {
        self.credentials = Some(Credentials::ApiKey(api_key.into()));
        self
    }

    /// Authenticate with a bearer token from a provider, asked before every attempt.
    ///
    /// Replaces any API key set on the builder, and the environment key is not read.
    pub fn credential_provider(mut self, provider: impl CredentialProvider) -> Self {
        self.credentials = Some(Credentials::Provider(Box::new(provider)));
        self
    }

    /// Authenticate with a bearer token from an async closure, asked before every attempt.
    ///
    /// ```no_run
    /// # fn example() -> kunobi_decision::Result<()> {
    /// use kunobi_decision::Client;
    ///
    /// let client = Client::builder()
    ///     .base_url("https://jev.example.com")
    ///     .credentials_fn(|| async { Ok::<_, std::io::Error>(String::from("short-lived-token")) })
    ///     .build()?;
    /// # Ok(()) }
    /// ```
    pub fn credentials_fn<F, Fut, T, E>(self, provider: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<T, E>> + Send + 'static,
        T: Into<SecretString> + 'static,
        E: Into<BoxError> + 'static,
    {
        self.credential_provider(FnProvider(provider))
    }

    /// API root; defaults to the selected provider's root.
    /// TypeSafe reads `TYPESAFE_BASE_URL`; OpenRouter reads `OPENROUTER_BASE_URL`.
    ///
    /// Must be https. Plain http is accepted only for loopback hosts, unless
    /// [`ClientBuilder::allow_insecure_http`] is set.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Accept a plain-http `base_url` on any host. Credentials then travel unencrypted.
    ///
    /// Meant for trusted private networks, such as a sidecar on the same pod.
    pub fn allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Include request and response bodies in `debug` logs. Default: off.
    ///
    /// Bodies carry the state you send and the answers you get, which may include
    /// personal data. Headers are always logged with credentials redacted.
    pub fn log_bodies(mut self, log_bodies: bool) -> Self {
        self.log_bodies = log_bodies;
        self
    }

    /// Default model; defaults to the selected provider's model.
    /// TypeSafe reads `TYPESAFE_DEFAULT_MODEL`; OpenRouter reads `OPENROUTER_DEFAULT_MODEL`.
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Retry policy; defaults to [`RetryPolicy::default`].
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }

    /// Timeout per attempt, including the response body.
    /// Default: [`DEFAULT_TIMEOUT`](crate::DEFAULT_TIMEOUT), 5 s.
    ///
    /// A credential provider gets the same timeout. To bound a whole call, retries
    /// included, use [`ClientBuilder::total_timeout`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Upper bound for a whole call, including credentials, retries and backoff.
    /// Default: [`DEFAULT_TOTAL_TIMEOUT`](crate::DEFAULT_TOTAL_TIMEOUT), 10 s.
    ///
    /// Pass `None` to remove the bound and let the retry policy run to its end.
    /// Per-call [`Call::total_timeout`](crate::Call::total_timeout) takes precedence.
    pub fn total_timeout(mut self, total_timeout: impl Into<Option<Duration>>) -> Self {
        self.total_timeout = Some(total_timeout.into());
        self
    }

    /// Limit how many attempts this client and its clones send at once. Default: no limit.
    ///
    /// Calls over the limit wait for a slot; the wait counts against
    /// [`total_timeout`](Self::total_timeout) but not the per-attempt timeout. A slot is
    /// held for one attempt and released during retry backoff.
    pub fn max_concurrent_requests(mut self, max: usize) -> Self {
        self.max_concurrent_requests = Some(max);
        self
    }

    /// Add a header sent with every request. Per-call headers take precedence.
    pub fn default_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.default_headers.insert(name, value);
        self
    }

    /// Add headers sent with every request, replacing earlier values for the same names.
    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers.extend(headers);
        self
    }

    /// Use a preconfigured HTTP client, for proxies, custom TLS or tests.
    ///
    /// Leave its own timeout unset: the client applies [`ClientBuilder::timeout`] per attempt.
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http_client = Some(http);
        self
    }

    /// Build the client.
    ///
    /// Fails when credentials are missing or configuration is invalid.
    pub fn build(self) -> Result<Client> {
        self.build_with_env(read_env)
    }

    fn build_with_env(self, env: impl Fn(&str) -> Option<String>) -> Result<Client> {
        self.provider.validate()?;
        let api_key_env = self.provider.api_key_env();
        let credentials = match self.credentials {
            Some(credentials) => credentials,
            None => Credentials::ApiKey(SecretString::from(env(api_key_env).ok_or_else(|| {
                Error::Config(format!(
                    "No credentials were provided. Call `ClientBuilder::api_key` or \
                     `ClientBuilder::credential_provider`, or set the {api_key_env} environment variable."
                ))
            })?)),
        };
        if let Credentials::ApiKey(key) = &credentials {
            bearer_header(key)
                .map_err(|message| Error::Config(format!("The API key {message}.")))?;
        }

        let base_url = self
            .base_url
            .or_else(|| self.provider.base_url_env().and_then(&env))
            .unwrap_or_else(|| self.provider.base_url().to_owned());
        let base_url = base_url.trim().trim_end_matches('/').to_owned();
        validate_base_url(&base_url, self.allow_insecure_http)?;

        let default_model = self
            .default_model
            .or_else(|| self.provider.default_model_env().and_then(&env))
            .unwrap_or_else(|| self.provider.default_model().to_owned());

        let retry = self.retry.unwrap_or_default();
        retry.validate()?;
        let timeout = validate_timeout(self.timeout.unwrap_or(DEFAULT_TIMEOUT))?;
        let total_timeout = self.total_timeout.unwrap_or(Some(DEFAULT_TOTAL_TIMEOUT));
        if total_timeout.is_some_and(|total| total.is_zero()) {
            return Err(Error::Config(
                "`total_timeout` must be a positive duration, got 0.".into(),
            ));
        }

        let limiter = match self.max_concurrent_requests {
            Some(max) if max == 0 || max > Semaphore::MAX_PERMITS => {
                return Err(Error::Config(format!(
                    "`max_concurrent_requests` must be between 1 and {}, got {max}.",
                    Semaphore::MAX_PERMITS
                )));
            }
            Some(max) => Some((Semaphore::new(max), max)),
            None => None,
        };

        let http = match self.http_client {
            Some(http) => http,
            None if !TLS_BACKEND && base_url.starts_with("https:") => {
                return Err(Error::Config(
                    "kunobi-decision was built without a TLS backend, so it cannot reach an https \
                     `base_url`. Enable the `rustls` or `native-tls` feature, or pass a client \
                     through `ClientBuilder::http_client`."
                        .into(),
                ));
            }
            None => reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|err| Error::Config(format!("Could not create the HTTP client: {err}")))?,
        };

        Ok(Client {
            inner: Arc::new(Inner {
                provider: self.provider,
                credentials,
                base_url,
                log_bodies: self.log_bodies,
                default_model,
                retry,
                timeout,
                total_timeout,
                limiter,
                default_headers: self.default_headers,
                http,
                request_count: AtomicU64::new(0),
            }),
        })
    }
}

/// Whether the default HTTP client can speak TLS.
const TLS_BACKEND: bool = cfg!(any(feature = "rustls", feature = "native-tls"));

/// Require https, allowing plain http only for loopback hosts or when explicitly allowed.
fn validate_base_url(base_url: &str, allow_insecure_http: bool) -> Result<()> {
    let url = reqwest::Url::parse(base_url).map_err(|_| {
        Error::Config(format!(
            "`base_url` must be an https URL, got \"{base_url}\"."
        ))
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "`base_url` must not contain credentials, a query or a fragment.".into(),
        ));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure_http || is_loopback(url.host_str().unwrap_or_default()) => Ok(()),
        "http" => Err(Error::Config(format!(
            "`base_url` \"{base_url}\" uses plain http, which would send credentials unencrypted. \
             Use https, a loopback host, or `ClientBuilder::allow_insecure_http(true)`."
        ))),
        _ => Err(Error::Config(format!(
            "`base_url` must be an https URL, got \"{base_url}\"."
        ))),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub(crate) fn validate_timeout(timeout: Duration) -> Result<Duration> {
    if timeout.is_zero() {
        return Err(Error::Config(
            "`timeout` must be a positive duration, got 0.".into(),
        ));
    }
    Ok(timeout)
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("provider", &self.provider)
            .field("credentials", &self.credentials)
            .field("base_url", &self.base_url)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("log_bodies", &self.log_bodies)
            .field("default_model", &self.default_model)
            .field("retry", &self.retry)
            .field("timeout", &self.timeout)
            .field("total_timeout", &self.total_timeout)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::env::non_blank;
    use crate::{DEFAULT_BASE_URL, DEFAULT_MODEL, ENV_API_KEY, ENV_BASE_URL, ENV_DEFAULT_MODEL};
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| non_blank(map.get(name).cloned())
    }

    /// A builder that works in every feature set: with no TLS backend compiled in, the
    /// default client refuses https, which is not what these tests are about.
    fn builder() -> ClientBuilder {
        ClientBuilder::default().http_client(reqwest::Client::new())
    }

    async fn authorization(client: &Client) -> String {
        client
            .inner
            .credentials
            .authorization(Duration::from_secs(1))
            .await
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn provider_credentials_and_defaults_are_isolated_from_typesafe_environment() {
        for provider in [
            Provider::Liquid,
            Provider::Vercel,
            Provider::OpenRouter,
            Provider::Cloudflare {
                account_id: "account".into(),
            },
        ] {
            let client = builder()
                .provider(provider.clone())
                .build_with_env(env(&[
                    (provider.api_key_env(), "provider-key"),
                    (ENV_API_KEY, "wrong-key"),
                    (ENV_BASE_URL, "https://wrong.example"),
                    (ENV_DEFAULT_MODEL, "wrong-model"),
                ]))
                .unwrap();
            assert_eq!(client.base_url(), provider.base_url());
            assert_eq!(client.default_model(), provider.default_model());
            assert_eq!(authorization(&client).await, "Bearer provider-key");
            assert!(
                builder()
                    .provider(provider)
                    .build_with_env(env(&[(ENV_API_KEY, "wrong-key")]))
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn openrouter_environment_is_scoped_and_explicit_settings_take_precedence() {
        let vars = env(&[
            ("OPENROUTER_API_KEY", "router-key"),
            ("OPENROUTER_BASE_URL", "http://localhost:9000/api"),
            ("OPENROUTER_DEFAULT_MODEL", "typesafe/jev-1.13"),
            (ENV_API_KEY, "typesafe-key"),
        ]);
        let c = builder()
            .provider(Provider::OpenRouter)
            .build_with_env(&vars)
            .unwrap();
        assert_eq!(c.base_url(), "http://localhost:9000/api");
        assert_eq!(c.default_model(), "typesafe/jev-1.13");
        assert_eq!(authorization(&c).await, "Bearer router-key");
        let c = builder()
            .provider(Provider::OpenRouter)
            .api_key("explicit-key")
            .base_url("http://localhost:9001/api")
            .default_model("liquid/d1")
            .build_with_env(&vars)
            .unwrap();
        assert_eq!(c.base_url(), "http://localhost:9001/api");
        assert_eq!(c.default_model(), "liquid/d1");
        assert_eq!(authorization(&c).await, "Bearer explicit-key");
        let c = builder().build_with_env(vars).unwrap();
        assert_eq!(c.provider(), &Provider::TypeSafe);
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
        assert_eq!(c.default_model(), DEFAULT_MODEL);
        assert_eq!(authorization(&c).await, "Bearer typesafe-key");
    }

    #[test]
    fn requires_credentials() {
        let err = builder().build_with_env(env(&[])).unwrap_err();
        assert!(err.to_string().contains(ENV_API_KEY), "{err}");
        let blank_env = builder().build_with_env(env(&[(ENV_API_KEY, "  ")]));
        assert!(blank_env.is_err());
        let blank_code = builder()
            .api_key(" ")
            .build_with_env(env(&[(ENV_API_KEY, "k")]));
        assert_eq!(blank_code.unwrap_err().to_string(), "The API key is blank.");
    }

    #[tokio::test]
    async fn falls_back_to_environment_then_defaults() {
        let client = builder()
            .build_with_env(env(&[(ENV_API_KEY, " k ")]))
            .unwrap();
        assert_eq!(client.base_url(), DEFAULT_BASE_URL);
        assert_eq!(client.default_model(), DEFAULT_MODEL);
        assert_eq!(client.retry(), &RetryPolicy::default());
        assert_eq!(client.timeout(), DEFAULT_TIMEOUT);
        assert_eq!(authorization(&client).await, "Bearer k");
        assert!(!client.inner.log_bodies);

        let from_env = builder()
            .build_with_env(env(&[
                (ENV_API_KEY, "k"),
                (ENV_BASE_URL, "http://localhost:8080//"),
                (ENV_DEFAULT_MODEL, "jev-test"),
            ]))
            .unwrap();
        assert_eq!(from_env.base_url(), "http://localhost:8080");
        assert_eq!(from_env.default_model(), "jev-test");
    }

    #[tokio::test]
    async fn code_takes_precedence_over_environment() {
        let client = builder()
            .api_key("from-code")
            .base_url("https://example.com/")
            .default_model("jev-code")
            .build_with_env(env(&[
                (ENV_API_KEY, "from-env"),
                (ENV_BASE_URL, "https://env.example.com"),
                (ENV_DEFAULT_MODEL, "jev-env"),
            ]))
            .unwrap();
        assert_eq!(authorization(&client).await, "Bearer from-code");
        assert_eq!(client.base_url(), "https://example.com");
        assert_eq!(client.default_model(), "jev-code");
    }

    #[tokio::test]
    async fn a_provider_replaces_api_keys_from_code_and_environment() {
        let client = builder()
            .api_key("from-code")
            .credentials_fn(|| async { Ok::<_, BoxError>("from-provider") })
            .build_with_env(env(&[(ENV_API_KEY, "from-env")]))
            .unwrap();
        assert_eq!(authorization(&client).await, "Bearer from-provider");
    }

    #[test]
    fn https_needs_a_tls_backend_or_a_custom_client() {
        let default_client = ClientBuilder::default()
            .api_key("k")
            .build_with_env(env(&[]));
        assert_eq!(default_client.is_ok(), TLS_BACKEND);
        if !TLS_BACKEND {
            let err = default_client.unwrap_err();
            assert!(err.to_string().contains("without a TLS backend"), "{err}");
        }
        let custom = ClientBuilder::default()
            .api_key("k")
            .http_client(reqwest::Client::new())
            .build_with_env(env(&[]));
        assert!(custom.is_ok());
    }

    #[test]
    fn plain_http_is_limited_to_loopback_hosts() {
        let build = |url: &str, allow: bool| {
            builder()
                .api_key("k")
                .base_url(url)
                .allow_insecure_http(allow)
                .build_with_env(env(&[]))
        };
        for ok in [
            "https://api.typesafe.ai",
            "http://localhost:1234",
            "http://LOCALHOST",
            "http://127.0.0.1:9",
            "http://127.8.0.1",
            "http://[::1]:80",
        ] {
            assert!(build(ok, false).is_ok(), "{ok}");
        }
        for insecure in [
            "http://api.typesafe.ai",
            "http://10.0.0.5",
            "http://localhost.evil.com",
            "http://[::2]",
        ] {
            let err = build(insecure, false).unwrap_err();
            assert!(err.to_string().contains("plain http"), "{insecure}: {err}");
            assert!(build(insecure, true).is_ok(), "{insecure} with opt-out");
        }
        for invalid in ["ftp://x", "not a url", ""] {
            assert!(build(invalid, true).is_err(), "{invalid}");
        }
    }

    #[test]
    fn plain_http_from_the_environment_is_refused_too() {
        let err = builder()
            .build_with_env(env(&[
                (ENV_API_KEY, "k"),
                (ENV_BASE_URL, "http://attacker.example"),
            ]))
            .unwrap_err();
        assert!(err.to_string().contains("plain http"), "{err}");
    }

    #[test]
    fn rejects_invalid_configuration() {
        let build = |builder: ClientBuilder| builder.api_key("k").build_with_env(env(&[]));
        let err = build(builder().timeout(Duration::ZERO)).unwrap_err();
        assert!(err.to_string().contains("timeout"));
        let err = build(builder().retry(RetryPolicy {
            backoff_jitter: 2.0,
            ..RetryPolicy::default()
        }))
        .unwrap_err();
        assert!(err.to_string().contains("retry.backoff_jitter"));
        let err = builder()
            .api_key("bad\nkey")
            .build_with_env(env(&[]))
            .unwrap_err();
        assert!(err.to_string().contains("HTTP header"));
        assert!(
            !err.to_string().contains("bad"),
            "the key must not leak: {err}"
        );
    }

    #[test]
    fn debug_output_hides_credentials() {
        let builder = builder().api_key("sk-secret");
        assert!(!format!("{builder:?}").contains("sk-secret"));
        let client = builder.build_with_env(env(&[])).unwrap();
        assert!(!format!("{client:?}").contains("sk-secret"));
    }
}
