# Core Runtime Rearchitecture And Reasoning Effort

Status: proposed. This plan separates behavior-preserving `qq-core` module
extraction from the reasoning-effort feature. The durable target architecture
is defined in `docs/design/core-runtime.md`.

## A0 Baseline (2026-08-04)

Environment: `rustc 1.97.1`, `cargo 1.97.1`, Linux x86-64 on an Intel Core
Ultra 5 235H. Timings are representative local wall-clock measurements and
include compilation where Cargo reported it.

| Command | Result | Wall time |
| --- | --- | ---: |
| `cargo test -p qq-core` | 190 unit tests and 1 integration test passed | 12.09 s |
| `cargo test -p qq-core --test mcp_session` | 1 test passed | 0.35 s |
| `cargo bench -p qq-core --bench tool_dispatch` | `read_tool_loop: 66,427 ns/iteration` (1,000 iterations) | 46.47 s |

The baseline benchmark includes a fresh optimized build; the per-iteration
result is the comparison point for A2 and A3. Existing named characterization
coverage is:

| Invariant | Regression coverage |
| --- | --- |
| persist before publish | `streams_committed_run_events_and_snapshots_the_result` |
| one terminal event per accepted run | `shutdown_cancels_running_and_queued_prompts_before_returning` |
| atomic model turn and tool-call persistence | `terminal_runs_project_exact_tool_boundaries_across_restart` and `orphaned_tool_call_blocks_replay_with_synthesized_interrupted_results` |
| interrupted tools are not re-executed | `recovery_interrupts_running_tools_without_reexecuting_them` |
| approval waiter registration cannot lose a response | `immediate_approval_response_cannot_race_past_registered_waiter` |
| atomic child creation and no orphan on failure | `failed_child_run_insert_leaves_no_idle_orphan` |
| parent/child saturation cannot deadlock | `saturated_parents_awaiting_children_never_deadlock` |
| shutdown linearizes with child admission | `shutdown_closes_child_admission_before_scanning_unfinished_runs` |
| compaction and pruning do not rewrite transcript history | `assembly_after_compaction_is_summary_plus_verbatim_span_and_recompaction_folds`, `compaction_runs_account_usage_and_cost_but_join_no_transcript`, and `assembly_prunes_stale_read_only_results_but_never_mutating_ones` |

Later extraction must preserve the public facade exported from `lib.rs`:
`Runtime`, `RunStream`, `TurnRetryPolicy`, `RuntimeConfigError`, the MCP port,
and the session runtime/loader/grant types re-exported from `sessions.rs`.
The crate-private seams that must remain usable across the new modules are the
runtime event/tool-call vocabulary, `ToolGate` and `GateDecision`,
`SubagentSpawner` and its outcome, workspace preparation and file-state types,
tool execution results, and approval policy/grant types. Extraction may narrow
visibility to `pub(super)` where the resulting module tree permits it, but must
not broaden or break these seams merely to move code.

## Problem

`qq-core` has the correct crate-level responsibilities but concentrates several
independent subsystems in three very large files:

- `src/sessions.rs` contains the public session facade, live coordination,
  scheduler, run execution, approval and sub-agent workflows, SQLite worker,
  schema and migrations, transactional domain logic, context assembly,
  recovery, and most session tests.
- `src/lib.rs` contains the public facade, provider-turn state machine, prompt
  construction, retry policy, runtime event vocabulary, tool orchestration, and
  tests.
- `src/tools.rs` contains workspace capability handling, persistent file state,
  provider-visible schemas, dispatch, built-in implementations, and tests.

The concentration makes navigation, review, and changes to session behavior
unnecessarily risky. It also makes the addition of another durable model
setting—reasoning effort—touch an already crowded path from protocol selection
through session persistence and runtime construction to provider requests.

The solution is not to replace the current architecture. The implementation
already has important properties worth preserving:

- SQLite is authoritative and accessed by one blocking worker.
- state and corresponding events commit before live publication;
- accepted runs settle to one durable terminal outcome;
- completed model turns and their tool calls commit atomically;
- recovery does not replay uncertain side effects;
- root and child runs use separate bounded permit pools;
- context compaction and pruning affect assembly, not transcript truth;
- provider identity remains inside `qq-provider` adapters.

