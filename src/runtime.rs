//! Application configuration to model-runtime composition.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures_util::{StreamExt, stream};
use qq_core::{RunStream, Runtime, RuntimeConfigError};
use qq_protocol::{AskRequest, RunCommand, RunEvent, RunFailureKind};
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ProviderCompiler,
    ProviderError, ProviderRecipe, bedrock::BedrockAuth as ProviderBedrockAuth,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    auth::{
        AuthError, CODEX_RESPONSES_ENDPOINT, CredentialStore, Secret, resolve_provider_credential,
    },
    config::{
        AwsAuth, BedrockAuth, ConfigError, ConfigLoader, ConfigSnapshot, Connection, LoadRequest,
        ProviderApi, ProviderAuth, ProviderConfig,
    },
    server::{AskFuture, AskHandler, AskHandlerError},
};

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CREDENTIAL_ENDPOINT: &str = "https://api.openai.com";
const OPENAI_STORED_CREDENTIAL: &str = "openai/default";
const OPENAI_ENVIRONMENT_CREDENTIAL: &str = "OPENAI_API_KEY";
const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_CREDENTIAL_ENDPOINT: &str = "https://api.anthropic.com";
const ANTHROPIC_STORED_CREDENTIAL: &str = "anthropic/default";
const ANTHROPIC_ENVIRONMENT_CREDENTIAL: &str = "ANTHROPIC_API_KEY";
const MAX_CACHED_RUNTIMES: usize = 16;
const MAX_SESSIONS: usize = 16;
const MAX_SESSION_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct RuntimeFactory {
    inner: Arc<RuntimeFactoryInner>,
}

struct RuntimeFactoryInner {
    config: ConfigLoader,
    credentials: CredentialStore,
    providers: ProviderCompiler,
    cache: Mutex<VecDeque<(RuntimeKey, Arc<Runtime>)>>,
}

impl RuntimeFactory {
    pub fn system() -> Result<Self, RuntimeBuildError> {
        Self::new(ConfigLoader::system()?, CredentialStore::system()?)
    }

    pub fn new(
        config: ConfigLoader,
        credentials: CredentialStore,
    ) -> Result<Self, RuntimeBuildError> {
        Ok(Self {
            inner: Arc::new(RuntimeFactoryInner {
                config,
                credentials,
                providers: ProviderCompiler::new()?,
                cache: Mutex::new(VecDeque::new()),
            }),
        })
    }

    pub fn load(&self, request: &LoadRequest) -> Result<ConfigSnapshot, RuntimeBuildError> {
        self.inner.config.load(request).map_err(Into::into)
    }

    pub fn runtime_for(&self, request: &LoadRequest) -> Result<Arc<Runtime>, RuntimeBuildError> {
        let snapshot = self.load(request)?;
        self.runtime_for_snapshot(&snapshot)
    }

    pub fn runtime_for_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<Arc<Runtime>, RuntimeBuildError> {
        self.runtime_with_key_for_snapshot(snapshot)
            .map(|(runtime, _)| runtime)
    }

    fn runtime_with_key_for_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<(Arc<Runtime>, RuntimeKey), RuntimeBuildError> {
        let provider_id = snapshot.model().provider();
        let provider_config = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        let (recipe, provider_key) = self.prepare_provider(provider_id, provider_config)?;
        let key = RuntimeKey::new(
            provider_id,
            snapshot.model().model(),
            snapshot.max_output_tokens(),
            &provider_key,
        );

        {
            let mut cache = self
                .inner
                .cache
                .lock()
                .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
            if let Some(runtime) = promote_cached_runtime(&mut cache, &key) {
                return Ok((runtime, key));
            }
        }

        let runtime = Arc::new(Runtime::with_provider(
            self.inner.providers.compile(recipe)?,
            snapshot.model().model(),
            snapshot.max_output_tokens(),
        )?);

        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
        if let Some(existing) = promote_cached_runtime(&mut cache, &key) {
            return Ok((existing, key));
        }
        cache.push_back((key.clone(), Arc::clone(&runtime)));
        while cache.len() > MAX_CACHED_RUNTIMES {
            cache.pop_front();
        }
        Ok((runtime, key))
    }

