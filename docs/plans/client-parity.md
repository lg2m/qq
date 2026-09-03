# Client Parity

Status: active. Sequenced between Phase 4 and Phase 5 of
`speed-first-extensible-agent-harness.md`. Phase 5 (H10) stays gated on R6 and
a platform threat model; Phase 6 needs an actual client; Phase 7 qualifies the
TUI as a first-class path. None of them should start while the shipped clients
cannot drive the shipped backend.

## Problem

Phases 1–4 added agent profiles, packs, skills, approval modes, steering, run
limits, compaction rollback, per-turn audits, and a versioned capability
document. An audit of `crates/qq-tui`, `crates/qq-client`, and `src/headless.rs`
on 2026-09-03 found the clients reach roughly a third of it:

- `CreateSession` always sends `approval_mode: default()` and
  `profile: default()` (`crates/qq-tui/src/app.rs:1443-1445`,
  `src/headless.rs:475`). `SetApprovalMode`, `SetSessionProfile`, and
  `RollbackCompaction` are never sent by any client. Profiles and the
  `ReadOnly`/`Ask`/`Full` approval modes are unreachable from the TUI.
- `qq-client` fetches capabilities once with `workspace_id: None`
  (`crates/qq-client/src/interactive.rs:379`) — so the server omits `profiles`
  and `workspace_tools` — and forwards only `steering`. `ClientRequest` is
  `Command | Snapshot`; nothing can be re-fetched on demand.
- Data already on the wire is dropped: `ToolApprovalRequested.shell`
  (`app/reduce.rs:159`, the TUI reparses `arguments` JSON instead),
  `SessionCompacted.summary`, `RunStarted.plan`, `ModelTurnCompleted`,
  `RunContextUpdated`, `SessionSummary.profile`.
- Skills work (`/name` forwards to the runtime, the model may `load_skill`) but
  are undiscoverable: no listing, no completion, no per-profile view.
- `qq run` has no `--profile`, no steering, and cannot approve for a session or
  workspace.

## Non-Goals

- No new runtime behavior. Every item consumes an existing command, event, or
  capability field; the one protocol change is additive.
- No TUI redesign. New surfaces reuse `Picker<T>`/`Overlay`, the notice line,
  and the sidebar.
- Run-limit composer syntax, `InputPart::WorkspaceFile` attachment UI, and
  history paging are recorded as Tier 3 and not started until Tiers 1–2 land.

## Design Rules

- Capabilities are the single source for what the TUI offers. Pickers list what
  the server advertised; no client-side hardcoded profile or command tables.
- Everything the TUI shows about a session comes from `SessionSummary` and
  events, so a reconnect or second client renders identically.
- Missing capabilities degrade to today's behavior: an absent `profiles` list
  disables `/profile` with a notice, it never errors.
- Bounds: capability documents stay under `MAX_CAPABILITIES_BYTES` (256 KiB).
  The additive skill listing is capped by `MAX_INDEXED_SKILLS` (64) entries with
  descriptions already bounded at 512 bytes, so the worst case adds ~40 KiB.
- Each item lands as its own commit with a reducer/render test or a headless
  test; the workspace gates run before each commit.

## Tier 1 — Unlock Shipped Features

### T1.1 Capabilities plumbing (`qq-client`)

- Fetch `capabilities(Some(workspace_id))` in `load_tui_models`.
- Replace `ClientUpdate::Steering(SteeringCapabilities)` with
  `ClientUpdate::Capabilities(Arc<ServerCapabilities>)`; the TUI derives
  steering from it. `Arc` because the document is read-only and shared with the
  pickers.
- Add `ClientRequest::Capabilities` so the TUI can refresh after the workspace
  changes (pack edits are compiled lazily; the refresh is what makes a new
  profile appear without a restart).
- Acceptance: interactive port test proves the workspace-scoped document reaches
  the TUI with `profiles` populated; a failed fetch leaves steering unadvertised
  as today.

### T1.2 Profiles (`qq-tui`, `qq run`)

