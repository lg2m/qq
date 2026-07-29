use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    io::Write,
};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use qq_protocol::{
    MessageId, MessageRole, MessageSnapshot, MessageState, SessionId, SessionStatus,
    ToolCallDisplay, ToolCallId, ToolCallSnapshot, ToolCallState,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    Layout,
    app::{App, ToolDetail, terminal_safe_character},
};

const MAX_RENDER_WIDTH: u16 = 320;
const MAX_RENDER_HEIGHT: u16 = 160;
const MAX_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
const MAX_CACHED_MARKDOWN_ROWS: usize = MAX_RENDER_HEIGHT as usize;
/// Runs with more than this many quiet tool calls fold into one summary row.
const TOOL_FOLD_THRESHOLD: usize = 3;
/// Emitted by qq-core tools when a result was cut short; excluded from counts.
const TOOL_TRUNCATION_MARKER: &str = "...[truncated by qq]";
const TOOL_SUBJECT_WIDTH: usize = 48;
const MAX_TOOL_ERROR_BYTES: usize = 2 * 1024;
const MAX_TOOL_ERROR_ROWS: usize = 6;
const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
const MAX_TOOL_ARGUMENT_ROWS: usize = 8;
const MAX_TOOL_RESULT_ROWS: usize = 12;
/// Rows of live streamed output shown under a running call's one-liner.
const MAX_LIVE_TAIL_ROWS: usize = 6;
const TOOL_SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: &str = env!("QQ_GIT_COMMIT");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    color: Option<Color>,
    background: Option<Color>,
    bold: bool,
    dim: bool,
    italic: bool,
}

impl Style {
    const fn color(color: Color) -> Self {
        Self {
            color: Some(color),
            background: None,
            bold: false,
            dim: false,
            italic: false,
        }
    }

    const fn on(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    text: String,
    style: Style,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Line {
    spans: Vec<Span>,
}

impl Line {
    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span {
                text: text.into(),
                style,
            }],
        }
    }

    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(Span { text, style });
    }

    fn width(&self) -> usize {
        self.spans
            .iter()
            .flat_map(|span| span.text.chars())
            .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
            .sum()
    }

    fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }
}

fn normal() -> Style {
    Style::color(Color::White)
}

fn muted() -> Style {
    Style::color(Color::DarkGrey).dim()
}

fn accent() -> Style {
    Style::color(Color::Cyan)
}

fn brand() -> Style {
    Style::color(Color::Rgb {
        r: 255,
        g: 159,
        b: 67,
    })
}

fn warning() -> Style {
    Style::color(Color::Yellow)
}

fn failure() -> Style {
    Style::color(Color::Red)
}

fn success() -> Style {
    Style::color(Color::Green)
}

/// Dark surface tint behind code-block panels, distinct from the terminal
/// background so a padded block reads as one solid slab.
const SURFACE_COLOR: Color = Color::Rgb {
    r: 38,
    g: 40,
    b: 48,
};

fn surface(style: Style) -> Style {
    style.on(SURFACE_COLOR)
}

/// Unified-diff line coloring: additions green, removals red, hunk headers in
/// the muted accent, context lines normal. Diff lines never reflow.
fn diff_line_style(line: &str) -> Style {
    if line.starts_with("@@") {
        accent().dim()
    } else if line.starts_with('+') {
        success()
    } else if line.starts_with('-') {
        failure()
    } else {
        normal()
    }
}

#[derive(Default)]
pub(crate) struct FrameRenderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
    markdown: HashMap<MessageId, CachedMarkdown>,
}

struct CachedMarkdown {
    width: usize,
    lines: Vec<Line>,
}

impl FrameRenderer {
    pub fn draw(&mut self, app: &mut App) -> io::Result<Vec<u8>> {
        let actual_size = terminal::size()?;
        let width = actual_size.0.clamp(1, MAX_RENDER_WIDTH);
        let height = actual_size.1.clamp(1, MAX_RENDER_HEIGHT);
        let frame = self.frame(app, usize::from(width), usize::from(height));
        let resized = self.size != Some(actual_size);
        let mut output = Vec::with_capacity(4096);
        queue!(&mut output, BeginSynchronizedUpdate)?;
        if resized {
            queue!(&mut output, Clear(ClearType::All))?;
        }
        for (row, line) in frame.iter().enumerate() {
            if resized || self.previous.get(row) != Some(line) {
                queue!(
                    &mut output,
                    MoveTo(
                        0,
                        u16::try_from(row).expect("bounded terminal row fits u16")
                    ),
                    SetAttribute(Attribute::Reset),
                    ResetColor,
                    Clear(ClearType::CurrentLine)
                )?;
                write_line(&mut output, line)?;
            }
        }
        queue!(
            &mut output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            EndSynchronizedUpdate
        )?;
        self.previous = frame;
        self.size = Some(actual_size);
        Ok(output)
    }

    fn frame(&mut self, app: &mut App, width: usize, height: usize) -> Vec<Line> {
        self.prune_markdown(app);
        if width < 32 || height < 9 {
            return fit_height(
                vec![
                    Line::styled(" qq", brand().bold()),
                    Line::default(),
                    Line::styled("Terminal is too small.", warning()),
                    Line::styled("Resize to at least 32 x 9. Ctrl-C exits.", muted()),
                ],
                height,
            );
        }

        let mut lines = vec![header(app, width), context(app, width)];
        let body_height = height.saturating_sub(5);
        let overlay = app.model_picker.is_some()
            || app.session_picker.is_some()
            || app.pending_approval().is_some();
        let body = if app.model_picker.is_some() {
            model_picker(app, width, body_height)
        } else if app.session_picker.is_some() {
            session_picker(app, width, body_height)
        } else if app.pending_approval().is_some() {
            approval_prompt(app, width, body_height)
        } else {
            match app.layout {
                Layout::Threadline => self.threadline(app, width),
                Layout::FoldFocus => self.fold_focus(app, width),
            }
        };
        let mut body = if overlay {
            body
        } else {
            app.update_transcript_viewport(body.len(), body_height);
            transcript_viewport(body, body_height, app.transcript_scroll_offset())
        };
        if !overlay {
            overlay_slash_autocomplete(&mut body, slash_autocomplete(app, width, body_height));
        }
        lines.extend(body);
        lines.push(composer(app, width));
        lines.push(footer_context(app, width));
        lines.push(footer_workspace(app, width));
        fit_height(lines, height)
    }

    fn prune_markdown(&mut self, app: &App) {
        let visible = if app.session_picker.is_none()
            && app.model_picker.is_none()
            && app.pending_approval().is_none()
        {
            app.focused
                .and_then(|session_id| app.sessions.get(&session_id))
                .and_then(|session| session.messages.as_ref())
                .map(|messages| {
                    let limit = match app.layout {
                        Layout::Threadline => MAX_VISIBLE_MESSAGES,
                        Layout::FoldFocus => 2,
                    };
                    messages
                        .iter()
                        .rev()
                        .take(limit)
                        .map(|message| message.id)
                        .collect::<HashSet<_>>()
                })
        } else {
            None
        };
        match visible {
            Some(visible) => self.markdown.retain(|id, _| visible.contains(id)),
            None => self.markdown.clear(),
        }
    }

    fn threadline(&mut self, app: &App, width: usize) -> Vec<Line> {
        let mut lines = vec![section(
            "THREADLINE",
            "conversation with child work in one chronology",
        )];
        lines.push(Line::default());
        lines.extend(self.transcript(app, width));
        if let Some(focused) = app.focused {
            let children = child_sessions(app, focused);
            if !children.is_empty() {
                lines.push(Line::default());
                lines.push(Line::styled("  +-- related sessions", muted().bold()));
                for child in children {
                    lines.push(session_line(app, child, width, "     "));
                }
            }
        }
        lines
    }

