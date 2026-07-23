//! Amazon Bedrock `ConverseStream` adapter.

use std::{
    collections::{HashMap, hash_map::Entry},
    error::Error,
    fmt::{self, Write as _},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, LazyLock},
    task::{Context, Poll},
    time::Duration,
};

use async_stream::try_stream;
#[allow(deprecated)]
use aws_config::profile::profile_file::ProfileFiles;
use aws_config::{
    BehaviorVersion, Region, SdkConfig,
    default_provider::credentials::DefaultCredentialsChain,
    environment::region::EnvironmentVariableRegionProvider,
    imds::region::ImdsRegionProvider,
    meta::region::{ProvideRegion, RegionProviderChain},
    profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider},
    provider_config::ProviderConfig,
    retry::RetryConfig,
};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sdk_bedrockruntime::{
    Client,
    config::{
        Config, ConfigBag, Intercept, RuntimeComponents, Token,
        interceptors::BeforeDeserializationInterceptorContextMut,
    },
    error::{BoxError, DisplayErrorContext, SdkError},
    operation::converse_stream::ConverseStreamError,
    types::{
        ContentBlock as BedrockContentBlock, ContentBlockDelta, ContentBlockStart,
        ConversationRole, ConverseStreamOutput, InferenceConfiguration, Message as BedrockMessage,
        StopReason, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock,
        ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
        error::ConverseStreamOutputError,
    },
};
use aws_smithy_http_client::{Builder as SmithyHttpClientBuilder, tls};
use aws_smithy_runtime_api::client::http::SharedHttpClient;
use aws_smithy_types::{Document, Number, body::SdkBody};
use bytes::Bytes;
use http_body::Body;
use serde_json::Value;
use tokio::sync::{OnceCell, Semaphore};

use crate::{
    ContentBlock, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, Role, limits::StreamLimits, request_auth::AwsCredentialLease,
    sanitize::sanitize_message,
};

const EVENT_FRAME_OVERHEAD_BYTES: usize = 64;
const AWS_CONFIG_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
const AWS_CONFIG_BUILD_CONCURRENCY: usize = 2;

static AWS_CONFIG_BUILD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)));
static DIRECT_AWS_HTTP_CLIENT: LazyLock<SharedHttpClient> = LazyLock::new(|| {
    // This client stays proxy-disabled unless proxy configuration is explicitly installed.
    SmithyHttpClientBuilder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .build_https()
});

/// Authentication used by Amazon Bedrock Runtime.
#[derive(Clone, PartialEq, Eq)]
pub enum BedrockAuth {
    /// Uses the standard AWS credential and region provider chains.
    DefaultChain,
    /// Uses one named AWS profile.
    Profile(String),
    /// Uses an Amazon Bedrock API key as an HTTP bearer token.
    ApiKey(String),
}

impl fmt::Debug for BedrockAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultChain => formatter.write_str("DefaultChain"),
            Self::Profile(profile) => formatter.debug_tuple("Profile").field(profile).finish(),
            Self::ApiKey(_) => formatter
                .debug_tuple("ApiKey")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

/// A client for Amazon Bedrock's `ConverseStream` API.
#[derive(Clone)]
pub struct Bedrock {
    client: Arc<OnceCell<Client>>,
    auth: BedrockAuth,
    region: Option<String>,
    redactions: Arc<[String]>,
}

impl fmt::Debug for Bedrock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Bedrock").finish_non_exhaustive()
    }
}

impl Bedrock {
    /// Creates a lazily initialized Bedrock client.
    ///
    /// AWS configuration is not loaded and no network access occurs until the
    /// first returned provider stream is polled. If `region` is `None`, the AWS
    /// region provider chain is used.
    ///
    /// # Errors
    ///
    /// Returns an error if the authentication or region configuration is empty or contains
    /// control characters.
    pub fn new(auth: BedrockAuth, region: Option<String>) -> Result<Self, ProviderError> {
        validate_configuration(&auth, region.as_deref())?;
        let redactions: Arc<[String]> = match &auth {
            BedrockAuth::ApiKey(secret) => Arc::from([secret.clone()]),
            BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => Arc::from([]),
        };
        Ok(Self {
            client: Arc::new(OnceCell::new()),
            auth,
            region,
            redactions,
        })
    }
}

impl Provider for Bedrock {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let auth = self.auth.clone();
        let region = self.region.clone();
        let redactions = Arc::clone(&self.redactions);

        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let request = ConverseRequest::try_from(&request)?;
            let client = match client.get_or_try_init(|| load_client(auth, region)).await {
                Ok(client) => client,
                Err(error) => Err(error.to_provider_error())?,
            };
            let body_limit = ResponseBodyLimit::new(limits.wire);
            let body_limit_exceeded = Arc::clone(&body_limit.exceeded);
            let response = client
                .converse_stream()
                .model_id(request.model_id)
                .set_messages(Some(request.messages))
                .set_tool_config(request.tool_config)
                .inference_config(request.inference_config)
                .customize()
                .interceptor(body_limit)
                .send()
                .await
                .map_err(|error| {
                    request_error(
                        &error,
                        redactions.as_ref(),
                        body_limit_exceeded.load(Ordering::Relaxed),
                    )
                })?;
            let mut receiver = response.stream;
            let mut output_bytes = 0_usize;
            // Maps streamed content-block indexes to tool-call ids so argument
            // deltas and block stops can be attributed after the start event.
            let mut tool_calls = ToolCallTracker::default();

