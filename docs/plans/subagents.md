# Sub-Agent Sessions

Status: step 1 implemented (`spawn_agent` builtin: read-only child
sessions, depth/concurrency/budget caps, cancellation propagation, and
the step-3 prompt guidance in the versioned base prompt). Phase A is
implemented: configured `worker_model` resolution follows explicit tool
argument, configured worker, then persisted parent selection precedence and
persists the resolved selection on each child. Phase B—durable inclusive
parent/child accounting—remains unimplemented and must not be implemented as
a special case in the TUI or as an in-memory-only shortcut.

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
  parent run are capped small (3); child runs draw from their own
  bounded permit pool, separate from the root pool — parents hold their
  permit while awaiting children, so a shared pool would deadlock at
  saturation. Global concurrency stays bounded by the two pools
  combined. Child runs are cancelled when the parent run is cancelled.
  Parallel spawn calls in one turn run concurrently like read-only
  tools, and a run may spawn at most 8 children in total.
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

## Remaining Implementation Plan

The remaining work is split into two phases. Phase A establishes the durable
model-selection contract used when a child is created. Phase B establishes
one durable accounting projection consumed by every interface. Keeping these
separate avoids coupling configuration policy to accounting and permits each
phase to land with complete tests.

### Phase A — Worker-Model Configuration and Resolution

#### Contract

Add an optional `worker_model` configuration value using the same canonical
model-route syntax and type used by `model`. Resolve a child's model in this
order:

1. the `model` argument on that `spawn_agent` call;
2. configured `worker_model`;
3. the parent session's persisted model selection.

An explicit argument therefore remains a deliberate per-task override, while
an absent configuration preserves today's behavior. Resolution is performed
once, before child creation. The resulting complete `ModelSelection` is
persisted on the child; a later config change or server restart must not
change an already-created child's route.

The fallback is the parent **session selection**, not the application's
current default model. This matters for resumed sessions and sessions whose
model was explicitly changed. Provider, model, organization, and output-token
settings must pass through the existing model-loading and managed-policy
validation paths rather than a second, sub-agent-specific parser. Parent
organization and output-token settings should be inherited when the worker
route does not replace them, with normal model capability limits still
applied by the loader.

#### Architecture

- Extend `src/config/document.rs` with an optional `worker_model` field in the
  same layers, merge/provenance machinery, documentation, and serialization
  path as the primary model. Extend `ConfigSnapshot` in `src/config.rs` with
  the validated typed value. Missing and explicitly cleared values must have
  normal layered-config semantics; do not encode fallback by copying `model`
  into the snapshot.
- Reuse `ModelRoute` and the existing provider/model policy validation. A bad
  configured worker route should produce an actionable configuration error at
  load time, not fail only after the model decides to delegate.
- Pass the resolved optional worker selection through the application/runtime
  composition boundary into `qq-core`. Core must not read application config
  files or duplicate config precedence rules. Conversely, the tool schema
  should remain `{ task, model? }`; global policy is not model-visible tool
  input.
- Centralize the three-level precedence in one resolver used by
  `SessionSubagentSpawner`. The resolver should return a complete load request
  or `ModelSelection`, and normal runtime loading should return the runtime,
  pricing, and durable selection together. Do not create a synthetic parent
  session or mutate the parent's loaded runtime.
- Resolve and validate the child runtime before the atomic child-session/run
  creation transaction. This composes with the crash-safety work described in
  `docs/plans/terminal-bench-readiness.md`: once the transaction commits, the
  child has both a durable model selection and a runnable queued run. A load
  failure leaves neither an orphan child nor a partial run.
- Preserve the explicit selected route on the child even when it equals the
  parent. This keeps audit/resume behavior independent from future defaults.

Likely touch points are `src/config/document.rs`, `src/config.rs`,
`src/runtime.rs`, application construction in `src/main.rs`, and
`SessionSubagentSpawner`/child creation in `crates/qq-core/src/sessions.rs`.
Protocol changes are unnecessary unless model resolution is moved behind a
new core-facing request type; if one is introduced, it should carry typed
model data rather than raw config strings.

#### Verification and acceptance

- Config tests cover user/project layering, precedence, explicit clearing,
  serialization, provenance, malformed routes, unavailable providers, and
  managed-provider/model policy rejection.
- Resolver unit tests cover all three precedence levels, including a resumed
  parent whose persisted model differs from the current application default.
- Session tests prove the child persists the resolved selection, an explicit
  tool argument wins, missing `worker_model` falls back to the parent, and
  config changes after creation do not affect the child.
- Failure tests prove model-load and policy failures create no child session
  or queued run and return a bounded, useful tool error.
- Existing root-session model behavior and the `spawn_agent` schema remain
  unchanged.

Phase A is complete when users can set `worker_model`, every child records the
correct durable selection, and every route reaches the provider through the
same validation/loading path as a root model.

### Phase B — Durable Inclusive Usage and Cost Accounting

#### Accounting model

Keep direct and inclusive accounting conceptually distinct:

- **Direct totals** are usage and cost generated by runs owned by that session.
- **Inclusive totals** are the session's direct totals plus direct totals of
  its immediate children.

