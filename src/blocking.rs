//! A blocking client, for code that is not async. Enabled by the `blocking` feature.
//!
//! Every call runs on a runtime this client owns, so it cannot be used from
//! inside an async runtime: that would block the thread driving it. Calls from
//! an async context fail with [`Error::Config`] rather than panicking, which is
//! what the underlying runtime would do.
//!
//! ```no_run
//! # fn main() -> kunobi_decision::Result<()> {
//! use kunobi_decision::blocking::Client;
//! use kunobi_decision::{Questions, SystemOneRequest, noul};
//!
//! let client = Client::new()?;
//! let mut questions = Questions::new();
//! let billing = questions.add("billing", noul("Is this about billing?"));
//!
//! let result = client.system_one(SystemOneRequest::new("I was charged twice.", questions))?;
//! println!("{}", result.answer(&billing)?.noul);
//! # Ok(()) }
//! ```

use std::future::Future;

use crate::answers::SystemOneResult;
use crate::client::ClientBuilder;
use crate::error::{Error, Result};
use crate::request::SystemOneRequest;
use crate::types::ModelCard;

/// Blocking wrapper around [`crate::Client`].
///
/// Cloning is cheap: clones share one connection pool and one runtime.
#[derive(Debug, Clone)]
pub struct Client {
    inner: crate::Client,
    runtime: std::sync::Arc<OwnedRuntime>,
}

/// The client's runtime, with a drop that is safe anywhere.
///
/// Dropping a runtime inside an async context panics, and a blocking client is
/// easy to build in async code by mistake, so the last owner hands it to a
/// plain thread instead of panicking in someone's request handler.
#[derive(Debug)]
struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl OwnedRuntime {
    fn get(&self) -> &tokio::runtime::Runtime {
        self.0.as_ref().expect("the runtime lives until drop")
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        let Some(runtime) = self.0.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(move || drop(runtime));
        }
    }
}

impl Client {
    /// A client configured from the environment. See [`crate::Client::new`].
    pub fn new() -> Result<Self> {
        Self::from_async(crate::Client::new()?)
    }

    /// A builder for a blocking client, with the same options as the async one.
    pub fn builder() -> BlockingClientBuilder {
        BlockingClientBuilder {
            inner: crate::Client::builder(),
        }
    }

    /// Wrap an async client, giving it a runtime of its own.
    pub fn from_async(inner: crate::Client) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| Error::Config(format!("Could not create a runtime: {err}")))?;
        Ok(Self {
            inner,
            runtime: std::sync::Arc::new(OwnedRuntime(Some(runtime))),
        })
    }

    /// The async client underneath, for code that has a runtime already.
    pub fn as_async(&self) -> &crate::Client {
        &self.inner
    }

    /// Answer named questions about text or structured state.
    pub fn system_one(&self, request: SystemOneRequest) -> Result<SystemOneResult> {
        self.run(self.inner.system_one(request))
    }

    /// List the models available to the account.
    pub fn models(&self) -> Result<Vec<ModelCard>> {
        self.run(self.inner.models().list())
    }

    fn run<T: Send + 'static>(
        &self,
        call: impl std::future::IntoFuture<Output = Result<T>, IntoFuture: Future<Output = Result<T>>>,
    ) -> Result<T> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(Error::Config(
                "kunobi_decision::blocking::Client was called from inside an async runtime, which \
                 would block the thread driving it. Use kunobi_decision::Client there, or call this \
                 from a thread with no runtime (for example tokio::task::spawn_blocking)."
                    .into(),
            ));
        }
        self.runtime.get().block_on(call.into_future())
    }
}

/// Builder for a blocking [`Client`].
#[derive(Debug)]
pub struct BlockingClientBuilder {
    inner: ClientBuilder,
}

impl BlockingClientBuilder {
    /// Configure the underlying async client.
    ///
    /// ```no_run
    /// # fn main() -> kunobi_decision::Result<()> {
    /// use std::time::Duration;
    ///
    /// let client = kunobi_decision::blocking::Client::builder()
    ///     .configure(|builder| builder.api_key("sk-…").timeout(Duration::from_secs(5)))
    ///     .build()?;
    /// # let _ = client;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn configure(mut self, configure: impl FnOnce(ClientBuilder) -> ClientBuilder) -> Self {
        self.inner = configure(self.inner);
        self
    }

    /// Build the blocking client.
    pub fn build(self) -> Result<Client> {
        Client::from_async(self.inner.build()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocking on a runtime from inside one deadlocks or panics, depending on
    /// the flavour. Neither is a good answer for a caller who picked the wrong
    /// client, so say which client to use instead.
    #[tokio::test]
    async fn calling_from_an_async_runtime_is_refused() {
        let client = Client::builder()
            // No request is sent. A loopback root keeps this test usable without TLS.
            .configure(|builder| builder.api_key("k").base_url("http://localhost:1234"))
            .build()
            .unwrap();
        let mut questions = crate::Questions::new();
        questions.add("q", crate::noul("q?"));

        let err = client
            .system_one(SystemOneRequest::new("state", questions))
            .unwrap_err();
        assert!(err.to_string().contains("inside an async runtime"), "{err}");
        assert!(err.to_string().contains("kunobi_decision::Client"), "{err}");
    }
}