            while let Some(event) = receiver
                .recv()
                .await
                .map_err(|error| {
                    stream_error(
                        &error,
                        redactions.as_ref(),
                        body_limit_exceeded.load(Ordering::Relaxed),
                    )
                })?
            {
                check_stream_event_size(&event, limits.event)?;

                match decode_stream_event(event)? {
                    DecodedEvent::OutputText(text) => {
                        if text.is_empty() {
                            continue;
                        }
                        add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                        yield ProviderEvent::OutputTextDelta { text };
                    }
                    DecodedEvent::Refusal(text) => {
                        add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                        yield ProviderEvent::RefusalDelta { text };
                        yield ProviderEvent::Completed;
                        return;
                    }
                    DecodedEvent::ToolCallStarted { index, id, name } => {
                        tool_calls.start(index, id.clone())?;
                        yield ProviderEvent::ToolCallStarted { id, name };
                    }
                    DecodedEvent::ToolCallArguments { index, json } => {
                        let id = tool_calls.arguments(index)?.to_owned();
                        add_output_bytes(&mut output_bytes, json.len(), limits.output)?;
                        yield ProviderEvent::ToolCallArgumentsDelta { id, json };
                    }
                    DecodedEvent::BlockStopped { index } => {
                        if let Some(id) = tool_calls.stop(index) {
                            yield ProviderEvent::ToolCallCompleted { id };
                        }
                    }
                    DecodedEvent::Completed => {
                        yield ProviderEvent::Completed;
                        return;
                    }
                    DecodedEvent::Ignored => {}
                }
            }

            Err(ProviderError::Protocol(
                "Amazon Bedrock stream ended before messageStop".to_owned(),
            ))?;
        })
    }
}

pub(crate) fn validate_configuration(
    auth: &BedrockAuth,
    region: Option<&str>,
) -> Result<(), ProviderError> {
    match auth {
        BedrockAuth::Profile(profile) if invalid_configuration_value(profile) => {
            return Err(ProviderError::Configuration(
                "AWS profile name must not be empty or contain control characters".to_owned(),
            ));
        }
        BedrockAuth::ApiKey(secret) if invalid_configuration_value(secret) => {
            return Err(ProviderError::Configuration(
                "Amazon Bedrock API key must not be empty or contain control characters".to_owned(),
            ));
        }
        BedrockAuth::DefaultChain | BedrockAuth::Profile(_) | BedrockAuth::ApiKey(_) => {}
    }

    if region.is_some_and(|region| !valid_region_label(region)) {
        return Err(ProviderError::Configuration(
            "AWS region must be a valid DNS label".to_owned(),
        ));
    }

    Ok(())
}

fn invalid_configuration_value(value: &str) -> bool {
    value.trim().is_empty() || value.chars().any(char::is_control)
}

pub(crate) fn valid_region_label(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 63
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && region
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && region
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

async fn load_client(
    auth: BedrockAuth,
    region: Option<String>,
) -> Result<Client, AwsConfigLoadError> {
    let loaded = match load_aws_config(&auth, region.as_deref()).await {
        Ok(config) => config,
        Err(error) => return Err(error),
    };
    let api_key = match auth {
        BedrockAuth::ApiKey(secret) => Some(secret),
        BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => None,
    };
    Ok(Client::from_conf(service_config(
        &loaded.sdk_config,
        api_key,
    )))
}

pub(crate) async fn load_aws_config(
    auth: &BedrockAuth,
    region: Option<&str>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    load_aws_config_with_profile_files(auth, region, None, Arc::clone(&AWS_CONFIG_BUILD_PERMITS))
        .await
}

#[allow(deprecated)]
async fn load_aws_config_with_profile_files(
    auth: &BedrockAuth,
    region: Option<&str>,
    profile_files: Option<ProfileFiles>,
    permits: Arc<Semaphore>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    let auth = auth.clone();
    let region = region.map(str::to_owned);
    run_bounded_aws_config_build(permits, AWS_CONFIG_LOAD_TIMEOUT, move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AwsConfigLoadError::RuntimeConstructionFailed)?;
        runtime.block_on(build_aws_config(&auth, region.as_deref(), profile_files))
    })
    .await
}

async fn run_bounded_aws_config_build<T>(
    permits: Arc<Semaphore>,
    timeout: Duration,
    build: impl FnOnce() -> Result<T, AwsConfigLoadError> + Send + 'static,
) -> Result<T, AwsConfigLoadError>
where
    T: Send + 'static,
{
    let permit = permits
        .try_acquire_owned()
        .map_err(|_| AwsConfigLoadError::CapacityUnavailable)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        build()
    });

    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(AwsConfigLoadError::JoinFailed),
        Err(_) => Err(AwsConfigLoadError::TimedOut),
    }
}

