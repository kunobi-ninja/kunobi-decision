//! Client behavior against a mock server: request shape, retries, timeouts and errors.

use std::time::{Duration, Instant};

use kunobi_decision::reqwest::StatusCode;
use kunobi_decision::reqwest::header::{HeaderName, HeaderValue};
use kunobi_decision::{
    ApiErrorKind, Client, Entry, Error, ErrorBody, Questions, RetryPolicy, SystemOneRequest,
    choice, noul, score,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// How long a mock server stalls when a test checks that the client gives up first.
/// Timing assertions compare against this, not against the client's own timeouts, so
/// a slow CI runner can't fail a client that behaves correctly.
const STALL: Duration = Duration::from_secs(3);

/// A retry policy with millisecond backoff and no jitter, so tests stay fast and deterministic.
fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        backoff_initial: Duration::from_millis(1),
        backoff_max: Duration::from_millis(5),
        backoff_jitter: 0.0,
        ..RetryPolicy::default()
    }
}

fn client(server: &MockServer) -> Client {
    Client::builder()
        .api_key("sk-test-key")
        .base_url(server.uri())
        .retry(fast_retry())
        .build()
        .unwrap()
}

fn models_body() -> Value {
    json!({"models": [{"name": "jev-latest", "description": "d", "release_date": "2026"}]})
}

fn system_one_body() -> Value {
    json!({
        "model": "jev-latest",
        "answers": {
            "billing": {"type": "noul", "noul": 0.93},
            "tone": {
                "type": "choice",
                "choice": "frustrated",
                "confidence": 0.71,
                "probabilities": {"calm": 0.05, "frustrated": 0.85, "angry": 0.10}
            },
            "urgency": {
                "type": "score",
                "score": 2.4,
                "confidence": 0.66,
                "legend": {"0": "can wait", "1": "today", "2": "now"},
                "probabilities": {"0": 0.1, "1": 0.2, "2": 0.7}
            }
        },
        "usage": {"input_tokens": 120, "output_tokens": 9}
    })
}

/// A responder that returns each template in turn, repeating the last one.
fn sequence(templates: Vec<ResponseTemplate>) -> impl Fn(&Request) -> ResponseTemplate {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    move |_: &Request| {
        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        templates[n.min(templates.len() - 1)].clone()
    }
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

#[tokio::test]
async fn system_one_sends_the_documented_request_and_types_answers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(system_one_body())
                .insert_header("x-typesafe-request-id", "req_42"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut questions = Questions::new();
    let billing = questions.add("billing", noul("Is this ticket about billing?"));
    let tone = questions.add(
        "tone",
        choice(
            "What is the tone?",
            [
                ("calm", None),
                ("frustrated", Some("upset")),
                ("angry", None),
            ],
        ),
    );
    let urgency = questions.add(
        "urgency",
        score("How urgent?", ["can wait", "today", "now"]),
    );
    let state = Entry::try_from(json!({"subject": "Charged twice", "body": "Fix it"})).unwrap();

    let response = client(&server)
        .system_one(SystemOneRequest::new(state, questions))
        .with_response()
        .await
        .unwrap();

    assert_eq!(response.request_id.as_deref(), Some("req_42"));
    let result = response.data;
    assert_eq!(result.answer(&billing).unwrap().noul, 0.93);
    let tone = result.answer(&tone).unwrap();
    assert_eq!(tone.choice, "frustrated");
    let labels: Vec<_> = tone.probabilities.keys().collect();
    assert_eq!(labels, ["calm", "frustrated", "angry"]);
    assert_eq!(result.answer(&urgency).unwrap().probabilities[&2], 0.7);
    assert_eq!(result.usage.output_tokens, Some(9));

    let requests = received(&server).await;
    let sent = &requests[0];
    let get = |name: &str| {
        sent.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert_eq!(get("authorization").as_deref(), Some("Bearer sk-test-key"));
    assert_eq!(get("content-type").as_deref(), Some("application/json"));
    assert_eq!(get("accept").as_deref(), Some("application/json"));
    let sdk = format!("kunobi-decision/{}", kunobi_decision::VERSION);
    assert_eq!(get("user-agent"), Some(sdk.clone()));
    assert_eq!(get("x-typesafe-sdk"), Some(sdk));
    assert!(get("x-typesafe-runtime").unwrap().starts_with("rust ("));
    assert_eq!(get("x-typesafe-retry-count"), None);

    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body,
        json!({
            "state": {"subject": "Charged twice", "body": "Fix it"},
            "questions": {
                "billing": {"type": "noul", "instructions": "Is this ticket about billing?"},
                "tone": {
                    "type": "choice",
                    "instructions": "What is the tone?",
                    "criteria": {"calm": null, "frustrated": "upset", "angry": null}
                },
                "urgency": {
                    "type": "score",
                    "instructions": "How urgent?",
                    "criteria": ["can wait", "today", "now"]
                }
            },
            "model": "jev-latest"
        })
    );
    // Question and state key order survive serialization.
    let raw = String::from_utf8(sent.body.clone()).unwrap();
    assert!(raw.find("\"subject\"").unwrap() < raw.find("\"body\"").unwrap());
    assert!(raw.find("\"billing\"").unwrap() < raw.find("\"urgency\"").unwrap());
}

#[tokio::test]
async fn models_list_unwraps_the_model_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_body()))
        .mount(&server)
        .await;

    let models = client(&server).models().list().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "jev-latest");

    let requests = received(&server).await;
    assert!(requests[0].headers.get("content-type").is_none());
    assert!(requests[0].body.is_empty());
}

