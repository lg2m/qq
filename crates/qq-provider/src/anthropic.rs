//! Anthropic Messages API adapter.

use std::{collections::HashMap, fmt, sync::Arc};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentBlock, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, ProviderUsage, Role, ToolSpec,
    http::{build_client, build_direct_client, validate_endpoint},
    limits::StreamLimits,
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
};

const MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

/// Authentication applied by an Anthropic-compatible Messages client.
#[derive(Clone, PartialEq, Eq)]
pub enum AnthropicAuth {
    NoAuth,
    XApiKey(String),
    Bearer(String),
    Header(String, String),
}

impl fmt::Debug for AnthropicAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAuth => formatter.write_str("NoAuth"),
            Self::XApiKey(_) => formatter
                .debug_tuple("XApiKey")
                .field(&"<redacted>")
                .finish(),
            Self::Bearer(_) => formatter
                .debug_tuple("Bearer")
                .field(&"<redacted>")
                .finish(),
            Self::Header(name, _) => formatter
                .debug_tuple("Header")
                .field(name)
                .field(&"<redacted>")
                .finish(),
        }
    }
}

/// A client for Anthropic-compatible Messages endpoints.
pub struct AnthropicMessages {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    redactions: Arc<[String]>,
    authorizer: RequestAuthorizer,
}

impl AnthropicMessages {
    /// Creates a client for Anthropic's standard Messages endpoint.
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        Self::with_endpoint(
            MESSAGES_ENDPOINT,
            AnthropicAuth::XApiKey(api_key.to_owned()),
            [],
            false,
        )
    }

    /// Creates a client for an exact Anthropic-compatible endpoint URL.
    ///
    /// Plain HTTP is accepted only when `allow_http` is true and the URL host is
    /// loopback. The Anthropic version defaults to `2023-06-01`.
    pub fn with_endpoint(
        endpoint: &str,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
    ) -> Result<Self, ProviderError> {
        Self::with_endpoint_and_version(
            endpoint,
            auth,
            static_headers,
            allow_http,
            DEFAULT_ANTHROPIC_VERSION,
        )
    }

    /// Creates a client with an explicit `anthropic-version` header value.
    pub fn with_endpoint_and_version(
        endpoint: &str,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
        anthropic_version: &str,
    ) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(endpoint, allow_http)?;
        let client = if endpoint.scheme() == "http" {
            build_direct_client()?
        } else {
            build_client()?
        };
        Self::with_client_and_version(client, endpoint, auth, static_headers, anthropic_version)
    }

    pub(crate) fn with_client(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ProviderError> {
        Self::with_client_and_version(
            client,
            endpoint,
            auth,
            static_headers,
            DEFAULT_ANTHROPIC_VERSION,
        )
    }

    pub(crate) fn with_client_and_version(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        anthropic_version: &str,
    ) -> Result<Self, ProviderError> {
        Self::with_client_authorizer_and_version(
            client,
            endpoint,
            auth,
            static_headers,
            RequestAuthorizer::default(),
            anthropic_version,
        )
    }

    pub(crate) fn with_client_and_authorizer(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
    ) -> Result<Self, ProviderError> {
        Self::with_client_authorizer_and_version(
            client,
            endpoint,
            auth,
            static_headers,
            authorizer,
            DEFAULT_ANTHROPIC_VERSION,
        )
    }

    fn with_client_authorizer_and_version(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
        anthropic_version: &str,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers, anthropic_version)?;

        Ok(Self {
            client,
            endpoint,
            headers,
            redactions: Arc::from(redactions),
            authorizer,
        })
    }
}

impl Provider for AnthropicMessages {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let redactions = Arc::clone(&self.redactions);
        let authorizer = self.authorizer.clone();

        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let body = MessagesRequest::from(&request);
            let mut wire_request = client
                .post(endpoint)
                .headers(headers)
                .header(ACCEPT, "text/event-stream")
                .json(&body)
                .build()
                .map_err(|error| transport_error(error, redactions.as_ref()))?;
            authorizer.authorize(&mut wire_request).await?;
            let response = client
                .execute(wire_request)
                .await
                .map_err(|error| transport_error(error, redactions.as_ref()))?;

            let response = if response.status().is_success() {
                response
            } else {
                Err(api_error(response, redactions.as_ref()).await)?
            };

