export type Id = string;

export interface EventCursor {
  store_id: Id;
  workspace_id: Id;
  sequence: number;
}

export interface ServerInfo {
  protocol_version: number;
  version: string;
  pid: number;
}

export interface WorkspaceSummary {
  id: Id;
  path: string;
}

export type SessionStatus = "idle" | "queued" | "running";
export type RunStatus =
  | "queued"
  | "running"
  | "completed"
  | "cancelled"
  | "failed"
  | "interrupted";
export type MessageState =
  | "queued"
  | "streaming"
  | "complete"
  | "cancelled"
  | "failed"
  | "interrupted";
export type ToolCallState =
  | "requested"
  | "awaiting_approval"
  | "running"
  | "completed"
  | "failed"
  | "denied"
  | "interrupted";

export interface AccountingTotal {
  usage?: TokenUsage;
  estimated_cost_usd_nanos?: number;
}

export interface SessionSummary {
  id: Id;
  workspace_id: Id;
  parent_id?: Id;
  title: string;
  status: SessionStatus;
  active_run_id?: Id;
  queued_prompts: number;
  model?: string;
  context_tokens?: number;
  accounting?: { direct: AccountingTotal; inclusive: AccountingTotal };
  estimated_cost_usd_nanos?: number;
  updated_at_ms: number;
  last_outcome?: RunOutcome;
}

export interface TokenUsage {
  input_tokens: number;
  cache_read_input_tokens: number;
  cache_write_input_tokens: number;
  output_tokens: number;
}

export type RunOutcome =
  | { type: "completed" }
  | { type: "cancelled" }
  | { type: "interrupted" }
  | { type: "failed"; failure: { kind: string; message: string } };

export interface RunSnapshot {
  id: Id;
  session_id: Id;
  status: RunStatus;
  outcome?: RunOutcome;
  usage?: TokenUsage;
  context_tokens?: number;
  estimated_cost_usd_nanos?: number;
}

export interface MessageSnapshot {
  id: Id;
  session_id: Id;
  run_id: Id;
  turn_ordinal: number;
  role: "user" | "assistant";
  state: MessageState;
  output: string;
  refusal: string;
  created_at_ms: number;
}

export interface ToolCallSnapshot {
  id: Id;
  session_id: Id;
  run_id: Id;
  turn_ordinal: number;
  call_ordinal: number;
  provider_call_id: string;
  name: string;
  arguments: string;
  state: ToolCallState;
  result?: string;
  is_error: boolean;
  display?: { type: "diff"; path: string; diff: string };
}

export interface SessionSnapshot {
  summary: SessionSummary;
  messages: MessageSnapshot[];
  runs: RunSnapshot[];
  tool_calls: ToolCallSnapshot[];
  has_older_tool_calls: boolean;
  has_older_messages: boolean;
}

export interface WorkspaceSnapshot {
  cursor: EventCursor;
  workspace: WorkspaceSummary;
  sessions: SessionSummary[];
  focused?: SessionSnapshot;
  has_older_sessions: boolean;
}

export interface ModelSelection {
  model?: string;
  max_output_tokens?: number;
  organization?: string;
}

export interface ModelDescriptor {
  provider: string;
  model: string;
  name?: string;
  context_window?: number;
  selection: ModelSelection;
}

export type SessionEvent = { type: string; [key: string]: unknown };

export interface SessionEventEnvelope {
  cursor: EventCursor;
  session_id: Id;
  run_id?: Id;
  caused_by?: Id;
  occurred_at_ms: number;
  event: SessionEvent;
}

export interface WebBootstrap {
  server: ServerInfo;
  csrf_token: string;
  workspaces: WorkspaceSummary[];
}

export interface CommandReceipt {
  command_id: Id;
  committed_through: EventCursor;
  outcome: { type: string; [key: string]: unknown };
}

export interface TranscriptPage {
  runs: RunSnapshot[];
  messages: MessageSnapshot[];
  tool_calls: ToolCallSnapshot[];
  next_before_run_id?: Id;
}

export interface SessionPage {
  sessions: SessionSummary[];
  next_before_session_id?: Id;
}
