# Speed-First Extensible Agent Harness Backend

Status: Phase 0 complete 2026-09-01. Phase 1 complete 2026-09-02: R4 qualified
2026-09-01, R5 qualified 2026-09-02, H1 feature profiles landed and measured.
Implementation Phases 2–7 and tasks H2–H12 remain proposed.

This plan defines how QQ becomes an extremely fast, lightweight, customizable
agent harness that can serve as the backend for products such as a
Hermes-style personal agent. It is intentionally a backend plan, not a plan to
copy every product surface from Codex, OpenCode, Pi, fx, or Hermes into QQ.

The central decision is:

> Compile customization once, execute directly in the hot path, persist before
> publishing, and keep every queue and concurrency boundary explicit.

QQ should become a small durable execution kernel with a compiled
customization plane. Messaging gateways, cron, voice, browser automation,
product identity, and user-facing memory products remain clients of that
kernel.

## Decision Summary

QQ already has the strongest combined backend foundation of the audited
projects:

- one Rust runtime shared by direct CLI, TUI, durable headless, and server
  paths;
- a provider-neutral request and streaming seam;
- authoritative SQLite events and persist-before-publish ordering;
- idempotent commands and cursor-based HTTP/SSE replay;
- bounded tools with capability-based filesystem containment;
- approval, cancellation, recovery, and cost-accounting semantics;
- MCP-based external tools; and
- durable, bounded read-only sub-agent sessions.

The next priorities, in order, are:

1. Measure and repair the persistence, context, and scheduling hot paths.
2. Introduce one immutable, content-addressed `CompiledAgentPlan`.
3. Complete the backend protocol needed by external agent products.
4. Add narrow extension lanes for agent packs, providers, tools, context, and
   observers.
5. Add durable terminal control and a real process-sandbox adapter when
   evaluation justifies them.
6. Add optional product-facing protocol adapters only in response to real
   consumers.

Do not start with a universal plugin API. It would put discovery, dynamic
dispatch, trust, lifecycle, and failure handling in the most latency-sensitive
part of the system before QQ has measured its baseline.

## Status And Authority

This document is a proposed companion to the current architecture and plans.
It does not silently override them.

- [`docs/design/architecture.md`](../design/architecture.md) remains the system
  boundary and dependency-direction source of truth.
- [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) owns linear
  streaming, reasoning batching, store fairness, resolved-model context
  planning, tool-contract ablations, terminal qualification, sub-agent
  economics, and warm-runtime work.
- [`terminal-bench-baseline-repair.md`](./terminal-bench-baseline-repair.md)
  owns the focused qualifying-baseline repair tranche.
- [`compaction.md`](./compaction.md) owns transcript pruning, summary mechanics,
  and the pending history-search work.
- [`subagents.md`](./subagents.md) owns current read-only delegation, worker
  model selection, child accounting, and child admission.
- [`run-snapshots.md`](./run-snapshots.md) owns reversible mutating-run state.
- [`lsp-diagnostics.md`](./lsp-diagnostics.md) owns diagnostics integration and
  its MCP-first validation path.
- [`core-runtime-rearchitecture.md`](./core-runtime-rearchitecture.md) owns the
  physical `qq-core` extraction sequence and reasoning-effort contract.

Where this plan depends on one of those contracts, implementation should land
through the owning plan and this document should record the dependency rather
than duplicate the design.

The architecture currently defers a public extension interface, a plugin
marketplace, JavaScript packages, multi-user tenancy, and distributed workers.
Later phases in this plan may cross those boundaries only after their entry
gates are met and the architecture document or an ADR records the decision.
This plan is not a license to pre-build the deferred phases.

## Goals

### Product Goal

QQ should maximize verified successful agent work per dollar, per minute, and
per unit of local resource use while remaining pleasant to embed and extend.
Correctness, durability, and safe execution are baseline constraints rather
than tradeable performance features.

An application developer should be able to:

1. create or resume a durable QQ session;
2. select a versioned agent profile;
3. submit an idempotent multimodal command with explicit resource limits;
4. receive durable progress through a reconnectable event stream;
5. respond to approvals or steer an active run;
6. add tools through MCP or a trusted embedded host;
7. add instructions and skills through declarative agent packs;
8. add a provider without changing the agent loop;
9. add memory retrieval without intercepting token streaming; and
10. recover from client or process loss without repeating uncertain side
    effects.

### Performance Goal

The default path should pay only for the behavior it uses:

- configuration, discovery, trust, schema preparation, and provider selection
  happen before the run hot path;
- disabled adapter families do not add shipping dependencies to minimal
  embedders;
- active runs use immutable shared plans and direct dispatch;
- observers consume committed events asynchronously;
- queues, buffers, retries, tasks, subprocesses, and fan-out are bounded; and
- every material optimization is supported by an end-to-end measurement.

### Customization Goal

Customization should be ergonomic without collapsing unlike concerns behind
one shallow interface. QQ will expose a small set of deep extension lanes with
different trust and performance contracts:

- declarative agent packs;
- compiled provider adapters;
- static native tools;
- external or embedded tool hosts;
- bounded context sources;
- post-commit event observers; and
- surface adapters using the versioned client protocol.

## Non-Goals

This plan does not authorize:

- a universal `Plugin` trait through which every token, event, and tool call
  passes;
- a dynamic Rust shared-library ABI;
- a plugin marketplace before addon packaging and trust have two real
  consumers;
- one crate per provider, tool, storage backend, or integration;
- a second agent runtime for embedding;
- a JavaScript or Python runtime inside `qq-core`;
- messaging, cron, voice, browser automation, or product-specific memory in
  `qq-core`;
- a distributed scheduler, hosted control plane, or general multi-tenant IAM;
- editing sub-agents before snapshots, isolation, and conflict semantics are
  implemented;
- WebSocket, gRPC, GraphQL, or a binary wire protocol without measurement or a
  feature that HTTP/SSE cannot express;
- unbounded queues, tasks, listeners, retries, output, or concurrency; or
- performance claims based only on binary size, source line count, or an
  isolated microbenchmark.

## Audit Method

The design is based on independent, read-only source audits of four ignored
local reference snapshots under `.source/` plus the current QQ implementation
and plans.

| Project | Inspected identity | Files | Snapshot manifest SHA-256 | Evidence boundary |
| --- | --- | ---: | --- | --- |
| Codex | workspace and SDK versions `0.0.0` / `0.0.0-dev` | 6,883 | `4f780e9d53ea4ef0c5f20ce307e9b89005b4515e622a22b0d08e9bb7c4b82f17` | No nested Git metadata; 141 Rust workspace member paths |
| OpenCode | version `1.18.25` | 6,543 | `4fbf5422a7bc33150d6d79bc70afcc9950d6ff1e730ee95bda8bd68ed935a39f` | Bun monorepo with current V1 and incomplete experimental V2 runtimes |
| Pi | coding-agent version `0.84.4` | 1,410 | `d5a87ac144bf16d1c8cfecac60f5cf3d9c40479da0e057e84452070b1421dc4e` | TypeScript monorepo; shipped coding runtime differs from newer durable-harness work |
| fx | runtime version `0.0.7` | 800 | `88732147452e5aa7164ed11bdef218bfcbe1f3020e1339a1e2667e03a4c7aa84` | Experimental Zig runtime; manifests contain placeholder versions |

The snapshots have no nested Git repositories, so QQ's enclosing commit must
not be attributed to them. The audit was static: their build, startup, binary
size, memory, and benchmark claims were not independently executed. The local
`.source/` directory remains ignored and is not a repository dependency.

The manifest hashes above identify the exact audited local trees. They hash
the sorted sequence of each regular file's SHA-256 and relative path; they do
not include empty directories or file modes. They detect snapshot drift but do
not replace missing upstream revision provenance or make the ignored sources a
shipping dependency.

Key evidence anchors retained for a future re-audit are:

| Project | Local snapshot anchors |
| --- | --- |
| Codex | `codex-rs/core/src/session/turn.rs`, `codex-rs/thread-store/src/store.rs`, `codex-rs/core/src/session/mod.rs`, `codex-rs/sandboxing/src/manager.rs`, `codex-rs/hooks/src/lib.rs`, `codex-rs/app-server/README.md` |
| OpenCode | `packages/core/src/event.ts`, `packages/core/src/session/input.ts`, `packages/sdk-next/src/opencode.ts`, `packages/plugin/src/index.ts`, `packages/opencode/src/server/server.ts`, `SECURITY.md` |
| Pi | `packages/agent/README.md`, `packages/agent/docs/harness.md`, `packages/agent/src/harness/agent-harness.ts`, `packages/coding-agent/src/core/session-manager.ts`, `packages/coding-agent/docs/extensions.md`, `packages/protocol/README.md` |
| fx | `src/core/agent/stream_provider.zig`, `src/core/session/session_log.zig`, `src/core/subagent/domain.zig`, `src/builtins/tools.zig`, `src/core/permissions/permissions.zig`, `sdk/README.md` |

