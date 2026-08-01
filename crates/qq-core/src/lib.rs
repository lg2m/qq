//! Agent runtime, session behavior, tools, and persistence.

#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_stream::stream;
use futures_core::Stream;
use futures_util::{StreamExt, stream as futures_stream};
use qq_protocol::{
    ApprovalMode, ReasoningKind, RunActivity, RunCommand, RunEvent, RunFailureKind, TokenUsage,
    ToolCallId,
};
use qq_provider::{
    ContentBlock, Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent, Role,
};
use thiserror::Error;

mod approval;
mod mcp;
mod sessions;
mod tools;

pub use mcp::{MCP_TOOL_PREFIX, McpCallFuture, McpRegistry, McpSpecsFuture, McpToolResult};
pub use sessions::{
    GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError,
    SessionRuntimeOptions, WorkerRuntimeLoadFuture, WorkspaceGrantAuthority, WorkspaceGrantSeed,
};

pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send + 'static>>;
type RuntimeStream = Pin<Box<dyn Stream<Item = RuntimeEvent> + Send + 'static>>;

const MAX_TOOL_CALLS_PER_TURN: usize = 16;
// A runaway-loop backstop, not a work limiter: routine agentic tasks
// must finish well below it. Exhaustion settles the run with a
// tool-free final turn, never a failure, so the session always
// continues from where the run stopped.
const MAX_TOOL_CALLS_PER_RUN: usize = 256;
// Leave room for the reserved final answer after the last permitted
// tool call.
const MAX_MODEL_TURNS: u16 = MAX_TOOL_CALLS_PER_RUN as u16 + 1;
// Appended to the system prompt on the reserved final turn so the
// model closes the run in a resumable state instead of attempting
// more work.
const TOOL_BUDGET_NOTICE: &str = "This run has used its entire tool budget, so no tools are \
available for this reply. Summarize what was accomplished, what remains, and the exact next \
step. The session keeps this full history: the next prompt resumes where this run stopped.";
const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_TOOL_CALL_ID_BYTES: usize = 1_024;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_RUN_MODEL_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUN_REASONING_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_READS: usize = 4;

#[derive(Debug, Clone, PartialEq)]
enum RuntimeEvent {
    Started,
    ActivityChanged {
        activity: RunActivity,
    },
    ReasoningStarted {
        kind: ReasoningKind,
    },
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    ReasoningCompleted {
        kind: ReasoningKind,
    },
    OutputTextDelta {
        text: String,
    },
    RefusalDelta {
        text: String,
    },
    AssistantTurnCompleted {
        turn_ordinal: u16,
        message: Message,
        usage: Option<TokenUsage>,
        /// Tool calls requested by this turn, in request order. Carried on the
        /// same event as the completed turn so the store can persist the turn
        /// and its calls in one transaction; a crash must never leave a
        /// persisted ToolCall block without its tool_calls rows.
        calls: Vec<RuntimeToolCall>,
    },
    ToolCallStarted {
        id: ToolCallId,
    },
    ToolCallDenied {
        id: ToolCallId,
        message: String,
    },
    /// A chunk of live output from a running tool (shell commands stream their
    /// combined stdout+stderr). Display-only: the bounded result on
    /// `ToolCallFinished` remains authoritative.
    ToolCallOutputDelta {
        id: ToolCallId,
        chunk: String,
    },
    ToolCallFinished {
        id: ToolCallId,
        result: String,
        is_error: bool,
        /// A file-state map entry recorded by this execution, persisted with
        /// the result so the map can be rebuilt for later runs.
        file_state: Option<tools::FileStateUpdate>,
        /// A UI-facing payload persisted with the result (the applied diff of
        /// a successful edit). Never enters model context.
        display: Option<qq_protocol::ToolCallDisplay>,
    },
    Completed,
    Failed {
        kind: RunFailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeToolCall {
    id: ToolCallId,
    turn_ordinal: u16,
    call_ordinal: u16,
    provider_call_id: String,
    name: String,
    arguments: String,
    /// Set when the provider streamed arguments that were not valid JSON. The
    /// call is never executed; this message is returned to the model as a
    /// retryable tool error instead of failing the run.
    argument_error: Option<String>,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The runtime's answer for one requested tool call after policy and, when
/// required, an approval round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Execute,
    Deny { message: String },
}

pub(crate) type ToolGateFuture = Pin<Box<dyn Future<Output = GateDecision> + Send + 'static>>;

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

/// Resolves approval policy for tool calls before they execute. The session
/// runtime installs a gate that persists approval state and waits for clients;
/// gate-less runs fall back to a static policy that cannot prompt.
pub(crate) trait ToolGate: Send + Sync {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture;
}

struct StaticPolicyGate {
    mode: ApprovalMode,
    /// Workspace-configured grants (today: MCP allowlist entries by exact
    /// namespaced name). Mode still wins: read-only denies granted tools.
    grants: approval::SessionGrants,
}

impl ToolGate for StaticPolicyGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let class = approval::classify(&call.name, &call.arguments);
        let decision = match approval::evaluate(self.mode, &call.name, &class, &self.grants) {
            approval::PolicyDecision::Execute => GateDecision::Execute,
            approval::PolicyDecision::Deny => GateDecision::Deny {
                message: approval::POLICY_DENIED_RESULT.to_owned(),
            },
            approval::PolicyDecision::RequireApproval => GateDecision::Deny {
                message: approval::UNATTENDED_DENIED_RESULT.to_owned(),
            },
        };
        Box::pin(std::future::ready(decision))
    }
}

#[derive(Debug)]
struct PendingToolCall {
    provider_call_id: String,
    name: String,
    arguments: String,
    parsed_arguments: Option<serde_json::Value>,
    argument_error: Option<String>,
    completed: bool,
}

enum TurnBlock {
    Text(String),
    ToolCall(usize),
}

/// Runs protocol commands against a configured model provider.
#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    model: Arc<str>,
    max_output_tokens: u32,
    mcp: Option<Arc<dyn McpRegistry>>,
}

impl Runtime {
    pub fn new(
        provider: impl Provider + 'static,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        Self::with_provider(Arc::new(provider), model, max_output_tokens)
    }

    /// Creates a runtime without reboxing an already shared provider.
    pub fn with_provider(
        provider: Arc<dyn Provider>,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(RuntimeConfigError::EmptyModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeConfigError::ZeroMaxOutputTokens);
        }