            if !is_event_stream(&response) {
                Err(ProviderError::Protocol(
                    "Anthropic-compatible provider returned a non-SSE response".to_owned(),
                ))?;
            }

            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::new(limits.event);
            let mut output_bytes = 0_usize;
            let mut wire_bytes = 0_usize;
            let mut usage = None;
            // Maps streamed content-block indexes to tool-call ids so argument
            // deltas and block stops can be attributed after the start event.
            let mut tool_calls: HashMap<u64, String> = HashMap::new();

            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|error| transport_error(error, redactions.as_ref()))?;
                add_wire_bytes(&mut wire_bytes, chunk.len(), limits.wire)?;

                for event in decoder.push(&chunk)? {
                    match decode_event(event, redactions.as_ref())? {
                        DecodedEvent::OutputText(text) => {
                            if text.is_empty() {
                                continue;
                            }
                            add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                            yield ProviderEvent::OutputTextDelta { text };
                        }
                        DecodedEvent::MessageStart(start) => {
                            if let Some(start) = start
                                && usage.replace(start).is_some()
                            {
                                Err(ProviderError::Protocol(
                                    "Anthropic-compatible stream reported starting usage more than once".to_owned(),
                                ))?;
                            }
                        }
                        DecodedEvent::MessageDelta { refusal, output_tokens } => {
                            if let Some(text) = refusal {
                                add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                                yield ProviderEvent::RefusalDelta { text };
                            }
                            if let Some(output_tokens) = output_tokens {
                                let current = usage.as_mut().ok_or_else(|| {
                                    ProviderError::Protocol(
                                        "Anthropic-compatible stream reported output usage before starting usage".to_owned(),
                                    )
                                })?;
                                if output_tokens < current.output_tokens {
                                    Err(ProviderError::Protocol(
                                        "Anthropic-compatible cumulative output usage decreased".to_owned(),
                                    ))?;
                                }
                                current.output_tokens = output_tokens;
                            }
                        }
                        DecodedEvent::ToolCallStarted { index, id, name } => {
                            if tool_calls.insert(index, id.clone()).is_some() {
                                Err(ProviderError::Protocol(
                                    "Anthropic-compatible stream reused a tool content-block index"
                                        .to_owned(),
                                ))?;
                            }
                            yield ProviderEvent::ToolCallStarted { id, name };
                        }
                        DecodedEvent::ToolCallArguments { index, json } => {
                            match tool_calls.get(&index) {
                                Some(id) => {
                                    add_output_bytes(&mut output_bytes, json.len(), limits.output)?;
                                    yield ProviderEvent::ToolCallArgumentsDelta {
                                        id: id.clone(),
                                        json,
                                    };
                                }
                                None => {
                                    Err(ProviderError::Protocol(
                                        "Anthropic-compatible stream sent arguments for an unknown tool call"
                                            .to_owned(),
                                    ))?;
                                }
                            }
                        }
                        DecodedEvent::BlockStopped { index } => {
                            if let Some(id) = tool_calls.remove(&index) {
                                yield ProviderEvent::ToolCallCompleted { id };
                            }
                        }
                        DecodedEvent::Completed => {
                            yield ProviderEvent::Completed { usage };
                            return;
                        }
                        DecodedEvent::Ignored => {}
                    }
                }
            }

            Err(ProviderError::Protocol(
                "Anthropic-compatible stream ended before message_stop".to_owned(),
            ))?;
        })
    }
}

fn build_headers(
    auth: AnthropicAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
    anthropic_version: &str,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    if anthropic_version.trim().is_empty() {
        return Err(ProviderError::Configuration(
            "anthropic-version must not be empty".to_owned(),
        ));
    }
    let version = HeaderValue::from_str(anthropic_version).map_err(|_| {
        ProviderError::Configuration(
            "anthropic-version is not a valid HTTP header value".to_owned(),
        )
    })?;

    let mut redactions = vec![anthropic_version.to_owned()];
    let auth_header = match auth {
        AnthropicAuth::NoAuth => None,
        AnthropicAuth::XApiKey(secret) => {
            let value = sensitive_secret_header(&secret, "x-api-key")?;
            redactions.push(secret);
            Some((X_API_KEY, value))
        }
        AnthropicAuth::Bearer(secret) => {
            if secret.trim().is_empty() {
                return Err(ProviderError::Configuration(
                    "Bearer secret must not be empty".to_owned(),
                ));
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                ProviderError::Configuration(
                    "Bearer secret is not a valid HTTP header value".to_owned(),
                )
            })?;
            value.set_sensitive(true);
            redactions.push(secret);
            Some((AUTHORIZATION, value))
        }
        AnthropicAuth::Header(name, secret) => {
            if secret.trim().is_empty() {
                return Err(ProviderError::Configuration(
                    "authentication header secret must not be empty".to_owned(),
                ));
            }
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if name == ANTHROPIC_VERSION || is_request_controlled_header(&name) {
                return Err(ProviderError::Configuration(
                    "authentication header is controlled by the provider".to_owned(),
                ));
            }
            let mut value = HeaderValue::from_str(&secret).map_err(|_| {
                ProviderError::Configuration(
                    "authentication secret is not a valid HTTP header value".to_owned(),
                )
            })?;
            value.set_sensitive(true);
            redactions.push(secret);
            Some((name, value))
        }
    };
    let auth_name = auth_header.as_ref().map(|(name, _)| name);

    let mut headers = HeaderMap::new();
    for (name, value) in static_headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::Configuration("static header name is invalid".to_owned())
        })?;
        if name == AUTHORIZATION
            || name == X_API_KEY
            || name == ANTHROPIC_VERSION
            || auth_name.is_some_and(|auth_name| auth_name == name)
            || is_request_controlled_header(&name)
        {
            return Err(ProviderError::Configuration(format!(
                "static header `{name}` is controlled by the provider"
            )));
        }
        if headers.contains_key(&name) {
            return Err(ProviderError::Configuration(format!(
                "static header `{name}` is duplicated"
            )));
        }

        let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
            ProviderError::Configuration("static header value is invalid".to_owned())
        })?;
        header_value.set_sensitive(true);
        if !value.trim().is_empty() {
            redactions.push(value);
        }
        headers.insert(name, header_value);
    }

    headers.insert(ANTHROPIC_VERSION, version);
    if let Some((name, value)) = auth_header {
        headers.insert(name, value);
    }

    redactions.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    redactions.dedup();
    Ok((headers, redactions))
}