The comparison records implemented code separately from experimental,
incomplete, or marketing-only behavior. Removed compatibility flags and
unimplemented roadmap claims are not counted as available features.

## Current QQ Baseline

QQ currently provides:

- one binary with TUI, `ask`, durable `run`, `serve`, configuration,
  authentication, organization, and workspace-trust commands;
- OpenAI Responses, OpenAI Codex subscription, Anthropic Messages, Google
  GenerateContent, xAI Responses and Chat, Bedrock ConverseStream, and Mantle
  Responses, Chat, and Anthropic protocol support;
- LiteLLM and compatible custom deployment recipes using supported protocols;
- model catalogs with pricing, context, output, reasoning, and cache metadata;
- built-in `read_file`, `list_dir`, `search`, `edit_file`, `write_file`, and
  one-shot `shell` tools, plus conditional `spawn_agent` and namespaced MCP
  tools;
- capability-scoped filesystem access, no-follow path handling, read-before-
  write hashes, atomic replacement, bounded output, process-group cleanup, and
  cancellation;
- read-only, ask, automatic, and full approval modes with scoped grants,
  managed denies, previews, and model-reviewed held tools;
- MCP over stdio and Streamable HTTP with trust, authorization, cached
  connections, schema caching, list-change handling, backoff, deadlines, and
  bounds;
- SQLite WAL persistence through a dedicated worker, idempotent commands,
  authoritative events, replay cursors, snapshots, recovery, cancellation,
  deletion, pruning, and compaction;
- automatic and manual context compaction and stale read-only result pruning;
- renewable internal 256-tool execution slices with durable checkpoints;
- durable read-only child sessions with atomic creation, parent ownership,
  depth and concurrency caps, cancellation, recovery, worker-model selection,
  and cost roll-up;
- repository-root instructions and explicit repository-local commands and
  skills with persisted content hashes;
- an HTTP/SSE server, authenticated client, TUI session management, live
  approvals, model selection, cost/context display, and reconnect/replay; and
- durable JSONL traces, Harbor/ATIF export support, provider canaries, provider
  compilation benchmarks, a synthetic tool-loop benchmark, and manual TUI
  performance cases.

The critical gaps already recorded in the active readiness plan are:

| Area | Current behavior | Consequence |
| --- | --- | --- |
| Streaming persistence | Text batches reconstruct context and grow stored strings by concatenation | Structurally superlinear long-output work |
| Reasoning persistence | Deltas commit independently | Transaction and queue pressure for reasoning-heavy models |
| Store scheduling | Control work is always preferred and full output queues poll | Output can be delayed or starved |
| Context planning | Fixed byte budget and trigger | Incorrect behavior across small and large context windows |
| Resolved model state | Runtime loading drops effective limits/capabilities | Core cannot reproduce or budget the actual execution |
| Run limits | Core-owned `RunLimits` and typed `budget_exhausted` outcome shipped in R5 | Clients other than `qq run` do not yet surface the limits they may impose |
| Terminal | `shell` is one-shot without stdin or a durable handle | Interactive and long-running processes are awkward |
| Search/edit | Literal scan and exact replacement | Additional model turns and I/O on large repositories |
| Retry ownership | Provider and core retries can amplify | Duplicate spend and unclear delivery certainty |
| Evaluation | No complete useful-result latency gate | Speed claims are not yet end-to-end |

One additional footprint issue matters for embedders: `qq-provider` currently
depends unconditionally on the AWS SDK family, and `qq-core` depends on
`qq-provider`. A consumer that needs only a lightweight HTTP provider therefore
inherits the heavy adapter graph. The fix is feature-gated adapter families
inside the existing provider crate, not a provider-per-crate redesign.

## Cross-Project Feature Inventory

`Yes` means the capability is substantiated in the inspected implementation.
`Partial` means experimental, incomplete, unsafe for backend use, or present
only through an example or alternate runtime. `No` means it was not found as a
first-class capability.

### Runtime And Durable State

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Shared core across interfaces | Yes: direct, TUI, server, durable headless | Yes | Partial: V1 and incomplete V2 | Partial: shipped loop and incomplete new harness | Yes: CLI, TUI, ACP, SDK |
| Streaming text, reasoning, and tools | Yes | Yes | Yes | Yes | Yes |
| Cancellation | Yes | Yes | Yes | Yes | Yes |
| Active-run steering | Partial: queue or cancel | Yes | Partial | Yes | Queued at model boundary |
| Structured output | No first-class contract | Yes | Yes | Provider-dependent | Yes |
| Multimodal input | Text-only protocol | Images and media | Files and images | Images and vision | Native vision; SDK lacks images |
| Bounded provider retries | Pre-stream only | Yes | Yes | Yes | Yes, with delivery certainty |
| Durable sessions | SQLite authority | JSONL plus SQLite projection | SQLite; stronger V2 event store | Shipped synchronous JSONL | Checksummed framed event log |
| Persist-before-publish | Yes, fail-closed | Attempted; append errors can be swallowed | Partial: implemented only in experimental V2 | No strong invariant | Strong local-log semantics |
| Idempotent command admission | Yes | Limited | Partial: implemented only in experimental V2 | No | No inbox equivalent |
| Cursor replay | Yes, SSE cursors | Subscription and replay | V2 event replay | Branching history | Tape and session recovery |
| Session lifecycle | Create, resume, delete, prune, compact | Resume, fork, archive, rollback, delete | Resume, fork, revert, import/export | Resume, fork, clone, labels, export | Resume, migrate, recover, undo |
| Compaction | Automatic/manual and stale-result pruning | Local and remote summaries | Summary, pruning, context epochs | Compaction and branch summaries | Fast deterministic extractive summary |
| Crash-safe long-run checkpointing | Renewable internal slices | No strong execution checkpoint | V2 explicitly incomplete | No | Recovery checkpoints and `/continue` |
| Sub-agents | Durable bounded read-only children | Full-session tree with caps | Foreground/background tasks | Example child CLI processes | Durable persistent children |
| Durable child recovery | Yes | Partial | No for background jobs | No | Yes |

### Providers, Tools, Extensions, And Security

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Provider coverage | OpenAI, Anthropic, Google, xAI, Bedrock, compatible gateways | Responses-compatible, Bedrock, Ollama, LM Studio | More than 20 providers | Roughly 30-provider catalog | Gateway, Codex, Grok |
| Existing-protocol custom deployments | Yes | Responses-compatible only | Yes | Yes | No generic endpoint registration |
| New provider protocol seam | Rust `Provider` boundary | Compile-time/core work | Core work for new wire protocol | Strong SDK boundary | Fixed enum/set and core edits |
| Built-in coding tools | Read, list, search, edit, write, shell | Broad coding, media, web, and agent set | Broad coding, web, LSP, and task set | Read, shell, PowerShell, edit, write, grep, find, list | Sixteen tools including terminal, web, vision, skills, MCP, sub-agent |
| Parallel tools | Read-only groups bounded; mutations serial | Parallel; no obvious per-turn semaphore | Supported | Unbounded fan-out risk | Leading read-only group; thread per call |
| Persistent terminal or PTY | No | Yes | Yes | No | Excellent durable terminal model |
| Bounded tool output | Yes | Generally | Yes with spill storage | Yes | Yes with opaque retrieval handles |
| Web and vision | External/provider dependent | Native | Native | Provider/extension dependent | Native |
| MCP client | Stdio and Streamable HTTP | Rich | Rich | Extension only | Stdio, HTTP, legacy SSE |
| Skills and commands | Repository-local | Rich discovery/packages | Local/remote skills and commands | Strong package/resource UX | Multiple compatible roots and install |
| External executable addons | MCP | MCP and hosted tools | MCP and JS/TS plugins | Extensions | MCP |
| In-process extensions | No general plugin API | Compile-time Rust contributors | Broad sequential JavaScript hooks | Broad TypeScript extension API | Compile-time typed hooks |
| Context/memory extension | No first-class seam | Compile-time contributors | Partial plugin/context mechanisms | Strong resource/context hooks | Budgets but no external provider seam |
| Approval policy | Read-only, ask, auto, full, scoped grants | Rich typed policy | Rich UX rules | Trust-oriented | Ask, auto, yolo, exact targets |
| Capability filesystem containment | Yes | Yes | No | No | Policy only |
| Native OS process sandbox | No | Seatbelt, seccomp/bubblewrap, Landlock, Windows | No | No | No substantiated backend |
| Bounded scheduling/backpressure | Mostly; store fairness remains | Several unbounded queues | V2 SSE bounded, global pubsub unbounded | Several unbounded fan-outs | Bounded data; steps default unbounded |
| Dynamic package installation | No | Plugins, skills, marketplaces | npm/local plugins and skills | Strong | Skills only |

