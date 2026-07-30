# Sub-Agent Sessions

Status: proposed.

Main-session context is premium real estate: every byte of gathered
evidence is re-sent on every later turn, crowds out reasoning, and ages
into compaction fodder. Compaction (docs/plans/compaction.md) recovers
context after it is spent; delegation avoids spending it. A sub-agent
gathers evidence in a disposable child session and returns only the
distilled answer to the parent.

## The Delegation Formula

Delegate a task to a sub-agent when all three hold:

1. **Compression** — expected raw evidence is much larger than the
   distilled answer (rule of thumb: 5× or more). Breadth-shaped work
   qualifies: "find every caller", "survey how X is handled", "which
   files implement Y". Depth-shaped work does not: one targeted read.
2. **Disposability** — the parent will not need the evidence verbatim
   later. If the parent must edit the file it just read, delegating the
   read is waste; it will re-read anyway.
3. **Independence** — the task is self-contained from a one-shot brief.
   Work that needs mid-flight steering belongs inline.

Override: several independent questions are worth delegating even when
each is small, because children run concurrently.

Never delegate below the cost floor: a child pays its system prompt and
tool declarations on every one of its turns. Single greps, single file
reads, and quick lookups are always inline. The default is inline;
delegation is for breadth. These rules live in the base agent
instructions — a spawn tool without guidance is used never or always,
both wrong.

## Mechanics

Foundations that already exist: sessions carry `parent_id` (the TUI
threads children under parents), run permits bound concurrency,
sessions carry per-session models and cost accounting, and tool results
are size-bounded.

- **`spawn_agent` builtin** — `{ task, model? }`. Creates a child
  session in the same workspace, submits `task` as its prompt, runs it
  to completion under the existing loop bounds, and returns the child's
  final assistant text as the tool result (existing result-size bounds
  apply). The call is one tool call in the parent: collapsed one-liner,
  live status, expandable like any other.
- **Read-only by default.** Children run in `read-only` approval mode:
  research agents never surface approval prompts and carry no delegated
  mutation authority. A mutating child mode is future work and requires
  explicit parent-side approval semantics of its own.
- **Worker model.** Children default to the configured `worker_model`
  (config, falling back to the parent's model). Cheap fast models make
  breadth delegation net-cheaper than inline gathering; the parent's
  conversation keeps its own model.
- **Bounds.** Depth 1: children cannot spawn. Concurrent children per
  parent run are capped small (2–4); each child consumes a run permit
  so global concurrency holds. Child runs are cancelled when the parent
  run is cancelled or times out. Parallel spawn calls in one turn run
  concurrently like read-only tools.
- **Cost and visibility.** Child usage and cost roll up into the parent
  session's displayed totals (children also show their own). The child
  session persists after completion — auditable like any session, and
  prunable like any other with the existing tools.
- **Context hygiene both directions.** The child starts from a clean
  context (task brief + agent instructions — not the parent transcript;
  the brief must carry what matters). The parent receives only the
  final message. Result pruning and compaction apply to each session
  independently.

## What This Is Not

- Not multi-agent editing: children do not mutate. Parallel mutation
  needs the run-snapshot/undo layer and conflict semantics first.
- Not a persistent worker pool: children are one-shot and disposable.
- Not automatic: the model chooses when to spawn, guided by the
  formula in its instructions; users see every spawn as a tool call.

## Sequencing

1. `spawn_agent` tool: child creation, read-only mode, result return,
   depth/concurrency caps, cancellation propagation.
2. Worker-model config knob and cost roll-up.
3. Agent-instruction guidance (the formula) in the versioned base
   prompt.
4. Later: mutating children with explicit approval semantics; parallel
   fan-out helpers.
