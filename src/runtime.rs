//! Application configuration to model-runtime composition.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use qq_core::{
    LoadedRuntime, Runtime, RuntimeConfigError, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError,
    SessionRuntimeOptions,
};
use qq_protocol::{
    CommandRequest, ModelCatalogRequest, ModelDescriptor, RunFailureKind, SnapshotRequest,
    SubscribeRequest,
};
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler, ProviderError,
    ProviderRecipe, bedrock::BedrockAuth as ProviderBedrockAuth,
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
    server::{AskHandler, AskHandlerError, CommandFuture, ModelsFuture, SnapshotFuture},
};

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const OPENAI_CREDENTIAL_ENDPOINT: &str = "https://api.openai.com";
const OPENAI_STORED_CREDENTIAL: &str = "openai/default";
const OPENAI_ENVIRONMENT_CREDENTIAL: &str = "OPENAI_API_KEY";
const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_CREDENTIAL_ENDPOINT: &str = "https://api.anthropic.com";
const ANTHROPIC_STORED_CREDENTIAL: &str = "anthropic/default";
const ANTHROPIC_ENVIRONMENT_CREDENTIAL: &str = "ANTHROPIC_API_KEY";
const GOOGLE_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
const GOOGLE_CREDENTIAL_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
const GOOGLE_STORED_CREDENTIAL: &str = "google/default";
const GOOGLE_ENVIRONMENT_CREDENTIAL: &str = "GEMINI_API_KEY";
const MAX_CACHED_RUNTIMES: usize = 16;
const MAX_MODEL_OPTIONS: usize = 4_096;

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

    pub fn model_options(&self, snapshot: &ConfigSnapshot) -> Vec<ModelDescriptor> {
        let allowed = snapshot.policy().allowed_providers();
        let denied = snapshot.policy().denied_providers();
        let mut options = Vec::new();
        'providers: for (provider_id, provider) in snapshot.providers() {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|id| id == provider_id))
                || denied.iter().any(|id| id == provider_id)
                || !self.provider_authenticated(provider_id, provider)
            {
                continue;
            }
            for (model_id, metadata) in provider.models() {
                if options.len() >= MAX_MODEL_OPTIONS {
                    break 'providers;
                }
                options.push(ModelDescriptor {
                    provider: provider_id.clone(),
                    model: model_id.clone(),
                    name: metadata.name().map(str::to_owned),
                    context_window: metadata.context_window(),
                    selection: qq_protocol::ModelSelection {
                        model: Some(format!("{provider_id}/{model_id}")),
                        max_output_tokens: Some(
                            metadata
                                .max_output_tokens()
                                .map_or(snapshot.max_output_tokens(), |limit| {
                                    limit.min(snapshot.max_output_tokens())
                                }),
                        ),
                        organization: snapshot.organization().map(str::to_owned),
                    },
                });
            }
        }
        if options.len() < MAX_MODEL_OPTIONS
            && !options
                .iter()
                .any(|option| option.selection.model.as_deref() == Some(snapshot.model().as_str()))
            && let Some(provider) = snapshot.providers().get(snapshot.model().provider())
            && self.provider_authenticated(snapshot.model().provider(), provider)
        {
            let metadata = provider.models().get(snapshot.model().model());
            options.push(ModelDescriptor {
                provider: snapshot.model().provider().to_owned(),
                model: snapshot.model().model().to_owned(),
                name: None,
                context_window: metadata.and_then(|metadata| metadata.context_window()),
                selection: qq_protocol::ModelSelection {
                    model: Some(snapshot.model().as_str().to_owned()),
                    max_output_tokens: Some(snapshot.max_output_tokens()),
                    organization: snapshot.organization().map(str::to_owned),
                },
            });
        }
        options.sort_by(|left, right| {
            (&left.provider, &left.name, &left.model).cmp(&(
                &right.provider,
                &right.name,
                &right.model,
            ))
        });
        options
    }

    pub fn models_for(
        &self,
        request: &ModelCatalogRequest,
    ) -> Result<Vec<ModelDescriptor>, RuntimeBuildError> {
        let requested_workspace = PathBuf::from(&request.workspace);
        let workspace = std::fs::canonicalize(&requested_workspace).map_err(|_| {
            ConfigError::InvalidWorkingDirectory {
                path: requested_workspace.clone(),
            }
        })?;
        if workspace != requested_workspace {
            return Err(ConfigError::InvalidWorkingDirectory {
                path: requested_workspace,
            }
            .into());
        }
        let load = LoadRequest::from_process_env(&workspace, None)?;
        match self.load(&load) {
            Ok(snapshot) => return Ok(self.model_options(&snapshot)),
            Err(RuntimeBuildError::Config(ConfigError::ModelRequired)) => {}
            Err(error) => return Err(error),
        }
        let mut load =
            LoadRequest::from_process_env(&workspace, request.selection.max_output_tokens)?;
        let mut overrides = load.overrides().clone();
        if let Some(model) = &request.selection.model {
            overrides = overrides.with_model(model.clone());
        }
        if let Some(organization) = &request.selection.organization {
            overrides = overrides.with_organization(organization.clone());
        }
        load = load.with_overrides(overrides);
        let snapshot = self.load(&load)?;
        Ok(self.model_options(&snapshot))
    }

    fn provider_authenticated(&self, _provider_id: &str, provider: &ProviderConfig) -> bool {
        match provider {
            ProviderConfig::OpenAi { api_key, .. } => resolve_provider_credential(
                &self.inner.credentials,
                api_key.as_ref(),
                OPENAI_STORED_CREDENTIAL,
                OPENAI_ENVIRONMENT_CREDENTIAL,
                Some(OPENAI_CREDENTIAL_ENDPOINT),
            )
            .is_ok(),
            ProviderConfig::Anthropic { api_key, .. } => resolve_provider_credential(
                &self.inner.credentials,
                api_key.as_ref(),
                ANTHROPIC_STORED_CREDENTIAL,
                ANTHROPIC_ENVIRONMENT_CREDENTIAL,
                Some(ANTHROPIC_CREDENTIAL_ENDPOINT),
            )
            .is_ok(),
            ProviderConfig::Google { api_key, .. } => resolve_provider_credential(
                &self.inner.credentials,
                api_key.as_ref(),
                GOOGLE_STORED_CREDENTIAL,
                GOOGLE_ENVIRONMENT_CREDENTIAL,
                Some(GOOGLE_CREDENTIAL_ENDPOINT),
            )
            .is_ok(),
            ProviderConfig::OpenAiCodex { profile, .. } => self
                .inner
                .credentials
                .resolve_with_endpoint(
                    &crate::config::SecretRef::Stored(format!(
                        "openai-codex/{}",
                        profile.as_deref().unwrap_or("default")
                    )),
                    Some("https://chatgpt.com"),
                )
                .is_ok(),
            ProviderConfig::AmazonBedrock { auth, .. }
            | ProviderConfig::AmazonBedrockMantle { auth, .. } => match auth {
                BedrockAuth::ApiKey(reference) => self.inner.credentials.resolve(reference).is_ok(),
                BedrockAuth::Aws(AwsAuth::Profile(profile)) => aws_profile_configured(profile),
                BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                    (std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
                        && std::env::var_os("AWS_SECRET_ACCESS_KEY").is_some())
                        || std::env::var_os("AWS_PROFILE")
                            .and_then(|profile| profile.into_string().ok())
                            .is_some_and(|profile| aws_profile_configured(&profile))
                        || (std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
                            && std::env::var_os("AWS_ROLE_ARN").is_some())
                        || std::env::var_os("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                        || std::env::var_os("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
                }
            },
            ProviderConfig::LiteLlm { connection, .. }
            | ProviderConfig::Custom { connection, .. } => connection.is_some(),
        }
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
            ProviderConfig::Google { api_key, .. } => {
                let secret = resolve_provider_credential(
                    &self.inner.credentials,
                    api_key.as_ref(),
                    GOOGLE_STORED_CREDENTIAL,
                    GOOGLE_ENVIRONMENT_CREDENTIAL,
                    Some(GOOGLE_CREDENTIAL_ENDPOINT),
                )?;
                let auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    GOOGLE_ENDPOINT,
                    "base",
                    "google_generate_content",
                    &auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                let recipe = ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::base(GOOGLE_ENDPOINT, false),
                    HttpProtocol::GoogleGenerateContent,
                    auth.into_http()?,
                ));
                Ok((recipe, key))
            }
            ProviderConfig::AmazonBedrock { region, auth, .. } => {
                self.prepare_bedrock_provider(provider_id, region.as_deref(), auth)
            }
            ProviderConfig::AmazonBedrockMantle {
                region, api, auth, ..
            } => self.prepare_bedrock_mantle_provider(provider_id, region.as_deref(), *api, auth),
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
            ProviderApi::GoogleGenerateContent => HttpProtocol::GoogleGenerateContent,
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

    fn prepare_bedrock_mantle_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        api: ProviderApi,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let protocol = match api {
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
        let credential_endpoint =
            region.map(|region| format!("https://bedrock-mantle.{region}.api.aws"));
        let key_endpoint = credential_endpoint
            .as_deref()
            .unwrap_or("aws-region-provider-chain");
        let api_name = provider_api_name(api);

        let (auth, key) = match auth {
            BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    api_name,
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
                    api_name,
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
                    api_name,
                    &key_auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                (ProviderBedrockAuth::ApiKey(api_key), key)
            }
        };

        Ok((
            ProviderRecipe::amazon_bedrock_mantle(region.map(str::to_owned), protocol, auth),
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

fn aws_profile_configured(profile: &str) -> bool {
    if profile.is_empty() {
        return false;
    }
    let home = directories::BaseDirs::new().map(|directories| directories.home_dir().to_owned());
    let files = [
        std::env::var_os("AWS_CONFIG_FILE")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".aws/config"))),
        std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".aws/credentials"))),
    ];
    let config_header = format!("[profile {profile}]");
    let credentials_header = format!("[{profile}]");
    files.into_iter().flatten().any(|path| {
        std::fs::read_to_string(path).is_ok_and(|content| {
            content.lines().any(|line| {
                let line = line.trim();
                line == config_header || line == credentials_header
            })
        })
    })
}