### Interfaces And Operations

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Interactive TUI | Yes | Yes | Yes | Rich | Differential renderer |
| Direct automation | `ask`, durable `run`, JSONL | Exec and JSONL | Run and JSON | Print, JSON, RPC | Ask, JSON, replay |
| Long-running HTTP/SSE server | Yes | No stable HTTP daemon | Yes | No | No |
| ACP | No | No primary ACP surface found | Yes | Custom RPC | Yes over stdio |
| OpenAI-compatible API | No | No | No | No | No |
| Native client | Rust `qq-client` | Internal Rust crates | Generated TypeScript client | TypeScript SDK | Native core plus JS bridge |
| Python or TypeScript SDK | No | Both, with performance caveats | Yes | Strong embedded SDK | Experimental Node/Wasm SDK |
| Layered configuration | Yes | Extremely broad | Extremely broad | Yes | Yes |
| Provider authentication | OAuth, keys, organizations | Broad | Broad | Broad | Three provider-specific flows |
| Server authentication | Local-instance bearer | Trusted-local app server | Optional Basic Auth | Not applicable | ACP stdio |
| Fleet observability | Partial | Strong OTLP/runtime metrics | OTLP and logs | Telemetry-oriented | Local trace and stats |
| Evaluation/performance gates | Partial | Weak benchmark coverage | Partial | Provider/eval ergonomics | Strong eval, startup, and size discipline |
| Messaging, cron, or voice | No | No harness-plane feature | No harness-plane feature | No | No |

## Detailed Reference Findings

### Codex

Codex provides:

- a streaming turn loop with reasoning, messages, tools, retries,
  cancellation, steering, review, diffs, goals, memories, web, images, and
  pre- and mid-turn compaction;
- Responses-compatible providers, Bedrock and Mantle variants, Ollama, LM
  Studio, model catalogs, reasoning controls, remote compaction, cached HTTP
  and WebSocket transport, preconnect, sticky routing, and request
  compression;
- a very broad tool catalog including PTY execution and stdin, patching,
  media, web, MCP, planning, permissions, skills, plugins, goals, and agent
  control;
- start, resume, fork, archive, delete, rollback, compact, name, list, and
  replay session behavior;
- canonical JSONL history plus a rebuildable SQLite projection;
- copy-on-write context history, tool-result pairing, token estimation, local
  and remote compaction;
- full-session sub-agents with depth/concurrency limits and root-scoped
  control;
- the strongest inspected sandbox set: macOS Seatbelt, Linux seccomp plus
  bubblewrap, legacy Landlock, and Windows restricted tokens;
- rich approval and permission profiles;
- MCP, declarative plugin bundles, compile-time Rust contributors, hooks,
  skills, marketplaces, app-server, exec-server, TUI, CLI, TypeScript and
  Python SDKs; and
- extensive OTLP metrics including startup, TTFT, tool, process, persistence,
  and memory signals.

QQ should borrow transport prewarming, immutable tool snapshots, sandbox
adapters, lifecycle vocabulary, hook trust hashes, root-scoped agent control,
and metric coverage.

QQ should reject Codex's 141-member decomposition, broad compile-time
extension and optional code-mode footprint, unbounded queues,
subprocess-per-turn TypeScript design, trusted-local administrative APIs on an
external surface, and fail-open persistence error handling.

### OpenCode

OpenCode provides:

- a current streaming coding loop with retries, tool use, structured output,
  compaction, doom-loop handling, and serialized sessions;
- broad provider, model, pricing, limit, variant, and cache catalogs;
- shell, read, glob, grep, edit, write, patch, task, web, question, todo,
  skill, LSP, custom JavaScript/TypeScript, plugin, and MCP tools;
- SQLite-backed sessions and an experimental V2 event store that atomically
  commits events, aggregate sequence, and projections before publishing;
- an idempotent durable prompt inbox and session-local serialized execution
  coordinator in V2;
- context epochs, compaction summaries, result pruning, and output spill;
- configurable built-in agents and foreground/background child tasks;
- wildcard permission rules, but explicitly no security sandbox;
- rich MCP support and very broad in-process sequential plugin hooks;
- local/remote skills, commands, references, CLI, TUI, ACP, HTTP/OpenAPI/SSE,
  PTY/WebSocket, desktop, generated clients, and an embedded internal-fetch
  SDK; and
- structured logs, OTLP, statistics, and partial performance tests.

QQ should borrow atomic event/projection semantics, durable idempotent prompt
admission, one drain per session with cross-session parallelism, typed
protocol/client composition, scoped tool registration, durable full-output
references, and catalog separation.

QQ should reject the dual V1/V2 runtime, enormous bundled dependency graph,
sequential trusted hooks, process-local background jobs, fire-and-forget plugin
readiness, optional unauthenticated server, and approval prompts without real
containment.

### Pi

Pi provides:

- a stateful streaming agent loop with text, thinking, tool events, steering,
  follow-ups, retry, cancellation, images, and reasoning controls;
- a broad provider and model API with dynamic models, SSE/WebSocket, vision,
  image generation, lazy imports, and deterministic fake providers;
- replaceable filesystem and process operations suitable for SSH, VM, or
  sandbox hosts;
- append-only branching sessions with resume, fork, clone, labels,
  export/import, and sharing;
- compaction and branch summaries;
- a broad TypeScript extension API for tools, commands, UI, providers,
  resources, hooks, themes, packages, skills, and context;
- TUI, print, JSON, RPC, embedded SDK, and an experimental strict binary
  protocol; and
- useful provider/resource/TUI telemetry and evaluation ergonomics.

Its shipped coding-agent path still uses synchronous JSONL persistence. The
newer JSONL/SQLite storage code has promising contracts, but central durable
harness operations are stubbed. MCP is an extension rather than native,
sub-agents are an example that spawns child CLI processes, extensions receive
full process authority, and listener/tool fan-out can be unbounded.

QQ should borrow provider/resource ergonomics, lazy imports and prewarming,
progressive skill disclosure, replaceable operation objects, dynamic tool
loading, differential TUI ideas, strict snapshots, and passive telemetry.

QQ should reject synchronous filesystem work in the agent loop,
publish-before-persist, full-process plugin authority by default, unbounded
`Promise.all` tool execution, and parallel legacy/stub runtimes.

### fx

fx provides:

- a small Zig runtime with streamed content/reasoning/tools, steering at model
  boundaries, cancellation, deadlines, recovery checkpoints, and explicit
  retry delivery certainty;
- three fixed provider identities with catalogs, auth, structured output,
  reasoning, vision, usage, and search;
- `glob_files`, `grep_files`, `read_file`, `write_file`, `edit_file`,
  `web_fetch`, `web_search`, `terminal`, `capability_search`, `skill`,
  `install_skill`, `subagent`, `mcp_select_tool`, `mcp_features`,
  `ask_user_question`, `vision`, and `read_tool_result`;
- excellent durable terminal semantics: exec, start, read, screen, write,
  wait, monitor, inspect, list, resize, signal, close, cursors, leases, and
  native/tmux backends;
- bounded and secret-masked tool results with opaque handles;
- strong local sessions using framed event logs, sequence and generation
  validation, checksummed replacement, periodic checkpoints, compaction,
  single-writer locking, and recovery;
- deterministic bounded compaction with stable prompt prefixes;
- rich durable sub-agent lifecycle, immutable admission snapshots,
  communication envelopes, consumer cursors, and non-escalating child
  authority;
- exact permission targets, MCP, compatible skill roots, typed compile-time
  hooks, CLI/TUI, ACP, native Node/Wasm embedding, configuration, local traces,
  deterministic replay, live-model evals, and profile-guided size tooling.

Its advertised 7.8 MiB binary and 2 ms startup budgets were not measured in
this audit. The provider, tool, and hook registries are fixed in source; no
HTTP/SSE daemon, generic provider registration, dynamic native plugin system,
fleet observability, or real OS sandbox was found. Its default maximum step
count is unbounded, and read-only parallel tools spawn an OS thread per call.

QQ should borrow delivery-certainty-aware retries, dynamic MCP schema
selection, terminal semantics, immutable sub-agent admission, exact permission
targets, performance budgets, and shared-runtime discipline.

QQ should reject fixed registries, ACP as the only service boundary,
thread-per-tool parallelism, an inert sandbox setting, unbounded default steps,
and hardening/debuggability sacrifices made only to hit a size claim.

## Hermes-Style Product Boundary

Hermes separates its platform-agnostic core from CLI, gateway, ACP, batch, and
API interfaces. Its extensions include tools, hooks, commands, memory, context,
and progressively disclosed skills. QQ should provide the lower execution
layer for that style of product rather than absorb the product layer.

References:

- [Hermes architecture](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/architecture.md)
- [Hermes API server](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/api-server.md)
- [Hermes skill guide](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/creating-skills.md)

The boundary is:

```text
Hermes-like product
  channels / cron / voice / web / product identity / product memory
                              |
                  qq-client / versioned HTTP
                              |
                         qq-server
                              |
                    deep SessionRuntime
                      /             \
          CompiledAgentPlan       SQLite
          - provider              authoritative events
          - tool plan                    |
          - agent pack            post-commit SSE/outbox
          - context plan
          - policy/budgets
```

A Hermes-style gateway should:

1. map its user, channel, and thread identity to a QQ `SessionId`;
2. select a versioned `AgentProfileId`;
3. submit an idempotent command containing input parts and run limits;
4. consume durable events through an SSE cursor;
5. relay approval requests or steering input;
6. feed committed events into product-owned memory or indexing; and
7. reconnect and resume without guessing whether work ran.

