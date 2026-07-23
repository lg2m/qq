//! Disposable interaction prototypes for QQ's eventual chat TUI.

#![forbid(unsafe_code)]

use std::{
    io::{self, Write, stdout},
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use futures_util::StreamExt;
use pulldown_cmark::{Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
use tokio::{
    io::AsyncWriteExt,
    time::{MissedTickBehavior, interval},
};
use unicode_width::UnicodeWidthChar;

const MAX_INPUT_BYTES: usize = 512;
const MAX_RENDER_WIDTH: u16 = 320;
const MAX_RENDER_HEIGHT: u16 = 160;
const STREAM_CHARS_PER_TICK: usize = 4;

fn terminal_safe_character(character: char) -> Option<char> {
    if character.is_control() {
        return character.is_whitespace().then_some(' ');
    }
    if matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    ) {
        return None;
    }
    Some(character)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Concept {
    Threadline,
    Marginalia,
    FoldFocus,
}

impl Concept {
    fn name(self) -> &'static str {
        match self {
            Self::Threadline => "Threadline",
            Self::Marginalia => "Marginalia",
            Self::FoldFocus => "Fold / Focus",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThinkingMode {
    Hidden,
    Pulse,
    Expanded,
}

impl ThinkingMode {
    fn next(self) -> Self {
        match self {
            Self::Hidden => Self::Pulse,
            Self::Pulse => Self::Expanded,
            Self::Expanded => Self::Hidden,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Pulse => "pulse",
            Self::Expanded => "expanded",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    User,
    Assistant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionStatus {
    Queued,
    Running,
    Done,
}

impl SessionStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
        }
    }

    fn marker(self, pulse: usize) -> &'static str {
        match self {
            Self::Queued => "○",
            Self::Running => ["◒", "◐", "◓", "◑"][pulse % 4],
            Self::Done => "●",
        }
    }
}

#[derive(Clone, Debug)]
struct Message {
    role: Role,
    content: String,
}

impl Message {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Session {
    parent: Option<usize>,
    title: &'static str,
    summary: &'static str,
    status: SessionStatus,
    thinking: &'static str,
    messages: Vec<Message>,
}

#[derive(Debug)]
struct StreamState {
    session: usize,
    message: usize,
    target: Vec<char>,
    cursor: usize,
    drives_demo: bool,
}

#[derive(Debug)]
struct App {
    concept: Concept,
    thinking: ThinkingMode,
    sessions: Vec<Session>,
    focused: usize,
    navigator: Option<usize>,
    input: String,
    stream: Option<StreamState>,
    animation_tick: usize,
    quit: bool,
}

impl App {
    fn new() -> Self {
        let target = concat!(
            "Three directions can share one fast rendering core while making ",
            "different tradeoffs.\n\n",
            "## What stays constant\n\n",
            "- **Streaming Markdown** remains readable while a response is incomplete.\n",
            "- Child work appears as status and receipts, never interleaved prose.\n",
            "- The renderer only writes terminal rows whose styled content changed.\n\n",
            "```rust\n",
            "render(changed_rows_only);\n",
            "```\n\n",
            "Use **F1-F3** to compare the information hierarchy, then open the ",
            "thread navigator with `Ctrl-T`."
        );

        let sessions = vec![
            Session {
                parent: None,
                title: "TUI direction",
                summary: "Compare three lightweight chat interaction models",
                status: SessionStatus::Running,
                thinking: "Separating persistent conversation, transient activity, and child-session navigation before choosing where each belongs on screen.",
                messages: vec![
                    Message::new(
                        Role::User,
                        "Can QQ feel calm even while several agents are active?",
                    ),
                    Message::new(
                        Role::Assistant,
                        "Yes. The default surface should show the conversation and only the **minimum live signal** needed to trust background work.",
                    ),
                    Message::new(
                        Role::User,
                        "Prototype three distinct approaches without turning this into a dashboard.",
                    ),
                    Message::new(Role::Assistant, ""),
                ],
            },
            Session {
                parent: Some(0),
                title: "renderer audit",
                summary: "Map low-latency rendering patterns",
                status: SessionStatus::Running,
                thinking: "Comparing normal-buffer redraw strategies and identifying the smallest safe invalidation unit.",
                messages: vec![
                    Message::new(
                        Role::User,
                        "Inspect fast terminal rendering patterns and return a recommendation.",
                    ),
                    Message::new(
                        Role::Assistant,
                        "Use a retained frame and compare **styled rows**. Queue only changed rows inside a synchronized update; keep input asynchronous so rendering never waits on a blocking read.",
                    ),
                ],
            },
            Session {
                parent: Some(0),
                title: "markdown edges",
                summary: "Exercise partial Markdown and narrow widths",
                status: SessionStatus::Queued,
                thinking: "Checking incomplete delimiters, fenced code, wide glyphs, and resize behavior.",
                messages: vec![
                    Message::new(
                        Role::User,
                        "Check how partial Markdown behaves during a token stream.",
                    ),
                    Message::new(
                        Role::Assistant,
                        "Reparse the mutable tail. CommonMark safely treats unfinished syntax as text, then promotes it when the closing delimiter arrives. Keep committed history immutable later if profiling warrants it.",
                    ),
                ],
            },
            Session {
                parent: Some(1),
                title: "diff benchmark",
                summary: "Estimate writes avoided by row diffing",
                status: SessionStatus::Queued,
                thinking: "Sampling frame mutations rather than optimizing escape sequence count in isolation.",
                messages: vec![
                    Message::new(
                        Role::User,
                        "Estimate the useful unit for a renderer benchmark.",
                    ),
                    Message::new(
                        Role::Assistant,
                        "Measure end-to-end frame latency and bytes written for a streaming turn. A row-level diff is simple and should avoid nearly all static transcript writes.",
                    ),
                ],
            },
        ];

        Self {
            concept: Concept::Threadline,
            thinking: ThinkingMode::Pulse,
            sessions,
            focused: 0,
            navigator: None,
            input: String::new(),
            stream: Some(StreamState {
                session: 0,
                message: 3,
                target: target.chars().collect(),
                cursor: 0,
                drives_demo: true,
            }),
            animation_tick: 0,
            quit: false,
        }
    }

    fn advance(&mut self) -> bool {
        self.animation_tick = self.animation_tick.wrapping_add(1);
        let mut changed = false;
        let mut demo_progress = None;
        let mut complete = false;

        if let Some(stream) = self.stream.as_mut() {
            let end = (stream.cursor + STREAM_CHARS_PER_TICK).min(stream.target.len());
            if end > stream.cursor {
                let content = &mut self.sessions[stream.session].messages[stream.message].content;
                content.extend(stream.target[stream.cursor..end].iter());
                stream.cursor = end;
                changed = true;
            }
            if stream.drives_demo {
                demo_progress = Some(stream.cursor * 100 / stream.target.len().max(1));
            }
            complete = stream.cursor == stream.target.len();
        }

        if let Some(progress) = demo_progress {
            let statuses = if progress < 30 {
                [
                    SessionStatus::Running,
                    SessionStatus::Queued,
                    SessionStatus::Queued,
                ]
            } else if progress < 65 {
                [
                    SessionStatus::Done,
                    SessionStatus::Running,
                    SessionStatus::Running,
                ]
            } else if progress < 90 {
                [
                    SessionStatus::Done,
                    SessionStatus::Running,
                    SessionStatus::Done,
                ]
            } else {
                [
                    SessionStatus::Done,
                    SessionStatus::Done,
                    SessionStatus::Done,
                ]
            };
            for (session, status) in self.sessions[1..].iter_mut().zip(statuses) {
                if session.status != status {
                    session.status = status;
                    changed = true;
                }
            }
        }

        if complete {
            if let Some(stream) = self.stream.take() {
                self.sessions[stream.session].status = SessionStatus::Done;
            }
            changed = true;
        }

        changed || (self.has_visible_activity() && self.animation_tick.is_multiple_of(4))
    }

    fn has_visible_activity(&self) -> bool {
        self.stream.is_some()
            || self
                .sessions
                .iter()
                .any(|session| session.status == SessionStatus::Running)
    }

    fn is_streaming_message(&self, session: usize, message: usize) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.session == session && stream.message == message)
    }

    fn pulse(&self) -> usize {
        self.animation_tick / 4
    }

    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) => {
                if self.navigator.is_some() {
                    return false;
                }
                let mut changed = false;
                let remaining = MAX_INPUT_BYTES.saturating_sub(self.input.len());
                for character in text.chars().take(remaining) {
                    changed |= self.push_input_character(character);
                }
                changed
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => true,
            Event::Key(_) | Event::Mouse(_) => false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return true;
        }

        match key.code {
            KeyCode::F(1) => self.concept = Concept::Threadline,
            KeyCode::F(2) => self.concept = Concept::Marginalia,
            KeyCode::F(3) => self.concept = Concept::FoldFocus,
            KeyCode::Tab if self.navigator.is_none() => self.thinking = self.thinking.next(),
            KeyCode::Char('t')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.navigator.is_none() =>
            {
                self.navigator = Some(self.focused);
            }
            _ if self.navigator.is_some() => return self.handle_navigator_key(key.code),
            KeyCode::Esc => {
                if let Some(parent) = self.sessions[self.focused].parent {
                    self.focused = parent;
                }
            }
            KeyCode::Enter => self.send_input(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return self.push_input_character(character);
            }
            _ => return false,
        }
        true
    }

    fn push_input_character(&mut self, character: char) -> bool {
        let Some(character) = terminal_safe_character(character) else {
            return false;
        };
        if self.input.len() + character.len_utf8() > MAX_INPUT_BYTES {
            return false;
        }
        self.input.push(character);
        true
    }

    fn handle_navigator_key(&mut self, code: KeyCode) -> bool {
        let selected = self.navigator.unwrap_or(self.focused);
        let order = self.thread_order();
        let position = order
            .iter()
            .position(|session| *session == selected)
            .expect("the selected session belongs to the thread tree");
        match code {
            KeyCode::Esc | KeyCode::Char('t') => self.navigator = None,
            KeyCode::Up => self.navigator = Some(order[position.saturating_sub(1)]),
            KeyCode::Down => {
                self.navigator = Some(order[(position + 1).min(order.len() - 1)]);
            }
            KeyCode::Enter => {
                self.focused = selected;
                self.navigator = None;
            }
            _ => return false,
        }
        true
    }

    fn send_input(&mut self) {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return;
        }

        if let Some(previous) = self.stream.take() {
            self.sessions[previous.session].messages[previous.message]
                .content
                .push_str("\n\n_Interrupted by the next prompt._");
            self.sessions[previous.session].status = SessionStatus::Done;
            if previous.drives_demo {
                for session in &mut self.sessions {
                    if session.status == SessionStatus::Running {
                        session.status = SessionStatus::Done;
                    }
                }
            }
        }

        let response = format!(
            "I would test **{prompt}** against the same constraints:\n\n\
             - keep the transcript primary,\n\
             - expose activity without interleaving it, and\n\
             - make every detail reachable in one deliberate action.\n\n\
             This is synthetic output, but it uses the same incremental Markdown path as the opening demo."
        );
        let session = &mut self.sessions[self.focused];
        if session.messages.len() >= 16 {
            session.messages.drain(..2);
        }
        session.messages.push(Message::new(Role::User, prompt));
        session.messages.push(Message::new(Role::Assistant, ""));
        session.status = SessionStatus::Running;
        let message = session.messages.len() - 1;
        self.stream = Some(StreamState {
            session: self.focused,
            message,
            target: response.chars().collect(),
            cursor: 0,
            drives_demo: false,
        });
        self.input.clear();
    }

    fn breadcrumb(&self) -> String {
        let mut path = Vec::new();
        let mut cursor = Some(self.focused);
        while let Some(index) = cursor {
            path.push(self.sessions[index].title);
            cursor = self.sessions[index].parent;
        }
        path.reverse();
        path.join(" / ")
    }

    fn depth(&self, session: usize) -> usize {
        let mut depth = 0;
        let mut cursor = self.sessions[session].parent;
        while let Some(index) = cursor {
            depth += 1;
            cursor = self.sessions[index].parent;
        }
        depth
    }

    fn direct_children(&self, parent: usize) -> impl Iterator<Item = (usize, &Session)> {
        self.sessions
            .iter()
            .enumerate()
            .filter(move |(_, session)| session.parent == Some(parent))
    }

    fn thread_order(&self) -> Vec<usize> {
        let mut stack: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| session.parent.is_none().then_some(index))
            .rev()
            .collect();
        let mut order = Vec::with_capacity(self.sessions.len());
        while let Some(index) = stack.pop() {
            order.push(index);
            stack.extend(
                self.sessions
                    .iter()
                    .enumerate()
                    .filter_map(|(child, session)| (session.parent == Some(index)).then_some(child))
                    .rev(),
            );
        }
        order
    }

    fn frame(&self, width: u16, height: u16) -> Vec<Line> {
        let width = usize::from(width).max(1);
        let height = usize::from(height).max(1);
        if width < 32 || height < 9 {
            let mut lines = vec![
                Line::styled(" qq", accent().bold()),
                Line::default(),
                Line::styled("Terminal is too small for this prototype.", warning()),
                Line::styled("Resize to at least 32 x 9. Ctrl-C exits.", muted()),
            ];
            resize_frame(&mut lines, width, height);
            return lines;
        }

        let mut frame = vec![self.header(width), self.context_line(width)];
        let body_height = height.saturating_sub(5);
        let body = if self.navigator.is_some() {
            self.render_navigator(width, body_height)
        } else {
            let lines = match self.concept {
                Concept::Threadline => self.render_threadline(width),
                Concept::Marginalia => self.render_marginalia(width, body_height),
                Concept::FoldFocus => self.render_fold_focus(width),
            };
            fit_concept_viewport(lines, body_height)
        };
        frame.extend(body);
        frame.push(Line::styled("─".repeat(width), rule()));
        frame.push(self.composer(width));
        frame.push(truncate_line(
            Line::styled(
                " F1-F3 concepts   Tab reasoning   Ctrl-T threads   Esc parent   Ctrl-C quit",
                muted(),
            ),
            width,
        ));
        resize_frame(&mut frame, width, height);
        frame
    }

    fn header(&self, width: usize) -> Line {
        let mut line = Line::styled(" qq ", accent().bold());
        for (concept, key) in [
            (Concept::Threadline, "F1"),
            (Concept::Marginalia, "F2"),
            (Concept::FoldFocus, "F3"),
        ] {
            let style = if concept == self.concept {
                selected().bold()
            } else {
                muted()
            };
            line.push(format!(" {key} {} ", concept.name()), style);
        }
        truncate_line(line, width)
    }

    fn context_line(&self, width: usize) -> Line {
        let mut line = Line::styled("  ", muted());
        line.push(self.breadcrumb(), normal().bold());
        line.push("   reasoning ", muted());
        line.push(self.thinking.label(), thinking());
        truncate_line(line, width)
    }

    fn composer(&self, width: usize) -> Line {
        let mut line = Line::styled(" › ", accent().bold());
        if self.input.is_empty() {
            line.push("Ask QQ…", muted().italic());
        } else {
            line.push(
                tail_by_width(&self.input, width.saturating_sub(5)),
                normal(),
            );
        }
        let cursor = if self.pulse().is_multiple_of(2) {
            "▌"
        } else {
            " "
        };
        line.push(cursor, accent());
        truncate_line(line, width)
    }

    fn render_threadline(&self, width: usize) -> Vec<Line> {
        let mut lines = vec![concept_label(
            "01 / THREADLINE",
            "one chronological surface; agents become receipts",
        )];
        lines.push(Line::default());
        lines.extend(self.render_transcript(self.focused, width, TranscriptStyle::Rail));

        let children: Vec<_> = self.direct_children(self.focused).collect();
        if !children.is_empty() {
            lines.push(Line::default());
            lines.push(Line::styled("  ├─ delegated work", muted().bold()));
            for (_, child) in children {
                let mut line = Line::styled("  │  ", rule());
                line.push(
                    child.status.marker(self.pulse()),
                    status_style(child.status),
                );
                line.push(format!("  {}  ", child.title), normal().bold());
                line.push(child.summary, muted());
                lines.push(truncate_line(line, width));
            }
            lines.push(Line::styled("  └─ Ctrl-T opens the full thread", muted()));
        }
        if self.thinking != ThinkingMode::Hidden {
            lines.push(Line::default());
            lines.extend(self.render_thinking(self.focused, width, "  │  "));
        }
        lines
    }

    fn render_marginalia(&self, width: usize, height: usize) -> Vec<Line> {
        let label = concept_label(
            "02 / MARGINALIA",
            "conversation in the page; activity at its edge",
        );
        if width < 88 {
            let mut lines = vec![label, Line::default()];
            lines.extend(self.render_transcript(self.focused, width, TranscriptStyle::Plain));
            lines.push(Line::default());
            lines.push(Line::styled(
                "  AGENT MARGIN / collapsed below",
                margin_heading(),
            ));
            lines.extend(self.render_margin(self.focused, width.saturating_sub(2)));
            return lines;
        }

        let main_width = width * 2 / 3;
        let margin_width = width.saturating_sub(main_width + 3);
        let mut main = vec![label];
        main.extend(fit_viewport(
            self.render_transcript(self.focused, main_width, TranscriptStyle::Plain),
            height.saturating_sub(1),
            true,
        ));
        let margin_lines = self.render_margin(self.focused, margin_width);
        let margin = if self.thinking == ThinkingMode::Hidden {
            fit_viewport(margin_lines, height, false)
        } else {
            fit_concept_viewport(margin_lines, height)
        };
        join_columns(main, margin, main_width, margin_width)
    }

    fn render_margin(&self, parent: usize, width: usize) -> Vec<Line> {
        let mut lines = vec![Line::styled(" AGENT MARGIN", margin_heading())];
        for (_, child) in self.direct_children(parent) {
            lines.push(Line::default());
            let mut title = Line::styled(" ", muted());
            title.push(
                child.status.marker(self.pulse()),
                status_style(child.status),
            );
            title.push(format!("  {}", child.title), normal().bold());
            lines.push(truncate_line(title, width));
            for line in wrap_plain(child.summary, width.saturating_sub(3), muted()) {
                lines.push(indent_line(line, "   ", muted(), width));
            }
        }

        if self.thinking != ThinkingMode::Hidden {
            lines.push(Line::default());
            lines.push(Line::styled(" REASONING", margin_heading()));
            match self.thinking {
                ThinkingMode::Hidden => {}
                ThinkingMode::Pulse => {
                    lines.push(Line::styled(" ◌  structure before chrome", thinking()));
                }
                ThinkingMode::Expanded => {
                    for line in wrap_plain(
                        self.sessions[parent].thinking,
                        width.saturating_sub(2),
                        thinking().italic(),
                    ) {
                        lines.push(indent_line(line, " ", muted(), width));
                    }
                }
            }
        }
        lines
    }

    fn render_fold_focus(&self, width: usize) -> Vec<Line> {
        let content_width = width.min(86);
        let prefix = " ".repeat(width.saturating_sub(content_width) / 2);
        let session = &self.sessions[self.focused];
        let mut lines = vec![concept_label(
            "03 / FOLD + FOCUS",
            "past and parallel work compress around the active turn",
        )];
        lines.push(Line::default());

        if session.messages.len() > 2 {
            let hidden = session.messages.len() - 2;
            let mut fold = Line::styled("  ▸ ", accent());
            fold.push(format!("{hidden} earlier turns  "), muted().bold());
            fold.push(
                preview(&session.messages[hidden - 1].content, 44),
                muted().italic(),
            );
            lines.push(fold);
            lines.push(Line::default());
        }

        let start = session.messages.len().saturating_sub(2);
        for (message_index, message) in session.messages.iter().enumerate().skip(start) {
            let role = match message.role {
                Role::User => "YOU / NOW",
                Role::Assistant => "QQ / FOCUS",
            };
            lines.push(Line::styled(
                format!("  {role}"),
                role_style(message.role).bold(),
            ));
            let mut body = markdown_lines(&message.content, content_width.saturating_sub(4));
            if body.is_empty() {
                body.push(Line::styled("…", thinking()));
            }
            if self.is_streaming_message(self.focused, message_index) {
                body.last_mut()
                    .expect("the body always has a line")
                    .push(" ▋", accent());
            }
            for line in body {
                lines.push(indent_line(line, "    ", muted(), content_width));
            }
            lines.push(Line::default());
        }

        for (_, child) in self.direct_children(self.focused) {
            let mut fold = Line::styled("  ▸ ", status_style(child.status));
            fold.push(format!("{}  ", child.title), normal().bold());
            fold.push(child.status.label(), status_style(child.status));
            fold.push(format!("  {}", child.summary), muted());
            lines.push(truncate_line(fold, content_width));
        }
        if self.direct_children(self.focused).next().is_some() {
            lines.push(Line::styled(
                "    Ctrl-T to move focus into a folded session",
                muted().italic(),
            ));
        }
        if self.thinking != ThinkingMode::Hidden {
            lines.push(Line::default());
            lines.extend(self.render_thinking(self.focused, content_width, "  ▸ "));
        }

        if prefix.is_empty() {
            lines
        } else {
            lines
                .into_iter()
                .map(|line| indent_line(line, &prefix, muted(), width))
                .collect()
        }
    }

    fn render_transcript(
        &self,
        session_index: usize,
        width: usize,
        transcript_style: TranscriptStyle,
    ) -> Vec<Line> {
        let session = &self.sessions[session_index];
        let indent = match transcript_style {
            TranscriptStyle::Rail => 5,
            TranscriptStyle::Plain => 3,
        };
        let content_width = width.saturating_sub(indent).max(1);
        let mut lines = Vec::new();

        for (message_index, message) in session.messages.iter().enumerate() {
            if message_index > 0 {
                lines.push(Line::default());
            }
            let (marker, label) = match message.role {
                Role::User => ("◆", "YOU"),
                Role::Assistant => ("●", "QQ"),
            };
            let mut header = match transcript_style {
                TranscriptStyle::Rail => Line::styled(format!("  {marker}  "), rule()),
                TranscriptStyle::Plain => Line::styled("  ", muted()),
            };
            header.push(label, role_style(message.role).bold());
            if self.is_streaming_message(session_index, message_index) {
                header.push("  streaming", thinking());
            }
            lines.push(truncate_line(header, width));

            let mut body = markdown_lines(&message.content, content_width);
            if body.is_empty() {
                body.push(Line::styled("…", thinking()));
            }
            if self.is_streaming_message(session_index, message_index) {
                body.last_mut()
                    .expect("the body always has a line")
                    .push(" ▋", accent());
            }
            let prefix = match transcript_style {
                TranscriptStyle::Rail => "  │  ",
                TranscriptStyle::Plain => "   ",
            };
            for line in body {
                lines.push(indent_line(line, prefix, rule(), width));
            }
        }
        lines
    }

    fn render_thinking(&self, session: usize, width: usize, prefix: &str) -> Vec<Line> {
        match self.thinking {
            ThinkingMode::Hidden => Vec::new(),
            ThinkingMode::Pulse => {
                let active = self
                    .stream
                    .as_ref()
                    .is_some_and(|stream| stream.session == session);
                let text = if active {
                    format!("{}  reasoning", SessionStatus::Running.marker(self.pulse()))
                } else {
                    "◇  reasoning available".to_owned()
                };
                vec![indent_line(
                    Line::styled(text, thinking()),
                    prefix,
                    rule(),
                    width,
                )]
            }
            ThinkingMode::Expanded => wrap_plain(
                self.sessions[session].thinking,
                width.saturating_sub(visible_width(prefix)),
                thinking().italic(),
            )
            .into_iter()
            .map(|line| indent_line(line, prefix, rule(), width))
            .collect(),
        }
    }

    fn render_navigator(&self, width: usize, height: usize) -> Vec<Line> {
        let selected_index = self.navigator.unwrap_or(self.focused);
        let mut lines = vec![
            concept_label("THREADS", "Up/Down select · Enter focus · Esc close"),
            Line::default(),
        ];
        let mut selected_row = 0;
        for index in self.thread_order() {
            let session = &self.sessions[index];
            let is_selected = index == selected_index;
            let row_style = if is_selected { selected() } else { normal() };
            let mut line = Line::styled(if is_selected { " › " } else { "   " }, row_style);
            line.push("  ".repeat(self.depth(index)), row_style);
            line.push(
                session.status.marker(self.pulse()),
                status_style(session.status),
            );
            line.push(format!("  {}", session.title), row_style.bold());
            line.push(
                format!("  {}", session.status.label()),
                status_style(session.status),
            );
            if is_selected {
                selected_row = lines.len();
            }
            lines.push(truncate_line(line, width));
            if is_selected {
                let summary_width = width.saturating_sub(8 + self.depth(index) * 2);
                for summary in wrap_plain(session.summary, summary_width, muted()) {
                    lines.push(indent_line(summary, "       ", muted(), width));
                }
            }
        }
        fit_navigator(lines, height, selected_row)
    }
}