impl RuntimeLoader for RuntimeFactory {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let factory = self.clone();
        Box::pin(async move {
            let build = tokio::task::spawn_blocking(move || {
                let requested_workspace = PathBuf::from(&request.workspace);
                let workspace = std::fs::canonicalize(&requested_workspace).map_err(|_| {
                    ConfigError::InvalidWorkingDirectory {
                        path: requested_workspace.clone(),
                    }
                })?;
                if workspace != requested_workspace {
                    return Err(ConfigError::InvalidWorkingDirectory {
                        path: requested_workspace,
                    }
                    .into());
                }
                let mut load =
                    LoadRequest::from_process_env(&workspace, request.model.max_output_tokens)?;
                let mut overrides = load.overrides().clone();
                if let Some(model) = request.model.model {
                    overrides = overrides.with_model(model);
                }
                if let Some(organization) = request.model.organization {
                    overrides = overrides.with_organization(organization);
                }
                load = load.with_overrides(overrides);
                let snapshot = factory.load(&load)?;
                let pricing = snapshot
                    .providers()
                    .get(snapshot.model().provider())
                    .and_then(|provider| provider.models().get(snapshot.model().model()))
                    .and_then(|metadata| metadata.pricing())
                    .cloned();
                let runtime = factory.runtime_for_snapshot(&snapshot)?;
                Ok::<_, RuntimeBuildError>(LoadedRuntime { runtime, pricing })
            })
            .await;
            match build {
                Ok(Ok(runtime)) => Ok(runtime),
                Ok(Err(error)) => Err(RuntimeLoadError {
                    kind: error.failure_kind(),
                    message: error.to_string(),
                }),
                Err(_) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "runtime construction stopped unexpectedly".to_owned(),
                }),
            }
        })
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
    durable: SessionRuntime,
    factory: RuntimeFactory,
}

