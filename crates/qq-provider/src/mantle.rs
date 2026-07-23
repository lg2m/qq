//! Amazon Bedrock Mantle adapter for OpenAI- and Anthropic-compatible APIs.

use std::{fmt, sync::Arc};

use async_stream::try_stream;
use aws_config::Region;
use futures_util::StreamExt;
use tokio::sync::OnceCell;

use crate::{
    Provider, ProviderError, ProviderErrorKind, ProviderStream,
    anthropic::{AnthropicAuth, AnthropicMessages},
    bedrock::{
        AwsConfigLoadError, BedrockAuth, load_aws_config, valid_region_label,
        validate_configuration,
    },
    compiler::HttpProtocol,
    http::validate_endpoint,
    openai::{OpenAi, ResponsesAuth},
    openai_chat::{ChatCompletionsAuth, OpenAiChatCompletions},
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
};

/// A lazily initialized Amazon Bedrock Mantle deployment.
pub(crate) struct Mantle {
    inner: Arc<MantleInner>,
}

struct MantleInner {
    client: reqwest::Client,
    region: Option<String>,
    protocol: HttpProtocol,
    auth: BedrockAuth,
    provider: OnceCell<MantleProvider>,
    #[cfg(test)]
    warm_streams: std::sync::atomic::AtomicUsize,
}

impl fmt::Debug for Mantle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Mantle")
            .field("region", &self.inner.region)
            .field("protocol", &self.inner.protocol)
            .field("auth", &self.inner.auth)
            .finish_non_exhaustive()
    }
}

impl Mantle {
    pub(crate) fn new(
        client: reqwest::Client,
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
    ) -> Result<Self, ProviderError> {
        if protocol == HttpProtocol::GoogleGenerateContent {
            return Err(ProviderError::Configuration(
                "Google GenerateContent is not supported by Amazon Bedrock Mantle".to_owned(),
            ));
        }
        validate_configuration(&auth, region.as_deref())?;
        let provider = match (region.as_deref(), &auth) {
            (Some(region), BedrockAuth::ApiKey(api_key)) => {
                let endpoint =
                    mantle_endpoint(region, protocol).map_err(|error| error.to_provider_error())?;
                let provider = build_provider(
                    client.clone(),
                    endpoint,
                    protocol,
                    Some(api_key.clone()),
                    RequestAuthorizer::default(),
                )
                .map_err(|error| error.to_provider_error())?;
                OnceCell::from(provider)
            }
            (Some(region), BedrockAuth::DefaultChain | BedrockAuth::Profile(_)) => {
                mantle_endpoint(region, protocol).map_err(|error| error.to_provider_error())?;
                OnceCell::new()
            }
            (None, _) => OnceCell::new(),
        };
        Ok(Self {
            inner: Arc::new(MantleInner {
                client,
                region,
                protocol,
                auth,
                provider,
                #[cfg(test)]
                warm_streams: std::sync::atomic::AtomicUsize::new(0),
            }),
        })
    }
}

impl Provider for Mantle {
    fn stream(&self, request: crate::ModelRequest) -> ProviderStream {
        if let Some(provider) = self.inner.provider.get() {
            #[cfg(test)]
            self.inner
                .warm_streams
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return provider.stream(request);
        }

        let inner = Arc::clone(&self.inner);
        Box::pin(try_stream! {
            let provider = match inner.provider.get_or_try_init(|| inner.load_provider()).await {
                Ok(provider) => provider,
                Err(error) => Err(error.to_provider_error())?,
            };
            let mut events = provider.stream(request);
            while let Some(event) = events.next().await {
                yield event?;
            }
        })
    }
}

impl MantleInner {
    async fn load_provider(&self) -> Result<MantleProvider, MantleInitError> {
        let config = load_aws_config(&self.auth, self.region.as_deref()).await?;
        let region = config
            .sdk_config
            .region()
            .map(Region::as_ref)
            .ok_or_else(|| {
                MantleInitError::Configuration(
                    "AWS region provider chain did not resolve a region".to_owned(),
                )
            })?;
        let endpoint = mantle_endpoint(region, self.protocol)?;

        match &self.auth {
            BedrockAuth::ApiKey(api_key) => build_provider(
                self.client.clone(),
                endpoint,
                self.protocol,
                Some(api_key.clone()),
                RequestAuthorizer::default(),
            ),
            BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => {
                let credentials = config.credentials.ok_or_else(|| {
                    MantleInitError::Authentication(
                        "AWS credential lease is unavailable".to_owned(),
                    )
                })?;
                build_provider(
                    self.client.clone(),
                    endpoint,
                    self.protocol,
                    None,
                    RequestAuthorizer::bedrock_mantle_sigv4(region, credentials),
                )
            }
        }
    }
}

