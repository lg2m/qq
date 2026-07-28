# Transcript Rendering

Status: proposed.

The transcript is the product. A run that reads as one undifferentiated wall
of text hides what the agent actually did; a run that reads as a story —
"said this, ran that, saw the result, said more" — is auditable at a glance.
This document covers the three rendering gaps that keep qq's transcript from
reading as a story, and the visual system that fixes them.

## Problems

1. **Ordering is lost.** A run renders as all assistant text concatenated,
   followed by every tool call in the run. The model's actual sequence —
   preamble text, two calls, interim text, another call, final answer — is
   flattened into "text blob, then call list."
2. **No breathing room.** Blocks abut each other; message headers, body
   text, and tool lines compete at the same visual weight.
3. **Code blocks and diffs are invisible.** Fenced code renders as
   yellow-tinted text inline with prose. Edits render as raw tool output.

## Why ordering needs a core change

The runtime creates one assistant message per run (`assistant_message_id`
is fixed on the run row at creation) and appends every model turn's text
into that message's flat `output` string. Tool calls carry
`turn_ordinal`/`call_ordinal`, but text carries no turn markers, so no
client can reconstruct the interleaving. The `model_turns` table already
persists per-turn assistant content for context assembly; the UI-facing
message stream simply discards that structure.

## Design: one assistant message per model turn

The unit of assistant output becomes the model turn, not the run.

- `MessageSnapshot` gains `turn_ordinal: u16`. User messages use 0.
- The runtime starts a new assistant message when a model turn begins
  (first delta after the previous turn's tool results), emitting
  `AssistantMessageStarted` per turn. `TextAppended` targets the current
  turn's message.
- The run row's `assistant_message_id` becomes the *current* assistant
  message; run claiming and crash recovery interrupt only that message.
- Persistence: the new message row commits in the same transaction as
  `persist_model_turn` (turn row + tool_call rows + events), preserving
  the atomic-persist invariant. Empty turns (calls with no text) persist
  no message row.
- Migration: existing rows keep `turn_ordinal = 0`; old runs render as
  today (one message, calls after). Schema version bumps; protocol
  version bumps; `SnapshotRequest.message_limit` semantics unchanged
  (limit counts messages, and per-turn messages are messages).

Rejected alternative: segment markers inside the single message's output
string. A flat string with positional markers is fragile under streaming
appends and pushes parsing into every client.

### Client assembly

The TUI orders a run's items by `turn_ordinal`, rendering each turn's
message followed by that turn's calls (`call_ordinal` order):

```
   QQ  Sure, I'll look into that...
   ● read_file crates/qq-core/src/lib.rs
   ● search "ToolGate"
   QQ  The gate resolves after the turn yields. Checking the store side...
   ● read_file crates/qq-core/src/sessions.rs
   QQ  Here's what happens: ...
```

The per-turn `QQ` header repeats only when a turn has text; consecutive
call-only turns merge into one call group. Group folding (>3 quiet calls)
applies per contiguous call group, not per run.

## Design: spacing

Rhythm rules, applied in the transcript assembler:

- One blank line between every block (message body ↔ call group ↔ next
  turn's text). Two blank lines before each `YOU` prompt — the
  prompt/response boundary is the strongest seam in the transcript.
- Message bodies indent under their role header (current 3-column gutter
  stays); call groups keep their own gutter glyphs so text and calls are
  distinguishable by silhouette alone.
- Headings inside markdown get a blank line above; list items stay tight.
- The `QQ` header line is dropped for turns whose text is a single short
  interim line? No — keep it; consistency beats density here.

## Design: code blocks

Fenced code becomes a visually distinct panel instead of tinted prose:

- Full-width background tint (dark surface color distinct from the
  terminal background) spanning the block, one padding row above and
  below inside the tint.
- A left border glyph (`│` in the accent-muted color) plus one cell of
  padding; content keeps character-exact wrapping (already literal-flagged
  in the wrap pass).
- The fence's language tag renders as a small right-aligned label on the
  panel's first row (` rust `, muted).
- Inline code keeps the current tinted-text treatment; only fenced blocks
  get panels.

Ratatui note: background tint means setting `Style::bg` on every span of
the block's lines and padding each line to full width so the tint reads
as a panel, not ragged highlights.

## Design: diffs

Two sources render as unified diffs with per-line coloring:

- `EditPreview.diff` in the approval modal (already carried on
  `ToolApprovalRequested`).
- Completed `edit_file`/`write_file` calls at Detailed/Expanded tool
  detail levels — the call's diff is recomputed or carried in the result
  metadata rather than shown as raw tool output.

Coloring: `+` lines green, `-` lines red, `@@` hunk headers in the muted
accent, context lines normal. Diff lines are literal (character wrap, no
reflow). Fenced blocks tagged ` ```diff ` in model output get the same
treatment inside the code-block panel.

## Sequencing

1. **Visual pass (qq-tui only):** spacing rhythm, code-block panels, diff
   coloring for the approval modal and `diff`-fenced blocks. No protocol
   changes; lands independently.
2. **Per-turn messages (qq-core + qq-protocol):** schema + protocol bump,
   per-turn `AssistantMessageStarted`, atomic persist extension, migration.
   Minimal TUI change: group calls under their turn's message.
3. **Integration polish (qq-tui):** turn-aware grouping refinements,
   contiguous call-group folding, edit-result diffs at detail levels.

Step 1 and step 2 touch disjoint crates and can run in parallel; step 3
follows step 2. The shell tool (tool-execution step 4) rewrites the same
regions of `sessions.rs` that step 2 touches and its streamed output will
render inside this layout — it starts after step 2 lands.
