# QQ Harness Reliability, Cost, And Terminal-Bench Readiness

Status: active. Re-baselined against `main` on 2026-08-02.

This plan turns QQ into a trustworthy autonomous terminal harness before
optimizing it against Terminal-Bench. It covers the failures found in the
session, context, streaming, headless, tool, provider, and sub-agent paths
without adding further crates or creating a second agent runtime.

The public benchmark is an evaluation target, not the product architecture.
Every improvement in this plan must also make ordinary local, server, and
future remote QQ sessions more correct, faster, or cheaper.

## Goal

QQ should maximize **verified successful work per dollar and per minute**.
Raw process speed matters, but it is subordinate to completing the task
correctly.

The priority order is:

1. Fixed-model verified task success.
2. Dollars and uncached tokens per successful task.
3. Wall-clock time per successful task.
4. Harness reliability and variance.
5. Local startup, streaming, persistence, replay, and rendering overhead.

The implementation is successful when:

- A non-interactive `qq run` exercises the same durable `SessionRuntime` as
  the TUI and server.
- Every committed turn remains valid future model context across completion,
  failure, cancellation, interruption, restart, and compaction.
- Long model and reasoning streams have linear persistence cost and remain
  visible without starving other sessions.
- Context budgeting uses the selected model's effective limits and reported
  usage rather than a global byte threshold alone.
- Every evaluation trial produces a complete replayable QQ trace and a valid
  Agent Trajectory Interchange Format (ATIF) trajectory.
- QQ can be compared against another harness with the same model, effort,
  resources, timeouts, and task set.
- Changes are promoted by paired evidence: a statistically meaningful
  capability gain, or equal capability at materially lower cost or latency.

## Current State And Confirmed Gaps

QQ already has important foundations:

- One provider-neutral runtime is reused by direct CLI, TUI, and server paths.
- Durable non-interactive `qq run` now composes `SessionRuntime`; `qq ask`
  remains the intentionally lightweight, non-durable compatibility path.
- Commands are idempotent and authoritative events persist before publication.
- Prompt queueing, tool turns, run completion, and compaction use transactions.
- SQLite work stays off Tokio workers.
- Queues, model turns, tool calls, root runs, and child runs are bounded.
- Shell processes have timeout, cancellation, process-group cleanup, bounded
  capture, and live output.
- Read-only sub-agents have clean contexts, bounded depth and concurrency,
  separate run permits, cancellation propagation, and durable child sessions.
- Provider usage, cache usage, and estimated cost are normalized and retained.
- The TUI now preserves and efficiently renders long completed output.

### 2026-08-03 Recovery Ledger

This ledger distinguishes shipped work from the broader phase contracts below.
Passing one regression does not imply that its whole phase is complete.

| Slice | Status | Evidence | Still open |
| --- | --- | --- | --- |
| Durable headless execution | on `main` | `f7b34a1` adds `qq run` with durable events, cancellation, timeout, turn/cost budgets, JSONL, and exit-status mapping | ATIF conversion, Harbor integration, resolved-model trial identity |
| Transient provider recovery | on `main` | `55d2aa7` retries a turn only before user-visible output | One combined retry/amplification budget across provider transport and core |
| Accepted-run supervision | implemented in `DEV-726` | Run-task panics settle as durable failures; headless sink/trace failures and owner aborts retain cancellation ownership; explicit runtime shutdown drains queued and running work; reopen interrupts abandoned running work without replaying tools | Process loss intentionally interrupts the in-flight provider request rather than attempting unsafe continuation |
| Authoritative context projection | implemented in `DEV-727` | Completed, failed, cancelled, interrupted, and recovered runs share one projection; runtime notices and exact persisted/interrupted/not-executed tool results reach follow-up, capacity, and compaction requests without altering the transcript | Merge the reviewed slice |
| Atomic child-run lifecycle | implemented in `DEV-728` | One transaction persists the initialized read-only child, queued prompt run, and parent-run ownership before publishing consecutive events; recovery cancels unclaimed children of interrupted parents; the parent receives only the final model turn | Merge with its `DEV-727` prerequisite |
| Tool/turn budget behavior | renewable slices and live cost visibility implemented in `DEV-725`; explicit headless turn/timeout gate on `main` | `d989cf8` counts the provider-request boundary, so `--max-turns` cancels before a silent over-budget turn; `DEV-725` turns the internal 256-call ceiling into a persisted checkpoint, restores tools inside the same durable run, and commits active-run cost so `--max-cost-usd` can cancel truthfully | Core-owned configurable token/dollar/turn outcomes across every front end |

The remaining P0 longevity contract is explicit: every accepted run must reach
one durable terminal event; every imposed bound must produce a truthful outcome;
and resumable work must retain enough committed context to continue without
repeating side effects. Raising a numeric cap is not completion semantics.

The remaining gaps are material:

