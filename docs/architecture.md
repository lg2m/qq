# QQ Architecture

Status: initial direction

## Purpose

QQ is a local-first agent harness for querying LLMs and using them to inspect,
modify, build, and test software. It will support interactive terminal use,
non-interactive automation, and long-running remote sessions without splitting
those use cases into separate products.

The architecture is ordered by two product priorities:

1. Speed and resource efficiency.
2. Developer friendliness.

Correctness, durability, and safe tool execution are baseline constraints. A
faster system that loses history or corrupts a workspace is not useful.

## Initial System Shape

QQ ships as one Rust binary named `qq`.

```text
TUI / CLI client
       |
       | HTTP commands + SSE events
       v
QQ server
  |-- agent runtime
  |-- model client
  |-- tool executor
  `-- SQLite store
```

The binary has multiple process modes:

- `qq` opens the TUI scoped to the current working directory. By default it
  starts a local server runtime in the same process and communicates with it
  through the same HTTP/SSE interface used by remote clients.
- `qq serve [ARGS]` runs the server without a TUI. It is suitable for a
  persistent process on a desktop or home server.
- `qq ask PROMPT` is the initial direct, automation-oriented path. It streams
  one model response to stdout through the same core runtime that the server
  will use.
- Additional direct CLI commands must reuse the same runtime rather than create
  another agent implementation.

Keeping the TUI and server in one executable provides a zero-setup local path
while still allowing several TUI or future browser clients to attach to a
long-running server. Agents and sessions belong to the server, so they may
continue when a client disconnects.

## Repository Layout

QQ is a Cargo workspace whose root package builds the `qq` binary. Library
crates live under `crates/`, while repository automation lives in `xtask/`.

The initial workspace is:

```text
Cargo.toml
src/
  main.rs
  cli.rs
  config.rs
  server.rs
crates/
  qq-core/
    Cargo.toml
    src/lib.rs
  qq-provider/
    Cargo.toml
    src/lib.rs
  qq-protocol/
    Cargo.toml
    src/lib.rs
  qq-tui/
    Cargo.toml
    src/lib.rs
xtask/
  Cargo.toml
  src/main.rs
```

- The root `qq` package is the executable and composition root. It owns process
  startup, top-level CLI dispatch, configuration loading, and the HTTP/SSE
  server adapter.
- `qq-core` contains the agent loop, session behavior, tool integration, and
  persistence behavior. It consumes the command and event vocabulary from
  `qq-protocol` and exposes a small interface that hides orchestration details
  from clients.
- `qq-provider` contains the provider-neutral model interface and concrete
  model-provider adapters.
- `qq-protocol` contains shared identifiers, commands, events, and versioned
  wire types. It does not depend on an HTTP client or server framework.
- `qq-tui` contains terminal rendering, input handling, and client-side state.
  It communicates through the protocol and does not depend directly on
  `qq-core`.
- `xtask` contains repository maintenance tasks and is not shipped as part of
  QQ.

The dependency graph points toward `qq-protocol` and `qq-provider`: `qq-core`
depends on both, `qq-tui` depends on `qq-protocol`, and the root package wires
the modules together. Application configuration types must not become a shared
dependency imported throughout the workspace; the root translates external
configuration into each module's settings.

Do not create additional placeholder crates for storage, tools, individual
providers, the server, plugins, web, or mobile. A module should become a crate
only when a measured build concern or multiple real consumers justify the
seam. In particular, extract a shared client only when both the TUI and direct
CLI commands need the same HTTP/SSE behavior.

## Runtime

QQ uses stable Rust with Tokio as its async runtime and Clap for command-line
parsing. Dependencies are added only for implemented behavior. Prefer a small,
well-understood dependency over a framework or abstraction stack.

The server owns session state and schedules work using bounded Tokio tasks and
channels. Every long-running operation must support cancellation. Model calls,
tool output, persistence, and client delivery must apply backpressure rather
than create unbounded queues.

## Provider Compilation

Provider names are configuration presets, not runtime dispatch keys. The root
package translates layered configuration into a `qq-provider` recipe, and the
provider compiler validates that recipe before returning the single
`Provider::stream` interface consumed by `qq-core`.

```text
provider configuration
        |
        v
typed provider recipe
        |
        v
ProviderCompiler -- shared HTTP pool
        |
        v