fn build_provider(
    client: reqwest::Client,
    endpoint: reqwest::Url,
    protocol: HttpProtocol,
    api_key: Option<String>,
    authorizer: RequestAuthorizer,
) -> Result<MantleProvider, MantleInitError> {
    let provider = match protocol {
        HttpProtocol::OpenAiResponses => {
            MantleProvider::OpenAi(OpenAi::with_client_and_authorizer(
                client,
                endpoint,
                api_key.map_or(ResponsesAuth::NoAuth, ResponsesAuth::Bearer),
                [],
                authorizer,
            )?)
        }
        HttpProtocol::OpenAiChatCompletions => MantleProvider::OpenAiChatCompletions(
            OpenAiChatCompletions::with_client_and_authorizer(
                client,
                endpoint,
                api_key.map_or(ChatCompletionsAuth::NoAuth, ChatCompletionsAuth::Bearer),
                [],
                authorizer,
            )?,
        ),
        HttpProtocol::AnthropicMessages => {
            MantleProvider::AnthropicMessages(AnthropicMessages::with_client_and_authorizer(
                client,
                endpoint,
                api_key.map_or(AnthropicAuth::NoAuth, AnthropicAuth::XApiKey),
                [],
                authorizer,
            )?)
        }
        HttpProtocol::GoogleGenerateContent => {
            return Err(MantleInitError::Configuration(
                "Google GenerateContent is not supported by Amazon Bedrock Mantle".to_owned(),
            ));
        }
    };
    Ok(provider)
}

enum MantleProvider {
    OpenAi(OpenAi),
    OpenAiChatCompletions(OpenAiChatCompletions),
    AnthropicMessages(AnthropicMessages),
}

impl Provider for MantleProvider {
    fn stream(&self, request: crate::ModelRequest) -> ProviderStream {
        match self {
            Self::OpenAi(provider) => provider.stream(request),
            Self::OpenAiChatCompletions(provider) => provider.stream(request),
            Self::AnthropicMessages(provider) => provider.stream(request),
        }
    }
}

fn mantle_endpoint(region: &str, protocol: HttpProtocol) -> Result<reqwest::Url, MantleInitError> {
    if !valid_region_label(region) {
        return Err(MantleInitError::Configuration(
            "AWS region must be a valid DNS label".to_owned(),
        ));
    }
    let path = match protocol {
        HttpProtocol::OpenAiResponses => "/v1/responses",
        HttpProtocol::OpenAiChatCompletions => "/v1/chat/completions",
        HttpProtocol::AnthropicMessages => "/anthropic/v1/messages",
        HttpProtocol::GoogleGenerateContent => {
            return Err(MantleInitError::Configuration(
                "Google GenerateContent is not supported by Amazon Bedrock Mantle".to_owned(),
            ));
        }
    };
    validate_endpoint(
        &format!("https://bedrock-mantle.{region}.api.aws{path}"),
        false,
    )
    .map_err(MantleInitError::from)
}

#[derive(Debug)]
enum MantleInitError {
    Configuration(String),
    Authentication(String),
    AwsConfiguration(AwsConfigLoadError),
}

impl MantleInitError {
    fn to_provider_error(&self) -> ProviderError {
        match self {
            Self::Configuration(message) => {
                ProviderError::Configuration(sanitize_message(message, &[]))
            }
            Self::Authentication(message) => ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Authentication,
                message: sanitize_message(message, &[]),
            },
            Self::AwsConfiguration(error) => error.to_provider_error(),
        }
    }
}

impl From<AwsConfigLoadError> for MantleInitError {
    fn from(error: AwsConfigLoadError) -> Self {
        Self::AwsConfiguration(error)
    }
}