| Area | Current behavior | Consequence |
| --- | --- | --- |
| Evaluation export | `qq run` is durable, but ATIF conversion and the Harbor adapter are absent | Trials cannot yet be compared or replayed through the benchmark's standard artifact |
| Streaming persistence | Each text batch reassembles the full context for capacity checks and grows SQLite strings by concatenation | Long output has structurally superlinear persistence work |
| Reasoning persistence | Provider reasoning deltas commit independently rather than using text batching | High-effort models can create excessive transactions and queue pressure |
| Store scheduling | The single worker always prefers control traffic; a full output queue is retried by polling every millisecond | Output can be delayed or starved under control load |
| Context budgeting | Automatic compaction uses a fixed 4 MiB budget and a 70% byte trigger | Small-context models may fail before compaction; large-context models may compact and spend unnecessarily |
| Resolved model state | Runtime loading returns the runtime and pricing but drops effective context limits and generation capabilities | Core cannot budget, explain, or reproduce the exact model execution |
| Completion behavior | Internal tool slices checkpoint and continue without a terminal event, but caller-requested turn/time/cost budgets still settle through the headless adapter rather than one core-owned outcome | TUI, server, and future clients cannot yet impose and observe the same explicit overall budget contract |
| Terminal control | `shell` is one-shot with null stdin and no persistent process or PTY handle | Interactive programs and background services are awkward or impossible |
| Search and editing | Built-in search is bounded literal scanning; edits require exact replacement | Discovery or mutation can consume unnecessary model turns on large repositories |
| Cost control | Headless time, turn, and cost limits exist, but core has no unified token/dollar/turn outcome and provider/core retries can multiply attempts | Other front ends lack the same guarantees, and one logical turn can exceed the intended request budget |
| Evaluation | Existing benchmarks cover provider construction, a synthetic read-tool loop, and manual rendering cases | There is no end-to-end evidence for useful-result latency, reliability, or cost |

Existing plans remain authoritative for their narrower scopes:

- `docs/plans/compaction.md` owns transcript pruning, summary mechanics, and
  the pending `search_history` work.
- `docs/plans/subagents.md` owns the delegation policy, worker-model setting,
  and child-cost roll-up.
- This plan supplies the cross-cutting sequencing, contracts, evaluation
  gates, and missing runtime work needed to make those plans effective.

## Architectural Rules

### Preserve The Crate Graph

Do not add storage, benchmark, terminal, telemetry, or agent-framework crates.
The existing crates already have the correct ownership:

- The root package remains the composition root and owns CLI modes, translation
  between config and runtime settings, server lifecycle, and benchmark
  integration.
- `qq-config` owns layered configuration and built-in provider/model presets.
- `qq-auth` owns provider OAuth, credential storage, and secret resolution.
- `qq-core` owns the agent loop, session semantics, context assembly, tools,
  persistence, cancellation, and scheduling.
- `qq-provider` owns provider-neutral generation inputs and concrete protocol,
  authentication, transport, framing, retry, and prompt-cache behavior.
- `qq-protocol` owns versioned externally visible commands, events, snapshots,
  identifiers, and run metadata.
- `qq-server` owns HTTP/SSE route wiring, local-instance metadata, and bearer
  authentication.
- `qq-client` owns authenticated HTTP/SSE requests, bounded decoding,
  reconnect, and replay.
- `qq-tui` projects protocol state and renders it; it does not acquire runtime
  responsibilities.
- `xtask` owns repository evaluation automation and generated-result handling.

The Harbor adapter is a non-shipping integration, not a new Rust crate. It may
live under `benchmarks/harbor/` because Harbor requires an importable installed
agent adapter. Generated jobs, credentials, trajectories, and benchmark output
must remain untracked.

### Preserve The Main Runtime Interface

Keep the external `SessionRuntime` interface small:

```text
open
command
snapshot
subscribe
shutdown
```

`qq run`, the TUI, and the server must compose those operations instead of
calling separate agent implementations. `shutdown` closes command and child-run
admission, stops new claims, durably cancels unfinished work, and waits for
settlement while snapshot and subscription reads remain available. Add protocol
commands or event fields only for behavior that must be externally visible or
durable.

### Deepen Internal Modules

The current large files should be split only as behavior moves behind a real
interface. Use sibling module files and directories, never `mod.rs`.

```text
crates/qq-core/src/
  sessions.rs                 external session-runtime interface
  sessions/context.rs         context projection, pruning, compaction planning
  sessions/store.rs           SQLite worker, migrations, durable operations
  sessions/subagents.rs       child ownership and final-result contract
  execution.rs                model/tool completion loop and budgets
  tools.rs                    built-in tool dispatch
  tools/terminal.rs           supervised persistent process implementation
```

Exact file moves should happen with the phase that needs the seam. Do not do a
standalone file-splitting refactor.

The intended deep modules are:

1. **Headless run module**
   - Interface: validated run options in, terminal run outcome out.
   - Hides session creation, commands, subscription, cancellation, trace
     writing, and exit-code mapping.
2. **Context module**
   - Interface: durable session state plus a resolved model budget in,
     `Send`, `Compact`, or `Reject` plan out.
   - Hides status projection, tool-result pairing, pruning, token estimates,
     summary selection, and capacity accounting.
3. **Session store module**
   - Interface: the existing typed durable operations.
   - Hides SQLite schema, chunks, transaction grouping, queue fairness, and
     migrations. Tests use real temporary SQLite; do not add a storage trait
     solely for tests.
4. **Execution module**
   - Interface: one claimed run plus its resolved runtime in, durable runtime
     events and one terminal outcome out.
   - Hides model turns, tool sequencing, completion policy, budget decisions,
     retry activity, and stall observations.
5. **Terminal supervisor module**
   - Interface: bounded start, poll, write, and stop actions.
   - Hides child processes, PTY or pipe implementation, output cursors,
     process groups, timeouts, and cancellation.

Tests should cross these interfaces and assert observable outcomes. Retire
implementation-trivia tests made redundant by the deeper interface tests.

## Measurement Contract

### Trial Identity

Every headless trial must record:

- QQ version and source revision.
- System-prompt version and content hash.
- Tool declaration version or stable schema hash.
- Effective provider/model route.
- Reasoning effort or other benchmark-relevant generation settings.
- Effective output and context limits.
- Pricing provenance.
- Workspace identity without leaking host-only secrets.
- Benchmark dataset, task, trial, seed, machine class, timeout, and resource
  configuration when the Harbor adapter supplies them.

Historical runs must remain explainable after configuration changes.