This plan makes those boundaries physically visible and then adds reasoning
effort through the resulting typed seams.

## Goals

1. Reduce the size and responsibility count of `lib.rs`, `tools.rs`, and
   `sessions.rs` without changing public behavior.
2. Make runtime, workspace, tools, session orchestration, and store transaction
   ownership explicit through idiomatic Rust modules.
3. Preserve current public `qq-core` APIs and crate dependency direction during
   extraction.
4. Preserve all durability, cancellation, replay, recovery, approval, and
   sub-agent invariants.
5. Add a provider-neutral optional reasoning-effort setting with durable
   session semantics.
6. Map explicit effort only in provider adapters that support it and reject
   unsupported explicit selections rather than silently ignoring them.
7. Keep every stage independently reviewable and verifiable.

## Non-Goals

- Splitting `qq-core` into additional crates.
- Replacing SQLite or introducing an ORM.
- Creating generic repository, event-bus, middleware, or tool-plugin
  frameworks.
- Rewriting the run loop and session store simultaneously.
- Changing command, replay, transcript, compaction, approval, or sub-agent
  semantics as part of module extraction.
- Supporting arbitrary provider-specific request parameters.
- Treating provider-exposed reasoning events as reasoning-effort configuration.
- Inventing numeric thinking-budget mappings without a verified provider
  contract.

## Observable Completion Criteria

The work is complete when:

- `qq-core` matches the responsibility and dependency boundaries in
  `docs/design/core-runtime.md` closely enough that each named subsystem has a
  clear module owner;
- `lib.rs` and `sessions.rs` act primarily as facades rather than implementation
  containers;
- all existing `qq-core` and workspace tests pass without weakened assertions;
- behavior-preserving extraction causes no meaningful regression in existing
  tool-dispatch benchmarks;
- `ReasoningEffort::{Low, Medium, High}` flows from configuration and protocol
  selection through durable sessions and runtime loading into
  `qq_provider::ModelRequest`;
- the runtime cache differentiates request behavior by effort;
- at least one provider adapter implements verified native effort mapping;
- adapters without support reject explicit effort with an actionable error;
- unset effort preserves prior provider request wire shapes;
- existing databases migrate with effort unset and retain provider-default
  behavior;
- session restart, model changes, and child resolution preserve the documented
  effort selection;
- protocol, configuration, provider, core, client/server, and TUI tests cover
  the new setting as applicable;
- formatting, Clippy, workspace tests, and workspace build pass.

## Workstream A — Behavior-Preserving `qq-core` Decomposition

This workstream changes physical ownership before changing session or provider
semantics. Every phase keeps current public re-exports and transaction
boundaries intact.

### A0 — Baseline And Characterization

Before moving implementation:

1. Record current test and benchmark commands and representative timings.
2. Ensure the critical invariants have named regression coverage:
   - persist before publish;
   - one terminal event per accepted run;
   - atomic model turn and tool-call persistence;
   - interrupted tools are not re-executed;
   - approval waiter registration cannot lose a response;
   - atomic child creation and no orphan on failure;
   - parent/child saturation cannot deadlock;
   - shutdown linearizes with child admission;
   - compaction and pruning do not rewrite transcript history.
3. Identify public and crate-private symbols that must retain their visibility.
4. Capture the existing `tool_dispatch` benchmark result in the change notes.

This phase adds tests only where a contract is not already demonstrated. It
does not duplicate the extensive existing session tests.

Acceptance:

- baseline `cargo test -p qq-core` and the MCP session integration test pass;
- benchmark output is recorded;
- no implementation behavior changes.

### A1 — Extract Low-Risk Runtime Components

Move cohesive, low-coupling code out of `lib.rs`:

- `TurnRetryPolicy`, transient classification, delay, and retry diagnostics to
  `runtime/retry.rs`;
- versioned agent prompt assembly to `runtime/prompt.rs`;
- internal `RuntimeEvent`, runtime tool-call, and turn-block vocabulary to
  `runtime/events.rs`;
- `ToolGate` and its decision types to `runtime/gate.rs`;
- `SubagentSpawner` and its outcome types to `runtime/subagent.rs`.

Keep `Runtime`, `RunStream`, public errors, and public re-exports stable. Do not
change provider request construction or event sequencing in this phase.

Acceptance:

- prompt bytes and prompt identity tests remain unchanged;
- retry behavior before and after visible output remains unchanged;
- runtime event mapping and reasoning lifecycle tests pass;
- public API consumers compile without source changes.

### A2 — Separate Workspace State From Tool Implementations

Create the workspace subsystem and move:

- canonical workspace access and containment to `workspace/access.rs`;
- root-to-leaf `AGENTS.md`/`CLAUDE.md` loading and hashing to
  `workspace/instructions.rs`;
- provider-work preflight to `workspace/prepare.rs`;
- session-aware read-before-write/CAS state to `workspace/file_state.rs`.

Tool code consumes workspace capabilities and file-state types but does not own
them. Session persistence continues to seed and commit file-state updates using
crate-private types.

Acceptance:

- symlink escape, canonicalization, instructions precedence, byte limit, and
  cancellation-before-provider tests pass;
- read-before-write and stale-content checks behave identically across runs and
  restart;
- no additional blocking work moves onto Tokio workers.

### A3 — Split Tool Declaration And Dispatch

Move provider-visible schemas into `tools/specs.rs`, dispatch and bounded result
shaping into `tools/dispatch.rs`, and concrete operations into behavior-named
modules.

The dispatcher stays explicit. Do not introduce boxed built-in tool trait
objects. MCP remains the external extension seam and `spawn_agent` remains a
runtime-provided tool rather than a filesystem built-in.

Acceptance:

- the provider receives identical built-in tool schemas;
- declaration order remains stable where request fixtures depend on it;
- read-only calls retain bounded parallel execution;
- mutating calls retain sequential request order;
- shell output streaming, truncation, cancellation, and approval metadata are
  unchanged;
- `cargo bench -p qq-core --bench tool_dispatch` shows no meaningful regression
  or the change is explained with measurements.

### A4 — Extract Session Live Workflows

Leave the store implementation in place initially and extract consumers of the
existing store facade:

1. durable approval workflow to `sessions/approvals.rs`;
2. worker/child orchestration to `sessions/subagents.rs`;
3. run scheduling and permit selection to `sessions/scheduler.rs`;
4. runtime-event persistence and accounting to `sessions/execution.rs`;
5. public session facade and live coordination to `sessions/runtime.rs`.

`sessions.rs` declares modules and re-exports the existing public session API.
Expected errors continue to map to the same durable outcomes.

Acceptance:

- approval request, timeout, duplicate response, session grant, workspace grant,
  and cancellation tests pass;
- child model resolution, validation, atomic creation, cancellation, restart,
  accounting, and saturation tests pass;
- scheduler failure and panicking-run tests still produce durable failures;
- output batching and replay ordering remain byte-for-byte compatible where
  asserted.

### A5 — Extract Store Worker And Schema

Move the asynchronous store facade and single database worker into
`sessions/store.rs` and `sessions/store/worker.rs`. Move database opening,
bootstrap, schema versioning, and migrations into `sessions/store/schema.rs`.

Preserve:

- one connection owned by one worker thread;
- separate bounded control and output queues;
- control priority;
- backpressure behavior;
- private permissions and symlink protection;
- WAL, foreign keys, full synchronous durability, and busy timeout;
- all historical migrations and their atomicity.

Acceptance:

- all migration fixtures pass without modification to expected state;
- store overload and scheduler failure behavior remains observable;
- database security tests pass;
- no SQL operation runs directly on a Tokio worker.

### A6 — Split Store Logic By Transactional Behavior

Move complete transactional operations rather than individual SQL statements:

- session command interpretation and command idempotency to `commands.rs`;
- run claim, cancellation, completion, accounting, and terminalization to
  `runs.rs`;
- model-turn and tool lifecycle persistence to `tools.rs`;
- approval/grant persistence to `approvals.rs`;
- snapshots, summaries, accounting projections, and event replay to
  `projection.rs`;
- context reconstruction and stale result pruning to `context.rs`;
- manual/automatic compaction claim and completion to `compaction.rs`;
- interrupted-run and active-tool settlement to `recovery.rs`.

Private shared SQL/domain types may remain in `store.rs` when multiple modules
need them. Avoid a `common.rs` dumping ground; shared types should have clear
domain ownership.

Acceptance:

- transactions retain their original begin/commit scope;
- command retries return their original durable receipt;
- context reconstruction remains exact across terminal, cancelled,
  interrupted, legacy, and compacted histories;
- recovery never re-executes uncertain tools;
- manual and automatic compaction retain the same terminal projection;
- snapshots and replay converge after restart.

### A7 — Reorganize Tests And Finalize Facades

Group the existing test suite by behavior while preserving black-box and
transaction-level coverage. Suggested groups are:

```text
sessions/tests/accounting.rs
sessions/tests/approvals.rs
sessions/tests/commands.rs
sessions/tests/compaction.rs
sessions/tests/context.rs
sessions/tests/execution.rs
sessions/tests/migrations.rs
sessions/tests/recovery.rs
sessions/tests/subagents.rs
```

Tests that require private module access may stay as descendant test modules.
`tests/mcp_session.rs` remains an integration test through public seams.

Finalize `lib.rs`, `tools.rs`, and `sessions.rs` as readable tables of contents.
Do not pursue a line-count target by creating trivial helpers or files.

Acceptance:

- a maintainer can locate each subsystem from the facade modules;
- test names and assertions retain their behavioral meaning;
- no broad formatting churn outside moved code;
- all Workstream A verification gates pass.

## Workstream B — Provider-Neutral Reasoning Effort

This workstream introduces typed request behavior after the affected runtime and
session boundaries are explicit. It may begin after A1 if needed, but protocol
and persistence changes should not be mixed with the high-risk A4–A6 moves in
one review.

### B0 — Confirm Provider Contracts

Before selecting mappings, verify current API contracts for each supported
protocol and model family:

- OpenAI Responses, including standard and Codex request modes;
- OpenAI Chat Completions and compatible xAI/custom deployments;
- Anthropic Messages;
- Google GenerateContent;
- Bedrock ConverseStream;
- Mantle protocol routes.

For each, record:

- whether selectable effort is supported;
- native field and legal values;
- whether support is endpoint-wide or model-specific;
- interaction with exposed reasoning streams;
- behavior when omitted;
- whether low/medium/high is a defensible mapping.

Do not implement mappings from model names or assumptions alone. Add provider
support one adapter at a time with captured request-contract tests.

Acceptance:

- the first supported adapter and every explicitly unsupported adapter have a
  documented behavior decision;
- no provider-specific values leak into the neutral type.

### B1 — Add The Shared Effort Vocabulary

Add to `qq-reasoning`:

```rust
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}
```

Provide only the traits needed by current consumers, such as serde,
`Display`, and `FromStr`. Re-export the type from `qq-provider` and
`qq-protocol` as appropriate.

Keep the concepts separate:

- `ModelMetadata::reasoning` indicates capability;
- `ReasoningEvent` carries provider-exposed run output;
- `ReasoningEffort` requests a deliberation level.

Acceptance:

- serde uses stable lowercase snake-case values;
- invalid values fail clearly;
- protocol and provider crates share one type without a dependency cycle.

### B2 — Extend The Provider-Neutral Request

Add `Option<ReasoningEffort>` to `qq_provider::ModelRequest` with an accessor and
builder. Keep the existing constructor signature source-compatible.

Rules:

- `None` emits no effort parameter and preserves the previous wire shape;
- an explicit setting must be mapped or rejected;
- adapters do not silently discard it.

Implement one verified native adapter first, preferably OpenAI Responses if its
current contract supports the three neutral levels. For every remaining
adapter, add either a verified mapping or an explicit unsupported-setting
error.

Acceptance:

- exact request JSON tests cover unset and every supported level;
- unset request fixtures remain unchanged;
- standard and special request modes are covered separately;
- unsupported errors identify provider/protocol and requested value;
- provider compiler/interface tests still enter through public seams.

### B3 — Add Effective Configuration And Direct Runtime Support

Add optional reasoning effort to the layered configuration model:

- document fields and merge behavior;
- `RuntimeOverrides`;
- `ConfigSnapshot` accessor and provenance;
- optional environment override if the project adopts one;
- `--reasoning-effort <low|medium|high>` for direct CLI use.

Add effort to immutable `qq_core::Runtime` generation settings and apply it when
building each `ModelRequest`. Include effort in the root `RuntimeKey` or any
other cache identity that can reuse a runtime.