impl From<ProviderError> for MantleInitError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Configuration(message) => Self::Configuration(message),
            error => Self::Configuration(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::atomic::{AtomicUsize, Ordering},
        thread::{self, JoinHandle},
        time::{Duration, SystemTime},
    };

    use aws_credential_types::{
        Credentials,
        provider::{self, ProvideCredentials, SharedCredentialsProvider, future},
    };

    use super::*;
    use crate::{Message, ModelRequest, http::build_direct_client};

    #[test]
    fn uses_canonical_regional_endpoints_for_each_protocol() {
        for (protocol, expected) in [
            (
                HttpProtocol::OpenAiResponses,
                "https://bedrock-mantle.us-east-1.api.aws/v1/responses",
            ),
            (
                HttpProtocol::OpenAiChatCompletions,
                "https://bedrock-mantle.us-east-1.api.aws/v1/chat/completions",
            ),
            (
                HttpProtocol::AnthropicMessages,
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages",
            ),
        ] {
            assert_eq!(
                mantle_endpoint("us-east-1", protocol).unwrap().as_str(),
                expected
            );
        }
        assert!(mantle_endpoint("us-east-1/path", HttpProtocol::OpenAiResponses).is_err());
        assert!(mantle_endpoint(&"a".repeat(64), HttpProtocol::OpenAiResponses).is_err());
    }

    #[test]
    fn construction_is_network_free_and_redacts_api_keys() {
        let provider = Mantle::new(
            reqwest::Client::new(),
            Some("us-east-1".to_owned()),
            HttpProtocol::OpenAiResponses,
            BedrockAuth::ApiKey("mantle-test-secret".to_owned()),
        )
        .unwrap();

        assert!(!format!("{provider:?}").contains("mantle-test-secret"));
        assert!(
            Mantle::new(
                reqwest::Client::new(),
                Some("us-east-1/path".to_owned()),
                HttpProtocol::OpenAiResponses,
                BedrockAuth::DefaultChain,
            )
            .is_err()
        );
    }

    #[test]
    fn initialized_provider_selects_the_synchronous_warm_stream_path() {
        let provider = Mantle::new(
            reqwest::Client::new(),
            Some("us-east-1".to_owned()),
            HttpProtocol::OpenAiResponses,
            BedrockAuth::ApiKey("mantle-test-secret".to_owned()),
        )
        .unwrap();

        let stream = provider.stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            64,
        ));

        assert_eq!(provider.inner.warm_streams.load(Ordering::Relaxed), 1);
        drop(stream);
    }

    #[tokio::test]
    async fn failed_initialization_is_retryable() {
        let provider = OnceCell::new();
        let first = provider
            .get_or_try_init(|| async { Err::<MantleProvider, _>("temporary failure") })
            .await;
        assert_eq!(first.err(), Some("temporary failure"));

        let second = provider
            .get_or_try_init(|| async {
                build_provider(
                    build_direct_client().unwrap(),
                    mantle_endpoint("us-east-1", HttpProtocol::OpenAiResponses).unwrap(),
                    HttpProtocol::OpenAiResponses,
                    Some("mantle-test-secret".to_owned()),
                    RequestAuthorizer::default(),
                )
            })
            .await;

        assert!(second.is_ok());
        assert!(provider.get().is_some());
    }

    #[tokio::test]
    async fn api_keys_use_protocol_specific_headers() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
        ] {
            let (endpoint, server) = serve_unauthorized();
            let provider = build_provider(
                build_direct_client().unwrap(),
                endpoint,
                protocol,
                Some("mantle-test-secret".to_owned()),
                RequestAuthorizer::default(),
            )
            .unwrap();

            let _events = provider
                .stream(ModelRequest::new(
                    "test-model",
                    vec![Message::user("hello")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;
            let request = server.join().unwrap();
            let headers = request.split_once("\r\n\r\n").unwrap().0;

            match protocol {
                HttpProtocol::OpenAiResponses | HttpProtocol::OpenAiChatCompletions => {
                    assert_eq!(
                        request_header(headers, "authorization"),
                        Some("Bearer mantle-test-secret")
                    );
                    assert_eq!(request_header(headers, "x-api-key"), None);
                }
                HttpProtocol::AnthropicMessages => {
                    assert_eq!(request_header(headers, "authorization"), None);
                    assert_eq!(
                        request_header(headers, "x-api-key"),
                        Some("mantle-test-secret")
                    );
                    assert_eq!(
                        request_header(headers, "anthropic-version"),
                        Some("2023-06-01")
                    );
                }
                HttpProtocol::GoogleGenerateContent => unreachable!("test cases are exhaustive"),
            }
        }
    }

    #[tokio::test]
    async fn codec_applies_sigv4_after_serializing_the_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let credentials = CountingCredentials {
            calls: Arc::clone(&calls),
            credentials: Credentials::new(
                "AKIDEXAMPLE",
                "test-secret-access-key",
                None,
                None,
                "test",
            ),
        };
        let authorizer = RequestAuthorizer::bedrock_mantle_sigv4_with_clock(
            "us-east-1",
            SharedCredentialsProvider::new(credentials),
            fixed_time,
        );
        let (endpoint, server) = serve_unauthorized();
        let provider = build_provider(
            build_direct_client().unwrap(),
            endpoint,
            HttpProtocol::OpenAiResponses,
            None,
            authorizer,
        )
        .unwrap();

        let _events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;
        let request = server.join().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let authorization = request_header(headers, "authorization").unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();

        assert!(authorization.contains("/us-east-1/bedrock-mantle/aws4_request"));
        assert_eq!(body["model"], "test-model");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    fn fixed_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
    }

    #[derive(Debug)]
    struct CountingCredentials {
        calls: Arc<AtomicUsize>,
        credentials: Credentials,
    }

    impl ProvideCredentials for CountingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::Relaxed);
            future::ProvideCredentials::ready(provider::Result::Ok(self.credentials.clone()))
        }
    }

    fn serve_unauthorized() -> (reqwest::Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            reqwest::Url::parse(&format!("http://{}/invoke", listener.local_addr().unwrap()))
                .unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        (endpoint, server)
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0; 4_096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or_default();
            if request.len() >= body_start + content_length {
                break;
            }
        }
        request
    }

    fn request_header<'a>(headers: &'a str, expected_name: &str) -> Option<&'a str> {
        headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(expected_name))
            .map(|(_, value)| value.trim())
    }
}