### Per-Run And Per-Turn Measurements

Record monotonic durations or timestamp pairs for:

- Command received and durably acknowledged.
- Run queued, claimed, runtime ready, and finished.
- Model request begun, first semantic provider event, and model turn completed.
- Tool requested, started, and finished.
- Cancellation requested and observed.
- Compaction started and committed.

Record these counters:

- Fresh input, cache-read input, cache-write input, reasoning when the provider
  reports it, and output tokens.
- Estimated and reported cost, with unknown cost represented as unknown.
- Model turns, tool calls, failed tool calls, retries, compactions, and
  sub-agents.
- Context occupancy and reserved output budget per turn.
- Queue wait and persistence delay.
- Peak resident memory and process CPU for evaluation jobs when available from
  the outer benchmark runner.

Do not introduce a generic telemetry framework before there is a second real
consumer. The first implementation should persist concrete run/turn fields and
emit the headless JSONL form. The Harbor adapter and `xtask` evaluation report
are the two consumers that justify a stable trace projection.

### Initial Local SLOs

These are engineering budgets, not promises. Recalibrate them only after a
repeatable baseline demonstrates that a different threshold is necessary.

| Signal | Initial target |
| --- | ---: |
| Local durable command acknowledgement | p95 <= 10 ms |
| Warm claimed run to provider send, excluding queue/provider latency | p95 <= 25 ms |
| Provider semantic delta to durable event | p95 <= 15 ms; p99 <= 40 ms |
| Provider delta to visible TUI update | p95 <= 25 ms; p99 <= 60 ms |
| Cancellation observed by active model/tool work | p95 <= 100 ms |
| One MiB stream persistence | Doubling bytes costs no more than 2.2x wall time |
| Eight simultaneous output streams | No stream starved by persistence for more than 50 ms |
| One MiB request plus 32 tool schemas encoded | p95 <= 10 ms; temporary heap <= 2x encoded body |
| Context overflow sent to a provider | Zero |
| Compaction shrinkage | At least 8x tokens with required-fact tests passing |
| Stable-prefix cache use after turn two, where supported | At least 80% |
| Retry amplification outside incidents | Fewer than 1.05 HTTP sends per logical turn |
| Harness-caused benchmark failure | Below 0.5% |

### Evaluation Scorecard

Publish these together for every experiment:

- Pass rate or normalized reward with confidence interval.
- Dollars per attempted task and dollars per passed task.
- Total and uncached tokens per passed task.
- Median and p95 wall time per passed task.
- Harness/infrastructure failure rate.
- Failure category counts.

The first competitive release gate is:

1. Remain within one percentage point of the strongest comparable harness
   under the same model and settings.
2. Then beat its dollars-per-pass or successful-task wall time by at least
   20-25%, or produce a statistically meaningful pass-rate lead.

Do not hide a capability regression behind a cheaper average.

## Phase 1: Durable Autonomous Headless Run

Priority: P0. This is the critical-path vertical slice.

Status: partially delivered by `f7b34a1`. The durable CLI, cancellation,
budgets, JSONL stream, and exit mapping exist. Trial identity, ATIF conversion,
Harbor integration, and the end-to-end evaluation gate remain.

### Behavior

`qq run` is the durable non-interactive execution mode. Its command shape is:

```text
qq run [OPTIONS] PROMPT

--workspace PATH
--approval read-only|auto
--timeout-seconds N
--max-turns N
--max-cost-usd VALUE
--format text|jsonl
--trace PATH
```

Names may change during CLI review, but the behavior may not:

- The default workspace is the canonical current directory.
- `auto` is an explicit grant of unattended authority and must be clearly
  described as suitable only for a disposable or otherwise trusted workspace.
- Headless mode never selects interactive `ask` approval. A configuration that
  could require approval must fail before submitting the prompt rather than
  wait indefinitely.
- The root package opens `SessionRuntime`, resolves the workspace, creates a
  session, applies the model and approval choices, submits the prompt, and
  subscribes from the returned cursor.
- Ctrl-C and the timeout send the ordinary idempotent cancellation command and
  wait for a terminal durable event within a bounded shutdown period.
- Embedded shutdown stops HTTP admission, settles accepted runs, then bounds
  HTTP/SSE response draining so a held subscription cannot block teardown.
- Text format streams user-facing output and concise tool activity to stderr
  while leaving the final answer on stdout.
- JSONL format emits ordered protocol events plus trial metadata without
  losing tool activity.
- Exit status distinguishes successful completion, task/model failure,
  timeout/budget exhaustion, and harness/persistence failure.
- `qq ask` remains the lightweight one-shot compatibility path; it is not used
  for autonomous evaluation.

### Trace And ATIF

The QQ event store remains authoritative. Do not put ATIF-specific concepts
inside `qq-core` or `qq-protocol`.

Add enough durable per-turn information to reconstruct:

- User and assistant messages.
- Provider-exposed reasoning channels only.
- Tool calls, arguments, results, errors, and timings.
- Per-turn model identity and token/cost metrics.
- Parent and child trajectory relationships.
- Terminal run outcome.

Extend `model_turns` additively with per-turn usage and timing. Extend protocol
events additively when live consumers need those values. Old rows and events
must retain their existing defaults and decode successfully.

The repository-owned Harbor installed-agent adapter:

1. Installs or locates the `qq` binary in the task container.
2. Runs `qq run` in the task workspace with explicit unattended policy.
3. Writes agent logs under Harbor's agent-log directory.
4. Converts QQ JSONL/durable events to the current ATIF schema after the run.
5. Embeds child trajectories when sub-agents ran.
6. Populates Harbor cost and token fields.
7. Validates `trajectory.json` before returning.

