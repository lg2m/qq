//! Discovery and authenticated streaming client for the local QQ server.

#![allow(
    dead_code,
    reason = "root lifecycle composition is intentionally outside this adapter-only change"
)]

use std::{pin::Pin, time::Duration};

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{AskRequest, PROTOCOL_VERSION, RunEvent};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use thiserror::Error;

use crate::server::{
    HealthProbeError, MAX_EVENT_BYTES, MAX_REQUEST_BYTES, ServerConnection, ServerError,
    ServerPaths, probe_client, probe_health, read_connection, read_response_bounded,
    validate_ask_request,
};

const DISCOVERY_RETRIES: usize = 3;
const DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(50);
const ASK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_SSE_WIRE_EVENT_BYTES: usize = MAX_EVENT_BYTES + 16 * 1024;
const MAX_SSE_LINE_BYTES: usize = MAX_SSE_WIRE_EVENT_BYTES;

/// Authenticated coordinates discovered from private local metadata.
pub type Connection = ServerConnection;

/// Owned event stream returned by [`ask`].
pub type RunEventStream =
    Pin<Box<dyn Stream<Item = Result<RunEvent, ClientError>> + Send + 'static>>;

/// Discovers and probes the current user's running QQ server.
pub async fn discover() -> Result<Option<Connection>, ClientError> {
    let paths = ServerPaths::for_user().map_err(map_metadata_error)?;
    discover_with_paths(&paths).await
}

/// Discovers using injected paths, primarily for embedding and tests.
pub async fn discover_with_paths(paths: &ServerPaths) -> Result<Option<Connection>, ClientError> {
    let client = probe_client().map_err(|()| ClientError::Unavailable)?;

    for attempt in 0..DISCOVERY_RETRIES {
        let connection = match read_connection(paths) {
            Ok(Some(connection)) => connection,
            Ok(None) => return Ok(None),
            Err(ServerError::StateIo { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(map_metadata_error(error)),
        };

        match probe_health(&client, &connection).await {
            Ok(info) if info == *connection.server_info() => return Ok(Some(connection)),
            Ok(_) | Err(HealthProbeError::Unavailable) => {}
            Err(HealthProbeError::ProtocolMismatch { found }) => {
                return Err(ClientError::ProtocolMismatch {
                    expected: PROTOCOL_VERSION,
                    found,
                });
            }
        }
        if attempt + 1 < DISCOVERY_RETRIES {
            tokio::time::sleep(DISCOVERY_RETRY_DELAY).await;
        }
    }

    Ok(None)
}

/// Sends one request and returns an owned, incrementally decoded SSE stream.
pub async fn ask(
    connection: &Connection,
    request: AskRequest,
) -> Result<RunEventStream, ClientError> {
    validate_ask_request(&request).map_err(ClientError::InvalidRequest)?;
    let body = serde_json::to_vec(&request).map_err(|_| ClientError::InvalidRequestEncoding)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ClientError::RequestTooLarge);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(ASK_CONNECT_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ClientError::Unavailable)?;
    let response = connection
        .authorize(client.post(connection.endpoint("/v1/ask")))
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "text/event-stream")
        .body(body)
        .send()
        .await
        .map_err(|_| ClientError::Unavailable)?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ERROR_BODY_BYTES as u64)
        {
            return Err(ClientError::ErrorResponseTooLarge);
        }
        tokio::time::timeout(
            ERROR_RESPONSE_TIMEOUT,
            read_response_bounded(response, MAX_ERROR_BODY_BYTES),
        )
        .await
        .map_err(|_| ClientError::ErrorResponseUnavailable)?
        .map_err(|()| ClientError::ErrorResponseTooLarge)?;
        return Err(ClientError::ServerResponse { status });
    }
    if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
        return Err(ClientError::UnexpectedContentType);
    }

    let output = stream! {
        let mut chunks = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut terminal = false;

        while let Some(chunk) = chunks.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    yield Err(ClientError::StreamTransport);
                    return;
                }
            };
            for byte in chunk {
                match decoder.feed_byte(byte) {
                    Ok(Some(event)) => {
                        if terminal {
                            yield Err(ClientError::EventAfterTerminal);
                            return;
                        }
                        terminal = is_terminal(&event);
                        yield Ok(event);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
        }

        match decoder.finish() {
            Ok(Some(event)) => {
                if terminal {
                    yield Err(ClientError::EventAfterTerminal);
                    return;
                }
                terminal = is_terminal(&event);
                yield Ok(event);
            }
            Ok(None) => {}
            Err(error) => {
                yield Err(error);
                return;
            }
        }
        if !terminal {
            yield Err(ClientError::MissingTerminalEvent);
        }
    };
    Ok(Box::pin(output))
}

