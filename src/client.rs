//! Discovery and authenticated streaming client for the local QQ server.

#![allow(
    dead_code,
    reason = "root lifecycle composition is intentionally outside this adapter-only change"
)]

use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{
    AskRequest, CommandId, CommandReceipt, CommandRequest, EventCursor, ModelSelection,
    PROTOCOL_VERSION, RunEvent, SessionCommand, SessionEventEnvelope, SnapshotRequest, WorkspaceId,
    WorkspaceSnapshot,
};
use qq_tui::{
    ClientFailure as TuiClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};

use crate::server::{
    HealthProbeError, MAX_EVENT_BYTES, MAX_REQUEST_BYTES, ServerConnection, ServerError,
    ServerPaths, probe_client, probe_health, read_connection, read_response_bounded,
    validate_ask_request,
};

const DISCOVERY_RETRIES: usize = 3;
const DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(50);
const ASK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const ERROR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_SSE_WIRE_EVENT_BYTES: usize = MAX_EVENT_BYTES + 16 * 1024;
const MAX_SSE_LINE_BYTES: usize = MAX_SSE_WIRE_EVENT_BYTES;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const TUI_REQUEST_CAPACITY: usize = 64;
const TUI_UPDATE_CAPACITY: usize = 256;
const TUI_CONCURRENT_REQUESTS: usize = 8;

/// Authenticated coordinates discovered from private local metadata.
pub type Connection = ServerConnection;

/// Owned event stream returned by [`ask`].
pub type RunEventStream =
    Pin<Box<dyn Stream<Item = Result<RunEvent, ClientError>> + Send + 'static>>;
pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, ClientError>> + Send + 'static>>;

#[derive(Clone)]
pub struct SessionClient {
    connection: Connection,
    http: reqwest::Client,
}

pub struct TuiClient {
    requests: mpsc::Sender<ClientRequest>,
    updates: mpsc::Receiver<ClientUpdate>,
}

impl TuiClient {
    pub fn start(
        connection: Connection,
        workspace: PathBuf,
        model: ModelSelection,
    ) -> Result<Self, ClientError> {
        let client = SessionClient::new(connection)?;
        let (request_tx, request_rx) = mpsc::channel(TUI_REQUEST_CAPACITY);
        let (update_tx, update_rx) = mpsc::channel(TUI_UPDATE_CAPACITY);
        tokio::spawn(run_tui_client(
            client, workspace, model, request_rx, update_tx,
        ));
        Ok(Self {
            requests: request_tx,
            updates: update_rx,
        })
    }
}

impl ClientPort for TuiClient {
    fn try_send(&self, request: ClientRequest) -> Result<(), TuiClientFailure> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    TuiClientFailure::new("client request queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => TuiClientFailure::new("client stopped"),
            })
    }

    async fn recv(&mut self) -> Option<ClientUpdate> {
        self.updates.recv().await
    }
}