    fn prepare_provider(
        &self,
        provider_id: &str,
        config: &ProviderConfig,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        match config {
            ProviderConfig::OpenAi { api_key, .. } => {
                let secret = resolve_provider_credential(
                    &self.inner.credentials,
                    api_key.as_ref(),
                    OPENAI_STORED_CREDENTIAL,
                    OPENAI_ENVIRONMENT_CREDENTIAL,
                    Some(OPENAI_CREDENTIAL_ENDPOINT),
                )?;
                let auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    OPENAI_ENDPOINT,
                    "exact",
                    "openai_responses",
                    &auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                let recipe = ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact(OPENAI_ENDPOINT, false),
                    HttpProtocol::OpenAiResponses,
                    auth.into_http()?,
                ));
                Ok((recipe, key))
            }
            ProviderConfig::LiteLlm { connection, .. }
            | ProviderConfig::Custom { connection, .. } => {
                let connection = connection
                    .as_ref()
                    .ok_or_else(|| RuntimeBuildError::IncompleteProvider(provider_id.to_owned()))?;
                self.prepare_http_provider(provider_id, connection)
            }
            ProviderConfig::OpenAiCodex { profile, .. } => {
                let credential = self
                    .inner
                    .credentials
                    .resolve_codex(profile.as_deref().unwrap_or("default"))?;
                let key_auth = ResolvedAuth::Bearer(credential.access_token().clone());
                let mut key_headers = vec![("chatgpt-account-id", credential.account_id())];
                if credential.is_fedramp() {
                    key_headers.push(("x-openai-fedramp", "true"));
                }
                let key = provider_key(
                    provider_id,
                    CODEX_RESPONSES_ENDPOINT,
                    "exact",
                    "openai_codex_responses",
                    &key_auth,
                    key_headers,
                );
                let recipe = ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact(CODEX_RESPONSES_ENDPOINT, false),
                    HttpProtocol::OpenAiResponses,
                    HttpAuth::Codex {
                        access_token: credential.access_token().expose_secret_str()?.to_owned(),
                        account_id: credential.account_id().to_owned(),
                        is_fedramp: credential.is_fedramp(),
                    },
                ));
                Ok((recipe, key))
            }
            ProviderConfig::Anthropic { api_key, .. } => {
                let secret = resolve_provider_credential(
                    &self.inner.credentials,
                    api_key.as_ref(),
                    ANTHROPIC_STORED_CREDENTIAL,
                    ANTHROPIC_ENVIRONMENT_CREDENTIAL,
                    Some(ANTHROPIC_CREDENTIAL_ENDPOINT),
                )?;
                let auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    ANTHROPIC_ENDPOINT,
                    "exact",
                    "anthropic_messages",
                    &auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                let recipe = ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact(ANTHROPIC_ENDPOINT, false),
                    HttpProtocol::AnthropicMessages,
                    auth.into_http()?,
                ));
                Ok((recipe, key))
            }
            ProviderConfig::AmazonBedrock { region, auth, .. } => {
                self.prepare_bedrock_provider(provider_id, region.as_deref(), auth)
            }
            ProviderConfig::AmazonBedrockMantle { .. } => {
                Err(RuntimeBuildError::UnsupportedProvider {
                    provider: provider_id.to_owned(),
                    detail: "Amazon Bedrock Mantle is not wired yet",
                })
            }
        }
    }

    fn prepare_http_provider(
        &self,
        provider_id: &str,
        connection: &Connection,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let auth = self.resolve_http_auth(connection.auth(), connection.base_url())?;
        let headers = connection
            .headers()
            .iter()
            .map(|(name, value)| (name.clone(), value.expose_value().to_owned()))
            .collect::<Vec<_>>();
        let key = provider_key(
            provider_id,
            connection.base_url(),
            "base",
            provider_api_name(connection.api()),
            &auth,
            headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        let protocol = match connection.api() {
            ProviderApi::OpenAiResponses => HttpProtocol::OpenAiResponses,
            ProviderApi::OpenAiChatCompletions => HttpProtocol::OpenAiChatCompletions,
            ProviderApi::AnthropicMessages => HttpProtocol::AnthropicMessages,
            api => {
                return Err(RuntimeBuildError::UnsupportedApi {
                    provider: provider_id.to_owned(),
                    api,
                });
            }
        };
        let allow_http = connection
            .base_url()
            .split_once("://")
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http"));
        let recipe = ProviderRecipe::http(
            HttpProviderRecipe::new(
                EndpointSpec::base(connection.base_url(), allow_http),
                protocol,
                auth.into_http()?,
            )
            .with_headers(headers),
        );
        Ok((recipe, key))
    }

    fn prepare_bedrock_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let credential_endpoint =
            region.map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        let key_endpoint = credential_endpoint
            .as_deref()
            .unwrap_or("aws-region-provider-chain");

        let (auth, key) = match auth {
            BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "default-chain")],
                );
                (ProviderBedrockAuth::DefaultChain, key)
            }
            BedrockAuth::Aws(AwsAuth::Profile(profile)) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "profile"), ("aws-profile", profile)],
                );
                (ProviderBedrockAuth::Profile(profile.clone()), key)
            }
            BedrockAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, credential_endpoint.as_deref())?;
                let api_key = secret.expose_secret_str()?.to_owned();
                let key_auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &key_auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                (ProviderBedrockAuth::ApiKey(api_key), key)
            }
        };

        Ok((
            ProviderRecipe::amazon_bedrock(region.map(str::to_owned), auth),
            key,
        ))
    }

    fn resolve_http_auth(
        &self,
        auth: &ProviderAuth,
        endpoint: &str,
    ) -> Result<ResolvedAuth, RuntimeBuildError> {
        match auth {
            ProviderAuth::NoAuth => Ok(ResolvedAuth::NoAuth),
            ProviderAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::ApiKey(secret))
            }
            ProviderAuth::Bearer(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::Bearer(secret))
            }
            ProviderAuth::Header(name, reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::Header(name.clone(), secret))
            }
        }
    }
}

