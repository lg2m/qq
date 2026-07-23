//! Authenticated, single-instance HTTP and SSE server adapter.

#![allow(
    dead_code,
    reason = "root lifecycle composition is intentionally outside this adapter-only change"
)]

use std::{
    convert::Infallible,
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    future::Future,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_stream::stream;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response, sse::Event, sse::KeepAlive, sse::Sse},
    routing::{get, post},
};
use directories::ProjectDirs;
use futures_util::StreamExt;
use qq_core::{RunStream, SessionEventStream};
use qq_protocol::{
    AskRequest, CommandReceipt, CommandRequest, PROTOCOL_VERSION, ServerInfo, SessionCommand,
    SnapshotRequest, SubscribeRequest, WorkspaceId, WorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

const METADATA_FORMAT_VERSION: u16 = 1;
const METADATA_FILE_NAME: &str = "server.ron";
const LOCK_FILE_NAME: &str = "server.lock";
const DEFAULT_BIND_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const MAX_METADATA_BYTES: usize = 16 * 1024;
pub(crate) const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_PROMPT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_WORKSPACE_BYTES: usize = 4096;
pub(crate) const MAX_MODEL_BYTES: usize = 512;
pub(crate) const MAX_ORGANIZATION_BYTES: usize = 512;
pub(crate) const SESSION_ID_HEX_BYTES: usize = 32;
pub(crate) const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_HEALTH_BYTES: usize = 16 * 1024;
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const STARTUP_RETRIES: usize = 8;
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(25);
const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;
const MAX_CONCURRENT_SESSION_REQUESTS: usize = 64;
const MAX_CONCURRENT_SUBSCRIPTIONS: usize = 64;

/// Future returned by [`AskHandler`].
pub type AskFuture =
    Pin<Box<dyn Future<Output = Result<RunStream, AskHandlerError>> + Send + 'static>>;

pub type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandReceipt, AskHandlerError>> + Send + 'static>>;
pub type SnapshotFuture =
    Pin<Box<dyn Future<Output = Result<WorkspaceSnapshot, AskHandlerError>> + Send + 'static>>;

/// Root-supplied application seam for handling one request.
pub trait AskHandler: Send + Sync + 'static {
    fn ask(&self, _request: AskRequest) -> AskFuture {
        Box::pin(async { Err(AskHandlerError::Unavailable) })
    }

    fn command(&self, _request: CommandRequest) -> CommandFuture {
        Box::pin(async { Err(AskHandlerError::Unavailable) })
    }

    fn snapshot(&self, _request: SnapshotRequest) -> SnapshotFuture {
        Box::pin(async { Err(AskHandlerError::Unavailable) })
    }

    fn subscribe(&self, _request: SubscribeRequest) -> Result<SessionEventStream, AskHandlerError> {
        Err(AskHandlerError::Unavailable)
    }
}

impl<F, Fut> AskHandler for F
where
    F: Fn(AskRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RunStream, AskHandlerError>> + Send + 'static,
{
    fn ask(&self, request: AskRequest) -> AskFuture {
        Box::pin(self(request))
    }
}

/// Sanitized failures a root handler may return before streaming starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AskHandlerError {
    #[error("request was rejected")]
    InvalidRequest,
    #[error("request service is unavailable")]
    Unavailable,
    #[error("request failed")]
    Internal,
}

/// Stable filesystem locations used for instance coordination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPaths {
    directory: PathBuf,
    lock_file: PathBuf,
    metadata_file: PathBuf,
}

impl ServerPaths {
    /// Uses `directory` as an injectable private state directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        Self {
            lock_file: directory.join(LOCK_FILE_NAME),
            metadata_file: directory.join(METADATA_FILE_NAME),
            directory,
        }
    }

    /// Resolves the current user's runtime directory, with a data-local fallback.
    pub fn for_user() -> Result<Self, ServerError> {
        let project =
            ProjectDirs::from("dev", "qq", "qq").ok_or(ServerError::StateDirectoryUnavailable)?;
        let directory = project
            .runtime_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.data_local_dir().join("runtime"));
        Ok(Self::new(directory))
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.lock_file
    }

    #[must_use]
    pub fn metadata_file(&self) -> &Path {
        &self.metadata_file
    }
}

/// Server startup settings.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    paths: ServerPaths,
    bind_address: SocketAddr,
}

impl ServerOptions {
    /// Creates options using the default ephemeral IPv4 loopback address.
    #[must_use]
    pub fn new(paths: ServerPaths) -> Self {
        Self {
            paths,
            bind_address: DEFAULT_BIND_ADDRESS,
        }
    }

    /// Creates options for the current user's state directory.
    pub fn for_user() -> Result<Self, ServerError> {
        Ok(Self::new(ServerPaths::for_user()?))
    }

    /// Overrides the listener address. Only loopback addresses are accepted.
    #[must_use]
    pub fn with_bind_address(mut self, bind_address: SocketAddr) -> Self {
        self.bind_address = bind_address;
        self
    }

    #[must_use]
    pub fn paths(&self) -> &ServerPaths {
        &self.paths
    }

