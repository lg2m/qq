import { describe, expect, it } from "vitest";
import { ApiError, cursorString, eventFromSseFrame, takeSseFrame } from "./api";
import type { EventCursor, SessionEventEnvelope } from "./types";

const previous: EventCursor = {
  store_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  workspace_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  sequence: 4,
};
const event: SessionEventEnvelope = {
  cursor: { ...previous, sequence: 5 },
  session_id: "cccccccccccccccccccccccccccccccc",
  occurred_at_ms: 1,
  event: { type: "session_deleted", session_id: "cccccccccccccccccccccccccccccccc" },
};

describe("SSE decoding", () => {
  it("extracts both CRLF and LF-delimited frames", () => {
    expect(takeSseFrame("data: one\r\n\r\ndata: two\n\nrest")).toEqual([
      "data: one",
      "data: two\n\nrest",
    ]);
    expect(takeSseFrame("data: two\n\nrest")).toEqual(["data: two", "rest"]);
  });

  it("validates the SSE id and contiguous event cursor", () => {
    const frame = `id: ${cursorString(event.cursor)}\ndata: ${JSON.stringify(event)}`;
    expect(eventFromSseFrame(frame, previous)).toEqual(event);
    expect(() => eventFromSseFrame(frame, { ...previous, sequence: 3 })).toThrow(ApiError);
    expect(() => eventFromSseFrame(`id: wrong\ndata: ${JSON.stringify(event)}`, previous)).toThrow(
      ApiError,
    );
  });
});
