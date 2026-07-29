//! Wires configuration-declared MCP servers into the core runtime seam.
//!
//! The composition root translates `ConfigSnapshot` MCP declarations into
//! `qq-mcp` settings (resolving bearer secrets like every other credential)
//! and adapts the resulting [`McpManager`] to `qq-core`'s [`McpRegistry`]
//! trait. Registries are cached by declaration digest, so every runtime
//! built from identical declarations shares one manager — and therefore one
//! client connection per server — for the whole server process.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use qq_core::{McpCallFuture, McpRegistry, McpSpecsFuture};
use qq_mcp::{McpManager, McpServerSettings, McpTransportSettings};
use sha2::{Digest, Sha256};

use crate::{
    auth::CredentialStore,
    config::{ConfigSnapshot, McpServerConfig, McpTransport},
    runtime::RuntimeBuildError,
};

const MAX_CACHED_REGISTRIES: usize = 8;

/// Adapts the shared [`McpManager`] to the `qq-core` registry seam.
pub struct WiredMcpRegistry {
    manager: Arc<McpManager>,
}

impl McpRegistry for WiredMcpRegistry {
    fn tool_specs(&self) -> McpSpecsFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move { manager.tool_specs().await })
    }

    fn config_grants(&self) -> Vec<String> {
        self.manager.config_grants()
    }

    fn call(&self, name: String, arguments: String, cancelled: Arc<AtomicBool>) -> McpCallFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            let outcome = manager.call(&name, &arguments, cancelled).await;
            qq_core::McpToolResult {
                content: outcome.content,
                is_error: outcome.is_error,
            }
        })
    }
}

/// A shared registry plus the digest identifying its declarations.
pub(crate) type KeyedMcpRegistry = (Arc<WiredMcpRegistry>, Vec<u8>);

/// Process-wide cache of wired registries keyed by declaration digest.
pub(crate) struct McpRegistryCache {
    cache: Mutex<VecDeque<(Vec<u8>, Arc<WiredMcpRegistry>)>>,
}

impl McpRegistryCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: Mutex::new(VecDeque::new()),
        }
    }

    /// Returns the shared registry for the snapshot's MCP declarations plus
    /// the digest identifying them, or `None` when no servers are declared.
    pub(crate) fn registry_for_snapshot(
        &self,
        credentials: &CredentialStore,
        snapshot: &ConfigSnapshot,
    ) -> Result<Option<KeyedMcpRegistry>, RuntimeBuildError> {
        if snapshot.mcp_servers().is_empty() {
            return Ok(None);
        }
        let mut digest = Sha256::new();
        let mut settings = Vec::with_capacity(snapshot.mcp_servers().len());
        for (name, server) in snapshot.mcp_servers() {
            settings.push(resolve_server(name, server, credentials, &mut digest)?);
        }
        let key = digest.finalize().to_vec();

        let mut cache = self
            .cache
            .lock()
            .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
        if let Some(index) = cache.iter().position(|(cached, _)| *cached == key) {
            let (cached_key, registry) = cache
                .remove(index)
                .expect("a located registry cache entry must exist");
            cache.push_back((cached_key, Arc::clone(&registry)));
            return Ok(Some((registry, key)));
        }
        let registry = Arc::new(WiredMcpRegistry {
            manager: Arc::new(McpManager::new(settings)?),
        });
        cache.push_back((key.clone(), Arc::clone(&registry)));
        while cache.len() > MAX_CACHED_REGISTRIES {
            cache.pop_front();
        }
        Ok(Some((registry, key)))
    }
}

fn resolve_server(
    name: &str,
    server: &McpServerConfig,
    credentials: &CredentialStore,
    digest: &mut Sha256,
) -> Result<McpServerSettings, RuntimeBuildError> {
    update_digest(digest, name.as_bytes());
    let transport = match server.transport() {
        McpTransport::Stdio { command, args, env } => {
            update_digest(digest, b"stdio");
            update_digest(digest, command.as_bytes());
            for arg in args {
                update_digest(digest, arg.as_bytes());
            }
            for variable in env {
                update_digest(digest, variable.as_bytes());
            }
            McpTransportSettings::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
            }
        }
        McpTransport::Http { url, bearer } => {
            update_digest(digest, b"http");
            update_digest(digest, url.as_bytes());
            let bearer = match bearer {
                Some(reference) => {
                    let secret = credentials.resolve_with_endpoint(reference, Some(url))?;
                    let token = secret.expose_secret_str()?.to_owned();
                    update_digest(digest, token.as_bytes());
                    Some(token)
                }
                None => {
                    update_digest(digest, b"no-auth");
                    None
                }
            };
            McpTransportSettings::Http {
                url: url.clone(),
                bearer,
            }
        }
    };
    digest.update([u8::from(server.eager())]);
    for tool in server.allow() {
        update_digest(digest, tool.as_bytes());
    }
    digest.update(server.call_timeout_seconds().to_le_bytes());
    digest.update(server.max_concurrent_calls().to_le_bytes());

    let mut settings = McpServerSettings::new(name, transport);
    settings.eager = server.eager();
    settings.allow = server.allow().to_vec();
    settings.call_timeout = std::time::Duration::from_secs(server.call_timeout_seconds());
    settings.max_concurrent_calls = usize::try_from(server.max_concurrent_calls())
        .expect("the validated concurrency bound fits usize");
    Ok(settings)
}

fn update_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update(value.len().to_le_bytes());
    digest.update(value);
}