#[derive(Clone, Copy, Debug)]
enum TranscriptStyle {
    Rail,
    Plain,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TextStyle {
    foreground: Option<Color>,
    bold: bool,
    dim: bool,
    italic: bool,
    underlined: bool,
    crossed_out: bool,
    reverse: bool,
}

impl TextStyle {
    fn foreground(color: Color) -> Self {
        Self {
            foreground: Some(color),
            ..Self::default()
        }
    }

    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    fn underlined(mut self) -> Self {
        self.underlined = true;
        self
    }

    fn crossed_out(mut self) -> Self {
        self.crossed_out = true;
        self
    }

    fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Span {
    text: String,
    style: TextStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Line {
    spans: Vec<Span>,
}

impl Line {
    fn styled(text: impl Into<String>, style: TextStyle) -> Self {
        let mut line = Self::default();
        line.push(text, style);
        line
    }

    fn push(&mut self, text: impl Into<String>, style: TextStyle) {
        let text = text.into();
        let text = if text
            .chars()
            .all(|character| terminal_safe_character(character) == Some(character))
        {
            text
        } else {
            text.chars().filter_map(terminal_safe_character).collect()
        };
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

    fn append(&mut self, other: Line) {
        for span in other.spans {
            self.push(span.text, span.style);
        }
    }

    fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|span| visible_width(&span.text))
            .sum()
    }

    fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    #[cfg(test)]
    fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

fn normal() -> TextStyle {
    TextStyle::default()
}

fn accent() -> TextStyle {
    TextStyle::foreground(Color::Cyan)
}

fn muted() -> TextStyle {
    TextStyle::foreground(Color::DarkGrey).dim()
}

fn rule() -> TextStyle {
    TextStyle::foreground(Color::DarkGrey)
}

fn thinking() -> TextStyle {
    TextStyle::foreground(Color::Magenta).dim()
}

fn warning() -> TextStyle {
    TextStyle::foreground(Color::Yellow).bold()
}

fn selected() -> TextStyle {
    TextStyle::foreground(Color::Cyan).reverse()
}

fn margin_heading() -> TextStyle {
    TextStyle::foreground(Color::Yellow).bold()
}

fn role_style(role: Role) -> TextStyle {
    match role {
        Role::User => TextStyle::foreground(Color::Green),
        Role::Assistant => accent(),
    }
}

fn status_style(status: SessionStatus) -> TextStyle {
    match status {
        SessionStatus::Queued => muted(),
        SessionStatus::Running => TextStyle::foreground(Color::Yellow),
        SessionStatus::Done => TextStyle::foreground(Color::Green),
    }
}

fn visible_width(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn truncate_line(line: Line, width: usize) -> Line {
    if width == 0 {
        return Line::default();
    }
    if line.width() <= width {
        return line;
    }

    let target = width.saturating_sub(1);
    let mut truncated = Line::default();
    let mut used = 0;
    'spans: for span in line.spans {
        let mut text = String::new();
        for character in span.text.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used + character_width > target {
                if !text.is_empty() {
                    truncated.push(text, span.style);
                }
                break 'spans;
            }
            text.push(character);
            used += character_width;
        }
        truncated.push(text, span.style);
    }
    truncated.push("…", muted());
    truncated
}

fn tail_by_width(text: &str, width: usize) -> String {
    let mut reversed = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        reversed.push(character);
        used += character_width;
    }
    reversed.into_iter().rev().collect()
}

fn wrap_line(line: Line, width: usize) -> Vec<Line> {
    if width == 0 {
        return vec![Line::default()];
    }
    if line.is_empty() {
        return vec![line];
    }

    let mut output = vec![Line::default()];
    let mut used = 0;
    for span in line.spans {
        let mut text = String::new();
        for character in span.text.chars() {
            if character == '\n' {
                output
                    .last_mut()
                    .expect("output starts with a line")
                    .push(std::mem::take(&mut text), span.style);
                output.push(Line::default());
                used = 0;
                continue;
            }
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used > 0 && used + character_width > width {
                output
                    .last_mut()
                    .expect("output starts with a line")
                    .push(std::mem::take(&mut text), span.style);
                output.push(Line::default());
                used = 0;
            }
            if character_width > width {
                continue;
            }
            text.push(character);
            used += character_width;
        }
        output
            .last_mut()
            .expect("output starts with a line")
            .push(text, span.style);
    }
    output
}

fn wrap_plain(text: &str, width: usize, style: TextStyle) -> Vec<Line> {
    wrap_line(Line::styled(text, style), width)
}

fn indent_line(line: Line, prefix: &str, prefix_style: TextStyle, width: usize) -> Line {
    let mut indented = Line::styled(prefix, prefix_style);
    indented.append(line);
    truncate_line(indented, width)
}

fn join_columns(
    left: Vec<Line>,
    right: Vec<Line>,
    left_width: usize,
    right_width: usize,
) -> Vec<Line> {
    let height = left.len().max(right.len());
    (0..height)
        .map(|index| {
            let mut line = truncate_line(left.get(index).cloned().unwrap_or_default(), left_width);
            let padding = left_width.saturating_sub(line.width());
            line.push(" ".repeat(padding), normal());
            line.push(" │ ", rule());
            line.append(truncate_line(
                right.get(index).cloned().unwrap_or_default(),
                right_width,
            ));
            line
        })
        .collect()
}

fn fit_viewport(mut lines: Vec<Line>, height: usize, follow_tail: bool) -> Vec<Line> {
    if lines.len() > height {
        if follow_tail {
            lines = lines.split_off(lines.len() - height);
        } else {
            lines.truncate(height);
        }
    }
    lines.resize_with(height, Line::default);
    lines
}

fn fit_concept_viewport(mut lines: Vec<Line>, height: usize) -> Vec<Line> {
    if height == 0 {
        return Vec::new();
    }
    if lines.len() <= height {
        lines.resize_with(height, Line::default);
        return lines;
    }

    let heading = lines.remove(0);
    let mut visible = vec![heading];
    visible.extend(fit_viewport(lines, height - 1, true));
    visible
}

fn fit_navigator(mut lines: Vec<Line>, height: usize, selected_row: usize) -> Vec<Line> {
    if lines.len() <= height {
        lines.resize_with(height, Line::default);
        return lines;
    }
    if height <= 2 {
        lines.truncate(height);
        return lines;
    }

    let body = lines.split_off(2);
    let available = height - 2;
    let selected = selected_row.saturating_sub(2);
    let start = selected
        .saturating_sub(available / 2)
        .min(body.len().saturating_sub(available));
    lines.extend(body.into_iter().skip(start).take(available));
    lines
}

fn resize_frame(lines: &mut Vec<Line>, width: usize, height: usize) {
    lines.truncate(height);
    lines.resize_with(height, Line::default);
    for line in lines {
        if line.width() > width {
            *line = truncate_line(std::mem::take(line), width);
        }
    }
}

fn concept_label(name: &str, description: &str) -> Line {
    let mut line = Line::styled(format!("  {name}"), accent().bold());
    line.push(format!("  ·  {description}"), muted().italic());
    line
}

fn preview(markdown: &str, width: usize) -> String {
    let plain = markdown
        .chars()
        .map(|character| match character {
            '*' | '_' | '`' | '#' => ' ',
            other => other,
        })
        .collect::<String>();
    let compact = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    tail_or_head_by_width(&compact, width)
}

fn tail_or_head_by_width(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn markdown_lines(markdown: &str, width: usize) -> Vec<Line> {
    if markdown.is_empty() {
        return Vec::new();
    }

    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let mut lines = vec![Line::default()];
    let mut styles = vec![normal()];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;

    for event in Parser::new_ext(markdown, options) {
        match event {
            MarkdownEvent::Start(tag) => {
                let mut style = *styles.last().expect("the base style remains present");
                match &tag {
                    Tag::Heading { .. } => {
                        ensure_new_line(&mut lines);
                        style = accent().bold();
                    }
                    Tag::BlockQuote(_) => {
                        ensure_new_line(&mut lines);
                        lines
                            .last_mut()
                            .expect("lines starts populated")
                            .push("│ ", rule());
                        style = style.italic();
                    }
                    Tag::CodeBlock(_) => {
                        ensure_new_line(&mut lines);
                        lines
                            .last_mut()
                            .expect("lines starts populated")
                            .push("  ", rule());
                        style = TextStyle::foreground(Color::Yellow);
                        in_code_block = true;
                    }
                    Tag::List(start) => {
                        ensure_new_line(&mut lines);
                        lists.push(*start);
                    }
                    Tag::Item => {
                        ensure_new_line(&mut lines);
                        let marker = match lists.last_mut() {
                            Some(Some(next)) => {
                                let marker = format!("{next}. ");
                                *next += 1;
                                marker
                            }
                            _ => "• ".to_owned(),
                        };
                        lines
                            .last_mut()
                            .expect("lines starts populated")
                            .push(marker, accent());
                    }
                    Tag::TableRow => ensure_new_line(&mut lines),
                    Tag::TableCell => {
                        if !lines.last().is_none_or(Line::is_empty) {
                            lines
                                .last_mut()
                                .expect("lines starts populated")
                                .push(" │ ", rule());
                        }
                    }
                    Tag::Emphasis => style = style.italic(),
                    Tag::Strong => style = style.bold(),
                    Tag::Strikethrough => style = style.crossed_out(),
                    Tag::Link { .. } => style = accent().underlined(),
                    Tag::Image { .. } => style = muted().italic(),
                    Tag::DefinitionListTitle => style = style.bold(),
                    Tag::DefinitionListDefinition => style = style.italic(),
                    Tag::Paragraph
                    | Tag::HtmlBlock
                    | Tag::FootnoteDefinition(_)
                    | Tag::DefinitionList
                    | Tag::Table(_)
                    | Tag::TableHead
                    | Tag::Superscript
                    | Tag::Subscript
                    | Tag::MetadataBlock(_) => {}
                }
                styles.push(style);
            }
            MarkdownEvent::End(tag) => {
                if styles.len() > 1 {
                    styles.pop();
                }
                match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) => {
                        paragraph_break(&mut lines);
                    }
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        paragraph_break(&mut lines);
                    }
                    TagEnd::List(_) => {
                        lists.pop();
                        ensure_new_line(&mut lines);
                    }
                    TagEnd::Item | TagEnd::TableRow => ensure_new_line(&mut lines),
                    TagEnd::TableCell => {}
                    TagEnd::HtmlBlock
                    | TagEnd::FootnoteDefinition
                    | TagEnd::DefinitionList
                    | TagEnd::DefinitionListTitle
                    | TagEnd::DefinitionListDefinition
                    | TagEnd::Table
                    | TagEnd::TableHead
                    | TagEnd::Emphasis
                    | TagEnd::Strong
                    | TagEnd::Strikethrough
                    | TagEnd::Superscript
                    | TagEnd::Subscript
                    | TagEnd::Link
                    | TagEnd::Image
                    | TagEnd::MetadataBlock(_) => {}
                }
            }
            MarkdownEvent::Text(text) => append_markdown_text(
                &mut lines,
                &text,
                *styles.last().expect("the base style remains present"),
                in_code_block,
            ),
            MarkdownEvent::Code(code) => {
                let mut style = TextStyle::foreground(Color::Yellow);
                style.bold = true;
                let line = lines.last_mut().expect("lines starts populated");
                line.push("‹", rule());
                line.push(code.to_string(), style);
                line.push("›", rule());
            }
            MarkdownEvent::InlineMath(math) => {
                lines
                    .last_mut()
                    .expect("lines starts populated")
                    .push(format!("${math}$"), TextStyle::foreground(Color::Yellow));
            }
            MarkdownEvent::DisplayMath(math) => {
                ensure_new_line(&mut lines);
                lines
                    .last_mut()
                    .expect("lines starts populated")
                    .push(format!("  {math}"), TextStyle::foreground(Color::Yellow));
                paragraph_break(&mut lines);
            }
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => {
                append_markdown_text(&mut lines, &html, muted(), false);
            }
            MarkdownEvent::FootnoteReference(reference) => {
                lines
                    .last_mut()
                    .expect("lines starts populated")
                    .push(format!("[{reference}]"), accent());
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => ensure_new_line(&mut lines),
            MarkdownEvent::Rule => {
                ensure_new_line(&mut lines);
                lines
                    .last_mut()
                    .expect("lines starts populated")
                    .push("────────────", rule());
                ensure_new_line(&mut lines);
            }
            MarkdownEvent::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                lines
                    .last_mut()
                    .expect("lines starts populated")
                    .push(marker, accent());
            }
        }
    }

    while lines.last().is_some_and(Line::is_empty) {
        lines.pop();
    }
    lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

