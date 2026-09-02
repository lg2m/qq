//! Authentication selection for the Amazon Bedrock provider family. Kept out
//! of the feature-gated AWS module so recipes stay constructible and
//! digestible in every build profile; only compilation needs the SDK.

use crate::credentials::SecretLiteral;

/// Authentication used by Amazon Bedrock Runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BedrockAuth {
    /// Uses the standard AWS credential and region provider chains.
    DefaultChain,
    /// Uses one named AWS profile.
    Profile(String),
    /// Uses an Amazon Bedrock API key as an HTTP bearer token.
    ApiKey(SecretLiteral),
}