        Ok(Self {
            provider,
            model,
            max_output_tokens,
            mcp: None,
        })
    }

    /// Attaches a registry of configuration-declared MCP servers. Its cached
    /// tool declarations join the built-ins for every run of this runtime,
    /// and `mcp__`-named calls dispatch to it.
    #[must_use]
    pub fn with_mcp_registry(mut self, registry: Arc<dyn McpRegistry>) -> Self {
        self.mcp = Some(registry);
        self
    }

    /// Runs one command and returns events as they become available.
    pub fn run(&self, command: RunCommand) -> RunStream {
        self.run_in_workspace(
            command,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// Runs one command with read-only tools scoped to `workspace`.
    pub fn run_in_workspace(&self, command: RunCommand, workspace: PathBuf) -> RunStream {
        let mut events =
            self.run_messages_in_workspace(vec![Message::user(command.into_prompt())], workspace);
        Box::pin(stream! {
            while let Some(event) = events.next().await {
                match event {
                    RuntimeEvent::Started => yield RunEvent::Started,
                    RuntimeEvent::ActivityChanged { activity } => {
                        yield RunEvent::ActivityChanged { activity };
                    }
                    RuntimeEvent::ReasoningStarted { kind } => {
                        yield RunEvent::ReasoningStarted { kind };
                    }
                    RuntimeEvent::ReasoningDelta { kind, text } => {
                        yield RunEvent::ReasoningDelta { kind, text };
                    }
                    RuntimeEvent::ReasoningCompleted { kind } => {
                        yield RunEvent::ReasoningCompleted { kind };
                    }
                    RuntimeEvent::OutputTextDelta { text } => {
                        yield RunEvent::OutputTextDelta { text };
                    }
                    RuntimeEvent::RefusalDelta { text } => {
                        yield RunEvent::RefusalDelta { text };
                    }
                    RuntimeEvent::AssistantTurnCompleted { usage: Some(usage), .. } => {
                        yield RunEvent::Usage { usage };
                    }
                    RuntimeEvent::AssistantTurnCompleted { usage: None, .. }
                    | RuntimeEvent::ToolCallStarted { .. }
                    | RuntimeEvent::ToolCallDenied { .. }
                    | RuntimeEvent::ToolCallOutputDelta { .. }
                    | RuntimeEvent::ToolCallFinished { .. } => {}
                    RuntimeEvent::Completed => {
                        yield RunEvent::Completed;
                        return;
                    }
                    RuntimeEvent::Failed { kind, message } => {
                        yield RunEvent::Failed { kind, message };
                        return;
                    }
                }
            }
        })
    }

    /// Runs a multi-turn model/tool loop with explicit prior conversation context.
    pub fn run_messages(&self, messages: Vec<Message>) -> RunStream {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut events = self.run_messages_in_workspace(messages, workspace);
        Box::pin(stream! {
            while let Some(event) = events.next().await {
                match event {
                    RuntimeEvent::Started => yield RunEvent::Started,
                    RuntimeEvent::ActivityChanged { activity } => yield RunEvent::ActivityChanged { activity },
                    RuntimeEvent::ReasoningStarted { kind } => yield RunEvent::ReasoningStarted { kind },
                    RuntimeEvent::ReasoningDelta { kind, text } => yield RunEvent::ReasoningDelta { kind, text },
                    RuntimeEvent::ReasoningCompleted { kind } => yield RunEvent::ReasoningCompleted { kind },
                    RuntimeEvent::OutputTextDelta { text } => yield RunEvent::OutputTextDelta { text },
                    RuntimeEvent::RefusalDelta { text } => yield RunEvent::RefusalDelta { text },
                    RuntimeEvent::AssistantTurnCompleted { usage: Some(usage), .. } => {
                        yield RunEvent::Usage { usage };
                    }
                    RuntimeEvent::AssistantTurnCompleted { usage: None, .. }
                    | RuntimeEvent::ToolCallStarted { .. }
                    | RuntimeEvent::ToolCallDenied { .. }
                    | RuntimeEvent::ToolCallOutputDelta { .. }
                    | RuntimeEvent::ToolCallFinished { .. } => {}
                    RuntimeEvent::Completed => {
                        yield RunEvent::Completed;
                        return;
                    }
                    RuntimeEvent::Failed { kind, message } => {
                        yield RunEvent::Failed { kind, message };
                        return;
                    }
                }
            }
        })
    }

    fn run_messages_in_workspace(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
    ) -> RuntimeStream {
        self.run_messages_in_workspace_with_cancellation(
            messages,
            workspace,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn run_messages_in_workspace_with_cancellation(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
    ) -> RuntimeStream {
        // Configuration allowlists are the only grants a gate-less run has;
        // read-only mode still denies them inside `evaluate`.
        let grants = approval::SessionGrants {
            tools: self
                .mcp
                .as_ref()
                .map(|registry| registry.config_grants().into_iter().collect())
                .unwrap_or_default(),
            shell_prefixes: Vec::new(),
        };
        self.run_loop(
            messages,
            workspace,
            cancelled,
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Ask,
                grants,
            }),
            Arc::new(tools::FileState::default()),
        )
    }

    pub(crate) fn run_loop(
        &self,
        messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<tools::FileState>,
    ) -> RuntimeStream {
        self.run_loop_with_spawner(messages, workspace, cancelled, gate, file_state, None)
    }

    pub(crate) fn run_loop_with_spawner(
        &self,
        mut messages: Vec<Message>,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        gate: Arc<dyn ToolGate>,
        file_state: Arc<tools::FileState>,
        spawner: Option<Arc<dyn SubagentSpawner>>,
    ) -> RuntimeStream {
        let provider = Arc::clone(&self.provider);
        let model = Arc::clone(&self.model);
        let max_output_tokens = self.max_output_tokens;
        let mcp = self.mcp.clone();
        Box::pin(stream! {
            let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancelled));
            yield RuntimeEvent::Started;

            if messages.is_empty() || messages.iter().any(|message| !message.has_content()) {
                yield RuntimeEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message: "conversation messages must not be empty".to_owned(),
                };
                return;
            }

            let workspace = match tools::open_workspace(workspace, Arc::clone(&cancelled)).await {
                Ok(workspace) => workspace,
                Err(error) => {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::InvalidCommand,
                        message: format!("could not open the workspace directory: {error}"),
                    };
                    return;
                }
            };
            // MCP declarations join the built-ins once per run: the cached
            // specs are fetched here (connecting lazily on first use) so
            // every turn of the run sees one stable tool list. The `mcp__`
            // prefix keeps collisions with built-ins impossible, and specs
            // that do not carry it are discarded to keep dispatch unambiguous.
            let mut tool_specs = tools::specs();
            // The sub-agent tool is declared only when this run may spawn:
            // depth is one, so child runs (spawner-less) never see it.
            if spawner.is_some() {
                tool_specs.push(tools::spawn_agent_spec());
            }
            if let Some(registry) = &mcp {
                for spec in registry.tool_specs().await {
                    if spec.name().starts_with(MCP_TOOL_PREFIX)
                        && spec.name().len() <= MAX_TOOL_NAME_BYTES
                        && !tool_specs.iter().any(|existing| existing.name() == spec.name())
                    {
                        tool_specs.push(spec);
                    }
                }
            }
            let system: Arc<str> = Arc::from(agent_system_prompt(workspace.path(), &tool_specs));

            let mut total_tool_calls = 0_usize;
            let mut model_text_bytes = 0_usize;
            for turn_ordinal in 1..=MAX_MODEL_TURNS {
                // Once the tool budget is spent (or only the reserved turn
                // remains), the model gets one tool-free turn to settle.
                // The run then completes normally, so every committed turn
                // stays valid context and a follow-up prompt resumes where
                // this run stopped — budget exhaustion is never a failure.
                let final_turn = total_tool_calls >= MAX_TOOL_CALLS_PER_RUN
                    || turn_ordinal == MAX_MODEL_TURNS;
                let request =
                    ModelRequest::new(Arc::clone(&model), messages.clone(), max_output_tokens);
                let request = if final_turn {
                    request.with_system(format!("{system}\n\n{TOOL_BUDGET_NOTICE}"))
                } else {
                    request
                        .with_tools(tool_specs.clone())
                        .with_system(Arc::clone(&system))
                };
                let mut activity = RunActivity::WaitingForProvider;
                yield RuntimeEvent::ActivityChanged { activity };
                let mut provider_events = provider.stream(request);
                let mut pending_calls = Vec::<PendingToolCall>::new();
                let mut calls_by_provider_id = HashMap::<String, usize>::new();
                let mut blocks = Vec::<TurnBlock>::new();
                let mut terminal_usage = None;
                let mut completed = false;
                let mut reasoning_bytes = 0_usize;
                let mut open_reasoning = None;

                while let Some(event) = provider_events.next().await {
                    match event {
                        Ok(ProviderEvent::ReasoningStarted { kind }) => {
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider started a reasoning block before completing the previous block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = Some(kind);
                            if activity != RunActivity::Reasoning {
                                activity = RunActivity::Reasoning;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            yield RuntimeEvent::ReasoningStarted { kind };
                        }
                        Ok(ProviderEvent::ReasoningDelta { kind, text }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider streamed reasoning outside its matching block".to_owned(),
                                };
                                return;
                            }
                            if reasoning_bytes.saturating_add(text.len()) > MAX_RUN_REASONING_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "displayable reasoning exceeded the 1 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            reasoning_bytes += text.len();
                            if !text.is_empty() {
                                yield RuntimeEvent::ReasoningDelta { kind, text };
                            }
                        }
                        Ok(ProviderEvent::ReasoningCompleted { kind }) => {
                            if open_reasoning != Some(kind) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed an unknown reasoning block".to_owned(),
                                };
                                return;
                            }
                            open_reasoning = None;
                            yield RuntimeEvent::ReasoningCompleted { kind };
                        }
                        Ok(ProviderEvent::OutputTextDelta { text }) => {
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::OutputTextDelta { text };
                        }
                        Ok(ProviderEvent::RefusalDelta { text }) => {
                            if activity != RunActivity::GeneratingResponse {
                                activity = RunActivity::GeneratingResponse;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if model_text_bytes.saturating_add(text.len()) > MAX_RUN_MODEL_TEXT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::Policy,
                                    message: "model text exceeded the 16 MiB per-run limit".to_owned(),
                                };
                                return;
                            }
                            model_text_bytes += text.len();
                            append_turn_text(&mut blocks, &text);
                            yield RuntimeEvent::RefusalDelta { text };
                        }
                        Ok(ProviderEvent::ToolCallStarted { id, name }) => {
                            if activity != RunActivity::PreparingToolCall {
                                activity = RunActivity::PreparingToolCall;
                                yield RuntimeEvent::ActivityChanged { activity };
                            }
                            if id.is_empty() || id.len() > MAX_TOOL_CALL_ID_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool call ID is empty or exceeds 1 KiB".to_owned(),
                                };
                                return;
                            }
                            if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider tool name is empty or exceeds 128 bytes".to_owned(),
                                };
                                return;
                            }
                            if pending_calls.len() >= MAX_TOOL_CALLS_PER_TURN {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("model requested more than {MAX_TOOL_CALLS_PER_TURN} tools in one turn"),
                                };
                                return;
                            }
                            // The per-run budget is enforced at the top of the
                            // turn, where it settles gracefully; a call arriving
                            // on the reserved tool-free turn is a provider bug.
                            if final_turn {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider requested a tool on the reserved final turn, which declares none".to_owned(),
                                };
                                return;
                            }
                            if calls_by_provider_id.contains_key(&id) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider reused tool call ID {id:?} in one turn"),
                                };
                                return;
                            }
                            let index = pending_calls.len();
                            calls_by_provider_id.insert(id.clone(), index);
                            pending_calls.push(PendingToolCall {
                                provider_call_id: id,
                                name,
                                arguments: String::new(),
                                parsed_arguments: None,
                                argument_error: None,
                                completed: false,
                            });
                            total_tool_calls += 1;
                            blocks.push(TurnBlock::ToolCall(index));
                        }
                        Ok(ProviderEvent::ToolCallArgumentsDelta { id, json }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments for unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider streamed arguments after completing tool call {id:?}"),
                                };
                                return;
                            }
                            if call.arguments.len().saturating_add(json.len()) > MAX_TOOL_ARGUMENT_BYTES {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("tool call {id:?} arguments exceed the 64 KiB limit"),
                                };
                                return;
                            }
                            call.arguments.push_str(&json);
                        }
                        Ok(ProviderEvent::ToolCallCompleted { id }) => {
                            let Some(index) = calls_by_provider_id.get(&id).copied() else {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed unknown tool call {id:?}"),
                                };
                                return;
                            };
                            let call = &mut pending_calls[index];
                            if call.completed {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!("provider completed tool call {id:?} twice"),
                                };
                                return;
                            }
                            let arguments = if call.arguments.trim().is_empty() {
                                "{}"
                            } else {
                                &call.arguments
                            };
                            // Malformed argument JSON is the model's mistake, not a
                            // run failure: return a retryable tool error instead.
                            let parsed = match serde_json::from_str(arguments) {
                                Ok(arguments) => arguments,
                                Err(error) => {
                                    call.argument_error = Some(format!(
                                        "tool call arguments were not valid JSON: {error}"
                                    ));
                                    serde_json::Value::Object(serde_json::Map::new())
                                }
                            };
                            call.arguments = serde_json::to_string(&parsed)
                                .expect("a parsed JSON value must serialize");
                            call.parsed_arguments = Some(parsed);
                            call.completed = true;
                        }
                        Ok(ProviderEvent::Completed { usage }) => {
                            if open_reasoning.is_some() {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: "provider completed the turn with an unfinished reasoning block".to_owned(),
                                };
                                return;
                            }
                            if let Some(call) = pending_calls.iter().find(|call| !call.completed) {
                                yield RuntimeEvent::Failed {
                                    kind: RunFailureKind::ProviderProtocol,
                                    message: format!(
                                        "provider completed the turn before tool call {:?}",
                                        call.provider_call_id
                                    ),
                                };
                                return;
                            }
                            terminal_usage = usage.map(provider_usage);
                            completed = true;
                            break;
                        }
                        Err(error) => {
                            yield RuntimeEvent::Failed {
                                kind: run_failure_kind(error.kind()),
                                message: error.to_string(),
                            };
                            return;
                        }
                    }
                }

                if !completed {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::ProviderProtocol,
                        message: "provider stream ended without a terminal event".to_owned(),
                    };
                    return;
                }

                let assistant_content = blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        TurnBlock::Text(text) if text.is_empty() => None,
                        TurnBlock::Text(text) => Some(ContentBlock::Text { text }),
                        TurnBlock::ToolCall(index) => {
                            let call = &pending_calls[index];
                            Some(ContentBlock::ToolCall {
                                id: call.provider_call_id.clone(),
                                name: call.name.clone(),
                                arguments: call
                                    .parsed_arguments
                                    .clone()
                                    .expect("completed calls have parsed arguments"),
                            })
                        }
                    })
                    .collect::<Vec<_>>();
                let assistant = Message::new(Role::Assistant, assistant_content);
                let mut calls = Vec::with_capacity(pending_calls.len());
                let mut id_generation_failed = None;
                for (index, pending) in pending_calls.into_iter().enumerate() {
                    let id = match ToolCallId::generate() {
                        Ok(id) => id,
                        Err(error) => {
                            id_generation_failed = Some(error.to_string());
                            break;
                        }
                    };
                    calls.push(RuntimeToolCall {
                        id,
                        turn_ordinal,
                        call_ordinal: u16::try_from(index + 1)
                            .expect("the per-turn tool bound fits u16"),
                        provider_call_id: pending.provider_call_id,
                        name: pending.name,
                        arguments: pending.arguments,
                        argument_error: pending.argument_error,
                    });
                }
                if let Some(message) = id_generation_failed {
                    yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Server,
                        message,
                    };
                    return;
                }
                // The completed turn and its requested calls travel on one event
                // so the store can persist them atomically.
                yield RuntimeEvent::AssistantTurnCompleted {
                    turn_ordinal,
                    message: assistant.clone(),
                    usage: terminal_usage,
                    calls: calls.clone(),
                };

                if calls.is_empty() {
                    yield RuntimeEvent::Completed;
                    return;
                }
                messages.push(assistant);

                // Policy resolves sequentially in request order, after the
                // turn and its `requested` call rows are persisted, so
                // approval prompts arrive one at a time. Calls with malformed
                // arguments never reach the gate: there is nothing executable
                // to approve, so they short-circuit to their tool error below.
                let mut results = vec![None; calls.len()];
                for (index, call) in calls.iter().enumerate() {
                    if call.argument_error.is_some() {
                        continue;
                    }
                    match gate.resolve(call).await {
                        GateDecision::Execute => {}
                        GateDecision::Deny { message } => {
                            results[index] = Some(tools::ToolExecutionResult {
                                content: message.clone(),
                                is_error: true,
                                file_state: None,
                            });
                            yield RuntimeEvent::ToolCallDenied { id: call.id, message };
                        }
                    }
                }
                let approved = calls
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| results[*index].is_none())
                    .map(|(_, call)| call.clone())
                    .collect::<Vec<_>>();
                for call in &approved {
                    yield RuntimeEvent::ToolCallStarted { id: call.id };
                }

                let execute_one = |call: RuntimeToolCall,
                                   output: Option<
                    tokio::sync::mpsc::UnboundedSender<String>,
                >| {
                    let workspace = workspace.clone();
                    let file_state = Arc::clone(&file_state);
                    let cancelled = Arc::clone(&cancelled);
                    let mcp = mcp.clone();
                    let spawner = spawner.clone();
                    async move {
                        let result = match call.argument_error.clone() {
                            Some(error) => tools::ToolExecutionResult {
                                content: error,
                                is_error: true,
                                file_state: None,
                            },
                            // spawn_agent dispatches to the session layer. A
                            // run without a spawner rejects the call outright:
                            // the declaration is already absent there, but a
                            // model may still guess the name.
                            None if call.name == tools::SPAWN_AGENT_TOOL => match &spawner {
                                Some(spawner) => {
                                    match serde_json::from_str::<tools::SpawnAgentArgs>(
                                        &call.arguments,
                                    ) {
                                        Ok(arguments) if arguments.task.trim().is_empty() => {
                                            tools::bounded_result(
                                                "task must not be empty".to_owned(),
                                                true,
                                            )
                                        }
                                        Ok(arguments) => {
                                            let outcome = spawner
                                                .spawn(arguments.task, arguments.model)
                                                .await;
                                            tools::bounded_result(outcome.content, outcome.is_error)
                                        }
                                        Err(error) => tools::bounded_result(
                                            format!("invalid arguments: {error}"),
                                            true,
                                        ),
                                    }
                                }
                                None => {
                                    tools::bounded_result(SPAWN_UNAVAILABLE_RESULT.to_owned(), true)
                                }
                            },
                            // MCP calls dispatch to the shared registry; the
                            // outcome flows through the same bounded-result
                            // truncation as built-in tools, so an MCP call is
                            // indistinguishable from a built-in on the wire.
                            None if call.name.starts_with(MCP_TOOL_PREFIX) && mcp.is_some() => {
                                let outcome = mcp
                                    .expect("the MCP registry was just checked")
                                    .call(call.name.clone(), call.arguments.clone(), cancelled)
                                    .await;
                                tools::bounded_result(outcome.content, outcome.is_error)
                            }
                            None => {
                                tools::execute(
                                    workspace,
                                    file_state,
                                    call.name.clone(),
                                    call.arguments.clone(),
                                    cancelled,
                                    output,
                                )
                                .await
                            }
                        };
                        (call, result)
                    }
                };
                // Read-only turns overlap under a small bound; a turn with any
                // mutating or shell call runs entirely in request order so
                // side effects never interleave and every read is
                // deterministically ordered against the mutations.
                let sequential = approved.iter().any(|call| {
                    !matches!(
                        approval::classify(&call.name, &call.arguments),
                        approval::ToolClass::ReadOnly
                    )
                });
                if sequential {
                    for call in approved {
                        // Live output chunks (shell) interleave with execution:
                        // drain the channel while the call runs so long
                        // commands render as they print.
                        let call_id = call.id;
                        let (delta_sender, mut deltas) =
                            tokio::sync::mpsc::unbounded_channel::<String>();
                        let mut execution = Box::pin(execute_one(call, Some(delta_sender)));
                        let (call, result) = loop {
                            tokio::select! {
                                biased;
                                chunk = deltas.recv() => match chunk {
                                    Some(chunk) => {
                                        yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                                    }
                                    // The execution dropped its sender early;
                                    // nothing more can stream, so just finish.
                                    None => break execution.await,
                                },
                                completed = &mut execution => break completed,
                            }
                        };
                        // Chunks sent in the execution's final poll may still
                        // be buffered; drain them before the terminal event.
                        while let Ok(chunk) = deltas.try_recv() {
                            yield RuntimeEvent::ToolCallOutputDelta { id: call_id, chunk };
                        }
                        results[usize::from(call.call_ordinal - 1)] = Some(result.clone());
                        let display = (!result.is_error)
                            .then(|| approval::edit_result_display(&call.name, &call.arguments))
                            .flatten();
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.content,
                            is_error: result.is_error,
                            file_state: result.file_state,
                            display,
                        };
                    }
                } else {
                    let mut executions = futures_stream::iter(
                        approved.into_iter().map(|call| execute_one(call, None)),
                    )
                        .buffer_unordered(MAX_PARALLEL_READS);
                    while let Some((call, result)) = executions.next().await {
                        results[usize::from(call.call_ordinal - 1)] = Some(result.clone());
                        yield RuntimeEvent::ToolCallFinished {
                            id: call.id,
                            result: result.content,
                            is_error: result.is_error,
                            file_state: result.file_state,
                            // Read-only turns never carry an edit display.
                            display: None,
                        };
                    }
                }
                let result_blocks = calls
                    .iter()
                    .zip(results.into_iter())
                    .map(|(call, result)| {
                        let result = result.expect("every bounded tool execution completed");
                        ContentBlock::ToolResult {
                            call_id: call.provider_call_id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                        }
                    })
                    .collect();
                messages.push(Message::tool_results(result_blocks));
            }

            yield RuntimeEvent::Failed {
                kind: RunFailureKind::Policy,
                message: format!("tool loop exceeded {MAX_MODEL_TURNS} model turns"),
            };
        })
    }
}