fn append_markdown_text(lines: &mut Vec<Line>, text: &str, style: TextStyle, in_code_block: bool) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            lines.push(Line::default());
            if in_code_block {
                lines
                    .last_mut()
                    .expect("a line was just pushed")
                    .push("  ", rule());
            }
        }
        lines
            .last_mut()
            .expect("lines starts populated")
            .push(part, style);
    }
}

fn ensure_new_line(lines: &mut Vec<Line>) {
    if !lines.last().is_none_or(Line::is_empty) {
        lines.push(Line::default());
    }
}

fn paragraph_break(lines: &mut Vec<Line>) {
    ensure_new_line(lines);
    lines.push(Line::default());
}

#[derive(Debug, Default)]
struct Renderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
}

impl Renderer {
    fn draw(&mut self, app: &App) -> io::Result<Vec<u8>> {
        let size = terminal::size()?;
        let frame = app.frame(
            size.0.clamp(1, MAX_RENDER_WIDTH),
            size.1.clamp(1, MAX_RENDER_HEIGHT),
        );
        let resized = self.size != Some(size);
        let mut output = Vec::with_capacity(4096);

        queue!(&mut output, BeginSynchronizedUpdate)?;
        if resized {
            queue!(&mut output, Clear(ClearType::All))?;
        }
        for (row, line) in frame.iter().enumerate() {
            if resized || self.previous.get(row) != Some(line) {
                queue!(
                    &mut output,
                    MoveTo(0, u16::try_from(row).expect("terminal rows fit in u16")),
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
        self.size = Some(size);
        Ok(output)
    }
}

fn write_line(output: &mut impl Write, line: &Line) -> io::Result<()> {
    for span in &line.spans {
        queue!(output, SetAttribute(Attribute::Reset), ResetColor)?;
        if let Some(color) = span.style.foreground {
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
        if span.style.underlined {
            queue!(output, SetAttribute(Attribute::Underlined))?;
        }
        if span.style.crossed_out {
            queue!(output, SetAttribute(Attribute::CrossedOut))?;
        }
        if span.style.reverse {
            queue!(output, SetAttribute(Attribute::Reverse))?;
        }
        queue!(output, Print(&span.text))?;
    }
    Ok(())
}

#[derive(Debug)]
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(
            stdout(),
            EnableBracketedPaste,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let mut output = stdout();
        let _ = execute!(
            output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            EndSynchronizedUpdate,
            DisableBracketedPaste,
            MoveTo(0, height.saturating_sub(1)),
            Clear(ClearType::CurrentLine),
            Show,
            Print("\r\n")
        );
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        result = &mut shutdown => return result,
        _ = tokio::task::yield_now() => {}
    }

    let _terminal = TerminalGuard::enter()?;
    let mut output = tokio::io::stdout();
    let mut events = EventStream::new();
    let mut stream_tick = interval(Duration::from_millis(32));
    let mut frame_tick = interval(Duration::from_millis(16));
    stream_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut app = App::new();
    let mut renderer = Renderer::default();
    let mut dirty = true;

    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                break;
            }
            event = events.next() => {
                match event {
                    Some(Ok(event)) => dirty |= app.handle_event(event),
                    Some(Err(error)) => return Err(error),
                    None => break,
                }
            }
            _ = stream_tick.tick(), if app.has_visible_activity() => {
                dirty |= app.advance();
            }
            _ = frame_tick.tick(), if dirty => {
                let frame = renderer.draw(&app)?;
                tokio::select! {
                    result = &mut shutdown => {
                        result?;
                        return Ok(());
                    }
                    result = async {
                        output.write_all(&frame).await?;
                        output.flush().await
                    } => result?,
                }
                dirty = false;
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_applies_inline_styles() {
        let lines = markdown_lines("A **strong** and `coded` value.", 80);
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();

        assert!(
            spans
                .iter()
                .any(|span| span.text == "strong" && span.style.bold)
        );
        assert!(
            spans.iter().any(|span| {
                span.text == "coded" && span.style.foreground == Some(Color::Yellow)
            })
        );
    }

    #[test]
    fn decoded_markdown_entities_cannot_emit_terminal_controls() {
        let lines = markdown_lines("&#27;]52;c;Y2xpcGJvYXJk&#7;", 80);

        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.text
                .chars()
                .all(|character| terminal_safe_character(character) == Some(character))
        }));
    }