async fn run_tui_client(
    mut client: SessionClient,
    workspace: PathBuf,
    model: ModelSelection,
    mut requests: mpsc::Receiver<ClientRequest>,
    updates: mpsc::Sender<ClientUpdate>,
) {
    if updates
        .send(ClientUpdate::Connection(ConnectionState::Connecting))
        .await
        .is_err()
    {
        return;
    }
    let (mut workspace_id, snapshot) = match bootstrap_tui(&client, &workspace, &model).await {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            send_bootstrap_failure(&updates, error).await;
            return;
        }
    };
    let mut cursor = snapshot.cursor;
    if updates
        .send(ClientUpdate::Snapshot(snapshot))
        .await
        .is_err()
    {
        return;
    }

    let request_permits = Arc::new(Semaphore::new(TUI_CONCURRENT_REQUESTS));
    let mut reconnect_delay = Duration::from_millis(50);
    loop {
        if updates
            .send(ClientUpdate::Connection(ConnectionState::Replaying))
            .await
            .is_err()
        {
            return;
        }
        let mut events = match client.events(workspace_id, cursor).await {
            Ok(events) => events,
            Err(error) => {
                if let Some((recovered_client, recovered_workspace, snapshot)) =
                    recover_tui_client(&client, &workspace, &model, &error).await
                {
                    client = recovered_client;
                    workspace_id = recovered_workspace;
                    cursor = snapshot.cursor;
                    reconnect_delay = Duration::from_millis(50);
                    if updates
                        .send(ClientUpdate::ResetSnapshot(snapshot))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if updates
                    .send(ClientUpdate::Connection(ConnectionState::Offline))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        if updates
            .send(ClientUpdate::Connection(ConnectionState::Live))
            .await
            .is_err()
        {
            return;
        }
        let mut reset_error = None;
        loop {
            tokio::select! {
                biased;
                request = requests.recv() => {
                    let Some(request) = request else { return; };
                    dispatch_tui_request(
                        client.clone(),
                        request,
                        Arc::clone(&request_permits),
                        updates.clone(),
                    );
                }
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        reconnect_delay = Duration::from_millis(50);
                        cursor = event.cursor;
                        if updates.send(ClientUpdate::Event(event)).await.is_err() {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        if matches!(error, ClientError::InvalidCursor | ClientError::EventTooLarge) {
                            reset_error = Some(error);
                            break;
                        }
                        if updates
                            .send(ClientUpdate::Connection(ConnectionState::Offline))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                        break;
                    },
                    None => {
                        if updates
                            .send(ClientUpdate::Connection(ConnectionState::Offline))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
                        break;
                    },
                }
            }
        }
        if let Some(error) = reset_error {
            if let Some((recovered_client, recovered_workspace, snapshot)) =
                recover_tui_client(&client, &workspace, &model, &error).await
            {
                client = recovered_client;
                workspace_id = recovered_workspace;
                cursor = snapshot.cursor;
                reconnect_delay = Duration::from_millis(50);
                if updates
                    .send(ClientUpdate::ResetSnapshot(snapshot))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            if updates
                .send(ClientUpdate::Connection(ConnectionState::Offline))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(Duration::from_secs(2));
        }
    }
}

async fn bootstrap_tui(
    client: &SessionClient,
    workspace: &Path,
    model: &ModelSelection,
) -> Result<(WorkspaceId, WorkspaceSnapshot), ClientError> {
    let (workspace_id, _) = client.resolve_workspace(workspace).await?;
    let snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: None,
            session_limit: 512,
            message_limit: 256,
        })
        .await?;
    let focused = if let Some(session) = snapshot.sessions.first() {
        session.id
    } else {
        let receipt = client
            .command(
                CommandId::generate().map_err(|_| ClientError::Unavailable)?,
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: model.clone(),
                },
            )
            .await?;
        let qq_protocol::CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            return Err(ClientError::MalformedEvent);
        };
        session_id
    };
    let snapshot = client
        .snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(focused),
            session_limit: 512,
            message_limit: 256,
        })
        .await?;
    Ok((workspace_id, snapshot))
}

async fn recover_tui_client(
    current: &SessionClient,
    workspace: &Path,
    model: &ModelSelection,
    error: &ClientError,
) -> Option<(SessionClient, WorkspaceId, WorkspaceSnapshot)> {
    if matches!(
        error,
        ClientError::InvalidCursor
            | ClientError::EventTooLarge
            | ClientError::ServerResponse { status: 400 }
    ) && let Ok((workspace_id, snapshot)) = bootstrap_tui(current, workspace, model).await
    {
        return Some((current.clone(), workspace_id, snapshot));
    }
    if !matches!(
        error,
        ClientError::Unavailable | ClientError::ServerResponse { status: 401 }
    ) {
        return None;
    }
    let connection = discover().await.ok().flatten()?;
    let client = SessionClient::new(connection).ok()?;
    let (workspace_id, snapshot) = bootstrap_tui(&client, workspace, model).await.ok()?;
    Some((client, workspace_id, snapshot))
}

