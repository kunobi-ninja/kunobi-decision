//! The [`SystemOne`] trait, so code can depend on "something that answers questions".

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::answers::SystemOneResult;
use crate::client::Client;
use crate::error::Result;
use crate::request::SystemOneRequest;

/// The future returned by [`SystemOne::ask`].
pub type AskFuture<'a> = Pin<Box<dyn Future<Output = Result<SystemOneResult>> + Send + 'a>>;

/// Answers a [`SystemOneRequest`].
///
/// The trait carries only the request. Per-call options such as timeouts and retries
/// belong to the implementation: configure them on the [`Client`] you pass in.
///
/// Take `&dyn SystemOne` or `impl SystemOne` in product code instead of [`Client`],
/// and pass a fake in tests; see [`FakeSystemOne`](crate::testing::FakeSystemOne)
/// behind the `testing` feature.
///
/// ```
/// use kunobi_decision::{Questions, Result, SystemOne, SystemOneRequest, noul};
///
/// async fn is_billing(jev: &dyn SystemOne, ticket: &str) -> Result<bool> {
///     let mut questions = Questions::new();
///     let billing = questions.add("billing", noul("Is this ticket about billing?"));
///     let result = jev.ask(SystemOneRequest::new(ticket, questions)).await?;
///     Ok(result.answer(&billing)?.is_yes(0.7))
/// }
/// ```
pub trait SystemOne: Send + Sync {
    /// Answer the request with the implementation's default options.
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_>;
}

impl SystemOne for Client {
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_> {
        Box::pin(std::future::IntoFuture::into_future(
            self.system_one(request),
        ))
    }
}

impl<T: SystemOne + ?Sized> SystemOne for Arc<T> {
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_> {
        (**self).ask(request)
    }
}

impl<T: SystemOne + ?Sized> SystemOne for Box<T> {
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_> {
        (**self).ask(request)
    }
}

impl<T: SystemOne + ?Sized> SystemOne for &T {
    fn ask(&self, request: SystemOneRequest) -> AskFuture<'_> {
        (**self).ask(request)
    }
}