    fn fold_focus(&mut self, app: &App, width: usize) -> Vec<Line> {
        let content_width = width.min(96);
        let mut lines = vec![section(
            "FOLD / FOCUS",
            "history and parallel work compressed around now",
        )];
        lines.push(Line::default());
        let Some(session_id) = app.focused else {
            lines.push(Line::styled("  Alt-N creates the first session.", muted()));
            return lines;
        };
        let Some(messages) = app
            .sessions
            .get(&session_id)
            .and_then(|session| session.messages.as_ref())
        else {
            lines.push(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return lines;
        };
        if messages.len() > 2 {
            lines.push(Line::styled(
                format!("  > {} earlier messages folded", messages.len() - 2),
                accent(),
            ));
            lines.push(Line::default());
        }
        for message in messages.iter().skip(messages.len().saturating_sub(2)) {
            lines.extend(self.render_message(message, content_width));
            lines.push(Line::default());
        }
        for prompt in app.pending_prompts(session_id) {
            let mut line = Line::styled("  YOU / PENDING  ", warning().bold());
            line.push(
                preview(prompt, content_width.saturating_sub(18)),
                muted().italic(),
            );
            lines.push(line);
        }
        for child in child_sessions(app, session_id) {
            lines.push(session_line(app, child, content_width, "  > "));
        }
        lines
    }

    fn transcript(&mut self, app: &App, width: usize) -> Vec<Line> {
        let Some(session_id) = app.focused else {
            return vec![Line::styled("  Alt-N creates the first session.", muted())];
        };
        let Some(session) = app.sessions.get(&session_id) else {
            return vec![Line::styled(
                "  Loading session history...",
                muted().italic(),
            )];
        };
        let Some(messages) = session.messages.as_ref() else {
            return vec![Line::styled(
                "  Loading session history...",
                muted().italic(),
            )];
        };
        let tool_calls = session.tool_calls.as_deref().unwrap_or_default();
        let mut lines = Vec::new();
        let hidden = messages.len().saturating_sub(MAX_VISIBLE_MESSAGES);
        if hidden > 0 {
            lines.push(Line::styled(
                format!("  {hidden} earlier messages outside the viewport"),
                muted(),
            ));
        }
        for (index, message) in messages.iter().enumerate().skip(hidden) {
            if !lines.is_empty() {
                lines.push(Line::default());
                // A user prompt starts a new turn; extra spacing keeps
                // prompt/response boundaries scannable.
                if message.role == MessageRole::User {
                    lines.push(Line::default());
                }
            }
            if message.role == MessageRole::Assistant {
                // Group calls under the assistant message of their turn.
                // Calls from turns without a message of their own (call-only
                // turns, legacy turn 0 messages) attach after the nearest
                // preceding assistant message of the run; the run's first
                // rendered message also collects any earlier orphan turns.
                let first_of_run = !messages[..index].iter().any(|earlier| {
                    earlier.role == MessageRole::Assistant && earlier.run_id == message.run_id
                });
                let next_turn = messages[index + 1..]
                    .iter()
                    .find(|later| {
                        later.role == MessageRole::Assistant && later.run_id == message.run_id
                    })
                    .map_or(u16::MAX, |later| later.turn_ordinal);
                let mut run_calls = tool_calls
                    .iter()
                    .filter(|tool_call| {
                        tool_call.run_id == message.run_id
                            && tool_call.turn_ordinal < next_turn
                            && (first_of_run || tool_call.turn_ordinal >= message.turn_ordinal)
                    })
                    .collect::<Vec<_>>();
                run_calls.sort_by_key(|tool_call| (tool_call.turn_ordinal, tool_call.call_ordinal));
                // Call-only turns before the run's first message executed
                // before its text streamed: render that head group ahead of
                // the message so execution order holds from the first block.
                let head = if first_of_run {
                    run_calls
                        .iter()
                        .take_while(|tool_call| tool_call.turn_ordinal < message.turn_ordinal)
                        .count()
                } else {
                    0
                };
                if head > 0 {
                    lines.extend(render_tool_calls(
                        &run_calls[..head],
                        &app.live_tool_output,
                        app.tool_detail,
                        app.animation_tick,
                        width,
                    ));
                    lines.push(Line::default());
                }
                lines.extend(self.render_message(message, width));
                if run_calls.len() > head {
                    lines.push(Line::default());
                    lines.extend(render_tool_calls(
                        &run_calls[head..],
                        &app.live_tool_output,
                        app.tool_detail,
                        app.animation_tick,
                        width,
                    ));
                }
            } else {
                lines.extend(self.render_message(message, width));
            }
        }
        for prompt in app.pending_prompts(session_id) {
            // A pending prompt is a YOU boundary: the same two blank lines
            // that precede any user turn.
            if !lines.is_empty() {
                lines.push(Line::default());
                lines.push(Line::default());
            }
            let mut line = Line::styled(" ▌ ", warning());
            line.push("YOU  pending", warning().bold());
            lines.push(line);
            lines.extend(indent_lines(
                bounded_markdown_lines(prompt, width.saturating_sub(3)),
                " ▌ ",
                warning(),
                width,
            ));
        }
        if lines.is_empty() {
            lines.push(Line::styled(
                "  Ask QQ to begin this session.",
                muted().italic(),
            ));
        }
        lines
    }

    fn render_message(&mut self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        // User turns carry an accent bar so prompt boundaries stand out.
        let (prefix, prefix_style, role, role_style) = match message.role {
            MessageRole::User => (" ▌ ", accent(), "YOU", accent().bold()),
            MessageRole::Assistant => ("   ", muted(), "QQ", normal().bold()),
        };
        let mut header = Line::styled(prefix, prefix_style);
        header.push(role, role_style);
        if !matches!(message.state, MessageState::Complete) {
            header.push(
                format!("  {}", message_state_label(message.state)),
                status_style(message.state),
            );
        }
        let mut lines = vec![truncate_line(header, width)];
        let content_width = width.saturating_sub(3).max(1);
        let terminal = matches!(
            message.state,
            MessageState::Complete
                | MessageState::Cancelled
                | MessageState::Failed
                | MessageState::Interrupted
        );
        let body = if terminal {
            if let Some(cached) = self
                .markdown
                .get(&message.id)
                .filter(|cached| cached.width == content_width)
            {
                cached.lines.clone()
            } else {
                let content = message_content(message);
                let lines = bounded_markdown_lines(&content, content_width);
                if !self.markdown.contains_key(&message.id)
                    && self.markdown.len() >= MAX_VISIBLE_MESSAGES
                    && let Some(stale) = self.markdown.keys().next().copied()
                {
                    self.markdown.remove(&stale);
                }
                self.markdown.insert(
                    message.id,
                    CachedMarkdown {
                        width: content_width,
                        lines: lines.clone(),
                    },
                );
                lines
            }
        } else {
            bounded_markdown_lines(&message_content(message), content_width)
        };
        if body.is_empty() {
            let mut ellipsis = Line::styled(prefix, prefix_style);
            ellipsis.push("...", muted());
            lines.push(ellipsis);
        } else {
            lines.extend(indent_lines(body, prefix, prefix_style, width));
        }
        lines
    }
}

/// Renders one run's tool calls: a folded count for quiet runs, otherwise one
/// gutter line per call, with errors and the expanded detail level adding
/// bounded body rows. Running calls with buffered live output show a bounded
/// tail of it at every detail level — a running command's output is the thing
/// the user is waiting for.
fn render_tool_calls(
    calls: &[&ToolCallSnapshot],
    live_output: &HashMap<ToolCallId, String>,
    detail: ToolDetail,
    tick: usize,
    width: usize,
) -> Vec<Line> {
    let quiet = |call: &ToolCallSnapshot| call.state == ToolCallState::Completed && !call.is_error;
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
    }
    lines
}

