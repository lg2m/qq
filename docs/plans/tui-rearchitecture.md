# TUI Rearchitecture

Status: complete 2026-09-02. Every phase T0–T7 has landed (receipts below),
including the T5 steering work that waited on H3. The module split in
[Target Module Layout](#target-module-layout) was not carried out; `app.rs`
and `view.rs` remain monolithic. That structural work, plus the UI and
multi-agent surface built on it, is owned by
[`tui-refinement.md`](./tui-refinement.md).

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
| Busy input | Enter steers when the server advertises steering, otherwise queues; Alt-S interrupts and steers; Ctrl-Enter queues explicitly; Esc Esc cancels | Designed for steering now; availability is capability-driven, never inferred. |
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
panes.rs               Tiling pane tree: splits, focus, zoom, resize, geometry; per-pane viewport
view/transcript.rs     retained message rendering, streaming tail, tool-call grouping
view/sidebar.rs        session tree with live status
view/chrome.rs         header, footer, notices, approval banner
view/overlay.rs        palette, picker, and approval rendering from the ModeStack top
view/markdown.rs, view/code.rs, view/diff.rs, view/wrap.rs   leaf renderers split out of view.rs
render.rs              Style, Span, Line, Surface, row diff, writer
theme.rs               Theme, Palette, and the per-frame active palette (docs/design/theme.md)
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

#### T2 Completion Receipt — 2026-09-02

Commits `ee54796`, `2f135d6`, and the indexing/eviction commit that follows.
All 149 tests pass; workspace fmt, Clippy, test, and build gates green.

| Case | T0 baseline (median / p95) | After T2 (median / p95) |
| --- | ---: | ---: |
| First frame, 64 completed code-bearing messages | 12.1 ms | **0.80 ms** plain; fully highlighted 13.2 ms later off-tick |
| Steady state, no changes | 18.6 µs / 21.8 µs | 22.0 µs / 24.8 µs |
| One streaming prose message, one delta per frame | 855 µs / 1,028 µs | **34.5 µs / 36.6 µs** |
| Run-on 32 KiB paragraph, no block boundary (new ceiling case) | 855 µs | 415 µs / 468 µs |
| Eight background sessions, one delta each | 17.8 µs / 18.7 µs | 20.3 µs / 20.8 µs |
| Keystroke to frame | 31.5 µs / 41.6 µs | **23.9 µs / 25.8 µs** |

Every budget in the table above is met: streaming `<= 2 ms` p95, steady
`<= 1 ms` p95, keystroke `<= 4 ms` p95, eight-background `<= 1.2x` steady
(0.84x; still a floor until T3 reduces live status for unfocused sessions).
The +3 µs on steady state is the cost of the highlight bookkeeping and LRU
stamping per frame; accepted for the 15x first-frame win.

What changed:

- **Settled-prefix live cache.** `settled_prefix_end` finds the last blank
  line outside a fence or indented code block; a corpus test proves
  `markdown_lines(prefix) ++ markdown_lines(suffix) == markdown_lines(whole)`.
  A streaming message re-lays-out only its open trailing block. `wrap_line`
  and `wrap_line_chars` now slice span text by byte range instead of
  allocating one `String` per character, which halved the run-on ceiling.
- **Off-tick highlighting.** `view/highlight.rs` schedules tree-sitter on the
  Tokio blocking pool (four in flight, bounded result channel). Completed
  messages cache plain immediately; results are installed by
  `HighlightKey` so stale work is dropped. Loop test proves the plain frame
  precedes the highlighted one through the real `select!`.
- **Loop.** Terminal size is cached from `Resize`. User input draws
  immediately (`Redraw::Immediate`); client updates coalesce to the tick
  (`Redraw::Scheduled`).
- **Indexes and eviction.** `thread_order` groups children by parent in one
  pass (was O(S²)); `children_of` replaces the view-side scan; per-delta
  lookups scan from the tail; markdown cache eviction is LRU by frame clock.
- **No `expect` on the frame path.** Viewport and message-body lookups
  degrade to blank rows and recover next frame.

Deferred: `Line::width()` memoization (measured immaterial after the wrap
rewrite; revisit if a profile shows it), `TextAppended` per-frame coalescing
(the settled-prefix cache made per-delta cost flat, so batching buys little),
and moving reduce-focused tests out of `app.rs` (cosmetic).

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

#### T3 Completion Receipt — 2026-09-02

Commits `181597e` (protocol and core) and the TUI/client commit that follows.
All workspace gates green; 154 TUI tests, 23 protocol tests, 324 core tests.

Protocol version 12 (additive, every field defaults when absent):

- `SessionSummary.spawned_by: Option<SpawnOrigin { run_id, tool_call_id }>`.
  Core threads the spawning `ToolCallId` through `SubagentSpawner::spawn` and
  persists it in `sessions.spawned_by_tool_call_id` (schema version 20).
  Historical children report the owner run with no call.
- `SessionSummary.activity: Option<RunActivity>`, read from the newest
  `run_activity_changed` for the active run at snapshot time.
- `SnapshotRequest.include_sessions` (at most 16) and
  `WorkspaceSnapshot.included`. Foreign or unknown ids are skipped; over-limit
  requests fail with `InvalidPageLimit`.

TUI session store:

- `SessionView` bodies are warm (`Some`) or cold (`None`). Loading a body no
  longer evicts every other body; `evict_cold_bodies` keeps the eight most
  recently focused (`WARM_BODY_LIMIT`), never the focused one.
- `LiveStatus` (256-byte collapsed assistant tail, active tool, awaiting
  approval set) reduces from every event for every session, warm or cold.
  Activity seeds from the summary and is replaced by live events.
- Fast create: a `SessionCreated` for this client's `PendingIntent::Create`
  adopts the session as warm-and-empty and moves focus in the same frame,
  whether the SSE event or the HTTP receipt arrives first. The client's
  auto-snapshot after create is removed. Zero requests are asserted.
- Fast switch: warm sessions focus with zero requests; cold sessions show
  their summary and live tail while one focused snapshot fills the body.
  Bootstrap pre-warms the four most recent other sessions via
  `include_sessions`.
- Sidebar (`Ctrl-B`; auto at 120 columns): the session tree with a status
  row per session showing approval waits, the active tool, the live tail, or
  the run activity label. Body width is `width - 36` when visible so caches
  key on one stable width per layout.

| Case | median / p95 |
| --- | ---: |
| Steady state (sidebar hidden, unchanged from T2) | 22.3 µs / 32.5 µs |
| Steady state with sidebar, nine sessions | 34.3 µs / 50.4 µs |
| Eight background sessions streaming (now reducing live status) | 21.5 µs / 22.4 µs |
| Twenty children streaming with the sidebar visible | 51.9 µs / 54.8 µs |
| Keystroke to frame | 24.0 µs / 28.4 µs |

The eight-background case stays at 0.97x the steady frame, within the 1.2x
gate, now that the deltas do real work. The sidebar costs about 12 µs per
frame for nine rows; twenty streaming children cost 30 µs over steady.

Deferred to T4: rendering `spawned_by` inline under the `spawn_agent` call,
parent/child navigation commands, and workspace-wide approval routing.

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

#### T4 Completion Receipt — 2026-09-02

All workspace gates green; 158 TUI tests. No render-bench change.

- **Inline children.** `render_tool_calls` takes a child-rows callback; a
  `spawn_agent` call whose `ToolCallId` matches a session's `spawned_by`
  renders that child directly beneath it (`↳ title`, then the same live
  status line the sidebar uses). A run with an inline child never folds into
  the "N tool calls" summary, and the child is not repeated in the "related
  sessions" list. Children without a recorded call (pre-version-12 stores)
  keep appearing in that list.
- **Navigation.** `FocusParent` / `FocusFirstChild` / `FocusNextSibling` /
  `FocusPreviousSibling` on Alt-Up/Down/Left/Right; siblings are ordered by
  `updated_at_ms` (spawn order) and wrap. `/agents` (reserved in the
  protocol list alongside `/sessions`) opens the session picker scoped to the
  focused session's root and its descendants; the header reads `AGENTS`.
- **Workspace-wide approvals.** `sessions_awaiting_approval` derives from
  `LiveStatus`, so it covers cold sessions. A status-area banner names the
  first waiting non-focused session and how many more; `Ctrl-G`
  (`FocusNextApproval`) jumps to the next one in tree order, after which the
  existing approval mode answers it. Approvals are therefore answerable from
  any session with one keystroke of context switch.

Not done, deliberately: answering a background approval *without* switching
focus. The modal reads the focused session's pending call; making it
session-addressable is a small change but the jump-then-answer flow keeps the
user looking at the diff or command they are approving, which is the safer
default. Revisit with T6 split panes, where the second pane can host it.
Cost on the inline child row is also deferred: `SessionAccounting` is on the
summary but the row is already dense; the sidebar row is the better home once
it gains a cost column.

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

#### T5 Completion Receipt — 2026-09-02

All workspace gates green; 168 TUI tests. Render bench unchanged.

- **Composer.** Ctrl-Left/Right word motion, Home/End and Ctrl-A/E, Ctrl-W
  and Alt-Backspace kill word, Ctrl-K/U kill to line end/start, Ctrl-Y yank
  from a one-slot kill ring, Ctrl-Z/Ctrl-_ undo (64 snapshots, coalesced on
  word boundaries). Pastes of three or more lines or 512+ bytes collapse to
  `[Pasted #n N lines]`; the placeholder is one token for cursor motion and
  deletion, and `Composer::expanded()` substitutes the content on submit.
  The 64 KiB input bound applies to the expanded text.
- **Busy input.** Enter during an active run holds the draft locally instead
  of sending it; Ctrl-Enter (or Ctrl-Q) queues explicitly; Alt-Up pulls the
  newest draft back for editing (queueing any current text first). Drafts
  render above the composer, are capped at eight per session, and flush one
  per `RunFinished` in order so each becomes its own run. Local drafts were
  chosen over the server queue because the server queue is not editable and
  the user was typing to *this* run's outcome.
- **Esc.** With a run active, Esc arms and shows "press Esc again to cancel";
  a second Esc within 16 animation ticks (2 s) cancels. Any other key disarms.
  Without a run, Esc still dismisses errors then walks to the parent. Alt-Up
  therefore moved off tree navigation; parent is Esc, the rest stay on
  Alt-Down/Left/Right.
- **Steering.** `Command::SteerRun` exists in the table; `steering_available`
  is `false` until the capability document (H3) sets it, so the command warns
  and falls back to queueing. Tested as a fixture of that fallback.
  *Completed after H3 landed (protocol 13); see the addendum below.*
- **Reasoning.** `ReasoningDelta` accumulates per run (16 KiB bound, warm
  sessions only, dropped when the run's messages are trimmed). A collapsed
  `∴ thought for Ns  <first paragraph>` row precedes the run's first
  assistant message; Ctrl-R expands to the full text under a `┆` rail. It
  never enters `MessageSnapshot.output`.
- **External editor.** Alt-E or `/editor` (reserved in the protocol list)
  hands the expanded draft to `$VISUAL`/`$EDITOR` via a temp file. The loop
  runs the editor inline (nothing else can use the TTY meanwhile), leaves raw
  mode and input modes for its lifetime, then repaints every row. Missing
  editor, non-zero exit, and I/O failures each surface as a typed
  `EditorError` warning with the draft intact. The loop test injects a
  scripted editor.

#### T5 Addendum: Steering — 2026-09-02

Landed once H3 shipped `steer_run` and `POST /v1/capabilities` (protocol 13).
The TUI consumes the contract; it defines none of it.

- **Capability-driven.** The client fetches the capability document on the
  same background task as the model catalog (never gating first paint, and
  restarted with it after a recovery) and forwards `ClientUpdate::Steering`.
  `App.steering: Option<SteeringCapabilities>` stays `None` until then, which
  reads as unavailable: Enter holds the draft, and the steering commands say
  why they queued instead. `boundary` and `interrupt` are gated separately, so
  a server that steers but cannot interrupt turns Alt-S into a queue with its
  own explanation rather than a plain steer the user did not ask for.
- **Commands.** Enter during a run sends `steer_run` with `interrupt: false`.
  `Command::InterruptRun` (new `Action::InterruptRun`, default `Alt-S`,
  configurable as `interrupt_run` in `tui.ron`) sends `interrupt: true`. Both
  use the expanded draft, record it in prompt history, and disarm a pending
  Esc-Esc. Ctrl-X cancel and Ctrl-Enter queue are unchanged.
- **Optimism with a receipt.** The draft shows as `YOU / PENDING` under
  `PendingIntent::Steer` until the receipt. A rejection (`400`, over the
  per-run bound, offline) returns the text to an empty composer with the
  error. A `run_already_finished` receipt is a success that applied nothing,
  so it also restores the draft with a warning instead of the generic "run
  already finished". `steering_queued` clears the pending row; the transcript
  row arrives through the event so it is durable before it is shown.
- **Transcript.** A `steering: true` user row keeps the `▌ YOU` prefix but
  labels its lifecycle in words the run's own messages never use:
  `steering  waiting for the next turn` (queued), `steered` (applied),
  `steering  run finished first` (superseded). It never shows the bare
  `queued` of a queued prompt, which would read as a new run waiting its turn.
- **Tests.** Fallbacks for no document and for boundary-only servers; Enter
  and Alt-S request shapes; empty draft and idle session send nothing and
  Enter still submits when idle; refusal, late receipt, and success each
  settle the pending row correctly; steering row labels at every state; the
  `interrupt_run` binding round-trips through `tui.ron`. 203 TUI tests, 54
  config tests. Render bench unchanged.

### T6 — Split Panes

Deliverables (revised at implementation from a fixed left/right pair to a
tiling tree, at the user's request):

- A binary pane tree (`panes.rs`): any pane can be split side by side or
  stacked to any depth (bounded at 16), closed back onto its sibling, zoomed,
  and resized; focus moves geometrically. Each pane has its own viewport and
  `TranscriptCache`; sessions shown in any pane stay warm; approvals,
  footers, composer, and tree navigation derive from the focused pane.

Acceptance:

- Two panes streaming concurrently stay within `1.5x` single-pane frame cost.
- Resize re-lays panes without full cache invalidation.

#### T6 Completion Receipt — 2026-09-02

All workspace gates green; 191 TUI tests (12 tree/geometry, 7 app, 4 view).

- **Model.** `Panes` owns a `Node` tree of `Split { axis, ratio }` and
  `Leaf(PaneId)` plus a `PaneId -> Pane { session, viewport }` map. Ids are
  stable across every mutation so per-pane render state survives splits and
  closes. The tree stores no geometry: `layout(rect)` computes tiles and
  dividers each frame, so a resize never touches the tree. A split that
  cannot fit two readable panes (24x4) shows only the side on the focus path
  and reappears when room returns; zoom shows only the focused pane. Tiles
  are remembered for mouse hit-testing.
- **Focus.** `App.focused: Option<SessionId>` became `App::focused()` reading
  the focused pane's session, so every focus-dependent surface (composer,
  approvals, footers, breadcrumb, drafts, Alt-arrows tree navigation,
  pickers) targets the focused pane without a second code path. A new pane
  inherits the focused session (tmux/vim behaviour) and takes focus, so a
  split never requests anything.
- **Warmth.** `evict_cold_bodies` pins every session shown in any pane;
  `apply_snapshot` installs a body for any shown session and only moves focus
  when the user is still on that pane (or nothing is focused); a late body
  for a session no pane shows is dropped as before. Session deletion
  repoints every pane showing it and fetches only if the replacement is
  cold. `ResetSnapshot` clears pane sessions and keeps the tree shape.
- **Rendering.** `FrameRenderer` keeps the row diff and the shared
  `Highlighter`; the markdown/live/anchor state moved into a
  `TranscriptCache` per pane. Highlight results fan out to every cache with
  a matching key. Panes are composed row by row in one pass, moving each
  pane's spans into place and truncating any row that would cross a divider;
  a single pane filling the body takes the pre-T6 path untouched. Multiple
  panes get a one-row title (focus marker, session title, live status).
  Caches for panes that are closed or hidden this frame are dropped.
- **Commands.** `SplitBeside` (Alt-\, `/split`), `SplitBelow` (Alt--,
  `/stack`), `ClosePane` (Alt-W, `/close`), `ZoomPane` (Alt-Z, `/zoom`),
  `FocusPane{Left,Down,Up,Right}` (Alt-H/J/K/L), `ResizePane*`
  (Alt-Shift-H/J/K/L, 5% steps clamped to 15–85%). Mouse wheel scrolls the
  pane under the cursor; a click focuses it. Four slash names were added to
  `RESERVED_CLIENT_SLASH_COMMANDS` (additive; protocol version unchanged).
- **Bench.** `streaming_two_panes_delta_to_frame` median 51.0 µs against
  `streaming_focused_delta_to_frame` 36.2 µs: **1.41x**, inside the 1.5x
  budget. Single-pane cases are unchanged from the T5 baseline (steady 20.4,
  keystroke 25.3, sidebar 37.3, background-8 24.3 µs). The two-pane cost is
  dominated by walking two 64-message bodies, not composition; a
  `steady_two_panes_frame` case (51.1 µs) records that floor.
- **Deferred.** Per-pane layout mode (Threadline/FoldFocus is still global);
  moving a pane within the tree; swapping two panes; saving the pane tree
  across restarts.

### T7 — Polish

Deliverables:

- Themes per [`docs/design/theme.md`](../design/theme.md): roles, discovery,
  live preview picker.
- Attention: bell or desktop notification on approval or run finish when the
  terminal is unfocused.
- Update the stale Ratatui note in
  [`docs/design/transcript.md`](../design/transcript.md) and the TUI paragraph
  in [`docs/design/architecture.md`](../design/architecture.md).

#### T7 Completion Receipt — 2026-09-02

All workspace gates green; 198 TUI tests, 52 config tests. Render bench
unchanged from T6 (steady 20.5, keystroke 26.0, two panes 51.5 µs).

- **Theme documents.** `qq-config/theme.rs` loads `<name>.ron` from the
  compiled set, then the global `themes/`, then project `.qq/themes/`
  nearest-last; `defs` aliases expand with cycle detection; every documented
  failure (unknown name, version, missing role, unknown alias, bad hex,
  cycle, unknown field) is a typed `ConfigError` before the TUI starts.
  `tui.ron` gains an optional `theme` with provenance (`qq config explain
  tui.theme`, `qq config show`, `qq config check` all cover it).
  `discover_themes` enumerates the catalog for the picker, skipping broken
  files so one experiment cannot hide the list.
- **Runtime model.** `qq-tui/theme.rs` holds `Theme { name, palette }` and a
  `Copy` `Palette`. The `render.rs` role helpers, called hundreds of times a
  frame, read a thread-local the renderer sets at the top of `frame()`: one
  store per frame, no lock, no theme parameter threaded through leaves, and
  no measurable bench change. Cached message rows bake colors in, so a
  `theme_generation` counter on `App` makes the renderer drop every pane
  cache and the row diff when the theme changes; the next frame repaints
  every row (tested). The root converts config colors to `qq_tui::ThemeColor`
  and builds themes with `Theme::from_roles`, so no terminal library type
  crosses the crate boundary. The compiled `qq` theme keeps the terminal's
  named colors so it follows the user's terminal palette; files are
  `#RRGGBB` only.
- **Picker.** `/theme` opens `Overlay::Themes`; Up/Down and typing preview
  the highlighted theme immediately, Enter keeps it and shows the `tui.ron`
  line to persist it, Esc restores the theme active when the picker opened.
  Rows carry a swatch of the theme's roles in its own colors. With only the
  compiled theme available the command is a notice instead. The design doc's
  "no in-TUI picker" line was superseded by this plan; `theme.md` now
  documents the picker and keeps write-back out of scope.
- **Attention.** The loop enables focus-change reporting; `FocusGained` /
  `FocusLost` track `terminal_focused` on `App`. The reducer requests
  attention on `ToolApprovalRequested` (awaiting) and `RunFinished` only
  while unfocused; regaining focus discards anything undelivered. The loop
  takes at most one request per iteration and writes BEL followed by an
  OSC 9 notification (`ESC ] 9 ; text BEL`) — shown by iTerm2, WezTerm,
  kitty, ghostty, and Windows Terminal, ignored elsewhere — with the session
  title scrubbed of control characters and capped at 200 chars so it cannot
  break out of the sequence. Loop test asserts the exact bytes and that a
  focused terminal is never rung. No new dependencies.
- **Docs.** `transcript.md` Ratatui note replaced with the hand-rolled
  primitives and row diff; `architecture.md` `qq-tui` paragraph describes
  retained rendering, the pane tree, off-tick highlighting, the command
  registry, and themes; README documents `theme`, `/theme`, pane keys, and
  the attention behaviour.

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
