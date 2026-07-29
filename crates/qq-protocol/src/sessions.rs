use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    CommandId, MessageId, RunFailureKind, RunId, SessionId, StoreId, ToolCallId, WorkspaceId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub store_id: StoreId,
    pub workspace_id: WorkspaceId,
    pub sequence: u64,
}

impl fmt::Display for EventCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}:{}",
            self.store_id, self.workspace_id, self.sequence
        )
    }
}

impl FromStr for EventCursor {
    type Err = CursorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        let store_id = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        let workspace_id = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        let sequence = parts
            .next()
            .ok_or(CursorError)?
            .parse()
            .map_err(|_| CursorError)?;
        if parts.next().is_some() {
            return Err(CursorError);
        }
        Ok(Self {
            store_id,
            workspace_id,
            sequence,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorError;

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid event cursor")
    }
}

impl std::error::Error for CursorError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tier: Option<ModelPricingTier>,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricingTier {
    pub above_input_tokens: u64,
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogRequest {
    pub workspace: String,
    pub selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    pub selection: ModelSelection,
}

/// Per-session policy for tool calls that mutate state or leave the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    ReadOnly,
    #[default]
    Ask,
    Auto,
}

/// A client's answer to one pending tool approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalDecision {
    ApproveOnce,
    ApproveForSession { grant: ApprovalGrant },
    Deny,
}

/// A session-scoped allowlist entry recorded by approve-for-session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalGrant {
    Tool { name: String },
    ShellPrefix { prefix: String },
}

/// The durable outcome of one tool approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolution {
    ApprovedOnce,
    ApprovedForSession,
    Denied,
    DeniedTimeout,
}