fn tool_fold_line(calls: &[&ToolCallSnapshot], width: usize) -> Line {
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
fn tool_summary_line(call: &ToolCallSnapshot, tick: usize, width: usize) -> Line {
    let (glyph, glyph_style) = tool_state_glyph(call, tick);
    let mut line = Line::styled("   ", muted());
    line.push(glyph, glyph_style);
    line.push(" ", muted());
    line.push(call.name.as_str(), normal().dim());
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

fn tool_state_glyph(call: &ToolCallSnapshot, tick: usize) -> (&'static str, Style) {
    match call.state {
        ToolCallState::Running => (TOOL_SPINNER[tick % TOOL_SPINNER.len()], accent()),
        ToolCallState::Requested => ("◌", muted()),
        ToolCallState::Completed => {
            if call.is_error {
                ("✗", failure())
            } else {
                ("●", muted())
            }
        }
        ToolCallState::Failed | ToolCallState::Denied => ("✗", failure()),
        ToolCallState::AwaitingApproval => ("◇", warning()),
        ToolCallState::Interrupted => ("◌", muted()),
    }
}

/// The most informative argument for known tools; a compact truncated argument
/// preview otherwise, so new tool names degrade gracefully. Malformed JSON
/// falls back to the raw truncated string.
fn tool_subject(call: &ToolCallSnapshot) -> Option<String> {
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
fn tool_result_metric(call: &ToolCallSnapshot) -> Option<String> {
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
fn tool_error_lines(result: &str, width: usize) -> Vec<Line> {
    let text = bounded_tail(result, MAX_TOOL_ERROR_BYTES);
    let total = text.lines().count();
    let mut lines = Vec::new();
    if total > MAX_TOOL_ERROR_ROWS || text.len() < result.len() {
        lines.push(Line::styled("     ...", muted().italic()));
    }
    for line in text.lines().skip(total.saturating_sub(MAX_TOOL_ERROR_ROWS)) {
        lines.push(truncate_line(
            Line::styled(format!("     {line}"), failure().dim()),
            width,
        ));
    }
    lines
}

/// Expanded detail: bounded pretty-printed arguments plus a bounded tail of
/// the result. Oversized or malformed arguments render as a raw bounded tail.
fn tool_expanded_lines(call: &ToolCallSnapshot, width: usize) -> Vec<Line> {
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
            let style = if diff {
                diff_line_style(line)
            } else {
                normal().dim()
            };
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
fn tool_live_output_lines(output: &str, width: usize) -> Vec<Line> {
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
fn looks_like_diff(text: &str) -> bool {
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

fn count_noun(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn format_result_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}.{} KB", bytes / 1024, (bytes % 1024) * 10 / 1024)
    }
}

const fn tool_state_label(state: ToolCallState) -> &'static str {
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

fn message_content(message: &MessageSnapshot) -> Cow<'_, str> {
    if message.refusal.is_empty() {
        return Cow::Borrowed(bounded_tail(&message.output, MAX_MARKDOWN_BYTES));
    }
    if message.output.is_empty() {
        return Cow::Borrowed(bounded_tail(&message.refusal, MAX_MARKDOWN_BYTES));
    }

    let refusal = bounded_tail(&message.refusal, MAX_MARKDOWN_BYTES.saturating_sub(2));
    let output_bytes = MAX_MARKDOWN_BYTES.saturating_sub(refusal.len() + 2);
    let output = bounded_tail(&message.output, output_bytes);
    Cow::Owned(format!("{output}\n\n{refusal}"))
}

fn header(app: &App, width: usize) -> Line {
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

fn context(app: &App, width: usize) -> Line {
    let mut line = Line::styled("  ", muted());
    if let Some(focused) = app.focused {
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
    if let Some(status) = &app.status {
        line.push(format!("  |  {status}"), warning());
    }
    truncate_line(line, width)
}

fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let picker = app.session_picker.as_ref().expect("session picker is open");
    let filtered = app.filtered_sessions();
    let mut lines = vec![section(
        "SESSIONS",
        "type to search, Up/Down select, Enter focuses, Esc closes",
    )];
    lines.push(Line::styled(
        format!(
            "  search: {}",
            if picker.query.is_empty() {
                "all sessions"
            } else {
                &picker.query
            }
        ),
        if picker.query.is_empty() {
            muted()
        } else {
            accent()
        },
    ));
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
        let depth = app.depth(session_id);
        let selected = picker.selected == Some(session_id);
        if selected {
            selected_row = results.len();
        }
        let prefix = format!(
            "  {}{} ",
            "  ".repeat(depth),
            if selected { ">" } else { " " }
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

fn model_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
    let picker = app.model_picker.as_ref().expect("model picker is open");
    let filtered = app.filtered_models();
    let mut lines = vec![section(
        "MODELS",
        "type to search, Up/Down select, Enter creates session, Esc closes",
    )];
    lines.push(Line::styled(
        format!(
            "  search: {}",
            if picker.query.is_empty() {
                "all models"
            } else {
                &picker.query
            }
        ),
        if picker.query.is_empty() {
            muted()
        } else {
            accent()
        },
    ));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::styled("  No matching models.", muted().italic()));
        return fit_height(lines, height);
    }

    let mut results = Vec::new();
    let mut selected_row = 0;
    let mut provider = None;
    for (position, index) in filtered.iter().enumerate() {
        let option = &app.models[*index];
        if provider != Some(option.provider.as_str()) {
            provider = Some(&option.provider);
            results.push(Line::styled(
                format!("  {}", option.provider.to_ascii_uppercase()),
                accent().bold(),
            ));
        }
        let selected = position == picker.selected.min(filtered.len() - 1);
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

fn approval_prompt(app: &App, width: usize, height: usize) -> Vec<Line> {
    let tool_call = app.pending_approval().expect("an approval is pending");
    let mut lines = vec![section(
        "TOOL APPROVAL",
        "y approves once, a approves for this session, n or Esc denies",
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
        "  [y] approve once   [a] approve for session   [n]/[Esc] deny",
        accent().bold(),
    ));
    fit_height(lines, height)
}

/// Shell approvals surface the exact command so the user can decide in place.
fn shell_command_preview(tool_call: &ToolCallSnapshot) -> Option<String> {
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

fn child_sessions(app: &App, parent: SessionId) -> Vec<SessionId> {
    let mut children = app
        .sessions
        .values()
        .filter(|session| session.summary.parent_id == Some(parent))
        .map(|session| session.summary.id)
        .collect::<Vec<_>>();
    children.sort_by_key(|id| app.sessions[id].summary.updated_at_ms);
    children
}

fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let session = &app.sessions[&session_id].summary;
    let (marker, style) = match session.status {
        SessionStatus::Idle => match session.last_outcome.as_ref() {
            Some(qq_protocol::RunOutcome::Completed) => (".", accent()),
            Some(qq_protocol::RunOutcome::Cancelled) => ("x", warning()),
            Some(qq_protocol::RunOutcome::Interrupted) => ("!", warning()),
            Some(qq_protocol::RunOutcome::Failed { .. }) => ("!", failure()),
            None => ("o", muted()),
        },
        SessionStatus::Queued => ("+", warning()),
        SessionStatus::Running => (["/", "-", "\\", "|"][app.animation_tick % 4], accent()),
    };
    let mut line = Line::styled(prefix, muted());
    line.push(format!("{marker}  "), style);
    line.push(
        &session.title,
        if app.focused == Some(session_id) {
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

fn composer(app: &App, width: usize) -> Line {
    let mut line = Line::styled(" > ", accent().bold());
    if app.input.is_empty() {
        line.push("Ask QQ...", muted().italic());
    } else {
        line.push(tail_by_width(&app.input, width.saturating_sub(5)), normal());
    }
    line.push(
        if app.animation_tick.is_multiple_of(2) {
            "|"
        } else {
            " "
        },
        accent(),
    );
    truncate_line(line, width)
}

fn footer_context(app: &App, width: usize) -> Line {
    let context = match app.focused_context_usage() {
        Some((tokens, limit)) if limit > 0 => {
            let tenths = u128::from(tokens) * 1_000 / u128::from(limit);
            format!(" context: {}.{}% / {limit}", tenths / 10, tenths % 10)
        }
        // Unknown usage reads as an empty gauge, never as a gap in the UI.
        Some(_) | None => " context: 0.0%".to_owned(),
    };
    let focused = app
        .focused
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

fn footer_workspace(app: &App, width: usize) -> Line {
    let workspace = if app.workspace_path.is_empty() {
        "cwd: connecting".to_owned()
    } else {
        format!("cwd: {}", app.workspace_path)
    };
    // An unknown cost reads as zero spend, never as a gap in the UI.
    let cost = app
        .focused
        .and_then(|id| app.sessions.get(&id))
        .and_then(|session| session.summary.estimated_cost_usd_nanos)
        .unwrap_or(0);
    let cost = format!("cost: {} ", format_cost(cost));
    align_sides(
        Line::styled(format!(" {workspace}"), muted()),
        Line::styled(cost, accent()),
        width,
    )
}

fn slash_autocomplete(app: &App, width: usize, height: usize) -> Vec<Line> {
    let commands = app.filtered_slash_commands();
    let selected = app.slash_selected().min(commands.len().saturating_sub(1));
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
            line.push(format!("  {}", command.description), muted());
            truncate_line(line, width)
        })
        .collect()
}

fn overlay_slash_autocomplete(body: &mut [Line], autocomplete: Vec<Line>) {
    let start = body.len().saturating_sub(autocomplete.len());
    for (target, line) in body[start..].iter_mut().zip(autocomplete) {
        *target = line;
    }
}

fn align_sides(mut left: Line, right: Line, width: usize) -> Line {
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

fn format_cost(usd_nanos: u64) -> String {
    let whole = usd_nanos / 1_000_000_000;
    let micros = (usd_nanos % 1_000_000_000) / 1_000;
    let mut fractional = format!("{micros:06}");
    while fractional.len() > 2 && fractional.ends_with('0') {
        fractional.pop();
    }
    format!("${whole}.{fractional}")
}

fn section(title: &str, subtitle: &str) -> Line {
    let mut line = Line::styled(format!(" {title} "), accent().bold());
    line.push(subtitle, muted());
    line
}

fn message_state_label(state: MessageState) -> &'static str {
    match state {
        MessageState::Queued => "queued",
        MessageState::Streaming => "streaming",
        MessageState::Complete => "complete",
        MessageState::Cancelled => "cancelled",
        MessageState::Failed => "failed",
        MessageState::Interrupted => "interrupted",
    }
}

fn status_style(state: MessageState) -> Style {
    match state {
        MessageState::Queued => warning(),
        MessageState::Streaming => accent(),
        MessageState::Complete => muted(),
        MessageState::Cancelled | MessageState::Interrupted => warning(),
        MessageState::Failed => failure(),
    }
}

fn markdown_lines(source: &str, width: usize) -> Vec<Line> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::default()];
    // Lines marked literal (code blocks, laid-out tables) keep character
    // wrapping so column alignment survives; prose lines wrap at words.
    let mut literal = vec![false];
    let mut styles = vec![normal()];
    let mut list_depth = 0_usize;
    let mut table: Option<TableBuffer> = None;
    let mut code_block: Option<CodeBlockBuffer> = None;
    let parser = Parser::new_ext(source, Options::all());
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    ensure_line(&mut lines);
                    // A blank line above the heading separates it from the
                    // preceding block; a leading heading stays flush.
                    if lines.len() > 1 {
                        lines.push(Line::default());
                    }
                    styles.push(accent().bold());
                }
                Tag::Strong => {
                    let mut style = *styles.last().expect("base style remains");
                    style.bold = true;
                    styles.push(style);
                }
                Tag::Emphasis => {
                    let mut style = *styles.last().expect("base style remains");
                    style.italic = true;
                    styles.push(style);
                }
                Tag::CodeBlock(kind) => {
                    ensure_line(&mut lines);
                    code_block = Some(CodeBlockBuffer::new(&kind));
                }
                Tag::List(_) => list_depth += 1,
                Tag::Item => {
                    ensure_line(&mut lines);
                    lines.last_mut().expect("line exists").push(
                        format!("{}- ", "  ".repeat(list_depth.saturating_sub(1))),
                        accent(),
                    );
                }
                Tag::BlockQuote(_) => {
                    ensure_line(&mut lines);
                    lines.last_mut().expect("line exists").push("> ", muted());
                }
                Tag::Table(_) => table = Some(TableBuffer::default()),
                Tag::TableHead => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.has_header = true;
                        buffer.begin_row();
                    }
                    let mut style = *styles.last().expect("base style remains");
                    style.bold = true;
                    styles.push(style);
                }
                Tag::TableRow => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.begin_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.begin_cell();
                    }
                }
                Tag::Link { .. }
                | Tag::Image { .. }
                | Tag::FootnoteDefinition(_)
                | Tag::HtmlBlock
                | Tag::DefinitionList
                | Tag::DefinitionListTitle
                | Tag::DefinitionListDefinition
                | Tag::Strikethrough
                | Tag::Subscript
                | Tag::Superscript
                | Tag::MetadataBlock(_) => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) => {
                    ensure_line(&mut lines);
                    if matches!(tag, TagEnd::Heading(_)) {
                        styles.pop();
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(buffer) = code_block.take() {
                        let rendered = layout_code_panel(&buffer, width.max(1));
                        if lines.last().is_some_and(Line::is_empty) {
                            lines.pop();
                            literal.pop();
                        }
                        literal.resize(lines.len(), false);
                        lines.extend(rendered);
                        literal.resize(lines.len(), true);
                        lines.push(Line::default());
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    styles.pop();
                }
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                TagEnd::Item => ensure_line(&mut lines),
                TagEnd::Table => {
                    if let Some(mut buffer) = table.take() {
                        buffer.end_row();
                        let rendered = layout_table(&buffer.rows, buffer.has_header, width.max(1));
                        if !rendered.is_empty() {
                            if lines.last().is_some_and(Line::is_empty) {
                                lines.pop();
                                literal.pop();
                            }
                            literal.resize(lines.len(), false);
                            lines.extend(rendered);
                            literal.resize(lines.len(), true);
                            lines.push(Line::default());
                        }
                    }
                }
                TagEnd::TableHead => {
                    styles.pop();
                    if let Some(buffer) = table.as_mut() {
                        buffer.end_row();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(buffer) = table.as_mut() {
                        buffer.end_row();
                    }
                }
                TagEnd::Link
                | TagEnd::Image
                | TagEnd::FootnoteDefinition
                | TagEnd::HtmlBlock
                | TagEnd::DefinitionList
                | TagEnd::DefinitionListTitle
                | TagEnd::DefinitionListDefinition
                | TagEnd::Strikethrough
                | TagEnd::Subscript
                | TagEnd::Superscript
                | TagEnd::TableCell
                | TagEnd::MetadataBlock(_) => {}
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if let Some(buffer) = code_block.as_mut() {
                    buffer.text.push_str(&text);
                } else {
                    let style = *styles.last().expect("base style remains");
                    match table.as_mut() {
                        Some(buffer) => buffer.append(&text, style),
                        None => append_safe_text(&mut lines, &text, style),
                    }
                }
            }
            Event::Code(code) => {
                push_inline(table.as_mut(), &mut lines, &code, warning().bold());
            }
            // A soft break is a source-formatting line break: render it as a
            // space so paragraphs reflow to the terminal width.
            Event::SoftBreak => {
                push_inline(
                    table.as_mut(),
                    &mut lines,
                    " ",
                    *styles.last().expect("base style remains"),
                );
            }
            Event::HardBreak => match table.as_mut() {
                Some(buffer) => buffer.append(" ", normal()),
                None => lines.push(Line::default()),
            },
            Event::Rule => {
                ensure_line(&mut lines);
                lines.push(Line::styled("------------", muted()));
                lines.push(Line::default());
            }
            Event::TaskListMarker(checked) => push_inline(
                table.as_mut(),
                &mut lines,
                if checked { "[x] " } else { "[ ] " },
                accent(),
            ),
            Event::FootnoteReference(reference) => push_inline(
                table.as_mut(),
                &mut lines,
                &format!("[{reference}]"),
                accent(),
            ),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                push_inline(table.as_mut(), &mut lines, &format!("${math}$"), warning());
            }
        }
        literal.resize(lines.len(), false);
    }
    while lines.last().is_some_and(Line::is_empty) {
        lines.pop();
        literal.pop();
    }
    lines
        .into_iter()
        .zip(literal)
        .flat_map(|(line, literal)| {
            if literal {
                wrap_line_chars(line, width.max(1))
            } else {
                wrap_line(line, width.max(1))
            }
        })
        .collect()
}