Depth is currently capped at one, but the query should express ownership
explicitly rather than rely on that invariant accidentally. A child's own
inclusive display equals its direct totals today. Never sum a child's
inclusive total into its parent: doing so would double-count if deeper
nesting is introduced later.

Runs remain the source of truth. Do not increment cached parent counters when
a child finishes: retries, duplicate terminal events, crashes between child
and parent writes, cancellation, repair, and pruning would all make such
counters drift. Compute the projection from durable run/session ownership in
the store. If performance eventually requires materialization, add a
transactionally maintained projection plus a rebuild/invariant check; do not
start with write-only denormalized counters.

#### Semantics

- Sum token categories with checked arithmetic. Overflow makes the aggregate
  unavailable and observable as an accounting error; it must not wrap or
  silently saturate.
- Preserve unknown-cost information. If an included run has billable usage
  but its price/cost is unknown, inclusive cost is unknown rather than the sum
  of only known children. Zero usage is not an unknown charge. Use an explicit
  type/state for this instead of treating `0.0` as both zero and unknown.
- Include persisted usage from terminal child runs regardless of success,
  failure, or cancellation; accounting describes consumed resources, not task
  success. In-progress usage appears as it is durably recorded under the same
  rules used for direct session totals.
- A child remains independently visible with its direct totals. Showing a
  parent's inclusive totals and its child rows together is intentional UI
  visibility, not an instruction to sum every visible row.
- Pruning a child removes its runs from the live derived inclusive projection.
  Historical benchmark/ATIF artifacts that require immutable trial cost must
  materialize their own snapshot before pruning. Document this behavior; do
  not leave stale cost hidden on the parent.

#### Architecture

- Introduce a store-level accounting projection that returns typed direct and
  inclusive usage/cost for a session in one consistent read. Build it over
  `sessions.parent_id` and persisted runs (`usage_json`/`cost_usd`), with a
  clear policy for corrupt legacy rows. Keep aggregation and unknown-value
  rules in one domain helper rather than SQL, TUI, and export implementations
  that can diverge.
- Evolve `qq-protocol` session snapshots/events to name direct and inclusive
  totals explicitly. Prefer adding a structured accounting object and a
  compatibility migration over silently changing the meaning of existing
  `usage`/`cost_usd` fields. During a compatibility window, old fields may map
  to direct totals while new consumers choose inclusive totals deliberately.
- Make the store projection the only source used by session snapshots,
  durable `SessionUpdated` payloads, TUI session/detail displays, headless
  output, and future ATIF conversion. Clients format values; they do not join
  children or calculate cost.
- When durable child accounting changes, publish an updated parent projection
  as well as the child's update. Establish deterministic child-before-parent
  event ordering after the accounting transaction commits. Event consumers
  must remain correct after a missed event by reloading the snapshot; events
  are invalidations/projections, not the accounting source of truth.
- Ensure the parent refresh is driven by durable parent ownership, not by the
  lifetime of the awaiting `spawn_agent` future. This is required for parent
  cancellation, server restart, and the atomic queued-child handoff planned
  in `docs/plans/terminal-bench-readiness.md`.
- Add the protocol/schema migration using the repository's normal additive
  compatibility rules. Existing databases derive totals from run rows and
  need no accounting backfill unless malformed legacy usage is detected.

Likely touch points are the store and run-finalization paths in
`crates/qq-core/src/sessions.rs`, session types/events in
`crates/qq-protocol/src/sessions.rs`, and the TUI/application consumers of
those fields. ATIF support should consume this projection when implemented,
not introduce a third accounting implementation.

#### Verification and acceptance

- Domain tests cover known totals, mixed known/unknown prices, zero usage,
  checked-overflow behavior, and no double counting.
- Store tests cover one parent with multiple parallel children, direct parent
  usage, partial/in-progress durable usage, failed and cancelled children,
  restart/reload, child pruning, and corrupt or legacy accounting rows.
- Event tests prove child and parent projections update once, in deterministic
  order, and snapshot reload yields the same values even if events are missed.
- Protocol compatibility tests cover old persisted events/snapshots and the
  additive direct/inclusive representation.
- TUI and headless tests prove both parent inclusive and child direct totals
  are labeled consistently; neither client performs its own aggregation.
- End-to-end tests run parallel `spawn_agent` calls with distinct costs and
  assert: each child reports only itself, the parent reports parent plus each
  child exactly once, and the same values survive process restart.

Phase B is complete when every interface obtains identical inclusive totals
from durable state, unknown cost cannot be mistaken for zero, and restart,
cancellation, retries, and pruning cannot cause accounting drift.

## Sequencing

1. **Complete:** `spawn_agent` tool: child creation, read-only mode, result
   return, depth/concurrency caps, and cancellation propagation.
2. **Complete:** Phase A worker-model configuration, typed resolution, policy
   validation, and durable child model selection.
3. **Phase B:** store-level direct/inclusive accounting projection, protocol
   representation, parent refresh events, and all client integrations.
4. **Complete:** agent-instruction guidance (the delegation formula) in the
   versioned base prompt.
5. **Later:** mutating children with explicit approval semantics; parallel
   fan-out helpers.
