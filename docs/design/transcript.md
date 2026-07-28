# Transcript Rendering

The transcript is the product. A run that reads as one undifferentiated wall
of text hides what the agent actually did; a run that reads as a story —
"said this, ran that, saw the result, said more" — is auditable at a glance.
This document describes the message model and visual system that make qq's
transcript read as a story.

## Why The Turn Is The Unit

The runtime originally created one assistant message per run and appended
every model turn's text into that message's flat `output` string. Tool calls
carried `turn_ordinal`/`call_ordinal`, but text carried no turn markers, so
no client could reconstruct the interleaving: a run rendered as all assistant
text concatenated, followed by every tool call in the run. The model's actual
sequence — preamble text, two calls, interim text, another call, final
answer — was flattened into "text blob, then call list", even though the
`model_turns` table already persisted per-turn assistant content for context
assembly.

The unit of assistant output is therefore the model turn, not the run:

- `MessageSnapshot` carries `turn_ordinal: u16`. User messages use 0.
- The runtime starts a new assistant message when a model turn begins
  (first delta after the previous turn's tool results), emitting
  `AssistantMessageStarted` per turn. `TextAppended` targets the current
  turn's message.
- The run row's `assistant_message_id` is the *current* assistant
  message; run claiming and crash recovery interrupt only that message.
- Persistence: the message row commits in the same transaction as
  `persist_model_turn` (turn row + tool_call rows + events), preserving
  the atomic-persist invariant. Empty turns (calls with no text) persist
  no message row.
- Migration: pre-existing rows keep `turn_ordinal = 0`; old runs render
  as one message with calls after. `SnapshotRequest.message_limit`
  semantics are unchanged (the limit counts messages, and per-turn
  messages are messages).

Rejected alternative: segment markers inside the single message's output
string. A flat string with positional markers is fragile under streaming
appends and pushes parsing into every client.

### Client Assembly

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
applies per contiguous call group, not per run. The header is kept even
for turns whose text is a single short interim line — consistency beats
density.

## Spacing

Rhythm rules, applied in the transcript assembler:

- One blank line between every block (message body ↔ call group ↔ next
  turn's text). Two blank lines before each `YOU` prompt — the
  prompt/response boundary is the strongest seam in the transcript.
- Message bodies indent under their role header (a 3-column gutter);
  call groups keep their own gutter glyphs so text and calls are
  distinguishable by silhouette alone.
- Headings inside markdown get a blank line above; list items stay tight.

## Code Blocks

Fenced code is a visually distinct panel instead of tinted prose:

- Full-width background tint (dark surface color distinct from the
  terminal background) spanning the block, one padding row above and
  below inside the tint.
- A left border glyph (`│` in the accent-muted color) plus one cell of
  padding; content keeps character-exact wrapping (literal-flagged in
  the wrap pass).
- The fence's language tag renders as a small right-aligned label on the
  panel's first row (` rust `, muted).
- Inline code keeps the tinted-text treatment; only fenced blocks get
  panels.

Ratatui note: background tint means setting `Style::bg` on every span of
the block's lines and padding each line to full width so the tint reads
as a panel, not ragged highlights.

## Diffs

Two sources render as unified diffs with per-line coloring:

- `EditPreview.diff` in the approval modal (carried on
  `ToolApprovalRequested`).
- Completed `edit_file`/`write_file` calls at Detailed/Expanded tool
  detail levels — when diff content is available for the call, it
  renders as a colored diff rather than raw tool output.

Coloring: `+` lines green, `-` lines red, `@@` hunk headers in the muted
accent, context lines normal. Diff lines are literal (character wrap, no
reflow). Fenced blocks tagged ` ```diff ` in model output get the same
treatment inside the code-block panel.

## Streamed Tool Output

Streamed shell tool output arrives as `ToolCallOutputDelta` events and
renders live under the running call, inside the same call-group layout,
so a long build is watchable as it happens rather than only after the
call completes.
