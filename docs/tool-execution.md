# QQ Tool Execution And Security Design

Status: initial direction

## Purpose

This document defines how QQ agents read, search, and modify a workspace,
execute shell commands, and call MCP tools. It resolves the tool-execution
decisions deferred by `architecture.md` and `design.md`.

The design is ordered by the product priorities: speed and ease of use first,
with correctness, durability, and workspace safety as baseline constraints. A
tool layer that corrupts a checkout or loses history is a failure regardless
of latency, but every safety mechanism here is chosen to avoid long-held
locks, avoidable round trips, and interactive ceremony.

## The Tool Loop

Today a run is a single model turn that streams text. With tools, a run
becomes a loop owned by `qq-core`:

1. Assemble session context and request a model turn.
2. Stream text and tool-call requests as they arrive.
3. Persist each requested tool call, then resolve it: execute it, or wait for
   approval first when policy requires it.
4. Append tool results to context and request the next turn.
5. Repeat until the model finishes a turn with no tool calls, or the run is
   cancelled, interrupted, or fails.

The loop lives in `qq-core` next to `execute_run`, reusing the existing
cancellation watch, run permits, and persist-before-publish ordering. The TUI,
server, and direct CLI paths share it; no mode gets a parallel agent
implementation.

### Message And Content Model

Tool calls require structured message content. `qq_provider::Message` grows
from `role + String` to a role plus ordered content blocks:

- `Text { text }`
- `ToolCall { id, name, arguments }` (assistant turns)
- `ToolResult { call_id, content, is_error }` (returned turns)

`ModelRequest` gains the list of available tool declarations
(`ToolSpec { name, description, input_schema }`), and `ProviderEvent` gains:

- `ToolCallStarted { id, name }`
- `ToolCallArgumentsDelta { id, json }`
- `ToolCallCompleted { id }`

Each provider codec maps these to its wire protocol internally. Provider
identity still must not branch in the request hot path; tool declarations are
compiled into the request the same way messages are. This content-block
refactor is the prerequisite for everything else in this document and should
land first, with contract fixtures per codec.

### Persistence And Replay

Tool calls follow the same authority rule as text: persist before publish.
Each call is a row keyed by run, call id, name, arguments, state
(`requested`, `awaiting_approval`, `running`, `completed`, `failed`,
`denied`, `interrupted`), and result. New `SessionEvent` variants mirror the
state transitions so clients can replay a run and see exactly what the agent
did:

- `ToolCallRequested`
- `ToolApprovalRequested` / `ToolApprovalResolved`
- `ToolCallStarted`
- `ToolCallOutputDelta` (streamed shell output; batched like text deltas)
- `ToolCallFinished`

Recovery invariant: a tool call persisted as `running` without a persisted
result is never re-executed after a crash. `recover_interrupted_runs` marks it
`interrupted`; if the session resumes, the model sees an explicit interrupted
result and decides what to verify. Side effects are not idempotent, so replay
must never mean re-run.

Tool results can be large. Persist the full result up to a bounded size
(default 256 KiB per call, truncated with an explicit marker) and stream
deltas through the existing batching path so persistence latency stays off
the token hot path.

## Built-In Tools

The first tool set is small, executed in-process, and dispatched statically —
an enum, not a trait-object registry. This keeps per-call overhead near zero
and keeps the schema for each tool in one place:

- `read_file` — bounded read with offset/limit; records a content hash for
  the staleness guard below.
- `list_dir` — bounded directory listing.
- `search` — file-name and content search over the workspace, bounded result
  count.
- `edit_file` — exact-string replacement.
- `write_file` — full-file create or overwrite.
- `shell` — bounded command execution.

Read-only tools (`read_file`, `list_dir`, `search`) never require approval
inside the workspace and may execute concurrently. Everything else is a
mutating or externally visible tool and goes through policy.

## Safe File Editing

### Containment

The workspace root is canonicalized once at session creation. Every tool path
is resolved against it and its canonical parent must remain inside the root;
symlinks that escape the root are rejected. Paths outside the workspace are
not an error class the agent can approve its way through by default — wider
access is an explicit per-session grant, off by default.

### Edit Semantics

`edit_file` takes an exact `old_string`/`new_string` pair rather than a
unified diff. Exact-string replacement is what current models produce most
reliably, validation is trivial (the string is present exactly once or the
call fails), and a failed match returns a precise, retryable error instead of
a mis-applied hunk. `write_file` covers new files and full rewrites.

### Optimistic Concurrency, Not Locks

Safety across concurrent sessions in one workspace uses compare-and-swap, not
long-held locks:

1. `read_file` records the file's content hash in the session's file-state
   map.
2. `edit_file` and `write_file` (of an existing file) require a prior read in
   the same session.
3. At apply time, under a short per-workspace exclusive section, the current
   content is re-hashed. If it no longer matches what the session last read,
   the call fails with a stale-file error and the agent re-reads.
4. The apply itself validates `old_string` still matches, writes a temp file
   in the same directory, preserves permissions, and renames atomically.

The exclusive section covers only the hash-check-and-rename — microseconds —
so read-heavy parallelism across sessions is untouched and two writing
sessions interleave safely at file granularity. Semantic conflicts surface as
stale-file errors to the losing agent, which is the correct outcome: the
model re-reads and reconciles, exactly as a human would after a rebase.