/// Version 5 of the base agent prompt. The text is versioned in code, not
/// configuration: bump this note and review the diff whenever it changes.
fn agent_system_prompt(workspace: &std::path::Path, specs: &[qq_provider::ToolSpec]) -> String {
    let mut tool_names = String::new();
    let mut has_mcp = false;
    let mut has_spawn = false;
    for spec in specs {
        if !tool_names.is_empty() {
            tool_names.push_str(", ");
        }
        tool_names.push_str(spec.name());
        has_mcp |= spec.name().starts_with(MCP_TOOL_PREFIX);
        has_spawn |= spec.name() == tools::SPAWN_AGENT_TOOL;
    }
    let mcp_note = if has_mcp {
        " Tools named mcp__<server>__<tool> call external MCP servers, execute outside the \
         workspace, and may require user approval."
    } else {
        ""
    };
    let spawn_section = if has_spawn {
        "\n\nDelegation:\n\
         - spawn_agent runs a one-shot read-only sub-agent in this workspace from a \
         self-contained task brief and returns only its final answer.\n\
         - Omit spawn_agent's model argument by default. QQ then uses the configured worker \
         model or this session's persisted selected model, including its authenticated provider. \
         Set model only when the user explicitly requests an exact provider/model route; never \
         guess, translate, or invent one.\n\
         - Delegate when all three hold: the raw evidence would dwarf the distilled answer, \
         you will not need that evidence verbatim later, and the task needs no mid-flight \
         steering.\n\
         - Default to working inline: single reads, searches, and quick lookups are never \
         worth a sub-agent.\n\
         - Exception: several independent questions are worth delegating even when each is \
         small, because sub-agents run concurrently."
    } else {
        ""
    };
    format!(
        "You are QQ, a coding agent operating in the workspace rooted at {root}.\n\
         \n\
         Available tools: {tool_names}. read_file, list_dir, and search are read-only; \
         edit_file and write_file modify workspace files and may require user approval; \
         shell runs one command in the workspace with a bounded timeout and may require user approval.{mcp_note}\n\
         \n\
         Working conventions:\n\
         - Read a file with read_file before editing or overwriting it; edits without a prior read in this session are rejected.\n\
         - Prefer search over guessing file paths.\n\
         - Give every tool path relative to the workspace root; absolute paths are rejected.\n\
         - Prefer edit_file and write_file over shell for changing files.{spawn_section}",
        root = workspace.display(),
    )
}