#[tokio::test]
async fn unexpected_success_shapes_are_decode_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([1, 2]))
                .insert_header("x-typesafe-request-id", "req_bad"),
        )
        .mount(&server)
        .await;

    let err = client(&server).models().list().await.unwrap_err();
    assert!(matches!(err, Error::Decode { .. }), "{err:?}");
    assert!(err.to_string().contains("expected { models: [...] }"));
    assert_eq!(err.request_id(), Some("req_bad"));
}

#[tokio::test]
async fn invalid_requests_fail_before_anything_is_sent() {
    let server = MockServer::start().await;
    let client = client(&server);

    let err = client
        .system_one(SystemOneRequest::new("s", Questions::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)));

    let mut short = Questions::new();
    short.add("level", score("How?", ["only"]));
    let err = client
        .system_one(SystemOneRequest::new("s", short))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at least two scores"));

    let mut ok = Questions::new();
    ok.add("q", noul("q"));
    let err = client
        .system_one(SystemOneRequest::new("s", ok))
        .timeout(Duration::ZERO)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timeout"));

    assert!(received(&server).await.is_empty());
}

#[tokio::test]
async fn retries_server_errors_and_reports_the_retry_count() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(503),
            ResponseTemplate::new(503),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;

    let models = client(&server).models().list().await.unwrap();
    assert_eq!(models[0].name, "jev-latest");

    let requests = received(&server).await;
    let counts: Vec<_> = requests
        .iter()
        .map(|r| {
            r.headers
                .get("x-typesafe-retry-count")
                .map(|v| v.to_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(counts, [None, Some("1".into()), Some("2".into())]);
}

#[tokio::test]
async fn gives_up_after_max_retries_with_the_last_error() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"message": "down"})))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .retry(RetryPolicy {
            max_retries: 3,
            ..fast_retry()
        })
        .build()
        .unwrap();
    let err = client.models().list().await.unwrap_err();
    let api = err.api_error().expect("an API error");
    assert_eq!(api.kind(), ApiErrorKind::InternalServer);
    assert_eq!(api.to_string(), "500 down");
    assert_eq!(api.body(), &ErrorBody::Json(json!({"message": "down"})));
    assert_eq!(received(&server).await.len(), 4);
}

#[tokio::test]
async fn does_not_retry_non_retryable_statuses() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "detail": [{"loc": ["body", "questions"], "msg": "field required"}]
        })))
        .mount(&server)
        .await;

    let mut questions = Questions::new();
    questions.add("q", noul("q"));
    let err = client(&server)
        .system_one(SystemOneRequest::new("s", questions))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert_eq!(err.to_string(), "422 questions: field required");
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn per_call_retry_settings_override_the_client() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = client(&server);

    let err = client.models().list().max_retries(0).await.unwrap_err();
    assert_eq!(err.status(), Some(StatusCode::SERVICE_UNAVAILABLE));
    assert_eq!(received(&server).await.len(), 1);
    assert_eq!(client.retry().max_retries, 2);

    server.reset().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let policy = RetryPolicy {
        max_retries: 1,
        http_statuses: [409].into(),
        ..fast_retry()
    };
    let err = client.models().list().retry(policy).await.unwrap_err();
    assert_eq!(err.api_error().unwrap().kind(), ApiErrorKind::Other);
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn honors_retry_after_ms() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(429).insert_header("retry-after-ms", "150"),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;

    let started = Instant::now();
    client(&server).models().list().await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn rate_limit_errors_expose_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_json(json!({"error": {"message": "slow down"}})),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .models()
        .list()
        .max_retries(0)
        .await
        .unwrap_err();
    let api = err.api_error().unwrap();
    assert_eq!(api.kind(), ApiErrorKind::RateLimit);
    assert_eq!(api.retry_after(), Some(Duration::from_secs(7)));
    assert_eq!(api.message(), "429 slow down");
}