#[allow(deprecated)]
async fn build_aws_config(
    auth: &BedrockAuth,
    region: Option<&str>,
    profile_files: Option<ProfileFiles>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    let provider_config = ProviderConfig::without_region()
        .with_http_client((*DIRECT_AWS_HTTP_CLIENT).clone())
        .with_retry_config(RetryConfig::disabled())
        .with_behavior_version(Some(BehaviorVersion::latest()));

    let resolved_region = match region {
        Some(region) => Some(Region::new(region.to_owned())),
        None => match auth {
            BedrockAuth::Profile(profile) => {
                let mut builder = ProfileFileRegionProvider::builder()
                    .configure(&provider_config)
                    .profile_name(profile);
                if let Some(profile_files) = profile_files.clone() {
                    builder = builder.profile_files(profile_files);
                }
                ProvideRegion::region(&builder.build()).await
            }
            BedrockAuth::DefaultChain | BedrockAuth::ApiKey(_) => {
                let mut profile = ProfileFileRegionProvider::builder().configure(&provider_config);
                if let Some(profile_files) = profile_files.clone() {
                    profile = profile.profile_files(profile_files);
                }
                RegionProviderChain::first_try(EnvironmentVariableRegionProvider::new())
                    .or_else(profile.build())
                    .or_else(
                        ImdsRegionProvider::builder()
                            .configure(&provider_config)
                            .build(),
                    )
                    .region()
                    .await
            }
        },
    };
    let provider_config = provider_config.with_region(resolved_region.clone());

    let credentials = match auth {
        BedrockAuth::DefaultChain => {
            let credentials = DefaultCredentialsChain::builder()
                .configure(provider_config)
                .region(resolved_region.clone())
                .build()
                .await;
            Some(SharedCredentialsProvider::new(credentials))
        }
        BedrockAuth::Profile(profile) => {
            let mut credentials = ProfileFileCredentialsProvider::builder()
                .configure(&provider_config)
                .profile_name(profile);
            if let Some(profile_files) = profile_files {
                credentials = credentials.profile_files(profile_files);
            }
            Some(SharedCredentialsProvider::new(credentials.build()))
        }
        BedrockAuth::ApiKey(_) => None,
    };

    let credentials = if let Some(credentials) = credentials {
        let initial = match credentials.provide_credentials().await {
            Ok(credentials) => credentials,
            Err(_) => return Err(AwsConfigLoadError::CredentialsUnavailable),
        };
        let credentials = AwsCredentialLease::new_primed(credentials, initial);
        Some(credentials)
    } else {
        None
    };

    let mut sdk_config = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(resolved_region)
        .retry_config(RetryConfig::disabled())
        .http_client((*DIRECT_AWS_HTTP_CLIENT).clone());
    if let Some(credentials) = &credentials {
        sdk_config =
            sdk_config.credentials_provider(SharedCredentialsProvider::new(credentials.clone()));
    }

    Ok(LoadedAwsConfig {
        sdk_config: sdk_config.build(),
        credentials,
    })
}

#[derive(Debug)]
pub(crate) struct LoadedAwsConfig {
    pub(crate) sdk_config: SdkConfig,
    pub(crate) credentials: Option<AwsCredentialLease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwsConfigLoadError {
    TimedOut,
    CapacityUnavailable,
    JoinFailed,
    RuntimeConstructionFailed,
    CredentialsUnavailable,
}

impl AwsConfigLoadError {
    pub(crate) fn to_provider_error(self) -> ProviderError {
        let message = match self {
            Self::TimedOut => "AWS configuration and region resolution timed out",
            Self::CapacityUnavailable => "AWS configuration worker capacity is exhausted",
            Self::JoinFailed => "AWS configuration worker stopped unexpectedly",
            Self::RuntimeConstructionFailed => {
                "AWS configuration worker runtime could not be initialized"
            }
            Self::CredentialsUnavailable => {
                return ProviderError::ResponseFailed {
                    kind: ProviderErrorKind::Authentication,
                    message: "Amazon Bedrock could not load AWS credentials".to_owned(),
                };
            }
        };
        ProviderError::Transport(message.to_owned())
    }
}

fn service_config(shared_config: &SdkConfig, api_key: Option<String>) -> Config {
    let builder = aws_sdk_bedrockruntime::config::Builder::from(shared_config)
        .retry_config(RetryConfig::disabled());
    let builder = if let Some(api_key) = api_key {
        builder
            .bearer_token(Token::new(api_key, None))
            .auth_scheme_preference(["httpBearerAuth".into()])
    } else {
        builder.auth_scheme_preference(["sigv4".into()])
    };

    builder.build()
}

#[derive(Debug)]
struct ConverseRequest {
    model_id: String,
    messages: Vec<BedrockMessage>,
    tool_config: Option<ToolConfiguration>,
    inference_config: InferenceConfiguration,
}

impl TryFrom<&ModelRequest> for ConverseRequest {
    type Error = ProviderError;

    fn try_from(request: &ModelRequest) -> Result<Self, Self::Error> {
        let max_tokens = i32::try_from(request.max_output_tokens()).map_err(|_| {
            ProviderError::Configuration(
                "Amazon Bedrock max_output_tokens must not exceed 2147483647".to_owned(),
            )
        })?;
        let messages = request
            .messages()
            .iter()
            .map(|message| {
                let mut builder = BedrockMessage::builder().role(match message.role() {
                    Role::User => ConversationRole::User,
                    Role::Assistant => ConversationRole::Assistant,
                });
                for block in message.content() {
                    builder = builder.content(bedrock_content_block(block)?);
                }
                builder.build().map_err(|_| {
                    ProviderError::Configuration(
                        "could not construct an Amazon Bedrock message".to_owned(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tool_config = if request.tools().is_empty() {
            None
        } else {
            let mut builder = ToolConfiguration::builder();
            for tool in request.tools() {
                let specification = ToolSpecification::builder()
                    .name(tool.name())
                    .description(tool.description())
                    .input_schema(ToolInputSchema::Json(document_from_value(
                        tool.input_schema(),
                    )))
                    .build()
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "could not construct an Amazon Bedrock tool specification".to_owned(),
                        )
                    })?;
                builder = builder.tools(Tool::ToolSpec(specification));
            }
            Some(builder.build().map_err(|_| {
                ProviderError::Configuration(
                    "could not construct an Amazon Bedrock tool configuration".to_owned(),
                )
            })?)
        };

        Ok(Self {
            model_id: request.model().to_owned(),
            messages,
            tool_config,
            inference_config: InferenceConfiguration::builder()
                .max_tokens(max_tokens)
                .build(),
        })
    }
}

fn bedrock_content_block(block: &ContentBlock) -> Result<BedrockContentBlock, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(BedrockContentBlock::Text(text.clone())),
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => ToolUseBlock::builder()
            .tool_use_id(id)
            .name(name)
            .input(document_from_value(arguments))
            .build()
            .map(BedrockContentBlock::ToolUse)
            .map_err(|_| {
                ProviderError::Configuration(
                    "could not construct an Amazon Bedrock tool use block".to_owned(),
                )
            }),
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let mut builder = ToolResultBlock::builder()
                .tool_use_id(call_id)
                .content(ToolResultContentBlock::Text(content.clone()));
            if *is_error {
                builder = builder.status(ToolResultStatus::Error);
            }
            builder
                .build()
                .map(BedrockContentBlock::ToolResult)
                .map_err(|_| {
                    ProviderError::Configuration(
                        "could not construct an Amazon Bedrock tool result block".to_owned(),
                    )
                })
        }
    }
}

fn document_from_value(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(number) => Document::Number(match (number.as_u64(), number.as_i64()) {
            (Some(value), _) => Number::PosInt(value),
            (None, Some(value)) => Number::NegInt(value),
            // Without serde_json's arbitrary_precision feature, a number that
            // fits neither integer range is always representable as f64.
            (None, None) => Number::Float(number.as_f64().unwrap_or(0.0)),
        }),
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(document_from_value).collect()),
        Value::Object(entries) => Document::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), document_from_value(value)))
                .collect(),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    Refusal(String),
    ToolCallStarted {
        index: i32,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: i32,
        json: String,
    },
    BlockStopped {
        index: i32,
    },
    Completed,
    Ignored,
}

