import type {
  CommandReceipt,
  EventCursor,
  ModelDescriptor,
  ModelSelection,
  SessionPage,
  SessionEventEnvelope,
  TranscriptPage,
  WebBootstrap,
  WorkspaceSnapshot,
} from "./types";

const JSON_HEADERS = { "content-type": "application/json" };

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function responseJson<T>(response: Response): Promise<T> {
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { error?: string } | null;
    throw new ApiError(body?.error ?? `request failed (${response.status})`, response.status);
  }
  return response.json() as Promise<T>;
}

export class QqApi {
  constructor(private csrfToken = "") {}

  setCsrfToken(value: string): void {
    this.csrfToken = value;
  }

  async pair(secret: string): Promise<WebBootstrap> {
    const response = await fetch("/v1/web/pair", {
      method: "POST",
      headers: JSON_HEADERS,
      credentials: "same-origin",
      body: JSON.stringify({ secret }),
    });
    const bootstrap = await responseJson<WebBootstrap>(response);
    this.csrfToken = bootstrap.csrf_token;
    return bootstrap;
  }

  async bootstrap(): Promise<WebBootstrap> {
    const bootstrap = await responseJson<WebBootstrap>(
      await fetch("/v1/web/bootstrap", { credentials: "same-origin" }),
    );
    this.csrfToken = bootstrap.csrf_token;
    return bootstrap;
  }

  private async post<T>(path: string, value: unknown): Promise<T> {
    return responseJson<T>(
      await fetch(path, {
        method: "POST",
        credentials: "same-origin",
        headers: { ...JSON_HEADERS, "x-qq-csrf": this.csrfToken },
        body: JSON.stringify(value),
      }),
    );
  }

  snapshot(workspaceId: string, focusedSessionId?: string): Promise<WorkspaceSnapshot> {
    return this.post("/v1/workspaces/snapshot", {
      workspace_id: workspaceId,
      focused_session_id: focusedSessionId,
      session_limit: 100,
      message_limit: 100,
    });
  }

  models(workspace: string, selection: ModelSelection): Promise<ModelDescriptor[]> {
    return this.post("/v1/models", { workspace, selection });
  }

  command(path: string, command: Record<string, unknown>): Promise<CommandReceipt> {
    return this.post(path, { command_id: randomId(), command });
  }

  transcript(sessionId: string, beforeRunId?: string): Promise<TranscriptPage> {
    return this.post("/v1/sessions/transcript", {
      session_id: sessionId,
      before_run_id: beforeRunId,
      run_limit: 20,
    });
  }

  sessions(workspaceId: string, beforeSessionId?: string): Promise<SessionPage> {
    return this.post("/v1/workspaces/sessions", {
      workspace_id: workspaceId,
      before_session_id: beforeSessionId,
      limit: 100,
    });
  }

  async events(
    workspaceId: string,
    cursor: EventCursor,
    signal: AbortSignal,
    onEvent: (event: SessionEventEnvelope) => void,
  ): Promise<void> {
    const response = await fetch(`/v1/workspaces/${workspaceId}/events`, {
      credentials: "same-origin",
      headers: {
        accept: "text/event-stream",
        "last-event-id": cursorString(cursor),
      },
      signal,
    });
    if (!response.ok || !response.body) {
      throw new ApiError(`event stream failed (${response.status})`, response.status);
    }
    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
    let buffer = "";
    let expected = cursor;
    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer += value;
      let extracted = takeSseFrame(buffer);
      while (extracted) {
        const [frame, rest] = extracted;
        buffer = rest;
        const event = eventFromSseFrame(frame, expected);
        if (event) {
          expected = event.cursor;
          onEvent(event);
        }
        extracted = takeSseFrame(buffer);
      }
    }
  }
}

export function takeSseFrame(buffer: string): [string, string] | undefined {
  const boundary = /\r?\n\r?\n/.exec(buffer);
  if (!boundary || boundary.index === undefined) return undefined;
  return [buffer.slice(0, boundary.index), buffer.slice(boundary.index + boundary[0].length)];
}

export function eventFromSseFrame(
  frame: string,
  previous: EventCursor,
): SessionEventEnvelope | undefined {
  const lines = frame.replaceAll("\r", "").split("\n");
  const data = lines
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n");
  if (!data) return undefined;
  const event = JSON.parse(data) as SessionEventEnvelope;
  const id = lines.find((line) => line.startsWith("id:"))?.slice(3).trimStart();
  if (
    event.cursor.store_id !== previous.store_id ||
    event.cursor.workspace_id !== previous.workspace_id ||
    event.cursor.sequence !== previous.sequence + 1 ||
    (id !== undefined && id !== cursorString(event.cursor))
  ) {
    throw new ApiError("event stream cursor is not contiguous", 502);
  }
  return event;
}

export function randomId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (value) => value.toString(16).padStart(2, "0")).join("");
}

export function cursorString(cursor: EventCursor): string {
  return `${cursor.store_id}:${cursor.workspace_id}:${cursor.sequence}`;
}