#[tokio::test]
async fn timeouts_apply_per_attempt_and_retry() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(Duration::from_millis(500)),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;
    let client = client(&server);

    let models = client
        .models()
        .list()
        .timeout(Duration::from_millis(100))
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn timeouts_surface_as_timeout_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_delay(STALL))
        .mount(&server)
        .await;

    let started = Instant::now();
    let err = client(&server)
        .models()
        .list()
        .timeout(Duration::from_millis(50))
        .retry(RetryPolicy {
            retry_timeouts: false,
            ..fast_retry()
        })
        .await
        .unwrap_err();
    assert!(err.is_timeout());
    assert!(err.is_connection());
    assert_eq!(err.to_string(), "Request timed out after 50ms.");
    assert!(started.elapsed() < STALL / 2, "{:?}", started.elapsed());
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn connection_errors_are_reported_and_retried_per_policy() {
    // Bind and drop a listener to get a port that refuses connections.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = Client::builder()
        .api_key("k")
        .base_url(format!("http://127.0.0.1:{port}"))
        .retry(RetryPolicy {
            max_retries: 1,
            ..fast_retry()
        })
        .build()
        .unwrap();

    let err = client.models().list().await.unwrap_err();
    assert!(matches!(err, Error::Connection { .. }), "{err:?}");
    assert!(err.is_connection());
    assert!(!err.is_timeout());
    assert!(err.to_string().starts_with("Connection error: "));
}

#[tokio::test]
async fn call_headers_override_defaults_but_not_auth() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_body()))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(format!("{}/", server.uri()))
        .default_header(
            HeaderName::from_static("x-team"),
            HeaderValue::from_static("default"),
        )
        .default_header(
            HeaderName::from_static("x-keep"),
            HeaderValue::from_static("kept"),
        )
        .build()
        .unwrap();

    client
        .models()
        .list()
        .header(
            HeaderName::from_static("x-team"),
            HeaderValue::from_static("call"),
        )
        .header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer stolen"),
        )
        .header(
            HeaderName::from_static("x-typesafe-retry-count"),
            HeaderValue::from_static("9"),
        )
        .await
        .unwrap();

    let requests = received(&server).await;
    let sent = &requests[0];
    assert_eq!(sent.url.path(), "/v1/models");
    assert_eq!(sent.headers.get("x-team").unwrap(), "call");
    assert_eq!(sent.headers.get("x-keep").unwrap(), "kept");
    assert_eq!(sent.headers.get("authorization").unwrap(), "Bearer k");
    assert!(sent.headers.get("x-typesafe-retry-count").is_none());
}

#[tokio::test]
async fn raw_returns_the_unparsed_body() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let raw = client(&server).models().list().raw().await.unwrap();
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(&raw.body[..], b"not json");
}

#[tokio::test]
async fn requests_can_override_the_model_and_forward_extra_fields() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(system_one_body()))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .default_model("jev-default")
        .build()
        .unwrap();
    let mut questions = Questions::new();
    questions.add("billing", noul("Billing?"));

    client
        .system_one(SystemOneRequest::new("s", questions.clone()))
        .await
        .unwrap();
    client
        .system_one(
            SystemOneRequest::new("s", questions)
                .model("jev-2")
                .extra("trace", "abc"),
        )
        .await
        .unwrap();

    let requests = received(&server).await;
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(first["model"], "jev-default");
    assert_eq!(second["model"], "jev-2");
    assert_eq!(second["trace"], "abc");
}

#[tokio::test]
async fn total_timeout_skips_a_retry_that_cannot_finish() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after-ms", "3000"))
        .mount(&server)
        .await;

    let started = Instant::now();
    let err = client(&server)
        .models()
        .list()
        .total_timeout(Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(StatusCode::SERVICE_UNAVAILABLE));
    // Sleeping through the 3 s Retry-After would be the bug.
    assert!(started.elapsed() < STALL / 2, "{:?}", started.elapsed());
    assert_eq!(received(&server).await.len(), 1);
}

#[tokio::test]
async fn total_timeout_shortens_a_slow_attempt() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(STALL),
        )
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .retry(fast_retry())
        .timeout(Duration::from_secs(10))
        .total_timeout(Duration::from_millis(150))
        .build()
        .unwrap();
    assert_eq!(client.total_timeout(), Some(Duration::from_millis(150)));

    let started = Instant::now();
    let err = client.models().list().await.unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(140) && elapsed < STALL / 2,
        "{elapsed:?}"
    );

    let err = Client::builder()
        .api_key("k")
        .total_timeout(Duration::ZERO)
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("total_timeout"), "{err}");
}

