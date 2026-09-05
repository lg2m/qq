# Plans

Active plans only. Shipped plans are deleted; their receipts live in Git
history and their durable contracts in [`docs/design/`](../design/). Read
[`../design/architecture.md`](../design/architecture.md) before changing
system boundaries.

## Priority Order

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 1 | Phase 6 — H18–H22 per-run copies, SSE framing, control admission, store consolidation, bundled fixes | [`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md) | Moves the 1 MiB heap, scaling, cancellation, and cold `plan_for` gates; carries the eight-stream service-gap remainder (29–45 ms vs the 20 ms target) and the `notify` cleanup from Phase 5; −1,500 LOC |
| 2 | D6b paired evaluation (paid runs) and the default decisions it feeds | [`supervised-delegation.md`](./supervised-delegation.md) | Decides delegation depth and worker-model defaults with evidence |
| 3 | Phase 6 tool tournament and terminal; Phase 7 sub-agent economics; Phase 8 remaining warm-path candidates | [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) | Evaluation-gated; feeds H10 |
| 4 | Phase 7 — H10 process sandbox | `speed-first-…` | Gated on R6 and a platform threat model |
| 5 | Phase 8 — H11 product adapters; Phase 9 — H12 qualification | `speed-first-…` | H11 needs a real consumer; H12 closes the story |
| — | Run snapshots | [`run-snapshots.md`](./run-snapshots.md) | Proposed; no scheduled slice |
| — | LSP diagnostics | [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed; MCP-first validation before native work |

## Ownership

| Concern | Owner |
| --- | --- |
| Compiled plan, protocol contract, extension lanes, store/provider hot path, perf gates and budgets | `speed-first-extensible-agent-harness.md` |
| Tool-contract ablations, terminal, sub-agent economics, Terminal-Bench evaluation program, remaining warm-path candidates | `terminal-bench-readiness.md` |
| Continuation on truncation, delegation roster, supervised write children, final-answer audit, paired evaluation | `supervised-delegation.md` |
| Reversible mutating-run state | `run-snapshots.md` |
| Diagnostics integration | `lsp-diagnostics.md` |

Shipped and removed 2026-09-04: TUI rearchitecture and refinement, compaction,
model-reviewed approvals, read-only sub-agents (Phases A–C), provider
rearchitecture, client parity (Tiers 1–2), the proposed `qq-core` physical
extraction (superseded by D9), and the Terminal-Bench baseline-repair tranche
(folded into readiness Phase 6 gates).

## Conventions

- A plan's `Status:` line is authoritative for what has landed. Update it in
  the same PR that ships the work.
- Record a pre-change baseline for every named performance gate before the
  change lands (Performance Constitution in `speed-first-…`).
- When a plan is fully shipped, move any durable contract into
  `docs/design/`, delete the plan, and update this index.