impl RuntimeHandler {
    pub async fn open(factory: RuntimeFactory) -> Result<Self, RuntimeHandlerError> {
        let database_path = factory.inner.config.session_database_path()?;
        let durable = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(factory.clone()),
        )
        .await?;
        Ok(Self { durable, factory })
    }
}

impl AskHandler for RuntimeHandler {
    fn command(&self, request: CommandRequest) -> CommandFuture {
        let runtime = self.durable.clone();
        Box::pin(async move {
            runtime
                .command(request.command_id, request.command)
                .await
                .map_err(map_session_runtime_error)
        })
    }

    fn snapshot(&self, request: SnapshotRequest) -> SnapshotFuture {
        let runtime = self.durable.clone();
        Box::pin(async move {
            runtime
                .snapshot(request)
                .await
                .map_err(map_session_runtime_error)
        })
    }

    fn models(&self, request: ModelCatalogRequest) -> ModelsFuture {
        let factory = self.factory.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || factory.models_for(&request))
                .await
                .map_err(|_| AskHandlerError::Internal)?
                .map_err(|error| match error.failure_kind() {
                    RunFailureKind::Configuration | RunFailureKind::Policy => {
                        AskHandlerError::InvalidRequest
                    }
                    _ => AskHandlerError::Internal,
                })
        })
    }

    fn subscribe(&self, request: SubscribeRequest) -> Result<SessionEventStream, AskHandlerError> {
        self.durable
            .subscribe(request)
            .map_err(map_session_runtime_error)
    }
}