- `/profile` opens `Overlay::Profiles(Picker<ProfileRow>)` listing
  `capabilities.profiles`: id, approval mode, model override, `pack@version`.
  Accept with a focused idle session sends `SetSessionProfile`; accept with no
  session or with `Ctrl-N` sets the default for the next `CreateSession`.
  Running sessions get a notice (the runtime rejects it anyway; do not send).
- New sessions send the chosen profile; the sidebar and session header render
  `SessionSummary.profile` when it is not `default`.
- `SessionUpdated` already carries the new profile; the reducer upserts, so no
  new event handling.
- Reserve `/profile` in `RESERVED_CLIENT_SLASH_COMMANDS` (array length bump
  and the `commands.rs` parity test).
- `qq run --profile <name>` maps to `CreateSession.profile`; an unknown profile
  fails as a configuration error before any run starts.
- Acceptance: reducer test for picker → command; render test showing the
  profile badge; headless test with a pack profile proving the plan descriptor
  carries the pack.

### T1.3 Approval mode (`qq-tui`)

- `/approval` opens `Overlay::ApprovalModes` over `capabilities.approval_modes`.
  Accept sends `SetApprovalMode` for the focused session, or sets the default
  for new sessions when none is focused.
- Sidebar renders the session's current mode next to the profile badge.
- Reserve `/approval`.
- Acceptance: reducer + render tests; the picker is empty and says so when the
  document is absent.

### T1.4 Skill and command discovery (`qq-protocol`, root, `qq-tui`)

- Additive field `SkillCapabilities.entries: Vec<SkillSummary { name, kind,
  source, description, disclosed }>` with `#[serde(default)]`; populated from
  `SkillIndex::entries()` in `src/runtime.rs`. The document tolerates unknown
  fields and `additive fields do not bump` the version (protocol.md:348), so
  `CAPABILITIES_VERSION` and `PROTOCOL_VERSION` stay put. Add a v14 golden.
- `/skills` opens `Overlay::Skills(Picker<SkillRow>)` listing entries with kind
  and source; Accept puts `/name ` in the composer (commands) or sends
  `/name` as a prompt (skills), matching how the runtime resolves them today.
- Slash completion in the composer includes workspace commands and skills after
  the reserved client commands, sourced from the same list. Reserved names win
  on collision, as the runtime already guarantees.
- Reserve `/skills`.
- Acceptance: golden fixture; render test for the picker; completion test that
  a pack skill appears for its profile's session.

## Tier 2 — Render What Is Already On The Wire

- T2.1 `ToolApprovalRequested.shell` preview drives the approval card; delete
  the `arguments` re-parse in `view/tools.rs`.
- T2.2 `SessionCompacted.summary` excerpt appears in the compaction notice and
  the transcript marker.
- T2.3 `/rollback` sends `RollbackCompaction` for an idle focused session; the
  existing `SessionCompactionRolledBack` handling renders the result.
- T2.4 `ModelTurnCompleted` feeds a per-run turn count and cumulative cost/usage
  into the run stats line; `RunContextUpdated` feeds the context meter between
  `SessionContextUpdated` events.
- T2.5 `RunStarted.plan` and `RunSnapshot.resolved_model`/`plan` show in the
  session header (profile, model actually used, plan digest prefix).
- T2.6 `qq run`: `--approve-for-session` and `--approve-for-workspace` map onto
  `ApprovalDecision::{ApproveForSession, ApproveForWorkspace}`; steering via
  stdin lines while a run is active.

## Tier 3 — New Surface (not started)

- Run limits from the composer or `/limits`.
- `InputPart::WorkspaceFile` attachments from the composer.
- "Load older" paging using `has_older_sessions`/`has_older_messages`.
- Per-field `LimitCapabilities` enforcement client-side (input bytes, pending
  prompts) instead of the TUI's own constants.

## Verification

Per item: the narrowest crate test, then before each commit
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace`. T1.4 also runs
`cargo test -p qq-protocol --test wire_fixtures`. Render latency: run
`cargo bench -p qq-tui` if a bench exists for the affected view; otherwise the
change must not add work to the per-event reduce path beyond a field copy.

## Receipts

Record each landed item here with its commit and the test that covers it.
