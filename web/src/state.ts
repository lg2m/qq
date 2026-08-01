import type {
  MessageSnapshot,
  RunSnapshot,
  SessionEventEnvelope,
  SessionSummary,
  ToolCallSnapshot,
  WorkspaceSnapshot,
} from "./types";

export interface WorkbenchState {
  cursor?: WorkspaceSnapshot["cursor"];
  workspace?: WorkspaceSnapshot["workspace"];
  sessions: Record<string, SessionSummary>;
  focusedId?: string;
  messages: Record<string, MessageSnapshot>;
  runs: Record<string, RunSnapshot>;
  tools: Record<string, ToolCallSnapshot>;
  liveToolOutput: Record<string, string>;
  connection: "connecting" | "replaying" | "live" | "offline";
}

export const emptyState: WorkbenchState = {
  sessions: {},
  messages: {},
  runs: {},
  tools: {},
  liveToolOutput: {},
  connection: "connecting",
};

export type Action =
  | { type: "snapshot"; snapshot: WorkspaceSnapshot }
  | {
      type: "history";
      runs: RunSnapshot[];
      messages: MessageSnapshot[];
      tools: ToolCallSnapshot[];
    }
  | { type: "sessionHistory"; sessions: SessionSummary[] }
  | { type: "event"; envelope: SessionEventEnvelope }
  | { type: "focus"; sessionId: string }
  | { type: "connection"; connection: WorkbenchState["connection"] };

export function reducer(state: WorkbenchState, action: Action): WorkbenchState {
  if (action.type === "connection") return { ...state, connection: action.connection };
  if (action.type === "focus") return { ...state, focusedId: action.sessionId };
  if (action.type === "sessionHistory") {
    return {
      ...state,
      sessions: {
        ...Object.fromEntries(action.sessions.map((session) => [session.id, session])),
        ...state.sessions,
      },
    };
  }
  if (action.type === "history") {
    return {
      ...state,
      runs: { ...Object.fromEntries(action.runs.map((run) => [run.id, run])), ...state.runs },
      messages: {
        ...Object.fromEntries(action.messages.map((message) => [message.id, message])),
        ...state.messages,
      },
      tools: {
        ...Object.fromEntries(action.tools.map((tool) => [tool.id, tool])),
        ...state.tools,
      },
    };
  }
  if (action.type === "snapshot") {
    const { snapshot } = action;
    return {
      ...state,
      cursor: snapshot.cursor,
      workspace: snapshot.workspace,
      sessions: Object.fromEntries(
        [...snapshot.sessions, ...(snapshot.focused ? [snapshot.focused.summary] : [])].map(
          (session) => [session.id, session],
        ),
      ),
      focusedId: snapshot.focused?.summary.id,
      messages: Object.fromEntries(
        (snapshot.focused?.messages ?? []).map((message) => [message.id, message]),
      ),
      runs: Object.fromEntries((snapshot.focused?.runs ?? []).map((run) => [run.id, run])),
      tools: Object.fromEntries(
        (snapshot.focused?.tool_calls ?? []).map((tool) => [tool.id, tool]),
      ),
    };
  }

  const envelope = action.envelope;
  if (state.cursor && envelope.cursor.sequence !== state.cursor.sequence + 1) return state;
  const event = envelope.event as Record<string, unknown> & { type: string };
  const next = { ...state, cursor: envelope.cursor };
  const session = event.session as SessionSummary | undefined;
  if (session) next.sessions = { ...state.sessions, [session.id]: session };
  if (event.type === "session_deleted") {
    const id = event.session_id as string;
    next.sessions = { ...state.sessions };
    delete next.sessions[id];
    if (id === state.focusedId) {
      next.focusedId = undefined;
      next.messages = {};
      next.runs = {};
      next.tools = {};
      next.liveToolOutput = {};
    }
  }

  const message = event.message as MessageSnapshot | undefined;
  if (message && message.session_id === state.focusedId) {
    next.messages = { ...state.messages, [message.id]: message };
  }
  if (event.type === "text_appended") {
    const id = event.message_id as string;
    const current = state.messages[id];
    if (current) {
      const channel = event.channel as "output" | "refusal";
      next.messages = {
        ...state.messages,
        [id]: { ...current, [channel]: current[channel] + (event.text as string) },
      };
    }
  }

  const run = event.run as RunSnapshot | undefined;
  if (run && run.session_id === state.focusedId) next.runs = { ...state.runs, [run.id]: run };
  const tool = event.tool_call as ToolCallSnapshot | undefined;
  if (tool && tool.session_id === state.focusedId) {
    next.tools = { ...state.tools, [tool.id]: tool };
  }
  if (event.type === "tool_call_output_delta") {
    const id = event.tool_call_id as string;
    next.liveToolOutput = {
      ...state.liveToolOutput,
      [id]: (state.liveToolOutput[id] ?? "") + (event.chunk as string),
    };
  }
  return next;
}
