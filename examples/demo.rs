//! Run with `cargo run --example demo`. Needs TYPESAFE_API_KEY in the environment.

use std::time::Duration;

use kunobi_decision::{Client, Entry, Error, Questions, SystemOneRequest, choice_of, noul, score};
use serde_json::json;

kunobi_decision::labels! {
    /// The customer's tone.
    pub enum Tone {
        Calm = "calm",
        Frustrated = "frustrated": "Annoyed but civil",
        Angry = "angry": "Strong language or threats",
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::builder()
        .total_timeout(Duration::from_secs(15))
        .build()?;

    let models = client.models().list().await?;
    let names: Vec<_> = models.iter().map(|m| m.name.as_str()).collect();
    println!("Available models: {}", names.join(", "));

    let ticket = Entry::try_from(json!({
        "subject": "Charged twice this month",
        "body": "Hi, I see two charges of $49 on my card for August. I only have one account. Please fix this ASAP, I'm pretty frustrated."
    }))?;

    let mut questions = Questions::new();
    let is_billing = questions.add("isBilling", noul("Is this ticket about billing?"));
    let tone = questions.add("tone", choice_of::<Tone>("What is the customer's tone?"));
    let urgency = questions.add(
        "urgency",
        score(
            "How urgent is this ticket?",
            ["can wait", "this week", "today", "right now"],
        ),
    );

    let result = match client
        .system_one(SystemOneRequest::new(ticket, questions))
        .await
    {
        Ok(result) => result,
        Err(Error::Api(err)) => {
            eprintln!(
                "API error {} (request {}): {:?}",
                err.status(),
                err.request_id().unwrap_or("unknown"),
                err.body()
            );
            std::process::exit(1);
        }
        Err(err) => return Err(err.into()),
    };

    let tone = result.answer(&tone)?;
    match tone.decide(0.6) {
        Some(Tone::Angry) => println!("tone         angry: escalate"),
        Some(label) => println!("tone         {label:?}"),
        None => println!(
            "tone         unclear ({:.2} confidence): {:?}",
            tone.confidence, tone.probabilities
        ),
    }
    println!("billing?     {}", result.answer(&is_billing)?.is_yes(0.7));
    let urgency = result.answer(&urgency)?;
    println!(
        "urgency      {:.2} on a 0-3 scale, most likely level {:?}",
        urgency.score,
        urgency.most_likely_level()
    );
    println!(
        "tokens       {} in / {} out",
        // The API does not always report usage.
        result.usage.input_tokens.unwrap_or_default(),
        result.usage.output_tokens.unwrap_or_default()
    );
    Ok(())
}
