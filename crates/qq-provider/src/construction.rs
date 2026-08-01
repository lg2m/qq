//! Shared construction for compiled direct HTTP providers.
//!
//! This module owns protocol/authentication compatibility and concrete adapter
//! selection. Callers must resolve endpoints and supply an HTTP client before
//! entering this boundary.

use crate::{
    Provider, ProviderError, ProviderStream, SharedRequestCredentialProvider,
    anthropic::{AnthropicAuth, AnthropicMessages},
    credentials::SecretLiteral,
    google::{GoogleAuth, GoogleEndpoint, GoogleGenerateContent},
    openai::{OpenAi, ResponsesAuth, ResponsesConstructionAuth},
    openai_chat::{ChatCompletionsAuth, OpenAiChatCompletions},
    request_auth::RequestAuthorizer,
};

/// Whether a resolved URL represents a protocol-independent base or an exact
/// request endpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpEndpointKind {
    Base,
    Exact,
}

/// Coherent authorization intent accepted by compiled HTTP construction.
pub(crate) enum HttpConstructionAuth {
    NoAuth,
    ApiKey(SecretLiteral),
    Bearer(SecretLiteral),
    Header(String, SecretLiteral),
    Codex {
        access_token: SecretLiteral,
        account_id: SecretLiteral,
        is_fedramp: bool,
    },
    RequestTimeBearer(SharedRequestCredentialProvider),
    RequestTimeCodex(SharedRequestCredentialProvider),
    MantleSigV4(RequestAuthorizer),
}

/// Fully resolved inputs for constructing one compiled HTTP provider.
pub(crate) struct HttpConstructionSpec {
    pub(crate) protocol: crate::compiler::HttpProtocol,
    pub(crate) endpoint: reqwest::Url,
    pub(crate) endpoint_kind: HttpEndpointKind,
    pub(crate) auth: HttpConstructionAuth,
    pub(crate) headers: Vec<(String, String)>,
}

/// The concrete HTTP provider selected by compiled construction.
pub(crate) enum CompiledHttpProvider {
    OpenAiResponses(OpenAi),
    OpenAiChatCompletions(OpenAiChatCompletions),
    AnthropicMessages(AnthropicMessages),
    GoogleGenerateContent(GoogleGenerateContent),
}

impl Provider for CompiledHttpProvider {
    fn stream(&self, request: crate::ModelRequest) -> ProviderStream {
        match self {
            Self::OpenAiResponses(provider) => provider.stream(request),
            Self::OpenAiChatCompletions(provider) => provider.stream(request),
            Self::AnthropicMessages(provider) => provider.stream(request),
            Self::GoogleGenerateContent(provider) => provider.stream(request),
        }
    }
}

/// Constructs one HTTP adapter from resolved inputs without creating clients or
/// performing network I/O.
pub(crate) fn construct_http_provider(
    client: reqwest::Client,
    spec: HttpConstructionSpec,
) -> Result<CompiledHttpProvider, ProviderError> {
    match spec.protocol {
        crate::compiler::HttpProtocol::OpenAiResponses => {
            let auth = responses_auth(spec.auth)?;
            Ok(CompiledHttpProvider::OpenAiResponses(
                OpenAi::with_client_and_auth(client, spec.endpoint, auth, spec.headers)?,
            ))
        }
        crate::compiler::HttpProtocol::OpenAiChatCompletions => {
            let (auth, authorizer) = chat_completions_auth(spec.auth)?;
            Ok(CompiledHttpProvider::OpenAiChatCompletions(
                OpenAiChatCompletions::with_client_and_authorizer(
                    client,
                    spec.endpoint,
                    auth,
                    spec.headers,
                    authorizer,
                )?,
            ))
        }
        crate::compiler::HttpProtocol::AnthropicMessages => {
            let (auth, authorizer) = anthropic_auth(spec.auth)?;
            Ok(CompiledHttpProvider::AnthropicMessages(
                AnthropicMessages::with_client_and_authorizer(
                    client,
                    spec.endpoint,
                    auth,
                    spec.headers,
                    authorizer,
                )?,
            ))
        }
        crate::compiler::HttpProtocol::GoogleGenerateContent => {
            let auth = google_auth(spec.auth)?;
            let endpoint_kind = match spec.endpoint_kind {
                HttpEndpointKind::Base => GoogleEndpoint::Base,
                HttpEndpointKind::Exact => GoogleEndpoint::Exact,
            };
            Ok(CompiledHttpProvider::GoogleGenerateContent(
                GoogleGenerateContent::with_client(
                    client,
                    spec.endpoint,
                    endpoint_kind,
                    auth,
                    spec.headers,
                )?,
            ))
        }
    }
}

