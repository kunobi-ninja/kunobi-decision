//! Live tests against the TypeSafe API.
//!
//! Ignored by default. Run with a key:
//!
//! ```sh
//! TYPESAFE_API_KEY=... cargo test --test e2e_api -- --ignored --nocapture
//! ```

use kunobi_decision::{Client, Entry, Questions, SystemOneRequest, choice, choice_of, noul, score};
use serde_json::json;

kunobi_decision::labels! {
    enum Channel {
        Payments = "payments": "Payment providers such as Stripe",
        Email = "email",
        Other = "other",
    }
}

fn client() -> Client {
    Client::new().expect("set TYPESAFE_API_KEY to run the live API tests")
}

#[tokio::test]
#[ignore = "calls the live API; needs TYPESAFE_API_KEY"]
async fn lists_models() {
    let models = client().models().list().await.unwrap();
    assert!(
        !models.is_empty(),
        "the account should see at least one model"
    );
    println!(
        "models: {:?}",
        models.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "calls the live API; needs TYPESAFE_API_KEY"]
async fn answers_all_question_types() {
    let mut questions = Questions::new();
    let department = questions.add(
        "department",
        choice(
            "Which team should handle this",
            [
                ("billing", "Payment or subscription issues"),
                ("technical", "Bugs or integration problems"),
                ("sales", "Pricing or account questions"),
            ],
        ),
    );
    let frustration = questions.add(
        "frustration",
        score(
            "How frustrated the customer appears",
            [
                "Calm, just stating facts",
                "Frustrated but civil",
                "Very angry, strong language",
            ],
        ),
    );
    let urgent = questions.add(
        "is_urgent",
        noul("The message conveys urgency or time-sensitivity"),
    );
    let channel = questions.add(
        "channel",
        choice_of::<Channel>("Which integration is failing?"),
    );

    let state = Entry::try_from(json!({
        "message": "Hi, I've been trying to connect my Stripe account for 3 days and it keeps failing. I'm losing sales. Please help ASAP."
    }))
    .unwrap();
    let response = client()
        .system_one(SystemOneRequest::new(state, questions))
        .with_response()
        .await
        .unwrap();
    let result = &response.data;

    let department = result.answer(&department).unwrap();
    assert_eq!(department.probabilities.len(), 3);
    assert!((0.0..=1.0).contains(&department.confidence));
    let frustration = result.answer(&frustration).unwrap();
    assert!((0.0..=2.0).contains(&frustration.score));
    let urgent = result.answer(&urgent).unwrap();
    let channel = result.answer(&channel).unwrap();
    assert_eq!(channel.probabilities.len(), 3);
    assert!((0.0..=1.0).contains(&urgent.noul));
    assert!(result.usage.input_tokens.is_some_and(|tokens| tokens > 0));

    println!(
        "request {:?}: department={} ({:.2}), frustration={:.2}, urgent={:.2}",
        response.request_id,
        department.choice,
        department.confidence,
        frustration.score,
        urgent.noul
    );
}