fn is_event_stream(value: Option<&reqwest::header::HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

const fn is_terminal(event: &RunEvent) -> bool {
    matches!(event, RunEvent::Completed | RunEvent::Failed { .. })
}

struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    first_line: bool,
    skip_lf: bool,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            event_bytes: 0,
            first_line: true,
            skip_lf: false,
        }
    }
}

impl SseDecoder {
    fn feed_byte(&mut self, byte: u8) -> Result<Option<RunEvent>, ClientError> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }

        match byte {
            b'\r' => {
                self.skip_lf = true;
                self.finish_line()
            }
            b'\n' => self.finish_line(),
            byte => {
                self.event_bytes = self
                    .event_bytes
                    .checked_add(1)
                    .ok_or(ClientError::EventTooLarge)?;
                if self.event_bytes > MAX_SSE_WIRE_EVENT_BYTES {
                    return Err(ClientError::EventTooLarge);
                }
                if self.line.len() >= MAX_SSE_LINE_BYTES {
                    return Err(ClientError::EventTooLarge);
                }
                self.line.push(byte);
                Ok(None)
            }
        }
    }

    fn finish(mut self) -> Result<Option<RunEvent>, ClientError> {
        let line_event = if self.line.is_empty() {
            None
        } else {
            self.finish_line()?
        };
        if line_event.is_some() {
            return Ok(line_event);
        }
        self.dispatch_event()
    }

    fn finish_line(&mut self) -> Result<Option<RunEvent>, ClientError> {
        if self.line.is_empty() {
            self.first_line = false;
            self.event_bytes = 0;
            return self.dispatch_event();
        }

        let line = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line).map_err(|_| ClientError::MalformedSse)?;
        let line = if self.first_line {
            self.first_line = false;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            self.event_bytes = 0;
            return self.dispatch_event();
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        if field == "data" {
            if self.data.len().saturating_add(value.len()) > MAX_EVENT_BYTES {
                return Err(ClientError::EventTooLarge);
            }
            self.data.extend_from_slice(value.as_bytes());
            self.data.push(b'\n');
        }
        Ok(None)
    }

    fn dispatch_event(&mut self) -> Result<Option<RunEvent>, ClientError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        self.data.pop();
        let data = std::mem::take(&mut self.data);
        serde_json::from_slice(&data)
            .map(Some)
            .map_err(|_| ClientError::MalformedEvent)
    }
}

fn map_metadata_error(error: ServerError) -> ClientError {
    match error {
        ServerError::StateDirectoryUnavailable => ClientError::StateDirectoryUnavailable,
        ServerError::InsecureStatePath(_) | ServerError::InsecurePermissions { .. } => {
            ClientError::InsecureMetadata
        }
        ServerError::MetadataVersionMismatch { expected, found } => {
            ClientError::MetadataVersionMismatch { expected, found }
        }
        ServerError::ProtocolMismatch { expected, found } => {
            ClientError::ProtocolMismatch { expected, found }
        }
        ServerError::MetadataCorrupt | ServerError::MetadataTooLarge => {
            ClientError::CorruptMetadata
        }
        ServerError::StateRace | ServerError::StateIo { .. } => ClientError::MetadataUnavailable,
        ServerError::NonLoopbackBind(_)
        | ServerError::RandomnessUnavailable
        | ServerError::Bind { .. }
        | ServerError::ExistingServerUnavailable
        | ServerError::Serve { .. }
        | ServerError::ServerTaskStopped => ClientError::MetadataUnavailable,
    }
}

