//! Shared construction for compiled direct HTTP providers.
//!
//! This module owns protocol/authentication compatibility and concrete adapter
//! selection. Callers must resolve endpoints and supply an HTTP client before
//! entering this boundary.

use crate::{
    Provider, ProviderError, ProviderStream,
    compiler::{EndpointKind, HttpAuth, HttpProtocol},
    providers::{
        anthropic::{AnthropicAuth, AnthropicMessages},
        google::{GoogleAuth, GoogleGenerateContent},
        openai::{OpenAi, ResponsesAuth, ResponsesConstructionAuth},
        openai_chat::{ChatCompletionsAuth, OpenAiChatCompletions},
    },
    request_auth::RequestAuthorizer,
};

pub(crate) const GOOGLE_MANTLE_UNSUPPORTED_MESSAGE: &str =
    "Google GenerateContent is not supported by Amazon Bedrock Mantle";
const CODEX_REQUIRES_RESPONSES_MESSAGE: &str =
    "Codex authentication requires the OpenAI Responses protocol";
const REQUEST_TIME_CODEX_REQUIRES_RESPONSES_MESSAGE: &str =
    "request-time Codex authentication requires the OpenAI Responses protocol";
const GOOGLE_REQUEST_TIME_UNSUPPORTED_MESSAGE: &str =
    "request-time credentials are not supported by Google GenerateContent";
const AUTHORIZER_WITH_AUTH_MESSAGE: &str =
    "internal request authorization cannot be combined with HTTP authentication";

/// Fully resolved inputs for constructing one compiled HTTP provider.
pub(crate) struct HttpConstructionSpec {
    pub(crate) protocol: HttpProtocol,
    pub(crate) endpoint: reqwest::Url,
    pub(crate) endpoint_kind: EndpointKind,
    pub(crate) auth: HttpAuth,
    pub(crate) authorizer: Option<RequestAuthorizer>,
    pub(crate) headers: Vec<(String, String)>,
}

enum ResolvedHttpAuth {
    OpenAiResponses(ResponsesConstructionAuth),
    OpenAiChatCompletions {
        auth: ChatCompletionsAuth,
        authorizer: RequestAuthorizer,
    },
    AnthropicMessages {
        auth: AnthropicAuth,
        authorizer: RequestAuthorizer,
    },
    GoogleGenerateContent(GoogleAuth),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpRetryMode {
    Default,
    Disabled,
}

/// The concrete HTTP provider selected by compiled construction.
pub(crate) enum CompiledHttpProvider {
    OpenAiResponses(OpenAi),
    OpenAiChatCompletions(OpenAiChatCompletions),
    AnthropicMessages(AnthropicMessages),
    GoogleGenerateContent(GoogleGenerateContent),
}

impl CompiledHttpProvider {
    #[must_use]
    pub(crate) fn with_retry_mode(self, retry_mode: HttpRetryMode) -> Self {
        if retry_mode == HttpRetryMode::Default {
            self
        } else {
            self.without_retries()
        }
    }

