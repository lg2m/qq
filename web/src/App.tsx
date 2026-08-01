import { FormEvent, useEffect, useMemo, useReducer, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ApiError, QqApi } from "./api";
import { emptyState, reducer } from "./state";
import type {
  ModelDescriptor,
  SessionSummary,
  ToolCallSnapshot,
  WebBootstrap,
  WorkspaceSummary,
} from "./types";

const ROUTES = {
  create: "/v1/sessions",
  prompt: "/v1/sessions/prompts",
  cancel: "/v1/runs/cancel",
  approval: "/v1/tools/approvals",
  approvalMode: "/v1/sessions/approval-mode",
  model: "/v1/sessions/model",
  remove: "/v1/sessions/delete",
  compact: "/v1/sessions/compact",
};

export function App() {
  const api = useMemo(() => new QqApi(), []);
  const [bootstrap, setBootstrap] = useState<WebBootstrap>();
  const [authError, setAuthError] = useState("");
  const [state, dispatch] = useReducer(reducer, emptyState);
  const [selectedWorkspace, setSelectedWorkspace] = useState<WorkspaceSummary>();
  const [models, setModels] = useState<ModelDescriptor[]>([]);
  const [selectedModel, setSelectedModel] = useState("");
  const [approvalMode, setApprovalMode] = useState("ask");
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [olderRun, setOlderRun] = useState<string>();
  const [olderSession, setOlderSession] = useState<string>();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const cursorRef = useRef(state.cursor);
  cursorRef.current = state.cursor;

  useEffect(() => {
    let cancelled = false;
    const fragment = new URLSearchParams(location.hash.slice(1));
    const secret = fragment.get("pair");
    (secret ? api.pair(secret) : api.bootstrap())
      .then((value) => {
        if (cancelled) return;
        history.replaceState(null, "", `${location.pathname}${location.search}`);
        setBootstrap(value);
        setSelectedWorkspace(value.workspaces[0]);
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setAuthError(
            error instanceof ApiError && error.status === 401
              ? "This browser is not paired. Run `qq pair` on the server and open its URL."
              : errorMessage(error),
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [api]);

  useEffect(() => {
    if (!selectedWorkspace) return;
    let cancelled = false;
    dispatch({ type: "connection", connection: "connecting" });
    api
      .snapshot(selectedWorkspace.id)
      .then(async (snapshot) => {
        if (cancelled) return;
        const focused = snapshot.sessions[0]?.id;
        const detailed = focused
          ? await api.snapshot(selectedWorkspace.id, focused)
          : snapshot;
        if (cancelled) return;
        dispatch({ type: "snapshot", snapshot: detailed });
        setOlderRun(detailed.focused?.runs[0]?.id);
        setOlderSession(
          detailed.has_older_sessions ? detailed.sessions.at(-1)?.id : undefined,
        );
        const catalog = await api.models(selectedWorkspace.path, {
          model: detailed.focused?.summary.model,
        });
        if (cancelled) return;
        setModels(catalog);
        setSelectedModel(detailed.focused?.summary.model ?? catalog[0]?.selection.model ?? "");
      })
      .catch((error) => !cancelled && setNotice(errorMessage(error)));
    return () => {
      cancelled = true;
    };
  }, [api, selectedWorkspace]);

  useEffect(() => {
    if (
      !selectedWorkspace ||
      !state.cursor ||
      state.cursor.workspace_id !== selectedWorkspace.id
    ) return;
    const controller = new AbortController();
    let stopped = false;
    void (async () => {
      let delay = 100;
      while (!stopped) {
        const cursor = cursorRef.current;
        if (!cursor) return;
        try {
          dispatch({ type: "connection", connection: "replaying" });
          await api.events(selectedWorkspace.id, cursor, controller.signal, (envelope) => {
            cursorRef.current = envelope.cursor;
            dispatch({ type: "event", envelope });
            dispatch({ type: "connection", connection: "live" });
          });
          if (!stopped) dispatch({ type: "connection", connection: "offline" });
        } catch (error) {
          if (stopped || controller.signal.aborted) return;
          if (error instanceof ApiError && error.status === 401) {
            setAuthError("The browser session expired. Run `qq pair` to reconnect.");
            return;
          }
          dispatch({ type: "connection", connection: "offline" });
        }
        await new Promise((resolve) => setTimeout(resolve, delay));
        delay = Math.min(delay * 2, 2_000);
      }
    })();
    return () => {
      stopped = true;
      controller.abort();
    };
  }, [api, selectedWorkspace, state.workspace?.id]);

  async function focus(sessionId: string) {
    if (!selectedWorkspace) return;
    setDrawerOpen(false);
    const snapshot = await api.snapshot(selectedWorkspace.id, sessionId);
    dispatch({ type: "snapshot", snapshot });
    setSelectedModel(snapshot.focused?.summary.model ?? selectedModel);
    setOlderRun(snapshot.focused?.runs[0]?.id);
    setOlderSession(snapshot.has_older_sessions ? snapshot.sessions.at(-1)?.id : undefined);
  }

  async function createSession(parentId?: string) {
    if (!selectedWorkspace || !selectedModel) return;
    await runCommand(async () => {
      const receipt = await api.command(ROUTES.create, {
        type: "create_session",
        workspace_id: selectedWorkspace.id,
        parent_id: parentId,
        model: { model: selectedModel },
        approval_mode: approvalMode,
      });
      const id = receipt.outcome.session_id as string | undefined;
      if (id) await focus(id);
    });
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    const value = prompt.trim();
    if (!value || !state.focusedId) return;
    setPrompt("");
    await runCommand(() =>
      api.command(ROUTES.prompt, {
        type: "submit_prompt",
        session_id: state.focusedId,
        prompt: value,
      }),
    );
  }

  async function runCommand(operation: () => Promise<unknown>) {
    setBusy(true);
    setNotice("");
    try {
      await operation();
    } catch (error) {
      setNotice(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  const focused = state.focusedId ? state.sessions[state.focusedId] : undefined;
  const sessions = Object.values(state.sessions).sort((a, b) => b.updated_at_ms - a.updated_at_ms);
  const messages = Object.values(state.messages).sort((a, b) => a.created_at_ms - b.created_at_ms);
  const tools = Object.values(state.tools).sort(
    (a, b) => a.turn_ordinal - b.turn_ordinal || a.call_ordinal - b.call_ordinal,
  );
  const timeline = transcriptTimeline(Object.values(state.runs), messages, tools);
  const pending = tools.find((tool) => tool.state === "awaiting_approval");

  if (!bootstrap) {
    return <PairingScreen message={authError || "Connecting to QQ…"} />;
  }

  return (
    <main className="app-shell">
      <header className="topbar">
        <button className="mobile-menu" onClick={() => setDrawerOpen(true)} aria-label="Sessions">
          ☰
        </button>
        <div className="brand"><span>qq</span> workbench</div>
        <select
          aria-label="Workspace"
          value={selectedWorkspace?.id ?? ""}
          onChange={(event) =>
            setSelectedWorkspace(bootstrap.workspaces.find((item) => item.id === event.target.value))
          }
        >
          {bootstrap.workspaces.map((workspace) => (
            <option key={workspace.id} value={workspace.id}>{workspace.path}</option>
          ))}
        </select>
        <span className={`connection ${state.connection}`}>{state.connection}</span>
      </header>

      <aside className={`sidebar ${drawerOpen ? "open" : ""}`}>
        <div className="sidebar-header">
          <strong>Sessions</strong>
          <button onClick={() => void createSession()} disabled={!selectedModel || busy}>New</button>
          <button className="mobile-close" onClick={() => setDrawerOpen(false)}>Close</button>
        </div>
        <nav aria-label="Sessions">
          {sessions.map((session) => (
            <SessionRow
              key={session.id}
              session={session}
              active={session.id === state.focusedId}
              onClick={() => void focus(session.id)}
            />
          ))}
          {!sessions.length && <p className="empty">Create a session to begin.</p>}
          {olderSession && (
            <button className="load-sessions" onClick={() => void runCommand(async () => {
              const page = await api.sessions(selectedWorkspace!.id, olderSession);
              dispatch({ type: "sessionHistory", sessions: page.sessions });
              setOlderSession(page.next_before_session_id);
            })}>Load older sessions</button>
          )}
        </nav>
      </aside>

      <section className="workbench">
        <div className="session-toolbar">
          <div>
            <h1>{focused?.title ?? "New workspace"}</h1>
            <small>{focused ? sessionMeta(focused, models) : selectedWorkspace?.path}</small>
          </div>
          {focused && (
            <div className="toolbar-actions">
              <select
                aria-label="Model"
                value={selectedModel}
                onChange={(event) => {
                  const model = event.target.value;
                  setSelectedModel(model);
                  void runCommand(() =>
                    api.command(ROUTES.model, {
                      type: "set_session_model",
                      session_id: focused.id,
                      model: { model },
                    }),
                  );
                }}
              >
                {models.map((model) => (
                  <option key={model.selection.model} value={model.selection.model}>
                    {model.name ?? model.selection.model}
                  </option>
                ))}
              </select>
              <select
                aria-label="Approval mode"
                value={approvalMode}
                onChange={(event) => {
                  const mode = event.target.value;
                  setApprovalMode(mode);
                  void runCommand(() =>
                    api.command(ROUTES.approvalMode, {
                      type: "set_approval_mode",
                      session_id: focused.id,
                      mode,
                    }),
                  );
                }}
              >
                <option value="read_only">Read only</option>
                <option value="ask">Ask</option>
                <option value="auto">Auto</option>
              </select>
              <button onClick={() => void createSession(focused.id)}>Child</button>
              <button
                onClick={() => void runCommand(() => api.command(ROUTES.compact, {
                  type: "compact_session", session_id: focused.id,
                }))}
                disabled={focused.status !== "idle"}
              >Compact</button>
              <button className="danger" onClick={() => {
                if (confirm(`Delete “${focused.title}”? This cannot be undone.`)) {
                  void runCommand(() => api.command(ROUTES.remove, {
                    type: "delete_session", session_id: focused.id,
                  }));
                }
              }} disabled={focused.status !== "idle"}>Delete</button>
            </div>
          )}
        </div>

        <div className="transcript" aria-live="polite">
          {olderRun && (
            <button className="load-older" onClick={() => void runCommand(async () => {
              const page = await api.transcript(state.focusedId!, olderRun);
              dispatch({ type: "history", runs: page.runs, messages: page.messages, tools: page.tool_calls });
              setOlderRun(page.next_before_run_id);
            })}>Load older history</button>
          )}
          {timeline.map((item) => item.type === "message" ? (
              <article className={`message ${item.value.role}`} key={`message-${item.value.id}`}>
                <div className="message-label">{item.value.role === "user" ? "You" : "QQ"}</div>
                <Markdown remarkPlugins={[remarkGfm]}>{item.value.output || item.value.refusal}</Markdown>
              </article>
            ) : (
              <ToolCall key={`tool-${item.value.id}`} tool={item.value} live={state.liveToolOutput[item.value.id]} />
            ))}
          {!timeline.length && focused && <div className="empty hero">What should QQ work on?</div>}
        </div>

        {notice && <div className="notice" role="alert">{notice}</div>}
        {focused ? (
          <form className="composer" onSubmit={submit}>
            <textarea
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault();
                  event.currentTarget.form?.requestSubmit();
                }
              }}
              placeholder="Ask QQ to inspect, change, build, or test…"
              aria-label="Prompt"
            />
            {focused.active_run_id ? (
              <button type="button" className="danger" onClick={() => void runCommand(() =>
                api.command(ROUTES.cancel, { type: "cancel_run", run_id: focused.active_run_id })
              )}>Cancel</button>
            ) : <button type="submit" disabled={busy || !prompt.trim()}>Send</button>}
          </form>
        ) : (
          <div className="empty composer-empty">
            <button onClick={() => void createSession()} disabled={!selectedModel}>Create session</button>
          </div>
        )}
      </section>

      {pending && <ApprovalPanel tool={pending} onDecision={(decision) => void runCommand(() =>
        api.command(ROUTES.approval, {
          type: "respond_tool_approval",
          run_id: pending.run_id,
          tool_call_id: pending.id,
          decision,
        })
      )} />}
    </main>
  );
}

function PairingScreen({ message }: { message: string }) {
  return <main className="pairing"><div className="pair-card"><div className="brand"><span>qq</span> workbench</div><h1>Private agent workspace</h1><p>{message}</p></div></main>;
}

function SessionRow({ session, active, onClick }: { session: SessionSummary; active: boolean; onClick: () => void }) {
  return <button className={`session-row ${active ? "active" : ""}`} onClick={onClick}>
    <span className={`status-dot ${session.status}`} />
    <span><strong>{session.title}</strong><small>{session.model ?? "No model"}</small></span>
    <time>{relativeTime(session.updated_at_ms)}</time>
  </button>;
}

function ToolCall({ tool, live }: { tool: ToolCallSnapshot; live?: string }) {
  const content = tool.display?.type === "diff" ? tool.display.diff : live || tool.result;
  return <details className={`tool ${tool.is_error ? "failed" : ""}`} open={tool.state === "running"}>
    <summary><span>{tool.name}</span><small>{tool.state.replaceAll("_", " ")}</small></summary>
    <pre>{content || tool.arguments}</pre>
  </details>;
}

function ApprovalPanel({ tool, onDecision }: { tool: ToolCallSnapshot; onDecision: (decision: object) => void }) {
  const grant = tool.name === "shell"
    ? { type: "shell_prefix", prefix: shellPrefix(tool.arguments) }
    : { type: "tool", name: tool.name };
  return <aside className="approval" role="dialog" aria-label="Tool approval">
    <div><span className="eyebrow">Approval required</span><h2>{tool.name}</h2></div>
    <pre>{tool.display?.type === "diff" ? tool.display.diff : tool.arguments}</pre>
    <div className="approval-actions">
      <button className="danger" onClick={() => onDecision({ type: "deny" })}>Deny</button>
      <button onClick={() => onDecision({ type: "approve_once" })}>Once</button>
      <button onClick={() => onDecision({ type: "approve_for_session", grant })}>Session</button>
      <button onClick={() => onDecision({ type: "approve_for_workspace", grant })}>Workspace</button>
    </div>
  </aside>;
}

function shellPrefix(argumentsJson: string): string {
  try {
    const command = (JSON.parse(argumentsJson) as { command?: string }).command ?? "";
    return command.trim().split(/\s+/).slice(0, 2).join(" ");
  } catch {
    return "";
  }
}

function transcriptTimeline(
  runs: { id: string }[],
  messages: import("./types").MessageSnapshot[],
  tools: ToolCallSnapshot[],
) {
  const items: Array<
    | { type: "message"; value: import("./types").MessageSnapshot }
    | { type: "tool"; value: ToolCallSnapshot }
  > = [];
  const knownRuns = new Set(runs.map((run) => run.id));
  for (const run of runs) {
    const runItems = [
      ...messages.filter((message) => message.run_id === run.id).map((value) => ({
        type: "message" as const,
        value,
        turn: value.turn_ordinal,
        order: value.role === "user" ? -1 : 0,
      })),
      ...tools.filter((tool) => tool.run_id === run.id).map((value) => ({
        type: "tool" as const,
        value,
        turn: value.turn_ordinal,
        order: value.call_ordinal + 1,
      })),
    ].sort((left, right) => left.turn - right.turn || left.order - right.order);
    for (const item of runItems) {
      if (item.type === "message") items.push({ type: "message", value: item.value });
      else items.push({ type: "tool", value: item.value });
    }
  }
  for (const message of messages) {
    if (!knownRuns.has(message.run_id)) items.push({ type: "message", value: message });
  }
  return items;
}

function sessionMeta(session: SessionSummary, models: ModelDescriptor[]): string {
  const contextWindow = models.find((model) => model.selection.model === session.model)?.context_window;
  const context = session.context_tokens === undefined
    ? "context --"
    : `context ${session.context_tokens.toLocaleString()}${contextWindow ? ` / ${contextWindow.toLocaleString()}` : ""}`;
  const nanos = session.accounting?.inclusive.estimated_cost_usd_nanos ?? session.estimated_cost_usd_nanos;
  return `${context} · ${nanos === undefined ? "cost --" : `cost $${(nanos / 1_000_000_000).toFixed(2)}`}`;
}

function relativeTime(timestamp: number): string {
  const minutes = Math.max(0, Math.floor((Date.now() - timestamp) / 60_000));
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return hours < 24 ? `${hours}h` : `${Math.floor(hours / 24)}d`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "Unexpected error";
}