The preferred deployment is a QQ sidecar or private service per trusted user
or workspace boundary. A product gateway owns internet-facing authentication,
tenant identity, channel policy, and rate limiting. If a real consumer needs a
shared QQ service, scoped bearer/session tokens and resource accounting should
be designed before broad remote exposure; do not grow general IAM inside
`qq-core`.

Hermes exposes an OpenAI-compatible API. QQ may add an equivalent facade for
compatibility, but it must not become QQ's primary contract. Chat-completions
semantics cannot faithfully represent durable approvals, replay cursors,
sub-agent state, tool progress, steering, or explicit run outcomes.

## Target Architecture

### Compile Cold, Execute Hot

The central new deep module is an immutable `CompiledAgentPlan`:

```text
compile(profile, resolved configuration, capability snapshots)
    -> Arc<CompiledAgentPlan>

run(session, input, limits, cancellation)
    -> durable event stream
```

`CompiledAgentPlan` is a live runtime object and is never serialized. It
contains:

- the compiled provider handle and immutable model identifier;
- effective context, output, reasoning, media, cache, and pricing capabilities;
- the versioned prompt, persona, and instruction snapshot;
- an immutable tool schema and dispatch plan;
- skill, command, addon, and MCP catalog digests;
- the bounded context-source plan;
- permission, approval, terminal, and sub-agent policy;
- cost, time, turn, tool, token, output, and concurrency limits; and
- an associated secret-free `AgentPlanDescriptor` sufficient to reproduce and
  explain the execution.

`AgentPlanDescriptor` is the canonical serializable identity. It contains only
behavioral and provenance data: provider endpoint/protocol/auth scheme, the
credential reference name, model and capability values, prompt/tool/context
digests, policy, limits, addon versions, and adapter build/version identity. It
never contains a resolved key, bearer token, authorization header, secret
hash, live provider handle, callback, or process handle.

`AgentPlanDigest` is the SHA-256 of one canonical encoding of that descriptor.
The canonical encoding, field ordering, normalization, and version are contract
fixtures. Secret values are neither hashed nor persisted. Credential rotation
uses a separate opaque, non-secret `CredentialEpoch` owned by `qq-auth`.
Rotation invalidates or reauthorizes matching live cache entries, and accepted
runs may record the opaque epoch for diagnosis without changing the behavioral
plan digest or revealing credential material.

Provider names, addon names, config layering, manifest discovery, schema
compilation, filesystem discovery, secret resolution, and trust decisions do
not belong in the turn loop. The loop receives direct handles and exhaustive
provider-neutral values.

Plans are cached by the plan digest plus adapter build identity and credential
epoch. A config, skill, tool schema, profile, provider, policy, model, or
adapter change produces a new generation. Active runs retain their original
`Arc`; a successful refresh atomically publishes the new generation for later
runs. A failed refresh does not poison an already-valid plan.

The cache has hard entry-count and estimated-byte ceilings established from
Phase 0 measurements. It evicts least-recently-used inactive generations.
Active `Arc` generations count toward the memory ceiling but cannot be evicted;
if admission cannot fit another plan after inactive eviction, compilation fails
with an explicit capacity error rather than growing without bound. Tests cover
eviction, active-generation pinning, refresh storms, credential rotation, and
shutdown.

### Ownership Within The Existing Crates

Do not add an agent-framework, plugin, tool-host, context, or addon crate.

| Owner | Responsibility |
| --- | --- |
| `qq-config` | Parse and merge agent-profile and addon declarations, validate syntax, retain provenance |
| `qq-auth` | Resolve provider and addon secret references without exposing secret values to config or protocol |
| Root package | Discover trusted sources, translate external config, compile/cache plans, and wire concrete adapters |
| `qq-provider` | Compile provider recipes and expose one provider-neutral stream handle plus effective capabilities |
| `qq-core` | Own `CompiledAgentPlan`, secret-free `AgentPlanDescriptor`, execution invariants, context-source contracts, tools, run limits, and durable outcomes |
| `qq-mcp` | Supply bounded MCP capability/catalog snapshots and execute selected MCP operations |
| `qq-protocol` | Own versioned profile IDs, plan digests, input parts, commands, outcomes, events, and capabilities |
| `qq-server` | Map authenticated HTTP/SSE requests onto protocol commands; no agent logic |
| `qq-client` | Provide bounded command, reconnect, replay, approval, steering, and capability APIs |
| `qq-tui` | Project protocol state; never discover or execute addons itself |

Application configuration types must not leak into `qq-core`. The root
translates them into typed provider handles, tool/catalog snapshots, context
plans, and policies.

### Extension Lanes

| Lane | Interface | Load time | Hot-path behavior | Trust/isolation |
| --- | --- | --- | --- | --- |
| Agent packs | Declarative versioned manifest | Discovery/startup | Immutable prompt/profile data | Hash and trust source; no code execution |
| Providers | Existing Rust provider compiler/stream seam | Startup or explicit refresh | Direct provider handle | Compile-time trusted adapter; secrets resolved outside core |
| Native tools | Static Rust registration | Build/startup | Direct dispatch | Fully trusted; capability-scoped execution |
| General tools | MCP, then a real embedded callback host | Startup catalog; call on demand | One selected adapter call | MCP process/HTTP boundary or trusted embedder |
| Context/memory | Typed bounded `ContextSource` | Plan compile plus pre-turn fetch | No per-delta hook | Time/byte/token budgets; fail policy explicit |
| Observers | Durable SSE/outbox | Subscription | Post-commit only | Cannot affect authoritative execution |
| Process execution | Local implementation plus one real sandbox adapter | Startup | Direct selected backend | Explicit filesystem/network/process capabilities |
| Surface adapters | Versioned `qq-client` contract | Client startup | Outside agent loop | Product owns remote auth and UX |

An addon package may declaratively bundle profiles, prompts, skills, commands,
MCP servers, context-source declarations, configuration schemas, and secret
references. The manifest is packaging and provenance, not a universal runtime
plugin object.

### Agent Packs

An agent pack is the cheapest and safest customization path. Its versioned
manifest should eventually support:

- stable `id`, version, display name, and schema version;
- one or more agent profiles;
- prompt/persona/instruction resources;
- skill and command roots;
- model and provider constraints;
- tool allow/deny/exposure policy;
- MCP server references;
- context-source declarations;
- run and sub-agent budget defaults;
- required configuration keys and secret references;
- minimum QQ protocol/capability requirements; and
- content digests for every execution-affecting resource.

Loading must use progressive disclosure. Pack metadata is cheap and available
at discovery; full skill or prompt content enters a plan only when the selected
profile requires it. A pack may not contain arbitrary native executable code.
Executable behavior arrives through an already-trusted native tool, MCP, or a
future embedded host.

### Tool Hosts

The built-in Rust tools remain the zero-overhead path. Do not wrap them in RPC
or a dynamic plugin abstraction.

MCP remains the general external executable-tool path. Improve it with dynamic
capability search and selected-schema loading so a large remote catalog does
not inflate every model request.

Only when an actual embedding consumer needs application callbacks should QQ
add a second external-tool adapter. At that point deepen MCP and embedded
callbacks behind one `ExternalToolHost` contract that provides:

- immutable catalog generation and digest;
- tool schema, annotations, effect class, and ownership;
- bounded request and result sizes;
- deadline and cancellation;
- concurrency and queue limits;
- readiness and shutdown;
- exact error and retry semantics; and
- optional durable full-output references.

The hot path selects one precompiled tool entry. It does not execute a list of
before/after plugin hooks or rediscover schemas.

### Context And Memory

Product memory is not a synchronous observer of every token. A
`ContextSource` supplies bounded pre-turn context and may consume committed
events asynchronously after the fact.

The source contract needs:

- stable identity and version;
- deterministic cache key inputs;
- a time, byte, result, and estimated-token budget;
- cancellation;
- provenance attached to inserted context;
- an explicit fail-open or fail-closed policy; and
- no authority to modify durable transcript history.

Ordinary retrieval should fail open with a visible diagnostic rather than
block an otherwise valid run. A product that requires a memory checkpoint
before compaction may opt into a specific fail-closed pre-compaction operation;
that is a separate contract from ordinary retrieval.

Post-commit event ingestion should use the same durable cursor semantics as
other clients. Memory consumers may fall behind and replay; they cannot delay
the event commit or user-visible stream.

### Observers And Hooks

Use durable event subscribers for logging, analytics, memory ingestion,
notifications, and product automation.

Synchronous decisions remain limited to core invariants such as approval,
exact tool validation, and budget admission. If an external policy engine is
eventually required, it must have a typed request, a strict deadline, a
documented fail policy, and no access to arbitrary stream mutation.

Do not provide arbitrary synchronous pre/post hooks around provider deltas,
tool output, persistence, compaction, or session lifecycle. Codex, OpenCode,
and Pi demonstrate how ergonomic broad hooks can silently add tail latency,
failure coupling, and unbounded authority.

