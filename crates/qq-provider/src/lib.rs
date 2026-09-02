//! Model-provider interfaces and adapters.

#![forbid(unsafe_code)]

use std::{pin::Pin, sync::Arc};

use futures_core::Stream;

// The AWS SDK family is the only heavy optional dependency closure; recipes
// and neutral types stay available without it.
#[cfg(feature = "provider-bedrock")]
mod aws;
mod bedrock_auth;
pub mod compiler;
mod construction;
mod credentials;
mod exchange;
mod http;
mod limits;
mod model;
mod providers;
mod request_auth;
mod sanitize;
mod sse;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;

pub use bedrock_auth::BedrockAuth;
pub use compiler::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler, ProviderRecipe,
};
pub use credentials::{SecretLiteral, SecretRef};
pub use model::{
    ContentBlock, Message, ModelRequest, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderUsage, Role, ToolSpec,
};
pub use qq_reasoning::ReasoningKind;
pub use request_auth::{
    RequestCredential, RequestCredentialError, RequestCredentialFuture, RequestCredentialProvider,
    SharedRequestCredentialProvider,
};

/// Canonical credential audience for the built-in xAI deployment.
pub const XAI_CREDENTIAL_ENDPOINT: &str = "https://api.x.ai";

/// A stream of semantic model events from a configured provider.
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>>;

/// The provider seam consumed by the agent runtime.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ProviderStream;
}

impl<T> Provider for Arc<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}

impl<T> Provider for Box<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}
