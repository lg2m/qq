use std::{future::Future, pin::Pin};

/// The outcome one spawned sub-agent call returns to its parent. The content
/// flows through the same bounded-result truncation as built-in tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnAgentOutcome {
    pub(crate) content: String,
    pub(crate) is_error: bool,
}

pub(crate) type SpawnAgentFuture =
    Pin<Box<dyn Future<Output = SpawnAgentOutcome> + Send + 'static>>;

/// Runs one sub-agent task to completion on behalf of a `spawn_agent` call.
/// The session runtime installs a spawner for eligible runs only: child
/// sessions (and session-less runs) get none, so the tool is neither declared
/// nor dispatchable there. Dropping the returned future must cancel the
/// in-flight child work.
pub(crate) trait SubagentSpawner: Send + Sync {
    fn spawn(&self, task: String, model: Option<String>) -> SpawnAgentFuture;
}

/// The dispatcher's defensive answer when `spawn_agent` is called by a run
/// that has no spawner (a child session, or a run outside the session layer).
pub(crate) const SPAWN_UNAVAILABLE_RESULT: &str =
    "spawn_agent is not available in this session; sub-agents cannot spawn sub-agents.";
