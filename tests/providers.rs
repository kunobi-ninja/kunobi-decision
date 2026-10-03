use kunobi_decision::{
    Client, Error, Provider, Questions, RetryPolicy, SystemOneRequest, choice_labels, noul, score,
};
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer, provider: Provider) -> Client {
    Client::builder()
        .provider(provider)
        .api_key("test-token")
        .base_url(server.uri())
        .build()
        .unwrap()
}
fn request() -> SystemOneRequest {
    let mut questions = Questions::new();
    questions.add("urgent", noul("Urgent?"));
    questions.add(
        "team",
        choice_labels("Which team?", ["billing", "technical"]),
    );
    questions.add("severity", score("How severe?", ["low", "high"]));
    SystemOneRequest::new(
        json!({"message":"Checkout is down"})
            .as_object()
            .unwrap()
            .clone(),
        questions,
    )
}
fn result(model: &str) -> serde_json::Value {
    json!({"model":model,"answers":{
        "urgent":{"type":"noul","noul":0.95},
        "team":{"type":"choice","choice":"technical","confidence":0.9,"probabilities":{"technical":0.9,"billing":0.1}},
        "severity":{"type":"score","score":0.8,"confidence":0.8,"legend":{"0":"low","1":"high"},"probabilities":{"0":0.2,"1":0.8}}
    },"usage":{"input_tokens":42,"output_tokens":0}})
}
fn cloudflare() -> Provider {
    Provider::Cloudflare {
        account_id: "account".into(),
    }
}

#[tokio::test]
async fn providers_send_typed_questions_and_preserve_answers_and_usage() {
    for (provider, route, model) in [
        (Provider::Liquid, "/v1/systemone", "d1:free"),
        (Provider::Vercel, "/v1/systemone", "typesafe-ai/jev"),
        (
            Provider::OpenRouter,
            "/v1/systemone",
            "~typesafe/jev-latest",
        ),
        (
            cloudflare(),
            "/client/v4/accounts/account/ai/run/@cf/cloudflare/clef",
            "clef",
        ),
    ] {
        let server = MockServer::start().await;
        let expected = result(model);
        let body = if matches!(provider, Provider::Cloudflare { .. }) {
            json!({"success":true,"result":expected})
        } else {
            expected.clone()
        };
        Mock::given(method("POST")).and(path(route)).and(header("authorization","Bearer test-token"))
            .and(body_partial_json(json!({"model":model,"state":{"message":"Checkout is down"},"questions":serde_json::to_value(request().questions).unwrap()})))
            .respond_with(ResponseTemplate::new(200).insert_header("cf-ray","request-42").set_body_json(body)).expect(1).mount(&server).await;
        let response = client(&server, provider)
            .system_one(request())
            .with_response()
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(response.data).unwrap(), expected);
        assert_eq!(response.request_id.as_deref(), Some("request-42"));
    }
}

