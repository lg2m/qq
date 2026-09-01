# QQ Protocol Specification

## Purpose

This document specifies the versioned HTTP/SSE wire protocol between QQ
clients and a QQ server. It is the contract for the TUI, future remote
clients, and any automation that talks to `qq serve`.

The protocol is transport-neutral in the `qq-protocol` crate: shared types do
not depend on an HTTP client or server framework. `qq-server` maps those types
onto HTTP routes and SSE frames, while `qq-client` performs the inverse mapping.

Canonical source of truth for schemas and tags:

- `crates/qq-protocol/src/lib.rs`
- `crates/qq-protocol/src/sessions.rs`
- `crates/qq-protocol/src/ids.rs`
- shared request and event limits in `crates/qq-protocol/src/limits.rs`
- local connection capability in `crates/qq-protocol/src/local.rs`
- route wiring in `crates/qq-server/src/lib.rs`
- client decoding in `crates/qq-client/src/lib.rs`

Related documents:

- `docs/design/architecture.md` — system shape and transport choices
- `docs/design/tools.md` — tool loop, approvals, and security policy
- `docs/design/transcript.md` — client presentation of session history

## Design Principles

1. **Commands in, events out.** Clients mutate state with ordinary HTTP POST
   requests. The server pushes ordered history over SSE. There is no
   bidirectional socket protocol.
2. **Persisted state is authoritative.** The server commits events before
   publishing them. A client may disconnect and resume without losing work.
3. **Idempotent mutation.** Every command carries a client-generated
   `command_id`. Retries with the same id and payload return the original
   receipt; a conflicting payload is rejected.
4. **Workspace-scoped streams.** Event sequences belong to a workspace, not a
   single session, so one subscription can observe every session in that
   workspace.
5. **Stable, versioned JSON.** Enums use externally tagged `type` fields with
   `snake_case` names. Unknown fields are rejected on request bodies that use
   `deny_unknown_fields`.

## Protocol Version

```text
PROTOCOL_VERSION = 7
```

The counter restarted at 1 on 2026-07-28, before any release; earlier
values (1–12) belonged to pre-release iterations and no released build
speaks them. The number is a build-compatibility counter, not a product
version — being "high" carries no meaning. Version 2 added session
compaction (`compact_session`, `session_compacted`); version 3 added
run context occupancy (`run_context_updated`,
`RunSnapshot.context_tokens`); version 4 added workspace-lifetime
approvals (the `approve_for_workspace` decision, the
`approved_for_workspace` resolution, and the
`workspace_grant_promoted` event); version 5 made context occupancy
authoritative session state (`SessionSummary.context_tokens` and
`session_context_updated`) so legacy billing totals and internal compaction
runs cannot masquerade as the current session context, and added explicit
direct and inclusive session accounting totals; version 6 added persisted
run prompt identity (`RunSnapshot.prompt_identity`); version 7 added the
persisted `model_turn_completed` event so each provider turn records its model,
usage, and estimated cost before it is published.

Clients and servers must agree on this value.

- `GET /v1/health` returns `ServerInfo.protocol_version`.
- Local discovery metadata also records the protocol version. A mismatch is a
  hard failure; clients must not speak to an incompatible server.
- Bump `PROTOCOL_VERSION` whenever an externally visible wire change is not
  backward compatible for existing clients (new required fields, removed
  variants, changed tag names, changed cursor format, and similar).

## Transport

| Concern | Choice |
| --- | --- |
| Scheme | HTTP/1.1 over loopback by default (`http://127.0.0.1:<port>`) |
| Commands | `POST` with `Content-Type: application/json` |
| Snapshots / catalog | `POST` with JSON request and JSON response |
| Live history | `GET` Server-Sent Events (`text/event-stream`) |
| Auth | `Authorization: Bearer <token>` on every request |
| Body limit | 1 MiB request bodies |
| Keep-alive | SSE comment/`keep-alive` every 15 seconds while idle |