### Provider Footprint

Keep the current provider crate and public `Provider::stream` seam. Feature-gate
adapter families internally. The H1 audit settled on one feature:

```text
provider-bedrock   (default; owns the seven optional AWS SDK crates)
test-support       (loopback fixtures; not a public API)
```

The HTTP families need no feature because they add no dependency beyond the
shared reqwest transport, so a `provider-http` flag would gate nothing. The
shipped full `qq` binary enables all supported providers, while embedders build
`--no-default-features` and compile only the HTTP families. Neutral request, model, pricing, event, and compiler types
remain available without pulling every vendor SDK.

Do not introduce one-provider-per-crate organization. Wire-format differences
are real, but the existing crate already centralizes shared HTTP, SSE,
redaction, construction, and compilation behavior. Split only measured heavy
dependencies, not cohesive protocol code.

## Backend Protocol Additions

The native QQ protocol remains the authoritative interface. Additions are
versioned and transport-neutral.

### Structured Input

Replace the external assumption that a prompt is only a string with bounded
input parts such as:

```text
InputPart
  Text
  WorkspaceFileReference
  ImageReference
```

The first implementation should not create a general asset service. Workspace
references remain capability-scoped and optionally hash-bound. Image content
may use a bounded request representation or an opaque server-owned content
reference. Provider capability validation occurs before accepting a run that
cannot consume the parts.

### Agent Profiles And Plan Identity

Add:

- `AgentProfileId`;
- selected profile on session creation or explicit session update;
- effective `AgentPlanDigest` on accepted runs and snapshots;
- source/addon/profile versions in trace metadata; and
- capability errors when a profile cannot compile in the current runtime.

The persisted run records the secret-free `AgentPlanDescriptor`, its digest,
and the opaque credential epoch—not the live compiled plan or secret material.
A later configuration or credential refresh must not change an accepted run's
identity or live handles.

### Core-Owned Run Limits

Define one provider-neutral `RunLimits` contract covering the supported subset
of:

- elapsed time;
- provider turns;
- total and per-slice tool calls;
- input and output tokens where measurable;
- estimated cost;
- tool/output bytes;
- child count, depth, and concurrent children; and
- provider/tool concurrency.

Every accepted limit yields a typed core-owned terminal outcome. A caller must
not believe a bound was enforced when the active provider cannot supply the
required accounting signal.

### Steering And Interruption

Distinguish:

- queueing the next user prompt;
- steering an active run at a safe model/tool boundary;
- interrupting the current provider/tool operation; and
- cancelling the run to a durable terminal outcome.

Each command is idempotent and receives an observable accepted, rejected,
superseded, or terminal response. Steering may not mutate a provider request
already known to be possibly delivered.

### Capability Discovery

Clients need a versioned capability document covering:

- protocol version;
- supported input parts;
- provider/model capabilities;
- available agent profiles;
- approval and steering operations;
- tool-host and context-source health;
- maximum request/event sizes;
- server feature generations; and
- optional compatibility facades.

Clients format supported behavior; they do not infer it from provider names or
silently fall back.

### Correlation Metadata

Allow a small bounded opaque correlation map on session/run creation for a
gateway's user, channel, thread, request, or job identifiers. QQ stores and
returns it for attribution but does not interpret product identity or use it
as authorization.

## Performance Constitution

### What To Measure

H0's Phase 0 deliverables are the canonical scope for the initial baseline.
They measure only current, publicly observable default behavior:

- release binary size and dependency closure;
- first and repeated fresh-process command startup, without claiming control of
  the operating-system page cache;
- server readiness plus new-store, existing-store, and idle-shutdown latency;
- idle release-server RSS and isolated active load-worker RSS;
- direct and HTTP command acknowledgement;
- submit start to deterministic provider entry as a public upper-bound proxy
  for scheduler admission and provider handoff;
- provider semantic delta to post-commit core observation;
- provider semantic delta to authenticated HTTP/SSE client observation;
- existing read and one-shot shell tool dispatch through durable completion;
- cancellation and shutdown;
- snapshot, reconnect, and replay;
- one, ten, and one hundred concurrent sessions; and
- long-stream scaling at 64 KiB, 512 KiB, and one MiB.

The SQLite commit instant and exact provider-claim/send instant are not exposed
by current public seams, so Phase 0 labels those observations as proxies rather
than adding generic runtime instrumentation. Exact commit-to-TUI rendering is
qualified by its owning readiness work. R4's complete streaming/fairness matrix
is now qualified and imported by the Phase 1 receipt below. Compaction/context
planning and sub-agent fan-out, fairness, cost, and memory remain owned by R5
and R7 and are imported only when those milestones complete. Completing H0 did
not itself complete any readiness milestone.

Use fake providers and temporary stores for deterministic runtime latency.
Separate provider network latency from QQ latency. Use fixed-model live runs
only for outcome and cache qualification.

Do not benchmark nonexistent behavior in Phase 0. H1 adds and then measures
full/minimal feature profiles. H2 adds a plan-compilation/cache benchmark. The
terminal-owning readiness phase adds persistent-terminal benchmarks if that
contract ships. Every later phase records the pre-change baseline for its own
new behavior before enforcing a regression gate.

### Existing Performance Targets

Carry forward the active readiness targets:

| Gate | Target |
| --- | ---: |
| Command acknowledgement p95 | `<= 10 ms` |
| Warm claimed run to provider send p95 | `<= 25 ms` |
| Semantic delta to durable commit | `<= 15 ms` p95; `<= 40 ms` p99 |
| Durable delta to TUI | `<= 25 ms` p95; `<= 60 ms` p99 |
| Cancellation | `<= 100 ms` |
| Output starvation with eight active streams | None longer than `50 ms` |
| One MiB request plus 32 schemas | `<= 10 ms` encode; heap `<= 2x` payload |
| One MiB stream scaling | `<= 2.2x` the half-size work after fixed cost |
| Context overflow sent to a provider | Zero |
| Compaction reduction when required | At least `8x` |
| Stable-prefix provider cache use | At least `80%` where supported |
| Provider retry amplification | `< 1.05` attempts per logical turn |
| Harness-attributable evaluation failures | `< 0.5%` |

Phase 0 establishes honest binary-size, startup, and RSS baselines. Its first
versioned budget carries forward directly enforceable readiness targets and
adds conservative relative regression limits; a current baseline may therefore
be correctly red without making the measurement harness incomplete. The fx
7.8 MiB and 2 ms claims are useful ambition, not transferable QQ targets: QQ
intentionally ships SQLite, HTTP/SSE, multiple provider protocols,
authentication, and stronger durability.

### Extension Performance Invariants

- A disabled adapter family adds no shipping dependency to a minimal build.
- Disabled addons add no run-loop allocation and no observer dispatch.
- Plan lookup is digest/cache lookup, not filesystem discovery.
- Tool/provider selection is resolved before repeated model turns where
  possible.
- No observer can block durable commit or client delivery.
- Every extension queue and concurrency permit is bounded.
- A new extension mechanism may not regress the disabled/default hot path by
  more than five percent without an explicitly accepted tradeoff.
- Catalog changes compile a new immutable generation rather than mutating a
  live registry under the run loop.
- Active runs never wait for an unrelated addon reload.
- All runtime traces identify the exact plan and addon generations.

## Keep, Redesign, And Reject

### Keep

- The current crate graph and root composition boundary.
- One runtime shared by all execution surfaces.
- `ProviderCompiler` and the small provider-neutral `Provider::stream` seam.
- SQLite as the first authoritative store.
- Persist-before-publish and one terminal event per accepted run.
- Idempotent commands, cursor replay, and HTTP/SSE.
- Static built-in tools as the fastest execution path.
- MCP as the general executable-addon protocol.
- Capability-scoped filesystem access, exact approval targets, CAS writes,
  bounded output, and cancellation.
- Durable child ownership, recovery, limits, and accounting.

### Redesign First

| Area | Problem | Direction |
| --- | --- | --- |
| Streaming store | Full reconstruction and string concatenation | Linear chunks and materialization at read/compaction boundaries |
| Reasoning store | Transaction per delta | Bounded batches preserving event order |
| Store scheduling | Priority and polling can starve output | Wake-driven fair scheduling and measured bounded group commit |
| Model/context | Effective model limits are dropped | Persist `ResolvedModel`; incremental model-aware context plan |
| Run budgets | Caller-specific enforcement | Core-owned limits and typed terminal outcomes |
| Retry | Core/provider ownership can amplify | Delivery-certainty-aware attempt contract |
| Search/edit | Literal/exact-only contracts | Evaluation-driven ignore-aware search and patch edit |
| Terminal | One-shot only | Durable process handles, cursors, leases, monitors, optional PTY |
| Provider footprint | AWS SDK always linked through provider crate | Feature-gate heavy adapter families internally |
| Embedding | No profile/plan plane | `AgentProfile` plus cached `CompiledAgentPlan` |
| Protocol | Text-only and weak active control | Input parts, profile, plan digest, limits, steering, capabilities |
| Observability | Incomplete end-to-end evidence | Admission, compile, send, TTFT, persist, deliver, tool, replay spans |