    #[test]
    fn wrapped_rows_respect_the_available_width() {
        let lines = markdown_lines("**Streaming** text remains narrow and readable.", 9);

        assert!(lines.iter().all(|line| line.width() <= 9));
        assert_eq!(
            lines.iter().map(Line::text).collect::<String>(),
            "Streaming text remains narrow and readable."
        );
    }

    #[test]
    fn viewport_follows_the_mutable_tail() {
        let lines = (0..6)
            .map(|index| Line::styled(index.to_string(), normal()))
            .collect();
        let visible = fit_viewport(lines, 3, true);

        assert_eq!(
            visible.iter().map(Line::text).collect::<Vec<_>>(),
            ["3", "4", "5"]
        );
    }

    #[test]
    fn synthetic_stream_reaches_a_terminal_state() {
        let mut app = App::new();
        while app.stream.is_some() {
            app.advance();
        }

        assert!(
            app.sessions
                .iter()
                .all(|session| session.status == SessionStatus::Done)
        );
        assert!(app.sessions[0].messages[3].content.contains("F1-F3"));
    }

    #[test]
    fn pasted_control_characters_cannot_reach_terminal_output() {
        let mut app = App::new();
        app.handle_event(Event::Paste("safe\u{1b}[31m\nnext".to_owned()));

        assert_eq!(app.input, "safe[31m next");
        assert!(!app.input.chars().any(char::is_control));
    }

