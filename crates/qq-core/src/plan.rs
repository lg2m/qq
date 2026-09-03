//! Compile-once execution plans.
//!
//! A [`CompiledAgentPlan`] is everything about an agent's behavior that does
//! not depend on the prompt: the compiled provider handle, the resolved model,
//! the open workspace with its instructions, the static tool catalog, the
//! sub-agent routes, and the retry policy. Compiling it does the filesystem
//! and provider work once; the run loop then executes directly from shared
//! immutable data. Its [`AgentPlanDescriptor`] is the secret-free account of
//! that behavior whose canonical digest identifies the plan for caching,
//! traces, and later protocol exposure.

mod descriptor;
mod fingerprint;

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use qq_protocol::{AgentPlanDigest, CredentialEpoch, InstructionHash, ResolvedModel};
use qq_provider::{Provider, ToolSpec};
use thiserror::Error;

pub use descriptor::{
    AgentPlanDescriptor, CredentialReference, DESCRIPTOR_VERSION, McpServerDescriptor,
    McpTransportKind, ProviderDescriptor, RetryPolicyDescriptor, ToolCatalogDescriptor,
};
pub use fingerprint::SourceFingerprint;

use crate::{
    McpRegistry, Runtime, RuntimeConfigError, TurnRetryPolicy,
    runtime::{search_history_spec, tool_schema_measurement},
    tools,
    workspace::{Workspace, WorkspaceInstructionError, WorkspaceInstructions},
};

/// Typed, application-neutral input to plan compilation. The embedding
/// application translates its configuration into this shape; core never sees
/// configuration documents, secret values, or credential stores.
pub struct AgentProfile {
    provider: Arc<dyn Provider>,
    provider_descriptor: ProviderDescriptor,
    resolved_model: ResolvedModel,
    workspace: PathBuf,
    mcp: Option<(Arc<dyn McpRegistry>, Vec<McpServerDescriptor>)>,
    spawn_model_routes: Vec<String>,
    turn_retry: TurnRetryPolicy,
    adapter_build: String,
    provenance: Vec<String>,
    credential_epoch: CredentialEpoch,
}

impl AgentProfile {
    /// Starts a profile for a compiled provider. `workspace` must already be
    /// the canonical absolute path the run will execute in.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        provider_descriptor: ProviderDescriptor,
        resolved_model: ResolvedModel,
        workspace: PathBuf,
    ) -> Self {
        Self {
            provider,
            provider_descriptor,
            resolved_model,
            workspace,
            mcp: None,
            spawn_model_routes: Vec::new(),
            turn_retry: TurnRetryPolicy::default(),
            adapter_build: qq_provider::BUILD_IDENTITY.to_owned(),
            provenance: Vec::new(),
            credential_epoch: CredentialEpoch::NONE,
        }
    }

    /// A profile for an embedded runtime built without configuration: the
    /// descriptor records the provider as embedded and takes model identity
    /// from the runtime. Used by direct `Runtime::run*` entry points and tests.
    #[must_use]
    pub fn embedded(runtime: &Runtime, workspace: PathBuf) -> Self {
        Self {
            provider: Arc::clone(&runtime.provider),
            provider_descriptor: ProviderDescriptor::embedded(),
            resolved_model: runtime.embedded_resolved_model(),
            workspace,
            mcp: runtime.mcp.clone().map(|registry| (registry, Vec::new())),
            spawn_model_routes: runtime.spawn_model_routes.to_vec(),
            turn_retry: runtime.turn_retry,
            adapter_build: qq_provider::BUILD_IDENTITY.to_owned(),
            provenance: Vec::new(),
            credential_epoch: CredentialEpoch::NONE,
        }
    }

    /// Attaches configuration-declared MCP servers with their secret-free
    /// descriptors. The registry's tool declarations still join each run when
    /// it starts; only the declaration identity enters the plan digest.
    #[must_use]
    pub fn with_mcp(
        mut self,
        registry: Arc<dyn McpRegistry>,
        servers: Vec<McpServerDescriptor>,
    ) -> Self {
        self.mcp = Some((registry, servers));
        self
    }

    /// Restricts model-visible sub-agent overrides to these canonical routes.
    #[must_use]
    pub fn with_spawn_model_routes(mut self, routes: Vec<String>) -> Self {
        self.spawn_model_routes = routes;
        self
    }

    #[must_use]
    pub fn with_turn_retry_policy(mut self, policy: TurnRetryPolicy) -> Self {
        self.turn_retry = policy;
        self
    }

    /// Human-readable, secret-free labels of the configuration layers that
    /// produced this profile, in application order. They participate in the
    /// digest so a plan compiled from a different source set is distinct.
    #[must_use]
    pub fn with_provenance(mut self, sources: Vec<String>) -> Self {
        self.provenance = sources;
        self
    }

    /// The credential epoch the provider handle was authorized against. It is
    /// carried beside the plan for rotation diagnosis and never enters the
    /// behavioral digest.
    #[must_use]
    pub fn with_credential_epoch(mut self, epoch: CredentialEpoch) -> Self {
        self.credential_epoch = epoch;
        self
    }
}