#[tokio::test]
async fn the_client_implements_system_one() {
    use kunobi_decision::SystemOne;

    let server = MockServer::start().await;
    Mock::given(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(system_one_body()))
        .mount(&server)
        .await;

    let jev: std::sync::Arc<dyn SystemOne> = std::sync::Arc::new(client(&server));
    let mut questions = Questions::new();
    let billing = questions.add("billing", noul("Billing?"));
    let result = jev
        .ask(SystemOneRequest::new("s", questions))
        .await
        .unwrap();
    assert!(result.answer(&billing).unwrap().is_yes(0.9));
}

#[tokio::test]
async fn a_retry_cut_short_by_the_total_timeout_reports_the_earlier_error() {
    // The budget leaves ample room for the 60 ms backoff even when the first attempt is
    // slow, so the retry always starts; the stalled second response then outlasts it.
    let budget = Duration::from_secs(1);
    let stall = Duration::from_secs(6);
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(503).insert_header("retry-after-ms", "60"),
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(stall),
        ]))
        .mount(&server)
        .await;

    let started = Instant::now();
    let err = client(&server)
        .models()
        .list()
        .total_timeout(budget)
        .await
        .unwrap_err();
    assert_eq!(
        err.status(),
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "{err:?}"
    );
    assert!(started.elapsed() < stall / 2, "{:?}", started.elapsed());
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn credential_time_counts_against_the_total_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(Duration::from_millis(80)),
        )
        .mount(&server)
        .await;

    let client = Client::builder()
        .base_url(server.uri())
        .retry(fast_retry())
        .total_timeout(Duration::from_millis(100))
        .credentials_fn(|| async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok::<_, kunobi_decision::BoxError>("token")
        })
        .build()
        .unwrap();

    let started = Instant::now();
    let err = client.models().list().await.unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    assert!(started.elapsed() < STALL / 2, "{:?}", started.elapsed());
}

#[tokio::test]
async fn max_concurrent_requests_queues_calls() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .max_concurrent_requests(1)
        .build()
        .unwrap();
    assert_eq!(client.max_concurrent_requests(), Some(1));

    let started = Instant::now();
    let (first, second) = tokio::join!(client.models().list(), client.models().list());
    first.unwrap();
    second.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(290),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn waiting_for_a_slot_counts_against_the_total_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models_body())
                .set_delay(STALL),
        )
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .max_concurrent_requests(1)
        .build()
        .unwrap();

    let busy = tokio::spawn({
        let client = client.clone();
        async move { client.models().list().await }
    });
    // Wait until the first call holds the only slot and its request reached the server.
    while received(&server).await.is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let started = Instant::now();
    let err = client
        .models()
        .list()
        .total_timeout(Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(err.is_timeout(), "{err:?}");
    assert!(started.elapsed() < STALL / 2, "{:?}", started.elapsed());
    assert_eq!(
        received(&server).await.len(),
        1,
        "the queued call must not be sent"
    );
    busy.abort();

    let err = Client::builder()
        .api_key("k")
        .max_concurrent_requests(0)
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("max_concurrent_requests"), "{err}");
}

#[tokio::test]
async fn a_client_bounds_whole_calls_by_default() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_body()))
        .mount(&server)
        .await;

    let client = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .build()
        .unwrap();
    assert_eq!(
        client.total_timeout(),
        Some(kunobi_decision::DEFAULT_TOTAL_TIMEOUT)
    );

    // The bound is removable, for a caller who would rather let the retry
    // policy run to its end.
    let unbounded = Client::builder()
        .api_key("k")
        .base_url(server.uri())
        .total_timeout(None)
        .build()
        .unwrap();
    assert_eq!(unbounded.total_timeout(), None);

    // And a call can drop the client's bound without rebuilding the client.
    unbounded.models().list().total_timeout(None).await.unwrap();
    client
        .models()
        .list()
        .total_timeout(Duration::from_secs(5))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_retry_predicate_widens_what_is_retried() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(409),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;

    // 409 is not retryable by default, so without the predicate this is one request.
    let plain = client(&server).models().list().await.unwrap_err();
    assert_eq!(plain.status(), Some(StatusCode::CONFLICT));
    assert_eq!(received(&server).await.len(), 1);

    server.reset().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(409),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;

    let policy = fast_retry().retry_if(|error: &kunobi_decision::Error| {
        error.status().is_some_and(|status| status.as_u16() == 409)
    });
    let models = client(&server).models().list().retry(policy).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(received(&server).await.len(), 2);
}

#[tokio::test]
async fn a_retry_predicate_cannot_disable_the_built_in_rules() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(sequence(vec![
            ResponseTemplate::new(503),
            ResponseTemplate::new(200).set_body_json(models_body()),
        ]))
        .mount(&server)
        .await;

    // The predicate says no to everything; 503 is still retried.
    let policy = fast_retry().retry_if(|_: &kunobi_decision::Error| false);
    client(&server).models().list().retry(policy).await.unwrap();
    assert_eq!(received(&server).await.len(), 2);
}
