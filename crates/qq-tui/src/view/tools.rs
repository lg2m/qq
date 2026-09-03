use super::*;

/// Runs with more than this many quiet tool calls fold into one summary row.
pub(super) const TOOL_FOLD_THRESHOLD: usize = 3;
/// Emitted by qq-core tools when a result was cut short; excluded from counts.
pub(super) const TOOL_TRUNCATION_MARKER: &str = "...[truncated by qq]";
pub(super) const TOOL_SUBJECT_WIDTH: usize = 48;
pub(super) const MAX_TOOL_ERROR_BYTES: usize = 2 * 1024;
pub(super) const MAX_TOOL_ERROR_ROWS: usize = 6;
pub(super) const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
pub(super) const MAX_TOOL_ARGUMENT_ROWS: usize = 8;
pub(super) const MAX_TOOL_RESULT_ROWS: usize = 12;
/// Rows of live streamed output shown under a running call's one-liner.
pub(super) const MAX_LIVE_TAIL_ROWS: usize = 6;
pub(super) const TOOL_SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// Renders one run's tool calls: a folded count for quiet runs, otherwise one
/// gutter line per call, with errors and the expanded detail level adding
/// bounded body rows. Running calls with buffered live output show a bounded
/// tail of it at every detail level — a running command's output is the thing
/// the user is waiting for.
/// Rows rendered beneath a tool call that spawned a child session.
pub(super) type ChildRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

pub(super) fn render_tool_calls(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    detail: ToolDetail,
    tick: usize,
    width: usize,
    children: ChildRows<'_>,
) -> Vec<Line> {
    let quiet = |call: &ToolCallSnapshot| {
        call.state == ToolCallState::Completed
            && !call.is_error
            && children(call.id, width).is_empty()
    };
    if detail == ToolDetail::Collapsed
        && calls.len() > TOOL_FOLD_THRESHOLD
        && calls.iter().all(|call| quiet(call))
    {
        return vec![tool_fold_line(calls, width)];
    }
    let mut lines = Vec::with_capacity(calls.len());
    for call in calls {
        lines.push(tool_summary_line(call, tick, width));
        if call.is_error {
            if let Some(result) = call.result.as_deref() {
                lines.extend(tool_error_lines(result, width));
            }
        } else if detail == ToolDetail::Expanded {
            lines.extend(tool_expanded_lines(call, width));
        }
        if call.state == ToolCallState::Running
            && let Some(output) = live_output.get(&call.id)
        {
            lines.extend(tool_live_output_lines(output, width));
        }
        // A spawned child renders under the call that created it so the
        // parent transcript shows delegated work in execution order.
        lines.extend(children(call.id, width));
    }
    lines
}

pub(super) fn tool_fold_line(calls: &[&ToolCallSnapshot], width: usize) -> Line {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        match counts.iter_mut().find(|(name, _)| *name == call.name) {
            Some((_, count)) => *count += 1,
            None => counts.push((call.name.as_str(), 1)),
        }
    }
    let mut line = Line::styled("   ", muted());
    line.push("▸ ", accent());
    line.push(format!("{} tool calls (", calls.len()), muted());
    for (index, (name, count)) in counts.iter().enumerate() {
        if index > 0 {
            line.push(", ", muted());
        }
        line.push(format!("{name} ×{count}"), muted());
    }
    line.push(")", muted());
    truncate_line(line, width)
}

/// One collapsed gutter line: state glyph, tool name, curated subject from the
/// arguments, and a metric derived from the result.
pub(super) fn tool_summary_line(call: &ToolCallSnapshot, tick: usize, width: usize) -> Line {
    let (glyph, glyph_style) = tool_state_glyph(call, tick);
    let mut line = Line::styled("   ", muted());
    line.push(glyph, glyph_style);
    line.push(" ", muted());
    line.push(call.name.as_str(), muted());
    if let Some(subject) = tool_subject(call) {
        line.push(format!(" {subject}"), muted());
    }
    if let Some(metric) = tool_result_metric(call) {
        line.push(format!(" ({metric})"), muted());
    }
    if call.state != ToolCallState::Completed {
        line.push(
            format!(" {}", tool_state_label(call.state)),
            match call.state {
                ToolCallState::Failed | ToolCallState::Denied => failure(),
                ToolCallState::AwaitingApproval => warning(),
                ToolCallState::Running => accent(),
                ToolCallState::Requested
                | ToolCallState::Interrupted
                | ToolCallState::Completed => muted(),
            },
        );
    }
    truncate_line(line, width)
}

