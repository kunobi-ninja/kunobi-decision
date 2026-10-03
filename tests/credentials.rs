//! Credentials end to end: providers and redirects.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kunobi_decision::{BoxError, Client, Error, RetryPolicy};
use serde_json::json;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "test-key-0123456789abcdef";

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        backoff_initial: Duration::from_millis(1),
        backoff_jitter: 0.0,
        ..RetryPolicy::default()
    }
}

#[tokio::test]
async fn the_provider_is_asked_before_every_attempt() {
    let server = MockServer::start().await;
    let responses = AtomicUsize::new(0);
    Mock::given(path("/v1/models"))
        .respond_with(move |_: &wiremock::Request| {
            if responses.fetch_add(1, Ordering::SeqCst) < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"models": []}))
            }
        })
        .mount(&server)
        .await;

    let issued = Arc::new(AtomicUsize::new(0));
    let counter = issued.clone();
    let client = Client::builder()
        .base_url(server.uri())
        .retry(fast_retry())
        .credentials_fn(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            async move { Ok::<_, BoxError>(format!("token-{n}")) }
        })
        .build()
        .unwrap();

    client.models().list().await.unwrap();

    let sent: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(sent, ["Bearer token-1", "Bearer token-2", "Bearer token-3"]);
}

#[tokio::test]
async fn provider_failures_stop_the_call_without_sending_or_retrying() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let client = Client::builder()
        .base_url(server.uri())
        .retry(fast_retry())
        .credentials_fn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err::<String, _>(std::io::Error::other("session expired")) }
        })
        .build()
        .unwrap();

    let err = client.models().list().await.unwrap_err();
    assert!(matches!(err, Error::Credentials { .. }), "{err:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn default_client_refuses_redirects() {
    let target = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
        .mount(&target)
        .await;
    let origin = MockServer::start().await;
    // Same IP, different port: reqwest treats it as a different origin.
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/models", target.uri())),
        )
        .mount(&origin)
        .await;

    let error = Client::builder()
        .api_key(SECRET)
        .base_url(origin.uri())
        .build()
        .unwrap()
        .models()
        .list()
        .await
        .unwrap_err();

    assert_eq!(error.status().unwrap().as_u16(), 307);
    assert!(target.received_requests().await.unwrap().is_empty());
}