/// Sanitized local discovery, HTTP, and SSE failures.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClientError {
    #[error("could not determine the user-scoped server state directory")]
    StateDirectoryUnavailable,
    #[error("local server metadata is not private")]
    InsecureMetadata,
    #[error("local server metadata is corrupt")]
    CorruptMetadata,
    #[error("local server metadata is unavailable")]
    MetadataUnavailable,
    #[error("server metadata version {found} is unsupported (expected {expected})")]
    MetadataVersionMismatch { expected: u16, found: u16 },
    #[error("server protocol version {found} does not match client version {expected}")]
    ProtocolMismatch { expected: u16, found: u16 },
    #[error("local server is unavailable")]
    Unavailable,
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("request cannot be encoded")]
    InvalidRequestEncoding,
    #[error("request exceeds the wire size limit")]
    RequestTooLarge,
    #[error("local server returned HTTP status {status}")]
    ServerResponse { status: u16 },
    #[error("local server error response exceeds the size limit")]
    ErrorResponseTooLarge,
    #[error("local server error response did not finish in time")]
    ErrorResponseUnavailable,
    #[error("local server returned an unexpected content type")]
    UnexpectedContentType,
    #[error("local server stream failed")]
    StreamTransport,
    #[error("local server returned malformed SSE")]
    MalformedSse,
    #[error("local server returned a malformed run event")]
    MalformedEvent,
    #[error("local server event exceeds the wire size limit")]
    EventTooLarge,
    #[error("local server stream ended without a terminal event")]
    MissingTerminalEvent,
    #[error("local server sent data after a terminal event")]
    EventAfterTerminal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_fragments(fragments: &[&[u8]]) -> Result<Vec<RunEvent>, ClientError> {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for fragment in fragments {
            for byte in *fragment {
                if let Some(event) = decoder.feed_byte(*byte)? {
                    events.push(event);
                }
            }
        }
        if let Some(event) = decoder.finish()? {
            events.push(event);
        }
        Ok(events)
    }

    #[test]
    fn decodes_fragmented_crlf_and_multiline_sse() {
        let events = decode_fragments(&[
            b"\xef",
            b"\xbb\xbf: hea",
            b"rtbeat\r",
            b"\ndata: {\"type\":\r\n",
            b"data: \"started\"}\r",
            b"\n\r\ndata: {\"type\":\"completed\"}\n\n",
        ])
        .unwrap();

        assert_eq!(events, vec![RunEvent::Started, RunEvent::Completed]);
    }

    #[test]
    fn accepts_a_final_event_without_a_blank_line() {
        let events = decode_fragments(&[b"data: {\"type\":\"completed\"}"]).unwrap();

        assert_eq!(events, vec![RunEvent::Completed]);
    }

    #[test]
    fn rejects_malformed_json_without_echoing_it() {
        let error = decode_fragments(&[b"data: definitely-secret\n\n"]).unwrap_err();

        assert_eq!(error, ClientError::MalformedEvent);
        assert!(!error.to_string().contains("definitely-secret"));
    }

    #[test]
    fn bounds_sse_lines_and_events() {
        let mut decoder = SseDecoder::default();
        for _ in 0..MAX_SSE_LINE_BYTES {
            decoder.feed_byte(b'x').unwrap();
        }

        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );

        let mut decoder = SseDecoder {
            event_bytes: MAX_SSE_WIRE_EVENT_BYTES,
            ..SseDecoder::default()
        };
        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );
    }
}