`cargo xtask eval` should build the release binary, invoke Harbor with a
selected dataset/task/model/repetition count, place generated jobs outside
tracked source, and summarize score, cost, time, and failure categories.
It must never print credentials.

### Tests

- CLI parsing covers every option and rejects `ask` approval in headless mode.
- A fake provider and temporary SQLite/workspace prove that `qq run` performs
  an auto-approved write and shell call through `SessionRuntime`.
- Events in JSONL have monotonic cursors and exactly one terminal outcome.
- Cancellation and timeout leave no active run or child process.
- Post-submit output and trace failures, owner-task abort, and graceful runtime
  shutdown each leave no accepted run active and publish one terminal event.
- Reopening a store interrupts abandoned running messages and tool calls
  without re-executing their side effects.
- Restart after a completed run reproduces the same final snapshot.
- Unknown usage or pricing remains explicitly unknown in the trace.
- ATIF conversion fixtures cover text-only, tool-loop, failure, cancellation,
  compaction, and parent/child runs.
- Harbor's ATIF validator accepts every generated fixture.
- One local Harbor smoke task passes without approval waits.

### Acceptance

- Zero approval stalls or direct-runtime bypasses in benchmark mode.
- Every trial has a complete QQ trace.
- Every passing Harbor trial has a valid ATIF trajectory.
- The first full Terminal-Bench baseline can run without manual intervention.

## Phase 2: Authoritative Context And Crash-Safe Child Runs

Priority: P0. Complete before judging compaction quality or sub-agent
economics.

Status: implemented by `DEV-727` and `DEV-728`. Terminal runtime notices, exact
tool-result pairing, restart equivalence, and shared
follow-up/capacity/compaction input now use one authoritative projection. Child
session creation, queued prompt creation, and parent-run ownership now commit
atomically, and a parent receives only the child's final model turn.

### Context Projection Contract

Define and test a status matrix for future model context:

| Run status | Committed user message | Committed assistant turns | Completed tool results | Runtime notice |
| --- | --- | --- | --- | --- |
| completed | included | included | included | none |
| running | included through committed turn | included | included | none |
| interrupted | included | included | included or synthesized interruption | explicit interruption |
| failed | included | included | included or synthesized failure | explicit failure |
| cancelled | included | included | included or synthesized cancellation | explicit cancellation |

The runtime notice is QQ-generated and must never masquerade as a user
instruction. Use one provider-neutral representation consistently across all
codecs.

For every replayed assistant `ToolCall`, assembly must append exactly one
matching `ToolResult`:

- Use the persisted result when one exists.
- Synthesize a deterministic interrupted result when recovery proves the call
  began but did not finish.
- Synthesize a deterministic not-executed result when cancellation or failure
  ended the run before execution.
- Never re-execute a recovered call.

Update compaction input to use this same context projection. The TUI transcript
remains unchanged and authoritative.

### Child Ownership Contract

`spawn_agent` uses one internal transaction instead of separate child-session
creation and prompt-submission commands. The transaction:

- Creates the child with its parent, workspace, model, read-only policy, and
  title.
- Creates and queues its prompt run.
- Installs parent ownership before the child becomes claimable.
- Returns child session and run identifiers together.

Parent cancellation finds every owned child through the persisted owner-run
link. On restart, recovery cancels a queued, unclaimed child when its owning
parent is interrupted. A child that cannot be submitted leaves no idle orphan.

The parent tool result contains only the final completed model turn's answer or
an explicit child failure. Intermediate assistant messages remain in the child
transcript and future ATIF trajectory but are not concatenated into the parent
context.

### Tests

- Table-driven projection tests cover every status and every point at which a
  tool call can be interrupted.
- Property tests assert that projected conversations never contain an orphaned
  tool call or duplicate result.
- A future prompt after a failed or cancelled run sees all committed visible
  work plus the runtime notice.
- Crash injection around child creation produces either no child or one
  claimable owned child, never an orphan.
- Parent cancellation before claim, during provider work, during a tool, and
  after child completion has deterministic outcomes.
- Final-answer selection ignores intermediate child assistant messages.

### Acceptance

- Future model context is a valid projection of durable visible history for
  every terminal state.
- Restart does not change the projection.
- Child ownership has no crash window.
- No recovered side effect is re-executed.

## Phase 3: Task-Completion Contract And Workspace Instructions

Priority: P0 for capability. Keep this generic and benchmark-independent.

### Workspace Instructions

Add a bounded instruction loader in the root/core composition path:

- Load a root `AGENTS.md` when present.
- Instruct the model to discover and obey more specific nested instruction
  files before changing files below them.
- Resolve every instruction path through the workspace capability.
- Reject symlink escape and bound individual and aggregate instruction bytes.
- Preserve deterministic root-to-leaf precedence.
- Include the instruction content in the stable system-prefix region.
- Persist the prompt version and instruction hash on the run.

Do not silently interpret project instructions in the TUI or Harbor adapter;
the shared core runtime owns the behavior.

### Completion Contract

Bump the versioned base prompt with concise, provider-neutral requirements:

- Determine observable completion criteria from the user's request.
- Inspect before changing and preserve unrelated work.
- Implement requested changes rather than stopping at analysis unless the user
  requested analysis only.
- Treat failed tools and tests as evidence, diagnose them, and continue when a
  safe path remains.
- Run the narrowest relevant verification before broader checks.
- Do not claim success without evidence from the resulting state.
- Report remaining failures and uncertainty honestly.
- Respect explicit time, token, cost, and safety budgets.

Do not add a planner DSL, hidden scratch database, or task-specific prompt
rules. First improve the prompt and measure it.

### Stall Observation

Record, but initially do not automatically interfere with:

- Repeated identical tool calls.
- Repeated command failures with unchanged arguments.
- Consecutive model turns without filesystem or process-state change.
- Verification never attempted after a mutation.
- Final answer immediately after a failed mutation.

Only add a model-visible recovery notice or forced final-verification state if
trajectory analysis shows a repeated generic failure and a paired experiment
demonstrates improvement.

### Tests And Evaluation

- Instruction precedence, bounds, containment, and hashing have deterministic
  tests.
- Prompt contract fixtures assert presence and provider mapping, not prose
  formatting beyond the versioned contract.
- Analysis-only requests are not forced into edits.
- Mutation tasks with a fake provider demonstrate a verification turn before
  completion.
- Run paired prompt-version evaluations with the same model, effort, tasks,
  and seeds.

Track:

- Premature final answers.
- Modified-but-never-verified runs.
- Repeated identical tool calls.
- Pass rate and cost change.

Target premature completion and unverified mutation below 2% without raising
median cost faster than successful-task rate improves.

## Phase 4: Linear And Fair Durable Streaming

Priority: P0 for long-output correctness and performance.

### Baseline First

Add deterministic benchmarks before changing storage:

- One long assistant message at 64 KiB, 1 MiB, 4 MiB, and the configured cap.
- One long provider-exposed reasoning stream.
- One long shell stream.
- Eight concurrent streams plus snapshot and cancellation control traffic.
- Replay and snapshot reconstruction after restart.

Record transaction count, bytes copied when measurable, wall time, peak
temporary memory, and maximum queue wait. The benchmark must fail or clearly
regress when doubling output becomes quadratic.

### Append-Only Text Chunks

Replace repeated SQLite string concatenation during streaming with an
append-only message-chunk representation:

- Add `message_chunks(message_id, channel, chunk_ordinal, text)`.
- Keep legacy `messages.output` and `messages.refusal` columns readable.
- New streaming writes append chunks; they do not rewrite the accumulated
  message.
- Snapshot and context readers concatenate legacy base text plus ordered
  chunks once per requested message.
- Message completion changes state atomically with the completed model turn.
- Deleting or pruning a session cascades through chunks.
- Migration is additive and idempotent; no old transcript is rewritten.

If measurement finds an equivalent simpler implementation with the same
linear bound, document the evidence in the implementation PR before changing
this schema decision.

### Incremental Capacity Accounting

Do not reconstruct full session context for every chunk.

- Compute the pruned assembled base once when a run is claimed.
- Persist that base and the run's incremental contribution.
- Each output, argument, or result transaction checks and increments the
  contribution atomically.
- Recompute the assembled base at the next claim, compaction commit, or other
  operation that changes the cutoff/pruning view.
- Reject an overflow before publishing the chunk that caused it.
- Add a consistency audit that compares the incremental value with a full
  reconstruction in tests and optional debug tooling.

The incremental value is an optimization, never the source of transcript
truth.

### Reasoning Batching

Apply the same bounded-delay and bounded-byte policy used for text to
provider-exposed reasoning:

- First reasoning event publishes promptly.
- Subsequent deltas batch by bytes or delay.
- Start, delta, and completion ordering remains durable.
- Cancellation flushes the last bounded batch before settling the run when
  persistence remains available.

### Store Fairness And Shutdown

- Replace permanently biased control selection with measured weighted
  fairness: control stays responsive, but pending output receives service
  within the starvation SLO.
- Replace one-millisecond async polling for output admission with a wake-driven
  bounded mechanism.
- Group compatible adjacent output operations into one transaction without
  crossing sessions or changing event order.
- Provide an explicit asynchronous close path. `Drop` may signal shutdown but
  must not synchronously join the database thread from a Tokio worker.

### Tests And Acceptance

- Legacy and migrated stores produce identical snapshots and context.
- Chunk ordering survives restart and concurrent output.
- Capacity accounting never undercounts and rejects the exact overflowing
  append.
- Failure injection before and after chunk, turn, and event commits preserves
  persist-before-publish.
- Reasoning batching reduces transactions while preserving replay.
- Cancellation and control requests meet their SLO during eight long streams.
- Doubling stream size remains within the 2.2x linearity budget.

## Phase 5: Resolved Model, Context Budget, And Spend Enforcement

Priority: P0 for context correctness, P1 for cost optimization.

### Resolved Model Contract

Extend the `RuntimeLoader` result so one immutable resolved-model value travels
with the runtime:

```text
effective route and model
effective maximum output tokens
context-window tokens, when known
pricing and provenance
implemented generation controls
implemented prompt-cache capabilities
```

The root composition layer constructs it from effective configuration and the
model catalog. `qq-core` consumes provider-neutral fields only; provider
identity must not branch in the request hot path.

Persist on every run:

- Effective route/model.
- Output and context limits.
- Generation settings such as reasoning effort when implemented.
- Pricing provenance.
- Prompt and tool-schema versions.

Historical rows use unknown defaults rather than current configuration.

### Context Planning

The context module returns one of:

- `Send`: an assembled request with measured occupancy and output reserve.
- `Compact`: a reason and target budget.
- `Reject`: an actionable error when even the irreducible context cannot fit.

Inputs include:

- Model context window.
- Last provider-reported context occupancy.
- Conservative preflight estimate for newly added content.
- System instructions and workspace instructions.
- Tool declaration weight.
- Output-token reserve.
- Latest compaction and pruning policy.

Do not claim tokenizer exactness where a provider does not supply it. Prefer
actual reported usage after each turn and a conservative safety margin before
the next request.

Automatic compaction triggers when the context plan cannot safely reserve the
next output, not merely at 70% of 4 MiB. Keep the existing byte cap as a
storage/resource backstop, separate from model-window policy.