pub(super) fn tool_state_glyph(call: &ToolCallSnapshot, tick: usize) -> (&'static str, Style) {
    match call.state {
        ToolCallState::Running => (spinner(tick), info()),
        ToolCallState::Requested => ("○", muted()),
        ToolCallState::Completed => {
            if call.is_error {
                ("✕", failure())
            } else {
                ("●", muted())
            }
        }
        ToolCallState::Failed | ToolCallState::Denied => ("✕", failure()),
        ToolCallState::AwaitingApproval => ("◇", warning()),
        ToolCallState::Interrupted => ("◌", muted()),
    }
}

/// The most informative argument for known tools; a compact truncated argument
/// preview otherwise, so new tool names degrade gracefully. Malformed JSON
/// falls back to the raw truncated string.
pub(super) fn tool_subject(call: &ToolCallSnapshot) -> Option<String> {
    let compact = || {
        let text = preview(&call.arguments, TOOL_SUBJECT_WIDTH);
        (!text.is_empty()).then_some(text)
    };
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return compact();
    };
    match call.name.as_str() {
        "read_file" | "list_dir" => arguments
            .get("path")
            .and_then(|value| value.as_str())
            .map(|path| preview(path, TOOL_SUBJECT_WIDTH))
            .or_else(compact),
        "search" => arguments
            .get("query")
            .and_then(|value| value.as_str())
            .map(|query| format!("\"{}\"", preview(query, TOOL_SUBJECT_WIDTH)))
            .or_else(compact),
        _ => compact(),
    }
}

/// A one-glance size for the result: line/entry/match counts for known tools,
/// byte size for everything else. Errors expand instead of summarizing.
pub(super) fn tool_result_metric(call: &ToolCallSnapshot) -> Option<String> {
    if call.is_error {
        return None;
    }
    let result = call.result.as_deref()?;
    let truncated = result.lines().any(|line| line == TOOL_TRUNCATION_MARKER);
    let content_lines = || {
        result
            .lines()
            .filter(|line| *line != TOOL_TRUNCATION_MARKER)
            .count()
    };
    let metric = match call.name.as_str() {
        "read_file" => count_noun(content_lines(), "line", "lines"),
        "list_dir" => count_noun(content_lines(), "entry", "entries"),
        "search" => {
            if result.starts_with("No matches found.") {
                "no matches".to_owned()
            } else {
                // Match rows are `path:line:content` or `path: filename
                // match`, grouped per file, so counting consecutive distinct
                // path prefixes counts files without allocating.
                let mut matches = 0_usize;
                let mut files = 0_usize;
                let mut previous: Option<&str> = None;
                for line in result.lines() {
                    if line.is_empty() || line == TOOL_TRUNCATION_MARKER {
                        continue;
                    }
                    matches += 1;
                    let path = line.split(':').next().unwrap_or(line);
                    if previous != Some(path) {
                        files += 1;
                        previous = Some(path);
                    }
                }
                format!(
                    "{}, {}",
                    count_noun(matches, "match", "matches"),
                    count_noun(files, "file", "files")
                )
            }
        }
        _ => format_result_size(result.len()),
    };
    if truncated {
        Some(format!("{metric}, truncated"))
    } else {
        Some(metric)
    }
}

/// Errors are the one case where content matters by default: show a bounded
/// tail of the error text under the gutter line.
pub(super) fn tool_error_lines(result: &str, width: usize) -> Vec<Line> {
    let text = bounded_tail(result, MAX_TOOL_ERROR_BYTES);
    let total = text.lines().count();
    let mut lines = Vec::new();
    if total > MAX_TOOL_ERROR_ROWS || text.len() < result.len() {
        lines.push(Line::styled("     ...", muted().italic()));
    }
    for line in text.lines().skip(total.saturating_sub(MAX_TOOL_ERROR_ROWS)) {
        lines.push(truncate_line(
            Line::styled(format!("     {line}"), failure()),
            width,
        ));
    }
    lines
}

