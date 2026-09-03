use super::*;

pub(super) fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let query = app
        .overlay
        .as_ref()
        .map_or("", |overlay| overlay.picker().query.as_str());
    let confirm = app.session_picker_confirm();
    let selected = app.session_picker_selected();
    let filtered = app.filtered_sessions();
    let scoped = matches!(
        &app.overlay,
        Some(crate::input::Overlay::Sessions { scope: Some(_), .. })
    );
    let mut lines = vec![section(
        if scoped { "AGENTS" } else { "SESSIONS" },
        if confirm.is_some() {
            "y confirms, n or Esc cancels"
        } else {
            "type to search, Enter focuses, Ctrl-D deletes, Ctrl-P prunes empty, Esc closes"
        },
    )];
    lines.push(search_line(query, "all sessions"));
    if let Some(confirm) = confirm {
        let question = match confirm {
            SessionConfirm::Delete(session_id) => {
                let title = app
                    .sessions
                    .get(&session_id)
                    .map_or("this session", |session| session.summary.title.as_str());
                format!("  ◇ delete '{title}'? y deletes, n keeps")
            }
            SessionConfirm::Prune => {
                "  ◇ delete every empty session in this workspace? y deletes, n keeps".to_owned()
            }
        };
        lines.push(truncate_line(
            Line::styled(question, warning().bold()),
            width,
        ));
    }
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::styled(
            if app.sessions.is_empty() {
                "  Alt-N creates a root session."
            } else {
                "  No matching sessions."
            },
            muted().italic(),
        ));
        return fit_height(lines, height);
    }

    let mut results = Vec::with_capacity(filtered.len());
    let mut selected_row = 0;
    for session_id in filtered {
        let depth = app.sessions.depth(session_id);
        let is_selected = selected == Some(session_id);
        if is_selected {
            selected_row = results.len();
        }
        let prefix = format!(
            "  {}{} ",
            "  ".repeat(depth),
            if is_selected { ">" } else { " " }
        );
        results.push(session_line(app, session_id, width, &prefix));
    }

    lines.extend(selection_viewport(
        results,
        height.saturating_sub(lines.len()),
        selected_row,
    ));
    fit_height(lines, height)
}

/// The `search:` row shared by every picker.
pub(super) fn search_line(query: &str, placeholder: &str) -> Line {
    Line::styled(
        format!(
            "  search: {}",
            if query.is_empty() { placeholder } else { query }
        ),
        if query.is_empty() { muted() } else { accent() },
    )
}

pub(super) fn model_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let picker = match &app.overlay {
        Some(overlay) => overlay.picker(),
        None => return fit_height(Vec::new(), height),
    };
    let filtered = app.filtered_models();
    let mut lines = vec![section(
        "MODELS",
        if app.focused().is_some() {
            "type to search, Enter sets the session model, Ctrl-N creates a session, Esc closes"
        } else {
            "type to search, Up/Down select, Enter creates session, Esc closes"
        },
    )];
    lines.push(search_line(&picker.query, "all models"));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::styled("  No matching models.", muted().italic()));
        return fit_height(lines, height);
    }

    let mut results = Vec::new();
    let mut selected_row = 0;
    let mut provider = None;
    let selected_position = picker.selected(filtered.len());
    for (position, index) in filtered.iter().enumerate() {
        let option = &app.models[*index];
        if provider != Some(option.provider.as_str()) {
            provider = Some(&option.provider);
            results.push(Line::styled(
                format!("  {}", option.provider.to_ascii_uppercase()),
                accent().bold(),
            ));
        }
        let selected = position == selected_position;
        if selected {
            selected_row = results.len();
        }
        let mut line = Line::styled(if selected { "  > " } else { "    " }, muted());
        line.push(
            option.name.as_deref().unwrap_or(&option.model),
            if selected { normal().bold() } else { normal() },
        );
        if option.name.as_deref() != Some(option.model.as_str()) {
            line.push(format!("  {}", option.model), muted());
        }
        results.push(truncate_line(line, width));
    }

    lines.extend(selection_viewport(
        results,
        height.saturating_sub(lines.len()),
        selected_row,
    ));
    fit_height(lines, height)
}

