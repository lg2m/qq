use super::*;

pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(super) const GIT_COMMIT: &str = env!("QQ_GIT_COMMIT");
pub(super) fn header(app: &App, width: usize) -> Line {
    let mut left = Line::styled(" qq", brand().bold());
    left.push(format!("  {VERSION} {GIT_COMMIT}"), muted());
    let mut right = Line::styled("local", normal());
    let connection = match app.connection {
        crate::ConnectionState::Connecting => Some("connecting"),
        crate::ConnectionState::Replaying => Some("reconnecting"),
        crate::ConnectionState::Live => None,
        crate::ConnectionState::Offline => Some("offline"),
    };
    if let Some(connection) = connection {
        right.push(format!("  {connection}"), warning());
    }
    align_sides(left, right, width)
}

pub(super) fn context(app: &App, width: usize) -> Line {
    let mut line = Line::styled("  ", muted());
    if let Some(focused) = app.focused() {
        let mut ancestors = Vec::new();
        let mut cursor = Some(focused);
        while let Some(id) = cursor {
            let Some(session) = app.sessions.get(&id) else {
                break;
            };
            ancestors.push(session.summary.title.as_str());
            cursor = session.summary.parent_id;
        }
        ancestors.reverse();
        line.push(ancestors.join(" / "), normal().bold());
    } else {
        line.push(
            if app.workspace_path.is_empty() {
                "QQ"
            } else {
                &app.workspace_path
            },
            muted(),
        );
    }
    truncate_line(line, width)
}

pub(super) fn status_notice(app: &App, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some((status, level)) = app.visible_status() {
        let (prefix, style) = match level {
            crate::app::NoticeLevel::Info => ("", accent()),
            crate::app::NoticeLevel::Warning => ("warning: ", warning()),
            crate::app::NoticeLevel::Error => ("error: ", failure()),
        };
        lines.extend(wrap_line(
            Line::styled(format!("  {prefix}{status}"), style.bold()),
            width.max(1),
        ));
    }
    if let Some(session) = app.focused().and_then(|id| app.sessions.get(&id))
        && session.summary.status == SessionStatus::Running
    {
        let label = match session.activity.map(|(_, activity)| activity) {
            Some(qq_protocol::RunActivity::WaitingForProvider) => "waiting for model",
            Some(qq_protocol::RunActivity::Reasoning) => "reasoning",
            Some(qq_protocol::RunActivity::GeneratingResponse) => "generating response",
            Some(qq_protocol::RunActivity::PreparingToolCall) => "preparing tool call",
            None => "working",
        };
        let spinner = TOOL_SPINNER[app.animation_tick % TOOL_SPINNER.len()];
        lines.extend(wrap_line(
            Line::styled(format!("  {spinner} {label}…"), accent().bold()),
            width.max(1),
        ));
    }
    // Approvals waiting in sessions the user is not looking at would
    // otherwise stall silently. One row names them and how to jump.
    let waiting: Vec<&str> = app
        .sessions_awaiting_approval()
        .into_iter()
        .filter(|id| Some(*id) != app.focused())
        .filter_map(|id| app.sessions.get(&id))
        .map(|session| session.summary.title.as_str())
        .collect();
    if !waiting.is_empty() {
        let mut line = Line::styled("  ? ", warning().bold());
        line.push(
            match waiting.as_slice() {
                [one] => format!("approval needed in {one}"),
                [first, rest @ ..] => {
                    format!("approval needed in {first} and {} more", rest.len())
                }
                [] => String::new(),
            },
            warning().bold(),
        );
        line.push("  Ctrl-G jumps there", muted());
        lines.push(truncate_line(line, width));
    }
    lines
}
/// Drafts held locally while the focused session runs, oldest first. Each
/// takes one row; the newest is the one Alt-Up brings back.
pub(super) fn queued_drafts(app: &App, width: usize) -> Vec<Line> {
    let Some(session_id) = app.focused() else {
        return Vec::new();
    };
    let drafts: Vec<&str> = app.queued_drafts(session_id).collect();
    if drafts.is_empty() {
        return Vec::new();
    }
    let count = drafts.len();
    drafts
        .into_iter()
        .enumerate()
        .map(|(index, draft)| {
            let mut line = Line::styled(" ~ ", warning());
            line.push(
                if index + 1 == count {
                    "queued  Alt-Up edits  "
                } else {
                    "queued  "
                },
                warning().dim(),
            );
            line.push(
                preview(draft, width.saturating_sub(line.width())),
                normal().dim(),
            );
            truncate_line(line, width)
        })
        .collect()
}

/// One logical composer line with paste placeholders styled as tokens so the
/// user can tell them from typed text.
pub(super) fn composer_row(part: &str) -> Line {
    let mut line = Line::default();
    let mut rest = part;
    while let Some(start) = rest.find("[Pasted #") {
        let Some(len) = rest[start..].find(']') else {
            break;
        };
        line.push(&rest[..start], normal());
        line.push(&rest[start..=start + len], accent().italic());
        rest = &rest[start + len + 1..];
    }
    line.push(rest, normal());
    line
}

