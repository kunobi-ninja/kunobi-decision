//! A scripted [`SystemOne`] for tests. Enabled by the `testing` feature.
//!
//! ```
//! use kunobi_decision::testing::FakeSystemOne;
//! use kunobi_decision::{Questions, SystemOne, SystemOneRequest, choice_of, noul};
//!
//! kunobi_decision::labels! {
//!     pub enum Tone { Calm = "calm", Angry = "angry" }
//! }
//!
//! # tokio_test_block(async {
//! let jev = FakeSystemOne::new()
//!     .noul("billing", 0.92)
//!     .choice_of("tone", Tone::Angry, 0.81);
//!
//! let mut questions = Questions::new();
//! let billing = questions.add("billing", noul("Is this about billing?"));
//! let tone = questions.add("tone", choice_of::<Tone>("What is the tone?"));
//! let result = jev.ask(SystemOneRequest::new("I was charged twice!", questions)).await.unwrap();
//!
//! assert!(result.answer(&billing).unwrap().is_yes(0.9));
//! assert_eq!(result.answer(&tone).unwrap().decide(0.8), Some(Tone::Angry));
//! assert_eq!(jev.requests().len(), 1);
//! # });
//! # fn tokio_test_block(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
//! # }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde_json::Value;

use crate::answers::{Answer, ChoiceAnswer, NoulAnswer, ScoreAnswer, SystemOneResult};
use crate::error::{Error, Result};
use crate::labels::Labels;
use crate::questions::Question;
use crate::request::SystemOneRequest;
use crate::system_one::{AskFuture, SystemOne};
use crate::types::Usage;

/// Model name reported by fake results unless the request sets one.
pub const FAKE_MODEL: &str = "fake";

enum Scripted {
    Noul(f64),
    Choice { label: String, confidence: f64 },
    Score { score: f64, confidence: f64 },
    Raw(Answer),
}

type ErrorFactory = Arc<dyn Fn() -> Error + Send + Sync>;

#[derive(Default)]
struct State {
    answers: HashMap<String, Scripted>,
    failure: Option<ErrorFactory>,
    requests: Vec<SystemOneRequest>,
}

/// A [`SystemOne`] that answers from a script and records every request.
///
/// It validates requests like the real client and checks that each scripted
/// answer fits its question: a choice label must be one of the question's
/// labels, and a score must fall within its rubric. Clones share the script
/// and the recorded requests.
#[derive(Clone, Default)]
pub struct FakeSystemOne {
    state: Arc<Mutex<State>>,
}

impl FakeSystemOne {
    /// A fake with no scripted answers.
    pub fn new() -> Self {
        Self::default()
    }

    fn script(self, name: impl Into<String>, answer: Scripted) -> Self {
        self.lock().answers.insert(name.into(), answer);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Answer the yes/no question `name` with this probability of yes.
    pub fn noul(self, name: impl Into<String>, probability: f64) -> Self {
        self.script(name, Scripted::Noul(probability))
    }

    /// Answer the choice question `name` with `label` at this confidence.
    ///
    /// The chosen label gets `confidence` as its probability; the others share the rest.
    pub fn choice(
        self,
        name: impl Into<String>,
        label: impl Into<String>,
        confidence: f64,
    ) -> Self {
        self.script(
            name,
            Scripted::Choice {
                label: label.into(),
                confidence,
            },
        )
    }

    /// Answer a typed choice question with a label from its enum.
    pub fn choice_of<L: Labels>(self, name: impl Into<String>, label: L, confidence: f64) -> Self {
        self.choice(name, label.label(), confidence)
    }

    /// Answer the score question `name` with this expected score and confidence.
    pub fn score(self, name: impl Into<String>, score: f64, confidence: f64) -> Self {
        self.script(name, Scripted::Score { score, confidence })
    }

    /// Answer question `name` with exactly this answer, without fitting it to the question.
    pub fn answer(self, name: impl Into<String>, answer: Answer) -> Self {
        self.script(name, Scripted::Raw(answer))
    }

    /// Fail every request with the error `error` builds.
    pub fn fail_with(self, error: impl Fn() -> Error + Send + Sync + 'static) -> Self {
        self.lock().failure = Some(Arc::new(error));
        self
    }

    /// Requests received so far, including rejected ones.
    pub fn requests(&self) -> Vec<SystemOneRequest> {
        self.lock().requests.clone()
    }

    fn respond(&self, request: SystemOneRequest) -> Result<SystemOneResult> {
        let failure = {
            let mut state = self.lock();
            state.requests.push(request.clone());
            state.failure.clone()
        };
        // The same checks the client runs before sending.
        request.to_body(FAKE_MODEL)?;
        // Called without the lock, so the closure may use the fake.
        if let Some(failure) = failure {
            return Err(failure());
        }

        let state = self.lock();
        let mut answers = IndexMap::new();
        for (name, question) in request.questions.iter() {
            let scripted = state.answers.get(name).ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "FakeSystemOne has no answer for question \"{name}\"."
                ))
            })?;
            answers.insert(name.to_owned(), fit(name, question, scripted)?);
        }
        Ok(SystemOneResult {
            model: request.model.unwrap_or_else(|| FAKE_MODEL.to_owned()),
            answers,
            usage: Usage::default(),
        })
    }
}