/// Theme picker. Each row shows the theme name and a swatch of its roles,
/// painted in that theme's own colors so the list doubles as a preview
/// strip; the whole frame is already drawn in the highlighted theme.
pub(super) fn theme_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let picker = match &app.overlay {
        Some(overlay) => overlay.picker(),
        None => return fit_height(Vec::new(), height),
    };
    let filtered = app.filtered_themes();
    let mut lines = vec![section(
        "THEMES",
        "Up/Down preview live, Enter keeps, Esc restores the previous theme",
    )];
    lines.push(search_line(&picker.query, "all themes"));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::styled("  No matching themes.", muted().italic()));
        return fit_height(lines, height);
    }
    let mut results = Vec::new();
    let mut selected_row = 0;
    let selected_position = picker.selected(filtered.len());
    for (position, index) in filtered.iter().enumerate() {
        let theme = &app.themes[*index];
        let selected = position == selected_position;
        if selected {
            selected_row = results.len();
        }
        let mut line = Line::styled(if selected { "  > " } else { "    " }, muted());
        line.push(
            format!("{:<18}", theme.name),
            if selected { normal().bold() } else { normal() },
        );
        let palette = theme.palette;
        for color in [
            palette.text,
            palette.muted,
            palette.accent,
            palette.brand,
            palette.warning,
            palette.error,
            palette.success,
        ] {
            line.push("██", Style::color(color).on(palette.surface));
        }
        if *index == app.theme {
            line.push("  active", accent());
        }
        results.push(truncate_line(line, width));
    }
    lines.extend(selection_viewport(
        results,
        height.saturating_sub(lines.len()),
        selected_row,
    ));
    fit_height(lines, height)
}

pub(super) fn approval_prompt(app: &App, width: usize, height: usize) -> Vec<Line> {
    let tool_call = app.pending_approval().expect("an approval is pending");
    let mut lines = vec![section(
        "TOOL APPROVAL",
        "y approves once, a approves for this session, w always allows in this workspace, \
         n or Esc denies",
    )];
    lines.push(Line::default());
    let mut name = Line::styled("  ◇ ", warning());
    name.push("tool: ", muted());
    name.push(tool_call.name.clone(), warning().bold());
    lines.push(truncate_line(name, width));
    if let Some(command) = shell_command_preview(tool_call) {
        let mut line = Line::styled("  command: ", muted());
        line.push(command, normal().bold());
        lines.push(truncate_line(line, width));
    }
    if let Some(edit) = app.pending_approval_edit() {
        // An edit approval shows what would change instead of the raw
        // arguments; diff lines truncate rather than reflow.
        let mut line = Line::styled("  file: ", muted());
        line.push(edit.path.clone(), normal().bold());
        lines.push(truncate_line(line, width));
        let available = height.saturating_sub(lines.len() + 2).max(1);
        for (shown, text) in edit.diff.lines().enumerate() {
            if shown == available {
                lines.push(Line::styled("    ...", muted().italic()));
                break;
            }
            lines.push(truncate_line(
                Line::styled(format!("    {text}"), diff_line_style(text)),
                width,
            ));
        }
    } else {
        lines.push(Line::styled("  arguments:", muted()));
        let arguments = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
            .and_then(|value| serde_json::to_string_pretty(&value))
            .unwrap_or_else(|_| tool_call.arguments.clone());
        let available = height.saturating_sub(lines.len() + 2).max(1);
        for (shown, text) in arguments.lines().enumerate() {
            if shown == available {
                lines.push(Line::styled("    ...", muted().italic()));
                break;
            }
            lines.push(truncate_line(
                Line::styled(format!("    {text}"), normal()),
                width,
            ));
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        "  [y] approve once   [a] for session   [w] for workspace   [n]/[Esc] deny",
        accent().bold(),
    ));
    fit_height(lines, height)
}

/// Shell approvals surface the exact command so the user can decide in place.
pub(super) fn shell_command_preview(tool_call: &ToolCallSnapshot) -> Option<String> {
    if tool_call.name != "shell" {
        return None;
    }
    let arguments = serde_json::from_str::<serde_json::Value>(&tool_call.arguments).ok()?;
    let command = arguments.get("command")?.as_str()?;
    let cwd = arguments.get("cwd").and_then(|value| value.as_str());
    Some(match cwd {
        Some(cwd) => format!("{command}  (in {cwd})"),
        None => command.to_owned(),
    })
}