fn promote_cached_runtime(
    cache: &mut VecDeque<(RuntimeKey, Arc<Runtime>)>,
    key: &RuntimeKey,
) -> Option<Arc<Runtime>> {
    let index = cache.iter().position(|(candidate, _)| candidate == key)?;
    let (cached_key, runtime) = cache
        .remove(index)
        .expect("a located runtime cache entry must exist");
    cache.push_back((cached_key, Arc::clone(&runtime)));
    Some(runtime)
}

enum ResolvedAuth {
    NoAuth,
    ApiKey(Secret),
    Bearer(Secret),
    Header(String, Secret),
}

impl ResolvedAuth {
    fn into_http(self) -> Result<HttpAuth, AuthError> {
        match self {
            Self::NoAuth => Ok(HttpAuth::NoAuth),
            Self::ApiKey(secret) => Ok(HttpAuth::ApiKey(secret.expose_secret_str()?.to_owned())),
            Self::Bearer(secret) => Ok(HttpAuth::Bearer(secret.expose_secret_str()?.to_owned())),
            Self::Header(name, secret) => Ok(HttpAuth::Header(
                name,
                secret.expose_secret_str()?.to_owned(),
            )),
        }
    }

    fn update_digest(&self, digest: &mut Sha256) {
        match self {
            Self::NoAuth => update_digest(digest, b"no_auth"),
            Self::ApiKey(secret) => {
                update_digest(digest, b"api_key");
                update_digest(digest, secret.expose_secret_bytes());
            }
            Self::Bearer(secret) => {
                update_digest(digest, b"bearer");
                update_digest(digest, secret.expose_secret_bytes());
            }
            Self::Header(name, secret) => {
                update_digest(digest, b"header");
                update_digest(digest, name.as_bytes());
                update_digest(digest, secret.expose_secret_bytes());
            }
        }
    }
}

#[derive(Clone)]
pub struct RuntimeHandler {
    factory: RuntimeFactory,
    sessions: Arc<Mutex<SessionStore>>,
}

impl RuntimeHandler {
    #[must_use]
    pub fn new(factory: RuntimeFactory) -> Self {
        Self {
            factory,
            sessions: Arc::new(Mutex::new(SessionStore::default())),
        }
    }
}

impl AskHandler for RuntimeHandler {
    fn ask(&self, request: AskRequest) -> AskFuture {
        let factory = self.factory.clone();
        let sessions = Arc::clone(&self.sessions);
        let AskRequest {
            prompt,
            workspace,
            session_id,
            model,
            max_output_tokens,
            organization,
        } = request;
        Box::pin(async move {
            let mut session = match session_id {
                Some(session_id) => {
                    sessions
                        .lock()
                        .map_err(|_| AskHandlerError::Internal)?
                        .reserve(&session_id)
                        .map_err(SessionError::handler_error)?;
                    Some(SessionRun::new(Arc::clone(&sessions), session_id))
                }
                None => None,
            };
            let build = tokio::task::spawn_blocking(move || -> Result<_, RuntimeBuildError> {
                let canonical_workspace = std::fs::canonicalize(&workspace).map_err(|_| {
                    ConfigError::InvalidWorkingDirectory {
                        path: workspace.clone(),
                    }
                })?;
                let mut load =
                    LoadRequest::from_process_env(&canonical_workspace, max_output_tokens)?;
                let mut overrides = load.overrides().clone();
                if let Some(model) = model {
                    overrides = overrides.with_model(model);
                }
                if let Some(organization) = organization {
                    overrides = overrides.with_organization(organization);
                }
                load = load.with_overrides(overrides);
                let snapshot = factory.load(&load)?;
                let organization = snapshot.organization().map(str::to_owned);
                let (runtime, runtime_key) = factory.runtime_with_key_for_snapshot(&snapshot)?;
                Ok((
                    runtime,
                    SessionIdentity {
                        workspace: canonical_workspace,
                        organization,
                        runtime_key,
                    },
                ))
            })
            .await;

            match build {
                Ok(Ok((runtime, identity))) => {
                    let Some(session) = session.take() else {
                        return Ok(runtime.run(RunCommand::new(prompt)));
                    };
                    let context = sessions
                        .lock()
                        .map_err(|_| AskHandlerError::Internal)?
                        .begin(session.id(), identity, runtime, prompt)
                        .map_err(SessionError::handler_error)?;
                    Ok(track_session_run(
                        context.runtime.run_messages(context.messages),
                        session,
                        context.remaining_response_bytes,
                    ))
                }
                Ok(Err(error)) => Ok(failed_run(error)),
                Err(_) => Err(AskHandlerError::Internal),
            }
        })
    }
}

