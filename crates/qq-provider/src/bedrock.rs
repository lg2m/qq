//! Amazon Bedrock `ConverseStream` adapter.

use std::{
    error::Error,
    fmt::{self, Write as _},
    pin::Pin,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use async_stream::try_stream;
use aws_config::{BehaviorVersion, Region, SdkConfig, retry::RetryConfig};
use aws_sdk_bedrockruntime::{
    Client,
    config::{
        Config, ConfigBag, Intercept, RuntimeComponents, Token,
        interceptors::BeforeDeserializationInterceptorContextMut,
    },
    error::{BoxError, DisplayErrorContext, SdkError},
    operation::converse_stream::ConverseStreamError,
    types::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseStreamOutput,
        InferenceConfiguration, Message as BedrockMessage, StopReason,
        error::ConverseStreamOutputError,
    },
};
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use futures_util::{FutureExt, future::BoxFuture, future::Shared};
use http_body::Body;

use crate::{
    ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderStream, Role,
    limits::StreamLimits, sanitize::sanitize_message,
};

const EVENT_FRAME_OVERHEAD_BYTES: usize = 64;

type ClientFuture = Shared<BoxFuture<'static, Client>>;

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
    client: ClientFuture,
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
        let client = load_client(auth, region).boxed().shared();

        Ok(Self { client, redactions })
    }
}

impl Provider for Bedrock {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let redactions = Arc::clone(&self.redactions);

        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let request = ConverseRequest::try_from(&request)?;
            let client = client.await;
            let body_limit = ResponseBodyLimit::new(limits.wire);
            let body_limit_exceeded = Arc::clone(&body_limit.exceeded);
            let response = client
                .converse_stream()
                .model_id(request.model_id)
                .set_messages(Some(request.messages))
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

fn validate_configuration(auth: &BedrockAuth, region: Option<&str>) -> Result<(), ProviderError> {
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

    if region.is_some_and(invalid_configuration_value) {
        return Err(ProviderError::Configuration(
            "AWS region must not be empty or contain control characters".to_owned(),
        ));
    }

    Ok(())
}

fn invalid_configuration_value(value: &str) -> bool {
    value.trim().is_empty() || value.chars().any(char::is_control)
}

async fn load_client(auth: BedrockAuth, region: Option<String>) -> Client {
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).retry_config(RetryConfig::disabled());
    if let Some(region) = region {
        loader = loader.region(Region::new(region));
    }
    match &auth {
        BedrockAuth::DefaultChain => {}
        BedrockAuth::Profile(profile) => loader = loader.profile_name(profile),
        BedrockAuth::ApiKey(_) => loader = loader.no_credentials(),
    }

    let shared_config = loader.load().await;
    let api_key = match auth {
        BedrockAuth::ApiKey(secret) => Some(secret),
        BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => None,
    };
    Client::from_conf(service_config(&shared_config, api_key))
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
                BedrockMessage::builder()
                    .role(match message.role() {
                        Role::User => ConversationRole::User,
                        Role::Assistant => ConversationRole::Assistant,
                    })
                    .content(ContentBlock::Text(message.content().to_owned()))
                    .build()
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "could not construct an Amazon Bedrock message".to_owned(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            model_id: request.model().to_owned(),
            messages,
            inference_config: InferenceConfiguration::builder()
                .max_tokens(max_tokens)
                .build(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    Refusal(String),
    Completed,
    Ignored,
}

fn decode_stream_event(event: ConverseStreamOutput) -> Result<DecodedEvent, ProviderError> {
    match event {
        ConverseStreamOutput::ContentBlockDelta(event) => match event.delta {
            Some(ContentBlockDelta::Text(text)) => Ok(DecodedEvent::OutputText(text)),
            Some(ContentBlockDelta::Citation(_) | ContentBlockDelta::ReasoningContent(_)) => {
                Ok(DecodedEvent::Ignored)
            }
            Some(ContentBlockDelta::ToolUse(_) | ContentBlockDelta::ToolResult(_)) => {
                Err(unsupported_output("tool use"))
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
        },
        ConverseStreamOutput::ContentBlockStart(event) => match event.start {
            Some(ContentBlockStart::ToolUse(_) | ContentBlockStart::ToolResult(_)) => {
                Err(unsupported_output("tool use"))
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
        },
        ConverseStreamOutput::MessageStop(event) => decode_stop_reason(event.stop_reason()),
        ConverseStreamOutput::ContentBlockStop(_)
        | ConverseStreamOutput::MessageStart(_)
        | ConverseStreamOutput::Metadata(_) => Ok(DecodedEvent::Ignored),
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
        StopReason::EndTurn | StopReason::StopSequence => Ok(DecodedEvent::Completed),
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
        StopReason::ToolUse => Err(unsupported_output("tool use")),
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
    use aws_sdk_bedrockruntime::types::{ContentBlockDeltaEvent, MessageStopEvent};

    use super::*;
    use crate::Message;

    #[test]
    fn constructor_is_network_free_and_debug_redacts_api_keys() {
        let auth = BedrockAuth::ApiKey("bedrock-test-secret".to_owned());
        let provider = Bedrock::new(auth.clone(), Some("us-east-1".to_owned())).unwrap();
        let default_provider = Bedrock::new(BedrockAuth::DefaultChain, None).unwrap();

        assert!(!format!("{auth:?}").contains("bedrock-test-secret"));
        assert!(!format!("{provider:?}").contains("bedrock-test-secret"));
        drop(default_provider);
    }

    #[test]
    fn rejects_invalid_auth_and_region_values() {
        for (auth, region) in [
            (BedrockAuth::ApiKey(String::new()), None),
            (BedrockAuth::ApiKey("\r\n".to_owned()), None),
            (BedrockAuth::Profile(" ".to_owned()), None),
            (BedrockAuth::DefaultChain, Some(String::new())),
            (BedrockAuth::DefaultChain, Some("bad\nregion".to_owned())),
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

        let tool_error = decode_stop_reason(&StopReason::ToolUse).unwrap_err();
        let unknown_error = decode_stop_reason(&StopReason::from("future_reason")).unwrap_err();
        assert!(matches!(tool_error, ProviderError::ResponseFailed { .. }));
        assert!(matches!(unknown_error, ProviderError::Protocol(_)));
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