    #[must_use]
    pub fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BearerToken(String);

impl BearerToken {
    fn generate() -> Result<Self, ServerError> {
        let mut random = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut random).map_err(|_| ServerError::RandomnessUnavailable)?;

        let mut encoded = String::with_capacity(TOKEN_HEX_BYTES);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in random {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Ok(Self(encoded))
    }

    fn parse(encoded: String) -> Result<Self, ServerError> {
        if encoded.len() != TOKEN_HEX_BYTES
            || !encoded
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ServerError::MetadataCorrupt);
        }
        Ok(Self(encoded))
    }

    fn expose(&self) -> &str {
        &self.0
    }

    fn matches(&self, candidate: &[u8]) -> bool {
        constant_time_eq(candidate, self.0.as_bytes())
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

impl fmt::Display for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// Authenticated coordinates for a running local server.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConnection {
    address: SocketAddr,
    token: BearerToken,
    server_info: ServerInfo,
}

impl ServerConnection {
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub(crate) fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(self.token.expose())
    }

    pub(crate) fn endpoint(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }
}

impl fmt::Debug for ServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConnection")
            .field("address", &self.address)
            .field("token", &self.token)
            .field("server_info", &self.server_info)
            .finish()
    }
}

impl fmt::Display for ServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (pid {}, protocol {}, token {})",
            self.address, self.server_info.pid, self.server_info.protocol_version, self.token
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataFile {
    format_version: u16,
    address: String,
    pid: u32,
    protocol_version: u16,
    version: String,
    token: String,
}

impl MetadataFile {
    fn new(connection: &ServerConnection) -> Self {
        Self {
            format_version: METADATA_FORMAT_VERSION,
            address: connection.address.to_string(),
            pid: connection.server_info.pid,
            protocol_version: connection.server_info.protocol_version,
            version: connection.server_info.version.clone(),
            token: connection.token.expose().to_owned(),
        }
    }

    fn into_connection(self) -> Result<ServerConnection, ServerError> {
        if self.format_version != METADATA_FORMAT_VERSION {
            return Err(ServerError::MetadataVersionMismatch {
                expected: METADATA_FORMAT_VERSION,
                found: self.format_version,
            });
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ServerError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: self.protocol_version,
            });
        }
        if self.pid == 0 || !valid_process_version(&self.version) {
            return Err(ServerError::MetadataCorrupt);
        }
        let address = self
            .address
            .parse::<SocketAddr>()
            .map_err(|_| ServerError::MetadataCorrupt)?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(ServerError::MetadataCorrupt);
        }

        Ok(ServerConnection {
            address,
            token: BearerToken::parse(self.token)?,
            server_info: ServerInfo {
                protocol_version: self.protocol_version,
                version: self.version,
                pid: self.pid,
            },
        })
    }

    fn belongs_to(&self, connection: &ServerConnection) -> bool {
        self.format_version == METADATA_FORMAT_VERSION
            && self.address == connection.address.to_string()
            && self.pid == connection.server_info.pid
            && self.protocol_version == connection.server_info.protocol_version
            && self.version == connection.server_info.version
            && constant_time_eq(self.token.as_bytes(), connection.token.expose().as_bytes())
    }
}

impl fmt::Debug for MetadataFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetadataFile")
            .field("format_version", &self.format_version)
            .field("address", &self.address)
            .field("pid", &self.pid)
            .field("protocol_version", &self.protocol_version)
            .field("version", &self.version)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for MetadataFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (pid {}, protocol {}, token [REDACTED])",
            self.address, self.pid, self.protocol_version
        )
    }
}

/// Result of attempting to become the user-scoped server.
#[derive(Debug)]
pub enum StartOutcome {
    Started(ServerHandle),
    Existing(ServerConnection),
}

/// Owns a running server task and its graceful shutdown signal.
pub struct ServerHandle {
    connection: ServerConnection,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServerError>>>,
}

impl ServerHandle {
    #[must_use]
    pub fn connection(&self) -> &ServerConnection {
        &self.connection
    }