fn map_session_runtime_error(error: SessionRuntimeError) -> AskHandlerError {
    match error {
        SessionRuntimeError::EmptyWorkspace
        | SessionRuntimeError::InvalidWorkspace
        | SessionRuntimeError::EmptyPrompt
        | SessionRuntimeError::PromptTooLarge
        | SessionRuntimeError::WorkspaceNotFound
        | SessionRuntimeError::SessionNotFound
        | SessionRuntimeError::ParentWorkspaceMismatch
        | SessionRuntimeError::RunNotFound
        | SessionRuntimeError::ContextTooLarge
        | SessionRuntimeError::EventTooLarge
        | SessionRuntimeError::InvalidModelSelection
        | SessionRuntimeError::IdempotencyConflict
        | SessionRuntimeError::CursorStoreMismatch
        | SessionRuntimeError::CursorWorkspaceMismatch
        | SessionRuntimeError::InvalidPageLimit => AskHandlerError::InvalidRequest,
        SessionRuntimeError::QueueFull
        | SessionRuntimeError::WorkspaceLimitReached
        | SessionRuntimeError::SessionLimitReached
        | SessionRuntimeError::CommandLimitReached
        | SessionRuntimeError::Overloaded => AskHandlerError::Unavailable,
        SessionRuntimeError::InvalidRunLimit
        | SessionRuntimeError::OutputTooLarge
        | SessionRuntimeError::Unavailable
        | SessionRuntimeError::Persistence => AskHandlerError::Internal,
    }
}