fn dispatch_tui_request(
    client: SessionClient,
    request: ClientRequest,
    permits: Arc<Semaphore>,
    updates: mpsc::Sender<ClientUpdate>,
) {
    let Ok(permit) = permits.try_acquire_owned() else {
        let update = match request {
            ClientRequest::Command(command) => ClientUpdate::CommandResult {
                command_id: command.command_id,
                result: Err(TuiClientFailure::new("too many client requests are active")),
            },
            ClientRequest::Snapshot(_) => ClientUpdate::SnapshotFailed(TuiClientFailure::new(
                "too many client requests are active",
            )),
        };
        let _ = updates.try_send(update);
        return;
    };
    tokio::spawn(async move {
        let (update, created) = match request {
            ClientRequest::Command(command) => {
                let result = client
                    .command(command.command_id, command.command)
                    .await
                    .map_err(|error| TuiClientFailure::new(error.to_string()));
                let created = result.as_ref().ok().and_then(|receipt| {
                    if let qq_protocol::CommandOutcome::SessionCreated { session_id } =
                        &receipt.outcome
                    {
                        Some((receipt.committed_through.workspace_id, *session_id))
                    } else {
                        None
                    }
                });
                (
                    ClientUpdate::CommandResult {
                        command_id: command.command_id,
                        result,
                    },
                    created,
                )
            }
            ClientRequest::Snapshot(request) => match client.snapshot(request).await {
                Ok(snapshot) => (ClientUpdate::Snapshot(snapshot), None),
                Err(error) => (
                    ClientUpdate::SnapshotFailed(TuiClientFailure::new(error.to_string())),
                    None,
                ),
            },
        };
        let _permit = permit;
        if updates.send(update).await.is_err() {
            return;
        }
        if let Some((workspace_id, session_id)) = created {
            let update = match client
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(session_id),
                    session_limit: 512,
                    message_limit: 256,
                })
                .await
            {
                Ok(snapshot) => ClientUpdate::Snapshot(snapshot),
                Err(error) => {
                    ClientUpdate::SnapshotFailed(TuiClientFailure::new(error.to_string()))
                }
            };
            let _ = updates.send(update).await;
        }
    });
}

async fn send_bootstrap_failure(updates: &mpsc::Sender<ClientUpdate>, error: ClientError) {
    let _ = updates
        .send(ClientUpdate::SnapshotFailed(TuiClientFailure::new(
            error.to_string(),
        )))
        .await;
    let _ = updates
        .send(ClientUpdate::Connection(ConnectionState::Offline))
        .await;
}