/// Why a profile could not be compiled into a plan.
#[derive(Debug, Error)]
pub enum PlanCompileError {
    #[error(transparent)]
    Runtime(#[from] RuntimeConfigError),
    #[error("workspace path must be absolute and canonical: {path}")]
    NonCanonicalWorkspace { path: PathBuf },
    #[error("could not open workspace {path}: {source}")]
    OpenWorkspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Instructions(#[from] WorkspaceInstructionError),
    #[error(
        "resolved model {route} does not match the runtime: model {runtime_model}, output limit \
         {runtime_max_output_tokens}, context window {runtime_context_window:?}"
    )]
    ModelMismatch {
        route: String,
        runtime_model: String,
        runtime_max_output_tokens: u32,
        runtime_context_window: Option<u32>,
    },
    #[error("descriptor could not be encoded canonically: {message}")]
    Encode { message: String },
}

/// The immutable live plan one or more runs execute from. It is never
/// serialized: its [`descriptor`](Self::descriptor) is the durable, secret-free
/// account, and the [`digest`](Self::digest) is its identity.
pub struct CompiledAgentPlan {
    pub(crate) runtime: Runtime,
    pub(crate) workspace: Workspace,
    pub(crate) instructions: WorkspaceInstructions,
    /// Built-in tools plus the `spawn_agent` and `search_history`
    /// declarations, in declaration order. Runs select from this slice by
    /// capability instead of rebuilding schemas.
    pub(crate) static_tools: Arc<[ToolSpec]>,
    spawn_agent_index: usize,
    search_history_index: usize,
    resolved_model: Arc<ResolvedModel>,
    descriptor: Arc<AgentPlanDescriptor>,
    digest: AgentPlanDigest,
    credential_epoch: CredentialEpoch,
    instruction_sources: Vec<SourceFingerprint>,
    estimated_bytes: usize,
}

impl fmt::Debug for CompiledAgentPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledAgentPlan")
            .field("digest", &self.digest)
            .field("credential_epoch", &self.credential_epoch)
            .field("route", &self.resolved_model.route)
            .field("workspace", &self.workspace.path())
            .finish_non_exhaustive()
    }
}