    /// Requests graceful shutdown and waits for metadata and lock cleanup.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join().await
    }

    /// Runs in the foreground until the server stops or this future is cancelled.
    pub async fn wait(mut self) -> Result<(), ServerError> {
        // Keeping the sender alive prevents the receiver from treating this as shutdown.
        let shutdown = self.shutdown.take();
        let result = self.join().await;
        drop(shutdown);
        result
    }

    async fn join(&mut self) -> Result<(), ServerError> {
        let task = self.task.take().ok_or(ServerError::ServerTaskStopped)?;
        task.await.map_err(|_| ServerError::ServerTaskStopped)?
    }
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerHandle")
            .field("connection", &self.connection)
            .field("running", &self.task.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Starts the server or returns the authenticated connection for the existing instance.
pub async fn start(
    handler: Arc<dyn AskHandler>,
    options: ServerOptions,
) -> Result<StartOutcome, ServerError> {
    if !options.bind_address.ip().is_loopback() {
        return Err(ServerError::NonLoopbackBind(options.bind_address));
    }

    ensure_private_directory(&options.paths.directory)?;
    let lock = open_private_lock_file(&options.paths.lock_file)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            drop(lock);
            return find_existing_server(&options.paths)
                .await
                .map(StartOutcome::Existing);
        }
        Err(TryLockError::Error(source)) => {
            return Err(ServerError::StateIo {
                action: "lock",
                source,
            });
        }
    }

    let listener = TcpListener::bind(options.bind_address)
        .await
        .map_err(|source| ServerError::Bind {
            address: options.bind_address,
            source,
        })?;
    let address = listener.local_addr().map_err(|source| ServerError::Bind {
        address: options.bind_address,
        source,
    })?;
    let connection = ServerConnection {
        address,
        token: BearerToken::generate()?,
        server_info: ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
        },
    };
    let metadata = MetadataFile::new(&connection);
    write_metadata_atomically(&options.paths, &metadata)?;

    let app = router(handler, connection.clone());
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let mut guard = InstanceGuard {
        _lock: lock,
        paths: options.paths,
        connection: connection.clone(),
        cleaned: false,
    };
    let task = tokio::spawn(async move {
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
            .map_err(|source| ServerError::Serve { source });
        let cleanup_result = guard.cleanup();
        serve_result.and(cleanup_result)
    });

    Ok(StartOutcome::Started(ServerHandle {
        connection,
        shutdown: Some(shutdown_sender),
        task: Some(task),
    }))
}

#[derive(Clone)]
struct AppState {
    handler: Arc<dyn AskHandler>,
    connection: ServerConnection,
    session_requests: Arc<Semaphore>,
    subscriptions: Arc<Semaphore>,
}

fn router(handler: Arc<dyn AskHandler>, connection: ServerConnection) -> Router {
    let state = AppState {
        handler,
        connection,
        session_requests: Arc::new(Semaphore::new(MAX_CONCURRENT_SESSION_REQUESTS)),
        subscriptions: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBSCRIPTIONS)),
    };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/workspaces/resolve", post(resolve_workspace))
        .route("/v1/workspaces/snapshot", post(workspace_snapshot))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/prompts", post(submit_prompt))
        .route("/v1/runs/cancel", post(cancel_run))
        .route(
            "/v1/workspaces/{workspace_id}/events",
            get(workspace_events),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
}

async fn authenticate(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !authorized(request.headers(), &state.connection.token) {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    next.run(request).await
}

fn authorized(headers: &HeaderMap, token: &BearerToken) -> bool {
    let candidate = headers
        .get(AUTHORIZATION)
        .map(|value| value.as_bytes())
        .and_then(|value| value.strip_prefix(b"Bearer "))
        .unwrap_or_default();
    token.matches(candidate)
}

async fn health(State(state): State<AppState>) -> Json<ServerInfo> {
    Json(state.connection.server_info.clone())
}

async fn ask(State(state): State<AppState>, body: Result<Bytes, BytesRejection>) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<AskRequest>(&body) {
        Ok(request) => request,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    if let Err(message) = validate_ask_request(&request) {
        return api_error(StatusCode::BAD_REQUEST, message);
    }

    let events = match state.handler.ask(request).await {
        Ok(events) => events,
        Err(AskHandlerError::InvalidRequest) => {
            return api_error(StatusCode::BAD_REQUEST, "request was rejected");
        }
        Err(AskHandlerError::Unavailable) => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "request service is unavailable",
            );
        }
        Err(AskHandlerError::Internal) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "request failed");
        }
    };

    let output = stream! {
        let mut events = events;
        while let Some(event) = events.next().await {
            let encoded = serde_json::to_string(&event)
                .expect("RunEvent serialization cannot fail");
            if encoded.len() > MAX_EVENT_BYTES {
                return;
            }
            yield Ok::<Event, Infallible>(Event::default().data(encoded));
        }
    };
    Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

async fn resolve_workspace(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::ResolveWorkspace { .. })
    })
    .await
}

async fn create_session(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::CreateSession { .. })
    })
    .await
}

async fn submit_prompt(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::SubmitPrompt { .. })
    })
    .await
}

async fn cancel_run(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    session_command(state, body, |command| {
        matches!(command, SessionCommand::CancelRun { .. })
    })
    .await
}

async fn session_command(
    state: AppState,
    body: Result<Bytes, BytesRejection>,
    expected: impl FnOnce(&SessionCommand) -> bool,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<CommandRequest>(&body) {
        Ok(request) if expected(&request.command) => request,
        Ok(_) | Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    match state.handler.command(request).await {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => handler_error_response(error),
    }
}

async fn workspace_snapshot(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Ok(_permit) = Arc::clone(&state.session_requests).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests are active",
        );
    };
    let body = match body {
        Ok(body) => body,
        Err(_) => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let request = match serde_json::from_slice::<SnapshotRequest>(&body) {
        Ok(request) => request,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid request"),
    };
    match state.handler.snapshot(request).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => handler_error_response(error),
    }
}