Config and model capability remain distinct. If model metadata does not yet
advertise legal levels, explicit values are validated during runtime loading
and defensively by the adapter.

Acceptance:

- config precedence and explicit clearing are tested;
- direct CLI runs propagate effort to provider requests;
- runtimes with different effort levels cannot share a cache entry;
- omitted effort behaves exactly as before;
- unsupported model/provider combinations fail before visible provider work
  where metadata permits early validation.

### B4 — Add Protocol And Durable Session Selection

Extend `qq_protocol::ModelSelection` with:

```rust
pub reasoning_effort: Option<ReasoningEffort>
```

Use default/omit-on-none serde behavior while recognizing that strict
`deny_unknown_fields` makes this a coordinated protocol revision. Bump the
protocol version and update compatibility tests and protocol documentation.

Add a nullable `reasoning_effort` column to the session store in a new schema
migration. Update every create, update, load, claim, snapshot, and child
selection query. Existing rows migrate to `NULL` and therefore retain provider
default behavior.

Effort changes follow the existing model-selection rule: the active run keeps
its claimed immutable selection; the next run observes the update.

Acceptance:

- protocol round trips cover omitted, low, medium, and high values;
- older databases open and migrate atomically;
- existing sessions load with no explicit effort;
- create and model-update commands persist effort;
- restart preserves effort;
- active-run and next-run behavior is tested;
- effort never appears as transcript or model-context content.

### B5 — Define Worker And Child Resolution

Extend worker-model resolution so a child receives one complete durable
selection before atomic creation.

Precedence:

1. explicit child/spawn selection, if the interface exposes effort there;
2. explicitly configured worker effort;
3. parent session's persisted effort;
4. provider/model default when all are unset.

A route replacement must still pass model capability and authenticated served
model validation. The resolved effort becomes part of the child session's
persisted `ModelSelection`; later config changes do not alter it.

The initial `spawn_agent` schema need not expose a separate effort argument if
that would add unnecessary model-visible complexity. Configured worker effort
and parent inheritance are sufficient for the first version unless a concrete
per-task need is established.

Acceptance:

- explicit worker configuration wins according to the documented precedence;
- parent effort is preserved on fallback;
- runtime-load or capability failure creates no child session or run;
- restart preserves the child's resolved selection;
- child resolution uses the normal runtime-loading path rather than a second
  parser.

### B6 — Advertise Capability And Add TUI Selection

Evolve model metadata/catalog output to distinguish:

- whether reasoning is present;
- which effort levels are selectable;
- the provider/model default when known.

The TUI always offers `Default` and offers explicit levels only when advertised
for the selected model. It displays the current session effort and changes it
through the existing session model-selection command path.

Clients format and select typed values; they do not infer support from provider
names or `reasoning: bool` alone.

Acceptance:

- model catalog serialization includes capability without conflating it with
  selection;
- switching models updates available effort choices;
- selecting default clears the explicit value;
- changing effort affects the next run only;
- client/server/TUI round trips preserve the setting.

### B7 — Extend Provider Coverage Incrementally

For each additional provider protocol:

1. verify the live/current API contract;
2. define model capability metadata;
3. add exact request serialization tests;
4. add decoder tests only if request effort changes reasoning event behavior;
5. add or update the provider validation matrix and opt-in canary coverage;
6. remove the unsupported error only when the native mapping is proven.

A provider may remain unsupported for explicit effort indefinitely while still
working normally with `None`.

## Sequencing And Pull Request Boundaries

The preferred sequence is:

1. A0 baseline and missing characterization tests.
2. A1 runtime support-module extraction.
3. A2 workspace extraction.
4. A3 tool declaration/dispatch extraction.
5. A4 session live-workflow extraction.
6. A5 store worker/schema extraction.
7. A6 store behavioral-module extraction.
8. A7 test organization and facade cleanup.
9. B0 provider contract confirmation.
10. B1 shared effort vocabulary.
11. B2 provider-neutral request and first adapter.
12. B3 config/direct runtime support.
13. B4 protocol and durable session migration.
14. B5 child resolution.
15. B6 TUI/catalog UX.
16. B7 additional adapters.

B0–B3 may proceed after A1 if they avoid files currently moving in another
branch. B4 must not share a pull request with A4–A6 because both alter the most
sensitive session/store areas. Parallel writing work uses isolated worktrees;
read-only investigation may proceed concurrently.