Intentionally not used: GraphQL, gRPC, raw TCP, WebRTC, or WebSocket. JSON is
the only wire encoding until profiling shows serialization or bandwidth is a
real bottleneck.

### Authentication

The server generates a random bearer token at startup and writes it, with the
bind address and process metadata, to a private per-user file (`server.ron`).
The binary discovers the running instance through `qq-server` and passes a
redacted `LocalServerConnection` capability to `qq-client`, which attaches with:

```http
Authorization: Bearer <token>
```

Missing or incorrect credentials receive `401` with:

```json
{ "error": "authentication required" }
```

Token comparison is constant-time. Metadata and tokens must never be logged in
full by clients or servers.

### Error Responses

Failed HTTP requests return JSON:

```json
{ "error": "<stable message>" }
```

Common status codes:

| Status | Meaning |
| --- | --- |
| `400` | Malformed body, wrong command for the route, invalid cursor/id |
| `401` | Missing or invalid bearer token |
| `404` | Unknown path |
| `405` | Wrong HTTP method |
| `413` | Body exceeds the 1 MiB limit |
| `503` | Handler unavailable or concurrency limit reached |
| `500` | Unexpected internal failure |

Error bodies intentionally avoid leaking internal details.

### Concurrency Limits

The HTTP adapter admits at most:

- 64 concurrent session/command/snapshot/catalog requests
- 64 concurrent workspace event subscriptions

Additional requests receive `503`.

## Resource Model

```text
Store
 └── Workspace (canonical filesystem path)
      └── Session
           └── Run
                ├── Message (user / assistant)
                └── ToolCall
```

| Resource | Identity | Notes |
| --- | --- | --- |
| Store | `store_id` | One durable SQLite store per server process data dir |
| Workspace | `workspace_id` | Canonical absolute path known to the server |
| Session | `session_id` | Conversation within a workspace; optional `parent_id` |
| Run | `run_id` | One queued/executed model attempt for a prompt |
| Message | `message_id` | User or assistant text for a run |
| Tool call | `tool_call_id` | One model-requested tool invocation |
| Command | `command_id` | Client idempotency key for one mutating request |

### Identifiers

All resource ids are 16 random bytes encoded as **32 lowercase hex
characters** with no separators:

```text
00112233445566778899aabbccddeeff
```

Uppercase hex, odd lengths, and non-hex characters are rejected. Ids are
generated by the party that creates the resource:

- Clients generate `command_id` values.
- The server generates workspace, session, run, message, and tool-call ids.

### Event Cursor

Workspace event order is a single monotonic sequence:

```text
EventCursor = { store_id, workspace_id, sequence }
wire form   = "<store_id>:<workspace_id>:<sequence>"
```

Example:

```text
aa..aa:bb..bb:42
```

Rules:

- `sequence` increases by one for each committed event in that workspace.
- Cursors are opaque to clients except for comparison and reconnect.
- A cursor from a different store or workspace must not be reused on another
  subscription.

## HTTP Routes

All routes require authentication. Command routes accept a `CommandRequest`
and return a `CommandReceipt` unless noted.

```text
GET  /v1/health
POST /v1/workspaces/resolve
POST /v1/workspaces/snapshot
POST /v1/models
POST /v1/sessions
POST /v1/sessions/prompts
POST /v1/sessions/approval-mode
POST /v1/sessions/model
POST /v1/sessions/delete
POST /v1/sessions/prune
POST /v1/sessions/compact
POST /v1/runs/cancel
POST /v1/tools/approvals
GET  /v1/workspaces/{workspace_id}/events
```

Each command route accepts only its matching `SessionCommand` variant. Sending
the wrong variant to a route is a `400`.

### `GET /v1/health`

Liveness and version probe used by discovery.

Response `ServerInfo`:

```json
{
  "protocol_version": 5,
  "version": "0.1.0",
  "pid": 12345
}
```

### Command envelope

Request:

```json
{
  "command_id": "<command_id>",
  "command": { "type": "<variant>", "...": "..." }
}
```

Response:

```json
{
  "command_id": "<command_id>",
  "committed_through": {
    "store_id": "<store_id>",
    "workspace_id": "<workspace_id>",
    "sequence": 0
  },
  "outcome": { "type": "<variant>", "...": "..." }
}
```

`committed_through` is the latest workspace cursor the command durable-ized.
Clients use it to advance local ack state even when the command emits no new
session events (for example, resolving an already-known workspace).

### `POST /v1/workspaces/resolve`

```json
{
  "command_id": "...",
  "command": {
    "type": "resolve_workspace",
    "path": "/absolute/or/relative/path"
  }
}
```

Outcome:

```json
{
  "type": "workspace_resolved",
  "workspace_id": "..."
}
```

The server canonicalizes the path, requires a directory, and reuses the
existing workspace row when the path was seen before.

### `POST /v1/sessions`

```json
{
  "command_id": "...",
  "command": {
    "type": "create_session",
    "workspace_id": "...",
    "parent_id": null,
    "model": {
      "model": "provider/model-id",
      "max_output_tokens": 8192,
      "organization": null
    },
    "approval_mode": "ask"
  }
}
```

| Field | Required | Notes |
| --- | --- | --- |
| `workspace_id` | yes | From `resolve_workspace` |
| `parent_id` | no | Optional parent session |
| `model` | yes | `ModelSelection`; fields inside may be omitted |
| `approval_mode` | no | Defaults to `ask` |

Outcome:

```json
{
  "type": "session_created",
  "session_id": "..."
}
```

Emits `session_created`. The workspace's effective config grants (see
`docs/design/tools.md`, "Grant Lifetimes") are copied into the new
session's grant set inside the creation transaction; a later config edit
affects only sessions created afterwards.

### `POST /v1/sessions/prompts`

```json
{
  "command_id": "...",
  "command": {
    "type": "submit_prompt",
    "session_id": "...",
    "prompt": "Explain how sessions are stored."
  }
}
```

Outcome on accept:

```json
{
  "type": "prompt_queued",
  "session_id": "...",
  "run_id": "...",
  "queue_position": 0
}
```

Emits `prompt_queued`, then later run lifecycle events as the runtime executes
the queue.

### `POST /v1/runs/cancel`

```json
{
  "command_id": "...",
  "command": {
    "type": "cancel_run",
    "run_id": "..."
  }
}
```

Outcomes:

```json
{ "type": "cancellation_requested", "run_id": "..." }
```

```json
{
  "type": "run_already_finished",
  "run_id": "...",
  "outcome": { "type": "completed" }
}
```

A live cancel emits `cancellation_requested` and eventually `run_finished`
with a cancelled/interrupted outcome once the runtime stops the work.

### `POST /v1/tools/approvals`

```json
{
  "command_id": "...",
  "command": {
    "type": "respond_tool_approval",
    "run_id": "...",
    "tool_call_id": "...",
    "decision": { "type": "approve_once" }
  }
}
```

Decision variants:

| `decision.type` | Meaning |
| --- | --- |
| `approve_once` | Run this call only |
| `approve_for_session` | Run this call and record a session grant |
| `approve_for_workspace` | Like `approve_for_session`, plus promote the grant into workspace configuration |
| `deny` | Reject the call |

`approve_for_session` and `approve_for_workspace` include a grant:

```json
{ "type": "approve_for_session", "grant": { "type": "tool", "name": "edit_file" } }
```

```json
{
  "type": "approve_for_workspace",
  "grant": { "type": "shell_prefix", "prefix": "cargo test" }
}
```

Outcome:

```json
{
  "type": "tool_approval_resolved",
  "tool_call_id": "...",
  "resolution": "approved_once"
}
```

Resolution values: `approved_once`, `approved_for_session`,
`approved_for_workspace`, `denied`, `denied_timeout`.