async fn workspace_events(
    State(state): State<AppState>,
    AxumPath(workspace_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = match workspace_id.parse::<WorkspaceId>() {
        Ok(workspace_id) => workspace_id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "workspace ID is invalid"),
    };
    let after = match headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
    {
        Some(cursor) => cursor,
        None => return api_error(StatusCode::BAD_REQUEST, "Last-Event-ID is required"),
    };
    let Ok(permit) = Arc::clone(&state.subscriptions).try_acquire_owned() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many event subscriptions are active",
        );
    };
    let events = match state.handler.subscribe(SubscribeRequest {
        workspace_id,
        after,
    }) {
        Ok(events) => events,
        Err(error) => return handler_error_response(error),
    };
    let output = stream! {
        let _permit = permit;
        let mut events = events;
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(_) => return,
            };
            let encoded = serde_json::to_string(&event)
                .expect("SessionEventEnvelope serialization cannot fail");
            if encoded.len() > MAX_EVENT_BYTES {
                return;
            }
            yield Ok::<Event, Infallible>(
                Event::default()
                    .id(event.cursor.to_string())
                    .event("session_event")
                    .data(encoded),
            );
        }
    };
    Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

fn handler_error_response(error: AskHandlerError) -> Response {
    match error {
        AskHandlerError::InvalidRequest => {
            api_error(StatusCode::BAD_REQUEST, "request was rejected")
        }
        AskHandlerError::Unavailable => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "request service is unavailable",
        ),
        AskHandlerError::Internal => api_error(StatusCode::INTERNAL_SERVER_ERROR, "request failed"),
    }
}

pub(crate) fn validate_ask_request(request: &AskRequest) -> Result<(), &'static str> {
    if request.prompt.trim().is_empty() {
        return Err("prompt must not be empty");
    }
    if request.prompt.len() > MAX_PROMPT_BYTES {
        return Err("prompt is too large");
    }
    if request.session_id.as_ref().is_some_and(|session_id| {
        session_id.len() != SESSION_ID_HEX_BYTES
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err("session ID is invalid");
    }
    let workspace = request
        .workspace
        .to_str()
        .ok_or("workspace path must be valid UTF-8")?;
    if workspace.is_empty() {
        return Err("workspace path must not be empty");
    }
    if workspace.len() > MAX_WORKSPACE_BYTES {
        return Err("workspace path is too large");
    }
    if request
        .model
        .as_ref()
        .is_some_and(|model| model.len() > MAX_MODEL_BYTES)
    {
        return Err("model is too large");
    }
    if request
        .organization
        .as_ref()
        .is_some_and(|organization| organization.len() > MAX_ORGANIZATION_BYTES)
    {
        return Err("organization is too large");
    }
    Ok(())
}

#[derive(Serialize)]
struct ApiErrorBody {
    error: &'static str,
}

fn api_error(status: StatusCode, error: &'static str) -> Response {
    (status, Json(ApiErrorBody { error })).into_response()
}

async fn method_not_allowed() -> Response {
    api_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not found")
}

fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut difference = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        difference |= usize::from(candidate_byte ^ expected_byte);
    }
    difference == 0
}

fn valid_process_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 256
        && version.bytes().all(|byte| byte.is_ascii_graphic())
}

struct InstanceGuard {
    _lock: File,
    paths: ServerPaths,
    connection: ServerConnection,
    cleaned: bool,
}