#[derive(Default)]
struct SessionStore {
    sessions: HashMap<String, SessionState>,
    clock: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct SessionIdentity {
    workspace: PathBuf,
    organization: Option<String>,
    runtime_key: RuntimeKey,
}

struct SessionContext {
    runtime: Arc<Runtime>,
    messages: Vec<Message>,
    remaining_response_bytes: usize,
}

struct SessionState {
    identity: Option<SessionIdentity>,
    runtime: Option<Arc<Runtime>>,
    messages: Vec<Message>,
    context_bytes: usize,
    running: bool,
    pending_user: bool,
    last_used: u64,
}

impl SessionStore {
    fn reserve(&mut self, id: &str) -> Result<(), SessionError> {
        if !self.sessions.contains_key(id) {
            self.make_room()?;
            self.sessions.insert(
                id.to_owned(),
                SessionState {
                    identity: None,
                    runtime: None,
                    messages: Vec::new(),
                    context_bytes: 0,
                    running: false,
                    pending_user: false,
                    last_used: 0,
                },
            );
        }

        let now = self.tick();
        let session = self.sessions.get_mut(id).ok_or(SessionError::Unavailable)?;
        if session.running {
            return Err(SessionError::Busy);
        }
        session.running = true;
        session.last_used = now;
        Ok(())
    }

    fn begin(
        &mut self,
        id: &str,
        identity: SessionIdentity,
        runtime: Arc<Runtime>,
        prompt: String,
    ) -> Result<SessionContext, SessionError> {
        let session = self.sessions.get_mut(id).ok_or(SessionError::Unavailable)?;
        if !session.running || session.pending_user {
            return Err(SessionError::Unavailable);
        }
        if session
            .identity
            .as_ref()
            .is_some_and(|existing| existing != &identity)
        {
            return Err(SessionError::IdentityMismatch);
        }
        if session.context_bytes.saturating_add(prompt.len()) > MAX_SESSION_CONTEXT_BYTES {
            return Err(SessionError::ContextTooLarge);
        }

        session.identity.get_or_insert(identity);
        let runtime = Arc::clone(session.runtime.get_or_insert(runtime));
        session.context_bytes += prompt.len();
        session.messages.push(Message::user(prompt));
        session.pending_user = true;
        Ok(SessionContext {
            runtime,
            messages: session.messages.clone(),
            remaining_response_bytes: MAX_SESSION_CONTEXT_BYTES - session.context_bytes,
        })
    }

    fn complete(&mut self, id: &str, response: String) -> Result<(), SessionError> {
        let now = self.tick();
        let session = self.sessions.get_mut(id).ok_or(SessionError::Unavailable)?;
        if !session.running || !session.pending_user {
            return Err(SessionError::Unavailable);
        }
        if response.trim().is_empty() {
            rollback_pending_turn(session);
            session.last_used = now;
            return Err(SessionError::EmptyResponse);
        }
        if session.context_bytes.saturating_add(response.len()) > MAX_SESSION_CONTEXT_BYTES {
            rollback_pending_turn(session);
            session.last_used = now;
            return Err(SessionError::ContextTooLarge);
        }

        session.context_bytes += response.len();
        session.messages.push(Message::assistant(response));
        session.running = false;
        session.pending_user = false;
        session.last_used = now;
        Ok(())
    }

    fn abort(&mut self, id: &str) {
        let now = self.tick();
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if session.running {
            rollback_pending_turn(session);
            session.last_used = now;
        }
        if session.messages.is_empty() {
            self.sessions.remove(id);
        }
    }