impl SessionClient {
    pub fn new(connection: Connection) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .connect_timeout(ASK_CONNECT_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClientError::Unavailable)?;
        Ok(Self { connection, http })
    }

    pub async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, ClientError> {
        let path = match command {
            SessionCommand::ResolveWorkspace { .. } => "/v1/workspaces/resolve",
            SessionCommand::CreateSession { .. } => "/v1/sessions",
            SessionCommand::SubmitPrompt { .. } => "/v1/sessions/prompts",
            SessionCommand::CancelRun { .. } => "/v1/runs/cancel",
        };
        self.post_json(
            path,
            &CommandRequest {
                command_id,
                command,
            },
            MAX_ERROR_BODY_BYTES,
        )
        .await
    }

    pub async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, ClientError> {
        self.post_json("/v1/workspaces/snapshot", &request, MAX_SNAPSHOT_BYTES)
            .await
    }

    pub async fn events(
        &self,
        workspace_id: WorkspaceId,
        after: EventCursor,
    ) -> Result<SessionEventStream, ClientError> {
        if after.workspace_id != workspace_id {
            return Err(ClientError::InvalidCursor);
        }
        let endpoint = self
            .connection
            .endpoint(&format!("/v1/workspaces/{workspace_id}/events"));
        let response = tokio::time::timeout(
            SSE_HEADER_TIMEOUT,
            self.connection
                .authorize(self.http.get(endpoint))
                .header(ACCEPT, "text/event-stream")
                .header(
                    "last-event-id",
                    HeaderValue::from_str(&after.to_string())
                        .map_err(|_| ClientError::InvalidCursor)?,
                )
                .send(),
        )
        .await
        .map_err(|_| ClientError::Unavailable)?
        .map_err(|_| ClientError::Unavailable)?;
        check_success(response.status().as_u16())?;
        if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
            return Err(ClientError::UnexpectedContentType);
        }

        let output = stream! {
            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::<SessionEventEnvelope>::default();
            let mut sequence = after.sequence;
            loop {
                let chunk = match tokio::time::timeout(SSE_IDLE_TIMEOUT, chunks.next()).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => {
                        yield Err(ClientError::StreamTransport);
                        return;
                    }
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => {
                        yield Err(ClientError::StreamTransport);
                        return;
                    }
                };
                for byte in chunk {
                    match decoder.feed_byte(byte) {
                        Ok(Some(decoded)) => {
                            if !session_event_cursor_is_next(
                                decoded.id.as_deref(),
                                &decoded.event.cursor,
                                workspace_id,
                                after.store_id,
                                sequence,
                            ) {
                                yield Err(ClientError::InvalidCursor);
                                return;
                            }
                            sequence = decoded.event.cursor.sequence;
                            yield Ok(decoded.event);
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
                Ok(Some(decoded)) => {
                    if session_event_cursor_is_next(
                        decoded.id.as_deref(),
                        &decoded.event.cursor,
                        workspace_id,
                        after.store_id,
                        sequence,
                    ) {
                        yield Ok(decoded.event);
                    } else {
                        yield Err(ClientError::InvalidCursor);
                    }
                }
                Ok(None) => {}
                Err(error) => yield Err(error),
            }
        };
        Ok(Box::pin(output))
    }

    pub async fn resolve_workspace(
        &self,
        path: &Path,
    ) -> Result<(WorkspaceId, EventCursor), ClientError> {
        let path = path.to_str().ok_or(ClientError::InvalidWorkspacePath)?;
        let receipt = self
            .command(
                CommandId::generate().map_err(|_| ClientError::Unavailable)?,
                SessionCommand::ResolveWorkspace {
                    path: path.to_owned(),
                },
            )
            .await?;
        let qq_protocol::CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome
        else {
            return Err(ClientError::MalformedEvent);
        };
        Ok((workspace_id, receipt.committed_through))
    }

    async fn post_json<Request, Response>(
        &self,
        path: &str,
        request: &Request,
        response_limit: usize,
    ) -> Result<Response, ClientError>
    where
        Request: serde::Serialize,
        Response: DeserializeOwned,
    {
        let body = serde_json::to_vec(request).map_err(|_| ClientError::InvalidRequestEncoding)?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ClientError::RequestTooLarge);
        }
        let response = self
            .connection
            .authorize(self.http.post(self.connection.endpoint(path)))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|_| ClientError::Unavailable)?;
        check_success(response.status().as_u16())?;
        if response
            .content_length()
            .is_some_and(|length| length > response_limit as u64)
        {
            return Err(ClientError::ResponseTooLarge);
        }
        let bytes = read_response_bounded(response, response_limit)
            .await
            .map_err(|()| ClientError::ResponseTooLarge)?;
        serde_json::from_slice(&bytes).map_err(|_| ClientError::MalformedEvent)
    }
}