fn sensitive_secret_header(secret: &str, name: &str) -> Result<HeaderValue, ProviderError> {
    if secret.trim().is_empty() {
        return Err(ProviderError::Configuration(format!(
            "{name} secret must not be empty"
        )));
    }
    let mut value = HeaderValue::from_str(secret).map_err(|_| {
        ProviderError::Configuration(format!("{name} secret is not a valid HTTP header value"))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn is_request_controlled_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "expect"
            | "host"
            | "http2-settings"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
    )
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
}

#[derive(Debug, PartialEq, Eq)]
struct SseEvent {
    name: Option<String>,
    data: String,
}

struct SseDecoder {
    bom_prefix: Vec<u8>,
    bom_checked: bool,
    line: Vec<u8>,
    event_name: Option<String>,
    data: Vec<u8>,
    event_bytes: usize,
    max_event_bytes: usize,
    skip_line_feed: bool,
}

impl SseDecoder {
    const fn new(max_event_bytes: usize) -> Self {
        Self {
            bom_prefix: Vec::new(),
            bom_checked: false,
            line: Vec::new(),
            event_name: None,
            data: Vec::new(),
            event_bytes: 0,
            max_event_bytes,
            skip_line_feed: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();

        for &byte in bytes {
            if !self.bom_checked {
                self.bom_prefix.push(byte);
                if b"\xef\xbb\xbf".starts_with(&self.bom_prefix) {
                    if self.bom_prefix.len() == 3 {
                        self.bom_prefix.clear();
                        self.bom_checked = true;
                    }
                    continue;
                }

                self.bom_checked = true;
                for prefix_byte in std::mem::take(&mut self.bom_prefix) {
                    self.push_byte(prefix_byte, &mut events)?;
                }
                continue;
            }

            self.push_byte(byte, &mut events)?;
        }

        Ok(events)
    }

    fn push_byte(&mut self, byte: u8, events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        if self.skip_line_feed {
            self.skip_line_feed = false;
            if byte == b'\n' {
                return Ok(());
            }
        }

        match byte {
            b'\r' => {
                if let Some(event) = self.finish_line()? {
                    events.push(event);
                }
                self.skip_line_feed = true;
            }
            b'\n' => {
                if let Some(event) = self.finish_line()? {
                    events.push(event);
                }
            }
            _ => {
                self.event_bytes = self.event_bytes.checked_add(1).ok_or_else(|| {
                    ProviderError::Protocol(
                        "Anthropic-compatible SSE event size overflowed".to_owned(),
                    )
                })?;
                if self.event_bytes > self.max_event_bytes {
                    return Err(ProviderError::Protocol(
                        "Anthropic-compatible SSE event exceeded the configured size limit"
                            .to_owned(),
                    ));
                }
                self.line.push(byte);
            }
        }

        Ok(())
    }

    fn finish_line(&mut self) -> Result<Option<SseEvent>, ProviderError> {
        if self.line.is_empty() {
            self.event_bytes = 0;
            let name = self.event_name.take();
            if self.data.is_empty() {
                return Ok(None);
            }

            self.data.pop();
            let data = String::from_utf8(std::mem::take(&mut self.data)).map_err(|_| {
                ProviderError::Protocol(
                    "Anthropic-compatible SSE event data was not UTF-8".to_owned(),
                )
            })?;
            return Ok(Some(SseEvent { name, data }));
        }

        let line = std::mem::take(&mut self.line);
        if line.starts_with(b":") {
            return Ok(None);
        }

        let (field, value) = line.iter().position(|byte| *byte == b':').map_or_else(
            || (line.as_slice(), &[][..]),
            |colon| {
                let value = line[colon + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[colon + 1..]);
                (&line[..colon], value)
            },
        );
        match field {
            b"event" => {
                self.event_name = Some(String::from_utf8(value.to_vec()).map_err(|_| {
                    ProviderError::Protocol(
                        "Anthropic-compatible SSE event name was not UTF-8".to_owned(),
                    )
                })?);
            }
            b"data" => {
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
            _ => {}
        }

        Ok(None)
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
    max_tokens: u32,
    stream: bool,
}

impl<'a> From<&'a ModelRequest> for MessagesRequest<'a> {
    fn from(request: &'a ModelRequest) -> Self {
        Self {
            model: request.model(),
            messages: request
                .messages()
                .iter()
                .map(AnthropicMessage::from)
                .collect(),
            tools: request.tools().iter().map(AnthropicTool::from).collect(),
            max_tokens: request.max_output_tokens(),
            stream: true,
        }
    }
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

impl<'a> From<&'a ToolSpec> for AnthropicTool<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self {
            name: tool.name(),
            description: tool.description(),
            input_schema: tool.input_schema(),
        }
    }
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: AnthropicRole,
    content: AnthropicContent<'a>,
}

impl<'a> From<&'a Message> for AnthropicMessage<'a> {
    fn from(message: &'a Message) -> Self {
        // A single text block serializes as a plain string so tool-less
        // requests keep their existing wire shape.
        let content = match message.content() {
            [ContentBlock::Text { text }] => AnthropicContent::Text(text),
            blocks => AnthropicContent::Blocks(blocks.iter().map(AnthropicBlock::from).collect()),
        };
        Self {
            role: match message.role() {
                Role::User => AnthropicRole::User,
                Role::Assistant => AnthropicRole::Assistant,
            },
            content,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent<'a> {
    Text(&'a str),
    Blocks(Vec<AnthropicBlock<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

impl<'a> From<&'a ContentBlock> for AnthropicBlock<'a> {
    fn from(block: &'a ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self::Text { text },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolUse {
                id,
                name,
                input: arguments,
            },
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                tool_use_id: call_id,
                content,
                is_error: *is_error,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum AnthropicRole {
    User,
    Assistant,
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StreamingEvent {
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u64, delta: ContentDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDelta,
        usage: Option<MessageDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: WireApiError },
    #[serde(rename = "message_start")]
    MessageStart { message: StartedMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u64,
        content_block: StartedBlock,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u64 },
    #[serde(rename = "ping")]
    Ping,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StartedBlock {
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
    stop_details: Option<StopDetails>,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    output_tokens: u64,
}

#[derive(Deserialize)]
struct StartedMessage {
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct StopDetails {
    #[serde(rename = "type")]
    detail_type: Option<String>,
    explanation: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: WireApiError,
}

#[derive(Deserialize)]
struct WireApiError {
    message: Option<String>,
    code: Option<Value>,
    #[serde(rename = "type")]
    error_type: Option<String>,
    status: Option<Value>,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    MessageStart(Option<ProviderUsage>),
    MessageDelta {
        refusal: Option<String>,
        output_tokens: Option<u64>,
    },
    ToolCallStarted {
        index: u64,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: u64,
        json: String,
    },
    BlockStopped {
        index: u64,
    },
    Completed,
    Ignored,
}

fn decode_event(event: SseEvent, redactions: &[String]) -> Result<DecodedEvent, ProviderError> {
    if event.data.trim().is_empty() {
        return Ok(DecodedEvent::Ignored);
    }

    let envelope: EventEnvelope = serde_json::from_str(&event.data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Anthropic-compatible event envelope: {error}"),
            redactions,
        ))
    })?;
    if event
        .name
        .as_deref()
        .is_some_and(|name| name != envelope.event_type)
    {
        return Err(ProviderError::Protocol(
            "Anthropic-compatible SSE event name did not match its payload type".to_owned(),
        ));
    }

    let event: StreamingEvent = serde_json::from_str(&event.data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Anthropic-compatible event: {error}"),
            redactions,
        ))
    })?;

    match event {
        StreamingEvent::ContentBlockDelta {
            delta: ContentDelta::Text { text },
            ..
        } => Ok(DecodedEvent::OutputText(text)),
        StreamingEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::InputJson { partial_json },
        } => Ok(DecodedEvent::ToolCallArguments {
            index,
            json: partial_json,
        }),
        StreamingEvent::ContentBlockStart {
            index,
            content_block: StartedBlock::ToolUse { id, name },
        } => Ok(DecodedEvent::ToolCallStarted { index, id, name }),
        StreamingEvent::ContentBlockStop { index } => Ok(DecodedEvent::BlockStopped { index }),
        StreamingEvent::ContentBlockDelta {
            delta: ContentDelta::Thinking | ContentDelta::Other,
            ..
        }
        | StreamingEvent::ContentBlockStart {
            content_block: StartedBlock::Other,
            ..
        }
        | StreamingEvent::Ping
        | StreamingEvent::Other => Ok(DecodedEvent::Ignored),
        StreamingEvent::MessageStart { message } => {
            Ok(DecodedEvent::MessageStart(message.usage.map(|usage| {
                ProviderUsage {
                    input_tokens: usage.input_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_write_input_tokens: usage.cache_creation_input_tokens,
                    output_tokens: usage.output_tokens,
                }
            })))
        }
        StreamingEvent::MessageDelta { delta, usage } => Ok(DecodedEvent::MessageDelta {
            refusal: decode_message_delta(delta, redactions)?,
            output_tokens: usage.map(|usage| usage.output_tokens),
        }),
        StreamingEvent::MessageStop => Ok(DecodedEvent::Completed),
        StreamingEvent::Error { error } => Err(wire_api_error(error, redactions)),
    }
}

fn decode_message_delta(
    delta: MessageDelta,
    redactions: &[String],
) -> Result<Option<String>, ProviderError> {
    let details_are_refusal = delta
        .stop_details
        .as_ref()
        .and_then(|details| details.detail_type.as_deref())
        == Some("refusal");
    if delta.stop_reason.as_deref() == Some("refusal") || details_are_refusal {
        let explanation = delta
            .stop_details
            .and_then(|details| details.explanation)
            .filter(|explanation| !explanation.trim().is_empty())
            .map_or_else(
                || "Anthropic declined the request".to_owned(),
                |explanation| sanitize_message(&explanation, redactions),
            );
        return Ok(Some(explanation));
    }

    match delta.stop_reason.as_deref() {
        None | Some("end_turn" | "stop_sequence" | "tool_use") => Ok(None),
        Some("max_tokens" | "model_context_window_exceeded") => {
            Err(ProviderError::ResponseIncomplete(
                "Anthropic response reached a configured model limit".to_owned(),
            ))
        }
        Some("pause_turn") => Err(ProviderError::ResponseIncomplete(
            "Anthropic paused the response before completion".to_owned(),
        )),
        Some(_) => Err(ProviderError::Protocol(
            "Anthropic response used an unsupported stop reason".to_owned(),
        )),
    }
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = wire_error_kind(&error);
    let message = error.message.as_deref().map_or_else(
        || "Anthropic-compatible provider did not provide an error message".to_owned(),
        |message| sanitize_message(message, redactions),
    );
    ProviderError::ResponseFailed { kind, message }
}

fn wire_error_kind(error: &WireApiError) -> ProviderErrorKind {
    if let Some(status) = error.status.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }
    if let Some(status) = error.code.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }

    for name in [
        error.code.as_ref().and_then(Value::as_str),
        error.error_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let kind = named_error_kind(name);
        if kind != ProviderErrorKind::Response {
            return kind;
        }
    }

    ProviderErrorKind::Response
}

fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn status_error_kind(status: u16) -> ProviderErrorKind {
    match status {
        400 | 404 | 409 | 413 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn named_error_kind(name: &str) -> ProviderErrorKind {
    match name.to_ascii_lowercase().as_str() {
        "authentication_error" | "invalid_api_key" | "permission_error" => {
            ProviderErrorKind::Authentication
        }
        "rate_limit_error" | "rate_limit_exceeded" => ProviderErrorKind::RateLimited,
        "conflict_error"
        | "invalid_request_error"
        | "model_not_found"
        | "not_found_error"
        | "request_too_large" => ProviderErrorKind::InvalidRequest,
        "api_error" | "overloaded_error" | "service_unavailable" | "timeout_error" => {
            ProviderErrorKind::Unavailable
        }
        _ => ProviderErrorKind::Response,
    }
}

fn add_output_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("Anthropic-compatible output size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "Anthropic-compatible output exceeded the configured size limit".to_owned(),
        ));
    }
    Ok(())
}

fn add_wire_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("Anthropic-compatible wire size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "Anthropic-compatible stream exceeded the configured wire size limit".to_owned(),
        ));
    }
    Ok(())
}

fn transport_error(error: reqwest::Error, redactions: &[String]) -> ProviderError {
    ProviderError::Transport(sanitize_message(
        &error.without_url().to_string(),
        redactions,
    ))
}