`approve_for_workspace` resolves the approval exactly like
`approve_for_session` — the session grant is recorded in the same
transaction, so the waiting run proceeds immediately — and additionally
requests that the grant be written into the workspace's `.qq/config.ron`
policy section. That durable write happens after the approval commits and
is first recorded in a bounded SQLite outbox in the same transaction. One
serial worker drains that outbox and atomically removes each entry with its
separate `workspace_grant_promoted` fate event, carrying one of:

```json
{ "type": "written", "path": "/repo/.qq/config.ron" }
{ "type": "already_present", "path": "/repo/.qq/config.ron" }
{ "type": "failed", "message": "..." }
```

A `failed` outcome (managed-layer deny, IO error) is informational only:
the approval stands and the session grant remains in force. Retrying the
`respond_tool_approval` command with the same `command_id` replays the
original receipt and does not enqueue a duplicate promotion. If the process
stops after accepting the approval, startup resumes the existing outbox entry;
the configuration write is idempotent, so recovery normally observes
`already_present` when the file write landed before the interruption.

### `POST /v1/sessions/approval-mode`

```json
{
  "command_id": "...",
  "command": {
    "type": "set_approval_mode",
    "session_id": "...",
    "mode": "auto"
  }
}
```

Modes:

| Mode | Behavior |
| --- | --- |
| `read_only` | Deny mutating, shell, and MCP tools without prompting |
| `ask` | Default. Prompt for mutating/shell/MCP unless granted |
| `auto` | Auto-allow workspace edits/writes; still ask for shell/MCP unless granted |

Outcome:

```json
{
  "type": "approval_mode_set",
  "session_id": "...",
  "mode": "auto"
}
```

### `POST /v1/sessions/model`

```json
{
  "command_id": "...",
  "command": {
    "type": "set_session_model",
    "session_id": "...",
    "model": {
      "model": "provider/model-id",
      "max_output_tokens": 8192,
      "organization": null
    }
  }
}
```

Repoints the session's model. The selection is validated exactly like
`create_session`. It takes effect when the next run is claimed; a run that is
already executing keeps the model it started with.

Outcome:

```json
{
  "type": "session_model_set",
  "session_id": "...",
  "model": { "model": "provider/model-id" }
}
```

Emits `session_updated` carrying the full refreshed `SessionSummary`.

### `POST /v1/sessions/delete`

```json
{
  "command_id": "...",
  "command": {
    "type": "delete_session",
    "session_id": "..."
  }
}
```

Deletes the session and every row it owns — runs, messages, model turns, tool
calls, session files, and grants — in one transaction. Rejected with `400`
while the session has an active run; cancel the run first. Child sessions
survive as roots.

The session's rows in the workspace event log are deliberately **kept**:
cursors promise a gapless `previous + 1` sequence to subscribers, so removing
event rows would break every replay that spans the deletion. Replaying the
kept events is harmless because the trailing `session_deleted` event
converges any client on the deleted state; snapshots never include deleted
sessions.

Outcome:

```json
{
  "type": "session_deleted",
  "session_id": "..."
}
```

Emits `session_deleted`.

### `POST /v1/sessions/prune`

```json
{
  "command_id": "...",
  "command": {
    "type": "prune_sessions",
    "workspace_id": "..."
  }
}
```

Deletes every idle session in the workspace that has no messages and no runs
(the residue of creating sessions without prompting them), with the same
per-session guarantees as `delete_session`.

Outcome:

```json
{
  "type": "sessions_pruned",
  "workspace_id": "...",
  "deleted": 2
}
```

Emits one `session_deleted` per deleted session.

### `POST /v1/sessions/compact`

```json
{
  "command_id": "...",
  "command": {
    "type": "compact_session",
    "session_id": "..."
  }
}
```

Compacts the session's model context. Valid only while the session is idle:
rejected with `400` while a run is active or prompts are queued, exactly like
`delete_session`'s active-run refusal. The command queues an **internal**
summarization run that flows through the ordinary run machinery (permits,
cancellation via `cancel_run`, usage and cost accounting) but whose request
messages and streamed output never join the session transcript. Its product
is a durable summary row plus a cutoff marker, committed atomically with the
run's completion; later runs assemble context as agent instructions + latest
summary + verbatim transcript after the marker. The client transcript is
untouched. A crash mid-summarization commits no marker; retry the command.