fn session_event_cursor_is_next(
    event_id: Option<&str>,
    cursor: &EventCursor,
    workspace_id: WorkspaceId,
    store_id: qq_protocol::StoreId,
    previous_sequence: u64,
) -> bool {
    let expected_id = cursor.to_string();
    event_id == Some(expected_id.as_str())
        && cursor.workspace_id == workspace_id
        && cursor.store_id == store_id
        && previous_sequence.checked_add(1) == Some(cursor.sequence)
}

fn check_success(status: u16) -> Result<(), ClientError> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(ClientError::ServerResponse { status })
    }
}

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
        let mut decoder = SseDecoder::<RunEvent>::default();
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
                    Ok(Some(decoded)) => {
                        let event = decoded.event;
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
            Ok(Some(decoded)) => {
                let event = decoded.event;
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

#[derive(Debug)]
struct DecodedSse<T> {
    id: Option<String>,
    event: T,
}

struct SseDecoder<T> {
    line: Vec<u8>,
    data: Vec<u8>,
    id: Option<String>,
    event_bytes: usize,
    first_line: bool,
    skip_lf: bool,
    marker: PhantomData<T>,
}

impl<T> Default for SseDecoder<T> {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            id: None,
            event_bytes: 0,
            first_line: true,
            skip_lf: false,
            marker: PhantomData,
        }
    }
}

impl<T> SseDecoder<T>
where
    T: DeserializeOwned,
{
    fn feed_byte(&mut self, byte: u8) -> Result<Option<DecodedSse<T>>, ClientError> {
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

    fn finish(mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
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

    fn finish_line(&mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
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
        } else if field == "id" {
            if value.len() > 256 || value.as_bytes().contains(&0) {
                return Err(ClientError::MalformedSse);
            }
            self.id = Some(value.to_owned());
        }
        Ok(None)
    }

    fn dispatch_event(&mut self) -> Result<Option<DecodedSse<T>>, ClientError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        self.data.pop();
        let data = std::mem::take(&mut self.data);
        let event = serde_json::from_slice(&data).map_err(|_| ClientError::MalformedEvent)?;
        Ok(Some(DecodedSse {
            id: self.id.take(),
            event,
        }))
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
    #[error("response exceeds the wire size limit")]
    ResponseTooLarge,
    #[error("workspace path must be valid UTF-8")]
    InvalidWorkspacePath,
    #[error("server returned an invalid event cursor")]
    InvalidCursor,
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
    use qq_protocol::StoreId;

    use super::*;

    fn decode_fragments(fragments: &[&[u8]]) -> Result<Vec<RunEvent>, ClientError> {
        let mut decoder = SseDecoder::<RunEvent>::default();
        let mut events = Vec::new();
        for fragment in fragments {
            for byte in *fragment {
                if let Some(event) = decoder.feed_byte(*byte)? {
                    events.push(event.event);
                }
            }
        }
        if let Some(event) = decoder.finish()? {
            events.push(event.event);
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
        let mut decoder = SseDecoder::<RunEvent>::default();
        for _ in 0..MAX_SSE_LINE_BYTES {
            decoder.feed_byte(b'x').unwrap();
        }

        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );

        let mut decoder = SseDecoder::<RunEvent> {
            event_bytes: MAX_SSE_WIRE_EVENT_BYTES,
            ..SseDecoder::default()
        };
        assert_eq!(
            decoder.feed_byte(b'x').unwrap_err(),
            ClientError::EventTooLarge
        );
    }

    #[test]
    fn rejects_forward_session_event_cursor_gaps() {
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let store_id = StoreId::from_bytes([2; 16]);
        let mut cursor = EventCursor {
            store_id,
            workspace_id,
            sequence: 11,
        };
        let mut event_id = cursor.to_string();

        assert!(session_event_cursor_is_next(
            Some(&event_id),
            &cursor,
            workspace_id,
            store_id,
            10,
        ));

        cursor.sequence = 12;
        event_id = cursor.to_string();
        assert!(!session_event_cursor_is_next(
            Some(&event_id),
            &cursor,
            workspace_id,
            store_id,
            10,
        ));
    }
}
