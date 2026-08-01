# QQ Documentation

Two kinds of documents live here, with different lifecycles.

## Map

- `design/` — durable engineering decisions, written in present tense,
  describing the system as built.
  - `product.md` — product intent, priorities, interaction modes, scope.
  - `architecture.md` — system shape, crate layout, protocol, persistence.
  - `protocol.md` — HTTP/SSE wire protocol specification and route contract.
  - `providers.md` — provider validation standard: matrix, test layers,
    live canaries, credential policy.
  - `tools.md` — tool execution and security: the tool loop, built-in
    tools, containment, approvals, shell, MCP.
  - `transcript.md` — transcript rendering: per-turn messages, spacing,
    code blocks, diffs.
- `plans/` — in-flight proposals only.
  - `run-snapshots.md` — shadow-repository restore points for runs.

## Conventions

1. `design/` docs are stateless. They describe the system as it is, with
   rationale and rejected alternatives; no "Status:" lines, no sequencing
   checklists, no implemented/pending markers. A design doc is amended in
   the same commit that changes the behavior it describes.
2. `plans/` docs are mortal. A plan carries problem, design, sequencing,
   and status; when it ships, its durable decisions are folded into
   `design/` and the plan file is deleted — git history is the archive.