### Compaction Hardening

Complete and extend `docs/plans/compaction.md`:

- Implement `search_history` over the full durable transcript with bounded,
  cited excerpts.
- Validate required summary section headings.
- Reject empty summaries and summaries that do not produce measured shrinkage.
- Retain the prior usable compaction when validation fails.
- Test preservation of seeded user constraints, exact paths, decisions,
  unresolved errors, and verification status through repeated compactions.
- Keep bounded compaction history and expose rollback before experimenting with
  cheaper summarizers.

A cheaper compaction model is an optimization experiment, not a default.
Promote it only when retention tests and end-to-end task success remain
statistically neutral.

### Enforced Budgets

Support per-run:

- Wall-clock deadline.
- Model-turn limit.
- Tool-call limit.
- Token limit.
- Dollar limit when pricing and usage are known.

`d989cf8` closes one headless failure mode by observing the provider-request
boundary before a silent over-budget turn can hang. `DEV-725` makes the
runtime's internal 256-call backstop renewable: it persists a tool-free
checkpoint and continues the same run instead of emitting ordinary
`Completed`. The remaining contract is core-owned configurable budgets, so
every front end can impose and observe the same explicit overall outcome.

Before each new model turn, reserve enough budget for a bounded final response.
When the work budget is exhausted:

- Request one final status response only when the reserve remains.
- Otherwise settle with an explicit `budget_exhausted` outcome.
- Never label budget exhaustion as provider failure.

If a hard dollar limit is requested but cost cannot be measured, reject the
configuration before the run rather than pretend to enforce it. If a provider
with configured pricing later omits turn usage, stop with an explicit
`budget_exhausted` result instead of silently continuing under an unknown cost.

### Generation Controls And Prompt Caching

Add provider-neutral generation controls only after a real model pair needs
them. Begin with reasoning effort because Terminal-Bench reports it and
multiple providers expose an equivalent concept.

For prompt caching:

- Keep system instructions and tool schemas stable and deterministically
  ordered.
- Map cache controls inside provider codecs.
- Record fresh, cache-read, and cache-write tokens.
- Do not expose provider-specific cache markers to `qq-core`.

### Tests And Acceptance

- Small, medium, and large context-window fake models trigger different plans
  for the same transcript.
- Tool schemas and output reserve are included in the budget.
- No stress test sends a known-overflow request.
- Repeated compaction preserves required facts and shrinks at least 8x.
- Hard budgets settle deterministically and account all completed turns.
- Runs retain their original resolved model after configuration changes.
- Supported providers reach the cache-ratio target on stable multi-turn
  fixtures without request-shape regressions.

## Phase 6: Tool-Contract Tournament And Terminal Sessions

Priority: P1. Use benchmark evidence; do not ship every candidate.

### Experiment Method

For each candidate tool contract:

1. Add deterministic adversarial fixtures.
2. Run paired agent evaluations with the same model and prompt.
3. Measure pass rate, tool turns, failed calls, input tokens, cost, and time.
4. Keep the candidate only if it improves capability or materially improves
   efficiency without a capability loss.
5. Remove experimental code and schemas for rejected variants.

### Search Candidate

Compare the current search with an ignore-aware implementation supporting:

- File-name and content modes.
- Literal and regular-expression matching.
- Glob/path filters.
- Deterministic path and line ordering.
- Bounded context lines, result count, file count, and bytes.
- Explicit truncation and continuation information.
- `.gitignore` and common generated-directory handling.

Acceptance target:

- Complete recall on adversarial fixtures within declared bounds.
- At least 25% fewer discovery tool calls on repository-navigation tasks.
- No unbounded index or cache.

### Patch Candidate

Compare exact replacement with a validated patch operation:

- Workspace capability containment.
- Prior-read and stale-hash checks.
- Exact hunk matching with actionable rejects.
- Atomic temp-file-and-rename application.
- Permission preservation.
- Bounded patch and output sizes.
- Persisted UI diff separate from compact model-facing output.

Keep exact replacement even if patch wins; they solve different concrete
cases. Do not add a generic editor registry.

### Persistent Terminal Candidate

Keep `shell` as the fast one-shot path. Add one `terminal` tool interface only
if stdin, background-service, or interactive-task failures are present in
trajectories.

The proposed actions are:

```text
start(command, cwd, terminal_mode, timeout)
poll(process_id, output_cursor, wait)
write(process_id, bytes, eof)
stop(process_id)
```

The terminal supervisor implementation must:

- Bind process ownership to a run/session.
- Bound active processes, output bytes, poll wait, input bytes, and lifetime.
- Use output cursors and a bounded ring or spool so the model does not resend
  all prior output on every poll.
- Return exit status and elapsed time.
- Kill the entire process group on stop, cancellation, timeout, crash
  recovery, or owner deletion.
- Prevent one process from being controlled by another session.
- Use pipes first when sufficient; add PTY support only when the ablation
  proves that terminal emulation changes task outcomes.
- Keep blocking work off Tokio workers.

Protocol events should reuse ordinary tool activity. A persistent process is a
tool implementation detail until its lifecycle must be displayed separately.

### Acceptance

- Internal tool-contract failures stay below 1%.
- Tool changes reduce median discovery/edit turns by at least 25% or produce a
  statistically meaningful task-success gain.
- No tool introduces unbounded state or weakens workspace containment.
- Terminal cancellation leaves no descendants.

## Phase 7: Sub-Agent Economics And Provider-Aware Scheduling

Priority: P1 after Phase 2 and Phase 5.

Complete the pending work in `docs/plans/subagents.md`:

- Add a configured worker-model selection with parent fallback.
- Persist the child's resolved model independently.
- Roll child usage and cost into parent totals without double-counting the
  child session display.