impl SystemOne for FakeSystemOne {
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_> {
        let result = self.respond(request);
        Box::pin(async move { result })
    }
}

impl fmt::Debug for FakeSystemOne {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        let mut scripted: Vec<_> = state.answers.keys().collect();
        scripted.sort();
        f.debug_struct("FakeSystemOne")
            .field("scripted", &scripted)
            .field("requests", &state.requests.len())
            .finish_non_exhaustive()
    }
}

/// Build the wire answer for a scripted value, checking it fits the question.
fn fit(name: &str, question: &Question, scripted: &Scripted) -> Result<Answer> {
    let mismatch = |detail: String| {
        Error::InvalidRequest(format!("FakeSystemOne answer for \"{name}\" {detail}."))
    };
    let probability = |value: f64| {
        if (0.0..=1.0).contains(&value) {
            Ok(value)
        } else {
            Err(mismatch(format!(
                "has probability {value}; use a value between 0 and 1"
            )))
        }
    };
    match (question, scripted) {
        (_, Scripted::Raw(answer)) => Ok(answer.clone()),
        (Question::Noul(_), Scripted::Noul(noul)) => Ok(Answer::Noul(NoulAnswer {
            noul: probability(*noul)?,
        })),
        (Question::Choice(choice), Scripted::Choice { label, confidence }) => {
            let confidence = &probability(*confidence)?;
            if !choice.criteria.contains_key(label) {
                let labels: Vec<_> = choice.criteria.keys().map(String::as_str).collect();
                return Err(mismatch(format!(
                    "uses label \"{label}\", which is not one of {}",
                    labels.join(", ")
                )));
            }
            let others = (choice.criteria.len() - 1).max(1) as f64;
            let probabilities = choice
                .criteria
                .keys()
                .map(|candidate| {
                    let p = if candidate == label {
                        *confidence
                    } else {
                        (1.0 - confidence).max(0.0) / others
                    };
                    (candidate.clone(), p)
                })
                .collect();
            Ok(Answer::Choice(ChoiceAnswer {
                choice: label.clone(),
                confidence: *confidence,
                probabilities,
            }))
        }
        (Question::Score(rubric), Scripted::Score { score, confidence }) => {
            let confidence = &probability(*confidence)?;
            let top = (rubric.criteria.len() - 1) as f64;
            if !(0.0..=top).contains(score) {
                return Err(mismatch(format!("has score {score}, outside 0..={top}")));
            }
            let nearest = score.round() as u32;
            let others = top.max(1.0);
            let legend: BTreeMap<u32, Value> = rubric
                .criteria
                .iter()
                .enumerate()
                .map(|(level, entry)| (level as u32, Value::from(entry.clone())))
                .collect();
            let probabilities = legend
                .keys()
                .map(|&level| {
                    let p = if level == nearest {
                        *confidence
                    } else {
                        (1.0 - confidence).max(0.0) / others
                    };
                    (level, p)
                })
                .collect();
            Ok(Answer::Score(ScoreAnswer {
                score: *score,
                confidence: *confidence,
                legend,
                probabilities,
            }))
        }
        (question, _) => {
            let expected = match question {
                Question::Noul(_) => "noul",
                Question::Choice(_) => "choice",
                Question::Score(_) => "score",
                // A raw question carries whatever type the caller wrote, so the
                // only script that can answer it is a raw answer.
                Question::Raw(raw) => raw
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("raw"),
            };
            let got = match scripted {
                Scripted::Noul(_) => "noul",
                Scripted::Choice { .. } => "choice",
                Scripted::Score { .. } => "score",
                Scripted::Raw(_) => unreachable!("raw answers match any question"),
            };
            Err(mismatch(format!(
                "is a {got} answer, but the question is a {expected}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::questions::{Questions, choice_labels, noul, score};

    fn ask(fake: &FakeSystemOne, questions: Questions) -> Result<SystemOneResult> {
        fake.respond(SystemOneRequest::new("state", questions))
    }

    #[test]
    fn answers_fit_the_questions() {
        let fake = FakeSystemOne::new()
            .choice("tone", "angry", 0.7)
            .score("urgency", 1.6, 0.5);
        let mut questions = Questions::new();
        let tone = questions.add("tone", choice_labels("Tone?", ["calm", "angry", "sad"]));
        let urgency = questions.add("urgency", score("Urgency?", ["low", "mid", "high"]));
        let result = ask(&fake, questions).unwrap();

        let tone = result.answer(&tone).unwrap();
        assert_eq!(tone.choice, "angry");
        let order: Vec<_> = tone.probabilities.keys().map(String::as_str).collect();
        assert_eq!(order, ["calm", "angry", "sad"]);
        assert!((tone.probability("calm").unwrap() - 0.15).abs() < 1e-9);

        assert_eq!(tone.probability("angry"), Some(0.7));
        assert_eq!(tone.confidence, 0.7);

        let urgency = result.answer(&urgency).unwrap();
        assert_eq!(urgency.score, 1.6);
        assert_eq!(urgency.confidence, 0.5);
        assert_eq!(urgency.most_likely_level(), Some(2));
        assert_eq!(urgency.legend[&0], Value::from("low"));
        assert_eq!(result.model, FAKE_MODEL);
    }

    #[test]
    fn mismatched_scripts_are_errors() {
        let mut questions = Questions::new();
        questions.add("tone", choice_labels("Tone?", ["calm", "angry"]));

        let unknown_label = FakeSystemOne::new().choice("tone", "happy", 0.9);
        let err = ask(&unknown_label, questions.clone()).unwrap_err();
        assert!(
            err.to_string()
                .contains("\"happy\", which is not one of calm, angry"),
            "{err}"
        );

        let wrong_type = FakeSystemOne::new().noul("tone", 0.9);
        let err = ask(&wrong_type, questions.clone()).unwrap_err();
        assert!(
            err.to_string()
                .contains("is a noul answer, but the question is a choice"),
            "{err}"
        );

        let missing = FakeSystemOne::new();
        let err = ask(&missing, questions).unwrap_err();
        assert!(
            err.to_string().contains("no answer for question \"tone\""),
            "{err}"
        );

        let mut scores = Questions::new();
        scores.add("level", score("Level?", ["low", "high"]));
        let out_of_range = FakeSystemOne::new().score("level", 2.5, 0.9);
        let err = ask(&out_of_range, scores).unwrap_err();
        assert!(err.to_string().contains("outside 0..=1"), "{err}");
    }

    #[test]
    fn invalid_requests_and_scripted_failures_are_recorded() {
        let fake = FakeSystemOne::new().fail_with(|| Error::InvalidRequest("boom".into()));
        let err = ask(&fake, Questions::new()).unwrap_err();
        assert_eq!(err.to_string(), "At least one question is required.");

        let mut questions = Questions::new();
        questions.add("q", noul("q"));
        let err = ask(&fake, questions).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(fake.clone().requests().len(), 2);
    }
}

#[cfg(test)]
mod script_checks {
    use super::*;
    use crate::questions::{Questions, choice_labels, noul, score};

    #[test]
    fn probabilities_outside_zero_to_one_are_rejected() {
        let mut questions = Questions::new();
        questions.add("tone", choice_labels("Tone?", ["calm", "angry"]));
        questions.add("urgent", noul("Urgent?"));
        questions.add("level", score("Level?", ["low", "high"]));
        let cases = [
            FakeSystemOne::new()
                .choice("tone", "angry", 1.5)
                .noul("urgent", 0.5)
                .score("level", 1.0, 0.5),
            FakeSystemOne::new()
                .choice("tone", "angry", 0.5)
                .noul("urgent", -0.1)
                .score("level", 1.0, 0.5),
            FakeSystemOne::new()
                .choice("tone", "angry", 0.5)
                .noul("urgent", 0.5)
                .score("level", 1.0, f64::NAN),
        ];
        for fake in cases {
            let err = fake
                .respond(SystemOneRequest::new("s", questions.clone()))
                .unwrap_err();
            assert!(err.to_string().contains("between 0 and 1"), "{err}");
        }
    }

    #[test]
    fn a_failure_closure_can_use_the_fake() {
        let fake = FakeSystemOne::new();
        let inner = fake.clone();
        let fake = fake.fail_with(move || {
            Error::InvalidRequest(format!("seen {} requests", inner.requests().len()))
        });
        let mut questions = Questions::new();
        questions.add("q", noul("q"));

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = fake.respond(SystemOneRequest::new("s", questions));
            done.send(result.unwrap_err().to_string()).unwrap();
        });
        let message = finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("fail_with closure deadlocked on the fake's lock");
        assert_eq!(message, "seen 1 requests");
    }
}