This is the same progression `design.md` already commits to: concurrent
sessions share a checkout safely at file granularity now; editing subagents
get isolated worktrees later. Worktree orchestration stays deferred.

## Shell Execution

`shell` runs one command via `tokio::process::Command` with:

- Working directory pinned to the workspace (or a contained subdirectory).
- A default timeout (120 s, capped per call) that kills the whole process
  group, as does run cancellation.
- Bounded captured output (default 128 KiB, truncated head+tail with a
  marker), streamed to clients as `ToolCallOutputDelta` events through the
  existing batching path so long builds render live.
- No login/profile shell initialization on the hot path.

Shell is the one tool that cannot be contained by path checks — any command
can touch anything the server process can. Containment is therefore the
approval policy's job, and the honest framing is that `shell` approval trusts
the command. OS-level sandboxing (Landlock on Linux) is a worthwhile later
hardening step, but it is not a substitute for policy and is not part of the
initial implementation.

## MCP

MCP is the extension mechanism. QQ does not grow a plugin API; anything
beyond the built-in tools arrives as an MCP server.

- Servers are declared in configuration (global and per-workspace), with
  stdio and streamable-HTTP transports. Use the official Rust SDK (`rmcp`)
  with minimal features rather than hand-rolling the protocol.
- The QQ server owns one client connection per configured MCP server, shared
  by every session. Connections start lazily on first use (or eagerly at
  boot when configured), and tool schemas are fetched once and cached,
  refreshed on `list_changed` notifications. Per-session connections would
  multiply startup cost and defeat connection reuse; a shared client keeps
  MCP calls as cheap as built-ins after the first use.
- MCP tools are namespaced `mcp__<server>__<tool>` and merged into the same
  declaration list, persistence, events, and approval flow as built-in
  tools. Clients render them identically.
- Concurrency: calls to distinct MCP servers proceed in parallel; calls to
  one server are limited by a small per-server bound so a slow server
  backpressures instead of queueing unboundedly.

MCP tools execute outside the workspace containment model, so they are
externally visible by default and require approval unless allowlisted.

## Approval Policy

Approvals are explicit policy, not hidden behavior, and they are first-class
protocol objects so every client — TUI, CLI, or future web — uses the same
flow.

Each session has an approval mode:

- `read-only` — only read-only built-ins and allowlisted read-only MCP tools
  execute; everything else is denied without prompting.
- `ask` (interactive default) — workspace-contained edits, writes, shell, and
  non-allowlisted MCP calls each request approval.
- `auto` — workspace-contained edits and writes execute without prompting;
  shell commands matching the allowlist execute; everything else still asks.

The allowlist is deliberately simple: exact commands or command prefixes
(`cargo test`, `git status`), plus per-tool grants for MCP. No pattern DSL
until real use demands one.

Flow: when policy requires approval, the runtime persists and publishes
`ToolApprovalRequested` and the run stays active but waiting — it holds its
run permit, other sessions are unaffected, and cancellation still works. A
client responds with an idempotent `RespondToolApproval` command
(approve once, approve-and-allowlist for the session, or deny). Denials are
returned to the model as tool errors, not run failures, so the agent can take
another path. Non-interactive automation chooses its policy up front via
flags; a headless run with `ask` semantics and no attached client fails the
approval after a bounded wait rather than hanging forever.

Approval requests carry enough to decide without leaving the client: the
resolved path and a diff preview for edits, the exact command and cwd for
shell, the server, tool, and arguments for MCP.

## Parallelism

- **Across sessions:** unchanged — runs are already concurrent under bounded
  permits. The tool layer adds no global locks; the only cross-session
  exclusion is the per-workspace microsecond apply section.
- **Within a turn:** when a model emits several tool calls in one turn,
  read-only calls execute concurrently under a small bound; mutating and
  shell calls execute in request order. Results are appended to context in
  request order regardless of completion order so context assembly stays
  deterministic.
- **Persistence:** all tool events flow through the existing single-writer
  store worker with the existing batching, keeping SQLite off the streaming
  hot path.
- **MCP:** shared clients, per-server bounds, parallel across servers.

## Sequencing

1. Content-block message model and provider tool-call events, with codec
   contract fixtures. No behavior change for tool-less runs.
2. Tool loop in `qq-core` with read-only tools; parallel read execution.
3. `edit_file`/`write_file` with containment, staleness CAS, and atomic
   apply; approval protocol and session modes.
4. `shell` with bounds, streaming output, and the command allowlist.
5. MCP client, configuration, and namespaced tool integration.

Each step ships with tests for its failure paths — containment escapes,
stale-file conflicts, approval denial and idempotent retry, timeout and
cancellation kills, crash recovery marking `running` calls interrupted — and
a benchmark for tool-call dispatch overhead once the loop exists.

## Intentionally Deferred

- Git worktree or sandbox isolation for editing subagents.
- OS-level shell sandboxing (Landlock/seccomp).
- Approval pattern languages or per-path ACLs.
- A plugin API beyond MCP.
- Cross-workspace tool access as anything but an explicit grant.