    fn make_room(&mut self) -> Result<(), SessionError> {
        if self.sessions.len() < MAX_SESSIONS {
            return Ok(());
        }
        let oldest = self
            .sessions
            .iter()
            .filter(|(_, session)| !session.running)
            .min_by_key(|(_, session)| session.last_used)
            .map(|(id, _)| id.clone())
            .ok_or(SessionError::Capacity)?;
        self.sessions.remove(&oldest);
        Ok(())
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

fn rollback_pending_turn(session: &mut SessionState) {
    if session.pending_user {
        if let Some(message) = session.messages.pop() {
            session.context_bytes = session
                .context_bytes
                .saturating_sub(message.content().len());
        }
        session.pending_user = false;
    }
    session.running = false;
}

struct SessionRun {
    sessions: Arc<Mutex<SessionStore>>,
    id: String,
    active: bool,
}

impl SessionRun {
    const fn new(sessions: Arc<Mutex<SessionStore>>, id: String) -> Self {
        Self {
            sessions,
            id,
            active: true,
        }
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&mut self, response: String) -> Result<(), SessionError> {
        if !std::mem::replace(&mut self.active, false) {
            return Err(SessionError::Unavailable);
        }
        self.sessions
            .lock()
            .map_err(|_| SessionError::Unavailable)?
            .complete(&self.id, response)
    }

    fn abort(&mut self) {
        if !std::mem::replace(&mut self.active, false) {
            return;
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.abort(&self.id);
        }
    }
}

impl Drop for SessionRun {
    fn drop(&mut self) {
        self.abort();
    }
}

fn track_session_run(
    events: RunStream,
    mut session: SessionRun,
    max_response_bytes: usize,
) -> RunStream {
    Box::pin(async_stream::stream! {
        let mut events = events;
        let mut response = String::new();
        while let Some(event) = events.next().await {
            match event {
                RunEvent::OutputTextDelta { text } => {
                    if response
                        .len()
                        .checked_add(text.len())
                        .is_none_or(|length| length > max_response_bytes)
                    {
                        session.abort();
                        yield RunEvent::Failed {
                            kind: RunFailureKind::Server,
                            message: SessionError::ContextTooLarge.to_string(),
                        };
                        return;
                    }
                    response.push_str(&text);
                    yield RunEvent::OutputTextDelta { text };
                }
                RunEvent::RefusalDelta { text } => {
                    if response
                        .len()
                        .checked_add(text.len())
                        .is_none_or(|length| length > max_response_bytes)
                    {
                        session.abort();
                        yield RunEvent::Failed {
                            kind: RunFailureKind::Server,
                            message: SessionError::ContextTooLarge.to_string(),
                        };
                        return;
                    }
                    response.push_str(&text);
                    yield RunEvent::RefusalDelta { text };
                }
                RunEvent::Completed => {
                    match session.complete(response) {
                        Ok(()) => yield RunEvent::Completed,
                        Err(error) => yield RunEvent::Failed {
                            kind: RunFailureKind::Server,
                            message: error.to_string(),
                        },
                    }
                    return;
                }
                RunEvent::Failed { kind, message } => {
                    session.abort();
                    yield RunEvent::Failed { kind, message };
                    return;
                }
                RunEvent::Started => yield RunEvent::Started,
            }
        }
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
enum SessionError {
    #[error("session already has a run in progress")]
    Busy,
    #[error("session context is too large")]
    ContextTooLarge,
    #[error("session capacity is exhausted")]
    Capacity,
    #[error("model completed without producing a response")]
    EmptyResponse,
    #[error("session workspace or model configuration changed")]
    IdentityMismatch,
    #[error("session state is unavailable")]
    Unavailable,
}

impl SessionError {
    const fn handler_error(self) -> AskHandlerError {
        match self {
            Self::ContextTooLarge | Self::IdentityMismatch => AskHandlerError::InvalidRequest,
            Self::Busy | Self::Capacity => AskHandlerError::Unavailable,
            Self::EmptyResponse | Self::Unavailable => AskHandlerError::Internal,
        }
    }
}

fn failed_run(error: RuntimeBuildError) -> RunStream {
    let kind = error.failure_kind();
    let message = error.to_string();
    Box::pin(stream::iter([
        RunEvent::Started,
        RunEvent::Failed { kind, message },
    ]))
}

fn provider_api_name(api: ProviderApi) -> &'static str {
    match api {
        ProviderApi::OpenAiResponses => "openai_responses",
        ProviderApi::OpenAiChatCompletions => "openai_chat_completions",
        ProviderApi::AnthropicMessages => "anthropic_messages",
        ProviderApi::GoogleGenerateContent => "google_generate_content",
        ProviderApi::BedrockConverse => "bedrock_converse",
    }
}

fn provider_key<'a>(
    provider: &str,
    endpoint: &str,
    endpoint_mode: &str,
    api: &str,
    auth: &ResolvedAuth,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<u8> {
    let mut digest = Sha256::new();
    update_digest(&mut digest, provider.as_bytes());
    update_digest(&mut digest, endpoint.as_bytes());
    update_digest(&mut digest, endpoint_mode.as_bytes());
    update_digest(&mut digest, api.as_bytes());
    auth.update_digest(&mut digest);
    for (name, value) in headers {
        update_digest(&mut digest, name.as_bytes());
        update_digest(&mut digest, value.as_bytes());
    }
    digest.finalize().to_vec()
}

fn update_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update(value.len().to_le_bytes());
    digest.update(value);
}

#[derive(Clone, PartialEq, Eq)]
struct RuntimeKey([u8; 32]);

impl RuntimeKey {
    fn new(provider: &str, model: &str, max_output_tokens: u32, provider_key: &[u8]) -> Self {
        let mut digest = Sha256::new();
        update_digest(&mut digest, provider.as_bytes());
        update_digest(&mut digest, model.as_bytes());
        digest.update(max_output_tokens.to_le_bytes());
        update_digest(&mut digest, provider_key);
        Self(digest.finalize().into())
    }
}

impl std::fmt::Debug for RuntimeKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RuntimeKey([REDACTED])")
    }
}

#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Runtime(#[from] RuntimeConfigError),
    #[error("configured provider does not exist: {0}")]
    UnknownProvider(String),
    #[error("provider {0:?} is missing its connection configuration")]
    IncompleteProvider(String),
    #[error("provider {provider:?} is not available: {detail}")]
    UnsupportedProvider {
        provider: String,
        detail: &'static str,
    },
    #[error("provider {provider:?} uses an API that is not available yet: {api:?}")]
    UnsupportedApi { provider: String, api: ProviderApi },
    #[error("runtime cache is unavailable")]
    CacheUnavailable,
}

impl RuntimeBuildError {
    fn failure_kind(&self) -> RunFailureKind {
        match self {
            Self::Config(ConfigError::PolicyViolation { .. }) => RunFailureKind::Policy,
            Self::Config(_) => RunFailureKind::Configuration,
            Self::Auth(_) => RunFailureKind::Authentication,
            Self::Provider(error) => match error.kind() {
                qq_provider::ProviderErrorKind::Configuration => {
                    RunFailureKind::ProviderConfiguration
                }
                qq_provider::ProviderErrorKind::Authentication => {
                    RunFailureKind::ProviderAuthentication
                }
                qq_provider::ProviderErrorKind::RateLimited => RunFailureKind::ProviderRateLimited,
                qq_provider::ProviderErrorKind::InvalidRequest => {
                    RunFailureKind::ProviderInvalidRequest
                }
                qq_provider::ProviderErrorKind::Unavailable => RunFailureKind::ProviderUnavailable,
                qq_provider::ProviderErrorKind::Transport => RunFailureKind::ProviderTransport,
                qq_provider::ProviderErrorKind::Api => RunFailureKind::ProviderApi,
                qq_provider::ProviderErrorKind::Response => RunFailureKind::ProviderResponse,
                qq_provider::ProviderErrorKind::Protocol => RunFailureKind::ProviderProtocol,
            },
            Self::Runtime(_)
            | Self::UnknownProvider(_)
            | Self::IncompleteProvider(_)
            | Self::UnsupportedProvider { .. }
            | Self::UnsupportedApi { .. } => RunFailureKind::ProviderConfiguration,
            Self::CacheUnavailable => RunFailureKind::Server,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        auth::{CredentialPaths, KeyringBackend, KeyringError},
        config::{ConfigPaths, RuntimeOverrides},
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use qq_provider::{ModelRequest, Provider, ProviderStream};

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct MemoryKeyring(Mutex<BTreeMap<String, Vec<u8>>>);

    struct EmptyProvider;

    impl Provider for EmptyProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::empty())
        }
    }