    #[test]
    fn replacing_a_stream_completes_its_previous_session() {
        let mut app = App::new();
        app.focused = 1;
        app.input.push_str("try another direction");
        app.send_input();

        assert_eq!(app.sessions[0].status, SessionStatus::Done);
        assert_eq!(app.sessions[1].status, SessionStatus::Running);
        assert_eq!(app.stream.as_ref().map(|stream| stream.session), Some(1));

        while app.stream.is_some() {
            app.advance();
        }
        assert!(
            app.sessions
                .iter()
                .all(|session| session.status != SessionStatus::Running)
        );
    }

    #[test]
    fn navigator_uses_tree_order_and_keeps_the_selection_visible() {
        let mut app = App::new();
        assert_eq!(app.thread_order(), [0, 1, 3, 2]);

        app.navigator = Some(2);
        let rows = app.render_navigator(32, 4);
        assert_eq!(rows.len(), 4);
        assert!(
            rows.iter()
                .map(Line::text)
                .any(|row| row.contains("markdown edges"))
        );
    }

    #[test]
    fn live_regions_survive_transcript_tail_clipping() {
        let mut app = App::new();
        while app.stream.is_some() {
            app.advance();
        }
        app.thinking = ThinkingMode::Expanded;

        let threadline = app.frame(80, 15);
        assert!(
            threadline
                .iter()
                .map(Line::text)
                .any(|row| row.contains("Separating"))
        );

        app.concept = Concept::Marginalia;
        let marginalia = app.frame(100, 15);
        assert!(
            marginalia
                .iter()
                .map(Line::text)
                .any(|row| row.contains("AGENT MARGIN"))
        );
    }

    #[test]
    fn reasoning_remains_visible_at_the_minimum_terminal_size() {
        let mut app = App::new();
        app.thinking = ThinkingMode::Pulse;

        for concept in [Concept::Threadline, Concept::Marginalia, Concept::FoldFocus] {
            app.concept = concept;
            let frame = app.frame(32, 9);
            assert!(
                frame
                    .iter()
                    .map(Line::text)
                    .any(|row| { row.to_ascii_lowercase().contains("reasoning") })
            );
        }
    }

    #[test]
    fn every_concept_produces_a_bounded_frame() {
        let mut app = App::new();
        for concept in [Concept::Threadline, Concept::Marginalia, Concept::FoldFocus] {
            app.concept = concept;
            let frame = app.frame(100, 30);
            assert_eq!(frame.len(), 30);
            assert!(frame.iter().all(|line| line.width() <= 100));
        }
    }
}