#[tokio::test]
async fn vercel_d1_is_a_model_override_on_the_same_system_one_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/typesafe/v1/systemone"))
        .and(body_partial_json(json!({"model":"liquid/d1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(result("liquid/d1")))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::builder()
        .provider(Provider::Vercel)
        .api_key("key")
        .base_url(format!("{}/typesafe", server.uri()))
        .build()
        .unwrap();
    assert_eq!(
        client
            .system_one(request().model("liquid/d1"))
            .await
            .unwrap()
            .model,
        "liquid/d1"
    );
}

#[tokio::test]
async fn cloudflare_catalog_is_authenticated_and_excludes_non_decision_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/client/v4/accounts/account/ai/models/search"))
        .and(header("authorization","Bearer test-token")).and(query_param("search","@cf/cloudflare/clef"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success":true,"result":[{"name":"@cf/cloudflare/clef"},{"name":"@cf/cloudflare/clef-flash"},{"name":"@cf/cloudflare/other"}]}))).expect(1).mount(&server).await;
    let models = client(&server, cloudflare()).models().list().await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        ["clef", "clef-flash"]
    );
}

#[tokio::test]
async fn liquid_model_check_uses_inference_and_vercel_lists_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_partial_json(
            json!({"model":"d1:free","state":"ready"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(result("d1:free")))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client(&server, Provider::Liquid)
            .models()
            .list()
            .await
            .unwrap()[0]
            .name,
        "d1:free"
    );
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models":[{"name":"liquid/d1"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client(&server, Provider::Vercel)
            .models()
            .list()
            .await
            .unwrap()[0]
            .name,
        "liquid/d1"
    );
}

#[tokio::test]
async fn cloudflare_rejects_invalid_requests_without_network_io() {
    let server = MockServer::start().await;
    let c = client(&server, cloudflare());
    for request in [
        request().model("../escape"),
        request().extra("model", "clef-flash"),
    ] {
        assert!(matches!(
            c.system_one(request).await,
            Err(Error::InvalidRequest(_))
        ));
    }
    let mut q = Questions::new();
    for i in 0..65 {
        q.add(format!("q{i}"), noul("Ready?"));
    }
    assert!(matches!(
        c.system_one(SystemOneRequest::new("ready", q)).await,
        Err(Error::InvalidRequest(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        Client::builder()
            .provider(Provider::Cloudflare {
                account_id: "../other".into()
            })
            .api_key("key")
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn clef_flash_normalizes_full_model_ids_and_enforces_shared_transport_timeouts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/client/v4/accounts/account/ai/run/@cf/cloudflare/clef-flash",
        ))
        .and(body_partial_json(json!({"model":"clef-flash"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success":true,"result":result("clef-flash")}))
                .set_delay(Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server, cloudflare())
        .system_one(request().model("@cf/cloudflare/clef-flash"))
        .timeout(Duration::from_millis(30))
        .max_retries(0)
        .await
        .unwrap_err();
    assert!(err.is_timeout());
}

#[tokio::test]
async fn cloudflare_envelope_failures_and_http_errors_do_not_echo_state() {
    for body in [
        json!({"success":false,"errors":[{"message":"secret-state"}]}),
        json!({"success":true}),
        json!({"success":true,"result":{}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server, cloudflare())
            .system_one(request())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Decode { .. }));
        assert!(!err.to_string().contains("secret-state"));
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"message":"secret-state"})))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server, cloudflare())
        .system_one(request())
        .max_retries(0)
        .await
        .unwrap_err();
    assert_eq!(err.status().unwrap().as_u16(), 429);
    assert!(!err.to_string().contains("secret-state"));
}

#[tokio::test]
async fn cloudflare_reuses_retry_policy() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success":true,"result":result("clef")})),
        )
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    let retry = RetryPolicy {
        max_retries: 1,
        backoff_initial: Duration::from_millis(1),
        ..Default::default()
    };
    assert_eq!(
        client(&server, cloudflare())
            .system_one(request())
            .retry(retry)
            .await
            .unwrap()
            .model,
        "clef"
    );
}

fn openrouter_catalog() -> serde_json::Value {
    json!({"data":[
        {"id":"typesafe/jev-1.13","name":"Jev","description":"Decision model",
         "architecture":{"output_modalities":["decisions"]}},
        {"id":"test-vendor/decision-v2","name":"Fixture decision model",
         "architecture":{"output_modalities":["decisions"]}},
        {"id":"other/text","name":"Chat","architecture":{"output_modalities":["text"]}}
    ]})
}

fn openrouter_client(server: &MockServer) -> Client {
    Client::builder()
        .provider(Provider::OpenRouter)
        .api_key("test-token")
        .base_url(format!("{}/api", server.uri()))
        .build()
        .unwrap()
}

#[tokio::test]
async fn openrouter_catalog_returns_routable_decision_ids_without_testing_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .and(query_param("output_modalities", "decisions"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openrouter_catalog()))
        .expect(1)
        .mount(&server)
        .await;
    let models = openrouter_client(&server).models().list().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].name, "typesafe/jev-1.13");
    assert_eq!(models[0].description, "Decision model");
    assert_eq!(models[1].name, "test-vendor/decision-v2");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn openrouter_check_verifies_credentials_before_reading_the_public_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/key"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"label":"private-label","limit_remaining":42}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openrouter_catalog()))
        .expect(1)
        .mount(&server)
        .await;
    let models = openrouter_client(&server).check().await.unwrap();
    assert_eq!(models[0].name, "typesafe/jev-1.13");
    assert!(
        !serde_json::to_string(&models)
            .unwrap()
            .contains("private-label")
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.path(), "/api/v1/key");
    assert_eq!(requests[1].url.path(), "/api/v1/models");
}

#[tokio::test]
async fn openrouter_invalid_key_or_key_response_stops_before_catalog_and_inference() {
    for (status, body) in [
        (401, json!({"error":{"message":"Unauthorized","code":401}})),
        (200, json!({"data":null})),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/key"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let err = openrouter_client(&server).check().await.unwrap_err();
        if status == 401 {
            assert_eq!(err.status().unwrap().as_u16(), 401);
        } else {
            assert!(matches!(err, Error::Decode { .. }));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(!err.to_string().contains("test-token"));
    }
}

#[tokio::test]
async fn openrouter_check_has_one_total_budget_for_key_and_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{}}))
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openrouter_catalog())
                .set_delay(Duration::from_millis(400)),
        )
        .mount(&server)
        .await;
    let c = Client::builder()
        .provider(Provider::OpenRouter)
        .api_key("test-token")
        .base_url(format!("{}/api", server.uri()))
        .timeout(Duration::from_secs(1))
        .total_timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    assert!(c.check().await.unwrap_err().is_timeout());
}

#[tokio::test]
async fn openrouter_accepts_another_vendor_model_and_preserves_answers_after_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/systemone"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let mut expected = result("test-vendor/decision-v2");
    expected["id"] = json!("decision-42");
    expected["provider"] = json!("Fixture provider");
    expected["usage"]["cost"] = json!(0.001);
    Mock::given(method("POST"))
        .and(path("/api/v1/systemone"))
        .and(header("authorization", "Bearer test-token"))
        .and(header("x-typesafe-retry-count", "1"))
        .and(body_partial_json(
            json!({"model":"test-vendor/decision-v2"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "router-42")
                .set_body_json(expected),
        )
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    let out = openrouter_client(&server)
        .system_one(request().model("test-vendor/decision-v2"))
        .retry(RetryPolicy {
            max_retries: 1,
            backoff_initial: Duration::from_millis(1),
            ..Default::default()
        })
        .with_response()
        .await
        .unwrap();
    assert_eq!(out.data.model, "test-vendor/decision-v2");
    assert_eq!(
        serde_json::to_value(&out.data).unwrap()["answers"],
        result("test-vendor/decision-v2")["answers"]
    );
    assert_eq!(out.data.usage.input_tokens, Some(42));
    assert_eq!(out.request_id.as_deref(), Some("router-42"));
}