fn responses_auth(auth: HttpConstructionAuth) -> Result<ResponsesConstructionAuth, ProviderError> {
    match auth {
        HttpConstructionAuth::NoAuth => {
            Ok(ResponsesConstructionAuth::Static(ResponsesAuth::NoAuth))
        }
        HttpConstructionAuth::ApiKey(secret) | HttpConstructionAuth::Bearer(secret) => Ok(
            ResponsesConstructionAuth::Static(ResponsesAuth::Bearer(secret)),
        ),
        HttpConstructionAuth::Header(name, secret) => Ok(ResponsesConstructionAuth::Static(
            ResponsesAuth::Header(name, secret),
        )),
        HttpConstructionAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => Ok(ResponsesConstructionAuth::Static(ResponsesAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        })),
        HttpConstructionAuth::RequestTimeBearer(credentials) => {
            Ok(ResponsesConstructionAuth::RequestTimeBearer(credentials))
        }
        HttpConstructionAuth::RequestTimeCodex(credentials) => {
            Ok(ResponsesConstructionAuth::RequestTimeCodex(credentials))
        }
        HttpConstructionAuth::MantleSigV4(authorizer) => {
            Ok(ResponsesConstructionAuth::MantleSigV4(authorizer))
        }
    }
}

fn chat_completions_auth(
    auth: HttpConstructionAuth,
) -> Result<(ChatCompletionsAuth, RequestAuthorizer), ProviderError> {
    match auth {
        HttpConstructionAuth::NoAuth => {
            Ok((ChatCompletionsAuth::NoAuth, RequestAuthorizer::default()))
        }
        HttpConstructionAuth::ApiKey(secret) | HttpConstructionAuth::Bearer(secret) => Ok((
            ChatCompletionsAuth::Bearer(secret),
            RequestAuthorizer::default(),
        )),
        HttpConstructionAuth::Header(name, secret) => Ok((
            ChatCompletionsAuth::Header(name, secret),
            RequestAuthorizer::default(),
        )),
        HttpConstructionAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
        HttpConstructionAuth::RequestTimeBearer(credentials) => Ok((
            ChatCompletionsAuth::NoAuth,
            RequestAuthorizer::request_time_bearer(credentials),
        )),
        HttpConstructionAuth::RequestTimeCodex(_) => Err(ProviderError::Configuration(
            "request-time Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
        HttpConstructionAuth::MantleSigV4(authorizer) => {
            Ok((ChatCompletionsAuth::NoAuth, authorizer))
        }
    }
}

fn anthropic_auth(
    auth: HttpConstructionAuth,
) -> Result<(AnthropicAuth, RequestAuthorizer), ProviderError> {
    match auth {
        HttpConstructionAuth::NoAuth => Ok((AnthropicAuth::NoAuth, RequestAuthorizer::default())),
        HttpConstructionAuth::ApiKey(secret) => {
            Ok((AnthropicAuth::XApiKey(secret), RequestAuthorizer::default()))
        }
        HttpConstructionAuth::Bearer(secret) => {
            Ok((AnthropicAuth::Bearer(secret), RequestAuthorizer::default()))
        }
        HttpConstructionAuth::Header(name, secret) => Ok((
            AnthropicAuth::Header(name, secret),
            RequestAuthorizer::default(),
        )),
        HttpConstructionAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
        HttpConstructionAuth::RequestTimeBearer(credentials) => Ok((
            AnthropicAuth::NoAuth,
            RequestAuthorizer::request_time_bearer(credentials),
        )),
        HttpConstructionAuth::RequestTimeCodex(_) => Err(ProviderError::Configuration(
            "request-time Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
        HttpConstructionAuth::MantleSigV4(authorizer) => Ok((AnthropicAuth::NoAuth, authorizer)),
    }
}

fn google_auth(auth: HttpConstructionAuth) -> Result<GoogleAuth, ProviderError> {
    match auth {
        HttpConstructionAuth::NoAuth => Ok(GoogleAuth::NoAuth),
        HttpConstructionAuth::ApiKey(secret) => Ok(GoogleAuth::XGoogApiKey(secret)),
        HttpConstructionAuth::Bearer(secret) => Ok(GoogleAuth::Bearer(secret)),
        HttpConstructionAuth::Header(name, secret) => Ok(GoogleAuth::Header(name, secret)),
        HttpConstructionAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
        HttpConstructionAuth::RequestTimeBearer(_) | HttpConstructionAuth::RequestTimeCodex(_) => {
            Err(ProviderError::Configuration(
                "request-time credentials are not supported by Google GenerateContent".to_owned(),
            ))
        }
        HttpConstructionAuth::MantleSigV4(_) => Err(ProviderError::Configuration(
            "Google GenerateContent is not supported by Amazon Bedrock Mantle".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RequestCredential, RequestCredentialFuture, RequestCredentialProvider,
        compiler::HttpProtocol,
    };

    struct StaticBearerCredentials;

    impl RequestCredentialProvider for StaticBearerCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            Box::pin(async { RequestCredential::bearer("test-bearer") })
        }
    }

    struct StaticCodexCredentials;

    impl RequestCredentialProvider for StaticCodexCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            Box::pin(async { RequestCredential::codex("test-codex", "test-account", false) })
        }
    }

    fn spec(protocol: HttpProtocol, auth: HttpConstructionAuth) -> HttpConstructionSpec {
        HttpConstructionSpec {
            protocol,
            endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
            endpoint_kind: HttpEndpointKind::Exact,
            auth,
            headers: Vec::new(),
        }
    }

    #[test]
    fn constructs_every_protocol_without_network_io() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            construct_http_provider(
                reqwest::Client::new(),
                spec(protocol, HttpConstructionAuth::ApiKey("test-secret".into())),
            )
            .unwrap();
        }
    }

    #[test]
    fn constructs_every_supported_direct_auth_intent_without_network_io() {
        let cases = [
            HttpConstructionAuth::NoAuth,
            HttpConstructionAuth::ApiKey("test-api-key".into()),
            HttpConstructionAuth::Bearer("test-bearer".into()),
            HttpConstructionAuth::Header("x-test-key".to_owned(), "test-value".into()),
            HttpConstructionAuth::Codex {
                access_token: "test-codex".into(),
                account_id: "test-account".into(),
                is_fedramp: false,
            },
            HttpConstructionAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                StaticBearerCredentials,
            )),
            HttpConstructionAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                StaticCodexCredentials,
            )),
        ];

        for auth in cases {
            construct_http_provider(
                reqwest::Client::new(),
                HttpConstructionSpec {
                    protocol: HttpProtocol::OpenAiResponses,
                    endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
                    endpoint_kind: HttpEndpointKind::Base,
                    auth,
                    headers: Vec::new(),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn rejects_codex_intent_for_non_responses_protocols() {
        for protocol in [
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            let error = construct_http_provider(
                reqwest::Client::new(),
                spec(
                    protocol,
                    HttpConstructionAuth::Codex {
                        access_token: "codex-secret".into(),
                        account_id: "account-secret".into(),
                        is_fedramp: false,
                    },
                ),
            )
            .err()
            .expect("Codex intent must be rejected");

            assert!(matches!(error, ProviderError::Configuration(_)));
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("codex-secret"));
            assert!(!rendered.contains("account-secret"));
        }
    }

    #[test]
    fn accepts_mantle_sigv4_only_for_mantle_protocols() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
        ] {
            construct_http_provider(
                reqwest::Client::new(),
                spec(
                    protocol,
                    HttpConstructionAuth::MantleSigV4(RequestAuthorizer::default()),
                ),
            )
            .unwrap();
        }

        let error = construct_http_provider(
            reqwest::Client::new(),
            spec(
                HttpProtocol::GoogleGenerateContent,
                HttpConstructionAuth::MantleSigV4(RequestAuthorizer::default()),
            ),
        )
        .err()
        .expect("Google must reject Mantle SigV4");
        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
