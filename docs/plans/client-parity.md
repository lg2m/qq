# Client Parity

Status: Tiers 1 and 2 complete 2026-09-03; Tier 3 not started. Sequenced between Phase 4 and Phase 5 of
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

### Tier 1 — 2026-09-03

| Item | Commit | Behavior | Evidence |
| --- | --- | --- | --- |
| T1.1 | `87e70ab` | `qq-client` fetches capabilities with the workspace id and forwards the whole document as `ClientUpdate::Capabilities(Arc<..>)`; `ClientRequest::Capabilities` re-fetches on demand; TUI derives steering from it | `runtime::tests::tui_client_delivers_workspace_capabilities_and_refreshes_them` drives `TuiClient` against a real server: initial document carries `profiles`, a pack trusted afterwards appears on refresh |
| T1.2 | `fcc9c77` | `/profile` picker over advertised profiles (mode, model, `pack@version`); Enter sends `SetSessionProfile` for the focused idle session, refuses a running one locally, or sets the default for new sessions; `Profile` status item shows `as <name>`; `qq run --profile` validates before any session exists and records `profile` in the trial record | TUI: `profile_picker_*` (4 tests), `top_row_names_a_non_default_profile_only`; headless: `the_selected_profile_reaches_the_loader_and_the_trial_record`; live: `qq run --profile reviewer --format jsonl` records `"profile":"reviewer"` |
| T1.3 | `93ade0d` | **Protocol 15**: `SessionSummary.approval_mode` (defaults to `auto` on decode so persisted events replay); `set_approval_mode` publishes `session_updated`; `/approval` picker with per-mode meaning; `ApprovalMode` status item names anything other than `auto` (warning style for `full`); protocol.md mode table corrected (`auto` is the default, `full` exists) | core: `auto_mode_executes_mutating_tools_after_a_mode_change` asserts the summary event and receipt cursor; TUI: `approval_picker_*`, `approval_mode_chosen_without_a_focused_session_*`, render `approval_mode_picker_and_badge_*`; goldens `fixtures/v15/`; harbor fixtures regenerated |
| T1.4 | `f6e9e49` | Additive `SkillCapabilities.entries`; slash completion lists client commands then workspace commands/skills; command accept leaves `/name ` for arguments, skill accept submits `/name`; `/skills` picker grouped by kind with sources and `explicit only` marks | commands: `workspace_guidance_joins_the_list_after_client_commands`; TUI: `slash_completion_offers_workspace_commands_and_skills_*`, `skills_picker_*`; render `skills_picker_groups_commands_before_skills_with_sources`; e2e asserts a real `SKILL.md` description arrives |

Reserved client slash commands: 16 → 19 (`/profile`, `/approval`, `/skills`).
Workspace guidance can no longer take those names; `docs/design/tools.md` and
the `design-skill-or-pack` skill reference the constant instead of a list.

Python is absent on the development host, so
`benchmarks/harbor/tests/test_atif.py` was not run after the fixture
regeneration; the Rust `harbor_atif_fixtures` test covers the shape.

### Tier 2 — 2026-09-03

| Item | Commit | Behavior | Evidence |
| --- | --- | --- | --- |
| T2.1 | `88f6ab7` | Approval card renders the server's `ShellCommandPreview` (command, then `(in cwd)`); the arguments re-parse is deleted; shell and edit previews share one per-call `ApprovalPreview` | `shell_approvals_show_the_server_preview_not_the_arguments` feeds a preview that differs from the arguments |
| T2.2 | `4b8b81d` | Compaction notice ends with a 96-char excerpt of the model's summary; absent or blank summary adds nothing | `session_compacted_events_surface_the_shrink_in_the_status_line` (extended) |
| T2.3 | `4b8b81d` | `/rollback` sends `RollbackCompaction` for the focused idle session; receipt distinguishes full restore from `N earlier retained`; server refusal surfaces as the failure notice; running session refused locally; reserved (20 entries) | `rollback_sends_for_an_idle_session_and_reports_the_receipt` |
| T2.4 | `41ca6c9` | `ModelTurnCompleted` advances `RunStats.turns` and `live_cost_usd_nanos`; composer rule shows `turn N  $x.xx` during a run; completion line adds `N turns` when >1. Also fixed `rule_with` eating one guaranteed rule glyph | `the_composer_rule_carries_run_telemetry_*` (extended), `a_finished_run_ends_with_a_completion_line_*` (extended) |
| T2.5 | `7657a8f` | Completion line names `as <profile> · plan <8 hex>` (or `plan <8 hex>` for default) from `RunStarted.plan`/snapshot, and `on <route>` when `ModelTurnCompleted.model` differs from the session's selection | `the_completion_line_names_the_plan_and_an_overridden_route` |
| T2.6 | `9f9842e` | `qq run --allow-tool`/`--allow-shell` answer held calls with `ApproveForSession` grants (word-boundary prefix rule via now-public `qq_core::shell_prefix_matches`); `--steer-stdin` injects stdin lines as `SteerRun` at the next boundary, bounded buffer of 8, refusals are warnings | `shell_allowlist_grants_the_session_on_first_hold`, `tool_allowlist_approves_held_calls_under_auto`, `stdin_steering_lines_reach_the_next_model_turn`; live: `printf '…' \| qq run --steer-stdin` |

Deliberately unchanged: `RunContextUpdated` stays a no-op because
`SessionSummary.context_tokens` is authoritative for the meter and old
persisted run-level events must not repopulate it. `--approve-for-workspace`
was not added to `qq run`: promoting a grant into workspace configuration from
an unattended run is a policy decision a human should make in the TUI, where
`WorkspaceGrantPromoted` reports the outcome.