    impl KeyringBackend for MemoryKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .remove(name)
                .map(|_| ())
                .ok_or(KeyringError::Missing)
        }
    }

    struct RuntimeFixture {
        root: PathBuf,
    }

    impl RuntimeFixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "qq-runtime-test-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            for directory in ["global", "data", "managed", "work"] {
                fs::create_dir_all(root.join(directory)).unwrap();
            }
            Self { root }
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root.join(relative)
        }

        fn factory(&self) -> RuntimeFactory {
            self.factory_with_credentials(CredentialStore::with_paths(CredentialPaths::new(
                self.path("data"),
            )))
        }

        fn factory_with_credentials(&self, credentials: CredentialStore) -> RuntimeFactory {
            RuntimeFactory::new(
                ConfigLoader::new(ConfigPaths::new(
                    self.path("global"),
                    self.path("data"),
                    self.path("managed"),
                )),
                credentials,
            )
            .unwrap()
        }

        fn request(&self, content: impl Into<String>) -> LoadRequest {
            LoadRequest::new(self.path("work"))
                .with_explicit_content(content)
                .with_overrides(RuntimeOverrides::new().with_max_output_tokens(128))
        }
    }

    impl Drop for RuntimeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn constructs_every_wired_http_api_and_builtin_anthropic() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for api in [
            "OpenAiResponses",
            "OpenAiChatCompletions",
            "AnthropicMessages",
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: {api},
                                auth: NoAuth,
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));
            factory
                .runtime_for(&request)
                .unwrap_or_else(|error| panic!("failed to construct {api}: {error}"));
        }

        let anthropic = fixture.request(
            r#"(
                version: 1,
                model: "anthropic/claude-test",
                providers: {
                    "anthropic": Anthropic(
                        api_key: Value("anthropic-test-secret"),
                        models: {"claude-test": (name: "Claude test")},
                    ),
                },
            )"#,
        );
        factory.runtime_for(&anthropic).unwrap();
    }

    #[test]
    fn accepts_case_insensitive_loopback_http_schemes() {
        let fixture = RuntimeFixture::new();
        let request = fixture.request(
            r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "HTTP://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"test-model": (name: "Test model")},
                    ),
                },
            )"#,
        );

        fixture.factory().runtime_for(&request).unwrap();
    }

    #[test]
    fn constructs_and_reuses_openai_codex_runtime_for_the_selected_profile() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        let id_payload = serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        }))
        .unwrap();
        let id_token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(id_payload));
        let refreshed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stored = serde_json::json!({
            "version": 1,
            "id_token": id_token,
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "account_id": "workspace-test-id",
            "is_fedramp": false,
            "refreshed_at": refreshed_at
        });
        credentials
            .set_with_metadata(
                "openai-codex/work",
                serde_json::to_vec(&stored).unwrap(),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        let factory = fixture.factory_with_credentials(credentials);
        let request = fixture.request(
            r#"(
                version: 1,
                model: "openai-codex/gpt-test",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                },
            )"#,
        );

        let first = factory.runtime_for(&request).unwrap();
        let second = factory.runtime_for(&request).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn constructs_amazon_bedrock_runtimes_for_every_auth_mode_without_network_access() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for (provider, auth) in [
            ("bedrock-default", "Aws(DefaultChain)"),
            ("bedrock-profile", r#"Aws(Profile("work"))"#),
            ("bedrock-api-key", r#"ApiKey(Value("bedrock-test-secret"))"#),
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "{provider}/test-model",
                    providers: {{
                        "{provider}": AmazonBedrock(
                            region: "us-east-1",
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));

            factory
                .runtime_for(&request)
                .unwrap_or_else(|error| panic!("failed to construct {provider}: {error}"));
        }
    }

    #[test]
    fn reuses_matching_runtimes_and_separates_auth_modes() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |auth: &str| {
            format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: {auth},
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            )
        };

        let api_key = fixture.request(document(r#"ApiKey(Value("same-test-secret"))"#));
        let bearer = fixture.request(document(r#"Bearer(Value("same-test-secret"))"#));
        let first = factory.runtime_for(&api_key).unwrap();
        let reused = factory.runtime_for(&api_key).unwrap();
        let different_auth = factory.runtime_for(&bearer).unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &different_auth));
    }

    #[test]
    fn cache_key_includes_custom_auth_header_name() {
        let secret = || Secret::from_secret_bytes(b"same-test-secret".to_vec());
        let first = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "exact",
            "openai_responses",
            &ResolvedAuth::Header("x-first".to_owned(), secret()),
            [],
        );
        let second = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "exact",
            "openai_responses",
            &ResolvedAuth::Header("x-second".to_owned(), secret()),
            [],
        );
        let different_endpoint_mode = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "base",
            "openai_responses",
            &ResolvedAuth::Header("x-first".to_owned(), secret()),
            [],
        );

        assert_ne!(first, second);
        assert_ne!(first, different_endpoint_mode);
    }

    #[test]
    fn sessions_replay_completed_turns_into_the_next_model_request() {
        let mut sessions = SessionStore::default();
        let session_id = "0123456789abcdef0123456789abcdef";
        let identity = test_session_identity(1);

        let first = begin_test_session(&mut sessions, session_id, &identity, "hey");
        assert_eq!(first.messages, [Message::user("hey")]);
        sessions
            .complete(session_id, "Hello! How can I help?".to_owned())
            .unwrap();

        let second = begin_test_session(
            &mut sessions,
            session_id,
            &identity,
            "what was my first message?",
        );
        assert_eq!(
            second.messages,
            [
                Message::user("hey"),
                Message::assistant("Hello! How can I help?"),
                Message::user("what was my first message?"),
            ]
        );
    }

    #[tokio::test]
    async fn session_stream_commits_the_complete_assistant_response() {
        let sessions = Arc::new(Mutex::new(SessionStore::default()));
        let session_id = "0123456789abcdef0123456789abcdef";
        let identity = test_session_identity(1);
        let context =
            begin_test_session(&mut sessions.lock().unwrap(), session_id, &identity, "hey");
        let events: RunStream = Box::pin(stream::iter([
            RunEvent::Started,
            RunEvent::OutputTextDelta {
                text: "Hello! ".to_owned(),
            },
            RunEvent::OutputTextDelta {
                text: "How can I help?".to_owned(),
            },
            RunEvent::Completed,
        ]));

        let emitted = track_session_run(
            events,
            SessionRun::new(Arc::clone(&sessions), session_id.to_owned()),
            context.remaining_response_bytes,
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(emitted.last(), Some(&RunEvent::Completed));

        let next = begin_test_session(
            &mut sessions.lock().unwrap(),
            session_id,
            &identity,
            "what was my first message?",
        );
        assert_eq!(
            next.messages,
            [
                Message::user("hey"),
                Message::assistant("Hello! How can I help?"),
                Message::user("what was my first message?"),
            ]
        );
    }

    #[test]
    fn sessions_reject_workspace_or_runtime_identity_changes() {
        let mut sessions = SessionStore::default();
        let session_id = "0123456789abcdef0123456789abcdef";
        let identity = test_session_identity(1);

        begin_test_session(&mut sessions, session_id, &identity, "secret");
        sessions
            .complete(session_id, "acknowledged".to_owned())
            .unwrap();
        sessions.reserve(session_id).unwrap();
        assert!(matches!(
            sessions.begin(
                session_id,
                test_session_identity(2),
                test_runtime(),
                "repeat it".to_owned()
            ),
            Err(SessionError::IdentityMismatch)
        ));
        sessions.abort(session_id);

        let resumed = begin_test_session(&mut sessions, session_id, &identity, "continue");
        assert_eq!(
            resumed.messages,
            [
                Message::user("secret"),
                Message::assistant("acknowledged"),
                Message::user("continue"),
            ]
        );
    }

    #[test]
    fn reserved_sessions_cannot_be_evicted_during_runtime_construction() {
        let mut sessions = SessionStore::default();
        let protected = "protected";
        sessions.reserve(protected).unwrap();

        for index in 1..MAX_SESSIONS {
            let id = format!("session-{index}");
            begin_test_session(&mut sessions, &id, &test_session_identity(1), "hello");
            sessions.complete(&id, "response".to_owned()).unwrap();
        }
        sessions.reserve("replacement").unwrap();

        let protected_state = sessions.sessions.get(protected).unwrap();
        assert!(protected_state.running);
        assert!(!protected_state.pending_user);
    }

    #[test]
    fn sessions_pin_the_first_compiled_runtime() {
        let mut sessions = SessionStore::default();
        let session_id = "0123456789abcdef0123456789abcdef";
        let identity = test_session_identity(1);
        let first_runtime = test_runtime();

        sessions.reserve(session_id).unwrap();
        sessions
            .begin(
                session_id,
                identity.clone(),
                Arc::clone(&first_runtime),
                "hello".to_owned(),
            )
            .unwrap();
        sessions
            .complete(session_id, "response".to_owned())
            .unwrap();
        sessions.reserve(session_id).unwrap();
        let next = sessions
            .begin(session_id, identity, test_runtime(), "continue".to_owned())
            .unwrap();

        assert!(Arc::ptr_eq(&next.runtime, &first_runtime));
    }

    #[tokio::test]
    async fn session_stream_rejects_blank_and_oversized_responses() {
        for (text, limit) in [("   ", 3), ("oversized", 4)] {
            let sessions = Arc::new(Mutex::new(SessionStore::default()));
            let session_id = "0123456789abcdef0123456789abcdef";
            begin_test_session(
                &mut sessions.lock().unwrap(),
                session_id,
                &test_session_identity(1),
                "hello",
            );
            let events: RunStream = Box::pin(stream::iter([
                RunEvent::Started,
                RunEvent::OutputTextDelta {
                    text: text.to_owned(),
                },
                RunEvent::Completed,
            ]));

            let emitted = track_session_run(
                events,
                SessionRun::new(Arc::clone(&sessions), session_id.to_owned()),
                limit,
            )
            .collect::<Vec<_>>()
            .await;
            assert!(matches!(emitted.last(), Some(RunEvent::Failed { .. })));

            let retry = begin_test_session(
                &mut sessions.lock().unwrap(),
                session_id,
                &test_session_identity(1),
                "retry",
            );
            assert_eq!(retry.messages, [Message::user("retry")]);
        }
    }

    fn begin_test_session(
        sessions: &mut SessionStore,
        id: &str,
        identity: &SessionIdentity,
        prompt: &str,
    ) -> SessionContext {
        sessions.reserve(id).unwrap();
        sessions
            .begin(id, identity.clone(), test_runtime(), prompt.to_owned())
            .unwrap()
    }

    fn test_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(EmptyProvider, "test-model", 128).unwrap())
    }

    fn test_session_identity(seed: u8) -> SessionIdentity {
        SessionIdentity {
            workspace: PathBuf::from(format!("/workspace-{seed}")),
            organization: Some(format!("organization-{seed}")),
            runtime_key: RuntimeKey([seed; 32]),
        }
    }
}
