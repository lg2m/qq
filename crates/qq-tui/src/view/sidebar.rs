use super::*;

/// Columns the session sidebar occupies, including its left border.
pub(super) const SIDEBAR_WIDTH: usize = 36;
pub(super) fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let session = &app.sessions[&session_id].summary;
    // The same state vocabulary tool rows use: ● done, ✕ failed, ◌ stopped
    // early, ◇ waiting, the shared spinner while running.
    let awaiting = !app.sessions[&session_id].live.awaiting_approval.is_empty();
    let (marker, style) = match session.status {
        _ if awaiting => ("◇", warning()),
        SessionStatus::Idle => match session.last_outcome.as_ref() {
            Some(qq_protocol::RunOutcome::Completed) => ("●", success()),
            Some(qq_protocol::RunOutcome::Cancelled | qq_protocol::RunOutcome::Interrupted) => {
                ("◌", warning())
            }
            Some(qq_protocol::RunOutcome::BudgetExhausted { .. }) => ("◌", warning()),
            Some(qq_protocol::RunOutcome::Failed { .. }) => ("✕", failure()),
            None => ("○", muted()),
        },
        SessionStatus::Queued => ("○", warning()),
        SessionStatus::Running => (spinner(app.animation_tick), info()),
    };
    let mut line = Line::styled(prefix, muted());
    line.push(format!("{marker} "), style);
    line.push(
        &session.title,
        if app.focused() == Some(session_id) {
            normal().bold()
        } else {
            normal()
        },
    );
    if session.queued_prompts > 0 {
        line.push(format!("  {} queued", session.queued_prompts), warning());
    }
    truncate_line(line, width)
}

/// Title row for a pane when several share the screen: the session title,
/// its live status, and an accent on the focused pane so the eye finds where
/// the composer will send.
pub(super) fn pane_title(
    app: &App,
    session_id: Option<SessionId>,
    focused: bool,
    width: usize,
) -> Line {
    let (marker, marker_style) = if focused {
        ("▎", border_active())
    } else {
        (" ", muted())
    };
    let mut line = Line::styled(marker, marker_style);
    match session_id.and_then(|id| app.sessions.get(&id)) {
        Some(session) => {
            line.push(
                &session.summary.title,
                if focused { normal().bold() } else { muted() },
            );
            if let Some((status, style)) = live_status_line(app, session.summary.id) {
                line.push("  ", muted());
                line.push(status, style);
            }
        }
        None => line.push("no session", muted().italic()),
    }
    truncate_line(line, width)
}

/// Right-hand session tree with live status for every session, warm or cold.
/// Each session takes one row (title) plus one row of status when it has
/// anything to say: the active tool, an approval waiting, or the newest
/// assistant text. Always `height` rows so it zips against the body.
pub(super) fn sidebar(app: &App, width: usize, height: usize) -> Vec<Line> {
    let inner = width.saturating_sub(2);
    let mut lines = Vec::with_capacity(height);
    let mut header = Line::styled("│ ", muted());
    header.push("SESSIONS", accent().bold());
    let running = app
        .sessions
        .values()
        .filter(|session| session.summary.status == SessionStatus::Running)
        .count();
    if running > 0 {
        header.push(format!("  {running} running"), accent());
    }
    lines.push(truncate_line(header, width));
    lines.push(Line::styled("│", muted()));
    let order = app.sessions.thread_order();
    let mut rows: Vec<Line> = Vec::new();
    let mut focused_row = 0;
    for &session_id in order {
        let depth = app.sessions.depth(session_id);
        let indent = "  ".repeat(depth.min(4));
        if app.focused() == Some(session_id) {
            focused_row = rows.len();
        }
        rows.push(session_line(app, session_id, width, &format!("│ {indent}")));
        if let Some(status) = live_status_line(app, session_id) {
            let mut line = Line::styled(format!("│ {indent}   "), muted());
            let used = line.width();
            let (text, style) = status;
            line.push(preview(&text, inner.saturating_sub(used)), style);
            rows.push(truncate_line(line, width));
        }
    }
    if rows.is_empty() {
        rows.push(Line::styled("│   no sessions yet", muted().italic()));
    }
    let available = height.saturating_sub(lines.len());
    lines.extend(selection_viewport(rows, available, focused_row));
    while lines.len() < height {
        lines.push(Line::styled("│", muted()));
    }
    lines.truncate(height);
    lines
}

/// Rows for the child session a `spawn_agent` call created: its title and
/// status glyph, then one status line (approval wait, active tool, live tail,
/// or activity). Empty when the call has no recorded child.
pub(super) fn child_rows(app: &App, tool_call_id: ToolCallId, width: usize) -> Vec<Line> {
    let Some(child) = app.sessions.child_spawned_by(tool_call_id) else {
        return Vec::new();
    };
    let mut rows = vec![session_line(app, child, width, "       ↳ ")];
    if let Some((text, style)) = live_status_line(app, child) {
        let mut line = Line::styled("            ", muted());
        let used = line.width();
        line.push(preview(&text, width.saturating_sub(used)), style);
        rows.push(truncate_line(line, width));
    }
    rows
}

/// One-line live status for a session row, most urgent first.
pub(super) fn live_status_line(app: &App, session_id: SessionId) -> Option<(String, Style)> {
    let session = app.sessions.get(&session_id)?;
    let live = &session.live;
    if !live.awaiting_approval.is_empty() {
        let tool = live.active_tool.as_deref().unwrap_or("tool");
        return Some((format!("? approve {tool}"), warning().bold()));
    }
    if session.summary.status == SessionStatus::Running {
        if let Some(tool) = &live.active_tool {
            return Some((format!("> {tool}"), accent()));
        }
        if !live.tail.is_empty() {
            return Some((live.tail.clone(), muted()));
        }
        let label = match session.activity.map(|(_, activity)| activity) {
            Some(qq_protocol::RunActivity::WaitingForProvider) | None => "waiting for provider",
            Some(qq_protocol::RunActivity::Reasoning) => "reasoning",
            Some(qq_protocol::RunActivity::GeneratingResponse) => "responding",
            Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing a tool call",
        };
        return Some((label.to_owned(), muted().italic()));
    }
    if session.summary.queued_prompts > 0 {
        return Some((
            format!("{} queued", session.summary.queued_prompts),
            warning(),
        ));
    }
    if app.focused() != Some(session_id) && !live.tail.is_empty() {
        return Some((live.tail.clone(), muted()));
    }
    None
}

/// Extend `line` with spaces to exactly `width` display columns.
pub(super) fn pad_line(line: &mut Line, width: usize) {
    let used = line.width();
    if used < width {
        line.push(" ".repeat(width - used), normal());
    }
}