A later `compact_session` summarizes the current summary together with the
span since the marker, so repeated compactions fold rather than stack. A
small bounded history of prior compactions is retained server-side.

Outcome:

```json
{
  "type": "compaction_queued",
  "session_id": "...",
  "run_id": "..."
}
```

Emits `session_updated` (the session shows queued), then the internal run's
`run_started` and `run_finished`, then `session_compacted` when the summary
commits. A failed or cancelled summarization emits only `run_finished` with
that outcome and leaves assembly unchanged.

### `POST /v1/workspaces/snapshot`

Not a command. Request:

```json
{
  "workspace_id": "...",
  "focused_session_id": null,
  "session_limit": 512,
  "message_limit": 256
}
```

| Field | Rules |
| --- | --- |
| `session_limit` | Required, `1..=512` |
| `message_limit` | Required, `1..=256` in current runtime bounds |
| `focused_session_id` | Optional; when set, include full session detail |

Response `WorkspaceSnapshot`:

```json
{
  "cursor": { "store_id": "...", "workspace_id": "...", "sequence": 12 },
  "workspace": { "id": "...", "path": "/path/to/repo" },
  "sessions": [ /* SessionSummary, newest first */ ],
  "focused": {
    "summary": { /* SessionSummary */ },
    "messages": [ /* MessageSnapshot */ ],
    "runs": [ /* RunSnapshot */ ],
    "tool_calls": [ /* ToolCallSnapshot */ ],
    "has_older_tool_calls": false,
    "has_older_messages": false
  },
  "has_older_sessions": false
}
```

Snapshots are the catch-up mechanism after connect or cursor loss. Live SSE
then advances the client from `cursor`.

### `POST /v1/models`

Model catalog lookup for a workspace and selection hint.

Request:

```json
{
  "workspace": "/path/to/repo",
  "selection": {
    "model": null,
    "max_output_tokens": null,
    "organization": null
  }
}
```

Response: JSON array of `ModelDescriptor`:

```json
[
  {
    "provider": "openai",
    "model": "gpt-5",
    "name": "GPT-5",
    "context_window": 128000,
    "selection": {
      "model": "openai/gpt-5",
      "max_output_tokens": null,
      "organization": null
    }
  }
]
```

### `GET /v1/workspaces/{workspace_id}/events`

Resumable SSE subscription for one workspace.

Required header:

```http
Last-Event-ID: <store_id>:<workspace_id>:<sequence>
Accept: text/event-stream
```

`Last-Event-ID` is mandatory. Clients start from the snapshot cursor (or a
previous event id). The server:

1. Validates the cursor's store and workspace.
2. Replays every persisted event with `sequence > after.sequence`.
3. Switches to live delivery of newly committed events.
4. Sends keep-alive comments every 15 seconds while idle.

Each data event:

```text
id: <store_id>:<workspace_id>:<sequence>
event: session_event
data: { ...SessionEventEnvelope... }
```

Client validation expectations:

- SSE `id` equals `envelope.cursor` rendered in wire form.
- `cursor.sequence` is exactly previous sequence + 1.
- `cursor.workspace_id` matches the subscribed workspace.
- `cursor.store_id` remains stable for the life of the stream.

If the stream gaps, the client must resnapshot and reconnect. Do not invent
events to fill holes.

## Session Event Envelope

Every streamed payload is a `SessionEventEnvelope`:

```json
{
  "cursor": {
    "store_id": "...",
    "workspace_id": "...",
    "sequence": 7
  },
  "session_id": "...",
  "run_id": "...",
  "caused_by": "...",
  "occurred_at_ms": 1710000000123,
  "event": {
    "type": "text_appended",
    "message_id": "...",
    "channel": "output",
    "text": "hello"
  }
}
```