Suggested review-sized changes:

```text
refactor(core): extract runtime support modules
refactor(core): separate workspace and tool responsibilities
refactor(core): extract session live workflows
refactor(core): split session store modules
refactor(core): organize behavioral session tests
feat(reasoning): add provider-neutral reasoning effort
feat(config): resolve reasoning effort for runtimes
feat(protocol)!: persist session reasoning effort
feat(tui): select session reasoning effort
```

The exact branch and commit names include the linked Linear identifier when
available.

## Verification Strategy

Use the narrowest relevant checks while iterating.

### Runtime Or Tool Extraction

```sh
cargo test -p qq-core
cargo test -p qq-core --test mcp_session
cargo bench -p qq-core --bench tool_dispatch
```

Add focused benchmarks if extraction touches measurable preflight or request
hot paths:

- text-only provider-turn orchestration;
- workspace preparation;
- tool specification assembly;
- child scheduling/coordination.

### Session Or Store Extraction

Run focused tests for the moved behavior first, then:

```sh
cargo test -p qq-core
cargo test -p qq-core --test mcp_session
```

Migration changes require all historical migration tests plus new old-to-current
fixtures. Recovery changes require interruption, active-tool, child ownership,
and restart coverage.

### Provider Effort

```sh
cargo test -p qq-reasoning
cargo test -p qq-provider
cargo bench -p qq-provider --bench provider_compiler
```

Run adapter contract/interface tests for every changed protocol. Live canaries
remain opt-in and follow `docs/design/providers.md`.

### Protocol, Config, And UI Effort

```sh
cargo test -p qq-protocol
cargo test -p qq-config
cargo test -p qq-client
cargo test -p qq-server
cargo test -p qq-tui
cargo test -p qq-core
```

### Final Workspace Gate

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

No phase is reported complete without evidence from the resulting tree and the
applicable checks. Benchmark regressions, unsupported providers, and skipped
live validation are reported explicitly.

## Risks And Mitigations

### Transaction Boundaries Are Accidentally Split

Mitigation: move complete transaction functions first; refactor internals only
after tests pass in the new module. Review begin/commit scope and event append
ordering in each move.

### Module Privacy Causes Broad API Expansion

Mitigation: prefer `pub(super)` and `pub(crate)` only where a real sibling
consumer exists. Keep the external re-export surface unchanged. Do not make
store implementation types public to simplify moves.

### Large File Moves Obscure Semantic Changes

Mitigation: extraction commits contain no intended behavior changes. Use
rustfmt only on moved code and keep semantic effort changes in later commits.

### Runtime Performance Regresses

Mitigation: avoid new dynamic dispatch, allocation, cloning, and channel hops.
Use the existing `tool_dispatch` benchmark and add focused benchmarks only for
changed hot paths.

### Runtime Cache Reuses The Wrong Effort

Mitigation: include optional effort in the runtime cache key and add a test that
constructs otherwise identical low/high selections.

### Provider Capability Metadata Drifts

Mitigation: adapters remain the defensive authority and reject invalid explicit
settings. Contract tests and live canaries validate catalog claims.

### Protocol Forward Compatibility Breaks

Mitigation: treat the strict `ModelSelection` addition as a protocol-versioned
change, update all clients and server together, and document compatibility in
`docs/design/protocol.md` when implemented.

### Existing Sessions Change Behavior On Migration

Mitigation: nullable migration defaults to no explicit effort. No migration
backfills a guessed effort from `reasoning: bool` or model identity.

### Portable Levels Have Misleading Provider Semantics

Mitigation: only add a native mapping when low/medium/high is defensible and
tested. Otherwise return unsupported for explicit values while allowing the
provider default.

## Documentation Lifecycle

`docs/design/core-runtime.md` records the durable target architecture. As each
behavior ships, that document and the relevant existing design documents are
updated in the same change:

- `docs/design/architecture.md` for workspace/crate shape;
- `docs/design/providers.md` for request mapping and validation;
- `docs/design/protocol.md` for the versioned selection field;
- `docs/design/tools.md` if tool/workspace ownership changes externally visible
  behavior.

This plan remains an in-flight status and sequencing document. When all accepted
work ships, durable decisions are folded into the design documents and this
file is deleted; git history remains the archive.