impl InstanceGuard {
    fn cleanup(&mut self) -> Result<(), ServerError> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        let Some(metadata) = read_metadata_file(&self.paths)? else {
            return Ok(());
        };
        if !metadata.belongs_to(&self.connection) {
            return Ok(());
        }
        fs::remove_file(&self.paths.metadata_file).map_err(|source| ServerError::StateIo {
            action: "remove metadata from",
            source,
        })?;
        sync_directory(&self.paths.directory)
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(crate) fn read_connection(
    paths: &ServerPaths,
) -> Result<Option<ServerConnection>, ServerError> {
    read_metadata_file(paths)?
        .map(MetadataFile::into_connection)
        .transpose()
}

fn read_metadata_file(paths: &ServerPaths) -> Result<Option<MetadataFile>, ServerError> {
    if !validate_existing_private_directory(&paths.directory)? {
        return Ok(None);
    }
    let Some(mut file) = open_private_read_file(&paths.metadata_file)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    Read::take(&mut file, (MAX_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| ServerError::StateIo {
            action: "read metadata from",
            source,
        })?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(ServerError::MetadataTooLarge);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ServerError::MetadataCorrupt)?;
    ron::from_str(text)
        .map(Some)
        .map_err(|_| ServerError::MetadataCorrupt)
}

fn write_metadata_atomically(
    paths: &ServerPaths,
    metadata: &MetadataFile,
) -> Result<(), ServerError> {
    if open_private_read_file(&paths.metadata_file)?.is_some() {
        // Opening validates that an existing destination is neither a symlink nor insecure.
    }
    let encoded = ron::ser::to_string(metadata).map_err(|_| ServerError::MetadataCorrupt)?;
    if encoded.len() > MAX_METADATA_BYTES {
        return Err(ServerError::MetadataTooLarge);
    }

    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| ServerError::RandomnessUnavailable)?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = paths.directory.join(format!(
        ".{METADATA_FILE_NAME}.{}.{suffix}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(encoded.as_bytes())
            .map_err(|source| ServerError::StateIo {
                action: "write metadata to",
                source,
            })?;
        file.sync_all().map_err(|source| ServerError::StateIo {
            action: "sync metadata in",
            source,
        })?;
        drop(file);
        fs::rename(&temporary, &paths.metadata_file).map_err(|source| ServerError::StateIo {
            action: "publish metadata in",
            source,
        })?;
        sync_directory(&paths.directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

async fn find_existing_server(paths: &ServerPaths) -> Result<ServerConnection, ServerError> {
    let client = probe_client().map_err(|_| ServerError::ExistingServerUnavailable)?;
    let mut meaningful_error = None;

    for attempt in 0..STARTUP_RETRIES {
        match read_connection(paths) {
            Ok(Some(connection)) => match probe_health(&client, &connection).await {
                Ok(info) if info == connection.server_info => return Ok(connection),
                Ok(_) | Err(HealthProbeError::Unavailable) => {}
                Err(HealthProbeError::ProtocolMismatch { found }) => {
                    meaningful_error = Some(ServerError::ProtocolMismatch {
                        expected: PROTOCOL_VERSION,
                        found,
                    });
                }
            },
            Ok(None) => {}
            Err(error @ ServerError::ProtocolMismatch { .. })
            | Err(error @ ServerError::MetadataVersionMismatch { .. })
            | Err(error @ ServerError::MetadataCorrupt)
            | Err(error @ ServerError::MetadataTooLarge) => meaningful_error = Some(error),
            Err(error) => return Err(error),
        }
        if attempt + 1 < STARTUP_RETRIES {
            tokio::time::sleep(STARTUP_RETRY_DELAY).await;
        }
    }

    Err(meaningful_error.unwrap_or(ServerError::ExistingServerUnavailable))
}

pub(crate) fn probe_client() -> Result<reqwest::Client, ()> {
    reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())
}

pub(crate) enum HealthProbeError {
    Unavailable,
    ProtocolMismatch { found: u16 },
}

pub(crate) async fn probe_health(
    client: &reqwest::Client,
    connection: &ServerConnection,
) -> Result<ServerInfo, HealthProbeError> {
    let response = connection
        .authorize(client.get(connection.endpoint("/v1/health")))
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|_| HealthProbeError::Unavailable)?;
    if response.status() != StatusCode::OK {
        return Err(HealthProbeError::Unavailable);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HEALTH_BYTES as u64)
    {
        return Err(HealthProbeError::Unavailable);
    }
    let bytes = read_response_bounded(response, MAX_HEALTH_BYTES)
        .await
        .map_err(|_| HealthProbeError::Unavailable)?;
    let info =
        serde_json::from_slice::<ServerInfo>(&bytes).map_err(|_| HealthProbeError::Unavailable)?;
    if info.protocol_version != PROTOCOL_VERSION {
        return Err(HealthProbeError::ProtocolMismatch {
            found: info.protocol_version,
        });
    }
    if info.pid == 0 || !valid_process_version(&info.version) {
        return Err(HealthProbeError::Unavailable);
    }
    Ok(info)
}

pub(crate) async fn read_response_bounded(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ()> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn ensure_private_directory(path: &Path) -> Result<(), ServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_directory_metadata(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            let metadata = fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
                action: "inspect",
                source,
            })?;
            validate_directory_metadata(path, &metadata)
        }
        Err(source) => Err(ServerError::StateIo {
            action: "inspect",
            source,
        }),
    }
}

fn validate_existing_private_directory(path: &Path) -> Result<bool, ServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_directory_metadata(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ServerError::StateIo {
            action: "inspect",
            source,
        }),
    }
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), ServerError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServerError::InsecureStatePath(path.to_path_buf()));
    }
    validate_private_permissions(path, metadata, 0o700)
}

fn create_private_directory(path: &Path) -> Result<(), ServerError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|source| ServerError::StateIo {
        action: "create",
        source,
    })
}

fn open_private_lock_file(path: &Path) -> Result<File, ServerError> {
    for _ in 0..4 {
        match fs::symlink_metadata(path) {
            Ok(path_metadata) => {
                validate_file_metadata(path, &path_metadata)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|source| ServerError::StateIo {
                        action: "open lock file in",
                        source,
                    })?;
                validate_open_file(path, &path_metadata, &file)?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match create_private_file(path) {
                    Ok(file) => return Ok(file),
                    Err(ServerError::StateIo { source, .. })
                        if source.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(source) => {
                return Err(ServerError::StateIo {
                    action: "inspect lock file in",
                    source,
                });
            }
        }
    }
    Err(ServerError::StateRace)
}

fn open_private_read_file(path: &Path) -> Result<Option<File>, ServerError> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ServerError::StateIo {
                action: "inspect metadata in",
                source,
            });
        }
    };
    validate_file_metadata(path, &path_metadata)?;
    let file = File::open(path).map_err(|source| ServerError::StateIo {
        action: "open metadata in",
        source,
    })?;
    validate_open_file(path, &path_metadata, &file)?;
    Ok(Some(file))
}