| Field | Presence | Meaning |
| --- | --- | --- |
| `cursor` | always | Durable workspace position |
| `session_id` | always | Session the event belongs to |
| `run_id` | optional | Set when the event is run-scoped |
| `caused_by` | optional | `command_id` that produced the event |
| `occurred_at_ms` | always | Server Unix time in milliseconds |
| `event` | always | Tagged `SessionEvent` body |

### `SessionEvent` variants

| `type` | Principal fields | When |
| --- | --- | --- |
| `session_created` | `session` | New session row committed |
| `session_updated` | `session` | Non-run session mutation (model repointed, compaction queued) |
| `session_deleted` | `session_id` | Session and its rows deleted; earlier events remain |
| `prompt_queued` | `session`, `message`, `run`, `queue_position` | User prompt accepted |
| `run_started` | `session`, `run_id` | Run leaves the queue |
| `assistant_message_started` | `message` | A model turn's message begins streaming |
| `text_appended` | `message_id`, `channel`, `text` | Output or refusal delta |
| `model_turn_completed` | `run_id`, `turn_ordinal`, `model`, optional `usage`, optional `estimated_cost_usd_nanos` | A provider inference and its accounting committed |
| `tool_call_requested` | `tool_call` | Model finished requesting a tool call |
| `tool_approval_requested` | `tool_call`, optional `shell`, optional `edit` | Policy needs a human decision |
| `tool_approval_resolved` | `tool_call`, `resolution` | Approval decision recorded |
| `workspace_grant_promoted` | `grant`, `outcome` | An approve-for-workspace promotion finished (`written`, `already_present`, or non-fatal `failed`) |
| `tool_call_started` | `tool_call` | Execution began |
| `tool_call_output_delta` | `tool_call_id`, `chunk` | Incremental output from a running call (shell) |
| `tool_call_finished` | `tool_call` | Execution ended with result/error |
| `run_context_updated` | `run_id`, `context_tokens` | A measured model turn committed; the run audit value moved |
| `session_context_updated` | `run_id`, optional `context_tokens` | A current-model prompt turn committed; the session meter moved or became unknown |
| `cancellation_requested` | `session`, `run_id` | Cancel command accepted for a live run |
| `run_finished` | `session`, `run_id`, `outcome`, optional `usage`, optional `context_tokens` | Terminal run state |
| `session_compacted` | `session`, optional `summary`, `before_bytes`, `after_bytes` | Compaction summary + cutoff committed |

Text channels:

- `output` — normal assistant text
- `refusal` — model refusal text

`session_compacted` carries the refreshed `SessionSummary` (the internal run
has already finished when it is published), a bounded excerpt of the summary
text (optional on the wire), and the assembled context size in bytes before
and after the compaction so clients can surface the shrink without waiting
for the next run's usage. The summary's `context_tokens` is absent because the
compaction provider usage measured the replaced input, not the new summary.

### Snapshots embedded in events

Events often carry denormalized snapshots so clients can render without a
round trip:

**`SessionSummary`**

```json
{
  "id": "...",
  "workspace_id": "...",
  "parent_id": null,
  "title": "New session",
  "status": "running",
  "active_run_id": "...",
  "queued_prompts": 0,
  "model": "provider/model",
  "context_tokens": 12500,
  "estimated_cost_usd_nanos": 1234567,
  "updated_at_ms": 1710000000123,
  "last_outcome": null
}
```

Session status: `idle`, `queued`, `running`.

`context_tokens` is the latest exact prompt-turn input total measured for the
session. It is absent when unknown. A successful compaction or a model change
clears it until another prompt turn reports usage; clients must not reconstruct
it from cumulative run billing.

**`RunSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "status": "running",
  "outcome": null,
  "prompt_identity": {
    "version": 7,
    "instruction_hash": "1111111111111111111111111111111111111111111111111111111111111111",
    "system_prompt_hash": "2222222222222222222222222222222222222222222222222222222222222222",
    "tool_schema_hash": "3333333333333333333333333333333333333333333333333333333333333333",
    "selected_guidance": {
      "kind": "skill",
      "name": "review",
      "source": ".qq/skills/review/SKILL.md",
      "content_hash": "4444444444444444444444444444444444444444444444444444444444444444"
    }
  },
  "usage": null,
  "estimated_cost_usd_nanos": null,
  "context_tokens": null
}
```

