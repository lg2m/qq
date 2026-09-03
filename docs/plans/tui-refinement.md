# TUI Refinement

Status: proposed 2026-09-02. Supersedes the structural claims in
[`tui-rearchitecture.md`](./tui-rearchitecture.md): that plan's T0–T7 phases
shipped their features and speed budgets, but the T1 module split it promised
(lines 78–108 there) did not land. This plan finishes the structure, removes
what is wrong on screen, and adds the multi-agent surface that makes `qq`
better than Codex, OpenCode, pi, and fx rather than merely faster.

Scope is `crates/qq-tui` plus additive changes to `qq-protocol`, `qq-client`,
and `qq-server` listed in [Protocol Prerequisites](#protocol-prerequisites).
Backend work stays owned by
[`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md).

## Audit Summary

Four read-only audits ran against the crate as of commit `8abdced`: code
quality, architecture and performance, UI/UX, and a competitive comparison
against the `.source/` snapshots. Every P0 finding below was verified against
source before inclusion.

### Bench baseline (release, 160x48, 2000 samples)

| Case | median | p95 | Budget |
| --- | ---: | ---: | --- |
| steady_state_frame | 21.7 µs | 32.3 µs | ≤ 1 ms |
| steady_state_with_sidebar_frame | 37.2 µs | 39.2 µs | – |
| streaming_focused_delta_to_frame | 35.7 µs | 38.1 µs | ≤ 2 ms |
| streaming_run_on_32kb_delta_to_frame | 403.5 µs | 459.9 µs | ≤ 2 ms |
| steady_two_panes_frame | 50.5 µs | 68.1 µs | – |
| streaming_two_panes_delta_to_frame | 50.1 µs | 77.0 µs | ≤ 1.5x |
| streaming_background_8_delta_to_frame | 23.5 µs | 32.3 µs | ≤ 1.2x |
| children_20_with_sidebar_delta_to_frame | 54.6 µs | 58.0 µs | – |
| keystroke_to_frame | 25.0 µs | 26.3 µs | ≤ 4 ms |

Every gate passes. The frame hot path is not the problem. The bench fixtures
contain no tool calls, no open picker, no resize, and no startup timer, so the
expensive paths found below are unmeasured.

### What is sound and must be preserved

Retained `TranscriptCache` with a settled-prefix streaming tail; tree-sitter
off the render tick; row diff inside synchronized updates; the tiling pane
tree with stable ids and no stored geometry; warm-body LRU pinned to shown
panes; bounded channels everywhere; the loop injectable over a fake port and
event stream; themes with a live-preview picker.

### What is wrong

| Area | Finding | Location |
| --- | --- | --- |
| Structure | `app.rs` 6010 lines (52% inline tests), `view.rs` 5234 (46%); together 64% of the crate. `App` has 43 fields across nine concerns. | `app.rs:405-479` |
| Structure | The planned `model/`, `view/transcript.rs`, `view/chrome.rs`, `view/sidebar.rs`, `view/overlay.rs` do not exist. | plan L78-108 |
| Coupling | `(bool, Vec<ClientRequest>)` returned from ~100 sites; forced four extra effect side-channels (`take_requests`, `take_editor_request`, `take_attention`, `quit`). | `app.rs:544,550,1453` |
| Coupling | Nine copy-pasted `CommandId::generate` blocks; three (`set_session_model`, `delete_session`, `prune_sessions`) forget to register a `PendingIntent`, so failures are attributed to the focused session. | `app.rs:1793,2024,2038,1049-1078` |
| Coupling | Reducer sets notices, requests attention, and submits a network request (`flush_draft`) despite the plan's "no notice logic". | `reduce.rs:216,235,246,267,288,325` |
| Coupling | View mutates the model mid-render (`app.update_viewport`); `Viewport` stores render geometry. | `view.rs:772`, `panes.rs:101-106` |
| Coupling | Server semantics re-implemented client-side: message ordering, turn finalization inference, run-outcome→message-state. | `reduce.rs:436-457,474-483,295-321` |
| Coupling | 25 of 39 `CommandSpec`s have no slash and no action; their chords are `if` chains in `handle_compose_key`. The registry drives only slash autocomplete. | `commands.rs:90-364`, `app.rs:1150-1231` |
| Extensibility | New overlay touches nine places in four files; three duplicate picker key handlers and renderers. | `app.rs:1401,1720,1895` |
| Extensibility | `Pane { session, viewport }` hardwires pane = transcript. | `panes.rs:170` |
| Perf | Opening any picker or approval calls `prune_all` on every pane cache; closing costs a full relayout plus a highlight storm. | `view.rs:566-569` |
| Perf | `thread_order()` (O(S log S) rebuild) runs twice per frame; `child_spawned_by` O(S) twice per tool call per frame; `append_message_indices` O(M² + M·T log T) per frame. | `view.rs:1966,2340,1533,1558,1294-1311` |
| Perf | `serde_json::from_str` on tool arguments per call per frame. | `view.rs:1639,1734,2228,2256` |
| Perf | Any in-sequence event for any session returns redraw=true; eight background streams with the sidebar hidden rebuild an unchanged frame at 125 Hz. | `app.rs:994` |
| Perf | First frame waits on `server::reserve` and, embedded, on runtime open. The ≤ 30 ms startup gate has no test and is violated by construction. | `src/main.rs:316-374` |
| UX | 7–9 fixed chrome rows at 80x24 (29–38% of the screen): two header rows, a per-pane `THREADLINE …` banner plus blank, two footer rows, an optional notice row. | `view.rs:1057-1063,1891-1932,2617-2674` |
| UX | Fake caret: `"|"` iff `animation_tick % 2 == 0`, and the tick only advances during activity. Idle on an odd tick shows no cursor; the real cursor is hidden. Caret is spliced into the text before wrapping, so text right of it jitters. | `view.rs:2544-2559`, `app.rs:2635` |
| UX | Enter means send, steer, or queue depending on state; the prompt row is identical in all three. Approval mode is never shown. | `app.rs:2184-2196` |
| UX | Only `read_file`, `list_dir`, `search` have curated subjects. `edit_file`, `write_file`, `shell`, `spawn_agent`, and every MCP tool render as truncated JSON with the result's byte size as the metric. | `view.rs:1634-1655,1702` |
| UX | Tool detail is global; expanded output shows the tail, so a diff shows its last 12 lines. | `app.rs:1602`, `view.rs:1767-1786` |
| UX | Approval replaces the whole pane body; the composer stays drawn while `y` approves. | `view.rs:574,2192-2249` |
| UX | No help overlay, no palette, no `/help`. ~45 chords, none listed in-app. F1 is "Threadline layout". | `commands.rs:3`, `settings.rs:224` |
| UX | Ctrl-P is "previous layout" in compose and "prune every empty session" in the session picker. Ctrl-N is "next layout" and "create session". Ctrl-B is the tmux prefix. 13 core actions require Alt, dead by default on macOS terminals. | `settings.rs:226-227`, `app.rs:1946,1180` |
| UX | Two spinners (`◐◓◑◒` and `/ - \ \|`), two `RunActivity` label tables that disagree, ASCII session markers where `!` means both Interrupted and Failed, `+--` ASCII tree beside `↳`. | `view.rs:72,2277,1950,2402,2267-2278,1080` |
| UX | `muted()` is DarkGrey plus Dim; error tails are `failure().dim()`; syntax colors are hardcoded named colors outside the theme. | `render.rs:114,150-176`, `view.rs:1720` |
| UX | Sidebar auto-hides below 120 columns and nothing replaces it; sibling sessions that finish, fail, or need input are invisible at 80–119 columns. | `app.rs:313` |
| UX | No run completion line, no elapsed time on runs or tools, no unread state, no "new content below" indicator when scrolled up. | `view.rs:2759-2764` |
| UX | No timestamps anywhere. A `shell` call that has run for four minutes is indistinguishable from one that started four seconds ago, and a new tool row is indistinguishable from the same row the user was already waiting on. `MessageSnapshot.created_at_ms` and `SessionEventEnvelope.occurred_at_ms` are received and discarded. | `view.rs:1586-1612` |

## Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Rewrite or refactor | Refactor. Caches, panes, loop, and row diff stay. | Every speed gate passes; the failures are structure and surface. |
| Update shape | `App::update(Msg) -> SmallVec<[Effect; 2]>`; effects are `Redraw(Scope)`, `Send`, `Editor`, `Attention`, `Quit`. | One channel replaces five; scoped redraw stops background streams rebuilding invisible frames. |
| Frame purity | `frame(&Model, &Ui, &mut RenderState) -> Frame`; viewport clamps, caches, `theme_generation`, and the previous frame live in `RenderState`. | Headless frame assertions; no model write-back during render. |
| Reducer purity | `reduce(&mut Model, envelope) -> SmallVec<Effect>`; no notices, attention, or requests inside. | Matches the original plan; makes the reducer unit-testable. |
| Pane content | `PaneContent::Transcript(SessionId)` now; `Attention`, `Changes` later behind the same enum. | The differentiator panes are blocked without it. |
| Command surface | `COMMANDS` is the only source of chords. `handle_compose_key` contains no literal chords. Palette, help, footer hints, and slash all read the table. | The "one registry" decision becomes true. |
| Chrome | One top row, one bottom hint row. Notices overlay the hint row and never shift the body. Footer items configurable in `tui.ron`. | Reclaims 3–5 rows; stops viewport jumps on run start and stop. |
| Cursor | Real terminal cursor positioned after the frame; no fake caret. | Fixes the invisible-idle-cursor bug, IME placement, and text jitter. |
| Glyphs | One vocabulary for tools and sessions; one spinner; no ASCII markers; `ascii` theme flag maps every glyph. | Coherent design language. |
| Tool rows | `<glyph> <Verb> <subject> <metric> <duration>`; per-tool subjects; per-call expand via a transcript cursor; diffs head-first with line numbers. | The "review a change" slice must work from the transcript. |
| Time | Collapsed rows show relative duration only. Expanded detail (Ctrl-O, per-call expand, reasoning expand) shows wall-clock timestamps: message written, call started, call finished or a live elapsed clock. Timestamps are `HH:MM:SS` in local time, in `muted`, right-aligned. | Answers "is this new, or am I still waiting on the same thing?" without adding noise to the collapsed view. |
| Approval | Inline block under the tool row; composer disabled; transcript visible; answerable from the attention surface without focus change. | Decisions need context. |
| Keybindings | Ctrl-N/Ctrl-P return to readline; layout cycling moves to the palette; no chord is destructive in one mode and cosmetic in another; every Alt chord has a non-Alt path via the palette. | Predictability over cleverness. |
| Theme roles | Add `selection_bg`, `border`, `border_active`, `diff_add_bg`, `diff_del_bg`, `surface_alt`; syntax colors derive from roles. | Eight roles cannot express the UI being drawn. |

## Design Language

```text
GUTTER   4 columns: role glyph, state glyph, space, space. Content starts at col 4.
         Nesting (tool detail, child agent) indents +2 per level. No other widths.
GLYPHS   user ▌   assistant (none; the text is the mark)   system ·
         tool/session state: ○ requested  ◐◓◑◒ running  ● done  ✕ error
                             ◇ needs approval  ◌ interrupted  ▶ focused  ! attention
         one spinner (◐◓◑◒) at 8 fps; the resting glyph is ●.
ROLES    text, muted (no Dim), accent (user and interactive only), info (running),
         success, warning, error, brand, surface, surface_alt, selection_bg,
         border, border_active, diff_add_bg, diff_del_bg.
CHROME   top:    [brand] [breadcrumb] ………… [model] [ctx %] [cost] [conn if degraded]
         bottom: right-aligned context-sensitive hints from the registry:
                 "? help  / commands  ^O detail"; notices overlay this row.
         no section headers, no layout banners, no version string.
COMPOSER top rule (border) + prompt glyph encoding mode:
         ›  send     ↦ steer     ⇥ queue     ✎ approval pending (disabled)
         approval-mode chip (read-only / ask / auto) at the right of the rule.
TOOLS    ● Read    crates/auth/src/cache.rs         212 lines   0.1s
         ● Edit    crates/auth/src/cache.rs          +12 −3      0.2s
         ● Run     cargo test -p qq-auth             exit 0      41s
         ● Search  "TokenCache"                      14 hits · 3 files
         ● Spawn   survey callers                    ↳ 3 tools · running
         paths middle-elided; diffs head-first with line numbers; errors never dim.
         consecutive read-only calls fold into one row naming the files:
         ▸ Read ×4  cache.rs, refresh.rs, lib.rs, +1
TIME     collapsed: relative duration only (0.2s, 41s, 4m12s), live for running rows.
         expanded (Ctrl-O or per-call): wall-clock in muted, right-aligned:
         ◐ Run     cargo test -p qq-auth                    started 14:32:07 · 4m12s
              Compiling qq-core v0.3.0                      last output 14:36:01
         ● Edit    crates/auth/src/cache.rs   +12 −3        14:31:55 → 14:31:56
         QQ                                                 14:31:40
         ∴ thought for 4s                                   14:31:36 → 14:31:40
         a running row also shows "last output HH:MM:SS" when it has live output,
         so a stalled shell is visibly stalled. A date prefix appears only when
         the day differs from today.
RHYTHM   0 blank rows between a message and its tool group; 1 between turns;
         1 before ▌ user. Completion line after each run in success:
         ✓ 42s · 8 tools · 12.3k tok · $0.04 · ttft 420ms · 88 tok/s
PANES    title row only when >1 pane: "▶ title  ◐ running 41s". Divider in border.
SIDEBAR  grouped NEEDS YOU / WORKING / IDLE / DONE with counts; unread count per row.
         when hidden and >1 session, a one-row agent strip above the composer:
         "3 agents  ◐2  ◇1 (^G)  ✓1 unread"
FALLBACK `ascii` maps every glyph; 16-color palette when COLORTERM is absent.
```

## Target Module Layout

Sibling files plus directories; no `mod.rs`.

```text
lib.rs
terminal.rs            raw mode, TerminalGuard, sync writer thread over mpsc<Vec<u8>>, EventStream
loop.rs                run_loop: drain ready terminal events → App::update → apply effects →
                       frame on Redraw scope → row diff → writer
app.rs                 App { model, ui, panes }; update(Msg) -> Effects; thin dispatch only
app/tests.rs
model.rs               Model { store, pending, connection, workspace }
model/store.rs         SessionStore: sessions, order, depth, children, spawned_by_call,
                       generation counter, warm-body LRU
model/body.rs          SessionBody: messages, tool_calls, turn_index, reasoning, live tail,
                       per-session side tables (history, drafts, live tool output, previews)
model/pending.rs       PendingIntent map, answered approvals
reduce.rs              reduce(&mut Model, envelope) -> Effects; one arm per variant
ui.rs                  Ui { modes: ModeStack, composer, prefs, notice, esc_armed }
ui/mode.rs             Mode = Compose | Picker(PickerKind) | Approval(ToolCallId) | Confirm | Help
picker.rs              Picker<T: PickerItem>: items, filtered, selection, query; one key handler
commands.rs            Command, COMMANDS with chords; palette/help/footer/slash derivation
composer.rs            editor; composer/handle_key.rs owns every editing chord
panes.rs               Panes tree; Pane { content: PaneContent, scroll }
view.rs                frame(&Model, &Ui, &mut RenderState, size) -> Frame
view/render_state.rs   per-pane caches, tool-row cache, chrome cache, viewport clamps, previous frame
view/transcript.rs     VirtualBody, PlainTextIndex, TranscriptCache
view/tools.rs          tool rows, fold groups, per-tool subjects, diff renderer
view/chrome.rs         top row, hint row, composer, agent strip
view/sidebar.rs        grouped session tree with live status and unread
view/overlay.rs        picker, palette, help, inline approval block
view/markdown.rs, view/wrap.rs, view/highlight.rs   unchanged leaf renderers
view/tests.rs
fixtures.rs            #[cfg(any(test, feature = "bench-support"))] shared builders
render.rs              Style, Span, Line, style-delta emitter, row diff
theme.rs, settings.rs  extended roles; chords read from COMMANDS
```

## Speed Budgets

All existing gates in `tui-rearchitecture.md` remain. New gates:

| Gate | Target |
| --- | ---: |
| Frame with 32 tool calls in the visible run, steady state | ≤ 60 µs p95 |
| Frame with 200 sessions and the sidebar shown | ≤ 80 µs p95 |
| Picker or approval open then close | zero transcript relayout; zero highlight requests |
| Background delta to a session shown in no pane, sidebar hidden | zero frame builds |
| Horizontal resize of 64 messages | ≤ 1.5 ms; highlight requests deferred one tick |
| Process start to first `Connecting` frame | ≤ 30 ms, measured by a loop test and `qq --bench-startup` |
| Escape bytes per changed row | ≤ 50% of current after the style-delta emitter |

## Phases

### F0 — Mechanical Split

No behavior change. Land in an isolated worktree and merge within a day.

- Move `mod tests` out of `app.rs`, `view.rs`, `terminal.rs`, `markdown.rs`
  into sibling `tests.rs` files.
- Create `fixtures.rs` with `session_summary()`, `message()`, `tool_call()`,
  `workspace_snapshot()` builders and replace every hand-built literal (8 in
  `app.rs`, 6 in `view.rs`, 2 each in `terminal.rs` and `bench_support.rs`).
- Split `view.rs` into `view/transcript.rs`, `view/tools.rs`,
  `view/chrome.rs`, `view/sidebar.rs`, `view/overlay.rs` along existing
  function boundaries.
- Move per-session side tables onto `SessionView`; delete the four manual
  lifecycle mirrors (`app.rs:862-869, 951-957, 381-385, 599-601`).
- Delete `CommandSpec.category` unless F2 reads it; delete
  `Layout::previous`; replace `#[allow]` at `panes.rs:340` with `#[expect]`.
- Add bench fixtures: 32 tool calls, 200 sessions with sidebar, picker
  open/close, horizontal resize, a 100 KB scrolled message.

Acceptance: identical bench numbers; `app.rs` and `view.rs` each under 1200
lines; tests unchanged in count and pass.

### F1 — Effects, Purity, Indexes

- `Msg`, `Effect`, `Scope`; `App::update`. Delete `take_requests`,
  `take_editor_request`, `take_attention`, and the `quit` flag. Drain all
  ready terminal events before drawing; keep `Redraw::Immediate` semantics
  for keystrokes.
- `App::send(intent, build)` replaces the nine `CommandId::generate` blocks;
  every command registers its intent. Regression tests for the three
  misattributed failures.
- Reducer returns effects; no `set_*` notice calls inside. Envelope passed by
  value on the live path; the `RunFinished` arm split with one session lookup.
- `frame(&Model, &Ui, &mut RenderState)`; `Viewport` loses `body_rows` and
  `height`; `theme_generation` moves into `RenderState`.
- `SessionStore` maintains `order`, `depth`, `children`, `spawned_by_call`
  on mutation with a generation counter; `thread_order()` becomes a slice
  read. `SessionBody` maintains `TurnIndex`; `append_message_indices` reads
  it.
- Overlays no longer call `prune_all`; caches drop only by byte budget or
  theme change. `TranscriptCache` LRU becomes byte-bounded; `recent_events`
  gains a byte cap; `sessions` gains a bound with eviction of cold terminal
  sessions.
- Scoped dirty tracking: a delta to a session shown in no pane with the
  sidebar hidden produces no `Redraw`.

Acceptance: new bench gates for tool calls, sessions, picker cycle, and
background delta pass; `reduce.rs` has direct unit tests; loop tests assert
frames via a structured `Frame` API, not ANSI substrings.

### F2 — One Command Surface

- `Picker<T: PickerItem>` with one `handle_key` and one renderer; sessions,
  models, themes, commands, and help are item impls. Delete the three
  handlers and three renderers.
- `ModeStack`; `Overlay` variants collapse to `Mode::Picker(kind)`.
- Every chord moves into `COMMANDS`; `handle_compose_key` dispatches through
  `settings.action_for` then the table. Composer editing chords move into
  `Composer::handle_key` with one after-edit hook.
- Command palette (`Ctrl-K`, `/commands`) listing every command with its
  bound chord; help overlay (`?` when the composer is empty, `F1`, `/help`)
  is the palette grouped by category with no query.
- Rebind: Ctrl-N/Ctrl-P are readline next/previous line; layout switching is
  palette-only plus `/layout`; the session picker's prune moves to a
  confirm-guarded `/prune`; Ctrl-B moves to `Ctrl-\`; every Alt chord keeps
  working but is also reachable from the palette.
- Footer hints render the chord from the table; a rebind updates every hint.
- Slash matching becomes subsequence fuzzy, and the menu draws inside a
  bordered box anchored above the composer instead of stamping over body rows.

Acceptance: adding an overlay touches `PickerItem` impl plus one `Command`
row; a parity test asserts every `Command` has a palette title and either a
chord or a slash name.

### F3 — Chrome And Cursor

- Delete the `THREADLINE` / `FOLD / FOCUS` banner and its blank row.
- One top row and one hint row per the design language; footer items
  configurable via `tui.ron` `status_line: [Model, Context, Cost, Branch, …]`.
- Notices overlay the hint row with a level color; the body height never
  changes because of a notice or run state.
- Real terminal cursor: `Show` after each frame at the composer caret;
  `Hide` only while an overlay owns input. Delete the fake caret and the
  activity-gated blink.
- Composer prompt glyph encodes send/steer/queue/approval; approval-mode chip
  on the composer rule.
- One glyph vocabulary and one spinner; delete ASCII markers and `+--`.
  `muted()` loses `Dim`; error tails lose `dim()`; syntax colors derive from
  theme roles; add the six roles; OSC 11 dark/light detection at startup.
- Run completion line with duration, tool count, tokens, cost.
- `↓ N new` pill when scrolled up during streaming; `End` / `Ctrl-End`
  jumps to tail; `Home` to top.
- Mouse capture off by default; `/mouse` and `tui.ron` toggle it.

Acceptance: 80x24 idle shows ≥ 20 transcript rows; a `NO_COLOR` frame test
asserts every role pair remains distinguishable by attribute; a test asserts
the cursor position sequence follows the composer caret; frame tests for
every mode glyph.

### F4 — Tool Rows And Review

- `ToolRowCache` in `RenderState` keyed by `(call_id, state, result_len,
  width)`; `serde_json` parsing leaves the frame.
- Per-tool subject table: `read_file`, `list_dir`, `search`, `edit_file`,
  `write_file`, `shell`, `spawn_agent`, `web_fetch`, MCP fallback (`server ·
  tool · first string arg`). Metrics: line count, `+N −M` from
  `ToolCallDisplay::Diff`, exit code, hits · files, child tool count.
- Duration from `ToolCallSnapshot.started_at_ms` / `finished_at_ms` (protocol
  prerequisite); elapsed clock on running rows.
- Timestamps in expanded detail. Every expanded tool row shows
  `started HH:MM:SS` and either `→ finished HH:MM:SS` or a live elapsed
  clock; a running row with live output adds `last output HH:MM:SS` from the
  most recent `ToolOutputAppended` envelope's `occurred_at_ms`. Expanded
  assistant and user messages show `created_at_ms`; expanded reasoning shows
  its first and last delta times. Timestamps are local `HH:MM:SS`, `muted`,
  right-aligned in the row's remaining width, with a `MM-DD` prefix only when
  the day differs from now. Until the protocol timestamps land, the TUI
  records `occurred_at_ms` of the first `ToolCallStarted` / last
  `ToolOutputAppended` / `ToolCallFinished` envelope it sees per call in
  `SessionBody`, so live sessions get timestamps immediately; snapshots of
  historical calls show none rather than a wrong value.
- Read-only fold names files; a child in the group no longer defeats folding.
- Transcript cursor (`Ctrl-Up`/`Ctrl-Down` or click) selecting a tool row;
  `Enter` toggles that call's expansion; `Ctrl-O` remains the global toggle.
- Diff renderer: unified, line numbers, intra-line inverse for one-to-one
  changes, head-first, side-by-side above 120 columns; shared by tool rows
  and approval.
- Inline approval block under the tool row with the diff scrollable, choices
  `y once · a session · p prefix · n deny`; composer disabled; hint text
  once. Prefix allow replaces blanket workspace allow for `shell`.
- Middle-elide paths everywhere; OSC 8 hyperlinks on file subjects.

Acceptance: the 32-tool-call gate passes; a frame test for each tool kind at
collapsed and expanded levels; a test that an `edit_file` approval shows the
diff head at 80x24; a frame test that a running `shell` call expanded with
Ctrl-O shows `started`, live elapsed, and `last output`, that the elapsed
clock advances across animation ticks, and that the collapsed row shows no
wall-clock time.

### F5 — Multi-Agent Surface

- `PaneContent` enum; transcript is the first content; focus semantics are
  defined per content.
- Sidebar grouped NEEDS YOU / WORKING / IDLE / DONE with counts; unread
  count per row derived from deltas since the session was last focused;
  focused row uses `selection_bg`; sidebar rows are clickable.
- Agent strip above the composer when the sidebar is hidden and more than one
  session exists.
- Attention model: `needs_you` is approval, failed, or finished-unread;
  `Ctrl-G` cycles all attention items, not only approvals; the banner answers
  approvals in place with `y`/`n` while focus stays put.
- Deny-and-steer and approve-and-steer: `N` or `Y` opens a one-line
  amendment field; on Enter the decision is sent followed by
  `SteerRun { interrupt: true }` with the amendment.
- Inline child card under `spawn_agent`: `↳ title  ◐ current tool · N tools ·
  elapsed`, Enter opens the child in the focused pane, `Alt-Enter` in a
  split.

Acceptance: 200-session sidebar gate passes; a frame test at 100 columns
shows the strip when a sibling needs approval; a loop test answers an
approval for an unfocused session without changing focus.

### F6 — Differentiators

- Speed telemetry: `ttft` and `tok/s` on the completion line and sidebar
  from `RunSnapshot` timing fields (protocol prerequisite); frame p95 in
  `/status`.
- `PaneContent::Attention`: every approval, failed run, finished-unread run,
  and unread child across the workspace in priority order; Enter jumps,
  `y`/`n` answers in place.
- `PaneContent::Changes`: `ToolCallDisplay::Diff` payloads across live
  sessions grouped by path with per-agent `+N −M`; files touched by more than
  one agent flagged; Enter jumps to the editing call.
- Persistent named layouts: `Panes` tree plus session ids serialized to
  `.qq/layouts/<name>.ron`; `/layout save|load|list`; auto-restore on
  reconnect when the sessions still exist.
- Startup gate: `qq_tui::run` accepts a lazy port that reports `Connecting`
  immediately; frame one paints before `server::reserve`; loop test and
  `qq --bench-startup` enforce ≤ 30 ms.
- Style-delta emitter in `write_line`; sync writer thread replaces
  `tokio::io::stdout`; memoized `Line::width`; chrome and composer row caches;
  tail layout cache keyed by `(settled, len, width)`.
- `!` shell passthrough rendered as a `YOU RAN` cell; `Ctrl-R`
  reverse-i-search over prompt history.

Acceptance: startup gate test passes; escape-byte gate passes; a two-agent
fixture editing the same file shows a conflict flag in the changes pane.

## Protocol Prerequisites

Additive only; each is a separate H-track issue and lands before the phase
that consumes it.

| Addition | Consumer |
| --- | --- |
| `ToolCallSnapshot { started_at_ms, finished_at_ms }` | F4 durations and expanded timestamps for historical calls |
| `RunSnapshot { started_at_ms, first_token_at_ms, finished_at_ms }` and output token count | F3 completion line, F6 telemetry |
| Explicit `MessageCompleted` event so the client stops inferring turn finalization | F1 removes `complete_streamed_turns` |
| Canonical `SessionBody` reducer in `qq-client` for message ordering and run-outcome→message-state | F1 removes server-mirroring from `reduce.rs` |
| `ShellPrefix` grant computed server-side from the approval request, not from client argument parsing | F4 prefix allow |
| File-find endpoint | `@` mentions, after this plan |

## Priority

1. F0 and F1: the structure blocks every later phase, and the misattributed
   failure bug is live today.
2. F2 and F3: one command surface and honest chrome are the visible "this is
   designed" moment and remove the P0 usability bugs.
3. F4: tool rows are the weakest visible dimension against every reference.
4. F5: the multi-agent lead.
5. F6: differentiators once the foundation holds them.

## Risks

| Risk | Mitigation |
| --- | --- |
| F0 conflicts with concurrent edits to `app.rs` / `view.rs` | Isolated worktree; merge within a day; no behavior change so rebases are mechanical |
| `Effect` migration regresses request ordering | Loop tests assert the exact request sequence for create, submit, steer, approve |
| Removing `prune_all` grows memory | Byte-bounded LRU replaces entry-count LRU in the same change |
| Real cursor misplaces under wide characters or wrapped composer rows | Cursor position computed from the same `wrap_line` output that drew the row; tests for CJK and emoji |
| Rebinding chords upsets existing users | `tui.ron` migration note; old chords keep working for one release with a hint pointing to the palette |
| Protocol timestamps arrive late | F4 ships without duration; the column is absent, not zero |

## Verification

```sh
cargo bench -p qq-tui --bench render
cargo test -p qq-tui
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

## Definition Of Done

The refinement is complete when `app.rs` and `view.rs` are each under 1200
lines with no inline tests; `App::update` is the only entry point and returns
effects; the reducer and frame are pure and unit-tested; every chord lives in
`COMMANDS` and appears in the palette; the chrome is two rows; the cursor is
the terminal's; tool rows use the design-language grammar with per-call
expansion and head-first diffs; approvals are inline and answerable without
focus change; the sidebar groups by attention with unread counts; all speed
gates including the new ones pass; and `tui-rearchitecture.md` carries a note
pointing here for the structural work it did not finish.
