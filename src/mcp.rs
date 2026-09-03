//! Wires configuration-declared MCP servers into the core tool-host seam.
//!
//! The composition root translates `ConfigSnapshot` MCP declarations into
//! `qq-mcp` settings (resolving bearer secrets like every other credential)
//! and adapts the resulting [`McpManager`] to `qq-core`'s
//! [`ExternalToolHost`] trait. Registries are cached by declaration digest,
//! so every plan built from identical declarations shares one manager — and
//! therefore one client connection per server — for the whole server
//! process. Plans snapshot the manager's catalog at compile and revalidate
//! against its generation.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use qq_auth::CredentialStore;
use qq_config::{ConfigSnapshot, McpServerConfig, McpTransport};
use qq_core::{
    ExternalToolHost, HostCallError, HostCallFuture, HostCatalog, HostReadiness,
    HostShutdownFuture, HostTool, HostToolResult, ToolHints,
    plan::{CredentialReference, McpServerDescriptor, McpTransportKind},
};
use qq_mcp::{McpCallFailure, McpManager, McpServerSettings, McpTransportSettings};
use qq_protocol::CredentialEpoch;
use qq_provider::SecretRef;
use sha2::{Digest, Sha256};

use crate::runtime::RuntimeBuildError;

const MAX_CACHED_REGISTRIES: usize = 8;
/// Bound on one catalog snapshot from a blocking compile thread: connect,
/// list, and namespace every declared server.
const CATALOG_SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Adapts the shared [`McpManager`] to the `qq-core` tool-host seam.
pub struct WiredMcpRegistry {
    manager: Arc<McpManager>,
}

impl ExternalToolHost for WiredMcpRegistry {
    fn name(&self) -> &str {
        "mcp"
    }

    fn catalog_blocking(&self) -> HostCatalog {
        // Compiles run on blocking threads inside a Tokio process; the
        // manager's connections live on the async runtime, so the fetch is
        // driven from there and awaited here under a hard bound.
        let manager = Arc::clone(&self.manager);
        let fetch =
            async move { tokio::time::timeout(CATALOG_SNAPSHOT_TIMEOUT, manager.catalog()).await };
        let fetched = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(fetch),
            Err(_) => {
                return HostCatalog {
                    generation: self.manager.generation(),
                    tools: Vec::new(),
                    readiness: HostReadiness::Unavailable {
                        message: "no async runtime is available to reach MCP servers".to_owned(),
                    },
                };
            }
        };
        match fetched {
            Ok(catalog) => HostCatalog {
                generation: catalog.generation,
                tools: catalog
                    .tools
                    .into_iter()
                    .map(|tool| HostTool {
                        spec: tool.spec,
                        hints: ToolHints {
                            read_only: tool.hints.read_only,
                            destructive: tool.hints.destructive,
                            idempotent: tool.hints.idempotent,
                            open_world: tool.hints.open_world,
                        },
                    })
                    .collect(),
                readiness: if catalog.unavailable.is_empty() {
                    HostReadiness::Ready
                } else {
                    HostReadiness::Degraded {
                        message: format!(
                            "unavailable MCP servers: {}",
                            catalog.unavailable.join(", ")
                        ),
                    }
                },
            },
            Err(_) => HostCatalog {
                generation: self.manager.generation(),
                tools: Vec::new(),
                readiness: HostReadiness::Unavailable {
                    message: format!(
                        "MCP catalog snapshot timed out after {} s",
                        CATALOG_SNAPSHOT_TIMEOUT.as_secs()
                    ),
                },
            },
        }
    }

    fn catalog_is_current(&self, generation: u64) -> bool {
        self.manager.catalog_is_current(generation)
    }

    fn config_grants(&self) -> Vec<String> {
        self.manager.config_grants()
    }

    fn call(&self, name: String, arguments: String, cancelled: Arc<AtomicBool>) -> HostCallFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            let outcome = manager.call(&name, &arguments, cancelled).await;
            match outcome.failure {
                None => Ok(HostToolResult {
                    content: outcome.content,
                    is_error: outcome.is_error,
                }),
                Some(McpCallFailure::Timeout) => Err(HostCallError::Timeout),
                Some(McpCallFailure::Cancelled) => Err(HostCallError::Cancelled),
                Some(McpCallFailure::Unavailable) => {
                    Err(HostCallError::Unavailable(outcome.content))
                }
                Some(McpCallFailure::InvalidArguments) => {
                    Err(HostCallError::Refused(outcome.content))
                }
                Some(McpCallFailure::UnknownTool) => Err(HostCallError::UnknownTool(name)),
                Some(McpCallFailure::ShutDown) => Err(HostCallError::ShutDown),
            }
        })
    }

    fn readiness(&self) -> HostReadiness {
        if self.manager.is_shut_down() {
            HostReadiness::ShutDown
        } else {
            HostReadiness::Ready
        }
    }

    fn shutdown(&self) -> HostShutdownFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move { manager.shutdown().await })
    }
}

/// A shared registry plus the digest identifying its declarations.
/// A wired registry plus the secret-free descriptors of the servers it holds.
pub(crate) struct WiredMcp {
    pub(crate) registry: Arc<WiredMcpRegistry>,
    pub(crate) servers: Vec<McpServerDescriptor>,
}