Run status: `queued`, `running`, `completed`, `cancelled`, `failed`,
`interrupted`.

`prompt_identity.version` identifies the shared provider-neutral system prompt
contract prepared for the run. `instruction_hash` identifies the selected root
instructions: `AGENTS.md`, `CLAUDE.md` when `AGENTS.md` is absent, or the empty
selection. `system_prompt_hash` identifies the exact prepared system text, and
`tool_schema_hash` identifies the ordered provider tool schemas. When a slash
command or skill was selected, `selected_guidance` records its kind, name,
source, optional declared version, and content hash. Nested instructions found
later remain durable tool evidence but do not retroactively change this
pre-provider identity. `prompt_identity` is absent for historical runs and
runs that failed before prompt preparation; version-6 rows may omit the fields
added in version 7.

`usage` sums every model turn in the run and is the billing figure;
`context_tokens` is the final completed turn's input-token total (fresh
input + cache reads + cache writes) for that run. Internal compaction runs
measure the pre-compaction summarizer request, so this per-run audit field is
not a substitute for `SessionSummary.context_tokens`.
`model_turn_completed` is persisted before any tool dispatch and records every
provider inference, including tool-only and internal compaction turns, so
trajectory exporters do not have to infer per-turn model or accounting data.
`run_context_updated` streams measured per-run audit values and is absent for
unmeasured turns and runs persisted before version 3.
`session_context_updated` is the live session-meter event. It is emitted only
for prompt turns whose model still matches the session's selected model; an
absent `context_tokens` explicitly clears the meter when that turn was not
measured. Clients must ignore `run_context_updated` for session occupancy,
including during replay of pre-version-5 events. Both events carry no
snapshots.

**`MessageSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "run_id": "...",
  "role": "assistant",
  "state": "streaming",
  "turn_ordinal": 1,
  "output": "partial text",
  "refusal": "",
  "created_at_ms": 1710000000123
}
```

Roles: `user`, `assistant`.  
Message state: `queued`, `streaming`, `complete`, `cancelled`, `failed`,
`interrupted`.

The unit of assistant output is the model turn: each turn that produces
text gets its own message, `turn_ordinal` 1-based and matching the
ordinals on that turn's tool calls, so clients can render text and calls
in execution order. User messages and rows persisted before per-turn
messages use `turn_ordinal` 0.

**`ToolCallSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "run_id": "...",
  "turn_ordinal": 1,
  "call_ordinal": 0,
  "provider_call_id": "call_...",
  "name": "read_file",
  "arguments": "{\"path\":\"README.md\"}",
  "state": "completed",
  "result": "# QQ\n...",
  "is_error": false,
  "display": { "type": "diff", "path": "src/lib.rs", "diff": "- old\n+ new\n" }
}
```

Tool state: `requested`, `awaiting_approval`, `running`, `completed`,
`failed`, `denied`, `interrupted`.

`display` is an optional, extensible tagged payload for client rendering
only (first variant: `diff`, carried by successful `edit_file` and
`write_file` calls). It is absent unless populated, never enters model
context, and the `result` string remains authoritative.

### Approval previews

`tool_approval_requested` may include one of:

```json
{
  "shell": {
    "command": "cargo test -p qq-core",
    "cwd": "crates/qq-core"
  }
}
```

```json
{
  "edit": {
    "path": "src/lib.rs",
    "diff": "- old\n+ new\n"
  }
}
```

Previews are advisory UI aids. The authoritative call remains `tool_call`.

### Run outcomes and failures

```json
{ "type": "completed" }
{ "type": "cancelled" }
{ "type": "interrupted" }
{
  "type": "failed",
  "failure": {
    "kind": "provider_rate_limited",
    "message": "..."
  }
}
```

