import { describe, expect, it } from "vitest";
import { emptyState, reducer } from "./state";
import type { MessageSnapshot, RunSnapshot, SessionEventEnvelope } from "./types";

const workspaceId = "11111111111111111111111111111111";
const sessionId = "22222222222222222222222222222222";
const runId = "33333333333333333333333333333333";
const messageId = "44444444444444444444444444444444";

const run: RunSnapshot = {
  id: runId,
  session_id: sessionId,
  status: "completed",
};

const message: MessageSnapshot = {
  id: messageId,
  session_id: sessionId,
  run_id: runId,
  turn_ordinal: 1,
  role: "assistant",
  state: "streaming",
  output: "hello",
  refusal: "",
  created_at_ms: 1,
};

describe("workbench reducer", () => {
  it("merges older session pages while preserving live summaries", () => {
    const live = {
      id: sessionId,
      workspace_id: workspaceId,
      title: "Live title",
      status: "idle" as const,
      queued_prompts: 0,
      updated_at_ms: 2,
    };
    const older = { ...live, id: "55555555555555555555555555555555", title: "Older" };

    const next = reducer({ ...emptyState, sessions: { [sessionId]: live } }, {
      type: "sessionHistory",
      sessions: [{ ...live, title: "Stale title" }, older],
    });

    expect(next.sessions[sessionId].title).toBe("Live title");
    expect(next.sessions[older.id]).toEqual(older);
  });

  it("merges older history without replacing live records", () => {
    const live = { ...message, output: "newer live text" };
    const state = { ...emptyState, messages: { [messageId]: live } };

    const next = reducer(state, {
      type: "history",
      runs: [run],
      messages: [message],
      tools: [],
    });

    expect(next.runs[runId]).toEqual(run);
    expect(next.messages[messageId]).toEqual(live);
  });

  it("appends streamed text only when the event cursor is contiguous", () => {
    const state = {
      ...emptyState,
      focusedId: sessionId,
      cursor: { store_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", workspace_id: workspaceId, sequence: 4 },
      messages: { [messageId]: message },
    };
    const event: SessionEventEnvelope = {
      cursor: { ...state.cursor, sequence: 5 },
      session_id: sessionId,
      run_id: runId,
      occurred_at_ms: 2,
      event: {
        type: "text_appended",
        message_id: messageId,
        channel: "output",
        text: " world",
      },
    };

    const next = reducer(state, { type: "event", envelope: event });
    expect(next.messages[messageId].output).toBe("hello world");
    expect(next.cursor?.sequence).toBe(5);

    const skipped = reducer(state, {
      type: "event",
      envelope: { ...event, cursor: { ...event.cursor, sequence: 6 } },
    });
    expect(skipped).toBe(state);
  });
});