fn create_private_file(path: &Path) -> Result<File, ServerError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|source| ServerError::StateIo {
        action: "create private file in",
        source,
    })?;
    let path_metadata = fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
        action: "inspect private file in",
        source,
    })?;
    validate_file_metadata(path, &path_metadata)?;
    validate_open_file(path, &path_metadata, &file)?;
    Ok(file)
}

fn validate_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), ServerError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServerError::InsecureStatePath(path.to_path_buf()));
    }
    validate_private_permissions(path, metadata, 0o600)
}

fn validate_open_file(
    path: &Path,
    path_metadata: &fs::Metadata,
    file: &File,
) -> Result<(), ServerError> {
    let file_metadata = file.metadata().map_err(|source| ServerError::StateIo {
        action: "inspect open file in",
        source,
    })?;
    validate_file_metadata(path, &file_metadata)?;
    let current_path_metadata =
        fs::symlink_metadata(path).map_err(|source| ServerError::StateIo {
            action: "reinspect open file in",
            source,
        })?;
    validate_file_metadata(path, &current_path_metadata)?;
    if !same_file(path_metadata, &current_path_metadata)
        || !same_file(&current_path_metadata, &file_metadata)
    {
        return Err(ServerError::StateRace);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(
    path: &Path,
    metadata: &fs::Metadata,
    expected: u32,
) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o7777 != expected {
        return Err(ServerError::InsecurePermissions {
            path: path.to_path_buf(),
            expected,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(
    _path: &Path,
    _metadata: &fs::Metadata,
    _expected: u32,
) -> Result<(), ServerError> {
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ServerError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ServerError::StateIo {
            action: "sync",
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ServerError> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not determine the user-scoped server state directory")]
    StateDirectoryUnavailable,
    #[error("non-loopback server bind address is not supported: {0}")]
    NonLoopbackBind(SocketAddr),
    #[error("server state path is not a private regular file or directory: {0}")]
    InsecureStatePath(PathBuf),
    #[error("server state path has insecure permissions (expected {expected:o}): {path}")]
    InsecurePermissions { path: PathBuf, expected: u32 },
    #[error("server state changed while it was being validated")]
    StateRace,
    #[error("could not {action} server state")]
    StateIo {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("server metadata is corrupt")]
    MetadataCorrupt,
    #[error("server metadata exceeds the size limit")]
    MetadataTooLarge,
    #[error("server metadata version {found} is unsupported (expected {expected})")]
    MetadataVersionMismatch { expected: u16, found: u16 },
    #[error("server protocol version {found} does not match client version {expected}")]
    ProtocolMismatch { expected: u16, found: u16 },
    #[error("secure random bytes are unavailable")]
    RandomnessUnavailable,
    #[error("could not bind local server at {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("the existing server did not become healthy")]
    ExistingServerUnavailable,
    #[error("local server stopped unexpectedly")]
    Serve {
        #[source]
        source: io::Error,
    },
    #[error("local server task stopped unexpectedly")]
    ServerTaskStopped,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, atomic::AtomicU64, atomic::Ordering},
        time::SystemTime,
    };

    use futures_util::stream as futures_stream;
    use qq_protocol::RunEvent;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        root: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "qq-server-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn paths(&self) -> ServerPaths {
            ServerPaths::new(self.root.join("state"))
        }

        fn child_paths(&self, name: &str) -> ServerPaths {
            ServerPaths::new(self.root.join(name))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fake_handler(events: Vec<RunEvent>) -> (Arc<dyn AskHandler>, Arc<Mutex<Vec<AskRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handler: Arc<dyn AskHandler> = Arc::new(move |request: AskRequest| {
            captured.lock().unwrap().push(request);
            let events = events.clone();
            async move { Ok(Box::pin(futures_stream::iter(events)) as RunStream) }
        });
        (handler, requests)
    }

    fn test_request(prompt: &str) -> AskRequest {
        AskRequest::new(prompt, PathBuf::from("/test/workspace"))
    }

    async fn start_test_server(paths: ServerPaths, handler: Arc<dyn AskHandler>) -> ServerHandle {
        match start(handler, ServerOptions::new(paths)).await.unwrap() {
            StartOutcome::Started(handle) => handle,
            StartOutcome::Existing(_) => panic!("test unexpectedly found an existing server"),
        }
    }

    #[tokio::test]
    async fn health_requires_the_metadata_token() {
        let directory = TestDirectory::new();
        let (handler, requests) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(directory.paths(), handler).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let health_url = server.connection().endpoint("/v1/health");

        let missing = http.get(&health_url).send().await.unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let wrong = http
            .get(&health_url)
            .header(AUTHORIZATION, format!("Bearer {}", "0".repeat(64)))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let response = server
            .connection()
            .authorize(http.get(&health_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<ServerInfo>().await.unwrap(),
            *server.connection().server_info()
        );
        let unsupported = server
            .connection()
            .authorize(http.post(&health_url))
            .send()
            .await
            .unwrap();
        assert_eq!(unsupported.status(), StatusCode::METHOD_NOT_ALLOWED);
        let missing_route = http
            .get(server.connection().endpoint("/not-a-route"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_route.status(), StatusCode::NOT_FOUND);
        assert!(requests.lock().unwrap().is_empty());

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_ask_route_is_not_exposed() {
        let directory = TestDirectory::new();
        let (handler, requests) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(directory.paths(), handler).await;
        let response = server
            .connection()
            .authorize(
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .post(server.connection().endpoint("/v1/ask")),
            )
            .json(&test_request("say hello"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(requests.lock().unwrap().is_empty());
        server.shutdown().await.unwrap();
    }

    #[test]
    fn validates_session_identifiers() {
        let mut request = test_request("hello");
        request.session_id = Some("0123456789abcdef0123456789abcdef".to_owned());
        assert_eq!(validate_ask_request(&request), Ok(()));

        for invalid in [
            "",
            "0123456789abcdef",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            request.session_id = Some(invalid.to_owned());
            assert_eq!(validate_ask_request(&request), Err("session ID is invalid"));
        }
    }

    #[tokio::test]
    async fn only_one_concurrent_start_wins() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);

        let (left, right) = tokio::join!(
            start(Arc::clone(&handler), ServerOptions::new(paths.clone())),
            start(handler, ServerOptions::new(paths)),
        );

        let mut started = None;
        let mut existing = None;
        for outcome in [left.unwrap(), right.unwrap()] {
            match outcome {
                StartOutcome::Started(handle) => {
                    assert!(started.replace(handle).is_none());
                }
                StartOutcome::Existing(connection) => {
                    assert!(existing.replace(connection).is_none());
                }
            }
        }
        let started = started.expect("one start should win");
        assert_eq!(
            existing.expect("one start should discover it").address(),
            started.connection().address()
        );
        started.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn detects_an_already_running_server() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(paths.clone(), Arc::clone(&handler)).await;

        let outcome = start(handler, ServerOptions::new(paths)).await.unwrap();
        let StartOutcome::Existing(existing) = outcome else {
            panic!("second start should report the existing server");
        };
        assert_eq!(existing, *server.connection());

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replaces_stale_metadata_when_the_lock_is_available() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        ensure_private_directory(paths.directory()).unwrap();
        let stale_listener = TcpListener::bind(DEFAULT_BIND_ADDRESS).await.unwrap();
        let stale_address = stale_listener.local_addr().unwrap();
        drop(stale_listener);
        let stale = ServerConnection {
            address: stale_address,
            token: BearerToken("b".repeat(TOKEN_HEX_BYTES)),
            server_info: ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "stale".to_owned(),
                pid: 42,
            },
        };
        write_metadata_atomically(&paths, &MetadataFile::new(&stale)).unwrap();
        assert!(
            crate::client::discover_with_paths(&paths)
                .await
                .unwrap()
                .is_none()
        );
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);

        let server = start_test_server(paths.clone(), handler).await;

        let current = read_connection(&paths).unwrap().unwrap();
        assert_eq!(current, *server.connection());
        assert_ne!(current, stale);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reports_protocol_mismatch_while_another_instance_owns_the_lock() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        ensure_private_directory(paths.directory()).unwrap();
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
        let mut metadata = MetadataFile {
            format_version: METADATA_FORMAT_VERSION,
            address: "127.0.0.1:9".to_owned(),
            pid: 42,
            protocol_version: PROTOCOL_VERSION + 1,
            version: "future".to_owned(),
            token: "c".repeat(TOKEN_HEX_BYTES),
        };
        metadata.format_version = METADATA_FORMAT_VERSION + 1;
        metadata.protocol_version = PROTOCOL_VERSION;
        write_metadata_atomically(&paths, &metadata).unwrap();
        assert_eq!(
            crate::client::discover_with_paths(&paths)
                .await
                .unwrap_err(),
            crate::client::ClientError::MetadataVersionMismatch {
                expected: METADATA_FORMAT_VERSION,
                found: METADATA_FORMAT_VERSION + 1,
            }
        );
        metadata.format_version = METADATA_FORMAT_VERSION;
        metadata.protocol_version = PROTOCOL_VERSION + 1;
        write_metadata_atomically(&paths, &metadata).unwrap();

        assert_eq!(
            crate::client::discover_with_paths(&paths)
                .await
                .unwrap_err(),
            crate::client::ClientError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: PROTOCOL_VERSION + 1,
            }
        );
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let error = start(handler, ServerOptions::new(paths)).await.unwrap_err();
        assert!(matches!(
            error,
            ServerError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found,
            } if found == PROTOCOL_VERSION + 1
        ));
    }

    #[tokio::test]
    async fn graceful_shutdown_removes_owned_metadata_and_releases_the_lock() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(paths.clone(), handler).await;
        assert!(paths.metadata_file().is_file());
        assert!(
            crate::client::discover_with_paths(&paths)
                .await
                .unwrap()
                .is_some()
        );

        server.shutdown().await.unwrap();

        assert!(!paths.metadata_file().exists());
        assert!(
            crate::client::discover_with_paths(&paths)
                .await
                .unwrap()
                .is_none()
        );
        let lock = open_private_lock_file(paths.lock_file()).unwrap();
        lock.try_lock().unwrap();
    }

    #[tokio::test]
    async fn shutdown_does_not_remove_replaced_metadata() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(paths.clone(), handler).await;
        let replacement = ServerConnection {
            address: "127.0.0.1:10".parse().unwrap(),
            token: BearerToken("d".repeat(TOKEN_HEX_BYTES)),
            server_info: ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "replacement".to_owned(),
                pid: 43,
            },
        };
        write_metadata_atomically(&paths, &MetadataFile::new(&replacement)).unwrap();

        server.shutdown().await.unwrap();

        assert_eq!(read_connection(&paths).unwrap().unwrap(), replacement);
    }

    #[tokio::test]
    async fn client_bounds_non_success_response_bodies() {
        let listener = TcpListener::bind(DEFAULT_BIND_ADDRESS).await.unwrap();
        let address = listener.local_addr().unwrap();
        let raw_server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let body = vec![b'x'; MAX_ERROR_BODY_FOR_TEST];
            let headers = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(headers).await;
            let _ = socket.write_all(&body).await;
        });
        let connection = ServerConnection {
            address,
            token: BearerToken("e".repeat(TOKEN_HEX_BYTES)),
            server_info: ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "test".to_owned(),
                pid: 1,
            },
        };

        let error = match crate::client::ask(&connection, test_request("hello")).await {
            Ok(_) => panic!("oversized error response should fail"),
            Err(error) => error,
        };

        assert_eq!(error, crate::client::ClientError::ErrorResponseTooLarge);
        raw_server.await.unwrap();
    }

    const MAX_ERROR_BODY_FOR_TEST: usize = 32 * 1024;

    #[test]
    fn connection_and_metadata_formatting_redact_tokens() {
        let token = "f".repeat(TOKEN_HEX_BYTES);
        let connection = ServerConnection {
            address: "127.0.0.1:1234".parse().unwrap(),
            token: BearerToken(token.clone()),
            server_info: ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "test".to_owned(),
                pid: 1,
            },
        };
        let metadata = MetadataFile::new(&connection);

        assert!(!format!("{connection:?}").contains(&token));
        assert!(!connection.to_string().contains(&token));
        assert!(!format!("{:?}", connection.token).contains(&token));
        assert!(!connection.token.to_string().contains(&token));
        assert!(!format!("{metadata:?}").contains(&token));
        assert!(!metadata.to_string().contains(&token));
    }

    #[tokio::test]
    async fn rejects_non_loopback_bind_addresses() {
        let directory = TestDirectory::new();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let options =
            ServerOptions::new(directory.paths()).with_bind_address("0.0.0.0:0".parse().unwrap());

        let error = start(handler, options).await.unwrap_err();

        assert!(matches!(error, ServerError::NonLoopbackBind(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enforces_unix_permissions_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TestDirectory::new();
        let secure_paths = directory.child_paths("secure");
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        let server = start_test_server(secure_paths.clone(), handler).await;
        assert_eq!(
            fs::metadata(secure_paths.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(secure_paths.metadata_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        server.shutdown().await.unwrap();

        let insecure_paths = directory.child_paths("insecure");
        fs::create_dir(insecure_paths.directory()).unwrap();
        fs::set_permissions(
            insecure_paths.directory(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        assert!(matches!(
            start(handler, ServerOptions::new(insecure_paths))
                .await
                .unwrap_err(),
            ServerError::InsecurePermissions { .. }
        ));

        let symlink_paths = directory.child_paths("symlink-state");
        let target = directory.root.join("symlink-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, symlink_paths.directory()).unwrap();
        let (handler, _) = fake_handler(vec![RunEvent::Completed]);
        assert!(matches!(
            start(handler, ServerOptions::new(symlink_paths))
                .await
                .unwrap_err(),
            ServerError::InsecureStatePath(_)
        ));

        let metadata_paths = directory.child_paths("metadata-symlink");
        ensure_private_directory(metadata_paths.directory()).unwrap();
        let metadata_target = directory.root.join("metadata-target");
        File::create(&metadata_target).unwrap();
        symlink(&metadata_target, metadata_paths.metadata_file()).unwrap();
        assert!(matches!(
            read_connection(&metadata_paths).unwrap_err(),
            ServerError::InsecureStatePath(_)
        ));

        let permission_paths = directory.child_paths("metadata-permissions");
        ensure_private_directory(permission_paths.directory()).unwrap();
        fs::write(permission_paths.metadata_file(), b"not important").unwrap();
        fs::set_permissions(
            permission_paths.metadata_file(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(matches!(
            read_connection(&permission_paths).unwrap_err(),
            ServerError::InsecurePermissions { .. }
        ));
    }
}