async fn api_error(response: reqwest::Response, redactions: &[String]) -> ProviderError {
    let status = response.status();
    let fallback = status
        .canonical_reason()
        .unwrap_or("Anthropic-compatible request failed")
        .to_owned();
    let body = read_error_body(response).await;
    let body_text = String::from_utf8_lossy(&body);
    let message = serde_json::from_slice::<ApiErrorEnvelope>(&body)
        .ok()
        .and_then(|envelope| envelope.error.message)
        .or_else(|| (!body_text.trim().is_empty()).then(|| body_text.into_owned()))
        .map_or(fallback, |message| sanitize_message(&message, redactions));

    ProviderError::Api {
        status: status.as_u16(),
        message,
    }
}

async fn read_error_body(response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();

    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_BYTES_LIMIT.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() == ERROR_BODY_BYTES_LIMIT {
            break;
        }
    }

    body
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use serde_json::json;

    use super::*;

    #[test]
    fn default_constructor_uses_anthropic_endpoint_and_redacts_auth_debug() {
        let provider = AnthropicMessages::new("anthropic-test-secret").unwrap();
        let auth_values = [
            AnthropicAuth::XApiKey("anthropic-test-secret".to_owned()),
            AnthropicAuth::Bearer("anthropic-test-secret".to_owned()),
            AnthropicAuth::Header(
                "x-custom-auth".to_owned(),
                "anthropic-test-secret".to_owned(),
            ),
        ];

        assert_eq!(provider.endpoint.as_str(), MESSAGES_ENDPOINT);
        for auth in auth_values {
            assert!(!format!("{auth:?}").contains("anthropic-test-secret"));
        }
    }

    #[test]
    fn validates_http_endpoint_policy_and_url_components() {
        for (endpoint, allow_http) in [
            ("not a URL", false),
            ("http://example.com/v1/messages", true),
            ("http://127.0.0.1/v1/messages", false),
            ("https://user:password@example.com/v1/messages", false),
            ("https://example.com/v1/messages#fragment", false),
            ("ftp://example.com/v1/messages", false),
        ] {
            let error =
                AnthropicMessages::with_endpoint(endpoint, AnthropicAuth::NoAuth, [], allow_http)
                    .err()
                    .expect("endpoint must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        AnthropicMessages::with_endpoint(
            "http://[::1]/v1/messages",
            AnthropicAuth::NoAuth,
            [],
            true,
        )
        .expect("IPv6 loopback HTTP should be accepted");
        AnthropicMessages::with_endpoint(
            "http://localhost/v1/messages",
            AnthropicAuth::NoAuth,
            [],
            true,
        )
        .expect("localhost HTTP should be accepted");
    }

    #[test]
    fn rejects_controlled_duplicate_and_invalid_headers() {
        for name in [
            "authorization",
            "x-api-key",
            "anthropic-version",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "accept",
            "content-type",
            "user-agent",
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                AnthropicAuth::NoAuth,
                [(name.to_owned(), "value".to_owned())],
                false,
            )
            .err()
            .expect("controlled header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        for (auth, headers) in [
            (
                AnthropicAuth::Header("x-custom-auth".to_owned(), "secret".to_owned()),
                vec![("x-custom-auth".to_owned(), "override".to_owned())],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![
                    ("x-test".to_owned(), "one".to_owned()),
                    ("X-Test".to_owned(), "two".to_owned()),
                ],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![("bad header".to_owned(), "value".to_owned())],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![("x-test".to_owned(), "bad\r\nvalue".to_owned())],
            ),
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                auth,
                headers,
                false,
            )
            .err()
            .expect("invalid headers must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        for auth in [
            AnthropicAuth::XApiKey(String::new()),
            AnthropicAuth::Bearer(String::new()),
            AnthropicAuth::Header("anthropic-version".to_owned(), "secret".to_owned()),
            AnthropicAuth::Header("x-auth".to_owned(), String::new()),
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                auth,
                [],
                false,
            )
            .err()
            .expect("invalid authentication must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn validates_and_applies_an_explicit_anthropic_version() {
        let (headers, redactions) =
            build_headers(AnthropicAuth::NoAuth, [], "mantle-version-test-secret").unwrap();
        assert_eq!(
            headers.get(ANTHROPIC_VERSION).unwrap(),
            "mantle-version-test-secret"
        );
        assert!(
            redactions
                .iter()
                .any(|value| value == "mantle-version-test-secret")
        );

        for version in ["", "bad\r\nversion"] {
            let error = AnthropicMessages::with_endpoint_and_version(
                "https://example.com/v1/messages",
                AnthropicAuth::NoAuth,
                [],
                false,
                version,
            )
            .err()
            .expect("invalid version must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn decodes_fragmented_named_sse_with_bom_and_all_line_endings() {
        let source = concat!(
            "\u{feff}: comment\r\n",
            "event: content_block_delta\r",
            "data: {\"type\":\"content_block_delta\",\r\n",
            "data: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\r",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\r\r",
        );
        let mut decoder = SseDecoder::new(1_024);
        let mut events = Vec::new();

        for byte in source.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
        assert_eq!(events[1].name.as_deref(), Some("message_stop"));
        assert!(matches!(
            decode_event(events.remove(0), &[]).unwrap(),
            DecodedEvent::OutputText(text) if text == "hello"
        ));
        assert!(matches!(
            decode_event(events.remove(0), &[]).unwrap(),
            DecodedEvent::Completed
        ));
    }

    #[test]
    fn decodes_cumulative_usage_and_rejects_overflow() {
        let start = decode_data(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":1}}}"#,
        )
        .unwrap();
        let delta = decode_event(
            SseEvent {
                name: Some("message_delta".to_owned()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#.to_owned(),
            },
            &[],
        )
        .unwrap();

        assert_eq!(
            start,
            DecodedEvent::MessageStart(Some(ProviderUsage {
                input_tokens: 12,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 3,
                output_tokens: 1,
            }))
        );
        assert_eq!(
            delta,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: Some(9),
            }
        );

        let error = decode_data(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":18446744073709551616,"output_tokens":1}}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn handles_text_refusal_errors_and_opaque_events() {
        let text = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        )
        .unwrap();
        let refusal = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"request declined"}}}"#,
        )
        .unwrap();
        let thinking = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private reasoning"}}"#,
        )
        .unwrap();
        let redacted = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-secret-data"}}"#,
        )
        .unwrap();
        let overloaded = decode_data(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
        )
        .unwrap_err();
        let rate_limited = decode_data(
            "error",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
        .unwrap_err();

        assert!(matches!(text, DecodedEvent::OutputText(text) if text == "hello"));
        assert!(matches!(
            refusal,
            DecodedEvent::MessageDelta {
                refusal: Some(text),
                output_tokens: None,
            } if text == "request declined"
        ));
        assert_eq!(thinking, DecodedEvent::Ignored);
        assert_eq!(redacted, DecodedEvent::Ignored);
        assert_eq!(overloaded.kind(), ProviderErrorKind::Unavailable);
        assert_eq!(rate_limited.kind(), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn rejects_incomplete_and_unsupported_stop_reasons() {
        let max_tokens = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        )
        .unwrap_err();
        let unknown = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"mystery"}}"#,
        )
        .unwrap_err();
        let tool_use = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        )
        .unwrap();

        assert!(matches!(max_tokens, ProviderError::ResponseIncomplete(_)));
        assert!(matches!(unknown, ProviderError::Protocol(_)));
        assert_eq!(
            tool_use,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: None,
            }
        );
    }

    #[test]
    fn decodes_tool_call_stream_events() {
        let started = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        )
        .unwrap();
        let arguments = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        )
        .unwrap();
        let stopped = decode_data(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        )
        .unwrap();

        assert_eq!(
            started,
            DecodedEvent::ToolCallStarted {
                index: 1,
                id: "toolu_1".to_owned(),
                name: "read_file".to_owned(),
            }
        );
        assert_eq!(
            arguments,
            DecodedEvent::ToolCallArguments {
                index: 1,
                json: "{\"path\":".to_owned(),
            }
        );
        assert_eq!(stopped, DecodedEvent::BlockStopped { index: 1 });
    }

    #[test]
    fn rejects_mismatched_event_names_and_enforces_all_size_limits() {
        let mismatch = decode_data("message_stop", r#"{"type":"ping"}"#).unwrap_err();
        let event_error = SseDecoder::new(8)
            .push(b"data: this event keeps going")
            .unwrap_err();
        let output_error = add_output_bytes(&mut 3, 2, 4).unwrap_err();
        let wire_error = add_wire_bytes(&mut 7, 2, 8).unwrap_err();

        assert!(matches!(mismatch, ProviderError::Protocol(_)));
        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
        assert!(matches!(wire_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn sends_exact_request_and_streams_fragmented_text_to_completion() {
        let chunks = vec![
            b"\xef".to_vec(),
            b"\xbb".to_vec(),
            b"\xbf: heartbeat\r".to_vec(),
            b"\nevent: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":4,\"output_tokens\":1}}}\r\n\r"
                .to_vec(),
            b"\nevent: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec(),
            b"event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"h\xc3"
                .to_vec(),
            b"\xa9l\"}}\r\n\r\n".to_vec(),
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"ignored\"}}\n\n"
                .to_vec(),
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n"
                .to_vec(),
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n"
                .to_vec(),
            b"event: message_stop\ndata: {\"type\":\"message_".to_vec(),
            b"stop\"}\r\r".to_vec(),
        ];
        let path = "/custom/messages?api-version=42";
        let (endpoint, server) =
            serve_once(path, "200 OK", "text/event-stream; charset=utf-8", chunks);
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::XApiKey("custom-test-secret".to_owned()),
            [("x-client".to_owned(), "qq-tests".to_owned())],
            true,
        )
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "claude-test",
                vec![Message::user("ping"), Message::assistant("pong")],
                321,
            ))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "hél"
        ));
        assert!(matches!(
            &events[1],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "lo"
        ));
        assert_eq!(
            events[2].as_ref().unwrap(),
            &ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 3,
                    output_tokens: 9,
                }),
            }
        );

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            head.lines().next(),
            Some("POST /custom/messages?api-version=42 HTTP/1.1")
        );
        assert_eq!(request_header(head, "accept"), Some("text/event-stream"));
        assert_eq!(
            request_header(head, "content-type"),
            Some("application/json")
        );
        assert_eq!(
            request_header(head, "x-api-key"),
            Some("custom-test-secret")
        );
        assert_eq!(
            request_header(head, "anthropic-version"),
            Some(DEFAULT_ANTHROPIC_VERSION)
        );
        assert_eq!(request_header(head, "x-client"), Some("qq-tests"));
        assert_eq!(request_header(head, "authorization"), None);
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({
                "model": "claude-test",
                "messages": [
                    {"role": "user", "content": "ping"},
                    {"role": "assistant", "content": "pong"}
                ],
                "max_tokens": 321,
                "stream": true
            })
        );
        assert!(!body.contains("system"));
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_blocks() {
        let body = concat!(
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let request = ModelRequest::new(
            "claude-test",
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
                Message::tool_results(vec![ContentBlock::ToolResult {
                    call_id: "toolu_1".to_owned(),
                    content: "(config)".to_owned(),
                    is_error: false,
                }]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({
                "model": "claude-test",
                "messages": [
                    {"role": "user", "content": "read the config"},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Reading it now."},
                        {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                         "input": {"path": "config.ron"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1",
                         "content": "(config)", "is_error": false}
                    ]}
                ],
                "tools": [
                    {"name": "read_file", "description": "Reads one file",
                     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}
                ],
                "max_tokens": 128,
                "stream": true
            })
        );
    }

    #[tokio::test]
    async fn streams_tool_calls_with_attributed_arguments_to_completion() {
        let body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking.\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let events = provider
            .stream(test_request())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "Checking.".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "toolu_1".to_owned(),
                    json: "{\"path\":".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "toolu_1".to_owned(),
                    json: "\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "toolu_1".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_tool_arguments_for_an_unknown_call() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn returns_typed_stream_errors_without_exposing_secrets() {
        let body = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
            "\"message\":\"stream-auth-secret static-test-secret overloaded\"}}\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::Bearer("stream-auth-secret".to_owned()),
            [(
                "x-client-secret".to_owned(),
                "static-test-secret".to_owned(),
            )],
            true,
        )
        .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::ResponseFailed { .. }));
        assert_eq!(error.kind(), ProviderErrorKind::Unavailable);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("stream-auth-secret"));
        assert!(!rendered.contains("static-test-secret"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn returns_typed_401_without_exposing_response_body_secrets() {
        let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid test-api-secret\nstatic-test-secret credential"},"request_id":"req_test"}"#;
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "401 Unauthorized",
            "application/json",
            vec![body.to_vec()],
        );
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::XApiKey("test-api-secret".to_owned()),
            [(
                "x-client-secret".to_owned(),
                "static-test-secret".to_owned(),
            )],
            true,
        )
        .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert!(matches!(error, ProviderError::Api { status: 401, .. }));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("test-api-secret"));
        assert!(!rendered.contains("static-test-secret"));
        assert!(!rendered.contains('\n'));

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let head = request.split_once("\r\n\r\n").unwrap().0;
        assert_eq!(request_header(head, "x-api-key"), Some("test-api-secret"));
    }

    #[tokio::test]
    async fn rejects_non_sse_success_responses() {
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "application/json",
            vec![b"{}".to_vec()],
        );
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reports_a_stream_that_ends_before_message_stop() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/messages",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let events = provider.stream(test_request()).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "partial"
        ));
        assert!(matches!(&events[1], Err(ProviderError::Protocol(_))));
        server.join().unwrap();
    }

    fn decode_data(name: &str, data: &str) -> Result<DecodedEvent, ProviderError> {
        decode_event(
            SseEvent {
                name: Some(name.to_owned()),
                data: data.to_owned(),
            },
            &[],
        )
    }

    fn test_request() -> ModelRequest {
        ModelRequest::new("claude-test", vec![Message::user("ping")], 128)
    }

    fn serve_once(
        path: &str,
        status: &str,
        content_type: &str,
        chunks: Vec<Vec<u8>>,
    ) -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let content_length = chunks.iter().map(Vec::len).sum::<usize>();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            request
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