fn append_turn_text(blocks: &mut Vec<TurnBlock>, text: &str) {
    match blocks.last_mut() {
        Some(TurnBlock::Text(existing)) => existing.push_str(text),
        Some(TurnBlock::ToolCall(_)) | None => blocks.push(TurnBlock::Text(text.to_owned())),
    }
}

const fn provider_usage(usage: qq_provider::ProviderUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
    }
}

const fn run_failure_kind(kind: ProviderErrorKind) -> RunFailureKind {
    match kind {
        ProviderErrorKind::Configuration => RunFailureKind::ProviderConfiguration,
        ProviderErrorKind::Authentication => RunFailureKind::ProviderAuthentication,
        ProviderErrorKind::RateLimited => RunFailureKind::ProviderRateLimited,
        ProviderErrorKind::InvalidRequest => RunFailureKind::ProviderInvalidRequest,
        ProviderErrorKind::Unavailable => RunFailureKind::ProviderUnavailable,
        ProviderErrorKind::Transport => RunFailureKind::ProviderTransport,
        ProviderErrorKind::Api => RunFailureKind::ProviderApi,
        ProviderErrorKind::Response => RunFailureKind::ProviderResponse,
        ProviderErrorKind::Protocol => RunFailureKind::ProviderProtocol,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeConfigError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("maximum output tokens must be greater than zero")]
    ZeroMaxOutputTokens,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{StreamExt, stream};
    use qq_provider::{ProviderError, ProviderStream};

    use super::*;

    struct ScriptedProvider {
        request: Arc<Mutex<Option<ModelRequest>>>,
        fails: bool,
    }

    impl Provider for ScriptedProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            *self.request.lock().unwrap() = Some(request);

            if self.fails {
                return Box::pin(stream::once(async {
                    Err(ProviderError::Transport("offline".to_owned()))
                }));
            }

            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "hel".to_owned(),
                }),
                Ok(ProviderEvent::OutputTextDelta {
                    text: "lo".to_owned(),
                }),
                Ok(ProviderEvent::RefusalDelta {
                    text: " cannot continue".to_owned(),
                }),
                Ok(ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 12,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                    }),
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn maps_provider_events_to_protocol_events() {
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("say hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events,
            vec![
                RunEvent::Started,
                RunEvent::ActivityChanged {
                    activity: RunActivity::WaitingForProvider,
                },
                RunEvent::ActivityChanged {
                    activity: RunActivity::GeneratingResponse,
                },
                RunEvent::OutputTextDelta {
                    text: "hel".to_owned()
                },
                RunEvent::OutputTextDelta {
                    text: "lo".to_owned()
                },
                RunEvent::RefusalDelta {
                    text: " cannot continue".to_owned()
                },
                RunEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 12,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                    }
                },
                RunEvent::Completed,
            ]
        );

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(request.model(), "gpt-test");
        assert_eq!(request.max_output_tokens(), 256);
        assert_eq!(request.messages(), [Message::user("say hello")]);
    }

    #[tokio::test]
    async fn maps_reasoning_lifecycle_without_joining_answer_text() {
        struct ReasoningProvider;

        impl Provider for ReasoningProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ReasoningStarted {
                        kind: qq_provider::ReasoningKind::Summary,
                    }),
                    Ok(ProviderEvent::ReasoningDelta {
                        kind: qq_provider::ReasoningKind::Summary,
                        text: "checking constraints".to_owned(),
                    }),
                    Ok(ProviderEvent::ReasoningCompleted {
                        kind: qq_provider::ReasoningKind::Summary,
                    }),
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "answer".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let events = Runtime::new(ReasoningProvider, "gpt-test", 256)
            .unwrap()
            .run(RunCommand::new("solve it"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events,
            vec![
                RunEvent::Started,
                RunEvent::ActivityChanged {
                    activity: RunActivity::WaitingForProvider,
                },
                RunEvent::ActivityChanged {
                    activity: RunActivity::Reasoning,
                },
                RunEvent::ReasoningStarted {
                    kind: ReasoningKind::Summary,
                },
                RunEvent::ReasoningDelta {
                    kind: ReasoningKind::Summary,
                    text: "checking constraints".to_owned(),
                },
                RunEvent::ReasoningCompleted {
                    kind: ReasoningKind::Summary,
                },
                RunEvent::ActivityChanged {
                    activity: RunActivity::GeneratingResponse,
                },
                RunEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                },
                RunEvent::Completed,
            ]
        );
    }

    #[tokio::test]
    async fn passes_multi_turn_context_to_the_provider() {
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        runtime
            .run_messages(vec![
                Message::user("hey"),
                Message::assistant("Hello!"),
                Message::user("what was my first message?"),
            ])
            .collect::<Vec<_>>()
            .await;

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.messages(),
            [
                Message::user("hey"),
                Message::assistant("Hello!"),
                Message::user("what was my first message?"),
            ]
        );
    }

    #[tokio::test]
    async fn executes_read_tools_and_returns_results_in_request_order() {
        struct ToolLoopProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ToolLoopProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "read".to_owned(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "read".to_owned(),
                            json: r#"{"path":"note.txt"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "read".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "list".to_owned(),
                            name: "list_dir".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "list".to_owned(),
                            json: r#"{"path":"."}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "list".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "contents\n").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            ToolLoopProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    RuntimeEvent::AssistantTurnCompleted { calls, .. } => Some(calls.len()),
                    _ => None,
                })
                .sum::<usize>(),
            2
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools().len(), 6);
        let system = requests[0]
            .system()
            .expect("agent runs set a system prompt");
        assert!(system.contains("edit_file"));
        assert!(system.contains(directory.path().to_str().unwrap()));
        let result_message = &requests[1].messages()[2];
        assert!(matches!(
            result_message.content(),
            [
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    call_id: second_id,
                    content: second_content,
                    is_error: false,
                }
            ] if call_id == "read"
                && content == "contents\n"
                && second_id == "list"
                && second_content == "note.txt\n"
        ));
    }

    #[tokio::test]
    async fn read_tools_overlap_and_cancellation_stops_in_flight_work() {
        struct ConcurrentProvider {
            turn: Mutex<usize>,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for ConcurrentProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request);
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "slow".to_owned(),
                            name: "__test_delay".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "slow".to_owned(),
                            json: r#"{"delay_ms":50,"result":"slow","synchronize":true}"#
                                .to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "slow".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "fast".to_owned(),
                            name: "__test_delay".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "fast".to_owned(),
                            json: r#"{"delay_ms":1,"result":"fast","synchronize":true}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "fast".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            ConcurrentProvider {
                turn: Mutex::new(0),
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        let requested = events
            .iter()
            .flat_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                    calls.iter().map(|call| call.id).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let finished = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished, [requested[1], requested[0]]);
        assert!(matches!(
            requests.lock().unwrap()[1].messages()[2].content(),
            [
                ContentBlock::ToolResult { content, .. },
                ContentBlock::ToolResult {
                    content: second_content,
                    ..
                }
            ] if content == "slow" && second_content == "fast"
        ));

        let workspace = tools::Workspace::open(directory.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = tools::test_executions_started();
        let execution = tokio::spawn(tools::execute(
            workspace,
            Arc::new(tools::FileState::default()),
            "__test_delay".to_owned(),
            r#"{"delay_ms":500,"result":"late"}"#.to_owned(),
            Arc::clone(&cancelled),
            None,
        ));
        while tools::test_executions_started() == started {
            tokio::task::yield_now().await;
        }
        cancelled.store(true, Ordering::Release);
        let result = execution.await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
    }

    #[tokio::test]
    async fn mutating_calls_execute_sequentially_in_request_order() {
        struct MutatingTurnProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MutatingTurnProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "slow".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "slow".to_owned(),
                            json: r#"{"delay_ms":50,"result":"slow"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "slow".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "fast".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "fast".to_owned(),
                            json: r#"{"delay_ms":1,"result":"fast"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "fast".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MutatingTurnProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        // With the concurrent read path the fast call would finish first (as
        // the read-overlap test proves); a mutating turn must instead finish
        // in request order because side effects may not interleave.
        let events = runtime
            .run_loop(
                vec![Message::user("mutate twice")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(StaticPolicyGate {
                    mode: ApprovalMode::Auto,
                    grants: approval::SessionGrants::default(),
                }),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        let requested = events
            .iter()
            .flat_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { calls, .. } => {
                    calls.iter().map(|call| call.id).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let finished = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ToolCallFinished { id, result, .. } => Some((*id, result.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            finished.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            requested
        );
        assert_eq!(finished[0].1, "slow");
        assert_eq!(finished[1].1, "fast");
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_calls_stream_output_deltas_before_their_result() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        struct ShellProvider {
            turn: Mutex<usize>,
        }

        impl Provider for ShellProvider {
            fn stream(&self, _request: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "run".to_owned(),
                            name: "shell".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "run".to_owned(),
                            json: r#"{"command":"echo streamed-hello"}"#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "run".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            ShellProvider {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_loop(
                vec![Message::user("run the command")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        let started = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
            .unwrap();
        let delta = events
            .iter()
            .position(|event| matches!(
                event,
                RuntimeEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("streamed-hello")
            ))
            .expect("shell output must stream as deltas");
        let finished = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::ToolCallFinished { result, is_error: false, .. }
                        if result.contains("streamed-hello") && result.ends_with("exit code: 0")
                )
            })
            .expect("the bounded result must follow the streamed output");
        assert!(started < delta && delta < finished);
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[tokio::test]
    async fn invalid_tool_argument_json_yields_a_tool_error_and_continues_the_run() {
        struct MalformedArgumentsProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MalformedArgumentsProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "bad".to_owned(),
                            name: "read_file".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "bad".to_owned(),
                            json: r#"{"path": "#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "bad".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MalformedArgumentsProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("inspect")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { is_error: true, result, .. }
                if result.contains("not valid JSON")
        )));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "bad" && content.contains("not valid JSON")
        ));
    }

    #[tokio::test]
    async fn reports_the_underlying_workspace_open_error() {
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run_in_workspace(
                RunCommand::new("hello"),
                PathBuf::from("/qq-test-missing-workspace"),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            events.as_slice(),
            [
                RunEvent::Started,
                RunEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message,
                }
            ] if message.contains("could not open the workspace directory")
                && message.len() > "could not open the workspace directory: ".len()
        ));
    }

    #[tokio::test]
    async fn gate_less_runs_deny_mutating_tools_and_return_the_error_to_the_model() {
        struct MutatingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MutatingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "call_0".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_0".to_owned(),
                            json: "{}".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "call_0".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = Runtime::new(
            MutatingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("mutate")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallDenied { message, .. }
                if message == approval::UNATTENDED_DENIED_RESULT
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolCallStarted { .. }))
        );
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        let requests = requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            }] if content == approval::UNATTENDED_DENIED_RESULT
        ));
    }

    #[tokio::test]
    async fn calls_with_malformed_arguments_short_circuit_without_consulting_the_gate() {
        struct RecordingGate {
            consulted: Arc<AtomicBool>,
        }

        impl ToolGate for RecordingGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                self.consulted.store(true, Ordering::Release);
                Box::pin(std::future::ready(GateDecision::Deny {
                    message: "the gate must not see unexecutable calls".to_owned(),
                }))
            }
        }

        struct MalformedMutatingProvider {
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for MalformedMutatingProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut requests = self.requests.lock().unwrap();
                let turn = requests.len();
                requests.push(request);
                drop(requests);
                if turn == 0 {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "bad".to_owned(),
                            name: "__test_mutate".to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "bad".to_owned(),
                            json: r#"{"broken": "#.to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "bad".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let consulted = Arc::new(AtomicBool::new(false));
        let runtime = Runtime::new(
            MalformedMutatingProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();

        // Even though the tool is mutating and the gate would deny it, a call
        // with malformed arguments has nothing executable to approve: it must
        // return its argument error without an approval round trip.
        let events = runtime
            .run_loop(
                vec![Message::user("mutate")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(RecordingGate {
                    consulted: Arc::clone(&consulted),
                }),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(!consulted.load(Ordering::Acquire));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolCallDenied { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { is_error: true, result, .. }
                if result.contains("not valid JSON")
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    #[tokio::test]
    async fn fails_when_provider_completes_with_an_unfinished_tool_call() {
        struct ToolCallingProvider;

        impl Provider for ToolCallingProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_1".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let runtime = Runtime::new(ToolCallingProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::ActivityChanged {
                activity: RunActivity::PreparingToolCall
            }
        )));
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_provider_tool_metadata() {
        struct OversizedMetadataProvider;

        impl Provider for OversizedMetadataProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([Ok(ProviderEvent::ToolCallStarted {
                    id: "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1),
                    name: "read_file".to_owned(),
                })]))
            }
        }

        let runtime = Runtime::new(OversizedMetadataProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn bounds_model_text_across_the_entire_tool_loop() {
        struct OversizedTextProvider;

        impl Provider for OversizedTextProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                Box::pin(stream::iter([Ok(ProviderEvent::OutputTextDelta {
                    text: "x".repeat(MAX_RUN_MODEL_TEXT_BYTES + 1),
                })]))
            }
        }

        let runtime = Runtime::new(OversizedTextProvider, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::Policy,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn enforces_per_turn_and_per_run_tool_call_limits() {
        struct TooManyInOneTurn;

        impl Provider for TooManyInOneTurn {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let events = (0..=MAX_TOOL_CALLS_PER_TURN)
                    .map(|index| {
                        Ok(ProviderEvent::ToolCallStarted {
                            id: format!("call-{index}"),
                            name: "read_file".to_owned(),
                        })
                    })
                    .collect::<Vec<Result<_, ProviderError>>>();
                Box::pin(stream::iter(events))
            }
        }

        let runtime = Runtime::new(TooManyInOneTurn, "gpt-test", 256).unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                ..
            })
        ));

        struct RespectsFinalTurn {
            turn: Arc<Mutex<usize>>,
        }

        impl Provider for RespectsFinalTurn {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                // The reserved final turn declares no tools; a conforming
                // provider answers with text and completes.
                if request.tools().is_empty() {
                    return Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "stopping point recorded".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]));
                }
                let mut events = Vec::with_capacity(MAX_TOOL_CALLS_PER_TURN * 3 + 1);
                for index in 0..MAX_TOOL_CALLS_PER_TURN {
                    let id = format!("call-{current}-{index}");
                    events.push(Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }));
                    events.push(Ok(ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            }
        }

        // A model that would consume tools forever settles gracefully:
        // exhausting the budget triggers one tool-free final turn and the
        // run completes, leaving the session resumable instead of failed.
        let turns = Arc::new(Mutex::new(0));
        let runtime = Runtime::new(
            RespectsFinalTurn {
                turn: Arc::clone(&turns),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.last(), Some(&RunEvent::Completed));
        assert!(events.iter().any(|event| matches!(
            event,
            RunEvent::OutputTextDelta { text } if text == "stopping point recorded"
        )));
        assert_eq!(
            *turns.lock().unwrap(),
            MAX_TOOL_CALLS_PER_RUN / MAX_TOOL_CALLS_PER_TURN + 1
        );

        struct FiniteToolTurns {
            turn: Mutex<usize>,
        }

        impl Provider for FiniteToolTurns {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let current = *turn;
                *turn += 1;
                drop(turn);
                if current == MAX_TOOL_CALLS_PER_RUN {
                    return Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]));
                }
                let id = format!("call-{current}");
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let runtime = Runtime::new(
            FiniteToolTurns {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.last(), Some(&RunEvent::Completed));

        struct EndlessToolTurns {
            turn: Mutex<usize>,
        }

        impl Provider for EndlessToolTurns {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut turn = self.turn.lock().unwrap();
                let id = format!("call-{}", *turn);
                *turn += 1;
                drop(turn);
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "unknown".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: "{}".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted { id }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }

        let runtime = Runtime::new(
            EndlessToolTurns {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;
        // A provider that emits a tool call on the reserved tool-free
        // final turn is violating the request, not the run policy.
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                message,
            }) if message.contains("reserved final turn")
        ));
    }

    #[tokio::test]
    async fn turns_provider_errors_into_failed_events() {
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: true,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ProviderTransport,
                message,
            }) if message.contains("offline")
        ));
    }

    struct MockMcpRegistry {
        specs: Vec<qq_provider::ToolSpec>,
        grants: Vec<String>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
        result: McpToolResult,
    }

    impl MockMcpRegistry {
        fn returning(result: McpToolResult) -> Self {
            Self {
                specs: vec![qq_provider::ToolSpec::new(
                    "mcp__srv__ping",
                    "Ping the fixture server.",
                    serde_json::json!({"type": "object"}),
                )],
                grants: Vec::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
                result,
            }
        }
    }

    impl McpRegistry for MockMcpRegistry {
        fn tool_specs(&self) -> McpSpecsFuture {
            let specs = self.specs.clone();
            Box::pin(async move { specs })
        }

        fn config_grants(&self) -> Vec<String> {
            self.grants.clone()
        }

        fn call(
            &self,
            name: String,
            arguments: String,
            _cancelled: Arc<AtomicBool>,
        ) -> McpCallFuture {
            self.calls.lock().unwrap().push((name, arguments.clone()));
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    /// Scripts one `mcp__srv__ping` call on the first turn, then completes.
    struct McpCallProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for McpCallProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "mcp__srv__ping".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"value":1}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    #[tokio::test]
    async fn merges_mcp_declarations_and_dispatches_granted_calls_to_the_registry() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = MockMcpRegistry::returning(McpToolResult {
            content: "pong".to_owned(),
            is_error: false,
        });
        // A spec that violates the namespace contract must be discarded.
        registry.specs.push(qq_provider::ToolSpec::new(
            "rogue_tool",
            "not namespaced",
            serde_json::json!({"type": "object"}),
        ));
        // The configuration allowlist covers the call, so gate-less Ask mode
        // executes it without an approval round trip.
        registry.grants = vec!["mcp__srv__ping".to_owned()];
        let calls = Arc::clone(&registry.calls);
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));

        let events = runtime
            .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: false, .. } if result == "pong"
        )));
        let requests = requests.lock().unwrap();
        let names = requests[0]
            .tools()
            .iter()
            .map(qq_provider::ToolSpec::name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"mcp__srv__ping"));
        assert!(
            !names.contains(&"rogue_tool"),
            "specs outside the mcp__ namespace must be discarded"
        );
        assert_eq!(requests[0].tools().len(), 7);
        let system = requests[0].system().unwrap();
        assert!(system.contains("mcp__srv__ping"));
        assert!(system.contains("MCP servers"));
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: false,
            }] if call_id == "call_0" && content == "pong"
        ));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [("mcp__srv__ping".to_owned(), r#"{"value":1}"#.to_owned())]
        );
    }

    #[tokio::test]
    async fn mcp_failures_are_tool_errors_and_results_are_truncated() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: "the server exploded".to_owned(),
            is_error: true,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result == "the server exploded"
        )));
        assert!(
            matches!(events.last(), Some(RuntimeEvent::Completed)),
            "an MCP failure must never fail the run"
        );

        let oversized = "x".repeat(tools::MAX_TOOL_RESULT_BYTES + 1024);
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: oversized,
            is_error: false,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        let result = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("the oversized MCP result must still finish");
        assert!(result.len() <= tools::MAX_TOOL_RESULT_BYTES);
        assert!(result.contains("truncated by qq"));
    }

    #[tokio::test]
    async fn ungranted_mcp_calls_are_denied_unattended_and_unknown_names_error() {
        let directory = tempfile::tempdir().unwrap();
        let registry = MockMcpRegistry::returning(McpToolResult {
            content: "pong".to_owned(),
            is_error: false,
        });
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_mcp_registry(Arc::new(registry));
        // No configuration grant covers the call: gate-less Ask mode denies
        // without executing, and the run still completes.
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("ping")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallDenied { message, .. }
                if message == approval::UNATTENDED_DENIED_RESULT
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));

        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        // Without a registry, an approved mcp__ call falls through to the
        // built-in dispatcher's precise unknown-tool error.
        let runtime = Runtime::new(
            McpCallProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop(
                vec![Message::user("ping")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(tools::FileState::default()),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result.contains("unknown tool")
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
    }

    /// Scripts one `spawn_agent` call on the first turn, then completes.
    struct SpawnCallProvider {
        turn: Mutex<usize>,
    }

    impl Provider for SpawnCallProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            if current == 0 {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "spawn_agent".to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"task":"count the widgets"}"#.to_owned(),
                    }),
                    Ok(ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    type SpawnedTasks = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct StubSpawner {
        outcome: SpawnAgentOutcome,
        tasks: SpawnedTasks,
    }

    impl SubagentSpawner for StubSpawner {
        fn spawn(&self, task: String, model: Option<String>) -> SpawnAgentFuture {
            self.tasks.lock().unwrap().push((task, model));
            let outcome = self.outcome.clone();
            Box::pin(std::future::ready(outcome))
        }
    }

    #[tokio::test]
    async fn spawner_less_runs_neither_declare_nor_dispatch_spawn_agent() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));

        struct CapturingSpawnProvider {
            inner: SpawnCallProvider,
            requests: Arc<Mutex<Vec<ModelRequest>>>,
        }

        impl Provider for CapturingSpawnProvider {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.requests.lock().unwrap().push(request.clone());
                self.inner.stream(request)
            }
        }

        let runtime = Runtime::new(
            CapturingSpawnProvider {
                inner: SpawnCallProvider {
                    turn: Mutex::new(0),
                },
                requests: Arc::clone(&requests),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        // run_messages_in_workspace passes no spawner: the tool must be
        // absent from the declarations and rejected by dispatch.
        let events = runtime
            .run_messages_in_workspace(vec![Message::user("go")], directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
                if result == SPAWN_UNAVAILABLE_RESULT
        )));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        let requests = requests.lock().unwrap();
        assert!(
            !requests[0]
                .tools()
                .iter()
                .any(|spec| spec.name() == tools::SPAWN_AGENT_TOOL)
        );
        let system = requests[0].system().unwrap();
        assert!(!system.contains("Delegation:"));
    }

    #[tokio::test]
    async fn spawner_runs_declare_the_tool_and_truncate_oversized_child_answers() {
        struct AllowAllGate;

        impl ToolGate for AllowAllGate {
            fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
                Box::pin(std::future::ready(GateDecision::Execute))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(StubSpawner {
            outcome: SpawnAgentOutcome {
                content: "x".repeat(tools::MAX_TOOL_RESULT_BYTES + 1024),
                is_error: false,
            },
            tasks: Arc::clone(&tasks),
        });
        let runtime = Runtime::new(
            SpawnCallProvider {
                turn: Mutex::new(0),
            },
            "gpt-test",
            256,
        )
        .unwrap();
        let events = runtime
            .run_loop_with_spawner(
                vec![Message::user("go")],
                directory.path().to_owned(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AllowAllGate),
                Arc::new(tools::FileState::default()),
                Some(spawner),
            )
            .collect::<Vec<_>>()
            .await;

        let result = events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::ToolCallFinished {
                    result,
                    is_error: false,
                    ..
                } => Some(result.clone()),
                _ => None,
            })
            .expect("the spawn call must finish successfully");
        assert!(result.len() <= tools::MAX_TOOL_RESULT_BYTES);
        assert!(result.contains("truncated by qq"));
        assert!(matches!(events.last(), Some(RuntimeEvent::Completed)));
        assert_eq!(
            tasks.lock().unwrap().as_slice(),
            [("count the widgets".to_owned(), None)]
        );
    }

    #[test]
    fn agent_prompt_teaches_delegation_only_when_spawn_agent_is_declared() {
        let workspace = std::path::Path::new("/tmp/qq-prompt-test");
        let without = agent_system_prompt(workspace, &tools::specs());
        assert!(!without.contains("spawn_agent"));
        assert!(!without.contains("Delegation:"));

        let mut specs = tools::specs();
        specs.push(tools::spawn_agent_spec());
        let with = agent_system_prompt(workspace, &specs);
        assert!(with.contains("spawn_agent"));
        assert!(with.contains("Delegation:"));
        assert!(with.contains("independent questions"));
        assert!(with.contains("read-only sub-agent"));
        assert!(with.contains("Omit spawn_agent's model argument by default"));
        assert!(with.contains("configured worker model"));
        assert!(with.contains("persisted selected model"));
        assert!(with.contains("never guess, translate, or invent one"));
    }
}