/// Expanded detail: bounded pretty-printed arguments plus a bounded tail of
/// the result. Oversized or malformed arguments render as a raw bounded tail.
pub(super) fn tool_expanded_lines(call: &ToolCallSnapshot, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let pretty = if call.arguments.len() <= MAX_TOOL_DETAIL_BYTES {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .and_then(|value| serde_json::to_string_pretty(&value))
            .ok()
    } else {
        None
    };
    let arguments = pretty
        .as_deref()
        .unwrap_or_else(|| bounded_tail(&call.arguments, MAX_TOOL_DETAIL_BYTES));
    for (shown, line) in arguments.lines().enumerate() {
        if shown == MAX_TOOL_ARGUMENT_ROWS {
            lines.push(Line::styled("     ...", muted().italic()));
            break;
        }
        lines.push(truncate_line(
            Line::styled(format!("     {line}"), muted()),
            width,
        ));
    }
    // Calls carrying a diff display payload render it in place of the raw
    // result string; diff-shaped results without a payload (shell output,
    // older stores) keep the looks_like_diff heuristic as a fallback.
    let (body, diff) = match &call.display {
        Some(ToolCallDisplay::Diff { diff, .. }) => (Some(diff.as_str()), true),
        None => {
            let result = call.result.as_deref();
            let diff = matches!(call.name.as_str(), "edit_file" | "write_file")
                && !call.is_error
                && result.is_some_and(looks_like_diff);
            (result, diff)
        }
    };
    if let Some(result) = body {
        let text = bounded_tail(result, MAX_TOOL_DETAIL_BYTES);
        let total = text.lines().count();
        if total > MAX_TOOL_RESULT_ROWS || text.len() < result.len() {
            lines.push(Line::styled("     ...", muted().italic()));
        }
        for line in text
            .lines()
            .skip(total.saturating_sub(MAX_TOOL_RESULT_ROWS))
        {
            let style = if diff { diff_line_style(line) } else { muted() };
            lines.push(truncate_line(
                Line::styled(format!("     {line}"), style),
                width,
            ));
        }
    }
    lines
}

/// The last few complete lines of a running call's streamed output, muted
/// and literal (character wrap, never reflowed) under the call's one-liner.
/// Only whole lines render: a chunk may end mid-line, and a partial line
/// reads as garbage until its newline arrives.
pub(super) fn tool_live_output_lines(output: &str, width: usize) -> Vec<Line> {
    let complete = &output[..output.rfind('\n').map_or(0, |index| index + 1)];
    let content_width = width.saturating_sub(5).max(1);
    let total = complete.lines().count();
    let mut rows = Vec::new();
    for line in complete
        .lines()
        .skip(total.saturating_sub(MAX_LIVE_TAIL_ROWS))
    {
        let safe = line
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        rows.extend(wrap_line_chars(Line::styled(safe, muted()), content_width));
    }
    // Long lines wrap into extra rows; keep the tail bounded regardless.
    let excess = rows.len().saturating_sub(MAX_LIVE_TAIL_ROWS);
    if excess > 0 {
        rows.drain(..excess);
    }
    indent_lines(rows, "     ", muted(), width)
}

/// Whether text is unified-diff-shaped: a hunk header, or both added and
/// removed lines. Prose summaries ("Edited x: replaced 1 occurrence(s).")
/// never match, so diff coloring only applies to an actual diff.
pub(super) fn looks_like_diff(text: &str) -> bool {
    let mut added = false;
    let mut removed = false;
    for line in text.lines() {
        if line.starts_with("@@") {
            return true;
        }
        added |= line.starts_with('+');
        removed |= line.starts_with('-');
        if added && removed {
            return true;
        }
    }
    false
}

pub(super) fn count_noun(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

pub(super) fn format_result_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}.{} KB", bytes / 1024, (bytes % 1024) * 10 / 1024)
    }
}

pub(super) const fn tool_state_label(state: ToolCallState) -> &'static str {
    match state {
        ToolCallState::Requested => "requested",
        ToolCallState::AwaitingApproval => "awaiting approval",
        ToolCallState::Running => "running",
        ToolCallState::Completed => "completed",
        ToolCallState::Failed => "failed",
        ToolCallState::Denied => "denied",
        ToolCallState::Interrupted => "interrupted",
    }
}