### Reject

- Provider-name branches in request or stream hot paths.
- A provider-per-crate layout without measured build evidence.
- A second embedded agent runtime.
- Universal synchronous plugin hooks.
- Dynamic native-library loading.
- Unbounded listener, tool, provider, or child fan-out.
- Fire-and-forget addon loading with suppressed failures.
- Permission prompts presented as process sandboxing.
- Messaging, cron, voice, browser, or product memory inside core.
- An OpenAI-compatible facade as the source-of-truth protocol.
- A speculative distributed scheduler or cloud control plane.
- A broad tool catalog whose quality and latency have not won paired
  evaluations.

## Implementation Sequence

The work uses `H` task identifiers to avoid colliding with the existing
Terminal-Bench task numbering.

### Existing Roadmap Dependencies

The following are milestones owned by
[`terminal-bench-readiness.md`](./terminal-bench-readiness.md), not new tasks in
this plan:

| Milestone | Owning phase | Required outcome here |
| --- | --- | --- |
| R4 | Phase 4, linear and fair durable streaming | Streaming/reasoning persistence meets linearity, ordering, and fairness gates |
| R5 | Phase 5, resolved model, context, and spend | Effective model state and core-owned limit outcomes are available to plan compilation |
| R6 | Phase 6, tool tournament and terminal | Any richer search/edit/terminal contract has won its ablation and cleanup gates |
| R7 | Phase 7, sub-agent economics and scheduling | Child accounting/admission behavior is measured and remains bounded |
| R8 | Phase 8, warm runtime and request efficiency | Retry ownership, stable-prefix caching, and warm-path work meet their owning gates |

This document consumes completion evidence from those phases. It does not
redefine their schemas, migrations, tool contracts, or tests.

### Task Index

| Task | Outcome | Depends on | Primary owner |
| --- | --- | --- | --- |
| H0 | Complete: current-runtime speed, size, RSS, replay, and concurrency baseline | None | `xtask`, existing benches |
| H1 | Cargo feature/dependency profiles, full/minimal baselines, and budget gates | H0 | Root, `qq-provider` |
| H2 | Immutable live `CompiledAgentPlan`, secret-free descriptor, and bounded cache | H1, R5 | Root, config, core, provider |
| H3 | Input parts, profiles, plan identity, limits, steering, capabilities, and correlation | H2 | `qq-protocol`, server, client |
| H4 | Client conformance fixtures and first external SDK | H3 | `qq-client`, external adapter |
| H5 | Declarative addon/agent-pack manifest | H2, real consumer | Root, config |
| H6 | Progressive skill and capability catalog compilation | H2, H5 | Root, core, MCP |
| H7 | Embedded external-tool host beside MCP | H2, real embedder | Core, MCP, root |
| H8 | Bounded `ContextSource` and cache contract | H2, real memory consumer | Core, root |
| H9 | Post-commit observer/outbox conformance | H3, H8 | Protocol, client, server |
| H10 | First real OS process-sandbox adapter | R6, platform threat model | Core tools, root |
| H11 | Optional ACP/OpenAI compatibility facade | H4, real consumer | Adapter in existing surface owner |
| H12 | Crash, load, security, quality, and performance qualification | All shipped tasks and required R milestones | Workspace-wide |

### Phase 0 — Establish The Speed Constitution

Implement H0 before extension work.

Deliverables:

- reproducible benchmark commands and machine metadata;
- the current default dependency tree;
- current release binary and RSS measurements;
- first and repeated fresh-process observations plus server/runtime startup
  distributions;
- admission, provider-send, durable-delta, client-delivery, cancellation,
  replay, and tool-dispatch distributions;
- one/ten/one-hundred session load profiles;
- long-stream complexity evidence; and
- benchmark output that is generated and remains untracked.

Acceptance:

- repeated runs report median, p95, and p99 where meaningful;
- network/provider latency is separated from QQ runtime latency;
- measurements describe the exact current feature set and revision;
- a versioned comparator recomputes summaries and rejects incompatible reports,
  failed fixture checks, excessive noise, p95 latency/size regressions,
  throughput-median regressions, and configured p99 or absolute limits; and
- no later phase can claim speed without naming the affected gate.

#### Phase 0 Completion Receipt — 2026-09-01

H0 is complete. `cargo xtask perf baseline` now pins the host target, builds the
default locked release artifact, and re-executes an optimized worker. It
snapshots the exact Git status, source content and file modes, and lockfile
identity before the build; removes inherited Rust/Cargo codegen overrides;
records hashed native-toolchain environment and Cargo-configuration identities;
and rechecks both source and artifact identity after measurement. It writes one
versioned JSON report and uniquely named dependency tree beneath ignored
`target/qq-perf/`; existing symlink components cannot redirect `--output`
outside that directory. The report records the source manifest, exact
default-feature build, artifact, machine, requested workload, effective
per-metric sample counts, raw samples, distribution summaries, metric
direction, correctness checks, and unsupported boundaries. `cargo xtask perf
check` recomputes every summary and compares compatible reports against
[`benchmarks/perf/budgets-v1.json`](../../benchmarks/perf/budgets-v1.json) and
exits nonzero on a regression.

The deterministic fixture uses temporary SQLite stores/workspaces, the runtime
default `WAL` plus `synchronous=FULL` durability, fake providers, authenticated
QQ HTTP/SSE established before prompt submission, validated tool results,
bounded provider timing signals, deadline-wrapped runtime/client cases, and
always-run bounded runtime/server/process cleanup. Each 1/10/100-session load
profile runs in an isolated optimized child with a sidecar RSS sampler. Linux
recording fails rather than omitting any required idle or active RSS sample.
One hundred sessions means one hundred admitted sessions contending through the
current default cap, and the receipt requires observed provider concurrency to
equal `min(session_count, configured_limit)`. Provider network time and model
quality are excluded. The recorder currently refuses non-Linux hosts until safe
native path isolation and RSS sampling exist.

The clean 100-sample qualification ran from detached revision
`638330550aa916d9540409a6497128a5ad9a61b9` with `source.dirty = false` on the
`linux-x86_64-local` machine class: Linux 7.2.0, AMD Ryzen 9 9950X, 32 logical
CPUs, 64.9 GB RAM, powersave governor, Rust/Cargo 1.97.1. The optimized default
artifact was 62,599,288 bytes with a 2,761-line dependency tree. The report
contained all 47 metrics and all 14 correctness receipts passed.

| Selected clean metric | Median | p95 | p99 |
| --- | ---: | ---: | ---: |
| Fresh `qq --version` process | 1.722 ms | 2.419 ms | 2.770 ms |
| Isolated server readiness | 75.442 ms | 79.080 ms | n/a (20 samples) |
| Idle server RSS | 15.59 MiB | 16.06 MiB | n/a (20 samples) |
| Durable direct command acknowledgement | 3.211 ms | 3.475 ms | 6.336 ms |
| Provider delta to committed core event | 6.235 ms | 7.491 ms | 11.710 ms |
| Cancellation to committed terminal event | 6.360 ms | 9.202 ms | 14.638 ms |
| 100-session batch | 3.050 s | 7.768 s | n/a (10 batches) |
| 100-session throughput | 29.710 runs/s | 38.216 runs/s | n/a (10 batches) |
| 100-session worker peak RSS | 15.54 MiB | 15.75 MiB | n/a (10 batches) |
| 100-session maximum active runs | 8 | 8 | n/a (10 batches) |

The versioned Phase 0 self-comparison intentionally reported one existing red
target: the measured 1 MiB/512 KiB durable-stream p95 ratio was `2.292x`
against the checked-in `2.200x` ceiling. The budget was not weakened. R4 later
turned the same gate green at `1.925x`; the historical Phase 0 report remains
unchanged. No other Phase 0 budget failure was reported.

Qualification commands:

```sh
cargo xtask perf baseline \
  --machine-class linux-x86_64-local \
  --samples 100 \
  --warmups 10 \
  --output target/qq-perf/phase0-full-r3.json
cargo xtask perf check \
  --baseline target/qq-perf/phase0-full-r3.json \
  --candidate target/qq-perf/phase0-full-r3.json \
  --budgets benchmarks/perf/budgets-v1.json
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

The isolated HTTP/SSE cases needed loopback permission outside the restricted
execution sandbox; provider traffic remained the deterministic in-process fake
and made no external network request. Raw machine samples remain untracked by
design. The reproducible protocol and complete measurement inventory are in
[`benchmarks/perf/README.md`](../../benchmarks/perf/README.md).

### Phase 1 — Complete Owned Prerequisites And Feature Profiles

Complete R4 and R5 through their owning readiness phases, then implement H1.
Do not copy their implementation contracts into this plan.

Status: complete 2026-09-02. R4, R5, and H1 receipts follow.

#### Imported R5 Completion Receipt — 2026-09-02

The owning [Terminal-Bench readiness plan](./terminal-bench-readiness.md)
records the Phase 5 receipt: immutable resolved-model identity, provider-aware
context admission with exact cross-run occupancy reuse (schema 18), core-owned
`RunLimits` with the typed `budget_exhausted` outcome (schema 19, protocol 10),
and compaction hardening — validated summaries, three-row bounded history,
`rollback_compaction`, and the `search_history` recall tool (protocol 11). The
clean detached 100-sample recorder at exact revision
`42f16c6168fef9b008e7427ed581511abb3b2760` reported 62 metrics with all 55
budgets green: a `1.931x` one-MiB/512-KiB scaling ratio, 23.410 ms p95
control response, 59.109 ms p95 cancellation, and a 49.000 ms p95 persisted
output service gap under eight concurrent streams. A first attempt on a loaded
host failed the service-gap budget by one 52 ms sample and was rerun without
weakening the budget. The live provider cache-ratio check is deferred for
credentials and is not counted as passed. Clean workspace test, format,
Clippy, and build gates passed.

#### H1 Completion Receipt — 2026-09-02

Dependency audit: of `qq-provider`'s dependencies only the seven AWS SDK crates
(`aws-config`, `aws-credential-types`, `aws-sdk-bedrockruntime`, `aws-sigv4`,
`aws-smithy-http-client`, `aws-smithy-runtime-api`, `aws-smithy-types`) are
adapter-specific; they were used by `aws.rs`, `providers/bedrock.rs`, and
`providers/mantle.rs` only. Everything else (reqwest, SSE framing, redaction,
construction, compilation, the neutral request/model/pricing/event types) is
shared by the HTTP families and stays unconditional. `aws-lc-rs` appears in
both profiles as rustls's crypto backend, not as an SDK dependency.

Feature manifest (commit `8ccba84`): `qq-provider` gains `provider-bedrock`
(default on) owning the seven optional AWS crates; the HTTP families need no
feature because they add no dependency. `BedrockAuth` moved to a neutral
module so `ProviderRecipe` is constructible and digestible in every profile;
without the feature `ProviderCompiler` returns a configuration error
(`BEDROCK_FAMILY_UNAVAILABLE_MESSAGE`) before any SDK or network work. The root
`qq` package mirrors it: `default = ["provider-bedrock"]` is the full binary,
`--no-default-features` is the minimal embedding profile. `xtask` pins the full
profile for canaries. The provider self dev-dependency drops default features so
the minimal profile is genuinely tested rather than re-unified by Cargo.

Acceptance:

- the readiness plan marks R4 and R5 complete (receipts above);
- the shared interface fixtures (`crates/qq-provider/tests/interface`, nine
  cases across OpenAI Responses, Chat Completions, Anthropic, Google, and the
  canary path) pass under both `--features provider-bedrock` (186 lib tests)
  and `--no-default-features` (142 lib tests; the 44 gated tests are AWS
  configuration, Bedrock, and Mantle adapter tests);
  `bedrock_recipes_compile_only_with_the_provider_bedrock_feature` asserts the
  refusal path;
- the minimal closure links none of the seven AWS crates, asserted by the
  recorder's `minimal_profile_excludes_heavy_provider_dependencies` receipt
  (272 distinct crates against 326 for the full profile);
- no neutral type, request path, or event changed shape; the gate sits at
  recipe compilation and in one `RequestAuthorizer` field;
- budgets are enforced: `qq_release_binary_bytes` ≤ 70 MB,
  `qq_minimal_release_binary_bytes` ≤ 56 MB, and
  `qq_minimal_dependency_closure_crates` ≤ 300 are absolute ceilings, with
  relative regression limits on both profiles' startup, readiness, and RSS
  metrics (`benchmarks/perf/budgets-v1.json`, fixture 3); and
- no provider-per-crate split or parallel provider interface was introduced.

Measurements from the clean detached 100-sample recorder at exact revision
`8ccba84d31fe5bc1ab37587268bc716dcada1a5f` (69 metrics, all 61 budgets green):

| Boundary | Full | Minimal |
| --- | ---: | ---: |
| Release binary | 63.99 MB | 51.88 MB |
| Distinct dependency-closure crates | 326 | 272 |
| Repeated `qq --version` p95 | 2.274 ms | 2.124 ms |
| Isolated `qq serve` readiness p95 | 111.988 ms | 120.846 ms |
| Idle server RSS p95 | 17.96 MB | 15.98 MB |
| Idle server peak RSS p95 | 17.97 MB | 15.98 MB |

The minimal profile removes 12.1 MB (19%) of binary and 54 crates; idle RSS
drops ~2 MB. Startup and readiness are within noise of each other, so the
AWS closure was a size and build-time cost rather than a startup cost. The R4
fairness metrics stayed green on this run (23.987 ms control upper bound,
50.000 ms output service gap at its limit, 1.951x scaling ratio).

Phase 1 is complete.

#### Imported R4 Completion Receipt — 2026-09-01

The owning [Terminal-Bench readiness plan](./terminal-bench-readiness.md)
records the full schema, batching, fairness, capacity, recovery, and
qualification receipt. Implementation commit `ecd42e5` plus diagnostic commit
`b1cc118` passed the clean detached 100-sample recorder at exact revision
`b1cc1189a32bc0361045472bc1a4e338c2e52d06`: 62 metrics, all 55 enforced
budgets green, a `1.925x` one-MiB/512-KiB scaling ratio, 23.521 ms p95 control
response, 40.819 ms p95 cancellation, and a 50.000 ms p95 persisted output
service gap under eight concurrent streams. The three-sample isolated release
diagnostic also passed exact 64 KiB through 4 MiB direct-store payloads, and
the clean workspace test, format, Clippy, and build gates passed. Raw reports
remain ignored and untracked.

Deliverables:

- completion receipts for R4 and R5 referencing their migrations, tests, and
  benchmarks;
- an audit of which dependencies belong to provider-neutral versus concrete
  adapter code;
- feature-gated heavy provider families inside `qq-provider`;
- an explicit full binary feature manifest;
- an explicit minimal embedding feature manifest; and
- binary, dependency-closure, startup, and RSS measurements for both profiles.

Acceptance:

- the owning readiness plan marks R4 and R5 complete with its own gates;
- full and minimal profiles pass shared provider contract fixtures;
- the minimal profile excludes unused AWS SDK dependencies;
- disabling one adapter family cannot alter neutral request/event behavior;
- full and minimal budgets are enforced rather than merely reported; and
- no provider-per-crate or parallel provider interface is introduced.

### Phase 2 — Compile The Execution Plan

Implement H2 before protocol-visible plan identity or general addon packaging.

Deliverables:

- typed `AgentProfile` input and provenance without leaking config documents
  into core;
- a secret-free canonical `AgentPlanDescriptor` and digest fixtures;
- a runtime-only immutable `CompiledAgentPlan`;
- opaque credential epochs and rotation invalidation without secret hashes;
- a bounded entry/byte cache with inactive LRU eviction and active-generation
  pinning;
- atomic generation swap after successful refresh;
- explicit compile, capacity, readiness, and shutdown errors; and
- plan compilation/cache benchmarks added before the implementation is
  accepted.

Acceptance:

- the same canonical descriptor produces the same digest;
- each behavior-affecting source or adapter change produces a new digest;
- secrets and secret hashes never enter descriptors, digests, events, traces,
  snapshots, or cache diagnostics;
- credential rotation changes the opaque epoch and refreshes matching live
  authorization without changing behavioral identity;
- warm plan lookup performs no filesystem discovery;
- active runs retain their admitted generation through refresh;
- refresh failure does not break an existing valid generation;
- refresh storms and pinned entries cannot grow the cache beyond its hard
  admission bound; and
- disabled plan support stays within the default-path regression gate.

### Phase 3 — Complete The Backend Contract

Implement H3, then H4. H3 may expose plan identity because H2 already supplies
the secret-free descriptor and digest.

Deliverables:

- bounded versioned `InputPart` commands;
- session/run profile selection and effective plan identity;
- core limits and typed outcomes visible through HTTP/SSE;
- active-run steering and interruption commands;
- versioned capability discovery;
- opaque bounded correlation metadata;
- protocol fixtures for command idempotency, replay, approvals, steering,
  limits, unknown fields, and version skew;
- a complete Rust client path; and
- one thin external reference client selected by a real consumer. For a
  Python Hermes-style application, prefer a small async Python client over a
  subprocess wrapper.

Acceptance:

- an external process can create, run, approve, steer, cancel, disconnect,
  reconnect, and resume a session without reading QQ internals;
- retries cannot duplicate prompts, approvals, steering, or cancellation;
- clients learn supported features through capabilities rather than provider
  name checks;
- malformed/oversized input fails before durable run admission; and
- the external client adds no second execution implementation.

### Phase 4 — Add Declarative And Executable Extensions

Implement H5-H9 only with concrete consumers. External tool hosts perform no
implicit retry; a host-specific idempotent retry contract would require
separate evidence and must not create a second provider/tool retry layer.

Deliverables:

- declarative addon/agent-pack manifests;
- progressive skill and capability loading;
- one embedded callback tool host beside MCP;
- a shared external-tool contract only after both adapters work;
- bounded catalog generation, cancellation, deadlines, effect classes,
  ownership, concurrency, and output;
- selected MCP schema loading for large catalogs;
- typed `ContextSource` retrieval with caching, provenance, and fail policy;
- durable post-commit event ingestion;
- explicit extension readiness, error, capacity, and shutdown states;
- conformance fixtures for MCP, embedded tools, context sources, and
  observers; and
- run traces containing profile, prompt, provider, model, tool, context, and
  addon digests.

Acceptance:

- static built-in dispatch is unchanged;
- warm runs do no filesystem addon discovery;
- active runs retain their admitted addon/catalog generation;
- failed addon refresh leaves the current valid generation available;
- an external tool crash, timeout, overload, or invalid result settles
  explicitly without destabilizing the runtime;
- a context-source failure follows its declared fail policy and cannot alter
  transcript history;
- an observer can fall behind, restart, and replay from a cursor;
- no observer delays persistence or delivery; and
- every external component has bounded readiness and shutdown;
- disabled addon support stays within the default-path regression gate; and
- no universal lifecycle hook or dynamic native-library ABI is introduced.

### Phase 5 — Upgrade Execution Quality And Isolation

R6-R8 remain owned by the readiness roadmap. Implement H10 only after R6 has
selected and shipped a real terminal/process contract and a platform threat
model defines the isolation boundary. This document does not redefine the
search, edit, terminal, sub-agent, scheduling, or warm-runtime contracts.

Acceptance:

- the owning readiness plan records completion evidence for each R6-R8
  milestone required by a shipped extension;
- sandbox tests prove filesystem, network, process, and secret boundaries;
- the local and sandbox adapters pass one shared process contract suite;
- sandbox failure never silently falls back to unsandboxed execution;
- terminal/process cancellation, timeout, shutdown, and recovery leak no
  processes; and
- the sandbox adapter remains optional and feature-gated where its platform
  dependencies are not needed.

### Phase 6 — Add Product Adapters On Demand

Implement H11 only for an actual client.

Possible adapters:

- ACP;
- OpenAI-compatible HTTP;
- messaging gateway;
- cron/scheduling service;
- voice/TTS application;
- browser or desktop client; and
- a product-specific memory service.

Acceptance:

- the adapter uses `qq-client` or the native HTTP protocol;
- it introduces no alternate runtime or direct store access;
- native QQ events remain authoritative;
- capability loss in the compatibility protocol is documented and tested;
- product auth and tenancy remain outside `qq-core`; and
- the adapter can be disabled without affecting the base binary's hot path.

### Phase 7 — Qualification

H12 qualifies the complete story, not only individual modules.

Required scenarios:

- cold and warm direct/TUI/server execution;
- one, ten, and one hundred concurrent sessions;
- long text and reasoning streams;
- provider connection failure before send and ambiguous failure after send;
- store saturation, disk-full, corruption, migration, and restart;
- client disconnect/reconnect during text, tool, approval, and terminal work;
- addon discovery failure, refresh failure, crash, timeout, overload, and
  shutdown;
- context-source slowness and stale cache;
- MCP and embedded tool conformance;
- cancellation at every provider/tool/sub-agent boundary;
- terminal process cleanup;
- sandbox escape attempts;
- same-model agent-quality comparison; and
- minimal/full binary, RSS, startup, and latency gates.

## Verification Strategy

### Unit And Contract Tests

- Provider request/stream fixtures for every protocol and delivery state.
- Stable plan-digest and invalidation fixtures.
- Manifest parsing, precedence, provenance, trust, and capability tests.
- Tool-host and context-source conformance tests.
- Exhaustive run-limit and terminal-outcome tests.
- Protocol compatibility and unknown-field tests.
- Queue, output, concurrency, and cancellation bounds.

### Durable Integration Tests

Use fake providers, temporary SQLite stores, and temporary workspaces to prove:

- commands are idempotent;
- events publish only after commit;
- accepted runs have one terminal event;
- restart does not repeat possibly-executed tools;
- plans and profiles remain stable across config changes;
- child ownership and authority survive restart;
- external tool/context failures settle deterministically; and
- cursor replay reconstructs the same client state.

### Performance Tests

- Provider compiler benchmark.
- Plan compilation and cache benchmark.
- Model-request encoding benchmark.
- Streaming append and reasoning batching benchmark.
- Store fairness and replay benchmark.
- Static versus MCP versus embedded tool-dispatch benchmark.
- Context-source cold/warm benchmark.
- Persistent terminal operation benchmark.
- TUI render benchmark.
- Concurrent-session and child-fan-out load test.

### Quality Evaluation

Run same-model, same-prompt paired evaluations for tool, context, delegation,
and compaction changes. Record:

- verified success;
- wall-clock time;
- provider turns and tool calls;
- input/output/cache/reasoning tokens;
- estimated cost;
- harness failures;
- context overflow or compaction events; and
- task-specific correctness evidence.

Do not accept a microbenchmark win that reduces verified success or merely
moves work into additional model turns.

### Workspace Gates

For implementation phases, run the narrowest relevant tests while iterating,
then the applicable workspace gates:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

Run provider compilation benchmarks when provider construction changes. Run
new full/minimal performance gates whenever feature flags, addon compilation,
streaming, persistence, context, tool dispatch, or terminal execution changes.

## Risk Register

| Risk | Failure mode | Mitigation |
| --- | --- | --- |
| Universal plugin abstraction | Every run pays dynamic discovery/hook cost | Separate deep extension lanes and immutable plans |
| Addon trust confusion | Declarative package gains accidental code authority | Manifests contain data; executable behavior uses explicit native/MCP/embed boundaries |
| Cache invalidation | Run uses stale prompt/tool/policy | Digest every behavior-affecting input and retain provenance |
| Reload races | Active run observes partially updated catalog | Compile new generation off-path and atomically swap for later runs |
| Provider feature flags | Full and minimal behavior diverge | Shared contract fixtures and CI feature matrix |
| External tool overload | Runtime tasks or output grow without bound | Per-host and global permits, deadlines, quotas, bounded queues/output |
| Context latency | Retrieval dominates TTFT | Cache/prefetch, strict budgets, explicit fail policy, trace separately |
| Observer coupling | Analytics/memory delays output | Consume committed events asynchronously with cursors |
| Retry ambiguity | Duplicate billed request or side effect | Delivery certainty and provider idempotency where supported |
| Compatibility facade | Lowest-common-denominator API becomes architecture | Native QQ protocol remains authoritative |
| Terminal complexity | Leaked processes or platform divergence | One-shot shell stays fast; durable supervisor is bounded and qualified |
| Sandbox claims | Policy UX mistaken for isolation | Call it a sandbox only after adversarial platform tests pass |
| Scope expansion | Backend absorbs Hermes product features | Keep product adapters above `qq-client` |
| Benchmark gaming | Startup/size improves while outcomes regress | Measure useful-result latency, reliability, cost, and resource use together |

## Architecture Review Questions

Before each new interface is accepted, answer:

1. What are the two real consumers that justify the seam?
2. What complexity does the module hide from its callers?
3. Can the behavior be compiled or cached outside the run hot path?
4. What is the queue, output, concurrency, retry, and shutdown bound?
5. What authority does the component receive?
6. What happens if it is slow, unavailable, invalid, or crashes?
7. Is its output authoritative, advisory, or observational?
8. How is the exact version/digest retained for replay and diagnosis?
9. What benchmark or evaluation proves its value?
10. Can the component be disabled without changing default-path behavior?

If only one hypothetical consumer exists, keep the behavior concrete. Extract
the interface when the second adapter makes the shared contract real.

## Definition Of Done

The speed-first extensible backend is complete when:

- QQ's full and minimal builds have enforced binary, startup, RSS, and latency
  budgets;
- long streaming and reasoning persistence meet linearity and fairness gates;
- effective model capabilities and core-owned run limits are durable and
  visible through every interface;
- the native protocol supports structured input, profiles, steering,
  capabilities, replay, and typed outcomes;
- a real external product can use QQ through a thin client without spawning a
  fresh process per turn or importing QQ internals;
- `CompiledAgentPlan` removes discovery/configuration work from warm runs and
  records reproducible plan identity;
- unused provider families and addon mechanisms impose no minimal-build or
  default-hot-path cost beyond accepted gates;
- agent packs, MCP, one embedded tool host, one context source, and one
  post-commit observer pass conformance and failure tests;
- every queue, task, process, retry, output, and concurrency dimension is
  bounded;
- terminal and sandbox behavior, if shipped, passes cleanup and adversarial
  tests on supported platforms;
- crash/restart never repeats uncertain side effects and every accepted run
  settles durably;
- same-model evaluations show that shipped tools, context, and delegation
  improve verified work per dollar and minute; and
- product integrations remain clients of one durable QQ runtime.

Until those conditions are met, the near-term implementation boundary is
Phase 0 plus the active readiness plan's linear streaming, fair persistence,
resolved model, context, and budget work. Plugin or marketplace work is not the
next slice.