/// Attributes streamed tool-call events to ids by content-block index.
#[derive(Debug, Default)]
struct ToolCallTracker {
    calls: HashMap<i32, String>,
}

impl ToolCallTracker {
    fn start(&mut self, index: i32, id: String) -> Result<(), ProviderError> {
        match self.calls.entry(index) {
            Entry::Occupied(_) => Err(ProviderError::Protocol(
                "Amazon Bedrock stream reused a tool content-block index".to_owned(),
            )),
            Entry::Vacant(entry) => {
                entry.insert(id);
                Ok(())
            }
        }
    }

    fn arguments(&self, index: i32) -> Result<&str, ProviderError> {
        self.calls.get(&index).map(String::as_str).ok_or_else(|| {
            ProviderError::Protocol(
                "Amazon Bedrock stream sent arguments for an unknown tool call".to_owned(),
            )
        })
    }

    fn stop(&mut self, index: i32) -> Option<String> {
        self.calls.remove(&index)
    }
}

fn decode_stream_event(event: ConverseStreamOutput) -> Result<DecodedEvent, ProviderError> {
    match event {
        ConverseStreamOutput::ContentBlockDelta(event) => {
            let index = event.content_block_index;
            match event.delta {
                Some(ContentBlockDelta::Text(text)) => Ok(DecodedEvent::OutputText(text)),
                Some(ContentBlockDelta::ToolUse(delta)) => Ok(DecodedEvent::ToolCallArguments {
                    index,
                    json: delta.input,
                }),
                Some(ContentBlockDelta::Citation(_) | ContentBlockDelta::ReasoningContent(_)) => {
                    Ok(DecodedEvent::Ignored)
                }
                Some(ContentBlockDelta::ToolResult(_)) => {
                    Err(unsupported_output("tool result output"))
                }
                Some(ContentBlockDelta::Image(_)) => Err(unsupported_output("image output")),
                Some(delta) if delta.is_unknown() => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unknown content block delta".to_owned(),
                )),
                None => Err(ProviderError::Protocol(
                    "Amazon Bedrock content block delta was missing its payload".to_owned(),
                )),
                Some(_) => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unsupported content block delta".to_owned(),
                )),
            }
        }
        ConverseStreamOutput::ContentBlockStart(event) => {
            let index = event.content_block_index;
            match event.start {
                Some(ContentBlockStart::ToolUse(start)) => Ok(DecodedEvent::ToolCallStarted {
                    index,
                    id: start.tool_use_id,
                    name: start.name,
                }),
                Some(ContentBlockStart::ToolResult(_)) => {
                    Err(unsupported_output("tool result output"))
                }
                Some(ContentBlockStart::Image(_)) => Err(unsupported_output("image output")),
                Some(start) if start.is_unknown() => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unknown content block start".to_owned(),
                )),
                None => Err(ProviderError::Protocol(
                    "Amazon Bedrock content block start was missing its payload".to_owned(),
                )),
                Some(_) => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unsupported content block start".to_owned(),
                )),
            }
        }
        ConverseStreamOutput::MessageStop(event) => decode_stop_reason(event.stop_reason()),
        ConverseStreamOutput::ContentBlockStop(event) => Ok(DecodedEvent::BlockStopped {
            index: event.content_block_index,
        }),
        ConverseStreamOutput::MessageStart(_) | ConverseStreamOutput::Metadata(_) => {
            Ok(DecodedEvent::Ignored)
        }
        event if event.is_unknown() => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unknown stream event".to_owned(),
        )),
        _ => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unsupported stream event".to_owned(),
        )),
    }
}

fn decode_stop_reason(reason: &StopReason) -> Result<DecodedEvent, ProviderError> {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence | StopReason::ToolUse => {
            Ok(DecodedEvent::Completed)
        }
        StopReason::ContentFiltered => Ok(DecodedEvent::Refusal(
            "Amazon Bedrock filtered the response".to_owned(),
        )),
        StopReason::GuardrailIntervened => Ok(DecodedEvent::Refusal(
            "Amazon Bedrock guardrail intervened".to_owned(),
        )),
        StopReason::MaxTokens => Err(ProviderError::ResponseIncomplete(
            "Amazon Bedrock reached the maximum output token limit".to_owned(),
        )),
        StopReason::ModelContextWindowExceeded => Err(ProviderError::ResponseIncomplete(
            "Amazon Bedrock exceeded the model context window".to_owned(),
        )),
        StopReason::MalformedModelOutput => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: "Amazon Bedrock reported malformed model output".to_owned(),
        }),
        StopReason::MalformedToolUse => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: "Amazon Bedrock reported malformed tool use".to_owned(),
        }),
        _ => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unsupported stop reason".to_owned(),
        )),
    }
}