/// Routes inline content to the open table cell when one exists, otherwise to
/// the current transcript line.
fn push_inline(table: Option<&mut TableBuffer>, lines: &mut [Line], text: &str, style: Style) {
    match table {
        Some(buffer) => buffer.append(text, style),
        None => lines
            .last_mut()
            .expect("line exists")
            .push(text.to_owned(), style),
    }
}

/// Narrowest useful column; below this per column the table stacks instead.
const TABLE_MIN_COLUMN_WIDTH: usize = 3;
/// Display width of the " │ " column separator.
const TABLE_SEPARATOR_WIDTH: usize = 3;

/// Buffers one table's rows of styled cells while the parser walks it, so the
/// layout can size columns from complete content.
#[derive(Default)]
struct TableBuffer {
    rows: Vec<Vec<Line>>,
    row: Option<Vec<Line>>,
    has_header: bool,
}

impl TableBuffer {
    fn begin_row(&mut self) {
        self.end_row();
        self.row = Some(Vec::new());
    }

    fn end_row(&mut self) {
        if let Some(row) = self.row.take()
            && !row.is_empty()
        {
            self.rows.push(row);
        }
    }

    fn begin_cell(&mut self) {
        self.row.get_or_insert_default().push(Line::default());
    }

    /// Appends inline content to the current cell, creating row and cell on
    /// demand so malformed or partial input never panics.
    fn append(&mut self, text: &str, style: Style) {
        let row = self.row.get_or_insert_default();
        if row.is_empty() {
            row.push(Line::default());
        }
        let safe = text
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        row.last_mut().expect("cell exists").push(safe, style);
    }
}

/// Lays a buffered table out as aligned columns sized from content. When the
/// natural table overflows the width, columns shrink proportionally and cells
/// wrap within their column; when even minimum columns cannot fit, rows stack
/// as `header: value` lines.
fn layout_table(rows: &[Vec<Line>], has_header: bool, width: usize) -> Vec<Line> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let overhead = TABLE_SEPARATOR_WIDTH * (columns - 1);
    let available = width.saturating_sub(overhead);
    if available < columns * TABLE_MIN_COLUMN_WIDTH {
        return layout_table_stacked(rows, has_header, width);
    }
    let natural = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row.get(column).map_or(0, Line::width))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect::<Vec<_>>();
    let mut widths = natural.clone();
    if natural.iter().sum::<usize>() > available {
        // Columns already within their fair share keep their natural width;
        // repeat because each fixed column raises the fair share of the rest.
        let mut remaining = available;
        let mut flexible = (0..columns).collect::<Vec<_>>();
        loop {
            let fair = remaining / flexible.len().max(1);
            let (fits, wide): (Vec<usize>, Vec<usize>) = flexible
                .into_iter()
                .partition(|column| natural[*column] <= fair);
            if fits.is_empty() || wide.is_empty() {
                flexible = if wide.is_empty() { fits } else { wide };
                break;
            }
            for column in fits {
                remaining = remaining.saturating_sub(natural[column]);
            }
            flexible = wide;
        }
        // The overflowing columns split the remaining space proportionally.
        let flexible_total = flexible
            .iter()
            .map(|column| natural[*column])
            .sum::<usize>()
            .max(1);
        for column in flexible {
            widths[column] = (natural[column] * remaining / flexible_total)
                .max(TABLE_MIN_COLUMN_WIDTH)
                .min(natural[column].max(TABLE_MIN_COLUMN_WIDTH));
        }
        // Rounding and minimums can leave a small excess; take it back from
        // the widest columns so the sum fits `available` again.
        while widths.iter().sum::<usize>() > available {
            let Some((index, _)) = widths
                .iter()
                .enumerate()
                .filter(|(_, allocated)| **allocated > TABLE_MIN_COLUMN_WIDTH)
                .max_by_key(|(_, allocated)| **allocated)
            else {
                break;
            };
            widths[index] -= 1;
        }
    }
    let mut output = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let cells = (0..columns)
            .map(|column| wrap_line(row.get(column).cloned().unwrap_or_default(), widths[column]))
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for cell_row in 0..height {
            let mut line = Line::default();
            for (column, cell) in cells.iter().enumerate() {
                if column > 0 {
                    line.push(" │ ", muted());
                }
                let content = cell.get(cell_row).cloned().unwrap_or_default();
                let content_width = content.width();
                for span in content.spans {
                    line.push(span.text, span.style);
                }
                if column + 1 < columns {
                    line.push(
                        " ".repeat(widths[column].saturating_sub(content_width)),
                        muted(),
                    );
                }
            }
            output.push(line);
        }
        if row_index == 0 && has_header && rows.len() > 1 {
            let mut rule = Line::default();
            for (column, column_width) in widths.iter().enumerate() {
                if column > 0 {
                    rule.push("─┼─", muted());
                }
                rule.push("─".repeat(*column_width), muted());
            }
            output.push(rule);
        }
    }
    output
}

/// Very-narrow fallback: each data row becomes `header: value` lines with a
/// muted divider between rows.
fn layout_table_stacked(rows: &[Vec<Line>], has_header: bool, width: usize) -> Vec<Line> {
    let (header, data) = if has_header && rows.len() > 1 {
        (rows.first(), &rows[1..])
    } else {
        (None, rows)
    };
    let mut output = Vec::new();
    for (row_index, row) in data.iter().enumerate() {
        if row_index > 0 {
            output.push(Line::styled("---", muted()));
        }
        for (column, cell) in row.iter().enumerate() {
            let mut line = Line::default();
            if let Some(title) = header.and_then(|header| header.get(column)) {
                for span in &title.spans {
                    line.push(span.text.clone(), span.style);
                }
                line.push(": ", muted());
            }
            for span in &cell.spans {
                line.push(span.text.clone(), span.style);
            }
            output.extend(wrap_line(line, width.max(1)));
        }
    }
    output
}

/// The panel's left border glyph plus one cell of padding.
const CODE_PANEL_GUTTER: &str = "│ ";
/// Display width of [`CODE_PANEL_GUTTER`].
const CODE_PANEL_GUTTER_WIDTH: usize = 2;

/// Buffers one code block's text while the parser walks it, so the panel can
/// be laid out from complete content. Streamed partial input closes the block
/// at end of input, so an unterminated fence still renders as a panel.
struct CodeBlockBuffer {
    language: Option<String>,
    text: String,
}

impl CodeBlockBuffer {
    fn new(kind: &CodeBlockKind) -> Self {
        let language = match kind {
            CodeBlockKind::Fenced(info) => info
                .split([',', ' ', '\t'])
                .next()
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            CodeBlockKind::Indented => None,
        };
        Self {
            language,
            text: String::new(),
        }
    }
}

/// Lays a buffered code block out as a full-width tinted panel: a padding row
/// carrying the right-aligned language label, character-wrapped content rows,
/// and a closing padding row. Every row is padded to `width` so the tint
/// reads as one solid panel rather than ragged highlights.
fn layout_code_panel(block: &CodeBlockBuffer, width: usize) -> Vec<Line> {
    let diff = block.language.as_deref() == Some("diff");
    let content_width = width.saturating_sub(CODE_PANEL_GUTTER_WIDTH).max(1);
    let mut top = Line::default();
    if let Some(language) = block.language.as_deref() {
        let label = language
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        let mut labelled = Line::styled(label, muted());
        let label_width = labelled.width();
        if label_width > 0 && label_width <= content_width {
            top.push(" ".repeat(content_width - label_width), muted());
            top.spans.append(&mut labelled.spans);
        }
    }
    let mut output = vec![code_panel_row(top, width)];
    for source_line in block.text.lines() {
        let safe = source_line
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        let style = if diff {
            diff_line_style(&safe)
        } else {
            normal()
        };
        for wrapped in wrap_line_chars(Line::styled(safe, style), content_width) {
            output.push(code_panel_row(wrapped, width));
        }
    }
    output.push(code_panel_row(Line::default(), width));
    output
}

/// One physical panel row: the bordered gutter, the content, and enough
/// trailing padding to carry the background tint to the full width.
fn code_panel_row(content: Line, width: usize) -> Line {
    let mut row = Line::styled(CODE_PANEL_GUTTER, surface(accent().dim()));
    let content_width = content.width();
    for span in content.spans {
        row.push(span.text, surface(span.style));
    }
    row.push(
        " ".repeat(width.saturating_sub(CODE_PANEL_GUTTER_WIDTH + content_width)),
        surface(normal()),
    );
    row
}

fn bounded_markdown_lines(source: &str, width: usize) -> Vec<Line> {
    let mut lines = markdown_lines(bounded_tail(source, MAX_MARKDOWN_BYTES), width);
    let excess = lines.len().saturating_sub(MAX_CACHED_MARKDOWN_ROWS);
    if excess > 0 {
        lines.drain(..excess);
    }
    lines
}

fn append_safe_text(lines: &mut Vec<Line>, text: &str, style: Style) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        let safe = part
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        lines.last_mut().expect("line exists").push(safe, style);
    }
}

fn ensure_line(lines: &mut Vec<Line>) {
    if !lines.last().is_none_or(Line::is_empty) {
        lines.push(Line::default());
    }
}

/// A run of characters that wraps as one unit: either whitespace or a word.
struct WrapToken {
    whitespace: bool,
    width: usize,
    characters: Vec<(char, Style)>,
}

