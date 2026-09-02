# TUI Rearchitecture

Status: proposed 2026-09-02. Phases T0 and T1 are complete (receipts below).
Phases T2–T7 are proposed.

This plan makes the `qq` TUI the fastest visible surface among the audited
harnesses while making it possible to create sessions instantly, watch an agent
spawn many children, drive everything from one command surface, and eventually
show more than one transcript at a time.

Scope is `crates/qq-tui` plus narrow, additive changes to `qq-protocol`,
`qq-client`, and `qq-server`. Backend work remains owned by
[`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md)
(H tasks) and [`terminal-bench-readiness.md`](./terminal-bench-readiness.md)
(R tasks). This plan consumes H3 steering and capability discovery when they
land; it does not redefine them.

## Audit Summary

The design is based on read-only audits of the four ignored reference
snapshots under `.source/` and the current `qq-tui` implementation. Snapshot
identities and manifest hashes are recorded in the harness plan and are not
repeated here.

| Feature | Codex | OpenCode | Pi | fx | QQ today |
| --- | --- | --- | --- | --- | --- |
| Renderer | ratatui; inline viewport plus scrollback insertion | OpenTUI/Solid; alt-screen; 60 fps dirty | `string[]` line diff; main screen default, alt screen opt-in | Inline; cell surface plus verified shadow VT diff | Hand-rolled `Style`/`Span`/`Line`; alt-screen row diff; whole frame model rebuilt per frame |
| Event loop | `TuiEvent` enum, `AppEvent` bus, per-thread channels | SSE micro-batched 16 ms into one reactive batch | 16 ms throttle; keyboard bypasses throttle | 8 ms poll, fact collection, input-pending frame abort | `select!` plus 8 ms dirty tick; blocking `terminal::size()` every frame |
| Composer | Textarea, paste burst, history, `@` mentions, vim, external editor | Extmarks for pastes/images, history, stash, `@` `/` `!` modes | 2.3k-line editor, kill ring, undo, paste markers | Kill ring, undo, paste placeholders, `@` file index, `$` skills | 159-line buffer; paste; history |
| Command menu | Frequency-ordered `SlashCommand` enum; generic `ListSelectionView` | One command registry drives keybinding, palette, slash, and hint | `SelectList` in editor slot; `if` chain dispatch | Typed router; inline picker; categorized `/help` | Seven `const` slash commands index-coupled to protocol; three duplicate pickers |
| Approvals | Overlay plus banner for inactive threads | Inline; child approvals bubble to root | Extension only | Inline plus alt-screen diff review | Modal for the focused session only |
| Busy input | Queue and steer | Server queue with `QUEUED` badge | Enter steers; Alt+Enter queues | Enter queues; Ctrl+Enter steers; queue review | Optimistic pending row |
| Multi-session | Per-thread bounded event buffers, spawn-tree walk, `/agents`, spawn-order navigation | All sessions live in one store; quick slots; child navigation; task row opens child | One session; `/tree` branching | Ctrl+X manager screen with per-child composer | One loaded body; children are one-line rows; events for unfocused sessions dropped |
| Reasoning | Shown | Collapsible | Toggle | Dropped | Dropped |
| Themes | Syntax theme picker | 33 JSON themes, system detection, hot reload | JSON, OSC 11, hot reload | Dark/light detection | None (design doc only) |

### What Blocks The Goals Today

| Goal | Blocker | Location |
| --- | --- | --- |
| Fast session creation | Three racing focus signals (`CommandResult`, `SessionCreated`, auto `SnapshotRequest`); focus change evicts every other body | `app.rs:339-345`, `app.rs:619-628`, `app.rs:542-545`, `qq-client/src/interactive.rs:574-590` |
| Agent spawns N children | `SessionCreated` carries no spawning run or tool call; child deltas dropped when `messages.is_none()`; `thread_order()` is O(S²) | `qq-protocol/src/sessions.rs:894-896`, `app.rs:938-944`, `app.rs:2116-2134` |
| Command palette | No mode or overlay enum; if-chains in key handling, frame assembly, and cache policy; nothing shared between pickers | `app.rs:1119-1216`, `view.rs:646-657`, `view.rs:682-685` |
| Concurrent visible sessions | Single `focused` drives transcript, approvals, notices, footers, cancel, and scroll; snapshot protocol loads exactly one body | `app.rs:231`, `app.rs:198-203`, `qq-protocol/src/sessions.rs:857` |
| Speed | Full frame model rebuilt per frame; tree-sitter on the render tick; per-frame `ioctl`; linear scans on every delta | `view.rs:618-679`, `view.rs:801`, `view.rs:582`, `app.rs:945-999` |

## Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Screen model | Keep the alternate screen with an app-owned viewport | The only model that supports split panes. With retained rendering its steady-state cost equals inline's O(changed rows) while avoiding scroll-region and reflow complexity. |
| Render library | Keep the hand-rolled `Style`/`Span`/`Line` types and row diff; no ratatui | Minimal dependencies; row diff is already correct. Add in-row changed-span emission only if measured. |
| Frame model | Retained per-message line cache and per-pane damage; rebuild only the streaming tail and chrome | Removes the per-frame O(visible transcript) rebuild. |
| Default view | One focused transcript plus a live session sidebar; split panes available | The sidebar is cheap and always live; panes need per-pane state and arrive after the store supports them. |
| Busy input | Enter steers when the server advertises steering, otherwise queues; Alt+Enter queues explicitly; Esc Esc interrupts | Designed for steering now; availability is capability-driven, never inferred. |
| Command surface | One `Command` registry drives keybindings, palette, slash commands, and footer hints | Removes the three duplicate pickers and the index-coupled slash table. |
| Protocol | Additive only: spawn linkage, per-session body fetch, activity in snapshots | Enough for children and warm multi-body without a redesign. |

## Speed Budgets

These are TUI-owned gates enforced by the benchmarks and loop tests added in
T0. Provider and store latency are excluded; the client port is a fake.

| Gate | Target |
| --- | ---: |
| Keystroke to frame bytes written (render only, no tick delay) | `<= 4 ms` p95 |
| Committed delta to frame | `<= 25 ms` p95; `<= 60 ms` p99 |
| Frame build, 64 visible completed messages, steady state | `<= 1 ms` p95 |
| Frame build with one streaming message | `<= 2 ms` p95 |
| Process start to first frame in `Connecting` state | `<= 30 ms`; must not wait for server readiness |
| Session create to focused empty transcript | Zero requests before first paint; one frame |
| Switch to a warm session body | One frame; zero requests |
| Frame cost with eight background streaming sessions | `<= 1.2x` single-session cost |
| Tree-sitter and heavy markdown | Never on the render tick |

Baselines are recorded in the T0 receipt below before any refactor.

## Target Module Layout

`crates/qq-tui/src`, sibling files plus directories, no `mod.rs`:

```text
lib.rs
terminal.rs            raw mode, event stream, run loop over an injected event source
app.rs                 App: owns Model, ModeStack, Registry; thin dispatch
model.rs               workspace model
model/sessions.rs      SessionStore: summaries, adjacency index, order cache, warm-body LRU, live status
model/transcript.rs    SessionBody: messages, tool calls, live output, indexes
model/pending.rs       optimistic intents; pending approvals across the workspace
reduce.rs              SessionEvent -> model mutations; one arm per variant; no notice logic
commands.rs            Command enum and metadata table; slash, keybind, palette derivation
input.rs               ModeStack (Compose | Palette | Approval | Confirm | Search); key dispatch
picker.rs              Picker<T>: query, filtered indices, selection, viewport, categories
composer.rs            editor; composer/kill_ring.rs, composer/paste.rs, composer/history.rs
view.rs                frame assembly from panes and chrome
view/panes.rs          Pane layout (single | vertical split); per-pane viewport and TranscriptCache
view/transcript.rs     retained message rendering, streaming tail, tool-call grouping
view/sidebar.rs        session tree with live status
view/chrome.rs         header, footer, notices, approval banner
view/overlay.rs        palette, picker, and approval rendering from the ModeStack top
view/markdown.rs, view/code.rs, view/diff.rs, view/wrap.rs   leaf renderers split out of view.rs
render.rs              Style, Span, Line, Surface, row diff, writer
theme.rs               theme roles per docs/design/theme.md (T7)
settings.rs            unchanged
```

`app.rs` and `view.rs` are split mechanically first; tests move with the code
they cover.

## Phases

### T0 — Measure First

Deliverables:

- Move `completed_transcript_render_benchmark` (`view.rs`, `#[ignore]`) to
  `crates/qq-tui/benches/render.rs` with cases: steady 64 completed messages,
  one streaming message, eight background streaming sessions, and keystroke
  echo.
- Factor `terminal::run` over an injected terminal event stream and a fake
  `ClientPort` so the loop is testable without a TTY. Add tests for the dirty
  flag, request drain, `ResetSnapshot`, and gap detection to `Replaying`.
- Record baseline numbers in this document.

Acceptance:

- `cargo bench -p qq-tui` runs the render benchmark.
- The loop tests exist and pass without a terminal.
- The baseline table below is filled from a release-mode run.

#### T0 Baseline — 2026-09-02

`cargo bench -p qq-tui --bench render` on `linux-x86_64-local` (AMD Ryzen 9
9950X, 32 logical CPUs, Rust 1.97.1), working tree at `5bb1471` plus the T0
changes, 160x48 terminal, 2,000 samples per case after 200 warmups. Two runs
agreed within noise; the worse run is recorded.

| Case | Median | p95 | p99 |
| --- | ---: | ---: | ---: |
| Steady state, first frame (64 completed messages, cold caches) | 12.1 ms | n/a | n/a |
| Steady state, no changes | 18.6 µs | 21.8 µs | 30.8 µs |
| One streaming message, one delta per frame | 855 µs | 1,028 µs | 1,435 µs |
| Eight background sessions, one delta each per frame | 17.8 µs | 18.7 µs | 20.5 µs |
| Keystroke to frame | 31.5 µs | 41.6 µs | 48.9 µs |

Observations that shape T2:

- The streaming frame costs roughly fifty times the steady frame. The live
  message is pushed through markdown layout on every delta and the entire
  frame model is rebuilt, so the cost scales with the live message length
  rather than the delta size.
- The eight-background case is cheaper than steady state because deltas for
  unfocused sessions are currently dropped when no body is loaded. This
  number is the floor for T3, which must reduce live status for every session;
  the plan's `<= 1.2x` gate is measured against the single-session steady
  frame once that work lands.
- The first frame pays 12 ms of markdown and highlight work for 64 messages on
  the render tick. T2 moves highlighting off the tick and caches per message.

The loop tests in `crates/qq-tui/src/terminal.rs` now drive `run_loop` with an
injected event stream, in-memory output, fixed size, and a fake `ClientPort`,
covering the initial frame, dirty-only redraw, request drain, send-failure
surfacing, gap detection to `Replaying`, `ResetSnapshot` recovery, and client
stop. The renderer no longer queries the terminal itself; the loop supplies the
size through an injected source. In production that source is still
`crossterm::terminal::size()` once per drawn frame; caching it from `Resize`
events remains a T2 item.

### T1 — Structure: Modes, Command Registry, Generic Picker

No user-visible behavior change.

Deliverables:

- `Command` enum with a table of name, title, category, slash aliases, default
  `KeyChord`, and `available(&Model) -> bool`. The palette (Ctrl+P) and `/`
  autocomplete both read the table. `RESERVED_CLIENT_SLASH_COMMANDS` in
  `qq-protocol` remains authoritative; a parity test replaces index coupling.
- `ModeStack` replaces the if-chains in key handling, frame assembly, and
  cache policy. Overlay rendering and cache policy derive from the stack top.
- `Picker<T>` replaces `ModelPicker`, `SessionPicker`, and slash autocomplete
  with categories, a current marker, footer actions, and selection preserved
  across refresh.
- `app.rs` and `view.rs` split into the target layout.

Acceptance:

- Existing tests pass with unchanged behavior.
- The three pickers share one implementation.
- Adding a command touches one table row.

#### T1 Completion Receipt — 2026-09-02

Landed without user-visible behavior change; all 135 pre-existing tests pass
unmodified in intent, plus seven new unit tests.

- `commands.rs`: `Command` enum and `COMMANDS` table (title, category, slash
  aliases, bound `Action`). `App::execute(Command)` is the single dispatch
  point for keybindings, slash entries, and Ctrl-C/Ctrl-O. A parity test
  asserts the table's slash names equal
  `qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS` as sets, replacing the old
  index coupling.
- `input.rs`: `Overlay { Models, Sessions }` and `Mode { Models, Sessions,
  Approval, Compose }`. `App::mode()` replaces the if-chains in
  `handle_key`, `frame`, and `prune_markdown`; the approval prompt stays
  derived from session data.
- `picker.rs`: one `Picker` (query, clamped cursor, bounded query bytes,
  case-insensitive match, `preserve` across refresh) shared by the model
  picker, session picker, and slash autocomplete.
- Module split, no `mod.rs`: `render.rs` (Style/Span/Line, writer),
  `view/markdown.rs` (markdown, tables, code panels, tree-sitter),
  `view/wrap.rs` (wrapping, truncation, viewport slicing), `app/reduce.rs`
  (`reduce_event` and transcript mutation). Markdown and wrap tests moved
  with their code. `view.rs` went from 5,258 to 3,645 lines and `app.rs`
  from 4,343 to 3,830.
- Render bench after T1 matched the T0 baseline within noise once slash
  autocomplete stopped rebuilding its list on ordinary typing
  (keystroke-to-frame p95 41.4 µs vs 41.6 µs baseline).

Deferred from T1 to T2: removing the remaining `expect`s in the approval
renderer and the transcript viewport path, and moving reduce-focused tests
out of `app.rs`.

### T2 — Hot Path

Deliverables:

- Retained `TranscriptCache` per pane: completed messages cached by
  `(MessageId, width)` with LRU eviction; only the streaming message tail and
  chrome are rebuilt per frame; message lookup is indexed and the viewport
  path has no `expect`.
- Tree-sitter highlighting runs in `spawn_blocking` with a bounded in-flight
  count; text renders plain immediately and upgrades when the highlighted
  result arrives.
- Terminal size is cached from `Resize`; the per-frame `terminal::size()`
  call is removed.
- Key events request an immediate frame instead of waiting for the 8 ms tick;
  client updates drain fully before a frame; `TextAppended` coalesces per
  message per frame.
- Per-session indexes replace linear `message_mut`, `upsert_tool_call`, and
  `push_message`; an adjacency index maintained on upsert and remove replaces
  the O(S²) `thread_order`.
- `Line::width()` is memoized on the line.

Acceptance:

- The T0 benchmarks meet the budget table.
- Frame cost with eight background streams is `<= 1.2x` single-session cost.

### T3 — Session Store, Fast Create, Warm Bodies

Protocol additions are versioned and additive with unknown-field fixtures.

Deliverables:

- Protocol: `SessionSummary.spawned_by: Option<SpawnOrigin { run_id,
  tool_call_id }>` (core already persists `parent_run_id`);
  `SessionSnapshotRequest { session_id, message_limit }` returning one
  `SessionSnapshot` without affecting other loaded bodies; `RunActivity`
  included in snapshots; `SessionUpdated` covers title and approval mode.
  Server, `qq-client`, and fixtures updated.
- `SessionStore`: bounded warm-body LRU (default eight; visible panes are
  pinned). Every session reduces a cheap `LiveStatus` (status, active tool,
  last assistant tail `<= 256` bytes, pending approvals, elapsed) regardless
  of warmth, so the sidebar is live for all children; full deltas apply only
  to warm bodies. A bounded per-session replay ring with a byte cap supports
  promotion into the warm set.
- Fast create: one focus source. `/new` inserts an optimistic `SessionView`
  with an empty body, focus moves in the same frame, and `SessionCreated`
  swaps in the real identifier. The client auto-snapshot after create is
  removed because the body is known empty. A missing model catalog does not
  block creation; the configured default is used.
- Fast switch: warm bodies need zero requests; cold bodies show the summary
  and live tail immediately and fill on `SessionSnapshot` arrival without
  evicting the previous body.
- Sidebar: session tree with depth, status glyph, live tail, and cost; toggle
  key; shown automatically above a width threshold.

Acceptance:

- Create and warm switch issue zero requests before first paint (asserted
  with the fake port).
- Twenty concurrent children render live status.
- Memory is bounded by the warm LRU and live-status caps.

### T4 — Children And Approvals Across The Workspace

Deliverables:

- Threadline: a `spawn_agent` tool-call row shows its linked child via
  `spawned_by` with status, live tail, and cost; Enter focuses the child.
- Navigation commands: parent, first child, next and previous sibling in
  stable spawn order; `/agents` picker via `Picker<T>`.
- Workspace-wide pending approvals: a banner for non-focused sessions with a
  jump key; child approvals bubble to the visible ancestor; the approval
  modal becomes a `Mode`.

Acceptance:

- Children spawned by a run are visible inline and in the sidebar within one
  frame of `SessionCreated`.
- Approvals in background sessions are surfaced and answerable without losing
  focus context.

### T5 — Composer And Busy Input

Deliverables:

- Composer: word motions, kill ring and yank, undo, bounded
  `[Pasted N lines]` placeholders expanded on submit.
- Busy input: `Submit` steers when the capability document advertises steering
  and otherwise queues with a `QUEUED` badge; `Queue` (Alt+Enter) is explicit;
  queued prompts are listed above the composer with edit and dequeue (Alt+Up);
  Esc Esc within two seconds cancels with a hint. Steering wires to H3
  commands when they exist; until then the command is present but
  unavailable.
- Reasoning renders as a collapsed `Thinking… (n s)` row with a toggle.
- External `$EDITOR` support with terminal suspend and resume (local only).

Acceptance:

- Editor behaviors are tested.
- The queued-prompt list is bounded.
- Steering availability is capability-driven with a fixture.

### T6 — Split Panes

Deliverables:

- `Layout::Split { left, right }` with per-pane `TranscriptCache` and viewport;
  pane focus cycling; pinned panes stay warm; approvals and footers derive from
  the focused pane; per-pane scroll state replaces the single viewport.

Acceptance:

- Two panes streaming concurrently stay within `1.5x` single-pane frame cost.
- Resize re-lays panes without full cache invalidation.

### T7 — Polish

Deliverables:

- Themes per [`docs/design/theme.md`](../design/theme.md): roles, discovery,
  live preview picker.
- Attention: bell or desktop notification on approval or run finish when the
  terminal is unfocused.
- Update the stale Ratatui note in
  [`docs/design/transcript.md`](../design/transcript.md) and the TUI paragraph
  in [`docs/design/architecture.md`](../design/architecture.md).

`@` file mentions require a server-side file-find endpoint because the TUI must
not discover workspace files itself. That is a protocol follow-up outside this
plan's protocol scope.

## Priority

1. T0, T1, T2: speed is non-negotiable and every later phase depends on modes,
   the registry, and retained rendering.
2. T3: fast create, warm multi-body, and the live sidebar are the core
   many-agent experience.
3. T4: children inline and workspace approvals.
4. T5: steer and queue plus composer depth.
5. T6 split panes, then T7 polish.

## Risks

| Risk | Mitigation |
| --- | --- |
| Splitting `app.rs` and `view.rs` while other agents edit them | Do T1 in an isolated worktree and land it quickly |
| Warm-body LRU shows a partial body as complete | Promotion replays from the bounded ring or refetches; never mark a body loaded until the snapshot arrives |
| Protocol additions break older clients | Additive fields only, unknown-field tolerance fixtures, versioned wire types |
| T5 invents a second steering contract | Steering semantics belong to H3; T5 only consumes them |
| Benchmarks measure isolated code paths | Loop tests measure keystroke and delta to frame bytes end to end with a fake port |

## Verification

For each phase run the narrowest relevant tests while iterating, then:

```sh
cargo bench -p qq-tui --bench render
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

## Definition Of Done

The rearchitecture is complete when every speed budget is enforced by a
benchmark or loop test, sessions can be created and switched without a request
before first paint, an agent spawning many children is visible live in the
sidebar and inline, one command registry drives every command surface, split
panes work within their frame budget, and the linked Linear issues describe the
shipped result.