/// Shell details carried by an approval request so clients can decide in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellCommandPreview {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Edit details carried by an approval request so clients can decide in place:
/// the workspace-relative path and a bounded unified-diff-style preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditPreview {
    pub path: String,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    ResolveWorkspace {
        path: String,
    },
    CreateSession {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<SessionId>,
        model: ModelSelection,
        #[serde(default)]
        approval_mode: ApprovalMode,
    },
    SubmitPrompt {
        session_id: SessionId,
        prompt: String,
    },
    CancelRun {
        run_id: RunId,
    },
    RespondToolApproval {
        run_id: RunId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    },
    SetApprovalMode {
        session_id: SessionId,
        mode: ApprovalMode,
    },
    /// Repoints the session's model. Takes effect when the next run is
    /// claimed; a run already executing keeps the model it started with.
    SetSessionModel {
        session_id: SessionId,
        model: ModelSelection,
    },
    /// Deletes an idle session and every row it owns. Refused while the
    /// session has an active run; the client cancels first.
    DeleteSession {
        session_id: SessionId,
    },
    /// Deletes every idle session in the workspace that never received a
    /// message (the residue of creating sessions without prompting them).
    PruneSessions {
        workspace_id: WorkspaceId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub command_id: CommandId,
    pub command: SessionCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceipt {
    pub command_id: CommandId,
    pub committed_through: EventCursor,
    pub outcome: CommandOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandOutcome {
    WorkspaceResolved {
        workspace_id: WorkspaceId,
    },
    SessionCreated {
        session_id: SessionId,
    },
    PromptQueued {
        session_id: SessionId,
        run_id: RunId,
        queue_position: u16,
    },
    CancellationRequested {
        run_id: RunId,
    },
    RunAlreadyFinished {
        run_id: RunId,
        outcome: RunOutcome,
    },
    ToolApprovalResolved {
        tool_call_id: ToolCallId,
        resolution: ApprovalResolution,
    },
    ApprovalModeSet {
        session_id: SessionId,
        mode: ApprovalMode,
    },
    SessionModelSet {
        session_id: SessionId,
        model: ModelSelection,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    SessionsPruned {
        workspace_id: WorkspaceId,
        deleted: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Queued,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Cancelled,
    Interrupted,
    Failed { failure: RunFailure },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFailure {
    pub kind: RunFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Queued,
    Streaming,
    Complete,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Requested,
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Denied,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextChannel {
    Output,
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSummary {
    pub id: WorkspaceId,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    pub title: String,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
    pub queued_prompts: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<RunOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub id: RunId,
    pub session_id: SessionId,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd_nanos: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSnapshot {
    pub id: MessageId,
    pub session_id: SessionId,
    pub run_id: RunId,
    /// The model turn this message belongs to within its run. User messages
    /// (and assistant messages persisted before per-turn messages existed)
    /// carry 0; assistant turns are 1-based, matching tool-call ordinals.
    #[serde(default)]
    pub turn_ordinal: u16,
    pub role: MessageRole,
    pub state: MessageState,
    pub output: String,
    pub refusal: String,
    pub created_at_ms: u64,
}

/// A UI-facing rendering payload carried alongside a tool call's result.
/// Display-only: model context assembly never includes it, so it can hold
/// richer content than the bounded result string the model sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallDisplay {
    /// A bounded unified-diff-style rendering of a completed
    /// `edit_file`/`write_file` call.
    Diff { path: String, diff: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallSnapshot {
    pub id: ToolCallId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_ordinal: u16,
    pub call_ordinal: u16,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
    pub state: ToolCallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolCallDisplay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub messages: Vec<MessageSnapshot>,
    pub runs: Vec<RunSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallSnapshot>,
    #[serde(default)]
    pub has_older_tool_calls: bool,
    pub has_older_messages: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub cursor: EventCursor,
    pub workspace: WorkspaceSummary,
    pub sessions: Vec<SessionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<SessionSnapshot>,
    pub has_older_sessions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_session_id: Option<SessionId>,
    pub session_limit: u16,
    pub message_limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {
    pub workspace_id: WorkspaceId,
    pub after: EventCursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEventEnvelope {
    pub cursor: EventCursor,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CommandId>,
    pub occurred_at_ms: u64,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionCreated {
        session: SessionSummary,
    },
    /// A non-run mutation of the session row (today: its model selection).
    /// Carries the full updated summary so clients re-render without a
    /// round trip.
    SessionUpdated {
        session: SessionSummary,
    },
    /// The session and every row it owned were deleted. Earlier events for
    /// the session remain in the workspace log; replaying them and then this
    /// event converges every client on the deleted state.
    SessionDeleted {
        session_id: SessionId,
    },
    PromptQueued {
        session: SessionSummary,
        message: MessageSnapshot,
        run: RunSnapshot,
        queue_position: u16,
    },
    RunStarted {
        session: SessionSummary,
        run_id: RunId,
    },
    AssistantMessageStarted {
        message: MessageSnapshot,
    },
    TextAppended {
        message_id: MessageId,
        channel: TextChannel,
        text: String,
    },
    ToolCallRequested {
        tool_call: ToolCallSnapshot,
    },
    ToolApprovalRequested {
        tool_call: ToolCallSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<ShellCommandPreview>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit: Option<EditPreview>,
    },
    ToolApprovalResolved {
        tool_call: ToolCallSnapshot,
        resolution: ApprovalResolution,
    },
    ToolCallStarted {
        tool_call: ToolCallSnapshot,
    },
    /// A chunk of live tool output (streamed shell output), batched like text
    /// deltas. Chunks are display-only: the authoritative bounded result
    /// arrives on the `ToolCallFinished` snapshot.
    ToolCallOutputDelta {
        tool_call_id: ToolCallId,
        chunk: String,
    },
    ToolCallFinished {
        tool_call: ToolCallSnapshot,
    },
    CancellationRequested {
        session: SessionSummary,
        run_id: RunId,
    },
    RunFinished {
        session: SessionSummary,
        run_id: RunId,
        outcome: RunOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T>(byte: u8) -> T
    where
        T: FromIdBytes,
    {
        T::from_id_bytes([byte; 16])
    }

    trait FromIdBytes {
        fn from_id_bytes(bytes: [u8; 16]) -> Self;
    }

    macro_rules! from_id_bytes {
        ($($kind:ty),+ $(,)?) => {$(
            impl FromIdBytes for $kind {
                fn from_id_bytes(bytes: [u8; 16]) -> Self {
                    Self::from_bytes(bytes)
                }
            }
        )+};
    }

    from_id_bytes!(
        StoreId,
        WorkspaceId,
        SessionId,
        RunId,
        MessageId,
        ToolCallId,
        CommandId
    );

    #[test]
    fn session_events_have_a_stable_tagged_wire_shape() {
        let workspace_id = id::<WorkspaceId>(2);
        let envelope = SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(1),
                workspace_id,
                sequence: 7,
            },
            session_id: id(3),
            run_id: Some(id(4)),
            caused_by: Some(id(5)),
            occurred_at_ms: 11,
            event: SessionEvent::TextAppended {
                message_id: id(6),
                channel: TextChannel::Output,
                text: "hello".to_owned(),
            },
        };

        let encoded = serde_json::to_value(&envelope).unwrap();

        assert_eq!(encoded["cursor"]["sequence"], 7);
        assert_eq!(encoded["event"]["type"], "text_appended");
        assert_eq!(encoded["event"]["channel"], "output");
        assert_eq!(
            serde_json::from_value::<SessionEventEnvelope>(encoded).unwrap(),
            envelope
        );
        assert_eq!(envelope.cursor.to_string().parse(), Ok(envelope.cursor));
    }

    #[test]
    fn tool_call_events_have_a_stable_tagged_wire_shape() {
        let event = SessionEvent::ToolCallFinished {
            tool_call: ToolCallSnapshot {
                id: id(7),
                session_id: id(3),
                run_id: id(4),
                turn_ordinal: 1,
                call_ordinal: 2,
                provider_call_id: "call_2".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"README.md"}"#.to_owned(),
                state: ToolCallState::Completed,
                result: Some("QQ".to_owned()),
                is_error: false,
                display: None,
            },
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "tool_call_finished");
        assert_eq!(encoded["tool_call"]["state"], "completed");
        assert_eq!(encoded["tool_call"]["name"], "read_file");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn tool_call_display_payloads_round_trip_and_legacy_payloads_decode_to_none() {
        let tool_call = ToolCallSnapshot {
            id: id(7),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_1".to_owned(),
            name: "edit_file".to_owned(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
            state: ToolCallState::Completed,
            result: Some("Edited src/lib.rs: replaced 1 occurrence(s).".to_owned()),
            is_error: false,
            display: Some(ToolCallDisplay::Diff {
                path: "src/lib.rs".to_owned(),
                diff: "- old\n+ new\n".to_owned(),
            }),
        };

        let encoded = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(encoded["display"]["type"], "diff");
        assert_eq!(encoded["display"]["diff"], "- old\n+ new\n");
        assert_eq!(
            serde_json::from_value::<ToolCallSnapshot>(encoded).unwrap(),
            tool_call
        );

        // Calls persisted before the protocol carried a display payload must
        // still decode; legacy snapshots default to no payload.
        let mut legacy = serde_json::to_value(&tool_call).unwrap();
        legacy.as_object_mut().unwrap().remove("display");
        let decoded = serde_json::from_value::<ToolCallSnapshot>(legacy).unwrap();
        assert_eq!(decoded.display, None);
        assert_eq!(decoded.result, tool_call.result);

        // Snapshots without a payload keep their previous wire shape.
        let bare = ToolCallSnapshot {
            display: None,
            ..tool_call
        };
        let encoded = serde_json::to_value(&bare).unwrap();
        assert!(encoded.get("display").is_none());
        assert_eq!(
            serde_json::from_value::<ToolCallSnapshot>(encoded).unwrap(),
            bare
        );
    }

    #[test]
    fn tool_output_deltas_have_a_stable_tagged_wire_shape() {
        let event = SessionEvent::ToolCallOutputDelta {
            tool_call_id: id(7),
            chunk: "Compiling qq-core v0.1.0\n".to_owned(),
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "tool_call_output_delta");
        assert_eq!(encoded["chunk"], "Compiling qq-core v0.1.0\n");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }

    #[test]
    fn approval_events_and_commands_round_trip_with_stable_tags() {
        let tool_call = ToolCallSnapshot {
            id: id(7),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_1".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"cargo test"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };
        let requested = SessionEvent::ToolApprovalRequested {
            tool_call: tool_call.clone(),
            shell: Some(ShellCommandPreview {
                command: "cargo test".to_owned(),
                cwd: Some("crates/qq-core".to_owned()),
            }),
            edit: None,
        };
        let encoded = serde_json::to_value(&requested).unwrap();
        assert_eq!(encoded["type"], "tool_approval_requested");
        assert_eq!(encoded["tool_call"]["state"], "awaiting_approval");
        assert_eq!(encoded["shell"]["command"], "cargo test");
        assert!(encoded.get("edit").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            requested
        );

        let edit_requested = SessionEvent::ToolApprovalRequested {
            tool_call: tool_call.clone(),
            shell: None,
            edit: Some(EditPreview {
                path: "src/lib.rs".to_owned(),
                diff: "- old\n+ new".to_owned(),
            }),
        };
        let encoded = serde_json::to_value(&edit_requested).unwrap();
        assert_eq!(encoded["edit"]["path"], "src/lib.rs");
        assert_eq!(encoded["edit"]["diff"], "- old\n+ new");
        assert!(encoded.get("shell").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            edit_requested
        );

        let resolved = SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        };
        let encoded = serde_json::to_value(&resolved).unwrap();
        assert_eq!(encoded["type"], "tool_approval_resolved");
        assert_eq!(encoded["resolution"], "denied_timeout");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            resolved
        );

        let command = SessionCommand::RespondToolApproval {
            run_id: id(4),
            tool_call_id: id(7),
            decision: ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test".to_owned(),
                },
            },
        };
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["type"], "respond_tool_approval");
        assert_eq!(encoded["decision"]["type"], "approve_for_session");
        assert_eq!(encoded["decision"]["grant"]["type"], "shell_prefix");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            command
        );

        let outcome = CommandOutcome::ToolApprovalResolved {
            tool_call_id: id(7),
            resolution: ApprovalResolution::ApprovedOnce,
        };
        let encoded = serde_json::to_value(&outcome).unwrap();
        assert_eq!(encoded["type"], "tool_approval_resolved");
        assert_eq!(encoded["resolution"], "approved_once");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            outcome
        );
    }

    #[test]
    fn message_snapshots_round_trip_turn_ordinal_and_default_legacy_payloads_to_zero() {
        let message = MessageSnapshot {
            id: id(6),
            session_id: id(3),
            run_id: id(4),
            turn_ordinal: 2,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            output: "checking".to_owned(),
            refusal: String::new(),
            created_at_ms: 11,
        };
        let event = SessionEvent::AssistantMessageStarted {
            message: message.clone(),
        };

        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "assistant_message_started");
        assert_eq!(encoded["message"]["turn_ordinal"], 2);
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );

        // Events persisted before the protocol carried turn_ordinal must
        // still decode; legacy messages default to turn 0.
        let mut legacy = serde_json::to_value(&message).unwrap();
        legacy.as_object_mut().unwrap().remove("turn_ordinal");
        let decoded = serde_json::from_value::<MessageSnapshot>(legacy).unwrap();
        assert_eq!(decoded.turn_ordinal, 0);
        assert_eq!(decoded.output, "checking");
    }

    #[test]
    fn create_session_without_an_approval_mode_defaults_to_ask() {
        let encoded = serde_json::json!({
            "type": "create_session",
            "workspace_id": id::<WorkspaceId>(2).to_string(),
            "model": { "model": "test/model" },
        });
        let command = serde_json::from_value::<SessionCommand>(encoded).unwrap();
        assert!(matches!(
            command,
            SessionCommand::CreateSession {
                approval_mode: ApprovalMode::Ask,
                ..
            }
        ));
    }

    #[test]
    fn session_management_commands_round_trip_with_stable_tags() {
        let session_id = id::<SessionId>(3);
        let workspace_id = id::<WorkspaceId>(2);

        let set_model = SessionCommand::SetSessionModel {
            session_id,
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: Some(256),
                organization: None,
            },
        };
        let encoded = serde_json::to_value(&set_model).unwrap();
        assert_eq!(encoded["type"], "set_session_model");
        assert_eq!(encoded["model"]["model"], "test/model");
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            set_model
        );

        let delete = SessionCommand::DeleteSession { session_id };
        let encoded = serde_json::to_value(&delete).unwrap();
        assert_eq!(encoded["type"], "delete_session");
        assert_eq!(encoded["session_id"], session_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            delete
        );

        let prune = SessionCommand::PruneSessions { workspace_id };
        let encoded = serde_json::to_value(&prune).unwrap();
        assert_eq!(encoded["type"], "prune_sessions");
        assert_eq!(encoded["workspace_id"], workspace_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionCommand>(encoded).unwrap(),
            prune
        );

        let model_set = CommandOutcome::SessionModelSet {
            session_id,
            model: ModelSelection {
                model: Some("test/model".to_owned()),
                max_output_tokens: None,
                organization: None,
            },
        };
        let encoded = serde_json::to_value(&model_set).unwrap();
        assert_eq!(encoded["type"], "session_model_set");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            model_set
        );

        let deleted = CommandOutcome::SessionDeleted { session_id };
        let encoded = serde_json::to_value(&deleted).unwrap();
        assert_eq!(encoded["type"], "session_deleted");
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            deleted
        );

        let pruned = CommandOutcome::SessionsPruned {
            workspace_id,
            deleted: 3,
        };
        let encoded = serde_json::to_value(&pruned).unwrap();
        assert_eq!(encoded["type"], "sessions_pruned");
        assert_eq!(encoded["deleted"], 3);
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(encoded).unwrap(),
            pruned
        );
    }

    #[test]
    fn session_update_and_deletion_events_round_trip_with_stable_tags() {
        let session_id = id::<SessionId>(3);
        let updated = SessionEvent::SessionUpdated {
            session: SessionSummary {
                id: session_id,
                workspace_id: id(2),
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model-b".to_owned()),
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: None,
            },
        };
        let encoded = serde_json::to_value(&updated).unwrap();
        assert_eq!(encoded["type"], "session_updated");
        assert_eq!(encoded["session"]["model"], "test/model-b");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            updated
        );

        let deleted = SessionEvent::SessionDeleted { session_id };
        let encoded = serde_json::to_value(&deleted).unwrap();
        assert_eq!(encoded["type"], "session_deleted");
        assert_eq!(encoded["session_id"], session_id.to_string());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            deleted
        );
    }

    #[test]
    fn run_finished_without_usage_retains_its_previous_wire_shape() {
        let workspace_id = id::<WorkspaceId>(2);
        let session_id = id::<SessionId>(3);
        let event = SessionEvent::RunFinished {
            session: SessionSummary {
                id: session_id,
                workspace_id,
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("test/model".to_owned()),
                estimated_cost_usd_nanos: None,
                updated_at_ms: 11,
                last_outcome: Some(RunOutcome::Completed),
            },
            run_id: id(4),
            outcome: RunOutcome::Completed,
            usage: None,
        };

        let encoded = serde_json::to_value(&event).unwrap();

        assert!(encoded.get("usage").is_none());
        assert_eq!(
            serde_json::from_value::<SessionEvent>(encoded).unwrap(),
            event
        );
    }
}