/// Cache key: the declaration digest (no secret bytes) plus the credential
/// epoch the bearer tokens were resolved under, so a rotated token rebuilds
/// the connection without any secret entering the key.
#[derive(Clone, PartialEq, Eq)]
struct RegistryKey {
    declarations: [u8; 32],
    epoch: CredentialEpoch,
}

/// Process-wide cache of wired registries keyed by declaration digest.
pub(crate) struct McpRegistryCache {
    cache: Mutex<VecDeque<(RegistryKey, Arc<WiredMcpRegistry>)>>,
}

impl McpRegistryCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(VecDeque::new()),
        }
    }

    /// Returns the shared registry for the snapshot's MCP declarations with
    /// their secret-free descriptors, or `None` when no servers are declared.
    /// `epoch` is the credential epoch the bearer secrets are resolved under.
    pub(crate) fn registry_for_snapshot(
        &self,
        credentials: &CredentialStore,
        epoch: CredentialEpoch,
        snapshot: &ConfigSnapshot,
    ) -> Result<Option<WiredMcp>, RuntimeBuildError> {
        if snapshot.mcp_servers().is_empty() {
            return Ok(None);
        }
        let mut descriptors = Vec::with_capacity(snapshot.mcp_servers().len());
        for (name, server) in snapshot.mcp_servers() {
            descriptors.push(describe_server(name, server));
        }
        let key = RegistryKey {
            declarations: declaration_digest(&descriptors),
            epoch,
        };

        {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
            if let Some(index) = cache.iter().position(|(cached, _)| *cached == key) {
                let (cached_key, registry) = cache
                    .remove(index)
                    .expect("a located registry cache entry must exist");
                cache.push_back((cached_key, Arc::clone(&registry)));
                return Ok(Some(WiredMcp {
                    registry,
                    servers: descriptors,
                }));
            }
        }

        // Secrets are resolved only on a miss, after the key is known.
        let mut settings = Vec::with_capacity(snapshot.mcp_servers().len());
        for (name, server) in snapshot.mcp_servers() {
            settings.push(resolve_server(name, server, credentials)?);
        }
        let registry = Arc::new(WiredMcpRegistry {
            manager: Arc::new(McpManager::new(settings)?),
        });
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
        cache.push_back((key, Arc::clone(&registry)));
        while cache.len() > MAX_CACHED_REGISTRIES {
            cache.pop_front();
        }
        Ok(Some(WiredMcp {
            registry,
            servers: descriptors,
        }))
    }
}

/// The digest of the declarations exactly as their descriptors encode them:
/// the descriptor is already the secret-free canonical form, so hashing it
/// keeps this key and the plan digest in agreement about what "changed".
fn declaration_digest(descriptors: &[McpServerDescriptor]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for descriptor in descriptors {
        let encoded = serde_json::to_vec(descriptor)
            .expect("MCP descriptors contain only strings, integers, and booleans");
        digest.update(encoded.len().to_le_bytes());
        digest.update(&encoded);
    }
    digest.finalize().into()
}

fn describe_server(name: &str, server: &McpServerConfig) -> McpServerDescriptor {
    let (transport, target, args, env, credential) = match server.transport() {
        McpTransport::Stdio { command, args, env } => (
            McpTransportKind::Stdio,
            command.clone(),
            args.clone(),
            env.clone(),
            CredentialReference::None,
        ),
        McpTransport::Http { url, bearer } => (
            McpTransportKind::Http,
            crate::runtime::describe_endpoint(url),
            Vec::new(),
            Vec::new(),
            match bearer {
                None => CredentialReference::None,
                Some(SecretRef::Env(name)) => CredentialReference::Environment(name.clone()),
                Some(SecretRef::Stored(name)) => CredentialReference::Stored(name.clone()),
                Some(SecretRef::Value(_)) => CredentialReference::Inline,
            },
        ),
    };
    McpServerDescriptor {
        name: name.to_owned(),
        transport,
        target,
        args,
        env,
        credential,
        eager: server.eager(),
        allow: server.allow().to_vec(),
        call_timeout_seconds: server.call_timeout_seconds(),
        max_concurrent_calls: server.max_concurrent_calls(),
    }
}

fn resolve_server(
    name: &str,
    server: &McpServerConfig,
    credentials: &CredentialStore,
) -> Result<McpServerSettings, RuntimeBuildError> {
    let transport = match server.transport() {
        McpTransport::Stdio { command, args, env } => McpTransportSettings::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
        },
        McpTransport::Http { url, bearer } => {
            let bearer = match bearer {
                Some(reference) => {
                    let secret = credentials.resolve_with_endpoint(reference, Some(url))?;
                    Some(secret.expose_secret_str()?.to_owned())
                }
                None => None,
            };
            McpTransportSettings::Http {
                url: url.clone(),
                bearer,
            }
        }
    };

    let mut settings = McpServerSettings::new(name, transport);
    settings.eager = server.eager();
    settings.allow = server.allow().to_vec();
    settings.call_timeout = std::time::Duration::from_secs(server.call_timeout_seconds());
    settings.max_concurrent_calls = usize::try_from(server.max_concurrent_calls())
        .expect("the validated concurrency bound fits usize");
    Ok(settings)
}
