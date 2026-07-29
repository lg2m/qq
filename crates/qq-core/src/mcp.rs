//! The seam between the run loop and an external MCP client registry.
//!
//! `qq-core` never speaks the MCP protocol itself: the composition root
//! wires a concrete registry (backed by the `qq-mcp` crate) through this
//! trait, exactly as providers arrive through [`qq_provider::Provider`].

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, atomic::AtomicBool},
};

use qq_provider::ToolSpec;

/// Namespace prefix for MCP tool names: `mcp__<server>__<tool>`. Built-in
/// tool names never carry it, so collisions are impossible by construction.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// The outcome of one MCP tool call. Every failure — connect error, timeout,
/// cancellation, server error result — is an `is_error` outcome that becomes
/// an ordinary tool error for the model, never a run failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    pub content: String,
    pub is_error: bool,
}

pub type McpSpecsFuture = Pin<Box<dyn Future<Output = Vec<ToolSpec>> + Send + 'static>>;
pub type McpCallFuture = Pin<Box<dyn Future<Output = McpToolResult> + Send + 'static>>;

/// Configuration-declared MCP servers as the run loop sees them.
///
/// Implementations own connection management and must keep every returned
/// future bounded in time: the run loop awaits `tool_specs` once per run and
/// `call` once per dispatched `mcp__` tool call, and neither may hang a run
/// on an unresponsive server.
pub trait McpRegistry: Send + Sync {
    /// Cached tool declarations, namespaced `mcp__<server>__<tool>`.
    /// Connecting lazily on first use is the implementation's business;
    /// an unavailable server contributes no declarations.
    fn tool_specs(&self) -> McpSpecsFuture;

    /// Exact namespaced tool names granted by workspace configuration
    /// allowlists, merged into policy evaluation as session-style grants.
    fn config_grants(&self) -> Vec<String>;

    /// Executes one namespaced call. `cancelled` is the run's cancellation
    /// flag; a cancelled call must return promptly without wedging the
    /// shared client connection.
    fn call(&self, name: String, arguments: String, cancelled: Arc<AtomicBool>) -> McpCallFuture;
}