pub(super) fn composer(app: &App, width: usize, max_rows: usize) -> Vec<Line> {
    let max_rows = max_rows.max(1);
    let caret = if app.animation_tick.is_multiple_of(2) {
        "|"
    } else {
        " "
    };
    if app.composer.text.is_empty() {
        let mut line = Line::styled(" > ", accent().bold());
        line.push("Ask QQ...", muted().italic());
        line.push(caret, accent());
        return vec![truncate_line(line, width)];
    }

    // Insert the visual caret into a rendering copy. Composer offsets are UTF-8
    // byte boundaries, so this remains correct for non-ASCII input.
    let mut display_text = app.composer.text.clone();
    display_text.insert_str(app.composer.cursor(), caret);

    // Keep hard newlines from Shift-Enter / paste, then soft-wrap each logical
    // line inside the content column so every visual row keeps a gutter.
    let content_width = width.saturating_sub(3).max(1);
    let mut wrapped = Vec::new();
    for (line_index, part) in display_text.split('\n').enumerate() {
        let content_rows = if part.is_empty() {
            vec![Line::default()]
        } else {
            wrap_line_chars(composer_row(part), content_width)
        };
        for (row_index, content) in content_rows.into_iter().enumerate() {
            let mut row = if line_index == 0 && row_index == 0 {
                Line::styled(" > ", accent().bold())
            } else {
                Line::styled("   ", muted())
            };
            for span in content.spans {
                row.push(span.text, span.style);
            }
            wrapped.push(row);
        }
    }

    // When the draft outgrows the reserved composer region, keep the tail so
    // the caret and newest typing stay visible.
    if wrapped.len() > max_rows {
        let skip = wrapped.len() - max_rows;
        wrapped.drain(..skip);
        if let Some(first) = wrapped.first_mut() {
            let mut clipped = Line::styled(" … ", muted());
            let rest = std::mem::take(first);
            let spans = match rest.spans.split_first() {
                Some((first_span, rest_spans))
                    if first_span.text == " > "
                        || first_span.text == "   "
                        || first_span.text == " … " =>
                {
                    rest_spans.to_vec()
                }
                _ => rest.spans,
            };
            for span in spans {
                clipped.push(span.text, span.style);
            }
            *first = truncate_line(clipped, width);
        }
    }

    if wrapped.is_empty() {
        let mut line = Line::styled(" > ", accent().bold());
        line.push(caret, accent());
        wrapped.push(truncate_line(line, width));
    }
    wrapped
}

pub(super) fn footer_context(app: &App, width: usize) -> Line {
    let context = match app.focused_context_usage() {
        Some((tokens, limit)) if limit > 0 => {
            let tenths = u128::from(tokens) * 1_000 / u128::from(limit);
            format!(" context: {}.{}% / {limit}", tenths / 10, tenths % 10)
        }
        Some(_) | None => app.focused_context_window().map_or_else(
            || " context: --".to_owned(),
            |limit| format!(" context: -- / {limit}"),
        ),
    };
    let focused = app
        .focused()
        .and_then(|id| app.sessions.get(&id))
        .map(|session| &session.summary);
    let selected_model = focused
        .and_then(|session| session.model.as_deref())
        .or(app.model.model.as_deref())
        .unwrap_or("default");
    let mut left = Line::styled(context, muted());
    left.push(format!("  tools: {}", app.tool_detail.label()), muted());
    align_sides(
        left,
        Line::styled(format!("model: {selected_model} "), accent()),
        width,
    )
}

pub(super) fn footer_workspace(app: &App, width: usize) -> Line {
    let workspace = if app.workspace_path.is_empty() {
        "cwd: connecting".to_owned()
    } else {
        format!("cwd: {}", app.workspace_path)
    };
    // Parent rows deliberately display inclusive accounting. Child rows use
    // the same field, whose inclusive total currently equals direct because
    // delegation depth is capped at one. Unknown cost stays visibly unknown
    // at this final formatting boundary. Legacy payloads without structured
    // accounting fall back to the compatibility direct-cost alias.
    let cost = app
        .focused()
        .and_then(|id| app.sessions.get(&id))
        .and_then(|session| {
            session
                .summary
                .accounting
                .map(|accounting| accounting.inclusive.estimated_cost_usd_nanos)
                .unwrap_or(session.summary.estimated_cost_usd_nanos)
        })
        .map(format_cost)
        .unwrap_or_else(|| "--".to_owned());
    let cost = format!("cost: {cost} ");
    align_sides(
        Line::styled(format!(" {workspace}"), muted()),
        Line::styled(cost, accent()),
        width,
    )
}

pub(super) fn slash_autocomplete(app: &App, width: usize, height: usize) -> Vec<Line> {
    let commands = app.filtered_slash_commands();
    let selected = app.slash_selected(commands.len());
    let visible = height.min(commands.len());
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(commands.len().saturating_sub(visible));
    commands
        .into_iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, command)| {
            let mut line = Line::styled(if index == selected { " > " } else { "   " }, accent());
            line.push(
                command.name,
                if index == selected {
                    normal().bold()
                } else {
                    normal()
                },
            );
            line.push(format!("  {}", command.title), muted());
            truncate_line(line, width)
        })
        .collect()
}

pub(super) fn overlay_slash_autocomplete(body: &mut [Line], autocomplete: Vec<Line>) {
    let start = body.len().saturating_sub(autocomplete.len());
    for (target, line) in body[start..].iter_mut().zip(autocomplete) {
        *target = line;
    }
}

pub(super) fn align_sides(mut left: Line, right: Line, width: usize) -> Line {
    let right_width = right.width();
    if right_width >= width {
        return truncate_line(right, width);
    }
    left = truncate_line(left, width - right_width);
    left.push(" ".repeat(width - right_width - left.width()), muted());
    for span in right.spans {
        left.push(span.text, span.style);
    }
    left
}

pub(super) fn format_cost(usd_nanos: u64) -> String {
    let whole = usd_nanos / 1_000_000_000;
    let micros = (usd_nanos % 1_000_000_000) / 1_000;
    let mut fractional = format!("{micros:06}");
    while fractional.len() > 2 && fractional.ends_with('0') {
        fractional.pop();
    }
    format!("${whole}.{fractional}")
}

pub(super) fn section(title: &str, subtitle: &str) -> Line {
    let mut line = Line::styled(format!(" {title} "), accent().bold());
    line.push(subtitle, muted());
    line
}