- Include child trajectories in headless and ATIF output.
- Record child queue wait and concurrency.
- Keep depth one and current read-only semantics.

Evaluate delegation by task shape:

- Breadth-shaped repository research.
- Several independent questions.
- Depth-shaped single-file work that should remain inline.
- Tasks where worker-model errors force expensive parent recovery.

The promotion gate is at least 20% lower dollars-per-pass on the task class
with no statistically meaningful pass-rate loss. Disable delegation by
default for classes where the ablation loses.

Add provider-aware scheduling only after telemetry shows pressure:

- Per-provider concurrent request bounds.
- Rate and token budgets.
- Fair root/child scheduling.
- Retry-attempt visibility.
- Cancellation of queued child work.

Do not add editing sub-agents, automatic swarms, or worktree orchestration in
this plan.

## Phase 8: Warm-Path Runtime And Request Efficiency

Priority: P2. Start only after end-to-end measurements identify the cost.

Candidate work:

- Cache effective configuration and compiled runtimes by validated generation
  or digest so each claimed run does not repeat unchanged resolution work.
- Cache valid credential leases in memory with expiry and single-flight
  refresh; never log or persist secret material in cache keys.
- Invalidate runtimes when configuration, credentials, MCP declarations, or
  organization selection changes.
- Share immutable message/tool storage where ownership is genuinely shared.
- Bound total MCP tool count and schema bytes.
- Select only concretely relevant tools when a measured use case justifies
  selection; do not introduce a generic tool router in advance.
- Keep serialized stable prefixes deterministic for provider caching.
- Benchmark realistic request encoding and temporary allocation.
- Expose retries and distinguish safe pre-stream retries from ambiguous
  failures that could duplicate spend. Use provider idempotency support where
  available.

Acceptance requires end-to-end improvement. Provider-recipe nanoseconds alone
do not justify complexity.

## Benchmark Program

### Evaluation Layers

1. **Per-PR deterministic tests**
   - Fake providers, temporary workspaces, temporary SQLite, crash injection,
     context projection, tool contracts, and performance regression fixtures.
2. **Private shadow suite**
   - Generic tasks covering repository discovery, editing, build/test repair,
     background processes, interactive input, data transformation, recovery,
     and long context.
   - Tasks must not copy public benchmark solutions or encode task-specific
     hints.
3. **Terminal-Bench development runs**
   - Small infrastructure smoke selection while the adapter stabilizes.
   - Full current dataset with one repetition at implementation milestones.
4. **Submission runs**
   - The exact official dataset command, resources, timeouts, and required
     repetition count.
   - Current Terminal-Bench 2.1 submissions use `k=5`; verify the official
     contract again before every submission.

### Comparison Discipline

- Compare QQ and another harness with the same model, reasoning effort,
  provider route, task timeout, resources, and machine class.
- Use paired per-task results and report confidence intervals.
- Separate model changes from harness changes.
- Run at least three seeds for internal comparisons when the benchmark
  contract does not already require more.
- Store the QQ revision, prompt version, tool hash, and full configuration with
  every result.
- Never choose only the tasks improved by a change.

### Failure Taxonomy

Every failed trial receives one primary category:

- Task misunderstanding.
- Workspace/instruction discovery.
- Missing or irrelevant evidence.
- Tool contract or tool misuse.
- Incorrect mutation.
- Dependency/environment failure.
- Verification omitted.
- Verification failed and recovery stopped.
- Repeated-work/stall loop.
- Context loss or compaction loss.
- Provider/authentication/rate failure.
- Timeout or budget exhaustion.
- Persistence/replay/harness failure.
- Benchmark infrastructure or invalid task.

Retain the trajectory link and supporting event/tool identifiers. Do not infer
the category from final text alone.

### Integrity

- Do not alter benchmark task timeouts or resources.
- Do not provide task-specific instructions, encrypted solutions, test
  contents unavailable to other agents, or internet-retrieved solutions.
- Use the same generic QQ prompt and tool contracts across the dataset.
- Produce ATIF for every passing trial and run the published trajectory judge
  when available.
- Keep benchmark adapters auditable and separate from task execution logic.

Official references:

- [Terminal-Bench 2.1 leaderboard](https://www.tbench.ai/leaderboard/terminal-bench/2.1)
- [Terminal-Bench 2.1 release](https://www.tbench.ai/news/terminal-bench-2-1)
- [Harbor custom-agent documentation](https://www.harborframework.com/docs/agents)
- [ATIF documentation](https://www.harborframework.com/docs/agents/trajectory-format)
- [Terminal-Bench leaderboard integrity policy](https://www.tbench.ai/news/leaderboard-integrity-update)

## Delivery Sequence

Each row should be one narrow issue and normally one focused PR.

| Order | Proposed change | Depends on | Milestone |
| ---: | --- | --- | --- |
| 1 | `feat(cli): add durable autonomous run` | none | Benchmarkable |
| 2 | `test(eval): add Harbor adapter and ATIF validation` | 1 | Benchmarkable |
| 3 | `fix(runtime): project terminal runs into valid follow-up context` | committed-turn replay | Correct continuation |
| 4 | `fix(runtime): make child spawn atomic and final-only` | 3 | Correct continuation |
| 5 | `feat(runtime): load scoped workspace instructions` | 1 | Completion quality |
| 6 | `test(eval): baseline completion prompt and failure taxonomy` | 2, 5 | First trustworthy baseline |
| 7 | `perf(session): make streamed text persistence linear` | 1 | Long-stream performance |
| 8 | `perf(session): batch reasoning and fairly schedule persistence` | 7 | Long-stream performance |
| 9 | `feat(runtime): carry and persist resolved model limits` | 3 | Model-aware runtime |
| 10 | `feat(runtime): plan context and enforce run budgets` | 8, 9 | Model-aware runtime |
| 11 | `feat(runtime): validate compaction and add history recall` | 10 | Durable long sessions |
| 12 | `perf(provider): add measured generation and cache controls` | 9, 10 | Cost frontier |
| 13 | `test(tools): run search edit and terminal contract ablations` | 2, 6 | Tool evidence |
| 14 | `feat(tools): ship winning tool contracts` | 13 | Tool capability |
| 15 | `perf(runtime): configure worker models and roll up child cost` | 4, 9 | Sub-agent economics |
| 16 | `perf(runtime): cache resolved runtimes and credential leases` | 6 | Warm path |
| 17 | `perf(runtime): add provider-aware scheduling from trace evidence` | 15, 16 | Concurrent efficiency |
| 18 | `test(eval): run full fixed-model and efficiency qualification` | all promoted work | Release candidate |

Orders 3 and 5 may proceed while the Harbor adapter is being built, and tool
fixtures may be prepared while storage work proceeds. Do not let multiple
writing agents edit the same checkout concurrently; use isolated worktrees and
integrate reviewed patches.

## Verification Gates

Every behavior change:

- Adds a regression test against public behavior and failure modes.
- Runs the narrowest relevant crate tests while iterating.
- Preserves protocol decoding for historical events.
- Preserves persist-before-publish and cancellation.
- Records performance impact when it touches a listed hot path.

Before merging a phase:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

Additional gates:

- Provider compilation or request hot path:
  `cargo bench -p qq-provider --bench provider_compiler`.
- Tool-loop changes:
  `cargo bench -p qq-core --bench tool_dispatch` plus the new end-to-end
  fixtures.
- TUI event/projection changes: long-output rendering tests and the manual
  rendering benchmark in release mode.
- Database migrations: open current, legacy, partially migrated, and corrupted
  fixture stores.
- Harbor changes: local smoke task, ATIF validation, then a bounded dataset
  sample.

No full public benchmark run is required for a change that cannot affect agent
behavior, but the reason must be recorded.

## Risks And Mitigations

### Unattended Shell On A Real Machine

Risk: benchmark authority is accidentally used in an ordinary workspace.

Mitigation: require explicit `auto` policy, explain it in CLI help, scope it to
one canonical workspace/session, retain all tool events, and make the Harbor
adapter select it only inside the benchmark environment.

### Benchmark Overfitting

Risk: public-task tuning improves the leaderboard but weakens the product or
violates submission integrity.

Mitigation: use generic prompt/tool changes, a private shadow suite, full-set
reporting, paired evaluation, ATIF review, and the published trajectory judge.

### Context Counter Drift

Risk: incremental accounting undercounts and sends an oversized request.

Mitigation: update it transactionally, use conservative arithmetic, compare it
with full reconstruction in tests, and retain the byte cap as a backstop.

### Migration Complexity

Risk: chunk storage or model metadata makes historical sessions unreadable.

Mitigation: additive idempotent migrations, legacy read paths, fixture stores
for every version, and no transcript rewrite.

### PTY Portability

Risk: terminal emulation adds OS-specific complexity and background leaks.

Mitigation: ship only after task evidence, start with pipes, keep the process
supervisor interface independent of its implementation, and test Unix process
groups plus explicit unsupported-platform behavior.

### Model Routing Increases Cost

Risk: cheap workers make mistakes that force expensive recovery, or parallel
models multiply spend.

Mitigation: fixed-model baseline first, sequential escalation, cost roll-up,
task-class ablations, and no default parallel ensemble.

### Performance Refactors Weaken Durability

Risk: batching and caching present uncommitted data as authoritative.

Mitigation: persist before publish remains invariant; grouped commits never
cross ordering guarantees; failure injection verifies every commit edge.

## Non-Goals

- Remote instance discovery, multi-machine workspace selection, or mobile UI.
- Distributed scheduling or hosted coordination.
- Editing sub-agents or automatic agent swarms.
- Run snapshots or generalized undo beyond the separate
  `docs/plans/run-snapshots.md`.
- A plugin interface, public tool registry, or alternate protocol.
- Replacing SQLite without measurements that show it remains the bottleneck
  after linearization.
- Model-specific branches in the core agent loop.
- Mid-stream provider replay without a provider-supported idempotent contract.
- Benchmark-specific prompts, task heuristics, or resource changes.
- Broad TUI restructuring unrelated to durable event projection.

The HTTP/SSE protocol, durable store, and shared runtime remain the foundation
for future remote and mobile clients. This plan strengthens that foundation;
it does not pre-build those clients.

## Overall Definition Of Done

The roadmap is complete when:

- The official benchmark invokes `qq run` through the durable core runtime.
- All passing trials produce validated ATIF and all trials produce QQ traces.
- A crash/restart conformance suite proves authoritative session continuation.
- Long text, reasoning, and shell streams scale linearly and meet queue SLOs.
- Context planning is model-aware, compaction is validated and recallable, and
  known overflows never reach providers.
- Prompt and workspace-instruction behavior have paired evaluation evidence.
- Shipped search, editing, and terminal contracts have won their ablations.
- Sub-agent cost is complete, visible, bounded, and beneficial on its enabled
  task classes.
- Fixed-model QQ results are within one percentage point of the best comparable
  harness, followed by a capability lead or a 20-25% successful-task cost/time
  advantage.
- Workspace tests, formatting, Clippy, build, migrations, benchmark smoke, and
  relevant performance gates pass.
- Documentation reflects the final interfaces and removed experiments are
  deleted rather than left as dormant extension points.