impl CompiledAgentPlan {
    /// Compiles a profile. This opens the workspace, reads its instruction
    /// file, builds every static tool declaration, and encodes the
    /// descriptor. It performs blocking filesystem work and must run off the
    /// async executor (`spawn_blocking` or a dedicated thread).
    pub fn compile_blocking(profile: AgentProfile) -> Result<Arc<Self>, PlanCompileError> {
        let AgentProfile {
            provider,
            provider_descriptor,
            resolved_model,
            workspace,
            mcp,
            spawn_model_routes,
            turn_retry,
            adapter_build,
            provenance,
            credential_epoch,
        } = profile;
        let mut runtime = Runtime::with_provider(
            provider,
            resolved_model.provider_model.clone(),
            resolved_model.max_output_tokens,
        )?
        .with_context_window(resolved_model.context_window)
        .with_turn_retry_policy(turn_retry)
        .with_spawn_model_routes(spawn_model_routes);
        let (mcp_servers, config_grants) = match mcp {
            Some((registry, servers)) => {
                let grants = registry.config_grants();
                runtime = runtime.with_mcp_registry(registry);
                (servers, grants)
            }
            None => (Vec::new(), Vec::new()),
        };
        if !is_canonical_absolute(&workspace) {
            return Err(PlanCompileError::NonCanonicalWorkspace { path: workspace });
        }
        let opened =
            Workspace::open(&workspace).map_err(|source| PlanCompileError::OpenWorkspace {
                path: workspace.clone(),
                source,
            })?;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (instructions, instruction_sources) =
            crate::workspace::load_instructions_with_sources(&opened, &cancelled)?;

        let mut static_tools = tools::specs();
        let spawn_agent_index = static_tools.len();
        static_tools.push(tools::spawn_agent_spec(&runtime.spawn_model_routes));
        let search_history_index = static_tools.len();
        static_tools.push(search_history_spec());
        let static_schema = tool_schema_measurement(&static_tools);

        let descriptor = AgentPlanDescriptor {
            version: DESCRIPTOR_VERSION,
            adapter_build,
            provider: provider_descriptor,
            model: resolved_model.clone(),
            workspace: workspace.display().to_string(),
            prompt_version: crate::runtime::AGENT_PROMPT_VERSION,
            instruction_hash: instructions.hash(),
            instruction_source: instructions.source_path().map(str::to_owned),
            tools: ToolCatalogDescriptor {
                static_schema_hash: static_schema.hash,
                names: static_tools
                    .iter()
                    .map(|spec| spec.name().to_owned())
                    .collect(),
                spawn_model_routes: runtime.spawn_model_routes.to_vec(),
                config_grants,
            },
            mcp_servers,
            retry: RetryPolicyDescriptor::from(runtime.turn_retry),
            provenance,
        };
        let digest = descriptor.digest()?;
        let estimated_bytes = descriptor.canonical_bytes()?.len()
            + instructions.content_len()
            + usize::try_from(static_schema.bytes).unwrap_or(usize::MAX)
            + std::mem::size_of::<Self>();
        Ok(Arc::new(Self {
            runtime,
            workspace: opened,
            instructions,
            static_tools: static_tools.into(),
            spawn_agent_index,
            search_history_index,
            resolved_model: Arc::new(resolved_model),
            descriptor: Arc::new(descriptor),
            digest,
            credential_epoch,
            instruction_sources,
            estimated_bytes,
        }))
    }

    #[must_use]
    pub fn descriptor(&self) -> &Arc<AgentPlanDescriptor> {
        &self.descriptor
    }

    #[must_use]
    pub const fn digest(&self) -> AgentPlanDigest {
        self.digest
    }

    #[must_use]
    pub const fn credential_epoch(&self) -> CredentialEpoch {
        self.credential_epoch
    }

    #[must_use]
    pub fn resolved_model(&self) -> &Arc<ResolvedModel> {
        &self.resolved_model
    }

    #[must_use]
    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    #[must_use]
    pub fn instruction_hash(&self) -> InstructionHash {
        self.instructions.hash()
    }

    /// The instruction files this plan read or found absent, for callers that
    /// revalidate a cached plan against the filesystem.
    #[must_use]
    pub fn instruction_sources(&self) -> &[SourceFingerprint] {
        &self.instruction_sources
    }

    /// A conservative accounting of the heap this plan holds, for cache
    /// admission. It excludes the provider transport and MCP connections,
    /// which are shared across generations.
    #[must_use]
    pub const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    #[must_use]
    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub(crate) fn spawn_agent_spec(&self) -> &ToolSpec {
        &self.static_tools[self.spawn_agent_index]
    }

    pub(crate) fn search_history_spec(&self) -> &ToolSpec {
        &self.static_tools[self.search_history_index]
    }

    pub(crate) fn built_in_specs(&self) -> &[ToolSpec] {
        &self.static_tools[..self.spawn_agent_index]
    }
}