configured Provider::stream
```

A recipe separates deployment identity from its wire protocol, endpoint mode,
and authentication intent. Built-in and custom deployments compile through the
same path. Base endpoints append protocol path segments; exact endpoints are
never rewritten. Invalid protocol/authentication combinations fail during
compilation rather than during a model request.

Provider compilation follows these performance rules:

- One `ProviderCompiler` and HTTP connection pool are shared by every model in
  a runtime factory.
- Provider configuration, URLs, headers, and protocol choices are validated
  once and remain immutable while streaming.
- Shared providers pass directly into `qq-core`; they are not boxed and then
  wrapped again.
- Immutable model identifiers use shared storage so each command does not
  allocate another model string.
- Provider identity must not cause branching in the request hot path.

Protocol codecs, request-time authorization, framing, retry policy, and
transport are internal implementation details. Add a public seam only when two
real adapters require it. A new deployment over an existing protocol should
normally require configuration only; a new protocol should add one codec and
its contract fixtures without changing `qq-core`.

Run `cargo bench -p qq-provider --bench provider_compiler` to measure compiled
recipe construction independently from provider network latency. End-to-end
startup and time-to-first-token benchmarks remain the primary performance
signals.

One run follows a simple loop:

1. Accept and validate a user command.
2. Persist the command.
3. Assemble model context and request a response.
4. Execute requested tools under the session's workspace policy.
5. Persist resulting messages and events before publishing them.
6. Repeat until completion, cancellation, or failure.

This ordering makes persisted state authoritative and allows clients to resume
an event stream without losing output.

## HTTP And SSE Protocol

Clients issue versioned HTTP requests with JSON bodies. The server streams
ordered events using Server-Sent Events (SSE). HTTP keep-alive and one
long-lived SSE connection per attached client avoid repeated connection setup.

The initial protocol needs operations equivalent to:

```text
POST /v1/sessions
GET  /v1/sessions/{session_id}
POST /v1/sessions/{session_id}/messages
POST /v1/runs/{run_id}/cancel
POST /v1/approvals/{approval_id}
GET  /v1/sessions/{session_id}/events
```

The final resource names belong in a protocol specification. These routes only
establish the required behaviors.

Every streamed event has:

- A monotonically increasing event ID within its stream.
- A session ID and, when applicable, a run ID.
- A stable event type.
- A versioned JSON payload.

Clients reconnect with `Last-Event-ID`; the server replays persisted events
after that ID before switching to live delivery. Heartbeats keep idle streams
detectable. Mutating requests carry request IDs or idempotency keys so retries
cannot accidentally duplicate work.

SSE is intentionally server-to-client. Client commands, approvals, and input
remain normal HTTP requests. Do not add GraphQL, raw TCP, gRPC, WebRTC, or
WebSocket initially. WebRTC is especially unnecessary because Tailscale
already provides private connectivity and NAT traversal. WebSocket may be
considered later only if an implemented feature, such as a full interactive
PTY, cannot be expressed cleanly through HTTP and SSE.

JSON is the initial wire format. Binary serialization should replace it only
after profiling demonstrates that serialization or bandwidth is material.

## Persistence

SQLite is the initial and default store. It provides fast local durability,
transactions, simple deployment, and no external service. Use WAL mode and
keep blocking database work off Tokio executor threads, preferably behind a
small storage module using a dedicated thread or bounded blocking work.

The store must preserve at least:

- Sessions and their workspace identity.
- User, assistant, and tool messages.
- Runs and terminal outcomes.
- Ordered events required for SSE replay.
- Model/provider metadata needed to explain and resume a session.

Schema migrations are part of the binary. Chat history must survive process
restarts, and a failed write must not be presented to clients as durable. Do
not introduce an external database until measurements show SQLite is the
bottleneck.

## Workspaces And Tools

The server executes tools on the machine where it runs. In local `qq` mode,
the workspace defaults to the canonical current working directory. Tool paths
must remain within the selected workspace unless the user explicitly grants
wider access.

The first useful tool set is deliberately small:

- Read files and directories.
- Search file names and contents.
- Apply explicit file changes.
- Execute bounded shell commands.

Tool calls and results are persisted and streamed so the user can understand
what the agent did. Destructive or externally visible operations require an
approval policy; the exact policy belongs in a separate tool-execution design.

A remote server can initially operate only on workspaces available on that
server. A hosted coordinator plus outbound-connected desktop workers is a
possible later architecture, but it is not part of the initial implementation.

## Concurrency And Multiple Agents

Parallel model requests are mechanically simple; useful parallel agents are
not. The server must eventually account for rate limits, token budgets,
cancellation, duplicate work, context exchange, and conflicting changes.

Initial concurrency should therefore be bounded and session-aware. Multiple
independent sessions may run concurrently, but two writing agents must not
modify the same checkout concurrently. When editing subagents are introduced,
each receives an isolated Git worktree or sandbox and returns a patch for
central review and integration. Read-only research agents may be parallelized
earlier.

Do not build an agent swarm, distributed scheduler, or worktree coordinator in
the initial version.

## Local And Remote Networking

The server binds to loopback by default. Binding to a Tailscale address or
another non-loopback interface must be explicit. Tailscale supplies encrypted
private networking and device-level access controls, but remote command
execution still needs an application authentication and authorization decision
before it is enabled broadly.

The same HTTP/SSE protocol serves local TUI clients, remote TUI clients, and
future browser or mobile clients. Protocol replay means moving between devices
does not require transferring in-memory client state.

## Performance Discipline

Optimize end-to-end time to a useful result, not isolated microbenchmarks.
Measure at least startup time, command acknowledgement, time to first model
token, tool execution, persistence latency, reconnect/replay time, memory, and
render responsiveness.

Keep hot paths direct, queues bounded, and interfaces small. Avoid speculative
abstractions and serialization layers. Any complexity introduced for speed
must be supported by a benchmark and must not make routine development hostile.

## Intentionally Deferred

The initial repository is pure Rust. Do not create or scaffold any of the
following yet:

- React or other web frontend.
- Native or cross-platform mobile application.
- JavaScript/TypeScript packages or package workspace.
- Separate server executable.
- Distributed workers or cloud control plane.
- Plugin marketplace or public extension interface.
- Multi-user tenancy.
- Multi-agent editing orchestration.

The HTTP/SSE server is designed to permit future clients, but future client
code must not shape the initial crate graph before it exists.