#[derive(Debug, Error)]
pub enum RuntimeHandlerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sessions(#[from] SessionRuntimeError),
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

    use super::*;
    use crate::{
        auth::{CredentialPaths, KeyringBackend, KeyringError},
        config::{ConfigPaths, RuntimeOverrides},
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct MemoryKeyring(Mutex<BTreeMap<String, Vec<u8>>>);

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
    fn catalog_hides_builtin_models_until_the_provider_is_authenticated() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        let factory = fixture.factory_with_credentials(credentials.clone());
        let snapshot = factory
            .load(&fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#))
            .unwrap();

        assert!(factory.model_options(&snapshot).is_empty());

        credentials
            .set("openai/default", "test-secret", false)
            .unwrap();
        let options = factory.model_options(&snapshot);
        assert!(!options.is_empty());
        assert!(options.iter().all(|option| option.provider == "openai"));
        assert!(options.iter().any(|option| option.model == "gpt-5.6"));
        assert!(
            options
                .iter()
                .find(|option| option.model == "gpt-5.6")
                .and_then(|option| option.context_window)
                .is_some()
        );
    }

    #[test]
    fn constructs_every_wired_http_api_and_builtin_key_provider() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for api in [
            "OpenAiResponses",
            "OpenAiChatCompletions",
            "AnthropicMessages",
            "GoogleGenerateContent",
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

        let google = fixture.request(
            r#"(
                version: 1,
                model: "google/gemini-test",
                providers: {
                    "google": Google(
                        api_key: Value("google-test-secret"),
                        models: {"gemini-test": (name: "Gemini test")},
                    ),
                },
            )"#,
        );
        factory.runtime_for(&google).unwrap();
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
    fn constructs_amazon_bedrock_mantle_runtimes_for_supported_apis_and_auth_modes() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for api in [
            "OpenAiResponses",
            "OpenAiChatCompletions",
            "AnthropicMessages",
        ] {
            for (auth_name, auth) in [
                ("default", "Aws(DefaultChain)"),
                ("profile", r#"Aws(Profile("work"))"#),
                ("api-key", r#"ApiKey(Value("mantle-test-secret"))"#),
            ] {
                let provider = format!("mantle-{api}-{auth_name}");
                let request = fixture.request(format!(
                    r#"(
                        version: 1,
                        model: "{provider}/test-model",
                        providers: {{
                            "{provider}": AmazonBedrockMantle(
                                region: "us-east-1",
                                api: {api},
                                auth: {auth},
                                models: {{"test-model": (name: "Test model")}},
                            ),
                        }},
                    )"#
                ));

                factory.runtime_for(&request).unwrap_or_else(|error| {
                    panic!("failed to construct Mantle {api}/{auth_name}: {error}")
                });
            }
        }
    }

    #[test]
    fn rejects_unsupported_amazon_bedrock_mantle_apis_before_network_access() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for (api, expected) in [
            ("GoogleGenerateContent", ProviderApi::GoogleGenerateContent),
            ("BedrockConverse", ProviderApi::BedrockConverse),
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "us-east-1",
                            api: {api},
                            auth: Aws(DefaultChain),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));

            let error = factory
                .runtime_for(&request)
                .err()
                .expect("unsupported Mantle API must fail");
            assert!(matches!(
                error,
                RuntimeBuildError::UnsupportedApi { api: actual, .. }
                    if actual == expected
            ));
        }
    }

    #[test]
    fn mantle_runtime_cache_identity_includes_region_api_and_aws_profile() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |region: &str, api: &str, auth: &str| {
            fixture.request(format!(
                r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "{region}",
                            api: {api},
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ))
        };

        let base = document("us-east-1", "OpenAiResponses", "Aws(DefaultChain)");
        let first = factory.runtime_for(&base).unwrap();
        let reused = factory.runtime_for(&base).unwrap();
        let different_region = factory
            .runtime_for(&document(
                "us-west-2",
                "OpenAiResponses",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_api = factory
            .runtime_for(&document(
                "us-east-1",
                "AnthropicMessages",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_profile = factory
            .runtime_for(&document(
                "us-east-1",
                "OpenAiResponses",
                r#"Aws(Profile("work"))"#,
            ))
            .unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &different_region));
        assert!(!Arc::ptr_eq(&first, &different_api));
        assert!(!Arc::ptr_eq(&first, &different_profile));
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
}