/// Wraps prose at whitespace, preserving span styles across breaks. A single
/// token wider than the width falls back to character breaking, and the
/// whitespace a break lands on is dropped rather than carried over.
fn wrap_line(line: Line, width: usize) -> Vec<Line> {
    if line.width() <= width {
        return vec![line];
    }
    let mut tokens: Vec<WrapToken> = Vec::new();
    for span in line.spans {
        for character in span.text.chars() {
            let whitespace = character.is_whitespace();
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            match tokens.last_mut() {
                Some(token) if token.whitespace == whitespace => {
                    token.width += character_width;
                    token.characters.push((character, span.style));
                }
                _ => tokens.push(WrapToken {
                    whitespace,
                    width: character_width,
                    characters: vec![(character, span.style)],
                }),
            }
        }
    }
    let mut output = vec![Line::default()];
    let mut used = 0_usize;
    for token in tokens {
        if used + token.width <= width {
            let line = output.last_mut().expect("output starts populated");
            for (character, style) in token.characters {
                line.push(character.to_string(), style);
            }
            used += token.width;
        } else if token.whitespace {
            if used > 0 {
                output.push(Line::default());
                used = 0;
            }
        } else if token.width <= width {
            output.push(Line::default());
            let line = output.last_mut().expect("output starts populated");
            for (character, style) in token.characters {
                line.push(character.to_string(), style);
            }
            used = token.width;
        } else {
            for (character, style) in token.characters {
                let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
                if used > 0 && used + character_width > width {
                    output.push(Line::default());
                    used = 0;
                }
                output
                    .last_mut()
                    .expect("output starts populated")
                    .push(character.to_string(), style);
                used += character_width;
            }
        }
    }
    while output.len() > 1 && output.last().is_some_and(Line::is_empty) {
        output.pop();
    }
    output
}

/// Character wrapping for literal content (code blocks, table rows) where
/// dropping or moving whitespace would break alignment.
fn wrap_line_chars(line: Line, width: usize) -> Vec<Line> {
    let mut output = vec![Line::default()];
    let mut current_width = 0;
    for span in line.spans {
        for character in span.text.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            if current_width > 0 && current_width + character_width > width {
                output.push(Line::default());
                current_width = 0;
            }
            output
                .last_mut()
                .expect("output starts populated")
                .push(character.to_string(), span.style);
            current_width += character_width;
        }
    }
    output
}

fn indent_lines(lines: Vec<Line>, prefix: &str, prefix_style: Style, width: usize) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut indented = Line::styled(prefix, prefix_style);
            for span in line.spans {
                indented.push(span.text, span.style);
            }
            truncate_line(indented, width)
        })
        .collect()
}

fn truncate_line(line: Line, width: usize) -> Line {
    if line.width() <= width {
        return line;
    }
    if width <= 3 {
        return Line::styled(".".repeat(width), muted());
    }
    let mut output = Line::default();
    let mut used = 0;
    let content_width = width - 3;
    for span in line.spans {
        let mut text = String::new();
        for character in span.text.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
            if used + character_width > content_width {
                break;
            }
            text.push(character);
            used += character_width;
        }
        output.push(text, span.style);
        if used >= content_width {
            break;
        }
    }
    output.push("...", muted());
    output
}

fn selection_viewport(lines: Vec<Line>, height: usize, selected_row: usize) -> Vec<Line> {
    let start = selected_row
        .saturating_sub(height / 2)
        .min(lines.len().saturating_sub(height));
    lines.into_iter().skip(start).take(height).collect()
}

fn transcript_viewport(mut lines: Vec<Line>, height: usize, offset: usize) -> Vec<Line> {
    let offset = offset.min(lines.len().saturating_sub(height));
    let end = lines.len().saturating_sub(offset);
    let start = end.saturating_sub(height);
    lines.drain(end..);
    lines.drain(..start);
    fit_height(lines, height)
}

fn fit_height(mut lines: Vec<Line>, height: usize) -> Vec<Line> {
    lines.resize(height, Line::default());
    lines.truncate(height);
    lines
}

fn tail_by_width(text: &str, width: usize) -> String {
    let mut output = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or_default();
        if used + character_width > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.into_iter().rev().collect()
}

fn preview(text: &str, width: usize) -> String {
    let plain = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.chars().count() <= width {
        plain
    } else {
        format!(
            "{}...",
            plain
                .chars()
                .take(width.saturating_sub(3))
                .collect::<String>()
        )
    }
}