fn unsupported_output(kind: &str) -> ProviderError {
    ProviderError::ResponseFailed {
        kind: ProviderErrorKind::Response,
        message: format!("Amazon Bedrock returned unsupported {kind}"),
    }
}

const fn converse_error_kind(error: &ConverseStreamError) -> ProviderErrorKind {
    match error {
        ConverseStreamError::AccessDeniedException(_) => ProviderErrorKind::Authentication,
        ConverseStreamError::ThrottlingException(_) => ProviderErrorKind::RateLimited,
        ConverseStreamError::ResourceNotFoundException(_)
        | ConverseStreamError::ValidationException(_) => ProviderErrorKind::InvalidRequest,
        ConverseStreamError::InternalServerException(_)
        | ConverseStreamError::ModelErrorException(_)
        | ConverseStreamError::ModelNotReadyException(_)
        | ConverseStreamError::ModelStreamErrorException(_)
        | ConverseStreamError::ModelTimeoutException(_)
        | ConverseStreamError::ServiceUnavailableException(_) => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

const fn output_error_kind(error: &ConverseStreamOutputError) -> ProviderErrorKind {
    match error {
        ConverseStreamOutputError::ThrottlingException(_) => ProviderErrorKind::RateLimited,
        ConverseStreamOutputError::ValidationException(_) => ProviderErrorKind::InvalidRequest,
        ConverseStreamOutputError::InternalServerException(_)
        | ConverseStreamOutputError::ModelStreamErrorException(_)
        | ConverseStreamOutputError::ServiceUnavailableException(_) => {
            ProviderErrorKind::Unavailable
        }
        _ => ProviderErrorKind::Response,
    }
}

#[derive(Debug)]
struct ResponseBodyLimit {
    max_bytes: usize,
    exceeded: Arc<AtomicBool>,
}

impl ResponseBodyLimit {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            exceeded: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Intercept for ResponseBodyLimit {
    fn name(&self) -> &'static str {
        "ResponseBodyLimit"
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _config: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let response = context.response_mut();
        let body = response.take_body();
        *response.body_mut() = SdkBody::from_body_1_x(LimitedBody {
            body: Box::pin(body),
            remaining: self.max_bytes,
            exceeded: Arc::clone(&self.exceeded),
        });
        Ok(())
    }
}

struct LimitedBody {
    body: Pin<Box<SdkBody>>,
    remaining: usize,
    exceeded: Arc<AtomicBool>,
}

impl Body for LimitedBody {
    type Data = <SdkBody as Body>::Data;
    type Error = aws_smithy_types::body::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match self.body.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                let bytes = frame.data_ref().map_or(0, Bytes::len);
                if bytes > self.remaining {
                    self.exceeded.store(true, Ordering::Relaxed);
                    return Poll::Ready(Some(Err(Box::new(ResponseBodyLimitExceeded))));
                }
                self.remaining -= bytes;
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }
}

#[derive(Debug)]
struct ResponseBodyLimitExceeded;

impl fmt::Display for ResponseBodyLimitExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Amazon Bedrock response exceeded the configured wire size limit")
    }
}

impl Error for ResponseBodyLimitExceeded {}

fn request_error<R>(
    error: &SdkError<ConverseStreamError, R>,
    redactions: &[String],
    body_limit_exceeded: bool,
) -> ProviderError
where
    R: fmt::Debug,
{
    if body_limit_exceeded {
        return ProviderError::Protocol(
            "Amazon Bedrock response exceeded the configured wire size limit".to_owned(),
        );
    }
    let message = sdk_error_message(error, redactions);
    match error {
        SdkError::ConstructionFailure(_) => ProviderError::Configuration(message),
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => {
            ProviderError::Transport(message)
        }
        SdkError::ResponseError(_) => ProviderError::Protocol(message),
        SdkError::ServiceError(error) => ProviderError::ResponseFailed {
            kind: converse_error_kind(error.err()),
            message,
        },
        _ => ProviderError::Transport(message),
    }
}

fn stream_error<R>(
    error: &SdkError<ConverseStreamOutputError, R>,
    redactions: &[String],
    body_limit_exceeded: bool,
) -> ProviderError
where
    R: fmt::Debug,
{
    if body_limit_exceeded {
        return ProviderError::Protocol(
            "Amazon Bedrock response exceeded the configured wire size limit".to_owned(),
        );
    }
    let message = sdk_error_message(error, redactions);
    match error {
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => {
            ProviderError::Transport(message)
        }
        SdkError::ConstructionFailure(_) | SdkError::ResponseError(_) => {
            ProviderError::Protocol(message)
        }
        SdkError::ServiceError(error) => ProviderError::ResponseFailed {
            kind: output_error_kind(error.err()),
            message,
        },
        _ => ProviderError::Protocol(message),
    }
}

fn sdk_error_message<E>(error: &E, redactions: &[String]) -> String
where
    E: Error,
{
    sanitize_message(&DisplayErrorContext(error).to_string(), redactions)
}

fn check_stream_event_size(
    event: &ConverseStreamOutput,
    limit: usize,
) -> Result<(), ProviderError> {
    let debug_bytes = bounded_debug_size(event, limit)?;
    let event_bytes = debug_bytes
        .checked_add(EVENT_FRAME_OVERHEAD_BYTES)
        .ok_or_else(|| {
            ProviderError::Protocol("Amazon Bedrock event size overflowed".to_owned())
        })?;
    if event_bytes > limit {
        return Err(ProviderError::Protocol(
            "Amazon Bedrock event exceeded the configured size limit".to_owned(),
        ));
    }
    Ok(())
}

fn bounded_debug_size(value: &impl fmt::Debug, limit: usize) -> Result<usize, ProviderError> {
    let mut counter = BoundedLength { length: 0, limit };
    write!(&mut counter, "{value:?}").map_err(|_| {
        ProviderError::Protocol(
            "Amazon Bedrock event exceeded the configured size limit".to_owned(),
        )
    })?;
    Ok(counter.length)
}

