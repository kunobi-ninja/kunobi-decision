//! Typed decision clients for TypeSafe Jev, Cloudflare Clef, Liquid d1, Vercel and OpenRouter.
//! Select an access service with [`ClientBuilder::provider`].
//!
//! Send state and typed questions to a decision model; get structured answers with
//! probabilities and confidence that code can act on directly.
//!
//! - [`noul`]: a yes/no question, answered with the probability of yes.
//! - [`choice`]: pick one label, answered with probabilities and confidence.
//! - [`score`]: rate against ordered levels, answered with an expected score.
//!
//! ```no_run
//! # async fn example() -> kunobi_decision::Result<()> {
//! use kunobi_decision::{Client, Questions, SystemOneRequest, choice_labels, noul, score};
//!
//! let client = Client::new()?; // reads TYPESAFE_API_KEY
//!
//! let mut questions = Questions::new();
//! let billing = questions.add("billing", noul("Is this ticket about billing?"));
//! let tone = questions.add("tone", choice_labels("What is the tone?", ["calm", "frustrated", "angry"]));
//! let urgency = questions.add("urgency", score("How urgent?", ["can wait", "today", "right now"]));
//!
//! let result = client
//!     .system_one(SystemOneRequest::new("I was charged twice. Fix this ASAP.", questions))
//!     .await?;
//!
//! let tone = result.answer(&tone)?;
//! if tone.confidence > 0.6 {
//!     println!("tone: {}", tone.choice);
//! }
//! println!("billing: {:.2}", result.answer(&billing)?.noul);
//! println!("urgency: {:.2}", result.answer(&urgency)?.score);
//! # Ok(()) }
//! ```
//!
//! Calls retry 408, 429 and 5xx responses, connection errors and timeouts with
//! capped exponential backoff, honoring `Retry-After`. See [`RetryPolicy`].
//! Requests log through [`tracing`]: summaries at `info` and redacted headers at
//! `debug`. Bodies are logged only with [`ClientBuilder::log_bodies`].
//!
//! Authenticate with an API key, or with a [`CredentialProvider`] when the client
//! runs on user machines and must not hold a TypeSafe key. See [`credentials`].

#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod answers;
#[cfg(feature = "blocking")]
#[cfg_attr(docsrs, doc(cfg(feature = "blocking")))]
pub mod blocking;
pub mod client;
pub mod credentials;
pub mod error;
pub mod labels;
pub mod questions;
pub mod request;
pub mod retry;
pub mod system_one;
#[cfg(feature = "testing")]
#[cfg_attr(docsrs, doc(cfg(feature = "testing")))]
pub mod testing;
pub mod types;

mod redact;

pub use answers::{
    Answer, AnswerKind, ChoiceAnswer, NoulAnswer, ScoreAnswer, SystemOneResult, TypedChoiceAnswer,
};
pub use client::{
    Call, Client, ClientBuilder, DEFAULT_BASE_URL, DEFAULT_MODEL, ENV_API_KEY, ENV_BASE_URL,
    ENV_DEFAULT_MODEL, Models, Provider, RawResponse, WithResponse,
};
pub use credentials::{BoxError, CredentialProvider, ExposeSecret, SecretString, TokenFuture};
pub use error::{ApiError, ApiErrorKind, Error, ErrorBody, REQUEST_ID_HEADER, Result};
pub use labels::Labels;
pub use questions::{
    AnswerKey, ChoiceQuestion, NoulCriteria, NoulQuestion, Question, QuestionKind, Questions,
    ScoreQuestion, TypedChoice, choice, choice_labels, choice_of, noul, score,
};
pub use request::SystemOneRequest;
pub use retry::{DEFAULT_TIMEOUT, DEFAULT_TOTAL_TIMEOUT, RetryPolicy, parse_retry_after};
pub use system_one::{AskFuture, SystemOne};
pub use types::{Entry, ModelCard, Usage};

/// Re-exported so callers can build headers and HTTP clients with matching versions.
pub use reqwest;

/// This crate's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
