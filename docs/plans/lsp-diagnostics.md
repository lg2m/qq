# LSP Diagnostics

Status: proposed.

The biggest quality lever for a coding agent is how fast it learns an
edit is wrong. Today that feedback is pull-only: the agent must choose
to run `cargo check` or equivalent through `shell`, pay a full
invocation, and spend a model turn reading the output. Language servers
(rust-analyzer, clangd, tsserver, ...) already compute diagnostics
continuously; the harness should push them into the edit loop.

## Scope: Diagnostics, Not An Editor

First-class LSP support means diagnostics in the tool loop. It does not
mean the editor surface:

- In scope: server lifecycle, document sync for files the agent
  touches, publishing diagnostics into edit results, an explicit
  `diagnostics` tool.
- Out of scope (intentionally): references, rename, hover, completion,
  code actions, formatting. Agent navigation is grep-shaped and the
  model reads code natively; each additional capability is lifecycle
  surface to maintain. Revisit only when usage shows a concrete gap.

## Design

**Lifecycle mirrors MCP.** A `qq-lsp` crate manages one shared server
per (workspace, language), config-declared, started lazily on the first
edit to a matching file, restarted with backoff after crashes. Servers
are ordinary local processes; declarations are trust-gated in workspace
config sources like MCP servers, with no default commands baked in
beyond named presets the user opts into.

**Config** (RON, layered like `mcp`):

```ron
lsp: {
    "rust": Preset(rust_analyzer),
    "c": Server(command: "clangd", args: [], patterns: ["*.c", "*.h"]),
}
```

Presets pin known-good invocations and file patterns for common servers
(rust-analyzer, clangd, tsserver, gopls, pyright); `Server` declares
anything else. Name and pattern validation follow the MCP rules.

**Document sync is minimal.** The agent edits real files on disk. The
harness sends `didOpen`/`didChange` with file content for files the
session's file-state map touches, and relies on the server's own
watching for the rest of the workspace. No incremental sync; full-text
updates are fine at agent edit rates.

**Post-edit diagnostics.** After `edit_file`/`write_file` completes, the
harness waits a bounded window (default 2 s, configurable) for the
server's next diagnostics publish on that file, then appends a bounded
summary to the tool result the model sees:

```
Edited src/lib.rs: replaced 2 occurrence(s).
2 new diagnostics: E0308 mismatched types (line 341); unused import
`std::fs` (line 12).
```

Only new-or-changed diagnostics for the edited file are reported, count
bounded (default 5, "and N more"). If the server has not published
within the window, the result says diagnostics are pending — never
block the loop on a slow analyzer. This lands in the model-facing
result string deliberately (unlike the display diff): the model is the
consumer.

**`diagnostics` tool.** A read-only builtin: current diagnostics for a
path or a bounded workspace summary. Classified read-only, no approval,
results through the standard truncation. This is the pull complement to
the post-edit push and the model's recovery path after "pending".

## Validate Through MCP First

MCP bridge servers for LSP exist. Before building `qq-lsp`, wiring one
into the new MCP support validates whether diagnostics actually change
agent behavior for real sessions at near-zero cost. The native build is
justified by the post-edit hook — that requires running inside the edit
tool's lifecycle, which a model-invoked MCP tool cannot do — but the
bridge answers the value question first.

## Sequencing

1. MCP-bridge validation in a real workspace (no code).
2. `qq-lsp` crate: lifecycle, document sync, diagnostics subscription.
3. `diagnostics` builtin tool.
4. Post-edit diagnostics in edit results.
5. Presets beyond rust-analyzer as demanded.
