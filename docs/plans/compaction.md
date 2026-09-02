# Context Compaction

Status: all five sequencing steps are implemented (result pruning in
assembly; `CompactSession` + `/compact` with summary/marker storage;
auto-compact thresholds replacing the hard budget failure; `search_history`
recall; summary validation and `RollbackCompaction`). See
`terminal-bench-readiness.md` "Compaction Hardening" for the shipped bounds.

Long sessions outgrow model context. Today the runtime fails the run
when the assembled context exceeds the session budget; compaction
replaces that failure with continuity: the session keeps going, the
model keeps what matters, and nothing is ever actually lost.

## Principle: Compact The Assembly, Not The Transcript

The store already holds the full transcript durably, and every run
assembles model context from it. Compaction is therefore a property of
context assembly, not an edit to history:

- A compaction produces a summary row and a cutoff marker for the
  session, persisted like any other session data.
- Assembly becomes: agent instructions + latest summary + verbatim
  context after the marker.
- The client transcript is untouched — users still scroll full history.
- Recompaction replaces the summary; removing the marker undoes
  compaction entirely. Repeated compactions summarize the prior summary
  plus the span since, so quality degrades gradually, not abruptly.

## Losses Come From Tool Results

Most context weight is tool traffic, and most tool traffic is
re-derivable state, not history. Two mechanisms ahead of summarization:

- **Result pruning.** During assembly, read-only tool results older
  than the recency window are replaced by one-line stubs naming the
  tool, arguments, and size ("re-read if needed"). The session
  file-state map already knows what was read and whether it has since
  changed; stubs are safe because the agent can re-derive on demand.
  Pruning is continuous and independent of compaction.
- **Recall.** A `search_history` built-in searches the session's full
  persisted transcript (messages and tool results) and returns bounded
  excerpts. With recall available, compaction can be aggressive: the
  summary is an index into history, not the only surviving copy.

## The Summary

Produced by a dedicated summarization run against the session's model
(configurable override), with a structured schema, not freeform prose:

- Intent: what the user is trying to accomplish, in their terms.
- Decisions and constraints, with the why; exact names, paths, and
  flags — vague references are forbidden by the prompt.
- Work state: what was done, what is in flight, what is pending.
- Files touched and their roles (the file-state map seeds this list
  mechanically; the model annotates it).
- Errors seen and how they were resolved, error strings verbatim.
- All user messages, preserved verbatim or near-verbatim — they are
  small and irreplaceable; assistant and tool content is what
  compresses.

## Triggers

- **Manual**: a `/compact` composer command in clients, mapped to a
  `CompactSession` command; valid only while the session is idle.
- **Automatic**: when assembly exceeds a threshold of the session
  budget (default ~70%), the next run compacts first, then assembles.
  Exceeding the hard budget compacts and retries once before failing —
  the current hard failure becomes the last resort, not the policy.
- Never mid-run; a run in flight completes on the context it started
  with.

## Mechanics

- `CompactSession` runs the summarizer through the ordinary run
  machinery (permits, cancellation, usage accounting, cost) but marks
  the run internal: its messages do not join the session transcript;
  its product is the summary row.
- Persistence: summary + marker commit atomically; assembly reads the
  latest marker. Three compactions are retained per session and
  `RollbackCompaction` (idle-only) steps back through them to the
  verbatim transcript.
- Validation: a summary must be non-empty, fit the session context limit,
  carry every required section heading, and shrink the measured assembly
  (above a small floor). Failing summaries settle the internal run as a
  policy failure and leave the prior compaction in force.
- The recency window (last K model turns kept verbatim, default small)
  and the pruning window are configuration alongside the context
  budget.
- Protocol: a command to request compaction, an event carrying the
  updated session state (context size before/after). The exact token meter
  becomes unknown after compaction until the next prompt turn measures the
  summarized context; clients must not display the summarizer request's input
  as the post-compaction session size.

## Sequencing

1. Result pruning in assembly (no protocol change, immediate win).
2. Summary + marker storage, `CompactSession`, manual `/compact`.
3. Auto-compact thresholds replacing the hard budget failure.
4. `search_history` recall tool.
5. Summary validation, bounded history, rollback.