`RunFailureKind` values:

```text
invalid_command
configuration
authentication
policy
server
provider_configuration
provider_authentication
provider_rate_limited
provider_invalid_request
provider_unavailable
provider_transport
provider_api
provider_response
provider_protocol
```

### Token usage

```json
{
  "input_tokens": 1000,
  "cache_read_input_tokens": 200,
  "cache_write_input_tokens": 0,
  "output_tokens": 250
}
```

Usage may appear on `run_finished`. Estimated costs on summaries are integer
US-dollar nanos (`1e-9 USD`) when pricing data is available.

## Idempotency

Mutating commands are durable-idempotent:

1. Client generates a fresh `command_id` per logical user action.
2. Server stores `(command_id, request_json, receipt_json)`.
3. A retry with the same id and identical request JSON returns the original
   `CommandReceipt` without re-executing side effects.
4. A retry with the same id and different request JSON fails as an
   idempotency conflict (surfaced as a rejected request).

Clients must not reuse a `command_id` for a different action. After a transport
failure with an unknown result, retry the **same** id and body, then reconcile
through snapshot/SSE rather than submitting a second logical command.

## Client Lifecycle

Recommended attach sequence for an interactive client:

```text
1. Discover local server metadata (or start qq serve / embedded server).
2. GET /v1/health and verify protocol_version.
3. POST resolve_workspace for the canonical workspace path.
4. POST workspaces/snapshot with desired page limits.
5. GET workspaces/{id}/events with Last-Event-ID = snapshot.cursor.
6. Apply live SessionEventEnvelope messages in order.
7. POST commands for user actions; match caused_by / command receipts.
8. On stream gap, 401 after restart, or invalid cursor: resnapshot and resume.
```

Direct CLI automation (`qq ask`) currently executes through the in-process
runtime rather than the session HTTP API. The session protocol above is the
supported multi-client surface.

## Direct Runtime Types

`qq-protocol` also exports a smaller vocabulary used by the in-process run
path (`qq ask` and internal runtime tests):

- `RunCommand { prompt }`
- `RunEvent` — `started`, `output_text_delta`, `refusal_delta`, `usage`,
  `completed`, `failed`

These types are not persisted session state. `qq ask` consumes them entirely
in-process; use the workspace/session routes or `qq run` for work that must be
durable, resumable, approval-capable, or shared across clients.

## Size And Validation Bounds

Current server/client bounds that affect interoperability:

| Limit | Value |
| --- | --- |
| HTTP request body | 1 MiB |
| SSE event JSON | 1 MiB |
| Snapshot response (client read limit) | 8 MiB |
| Model catalog response | 2 MiB |
| Health response | 16 KiB |
| Workspace path field | 4 KiB |
| Model id field | 512 bytes |
| Organization field | 512 bytes |
| Snapshot `session_limit` | 1..=512 |
| Bootstrap snapshot used by the shipped TUI client | 512 sessions / 256 messages |

Field-level validation rejects empty prompts/paths where applicable and
unknown fields on structured request bodies.

## Compatibility Rules

When changing the protocol:

1. Update types in `qq-protocol` first.
2. Add/adjust contract tests that lock tag names and field shapes.
3. Bump `PROTOCOL_VERSION` for breaking wire changes.
4. Update this document in the same change.
5. Keep provider-specific codecs out of this protocol; provider wire formats
   belong in `qq-provider`.

Additive optional fields may be introduced carefully with serde defaults, but
clients generated against older structs will ignore unknown response fields
only if their decoders allow it. The server continues to reject unknown fields
on inbound request bodies that opt into `deny_unknown_fields`.

## Non-Goals

The current protocol does not define:

- Multi-user identity or ACLs beyond the single local bearer token
- Per-session SSE streams (streams are workspace-scoped)
- Binary attachment upload APIs
- Interactive PTY multiplexing
- Client-to-client messaging
- Partial event patch frames (events are whole JSON objects)

Those features require an explicit protocol revision.
