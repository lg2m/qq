use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{CommandId, MessageId, RunFailureKind, RunId, SessionId, StoreId, WorkspaceId};

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
    pub provenance: String,
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
    },
    SubmitPrompt {
        session_id: SessionId,
        prompt: String,
    },
    CancelRun {
        run_id: RunId,
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
    pub role: MessageRole,
    pub state: MessageState,
    pub output: String,
    pub refusal: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub messages: Vec<MessageSnapshot>,
    pub runs: Vec<RunSnapshot>,
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
    CancellationRequested {
        session: SessionSummary,
        run_id: RunId,
    },
    RunFinished {
        session: SessionSummary,
        run_id: RunId,
        outcome: RunOutcome,
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

    from_id_bytes!(StoreId, WorkspaceId, SessionId, RunId, MessageId, CommandId);

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
}