fn bounded_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn write_line(output: &mut impl Write, line: &Line) -> io::Result<()> {
    for span in &line.spans {
        queue!(output, SetAttribute(Attribute::Reset), ResetColor)?;
        if let Some(color) = span.style.color {
            queue!(output, SetForegroundColor(color))?;
        }
        if let Some(background) = span.style.background {
            queue!(output, SetBackgroundColor(background))?;
        }
        if span.style.bold {
            queue!(output, SetAttribute(Attribute::Bold))?;
        }
        if span.style.dim {
            queue!(output, SetAttribute(Attribute::Dim))?;
        }
        if span.style.italic {
            queue!(output, SetAttribute(Attribute::Italic))?;
        }
        let safe = span
            .text
            .chars()
            .filter_map(terminal_safe_character)
            .collect::<String>();
        queue!(output, Print(safe))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
    use qq_protocol::{
        EventCursor, ModelSelection, RunId, SessionEvent, SessionEventEnvelope, SessionId,
        SessionSnapshot, SessionStatus, SessionSummary, StoreId, WorkspaceId, WorkspaceSnapshot,
        WorkspaceSummary,
    };

    use super::*;
    use crate::{ClientUpdate, ModelOption, TuiOptions};

    fn completed_message(byte: u8, output: String) -> MessageSnapshot {
        MessageSnapshot {
            id: MessageId::from_bytes([byte; 16]),
            session_id: SessionId::from_bytes([1; 16]),
            run_id: RunId::from_bytes([2; 16]),
            turn_ordinal: 0,
            role: MessageRole::Assistant,
            state: MessageState::Complete,
            output,
            refusal: String::new(),
            created_at_ms: 1,
        }
    }

    fn app_with_messages(count: u8) -> App {
        let workspace_id = WorkspaceId::from_bytes([3; 16]);
        let session_id = SessionId::from_bytes([1; 16]);
        let summary = SessionSummary {
            id: session_id,
            workspace_id,
            parent_id: None,
            title: "Session".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 1,
            last_outcome: None,
        };
        let mut app = App::new(TuiOptions::default());
        app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([4; 16]),
                workspace_id,
                sequence: 1,
            },
            workspace: WorkspaceSummary {
                id: workspace_id,
                path: "/workspace".to_owned(),
            },
            sessions: vec![summary.clone()],
            focused: Some(SessionSnapshot {
                summary,
                messages: (0..count)
                    .map(|row| completed_message(row + 1, format!("row {row}")))
                    .collect(),
                runs: Vec::new(),
                tool_calls: Vec::new(),
                has_older_tool_calls: false,
                has_older_messages: false,
            }),
            has_older_sessions: false,
        }));
        app
    }

    fn frame_text(frame: &[Line]) -> String {
        frame
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn frame_rows(frame: &[Line]) -> Vec<String> {
        frame
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn markdown_rows_remain_within_the_render_width() {
        let lines = markdown_lines("**Streaming** text remains narrow and readable.", 9);
        assert!(lines.iter().all(|line| line.width() <= 9));
    }

    #[test]
    fn tables_render_aligned_columns_with_a_header_separator() {
        let source =
            "| Order | Source |\n| --- | --- |\n| 1 | Built-in defaults |\n| 2 | Cached manifest |";
        let lines = markdown_lines(source, 60);
        let rows = frame_rows(&lines);

        assert_eq!(
            rows,
            [
                "Order │ Source".to_owned(),
                format!("{}─┼─{}", "─".repeat(5), "─".repeat(17)),
                "1     │ Built-in defaults".to_owned(),
                "2     │ Cached manifest".to_owned(),
            ]
        );
        assert!(lines[0].spans[0].style.bold, "header row renders bold");
    }

    #[test]
    fn wide_tables_wrap_cell_content_within_columns() {
        let source = "| Key | Description |\n| --- | --- |\n| alpha | a very long description that must wrap inside its own column |";
        let width = 32;
        let lines = markdown_lines(source, width);
        let rows = frame_rows(&lines);

        assert!(lines.iter().all(|line| line.width() <= width));
        // The oversized description wraps into multiple physical rows.
        assert!(rows.iter().filter(|row| row.contains('│')).count() > 2);
        // Every column separator sits at the same display position.
        let positions = rows
            .iter()
            .filter(|row| row.contains('│') || row.contains('┼'))
            .map(|row| {
                row.chars()
                    .take_while(|character| *character != '│' && *character != '┼')
                    .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
                    .sum::<usize>()
            })
            .collect::<Vec<_>>();
        assert!(!positions.is_empty());
        assert!(positions.iter().all(|position| *position == positions[0]));
    }

    #[test]
    fn very_narrow_tables_stack_rows_as_header_value_lines() {
        let source = "| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |";
        let rows = frame_rows(&markdown_lines(source, 10));

        assert_eq!(
            rows,
            ["A: 1", "B: 2", "C: 3", "---", "A: 4", "B: 5", "C: 6"]
        );
    }

    #[test]
    fn cjk_table_content_aligns_by_display_width() {
        let source = "| 名前 | 説明 |\n| --- | --- |\n| 短い | 長い説明テキスト |";
        let lines = markdown_lines(source, 40);
        let rows = frame_rows(&lines);

        assert!(lines.iter().all(|line| line.width() <= 40));
        let positions = rows
            .iter()
            .map(|row| {
                row.chars()
                    .take_while(|character| *character != '│' && *character != '┼')
                    .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
                    .sum::<usize>()
            })
            .collect::<Vec<_>>();
        assert_eq!(positions, [5, 5, 5]);
    }

    #[test]
    fn partial_streaming_table_input_never_panics_and_stays_bounded() {
        let fragments = [
            "| Order | Source",
            "| Order | Source |\n| ---",
            "| Order | Source |\n| --- | --- |\n| 1 | Built",
            "| a |\n| --- |\n| b |\n\ntext after",
            "| |\n| --- |\n| |",
        ];
        for fragment in fragments {
            for width in 0..48 {
                let lines = markdown_lines(fragment, width);
                assert!(lines.iter().all(|line| line.width() <= width.max(1)));
            }
        }
    }

    #[test]
    fn word_wrap_breaks_at_whitespace_and_preserves_styles() {
        let mut line = Line::default();
        line.push("manage ", normal().bold());
        line.push("daemons cleanly", normal());

        let wrapped = wrap_line(line, 10);

        assert_eq!(frame_rows(&wrapped), ["manage ", "daemons ", "cleanly"]);
        assert_eq!(wrapped[0].spans[0].style, normal().bold());
        assert_eq!(wrapped[1].spans[0].style, normal());
    }

    #[test]
    fn overlong_tokens_fall_back_to_character_breaks() {
        let mut line = Line::default();
        line.push("ab", normal().bold());
        line.push("cdefgh", normal());

        let wrapped = wrap_line(line, 4);

        assert_eq!(frame_rows(&wrapped), ["abcd", "efgh"]);
        assert_eq!(wrapped[0].spans.len(), 2);
        assert_eq!(wrapped[0].spans[0].style, normal().bold());
        assert_eq!(wrapped[0].spans[1].style, normal());
    }

    #[test]
    fn soft_breaks_reflow_paragraphs_to_the_render_width() {
        // A source-wrapped paragraph joins into one row when it fits...
        assert_eq!(
            frame_rows(&markdown_lines("alpha beta\ngamma delta", 40)),
            ["alpha beta gamma delta"]
        );
        // ...and rewraps at the terminal width, not the source width.
        assert_eq!(
            frame_rows(&markdown_lines("alpha beta\ngamma delta", 12)),
            ["alpha beta ", "gamma delta"]
        );
        // A hard break still forces an explicit line break.
        assert_eq!(
            frame_rows(&markdown_lines("alpha  \nbeta", 40)),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn code_blocks_keep_character_wrapping() {
        let rows = frame_rows(&markdown_lines("```\nlet answer_value = 42;\n```", 12));

        assert_eq!(
            rows,
            [
                "│           ",
                "│ let answer",
                "│ _value = 4",
                "│ 2;        ",
                "│           ",
            ]
        );
    }

    #[test]
    fn fenced_code_renders_as_a_tinted_panel_with_a_language_label() {
        let width = 24;
        let lines = markdown_lines("```rust\nlet x = 1;\n```", width);
        let rows = frame_rows(&lines);

        assert_eq!(
            rows,
            [
                format!("│ {}rust", " ".repeat(18)),
                format!("│ let x = 1;{}", " ".repeat(12)),
                format!("│{}", " ".repeat(23)),
            ]
        );
        // Every row is padded to the full width with the surface tint so the
        // panel reads as one solid slab.
        assert!(lines.iter().all(|line| line.width() == width));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.background == Some(SURFACE_COLOR))
        );
        assert_eq!(lines[0].spans[0].style, surface(accent().dim()));
        assert_eq!(lines[0].spans[1].style, surface(muted()));
    }

    #[test]
    fn diff_fenced_blocks_color_lines_inside_the_panel() {
        let source = "```diff\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context\n```";
        let lines = markdown_lines(source, 30);

        let style_of = |needle: &str| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.text.contains(needle))
                .map(|span| span.style)
        };
        assert_eq!(style_of("@@ -1,2 +1,2 @@"), Some(surface(accent().dim())));
        assert_eq!(style_of("-old line"), Some(surface(failure())));
        assert_eq!(style_of("+new line"), Some(surface(success())));
        assert_eq!(style_of(" context"), Some(surface(normal())));
        assert!(lines.iter().all(|line| line.width() == 30));
    }

    #[test]
    fn unterminated_fences_render_panels_safely_mid_stream() {
        let fragments = [
            "```",
            "```rust",
            "```rust\nfn main() {",
            "prose\n\n```diff\n+partial",
        ];
        for fragment in fragments {
            for width in 0..48 {
                let lines = markdown_lines(fragment, width);
                assert!(lines.iter().all(|line| line.width() <= width.max(1)));
            }
        }
        // A fence still streaming renders as a panel with the text so far.
        let rows = frame_rows(&markdown_lines("```rust\nfn main() {", 24));
        assert!(rows.iter().any(|row| row.starts_with("│ fn main() {")));
    }

    #[test]
    fn headings_get_a_blank_line_above_and_lists_stay_tight() {
        let rows = frame_rows(&markdown_lines("intro\n# Title\n- alpha\n- beta", 40));

        assert_eq!(rows, ["intro", "", "Title", "- alpha", "- beta"]);
    }

    #[test]
    fn markdown_entities_cannot_emit_terminal_controls() {
        let lines = markdown_lines("&#27;]52;c;Y2xpcGJvYXJk&#7;", 80);
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.text
                .chars()
                .all(|character| terminal_safe_character(character) == Some(character))
        }));
    }

    fn tool_call_snapshot(
        byte: u8,
        name: &str,
        arguments: &str,
        state: ToolCallState,
        result: Option<&str>,
        is_error: bool,
    ) -> ToolCallSnapshot {
        ToolCallSnapshot {
            id: qq_protocol::ToolCallId::from_bytes([byte; 16]),
            session_id: SessionId::from_bytes([1; 16]),
            run_id: RunId::from_bytes([2; 16]),
            turn_ordinal: 1,
            call_ordinal: u16::from(byte),
            provider_call_id: format!("call-{byte}"),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            state,
            result: result.map(str::to_owned),
            is_error,
            display: None,
        }
    }

    #[test]
    fn transcript_renders_replayed_tool_activity_collapsed() {
        let mut app = app_with_messages(1);
        let session_id = app.focused.unwrap();
        app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"note.txt"}"#,
            ToolCallState::Completed,
            Some("contents"),
            false,
        )]);

        let frame = FrameRenderer::default().frame(&mut app, 100, 30);
        let rows = frame_rows(&frame);

        assert!(
            rows.iter()
                .any(|row| row.contains("● read_file note.txt (1 line)"))
        );
        assert!(!frame_text(&frame).contains("contents"));
    }

    #[test]
    fn transcript_spacing_separates_blocks_and_doubles_before_prompts() {
        let mut app = app_with_messages(3);
        let session_id = app.focused.unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        let messages = session.messages.as_mut().unwrap();
        messages[0].role = MessageRole::User;
        messages[2].role = MessageRole::User;
        session.tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"note.txt"}"#,
            ToolCallState::Completed,
            Some("contents"),
            false,
        )]);

        let rows = frame_rows(&FrameRenderer::default().transcript(&app, 80));

        assert_eq!(
            rows,
            [
                " ▌ YOU",
                " ▌ row 0",
                "",
                "   QQ",
                "   row 1",
                "",
                "   ● read_file note.txt (1 line)",
                "",
                "",
                " ▌ YOU",
                " ▌ row 2",
            ]
        );
    }

    #[test]
    fn head_orphan_call_turns_render_before_the_runs_first_message() {
        let mut app = app_with_messages(2);
        let session_id = app.focused.unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        let messages = session.messages.as_mut().unwrap();
        messages[0].role = MessageRole::User;
        messages[1].turn_ordinal = 2;
        let call = |byte, turn, name: &str, arguments: &str, result: &str| {
            let mut call = tool_call_snapshot(
                byte,
                name,
                arguments,
                ToolCallState::Completed,
                Some(result),
                false,
            );
            call.turn_ordinal = turn;
            call
        };
        // Arrival order is scrambled; rendering re-sorts by (turn, call).
        session.tool_calls = Some(vec![
            call(5, 2, "search", r#"{"query":"x"}"#, "No matches found.\n"),
            call(4, 1, "read_file", r#"{"path":"b.rs"}"#, "b\n"),
            call(3, 1, "read_file", r#"{"path":"a.rs"}"#, "a\n"),
        ]);

        let rows = frame_rows(&FrameRenderer::default().transcript(&app, 80));

        // The call-only turn 1 renders before the run's first message (turn
        // 2), so the transcript reads in execution order.
        assert_eq!(
            rows,
            [
                " ▌ YOU",
                " ▌ row 0",
                "",
                "   ● read_file a.rs (1 line)",
                "   ● read_file b.rs (1 line)",
                "",
                "   QQ",
                "   row 1",
                "",
                "   ● search \"x\" (no matches)",
            ]
        );
    }

    #[test]
    fn consecutive_call_only_turns_merge_into_one_folded_group() {
        let mut app = app_with_messages(1);
        let session_id = app.focused.unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[0].turn_ordinal = 1;
        let calls = [(2, 1), (3, 1), (4, 2), (5, 3)]
            .into_iter()
            .map(|(byte, turn)| {
                let mut call = tool_call_snapshot(
                    byte,
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    ToolCallState::Completed,
                    Some("a\n"),
                    false,
                );
                call.turn_ordinal = turn;
                call
            })
            .collect::<Vec<_>>();
        session.tool_calls = Some(calls);

        let rows = frame_rows(&FrameRenderer::default().transcript(&app, 80));

        // The call-only turns 2 and 3 merge into turn 1's contiguous call
        // group, and the four quiet calls fold as one, not per turn.
        assert_eq!(
            rows,
            ["   QQ", "   row 0", "", "   ▸ 4 tool calls (read_file ×4)",]
        );
    }

    #[test]
    fn completed_edit_results_color_diff_shaped_content_at_expanded_detail() {
        let diff_call = tool_call_snapshot(
            1,
            "edit_file",
            r#"{"path":"src/lib.rs"}"#,
            ToolCallState::Completed,
            Some("@@ -1 +1 @@\n-old\n+new\n context"),
            false,
        );
        let lines = render_tool_calls(&[&diff_call], &HashMap::new(), ToolDetail::Expanded, 0, 80);
        let style_of = |lines: &[Line], needle: &str| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.text.contains(needle))
                .map(|span| span.style)
        };
        assert_eq!(style_of(&lines, "@@ -1 +1 @@"), Some(accent().dim()));
        assert_eq!(style_of(&lines, "-old"), Some(failure()));
        assert_eq!(style_of(&lines, "+new"), Some(success()));
        assert_eq!(style_of(&lines, " context"), Some(normal()));

        // Today's summary results are not diff-shaped and keep the raw style.
        let summary_call = tool_call_snapshot(
            2,
            "edit_file",
            r#"{"path":"src/lib.rs"}"#,
            ToolCallState::Completed,
            Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
            false,
        );
        let lines = render_tool_calls(
            &[&summary_call],
            &HashMap::new(),
            ToolDetail::Expanded,
            0,
            80,
        );
        assert_eq!(style_of(&lines, "Edited src/lib.rs"), Some(normal().dim()));
    }

    #[test]
    fn display_payload_diffs_replace_the_result_summary_at_expanded_detail() {
        let mut call = tool_call_snapshot(
            3,
            "edit_file",
            r#"{"path":"src/lib.rs"}"#,
            ToolCallState::Completed,
            Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
            false,
        );
        call.display = Some(ToolCallDisplay::Diff {
            path: "src/lib.rs".to_owned(),
            diff: "- old line\n+ new line\n".to_owned(),
        });

        let lines = render_tool_calls(&[&call], &HashMap::new(), ToolDetail::Expanded, 0, 80);
        let style_of = |needle: &str| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.text.contains(needle))
                .map(|span| span.style)
        };
        assert_eq!(style_of("- old line"), Some(failure()));
        assert_eq!(style_of("+ new line"), Some(success()));
        // The payload renders instead of the raw summary sentence.
        assert!(style_of("replaced 1 occurrence").is_none());

        // Collapsed detail keeps the one-liner; the payload adds no rows.
        let lines = render_tool_calls(&[&call], &HashMap::new(), ToolDetail::Collapsed, 0, 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn running_calls_show_a_live_output_tail_of_complete_lines() {
        let call = tool_call_snapshot(
            3,
            "shell",
            r#"{"command":"cargo build"}"#,
            ToolCallState::Running,
            None,
            false,
        );
        let mut live = HashMap::new();
        live.insert(
            call.id,
            "one\ntwo\nthree\nfour\nfive\nsix\nseven b\u{7}ell\npartial".to_owned(),
        );

        for detail in [ToolDetail::Collapsed, ToolDetail::Expanded] {
            let rows = frame_rows(&render_tool_calls(&[&call], &live, detail, 0, 80));
            assert!(rows[0].contains("shell"), "the spinner one-liner stays");
            let tail_start = rows.len() - MAX_LIVE_TAIL_ROWS;
            assert_eq!(
                &rows[tail_start..],
                [
                    "     two",
                    "     three",
                    "     four",
                    "     five",
                    "     six",
                    // Control characters are stripped; the mid-line chunk
                    // tail stays hidden until its newline arrives.
                    "     seven bell",
                ]
            );
            assert!(!rows.iter().any(|row| row.contains("partial")));
        }

        // Overlong lines wrap literally at the character level and the tail
        // stays bounded in rows.
        let mut live = HashMap::new();
        live.insert(call.id, format!("{}\n", "x".repeat(40)));
        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &live,
            ToolDetail::Collapsed,
            0,
            20,
        ));
        assert_eq!(rows[1], format!("     {}", "x".repeat(15)));
        assert_eq!(rows[2], format!("     {}", "x".repeat(15)));
        assert!(rows.len() <= 1 + MAX_LIVE_TAIL_ROWS);

        // Calls that are no longer running render no tail even if a stale
        // buffer lingers.
        let mut finished = call.clone();
        finished.state = ToolCallState::Completed;
        finished.result = Some("ok\n".to_owned());
        let rows = frame_rows(&render_tool_calls(
            &[&finished],
            &live,
            ToolDetail::Collapsed,
            0,
            80,
        ));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn diff_detection_requires_hunks_or_paired_change_lines() {
        assert!(looks_like_diff("@@ -1 +1 @@\n context"));
        assert!(looks_like_diff("-old\n+new"));
        assert!(!looks_like_diff(
            "Edited src/lib.rs: replaced 1 occurrence(s)."
        ));
        assert!(!looks_like_diff("+new line only"));
        assert!(!looks_like_diff(""));
    }

    #[test]
    fn approval_prompts_render_edit_previews_as_colored_diffs() {
        let mut app = app_with_messages(1);
        let session_id = app.focused.unwrap();
        let tool_call = tool_call_snapshot(
            9,
            "edit_file",
            r#"{"path":"src/lib.rs","content":"new"}"#,
            ToolCallState::AwaitingApproval,
            None,
            false,
        );
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([4; 16]),
                workspace_id: app.workspace_id.unwrap(),
                sequence: 2,
            },
            session_id,
            run_id: Some(tool_call.run_id),
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::ToolApprovalRequested {
                tool_call,
                shell: None,
                edit: Some(qq_protocol::EditPreview {
                    path: "src/lib.rs".to_owned(),
                    diff: "@@ -1 +1 @@\n-old\n+new".to_owned(),
                }),
            },
        }));

        let frame = FrameRenderer::default().frame(&mut app, 80, 24);
        let rows = frame_rows(&frame);

        assert!(rows.iter().any(|row| row.contains("file: src/lib.rs")));
        assert!(!frame_text(&frame).contains("arguments:"));
        let style_of = |needle: &str| {
            frame
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.text.contains(needle))
                .map(|span| span.style)
        };
        assert_eq!(style_of("@@ -1 +1 @@"), Some(accent().dim()));
        assert_eq!(style_of("-old"), Some(failure()));
        assert_eq!(style_of("+new"), Some(success()));
    }

    #[test]
    fn collapsed_summaries_curate_known_tools() {
        let cases = [
            (
                tool_call_snapshot(
                    1,
                    "read_file",
                    r#"{"path":"src/config/loader.rs"}"#,
                    ToolCallState::Completed,
                    Some("a\nb\nc\n"),
                    false,
                ),
                "   ● read_file src/config/loader.rs (3 lines)",
            ),
            (
                tool_call_snapshot(
                    2,
                    "read_file",
                    r#"{"path":"big.log"}"#,
                    ToolCallState::Completed,
                    Some("a\n...[truncated by qq]\n"),
                    false,
                ),
                "   ● read_file big.log (1 line, truncated)",
            ),
            (
                tool_call_snapshot(
                    3,
                    "search",
                    r#"{"query":"pattern"}"#,
                    ToolCallState::Completed,
                    Some("src/a.rs:1:x pattern\nsrc/a.rs:9:pattern y\nsrc/b.rs: filename match\n"),
                    false,
                ),
                "   ● search \"pattern\" (3 matches, 2 files)",
            ),
            (
                tool_call_snapshot(
                    4,
                    "search",
                    r#"{"query":"absent"}"#,
                    ToolCallState::Completed,
                    Some("No matches found.\n"),
                    false,
                ),
                "   ● search \"absent\" (no matches)",
            ),
            (
                tool_call_snapshot(
                    5,
                    "list_dir",
                    r#"{"path":"crates/qq-core/src"}"#,
                    ToolCallState::Completed,
                    Some("lib.rs\nsessions.rs\ntools.rs\n"),
                    false,
                ),
                "   ● list_dir crates/qq-core/src (3 entries)",
            ),
        ];
        for (call, expected) in cases {
            let rows = frame_rows(&render_tool_calls(
                &[&call],
                &HashMap::new(),
                ToolDetail::Collapsed,
                0,
                120,
            ));
            assert_eq!(rows, [expected]);
        }
    }

    #[test]
    fn unknown_tools_fall_back_to_compact_arguments_and_byte_size() {
        let result = "x".repeat(2048);
        let call = tool_call_snapshot(
            1,
            "edit_file",
            r#"{"path":"src/main.rs","content":"fn main() {}"}"#,
            ToolCallState::Completed,
            Some(&result),
            false,
        );

        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            160,
        ));

        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("● edit_file"));
        assert!(rows[0].contains(r#"{"path":"src/main.rs","#));
        assert!(rows[0].contains("(2.0 KB)"));
    }

    #[test]
    fn malformed_arguments_fall_back_to_a_raw_preview() {
        let call = tool_call_snapshot(
            1,
            "read_file",
            "{not json",
            ToolCallState::Completed,
            Some("a\n"),
            false,
        );

        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
        ));

        assert!(rows[0].contains("read_file {not json (1 line)"));
    }

    #[test]
    fn error_results_expand_under_the_summary_by_default() {
        let call = tool_call_snapshot(
            1,
            "read_file",
            r#"{"path":"gone.txt"}"#,
            ToolCallState::Completed,
            Some("path is not a file"),
            true,
        );

        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
        ));

        assert_eq!(rows[0], "   ✗ read_file gone.txt");
        assert_eq!(rows[1], "     path is not a file");
    }

    #[test]
    fn pending_states_show_their_glyph_and_label() {
        let awaiting = tool_call_snapshot(
            1,
            "shell",
            r#"{"command":"cargo test"}"#,
            ToolCallState::AwaitingApproval,
            None,
            false,
        );
        let rows = frame_rows(&render_tool_calls(
            &[&awaiting],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
        ));
        assert_eq!(
            rows,
            ["   ◇ shell {\"command\":\"cargo test\"} awaiting approval"]
        );

        let running = tool_call_snapshot(
            2,
            "search",
            r#"{"query":"x"}"#,
            ToolCallState::Running,
            None,
            false,
        );
        let rows = frame_rows(&render_tool_calls(
            &[&running],
            &HashMap::new(),
            ToolDetail::Collapsed,
            1,
            120,
        ));
        assert_eq!(rows, ["   ◓ search \"x\" running"]);
    }

    #[test]
    fn quiet_runs_fold_into_a_single_counted_line() {
        let mut calls = Vec::new();
        for byte in 1..=4 {
            calls.push(tool_call_snapshot(
                byte,
                "read_file",
                r#"{"path":"a.rs"}"#,
                ToolCallState::Completed,
                Some("a\n"),
                false,
            ));
        }
        for byte in 5..=6 {
            calls.push(tool_call_snapshot(
                byte,
                "search",
                r#"{"query":"x"}"#,
                ToolCallState::Completed,
                Some("No matches found.\n"),
                false,
            ));
        }
        let references = calls.iter().collect::<Vec<_>>();

        let rows = frame_rows(&render_tool_calls(
            &references,
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
        ));
        assert_eq!(rows, ["   ▸ 6 tool calls (read_file ×4, search ×2)"]);

        // An active or failed call keeps every line visible.
        calls[5].state = ToolCallState::Running;
        let references = calls.iter().collect::<Vec<_>>();
        let rows = frame_rows(&render_tool_calls(
            &references,
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
        ));
        assert_eq!(rows.len(), 6);

        // Expanded detail never folds.
        calls[5].state = ToolCallState::Completed;
        let references = calls.iter().collect::<Vec<_>>();
        let rows = frame_rows(&render_tool_calls(
            &references,
            &HashMap::new(),
            ToolDetail::Expanded,
            0,
            120,
        ));
        assert!(rows.len() > 6);
    }

    #[test]
    fn detail_cycling_reveals_arguments_and_result_tails() {
        let mut app = app_with_messages(1);
        let session_id = app.focused.unwrap();
        app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"note.txt"}"#,
            ToolCallState::Completed,
            Some("alpha\nbeta"),
            false,
        )]);
        let mut renderer = FrameRenderer::default();
        let ctrl_o = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));

        let collapsed = frame_rows(&renderer.frame(&mut app, 100, 30));
        assert!(!collapsed.iter().any(|row| row.contains("beta")));
        assert!(collapsed.iter().any(|row| row.contains("tools: collapsed")));

        app.handle_terminal_event(ctrl_o.clone());
        let expanded = frame_rows(&renderer.frame(&mut app, 100, 30));
        assert!(
            expanded
                .iter()
                .any(|row| row.contains("\"path\": \"note.txt\""))
        );
        assert!(expanded.iter().any(|row| row.contains("beta")));
        assert!(expanded.iter().any(|row| row.contains("tools: expanded")));

        app.handle_terminal_event(ctrl_o);
        let collapsed = frame_rows(&renderer.frame(&mut app, 100, 30));
        assert!(!collapsed.iter().any(|row| row.contains("beta")));
    }

    #[test]
    fn tool_rows_respect_narrow_widths() {
        let calls = [
            tool_call_snapshot(
                1,
                "read_file",
                r#"{"path":"a/very/long/path/that/never/ends.rs"}"#,
                ToolCallState::Completed,
                Some("line one that is fairly long\nline two\n"),
                false,
            ),
            tool_call_snapshot(
                2,
                "shell",
                r#"{"command":"cargo test --workspace --all-features"}"#,
                ToolCallState::Failed,
                Some("error: a very long failure message that overflows"),
                true,
            ),
        ];
        let references = calls.iter().collect::<Vec<_>>();
        for width in 0..24 {
            for detail in [ToolDetail::Collapsed, ToolDetail::Expanded] {
                let lines = render_tool_calls(&references, &HashMap::new(), detail, 0, width);
                assert!(lines.iter().all(|line| line.width() <= width));
            }
        }
    }

    #[test]
    fn user_prompts_carry_an_accent_bar() {
        let mut renderer = FrameRenderer::default();
        let mut message = completed_message(1, "deploy the API".to_owned());
        message.role = MessageRole::User;

        let rows = frame_rows(&renderer.render_message(&message, 80));

        assert!(rows[0].starts_with(" ▌ YOU"));
        assert!(rows[1].starts_with(" ▌ "));
        assert_eq!(
            renderer.render_message(&message, 80)[0].spans[0].style,
            accent()
        );
    }

    #[test]
    fn final_output_sanitizes_every_dynamic_span() {
        let line = Line::styled("title\u{1b}]52;c;Y2xpcGJvYXJk\u{7}\u{202e}", normal());
        let mut rendered = Vec::new();

        write_line(&mut rendered, &line).unwrap();

        let rendered = String::from_utf8(rendered).unwrap();
        assert!(!rendered.contains("\u{1b}]52"));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn panel_rows_emit_the_surface_background_to_the_terminal() {
        let row = code_panel_row(Line::styled("x", normal()), 8);
        let mut rendered = Vec::new();

        write_line(&mut rendered, &row).unwrap();

        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("48;2;38;40;48"));
    }

    #[test]
    fn truncated_rows_never_exceed_the_terminal_width() {
        for width in 0..10 {
            let line = truncate_line(Line::styled("a long row", normal()), width);
            assert!(line.width() <= width);
        }
    }

    #[test]
    fn completed_markdown_cache_is_bounded_and_keeps_one_width() {
        let mut renderer = FrameRenderer::default();
        let message = completed_message(1, "hello".to_owned());
        renderer.render_message(&message, 40);
        renderer.render_message(&message, 80);
        assert_eq!(renderer.markdown.len(), 1);
        assert_eq!(renderer.markdown[&message.id].width, 77);

        for byte in 2..=u8::try_from(MAX_VISIBLE_MESSAGES + 8).unwrap() {
            renderer.render_message(&completed_message(byte, byte.to_string()), 80);
        }
        assert!(renderer.markdown.len() <= MAX_VISIBLE_MESSAGES);
    }

    #[test]
    fn completed_markdown_uses_a_bounded_tail() {
        let mut renderer = FrameRenderer::default();
        let output = format!("START-MARKER{}END-MARKER", "x".repeat(MAX_MARKDOWN_BYTES));
        let message = completed_message(1, output);
        renderer.render_message(&message, 80);

        let cached = &renderer.markdown[&message.id].lines;
        let text = cached
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(!text.contains("START-MARKER"));
        assert!(text.contains("END-MARKER"));
        assert!(cached.len() <= MAX_CACHED_MARKDOWN_ROWS);
    }

    #[test]
    fn combined_output_and_refusal_respect_the_markdown_limit() {
        let mut message = completed_message(1, "o".repeat(MAX_MARKDOWN_BYTES));
        message.refusal = format!("{}END", "r".repeat(MAX_MARKDOWN_BYTES));

        let content = message_content(&message);

        assert!(content.len() <= MAX_MARKDOWN_BYTES);
        assert!(content.ends_with("END"));
    }

    #[test]
    fn refreshed_chrome_shows_identity_status_and_session_metrics() {
        let mut app = app_with_messages(1);
        app.connection = crate::ConnectionState::Live;
        app.models.push(ModelOption {
            provider: "openai".to_owned(),
            model: "gpt-test".to_owned(),
            name: Some("GPT Test".to_owned()),
            context_window: Some(128_000),
            selection: ModelSelection {
                model: Some("openai/gpt-test".to_owned()),
                max_output_tokens: Some(4_096),
                organization: None,
            },
        });
        let session = app.sessions.get_mut(&app.focused.unwrap()).unwrap();
        session.latest_input_tokens = Some(64_000);
        session.context_window = Some(128_000);
        let frame = FrameRenderer::default().frame(&mut app, 80, 12);
        let rows = frame_rows(&frame);

        assert!(rows[0].contains(&format!("qq  {VERSION} {GIT_COMMIT}")));
        assert!(rows[0].ends_with("local"));
        assert!(!rows[0].contains("Threadline"));
        assert!(rows[9].contains("> Ask QQ..."));
        assert!(rows[10].contains("context: 50.0% / 128000"));
        assert!(rows[10].ends_with("model: openai/gpt-test "));
        assert!(rows[11].contains("cwd: /workspace"));
        assert!(rows[11].ends_with("cost: $0.00 "));
        assert_eq!(frame[0].spans[0].style, brand().bold());
    }

    #[test]
    fn footer_placeholders_read_as_zero_rather_than_unavailable() {
        let mut app = app_with_messages(0);
        let session = app.sessions.get_mut(&app.focused.unwrap()).unwrap();
        session.summary.estimated_cost_usd_nanos = None;
        session.latest_input_tokens = None;

        let rows = frame_rows(&[footer_context(&app, 80), footer_workspace(&app, 80)]);

        assert!(rows[0].contains("context: 0.0%"));
        assert!(rows[1].ends_with("cost: $0.00 "));
        assert!(!rows.iter().any(|row| row.contains("unavailable")));
    }

    #[test]
    fn header_only_qualifies_local_when_the_connection_has_a_problem() {
        let mut app = app_with_messages(0);
        for (connection, expected) in [
            (crate::ConnectionState::Connecting, "local  connecting"),
            (crate::ConnectionState::Replaying, "local  reconnecting"),
            (crate::ConnectionState::Offline, "local  offline"),
        ] {
            app.connection = connection;
            assert!(frame_rows(&[header(&app, 80)])[0].ends_with(expected));
        }
    }

    #[test]
    fn threadline_has_no_vertical_message_rails() {
        let mut app = app_with_messages(2);
        let frame = FrameRenderer::default().frame(&mut app, 80, 14);

        assert!(frame_rows(&frame).iter().all(|row| !row.contains("  |  ")));
    }

    #[test]
    fn slash_autocomplete_is_filtered_above_the_composer() {
        let mut app = app_with_messages(1);
        app.input = "/".to_owned();
        let frame = FrameRenderer::default().frame(&mut app, 80, 16);
        let text = frame_text(&frame);
        for command in ["/models", "/sessions", "/resume", "/new", "/quit", "/exit"] {
            assert!(text.contains(command));
        }

        app.input = "/qu".to_owned();
        let frame = FrameRenderer::default().frame(&mut app, 80, 14);
        let text = frame_text(&frame);

        assert!(text.contains("/quit"));
        assert!(!text.contains("/models"));
        assert!(!text.contains("/sessions"));
    }

    #[test]
    fn session_picker_pins_search_and_keeps_the_selection_visible() {
        let mut app = app_with_messages(0);
        let workspace_id = app.workspace_id.unwrap();
        let store_id = StoreId::from_bytes([4; 16]);
        let mut selected = None;
        for byte in 2..20 {
            let session_id = SessionId::from_bytes([byte; 16]);
            if byte == 10 {
                selected = Some(session_id);
            }
            let summary = SessionSummary {
                id: session_id,
                workspace_id,
                parent_id: None,
                title: format!("Session {byte}"),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("openai/gpt-test".to_owned()),
                estimated_cost_usd_nanos: Some(0),
                updated_at_ms: u64::from(byte),
                last_outcome: None,
            };
            app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id,
                    workspace_id,
                    sequence: u64::from(byte),
                },
                session_id,
                run_id: None,
                caused_by: None,
                occurred_at_ms: u64::from(byte),
                event: SessionEvent::SessionCreated { session: summary },
            }));
        }
        app.session_picker = Some(crate::app::SessionPicker {
            query: String::new(),
            selected,
        });

        let frame = FrameRenderer::default().frame(&mut app, 80, 12);
        let text = frame_text(&frame);

        assert!(text.contains("SESSIONS"));
        assert!(text.contains("search: all sessions"));
        assert!(text.contains("Session 10"));
    }

    #[test]
    fn session_picker_renders_an_empty_search_result() {
        let mut app = app_with_messages(0);
        app.session_picker = Some(crate::app::SessionPicker {
            query: "missing".to_owned(),
            selected: None,
        });

        let frame = FrameRenderer::default().frame(&mut app, 80, 12);
        let text = frame_text(&frame);

        assert!(text.contains("search: missing"));
        assert!(text.contains("No matching sessions."));
    }

    #[test]
    fn transcript_viewport_renders_rows_above_the_tail_and_clamps_at_the_top() {
        let lines = (0..8)
            .map(|row| Line::styled(row.to_string(), normal()))
            .collect::<Vec<_>>();

        let scrolled = transcript_viewport(lines.clone(), 3, 2);
        let top = transcript_viewport(lines, 3, usize::MAX);
        let text = |rows: &[Line]| {
            rows.iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(text(&scrolled), ["3", "4", "5"]);
        assert_eq!(text(&top), ["0", "1", "2"]);
    }

    #[test]
    fn page_up_replaces_the_rendered_live_tail_with_older_transcript_rows() {
        let mut app = app_with_messages(10);
        let mut renderer = FrameRenderer::default();
        let tail = renderer.frame(&mut app, 80, 12);

        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        let scrolled = renderer.frame(&mut app, 80, 12);

        assert!(frame_text(&tail).contains("row 9"));
        assert!(!frame_text(&scrolled).contains("row 9"));
        assert!(frame_text(&scrolled).contains("row 6"));
    }
}
