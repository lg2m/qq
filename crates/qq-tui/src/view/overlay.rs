use super::*;
use crate::{
    commands::Category,
    input::{CommandRow, ModelRow, Overlay, SessionRow, ThemeRow},
    picker::{Picker, PickerItem},
};

/// Static text around a picker's rows.
struct PickerChrome<'a> {
    title: &'a str,
    hint: &'a str,
    placeholder: &'a str,
    /// A pending yes/no question shown under the search row.
    question: Option<Line>,
    /// Shown instead of rows when nothing matches.
    empty: &'a str,
}

/// The frame every picker shares: a section header with hints, the search
/// row, an optional question, then the filtered rows scrolled so the cursor
/// stays visible. `row` draws one item; the bool marks the cursor.
fn picker_frame<T: PickerItem>(
    picker: &Picker<T>,
    chrome: PickerChrome<'_>,
    width: usize,
    height: usize,
    mut row: impl FnMut(&T, bool, &mut Vec<Line>),
) -> Vec<Line> {
    let PickerChrome {
        title,
        hint,
        placeholder,
        question,
        empty,
    } = chrome;
    let mut lines = vec![section(title, hint)];
    lines.push(search_line(&picker.query, placeholder));
    if let Some(question) = question {
        lines.push(truncate_line(question, width));
    }
    lines.push(Line::default());
    if picker.filtered().len() == 0 {
        lines.push(Line::styled(empty, muted().italic()));
        return fit_height(lines, height);
    }
    let cursor = picker.cursor();
    let mut results = Vec::with_capacity(picker.filtered().len());
    let mut selected_row = 0;
    for (position, (_, item)) in picker.filtered().enumerate() {
        let selected = position == cursor;
        if selected {
            selected_row = results.len();
        }
        row(item, selected, &mut results);
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

fn cursor_prefix(selected: bool) -> Line {
    Line::styled(if selected { "  > " } else { "    " }, muted())
}

pub(super) fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Sessions {
        picker,
        scope,
        confirm,
    }) = &app.overlay
    else {
        return fit_height(Vec::new(), height);
    };
    let question = confirm.map(|confirm| {
        let text = match confirm {
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
        Line::styled(text, warning().bold())
    });
    let create = app
        .chord_label(crate::commands::Command::NewRootSession)
        .unwrap_or_else(|| "/new".to_owned());
    let empty = if app.sessions.is_empty() {
        format!("  {create} creates a root session.")
    } else {
        "  No matching sessions.".to_owned()
    };
    picker_frame(
        picker,
        PickerChrome {
            title: if scope.is_some() {
                "AGENTS"
            } else {
                "SESSIONS"
            },
            hint: if confirm.is_some() {
                "y confirms, n or Esc cancels"
            } else {
                "type to search, Enter focuses, Ctrl-D deletes, Esc closes"
            },
            placeholder: "all sessions",
            question,
            empty: &empty,
        },
        width,
        height,
        |row: &SessionRow, selected, out| {
            let depth = app.sessions.depth(row.id);
            let prefix = format!(
                "  {}{} ",
                "  ".repeat(depth),
                if selected { ">" } else { " " }
            );
            out.push(session_line(app, row.id, width, &prefix));
        },
    )
}

pub(super) fn model_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Models(picker)) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    let mut provider: Option<String> = None;
    picker_frame(
        picker,
        PickerChrome {
            title: "MODELS",
            hint: if app.focused().is_some() {
                "type to search, Enter sets the session model, Ctrl-N creates a session, Esc closes"
            } else {
                "type to search, Up/Down select, Enter creates session, Esc closes"
            },
            placeholder: "all models",
            question: None,
            empty: "  No matching models.",
        },
        width,
        height,
        |row: &ModelRow, selected, out| {
            if provider.as_deref() != Some(row.provider.as_str()) {
                provider = Some(row.provider.clone());
                out.push(Line::styled(
                    format!("  {}", row.provider.to_ascii_uppercase()),
                    accent().bold(),
                ));
            }
            let mut line = cursor_prefix(selected);
            line.push(
                row.name.as_deref().unwrap_or(&row.model),
                if selected { normal().bold() } else { normal() },
            );
            if row.name.as_deref() != Some(row.model.as_str()) {
                line.push(format!("  {}", row.model), muted());
            }
            out.push(truncate_line(line, width));
        },
    )
}

/// Theme picker. Each row shows the theme name and a swatch of its roles,
/// painted in that theme's own colors so the list doubles as a preview
/// strip; the whole frame is already drawn in the highlighted theme.
pub(super) fn theme_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some(Overlay::Themes { picker, .. }) = &app.overlay else {
        return fit_height(Vec::new(), height);
    };
    picker_frame(
        picker,
        PickerChrome {
            title: "THEMES",
            hint: "Up/Down preview live, Enter keeps, Esc restores the previous theme",
            placeholder: "all themes",
            question: None,
            empty: "  No matching themes.",
        },
        width,
        height,
        |row: &ThemeRow, selected, out| {
            let theme = &app.themes[row.index];
            let mut line = cursor_prefix(selected);
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
            if row.index == app.theme {
                line.push("  active", accent());
            }
            out.push(truncate_line(line, width));
        },
    )
}

/// The command palette and the help overlay: every command with its chord
/// and slash name. Help groups rows under category headers.
pub(super) fn command_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let Some((picker, help)) = app.command_picker() else {
        return fit_height(Vec::new(), height);
    };
    let mut category: Option<Category> = None;
    let chord_column = 14;
    picker_frame(
        picker,
        PickerChrome {
            title: if help { "HELP" } else { "COMMANDS" },
            hint: if help {
                "every key and command; type to filter, Enter runs, Esc closes"
            } else {
                "type to search, Enter runs the command, Esc closes"
            },
            placeholder: "all commands",
            question: None,
            empty: "  No matching commands.",
        },
        width,
        height,
        |row: &CommandRow, selected, out| {
            if help && category != Some(row.spec.category) {
                if category.is_some() {
                    out.push(Line::default());
                }
                category = Some(row.spec.category);
                out.push(Line::styled(
                    format!("  {}", row.spec.category.label()),
                    accent().bold(),
                ));
            }
            let mut line = cursor_prefix(selected);
            let chord = app.chord_label(row.spec.command).unwrap_or_default();
            line.push(
                format!("{chord:<chord_column$}"),
                if chord.is_empty() { muted() } else { accent() },
            );
            line.push(
                row.spec.title,
                if selected { normal().bold() } else { normal() },
            );
            if let Some(slash) = row.spec.slash.first() {
                line.push(format!("  {slash}"), muted());
            }
            out.push(truncate_line(line, width));
        },
    )
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