struct BoundedLength {
    length: usize,
    limit: usize,
}

impl fmt::Write for BoundedLength {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.length = self.length.checked_add(value.len()).ok_or(fmt::Error)?;
        if self.length > self.limit {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

fn add_output_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("Amazon Bedrock output size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "Amazon Bedrock output exceeded the configured size limit".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    #[allow(deprecated)]
    use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
    use aws_credential_types::provider::ProvideCredentials;
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent, MessageStopEvent,
        ToolUseBlockDelta, ToolUseBlockStart,
    };
    use serde_json::json;

    use super::*;
    use crate::{Message, ToolSpec};

    #[test]
    fn constructor_is_network_free_and_debug_redacts_api_keys() {
        let auth = BedrockAuth::ApiKey("bedrock-test-secret".to_owned());
        let provider = Bedrock::new(auth.clone(), Some("us-east-1".to_owned())).unwrap();
        let default_provider = Bedrock::new(BedrockAuth::DefaultChain, None).unwrap();

        assert!(!format!("{auth:?}").contains("bedrock-test-secret"));
        assert!(!format!("{provider:?}").contains("bedrock-test-secret"));
        drop(default_provider);
    }

    #[tokio::test]
    async fn failed_client_initialization_is_retryable() {
        let client = OnceCell::new();
        let first = client
            .get_or_try_init(|| async { Err::<usize, _>("temporary failure") })
            .await;
        assert_eq!(first.err(), Some("temporary failure"));

        let second = client.get_or_try_init(|| async { Ok::<_, &str>(42) }).await;

        assert_eq!(second, Ok(&42));
        assert_eq!(client.get(), Some(&42));
    }

    #[tokio::test]
    async fn aws_configuration_capacity_is_fail_fast() {
        let permits = Arc::new(Semaphore::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_build = Arc::clone(&ran);
        let error = run_bounded_aws_config_build(permits, Duration::from_secs(1), move || {
            ran_in_build.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap_err();

        assert_eq!(error, AwsConfigLoadError::CapacityUnavailable);
        assert!(!ran.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn timed_out_aws_build_retains_capacity_and_rejects_excess_work() {
        let permits = Arc::new(Semaphore::new(1));
        let (release, released) = mpsc::channel();
        let error = run_bounded_aws_config_build(Arc::clone(&permits), Duration::ZERO, move || {
            released.recv().unwrap();
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(error, AwsConfigLoadError::TimedOut);
        assert_eq!(permits.available_permits(), 0);

        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_build = Arc::clone(&ran);
        let excess =
            run_bounded_aws_config_build(Arc::clone(&permits), Duration::from_secs(1), move || {
                ran_in_build.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(excess, AwsConfigLoadError::CapacityUnavailable);
        assert!(!ran.load(Ordering::Relaxed));

        release.send(()).unwrap();
        let permit = Arc::clone(&permits).acquire_owned().await.unwrap();
        assert_eq!(permits.available_permits(), 0);
        drop(permit);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn aws_build_join_and_runtime_errors_are_typed_and_sanitized() {
        let join_error = run_bounded_aws_config_build(
            Arc::new(Semaphore::new(1)),
            Duration::from_secs(1),
            || -> Result<(), AwsConfigLoadError> { panic!("blocking builder stopped") },
        )
        .await
        .unwrap_err();

        assert_eq!(join_error, AwsConfigLoadError::JoinFailed);
        for error in [
            AwsConfigLoadError::TimedOut,
            AwsConfigLoadError::CapacityUnavailable,
            AwsConfigLoadError::JoinFailed,
            AwsConfigLoadError::RuntimeConstructionFailed,
        ] {
            let provider_error = error.to_provider_error();
            assert_eq!(provider_error.kind(), ProviderErrorKind::Transport);
            assert!(
                !provider_error
                    .to_string()
                    .contains("blocking builder stopped")
            );
        }
        assert_eq!(
            AwsConfigLoadError::CredentialsUnavailable
                .to_provider_error()
                .kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn named_profiles_control_region_and_credentials_unless_region_is_explicit() {
        let permits = Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY));
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[default]\nregion = us-east-1\n[profile selected]\nregion = us-west-2\n",
            )
            .with_contents(
                ProfileFileKind::Credentials,
                "[default]\naws_access_key_id = DEFAULTKEY\naws_secret_access_key = default-secret\n\
                 [selected]\naws_access_key_id = SELECTEDKEY\naws_secret_access_key = selected-secret\n",
            )
            .build();
        let config = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            None,
            Some(profile_files.clone()),
            Arc::clone(&permits),
        )
        .await
        .unwrap();
        let credentials = config
            .credentials
            .as_ref()
            .unwrap()
            .provide_credentials()
            .await
            .unwrap();
        let sdk_credentials = config
            .sdk_config
            .credentials_provider()
            .unwrap()
            .provide_credentials()
            .await
            .unwrap();
        let explicit = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            Some("eu-central-1"),
            Some(profile_files),
            permits,
        )
        .await
        .unwrap();

        assert_eq!(credentials.access_key_id(), "SELECTEDKEY");
        assert_eq!(sdk_credentials.access_key_id(), "SELECTEDKEY");
        assert_eq!(
            config.sdk_config.region().map(Region::as_ref),
            Some("us-west-2")
        );
        assert_eq!(
            explicit.sdk_config.region().map(Region::as_ref),
            Some("eu-central-1")
        );
        assert!(config.sdk_config.http_client().is_some());
        assert!(explicit.sdk_config.http_client().is_some());
    }

    #[tokio::test]
    async fn profile_endpoint_overrides_do_not_change_bedrock_routing() {
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[profile selected]\nregion = us-east-1\nendpoint_url = http://127.0.0.1:9\n",
            )
            .with_contents(
                ProfileFileKind::Credentials,
                "[selected]\naws_access_key_id = SELECTEDKEY\naws_secret_access_key = selected-secret\n",
            )
            .build();

        let config = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            None,
            Some(profile_files),
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap();

        assert_eq!(config.sdk_config.endpoint_url(), None);
    }

    #[tokio::test]
    async fn credential_process_profiles_are_rejected_without_running_a_process() {
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[profile selected]\nregion = us-east-1\ncredential_process = this-command-must-not-run\n",
            )
            .build();

        let error = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            Some("us-east-1"),
            Some(profile_files),
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap_err();

        assert_eq!(error, AwsConfigLoadError::CredentialsUnavailable);
        assert_eq!(
            error.to_provider_error().kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn api_key_configuration_has_no_credential_lease() {
        let config = load_aws_config_with_profile_files(
            &BedrockAuth::ApiKey("test-key".to_owned()),
            Some("us-east-1"),
            None,
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap();

        assert!(config.credentials.is_none());
        assert!(config.sdk_config.credentials_provider().is_none());
    }

    #[test]
    fn rejects_invalid_auth_and_region_values() {
        for (auth, region) in [
            (BedrockAuth::ApiKey(String::new()), None),
            (BedrockAuth::ApiKey("\r\n".to_owned()), None),
            (BedrockAuth::Profile(" ".to_owned()), None),
            (BedrockAuth::DefaultChain, Some(String::new())),
            (BedrockAuth::DefaultChain, Some("bad\nregion".to_owned())),
            (
                BedrockAuth::ApiKey("bedrock-test-secret".to_owned()),
                Some("attacker.example?x=".to_owned()),
            ),
        ] {
            let error =
                Bedrock::new(auth, region).expect_err("invalid configuration must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn maps_messages_roles_and_max_output_tokens() {
        let request = ModelRequest::new(
            "anthropic.claude-test",
            vec![Message::user("hello"), Message::assistant("hi")],
            512,
        );
        let mapped = ConverseRequest::try_from(&request).unwrap();

        assert_eq!(mapped.model_id, "anthropic.claude-test");
        assert_eq!(mapped.messages.len(), 2);
        assert_eq!(mapped.messages[0].role(), &ConversationRole::User);
        assert_eq!(mapped.messages[1].role(), &ConversationRole::Assistant);
        assert_eq!(mapped.messages[0].content()[0].as_text().unwrap(), "hello");
        assert_eq!(mapped.messages[1].content()[0].as_text().unwrap(), "hi");
        assert_eq!(mapped.inference_config.max_tokens(), Some(512));
        assert!(mapped.tool_config.is_none());
    }

    #[test]
    fn maps_tool_declarations_and_tool_history_blocks() {
        let request = ModelRequest::new(
            "anthropic.claude-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::ToolCall {
                            id: "toolu_1".to_owned(),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": "config.ron"}),
                        },
                    ],
                ),
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "toolu_1".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "toolu_2".to_owned(),
                        content: "denied".to_owned(),
                        is_error: true,
                    },
                ]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let mapped = ConverseRequest::try_from(&request).unwrap();

        let assistant = mapped.messages[1].content();
        assert_eq!(assistant[0].as_text().unwrap(), "Reading it now.");
        let tool_use = assistant[1].as_tool_use().unwrap();
        assert_eq!(tool_use.tool_use_id(), "toolu_1");
        assert_eq!(tool_use.name(), "read_file");
        assert_eq!(
            tool_use.input(),
            &Document::Object(HashMap::from([(
                "path".to_owned(),
                Document::String("config.ron".to_owned()),
            )]))
        );

        assert_eq!(mapped.messages[2].role(), &ConversationRole::User);
        let results = mapped.messages[2].content();
        let success = results[0].as_tool_result().unwrap();
        assert_eq!(success.tool_use_id(), "toolu_1");
        assert_eq!(success.content()[0].as_text().unwrap(), "(config)");
        assert_eq!(success.status(), None);
        let failure = results[1].as_tool_result().unwrap();
        assert_eq!(failure.tool_use_id(), "toolu_2");
        assert_eq!(failure.content()[0].as_text().unwrap(), "denied");
        assert_eq!(failure.status(), Some(&ToolResultStatus::Error));

        let tools = mapped.tool_config.unwrap();
        let specification = tools.tools()[0].as_tool_spec().unwrap();
        assert_eq!(specification.name(), "read_file");
        assert_eq!(specification.description(), Some("Reads one file"));
        assert_eq!(
            specification.input_schema().unwrap().as_json().unwrap(),
            &document_from_value(
                &json!({"type": "object", "properties": {"path": {"type": "string"}}})
            )
        );
    }

    #[test]
    fn converts_json_values_to_smithy_documents() {
        let value = json!({
            "text": "path",
            "count": 3,
            "offset": -7,
            "ratio": 0.5,
            "flag": true,
            "missing": null,
            "items": [1, "two"],
        });

        assert_eq!(
            document_from_value(&value),
            Document::Object(HashMap::from([
                ("text".to_owned(), Document::String("path".to_owned())),
                ("count".to_owned(), Document::Number(Number::PosInt(3))),
                ("offset".to_owned(), Document::Number(Number::NegInt(-7))),
                ("ratio".to_owned(), Document::Number(Number::Float(0.5))),
                ("flag".to_owned(), Document::Bool(true)),
                ("missing".to_owned(), Document::Null),
                (
                    "items".to_owned(),
                    Document::Array(vec![
                        Document::Number(Number::PosInt(1)),
                        Document::String("two".to_owned()),
                    ]),
                ),
            ]))
        );
    }

    #[test]
    fn rejects_max_output_tokens_that_do_not_fit_bedrock() {
        let request = ModelRequest::new("model", vec![Message::user("hello")], u32::MAX);
        let error = ConverseRequest::try_from(&request)
            .expect_err("out-of-range token count must be rejected");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn decodes_text_and_successful_stop_events() {
        let text = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::Text("hello".to_owned()))
                .build()
                .unwrap(),
        );
        let stop = |reason| {
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(reason)
                    .build()
                    .unwrap(),
            )
        };

        assert_eq!(
            decode_stream_event(text).unwrap(),
            DecodedEvent::OutputText("hello".to_owned())
        );
        assert_eq!(
            decode_stream_event(stop(StopReason::EndTurn)).unwrap(),
            DecodedEvent::Completed
        );
        assert_eq!(
            decode_stream_event(stop(StopReason::StopSequence)).unwrap(),
            DecodedEvent::Completed
        );
    }

    #[test]
    fn classifies_refusal_incomplete_tool_and_unknown_stops() {
        for reason in [StopReason::ContentFiltered, StopReason::GuardrailIntervened] {
            assert!(matches!(
                decode_stop_reason(&reason).unwrap(),
                DecodedEvent::Refusal(_)
            ));
        }

        for reason in [
            StopReason::MaxTokens,
            StopReason::ModelContextWindowExceeded,
        ] {
            assert!(matches!(
                decode_stop_reason(&reason),
                Err(ProviderError::ResponseIncomplete(_))
            ));
        }

        assert_eq!(
            decode_stop_reason(&StopReason::ToolUse).unwrap(),
            DecodedEvent::Completed
        );
        let unknown_error = decode_stop_reason(&StopReason::from("future_reason")).unwrap_err();
        assert!(matches!(unknown_error, ProviderError::Protocol(_)));
    }

    #[test]
    fn decodes_tool_call_stream_events() {
        let started = ConverseStreamOutput::ContentBlockStart(
            ContentBlockStartEvent::builder()
                .content_block_index(1)
                .start(ContentBlockStart::ToolUse(
                    ToolUseBlockStart::builder()
                        .tool_use_id("toolu_1")
                        .name("read_file")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        );
        let arguments = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(1)
                .delta(ContentBlockDelta::ToolUse(
                    ToolUseBlockDelta::builder()
                        .input("{\"path\":")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        );
        let stopped = ConverseStreamOutput::ContentBlockStop(
            ContentBlockStopEvent::builder()
                .content_block_index(1)
                .build()
                .unwrap(),
        );
        let stop = ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(StopReason::ToolUse)
                .build()
                .unwrap(),
        );

        assert_eq!(
            decode_stream_event(started).unwrap(),
            DecodedEvent::ToolCallStarted {
                index: 1,
                id: "toolu_1".to_owned(),
                name: "read_file".to_owned(),
            }
        );
        assert_eq!(
            decode_stream_event(arguments).unwrap(),
            DecodedEvent::ToolCallArguments {
                index: 1,
                json: "{\"path\":".to_owned(),
            }
        );
        assert_eq!(
            decode_stream_event(stopped).unwrap(),
            DecodedEvent::BlockStopped { index: 1 }
        );
        assert_eq!(decode_stream_event(stop).unwrap(), DecodedEvent::Completed);
    }

    #[test]
    fn attributes_tool_calls_by_index_and_rejects_unknown_or_reused_indexes() {
        let mut tracker = ToolCallTracker::default();
        tracker.start(1, "toolu_1".to_owned()).unwrap();

        assert_eq!(tracker.arguments(1).unwrap(), "toolu_1");
        let unknown = tracker.arguments(4).unwrap_err();
        assert!(matches!(unknown, ProviderError::Protocol(_)));
        let reused = tracker.start(1, "toolu_2".to_owned()).unwrap_err();
        assert!(matches!(reused, ProviderError::Protocol(_)));
        assert_eq!(tracker.stop(1), Some("toolu_1".to_owned()));
        assert_eq!(tracker.stop(1), None);
    }

    #[test]
    fn enforces_output_and_event_size_limits() {
        let event = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::Text("oversized".to_owned()))
                .build()
                .unwrap(),
        );
        let event_error = check_stream_event_size(&event, 8).unwrap_err();
        let output_error = add_output_bytes(&mut 7, 2, 8).unwrap_err();

        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn enforces_the_raw_response_body_limit() {
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = LimitedBody {
            body: Box::pin(SdkBody::from("too large")),
            remaining: 4,
            exceeded: Arc::clone(&exceeded),
        };
        let frame = std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context))
            .await
            .expect("body should return one frame");

        assert!(frame.is_err());
        assert!(exceeded.load(Ordering::Relaxed));
    }

    #[test]
    fn disables_sdk_retries_and_selects_the_requested_auth_scheme() {
        let shared_config = SdkConfig::builder()
            .region(Region::new("us-east-1"))
            .build();
        let aws = service_config(&shared_config, None);
        let api_key = service_config(&shared_config, Some("bedrock-test-secret".to_owned()));

        assert_eq!(aws.retry_config().unwrap().max_attempts(), 1);
        assert_eq!(api_key.retry_config().unwrap().max_attempts(), 1);
        assert!(format!("{:?}", aws.auth_scheme_preference()).contains("sigv4"));
        assert!(format!("{:?}", api_key.auth_scheme_preference()).contains("httpBearerAuth"));
    }

    #[test]
    fn sanitizes_secrets_controls_and_long_error_messages() {
        let message = format!("bedrock-test-secret\n{}", "x".repeat(2_000));
        let sanitized = sanitize_message(&message, &["bedrock-test-secret".to_owned()]);

        assert!(!sanitized.contains("bedrock-test-secret"));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("[REDACTED]"));
        assert!(sanitized.chars().count() <= crate::sanitize::ERROR_MESSAGE_CHARS_LIMIT);
    }
}
