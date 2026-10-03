//! The blocking client, which owns its runtime.

#![cfg(feature = "blocking")]

use kunobi_decision::blocking::Client;
use kunobi_decision::{Questions, SystemOneRequest, noul};
use serde_json::json;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// wiremock needs a runtime to serve, the blocking client refuses to run inside
/// one, so the server runs here and the calls run on their own thread.
fn with_server<T: Send + 'static>(
    responses: impl FnOnce(&MockServer) -> Vec<Mock>,
    call: impl FnOnce(String) -> T + Send + 'static,
) -> T {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.block_on(MockServer::start());
    for mock in responses(&server) {
        runtime.block_on(server.register(mock));
    }
    let uri = server.uri();
    let handle = std::thread::spawn(move || call(uri));
    let result = handle.join().unwrap();
    drop(server);
    result
}

#[test]
fn it_answers_questions_and_lists_models() {
    let answered = with_server(
        |_| {
            vec![
                Mock::given(path("/v1/systemone")).respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({
                        "model": "jev-latest",
                        "answers": {"billing": {"type": "noul", "noul": 0.91}},
                        "usage": {"input_tokens": 12, "output_tokens": 3}
                    })),
                ),
                Mock::given(path("/v1/models")).respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"models": [{"name": "jev-latest"}]})),
                ),
            ]
        },
        |uri| {
            let client = Client::builder()
                .configure(|builder| builder.api_key("k").base_url(uri))
                .build()
                .unwrap();

            let models = client.models().unwrap();
            assert_eq!(models[0].name, "jev-latest");

            let mut questions = Questions::new();
            let billing = questions.add("billing", noul("Is this about billing?"));
            let result = client
                .system_one(SystemOneRequest::new("I was charged twice.", questions))
                .unwrap();
            result.answer(&billing).unwrap().noul
        },
    );
    assert_eq!(answered, 0.91);
}

#[test]
fn errors_come_back_like_the_async_client() {
    let status = with_server(
        |_| {
            vec![Mock::given(path("/v1/models")).respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "bad key"})),
            )]
        },
        |uri| {
            let client = Client::builder()
                .configure(|builder| {
                    builder
                        .api_key("k")
                        .base_url(uri)
                        .max_concurrent_requests(1)
                })
                .build()
                .unwrap();
            let err = client.models().unwrap_err();
            assert_eq!(err.to_string(), "401 bad key");
            err.status()
        },
    );
    assert_eq!(status.map(|status| status.as_u16()), Some(401));
}
