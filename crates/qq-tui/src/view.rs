use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    io::Write,
};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use qq_protocol::{
    MessageId, MessageRole, MessageSnapshot, MessageState, SessionId, SessionStatus,
};
use unicode_width::UnicodeWidthChar;

use crate::{Layout, app::App, app::terminal_safe_character};

const MAX_RENDER_WIDTH: u16 = 320;
const MAX_RENDER_HEIGHT: u16 = 160;
const MAX_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
const MAX_CACHED_MARKDOWN_ROWS: usize = MAX_RENDER_HEIGHT as usize;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: &str = env!("QQ_GIT_COMMIT");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    color: Option<Color>,
    bold: bool,
    dim: bool,
    italic: bool,
}

impl Style {
    const fn color(color: Color) -> Self {
        Self {
            color: Some(color),
            bold: false,
            dim: false,
            italic: false,
        }
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
        let body = if app.model_picker.is_some() {
            model_picker(app, width, body_height)
        } else if app.session_picker.is_some() {
            session_picker(app, width, body_height)
        } else {
            match app.layout {
                Layout::Threadline => self.threadline(app, width),
                Layout::FoldFocus => self.fold_focus(app, width),
            }
        };
        let mut body = if app.model_picker.is_some() || app.session_picker.is_some() {
            body
        } else {
            app.update_transcript_viewport(body.len(), body_height);
            transcript_viewport(body, body_height, app.transcript_scroll_offset())
        };
        if app.model_picker.is_none() && app.session_picker.is_none() {
            overlay_slash_autocomplete(&mut body, slash_autocomplete(app, width, body_height));
        }
        lines.extend(body);
        lines.push(composer(app, width));
        lines.push(footer_context(app, width));
        lines.push(footer_workspace(app, width));
        fit_height(lines, height)
    }

    fn prune_markdown(&mut self, app: &App) {
        let visible = if app.session_picker.is_none() && app.model_picker.is_none() {
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
        let Some(messages) = app
            .sessions
            .get(&session_id)
            .and_then(|session| session.messages.as_ref())
        else {
            return vec![Line::styled(
                "  Loading session history...",
                muted().italic(),
            )];
        };
        let mut lines = Vec::new();
        let hidden = messages.len().saturating_sub(MAX_VISIBLE_MESSAGES);
        if hidden > 0 {
            lines.push(Line::styled(
                format!("  {hidden} earlier messages outside the viewport"),
                muted(),
            ));
        }
        for message in messages.iter().skip(hidden) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(self.render_message(message, width));
        }
        for prompt in app.pending_prompts(session_id) {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let mut line = Line::styled("  ", muted());
            line.push("YOU  pending", warning().bold());
            lines.push(line);
            lines.extend(indent_lines(
                bounded_markdown_lines(prompt, width.saturating_sub(3)),
                "   ",
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
        let prefix = "   ";
        let role = match message.role {
            MessageRole::User => "YOU",
            MessageRole::Assistant => "QQ",
        };
        let mut header = Line::styled(prefix, muted());
        header.push(
            role,
            if message.role == MessageRole::User {
                accent().bold()
            } else {
                normal().bold()
            },
        );
        if !matches!(message.state, MessageState::Complete) {
            header.push(
                format!("  {}", message_state_label(message.state)),
                status_style(message.state),
            );
        }
        let mut lines = vec![truncate_line(header, width)];
        let content_width = width.saturating_sub(prefix.len()).max(1);
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
            lines.push(Line::styled(format!("{prefix}..."), muted()));
        } else {
            lines.extend(indent_lines(body, prefix, width));
        }
        lines
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
            format!(" context: {}.{}%", tenths / 10, tenths % 10)
        }
        Some(_) | None => " context: unavailable".to_owned(),
    };
    let focused = app
        .focused
        .and_then(|id| app.sessions.get(&id))
        .map(|session| &session.summary);
    let selected_model = focused
        .and_then(|session| session.model.as_deref())
        .or(app.model.model.as_deref())
        .unwrap_or("default");
    align_sides(
        Line::styled(context, muted()),
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
    let cost = app
        .focused
        .and_then(|id| app.sessions.get(&id))
        .and_then(|session| session.summary.estimated_cost_usd_nanos)
        .map_or_else(
            || "cost: unavailable ".to_owned(),
            |cost| format!("cost: {} ", format_cost(cost)),
        );
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
    let mut styles = vec![normal()];
    let mut list_depth = 0_usize;
    let parser = Parser::new_ext(source, Options::all());
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    ensure_line(&mut lines);
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
                Tag::CodeBlock(_) => {
                    ensure_line(&mut lines);
                    styles.push(warning());
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
                | Tag::Table(_)
                | Tag::TableHead
                | Tag::TableRow
                | Tag::TableCell
                | Tag::MetadataBlock(_) => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::CodeBlock
                | TagEnd::BlockQuote(_) => {
                    ensure_line(&mut lines);
                    if matches!(tag, TagEnd::Heading(_) | TagEnd::CodeBlock) {
                        styles.pop();
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    styles.pop();
                }
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                TagEnd::Item => ensure_line(&mut lines),
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
                | TagEnd::Table
                | TagEnd::TableHead
                | TagEnd::TableRow
                | TagEnd::TableCell
                | TagEnd::MetadataBlock(_) => {}
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                append_safe_text(
                    &mut lines,
                    &text,
                    *styles.last().expect("base style remains"),
                );
            }
            Event::Code(code) => {
                lines
                    .last_mut()
                    .expect("line exists")
                    .push(code.to_string(), warning().bold());
            }
            Event::SoftBreak | Event::HardBreak => lines.push(Line::default()),
            Event::Rule => {
                ensure_line(&mut lines);
                lines.push(Line::styled("------------", muted()));
                lines.push(Line::default());
            }
            Event::TaskListMarker(checked) => lines
                .last_mut()
                .expect("line exists")
                .push(if checked { "[x] " } else { "[ ] " }, accent()),
            Event::FootnoteReference(reference) => lines
                .last_mut()
                .expect("line exists")
                .push(format!("[{reference}]"), accent()),
            Event::InlineMath(math) | Event::DisplayMath(math) => lines
                .last_mut()
                .expect("line exists")
                .push(format!("${math}$"), warning()),
        }
    }
    while lines.last().is_some_and(Line::is_empty) {
        lines.pop();
    }
    lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width.max(1)))
        .collect()
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

fn wrap_line(line: Line, width: usize) -> Vec<Line> {
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

fn indent_lines(lines: Vec<Line>, prefix: &str, width: usize) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut indented = Line::styled(prefix, muted());
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
    fn markdown_entities_cannot_emit_terminal_controls() {
        let lines = markdown_lines("&#27;]52;c;Y2xpcGJvYXJk&#7;", 80);
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.text
                .chars()
                .all(|character| terminal_safe_character(character) == Some(character))
        }));
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
        assert!(rows[10].contains("context: 50.0%"));
        assert!(rows[10].ends_with("model: openai/gpt-test "));
        assert!(rows[11].contains("cwd: /workspace"));
        assert!(rows[11].ends_with("cost: $0.00 "));
        assert_eq!(frame[0].spans[0].style, brand().bold());
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