    #[must_use]
    pub(crate) fn without_retries(self) -> Self {
        let mut provider = self;
        let exchange = match &mut provider {
            Self::OpenAiResponses(provider) => &mut provider.exchange,
            Self::OpenAiChatCompletions(provider) => &mut provider.exchange,
            Self::AnthropicMessages(provider) => &mut provider.exchange,
            Self::GoogleGenerateContent(provider) => &mut provider.exchange,
        };
        exchange.disable_retries();
        provider
    }
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
// Recipe compilation is a measured startup path with multiple facade call sites.
#[inline(always)]
pub(crate) fn construct_http_provider(
    client: reqwest::Client,
    spec: HttpConstructionSpec,
) -> Result<CompiledHttpProvider, ProviderError> {
    let HttpConstructionSpec {
        protocol,
        endpoint,
        endpoint_kind,
        auth,
        authorizer,
        headers,
    } = spec;

    match resolve_http_auth(protocol, auth, authorizer)? {
        ResolvedHttpAuth::OpenAiResponses(auth) => Ok(CompiledHttpProvider::OpenAiResponses(
            OpenAi::with_client_and_auth(client, endpoint, auth, headers)?,
        )),
        ResolvedHttpAuth::OpenAiChatCompletions { auth, authorizer } => {
            Ok(CompiledHttpProvider::OpenAiChatCompletions(
                OpenAiChatCompletions::with_client_and_authorizer(
                    client, endpoint, auth, headers, authorizer,
                )?,
            ))
        }
        ResolvedHttpAuth::AnthropicMessages { auth, authorizer } => Ok(
            CompiledHttpProvider::AnthropicMessages(AnthropicMessages::with_client_and_authorizer(
                client, endpoint, auth, headers, authorizer,
            )?),
        ),
        ResolvedHttpAuth::GoogleGenerateContent(auth) => {
            Ok(CompiledHttpProvider::GoogleGenerateContent(
                GoogleGenerateContent::with_client(client, endpoint, endpoint_kind, auth, headers)?,
            ))
        }
    }
}

fn resolve_http_auth(
    protocol: HttpProtocol,
    auth: HttpAuth,
    authorizer: Option<RequestAuthorizer>,
) -> Result<ResolvedHttpAuth, ProviderError> {
    match (auth, authorizer) {
        (HttpAuth::NoAuth, Some(authorizer)) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::MantleSigV4(authorizer),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer,
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer,
            }),
            HttpProtocol::GoogleGenerateContent => Err(ProviderError::Configuration(
                GOOGLE_MANTLE_UNSUPPORTED_MESSAGE.to_owned(),
            )),
        },
        (_, Some(_)) => Err(ProviderError::Configuration(
            AUTHORIZER_WITH_AUTH_MESSAGE.to_owned(),
        )),
        (HttpAuth::NoAuth, None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::NoAuth),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => {
                Ok(ResolvedHttpAuth::GoogleGenerateContent(GoogleAuth::NoAuth))
            }
        },
        (HttpAuth::ApiKey(secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Bearer(secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::XApiKey(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent(
                GoogleAuth::XGoogApiKey(secret),
            )),
        },
        (HttpAuth::Bearer(secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Bearer(secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent(
                GoogleAuth::Bearer(secret),
            )),
        },
        (HttpAuth::Header(name, secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Header(name, secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Header(name, secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::Header(name, secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent(
                GoogleAuth::Header(name, secret),
            )),
        },
        (
            HttpAuth::Codex {
                access_token,
                account_id,
                is_fedramp,
            },
            None,
        ) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Codex {
                    access_token,
                    account_id,
                    is_fedramp,
                }),
            )),
            _ => Err(ProviderError::Configuration(
                CODEX_REQUIRES_RESPONSES_MESSAGE.to_owned(),
            )),
        },
        (HttpAuth::RequestTimeBearer(credentials), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::RequestTimeBearer(credentials),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer: RequestAuthorizer::request_time_bearer(credentials),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer: RequestAuthorizer::request_time_bearer(credentials),
            }),
            HttpProtocol::GoogleGenerateContent => Err(ProviderError::Configuration(
                GOOGLE_REQUEST_TIME_UNSUPPORTED_MESSAGE.to_owned(),
            )),
        },
        (HttpAuth::RequestTimeCodex(credentials), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::RequestTimeCodex(credentials),
            )),
            HttpProtocol::GoogleGenerateContent => Err(ProviderError::Configuration(
                GOOGLE_REQUEST_TIME_UNSUPPORTED_MESSAGE.to_owned(),
            )),
            _ => Err(ProviderError::Configuration(
                REQUEST_TIME_CODEX_REQUIRES_RESPONSES_MESSAGE.to_owned(),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RequestCredential, RequestCredentialFuture, RequestCredentialProvider,
        SharedRequestCredentialProvider, compiler::HttpProtocol,
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

    fn spec(protocol: HttpProtocol, auth: HttpAuth) -> HttpConstructionSpec {
        HttpConstructionSpec {
            protocol,
            endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
            endpoint_kind: EndpointKind::Exact,
            auth,
            authorizer: None,
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
                spec(protocol, HttpAuth::ApiKey("test-secret".into())),
            )
            .unwrap();
        }
    }

    #[test]
    fn constructs_every_supported_direct_auth_intent_without_network_io() {
        let cases = [
            HttpAuth::NoAuth,
            HttpAuth::ApiKey("test-api-key".into()),
            HttpAuth::Bearer("test-bearer".into()),
            HttpAuth::Header("x-test-key".to_owned(), "test-value".into()),
            HttpAuth::Codex {
                access_token: "test-codex".into(),
                account_id: "test-account".into(),
                is_fedramp: false,
            },
            HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                StaticBearerCredentials,
            )),
            HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                StaticCodexCredentials,
            )),
        ];

        for auth in cases {
            construct_http_provider(
                reqwest::Client::new(),
                HttpConstructionSpec {
                    protocol: HttpProtocol::OpenAiResponses,
                    endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
                    endpoint_kind: EndpointKind::Base,
                    auth,
                    authorizer: None,
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
                    HttpAuth::Codex {
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
            let mut spec = spec(protocol, HttpAuth::NoAuth);
            spec.authorizer = Some(RequestAuthorizer::default());
            construct_http_provider(reqwest::Client::new(), spec).unwrap();
        }

        let mut spec = spec(HttpProtocol::GoogleGenerateContent, HttpAuth::NoAuth);
        spec.authorizer = Some(RequestAuthorizer::default());
        let error = construct_http_provider(reqwest::Client::new(), spec)
            .err()
            .expect("Google must reject Mantle SigV4");
        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn rejects_internal_authorizer_combined_with_recipe_auth() {
        let error = construct_http_provider(
            reqwest::Client::new(),
            HttpConstructionSpec {
                protocol: HttpProtocol::OpenAiResponses,
                endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
                endpoint_kind: EndpointKind::Exact,
                auth: HttpAuth::ApiKey("test-secret".into()),
                authorizer: Some(RequestAuthorizer::default()),
                headers: Vec::new(),
            },
        )
        .err()
        .expect("recipe auth and an internal authorizer must be mutually exclusive");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