fn is_canonical_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::stream;
    use qq_protocol::{
        CapabilitySupport, GenerationCapabilities, PromptCacheCapabilities, PromptVersion,
        ProviderRequestShapeIdentity, ProviderRequestShapeVersion, ResolvedModelVersion,
    };
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

    use super::*;

    struct SilentProvider;

    impl Provider for SilentProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
        }
    }

    fn canonical_temp() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            std::fs::canonicalize(directory.path()).unwrap(),
            directory.path(),
            "test temp dir must be canonical"
        );
        directory
    }

    fn resolved_model() -> ResolvedModel {
        ResolvedModel {
            version: ResolvedModelVersion::new(2).unwrap(),
            request_shape: Some(ProviderRequestShapeIdentity {
                version: ProviderRequestShapeVersion::new(1).unwrap(),
                digest: qq_protocol::ContentHash::from_bytes([7; 32]),
            }),
            route: "custom/test-model".to_owned(),
            provider_model: "test-model".to_owned(),
            organization: None,
            credential_profile: None,
            max_output_tokens: 256,
            context_window: Some(8_192),
            pricing: None,
            output_token_control: CapabilitySupport::Native,
            generation: GenerationCapabilities {
                reasoning_effort: CapabilitySupport::Unsupported,
            },
            prompt_cache: PromptCacheCapabilities {
                control: CapabilitySupport::Unsupported,
                cache_read_usage: false,
                cache_write_usage: false,
            },
        }
    }

    fn provider_descriptor() -> ProviderDescriptor {
        ProviderDescriptor {
            id: "custom".to_owned(),
            api: "openai_responses".to_owned(),
            endpoint: Some("https://api.example.test/v1".to_owned()),
            endpoint_mode: Some("base".to_owned()),
            auth_scheme: "bearer".to_owned(),
            credential: CredentialReference::Environment("CUSTOM_TOKEN".to_owned()),
            header_names: Vec::new(),
            region: None,
        }
    }

    fn profile(workspace: &Path) -> AgentProfile {
        AgentProfile::new(
            Arc::new(SilentProvider),
            provider_descriptor(),
            resolved_model(),
            workspace.to_owned(),
        )
    }

    /// A descriptor with fixed inputs, independent of the filesystem, so the
    /// golden digest below is stable across machines.
    fn golden_descriptor() -> AgentPlanDescriptor {
        AgentPlanDescriptor {
            version: DESCRIPTOR_VERSION,
            adapter_build: "qq-provider/0.1.0+bedrock".to_owned(),
            provider: provider_descriptor(),
            model: resolved_model(),
            workspace: "/work".to_owned(),
            prompt_version: crate::runtime::AGENT_PROMPT_VERSION,
            instruction_hash: InstructionHash::from_bytes([1; 32]),
            instruction_source: Some("AGENTS.md".to_owned()),
            tools: ToolCatalogDescriptor {
                static_schema_hash: qq_protocol::ContentHash::from_bytes([2; 32]),
                names: vec!["read_file".to_owned(), "spawn_agent".to_owned()],
                spawn_model_routes: vec!["custom/worker".to_owned()],
                config_grants: Vec::new(),
            },
            mcp_servers: vec![McpServerDescriptor {
                name: "executor".to_owned(),
                transport: McpTransportKind::Stdio,
                target: "executor".to_owned(),
                args: vec!["mcp".to_owned()],
                env: Vec::new(),
                credential: CredentialReference::None,
                eager: true,
                allow: vec!["execute".to_owned()],
                call_timeout_seconds: 60,
                max_concurrent_calls: 4,
            }],
            retry: RetryPolicyDescriptor::from(TurnRetryPolicy::default()),
            provenance: vec!["compiled defaults".to_owned()],
        }
    }

    #[test]
    fn canonical_encoding_and_digest_are_stable() {
        let descriptor = golden_descriptor();
        let bytes = descriptor.canonical_bytes().unwrap();
        assert!(bytes.starts_with(b"qq-agent-plan-descriptor-v1\0{\"version\":1,"));
        // The golden digest pins the canonical encoding. A change here means
        // DESCRIPTOR_VERSION must be bumped and every recorded digest is
        // from a different encoding.
        assert_eq!(
            descriptor.digest().unwrap().to_string(),
            "b04a4fbe97c15db302c2a6d002180ea4aad7b7612eb7686086263d667d2be069"
        );
        let round_trip: AgentPlanDescriptor =
            serde_json::from_slice(&bytes[b"qq-agent-plan-descriptor-v1\0".len()..]).unwrap();
        assert_eq!(round_trip, descriptor);
        assert_eq!(round_trip.digest().unwrap(), descriptor.digest().unwrap());
    }

    #[test]
    fn every_behavior_affecting_field_changes_the_digest() {
        let base = golden_descriptor();
        let base_digest = base.digest().unwrap();
        type Mutation = Box<dyn Fn(&mut AgentPlanDescriptor)>;
        let variants: Vec<(&str, Mutation)> = vec![
            ("adapter_build", Box::new(|d| d.adapter_build.push('x'))),
            ("provider.id", Box::new(|d| d.provider.id.push('x'))),
            ("provider.api", Box::new(|d| d.provider.api.push('x'))),
            (
                "provider.endpoint",
                Box::new(|d| d.provider.endpoint = None),
            ),
            (
                "provider.endpoint_mode",
                Box::new(|d| {
                    d.provider.endpoint_mode = Some("exact".to_owned());
                }),
            ),
            (
                "provider.auth_scheme",
                Box::new(|d| d.provider.auth_scheme.push('x')),
            ),
            (
                "provider.credential",
                Box::new(|d| {
                    d.provider.credential = CredentialReference::Inline;
                }),
            ),
            (
                "provider.header_names",
                Box::new(|d| {
                    d.provider.header_names.push("X-Extra".to_owned());
                }),
            ),
            (
                "provider.region",
                Box::new(|d| d.provider.region = Some("us-east-1".to_owned())),
            ),
            (
                "model.provider_model",
                Box::new(|d| d.model.provider_model.push('x')),
            ),
            (
                "model.max_output_tokens",
                Box::new(|d| d.model.max_output_tokens += 1),
            ),
            (
                "model.context_window",
                Box::new(|d| d.model.context_window = None),
            ),
            (
                "model.request_shape",
                Box::new(|d| d.model.request_shape = None),
            ),
            ("workspace", Box::new(|d| d.workspace.push('x'))),
            (
                "prompt_version",
                Box::new(|d| {
                    d.prompt_version = PromptVersion::new(1).unwrap();
                }),
            ),
            (
                "instruction_hash",
                Box::new(|d| {
                    d.instruction_hash = InstructionHash::from_bytes([9; 32]);
                }),
            ),
            (
                "instruction_source",
                Box::new(|d| d.instruction_source = None),
            ),
            (
                "tools.static_schema_hash",
                Box::new(|d| {
                    d.tools.static_schema_hash = qq_protocol::ContentHash::from_bytes([9; 32]);
                }),
            ),
            (
                "tools.names",
                Box::new(|d| d.tools.names.push("shell".to_owned())),
            ),
            (
                "tools.spawn_model_routes",
                Box::new(|d| d.tools.spawn_model_routes.clear()),
            ),
            (
                "tools.config_grants",
                Box::new(|d| {
                    d.tools
                        .config_grants
                        .push("mcp__executor__execute".to_owned());
                }),
            ),
            ("mcp_servers", Box::new(|d| d.mcp_servers.clear())),
            (
                "mcp_servers.allow",
                Box::new(|d| d.mcp_servers[0].allow.clear()),
            ),
            (
                "mcp_servers.credential",
                Box::new(|d| {
                    d.mcp_servers[0].credential = CredentialReference::Stored("tok".to_owned());
                }),
            ),
            (
                "retry.max_attempts",
                Box::new(|d| d.retry.max_attempts += 1),
            ),
            (
                "provenance",
                Box::new(|d| d.provenance.push("extra".to_owned())),
            ),
        ];
        for (name, mutate) in variants {
            let mut variant = golden_descriptor();
            mutate(&mut variant);
            assert_ne!(
                variant.digest().unwrap(),
                base_digest,
                "changing {name} must change the digest"
            );
        }
    }

    #[test]
    fn compiled_plan_reads_instructions_once_and_records_secret_free_identity() {
        let directory = canonical_temp();
        std::fs::write(directory.path().join("AGENTS.md"), "Answer tersely.\n").unwrap();
        let plan = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()])
                .with_credential_epoch(CredentialEpoch::new(3)),
        )
        .unwrap();

        let descriptor = plan.descriptor();
        assert_eq!(descriptor.version, DESCRIPTOR_VERSION);
        assert_eq!(descriptor.adapter_build, qq_provider::BUILD_IDENTITY);
        assert_eq!(descriptor.workspace, directory.path().display().to_string());
        assert_eq!(descriptor.instruction_source.as_deref(), Some("AGENTS.md"));
        assert_eq!(descriptor.instruction_hash, plan.instruction_hash());
        assert_eq!(
            descriptor.tools.spawn_model_routes,
            vec!["custom/worker".to_owned()]
        );
        assert!(
            descriptor
                .tools
                .names
                .ends_with(&["spawn_agent".to_owned(), "search_history".to_owned()])
        );
        assert_eq!(plan.credential_epoch(), CredentialEpoch::new(3));
        assert_eq!(plan.digest(), descriptor.digest().unwrap());
        assert_eq!(plan.resolved_model().route, "custom/test-model");
        assert!(plan.estimated_bytes() > "Answer tersely.\n".len());

        // Both instruction candidates are fingerprinted, present or absent.
        let sources = plan.instruction_sources();
        assert_eq!(sources.len(), 2);
        assert!(sources[0].path().ends_with("AGENTS.md") && sources[0].is_present());
        assert!(sources[1].path().ends_with("CLAUDE.md") && !sources[1].is_present());

        // The epoch is not part of behavioral identity.
        let other_epoch = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()])
                .with_credential_epoch(CredentialEpoch::new(4)),
        )
        .unwrap();
        assert_eq!(other_epoch.digest(), plan.digest());

        // Changing the instructions changes the digest.
        std::fs::write(directory.path().join("AGENTS.md"), "Answer verbosely.\n").unwrap();
        let edited = CompiledAgentPlan::compile_blocking(
            profile(directory.path())
                .with_spawn_model_routes(vec!["custom/worker".to_owned()])
                .with_provenance(vec!["inline".to_owned()]),
        )
        .unwrap();
        assert_ne!(edited.digest(), plan.digest());
    }

    #[test]
    fn compile_rejects_non_canonical_paths_missing_workspaces_and_bad_instructions() {
        let directory = canonical_temp();
        let relative = CompiledAgentPlan::compile_blocking(profile(Path::new("relative/path")));
        assert!(matches!(
            relative,
            Err(PlanCompileError::NonCanonicalWorkspace { .. })
        ));
        let dotted = CompiledAgentPlan::compile_blocking(profile(&directory.path().join("..")));
        assert!(matches!(
            dotted,
            Err(PlanCompileError::NonCanonicalWorkspace { .. })
        ));
        let missing =
            CompiledAgentPlan::compile_blocking(profile(&directory.path().join("absent")));
        assert!(matches!(
            missing,
            Err(PlanCompileError::OpenWorkspace { .. })
        ));

        std::fs::create_dir(directory.path().join("AGENTS.md")).unwrap();
        let not_a_file = CompiledAgentPlan::compile_blocking(profile(directory.path()));
        assert!(matches!(
            not_a_file,
            Err(PlanCompileError::Instructions(
                WorkspaceInstructionError::NotAFile { .. }
            ))
        ));
    }

    #[test]
    fn embedded_profiles_and_runtime_mismatches_are_reported() {
        let directory = canonical_temp();
        let runtime = Runtime::new(SilentProvider, "embedded-model", 512)
            .unwrap()
            .with_context_window(Some(2_048));
        let plan = CompiledAgentPlan::compile_blocking(AgentProfile::embedded(
            &runtime,
            directory.path().to_owned(),
        ))
        .unwrap();
        assert_eq!(plan.descriptor().provider, ProviderDescriptor::embedded());
        assert_eq!(plan.resolved_model().route, "embedded/embedded-model");
        assert_eq!(plan.resolved_model().max_output_tokens, 512);
        assert_eq!(plan.resolved_model().context_window, Some(2_048));

        let mut mismatched = resolved_model();
        mismatched.provider_model = "other".to_owned();
        let error = crate::LoadedRuntime::compile_blocking(
            &runtime,
            mismatched,
            directory.path().to_owned(),
        )
        .err()
        .expect("mismatch must fail");
        assert!(matches!(error, PlanCompileError::ModelMismatch { .. }));
    }
}
