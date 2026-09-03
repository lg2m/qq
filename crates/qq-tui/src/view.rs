//! Frame assembly: composes chrome, transcript, and overlays into the lines
//! the renderer diffs against the previous frame.

mod highlight;
mod markdown;
mod wrap;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    ops::Range,
};

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use qq_protocol::{
    MessageId, MessageRole, MessageSnapshot, MessageState, RunId, SessionId, SessionStatus,
    ToolCallDisplay, ToolCallId, ToolCallSnapshot, ToolCallState,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    Layout,
    app::{App, ToolDetail, terminal_safe_character},
    input::{Mode, SessionConfirm},
    panes::{PaneId, Rect, Tile, Viewport},
    render::{
        Line, Style, accent, brand, diff_line_style, failure, muted, normal, warning, write_line,
    },
    theme,
};
use highlight::HighlightKey;
pub(crate) use highlight::{Highlighted, Highlighter};
use markdown::{has_fenced_code, markdown_lines, settled_prefix_end};
#[cfg(test)]
use wrap::transcript_viewport;
use wrap::{
    bounded_tail, fit_height, indent_lines, preview, selection_viewport, truncate_line, wrap_line,
    wrap_line_chars,
};

const MAX_RENDER_WIDTH: u16 = 320;
const MAX_RENDER_HEIGHT: u16 = 160;
const MAX_LIVE_MARKDOWN_BYTES: usize = 32 * 1024;
const MAX_VISIBLE_MESSAGES: usize = 64;
const MAX_LIVE_MARKDOWN_ROWS: usize = MAX_RENDER_HEIGHT as usize;
/// Completed messages at or below these bounds retain full markdown styling.
/// Larger messages use a sparse plain-text row index so scrolling stays
/// complete without caching every rendered row.
const MAX_FULL_MARKDOWN_BYTES: usize = 64 * 1024;
const MAX_FULL_MARKDOWN_ROWS: usize = 4 * 1024;
const PLAIN_TEXT_CHECKPOINT_ROWS: usize = 1024;
const MAX_PLAIN_TEXT_CHECKPOINTS: usize = 4 * 1024;
const MAX_PLAIN_TEXT_ROW_BYTES: usize = 4 * 1024;
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
/// Columns the session sidebar occupies, including its left border.
const SIDEBAR_WIDTH: usize = 36;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: &str = env!("QQ_GIT_COMMIT");

/// Frame assembly and the row diff against the previous frame. Per-pane
/// transcript state lives in a [`TranscriptCache`] per pane; the highlighter
/// is shared because its results are keyed by message and width, not pane.
#[derive(Default)]
pub(crate) struct FrameRenderer {
    previous: Vec<Line>,
    size: Option<(u16, u16)>,
    /// Off-tick syntax highlighting for cached completed messages.
    pub(crate) highlighter: Highlighter,
    panes: HashMap<PaneId, TranscriptCache>,
    /// `App::theme_generation` the caches were built under. Cached rows
    /// bake in colors, so a theme change discards every layout and forces
    /// a full repaint.
    theme_generation: u64,
}

/// Retained rendering for one pane's transcript: completed messages cached
/// by width, the settled prefix of streaming messages, and the row ranges
/// the live messages occupied on the last frame.
#[derive(Default)]
pub(crate) struct TranscriptCache {
    markdown: HashMap<MessageId, CachedMarkdown>,
    /// Monotonic counter bumped per `prepare_markdown`; stamps cache use.
    clock: u64,
    /// Settled rows of messages still streaming, keyed by message. Each entry
    /// holds the layout of the message's block-boundary-settled prefix so a
    /// frame only lays out the trailing open block.
    live: HashMap<MessageId, LiveMarkdown>,
    live_message_ranges: HashMap<MessageId, Range<usize>>,
    preserve_tail_anchor: bool,
}

struct LiveMarkdown {
    width: usize,
    /// Bytes of the combined output+refusal text covered by `rows`.
    settled_bytes: usize,
    /// Rendered, indented rows for the settled prefix.
    rows: Vec<Line>,
}

struct CachedMarkdown {
    width: usize,
    output_bytes: usize,
    refusal_bytes: usize,
    loaded_through: u64,
    body: CachedMessageBody,
    /// A highlighted layout has been requested or applied; `false` means a
    /// later frame should try again once the highlighter has capacity.
    highlight_requested: bool,
    /// Frame counter at last use, for least-recently-used eviction.
    last_used: u64,
}

impl CachedMarkdown {
    fn key(&self, message_id: MessageId) -> HighlightKey {
        HighlightKey {
            message_id,
            width: self.width,
            output_bytes: self.output_bytes,
            refusal_bytes: self.refusal_bytes,
            loaded_through: self.loaded_through,
        }
    }
}

enum CachedMessageBody {
    Markdown(Vec<Line>),
    Plain(PlainTextIndex),
}

#[derive(Debug, Clone, Copy)]
struct PlainTextCheckpoint {
    row: usize,
    byte: usize,
}

#[derive(Clone, Copy)]
struct MessageText<'a> {
    output: &'a str,
    refusal: &'a str,
}

impl<'a> MessageText<'a> {
    const SEPARATOR: &'static str = "\n\n";

    fn new(message: &'a MessageSnapshot) -> Self {
        Self {
            output: &message.output,
            refusal: &message.refusal,
        }
    }

    const fn has_separator(self) -> bool {
        !self.output.is_empty() && !self.refusal.is_empty()
    }

    fn len(self) -> usize {
        self.output.len()
            + self.refusal.len()
            + usize::from(self.has_separator()) * Self::SEPARATOR.len()
    }

    fn as_cow(self) -> Cow<'a, str> {
        if self.refusal.is_empty() {
            return Cow::Borrowed(self.output);
        }
        if self.output.is_empty() {
            return Cow::Borrowed(self.refusal);
        }
        Cow::Owned(format!(
            "{}{}{}",
            self.output,
            Self::SEPARATOR,
            self.refusal
        ))
    }

    fn next_char(self, byte: usize) -> Option<(char, usize)> {
        if byte < self.output.len() {
            let character = self.output[byte..].chars().next()?;
            return Some((character, byte + character.len_utf8()));
        }
        let refusal_start =
            self.output.len() + usize::from(self.has_separator()) * Self::SEPARATOR.len();
        if byte < refusal_start {
            return Some(('\n', byte + 1));
        }
        if byte < self.len() {
            let local = byte - refusal_start;
            let character = self.refusal[local..].chars().next()?;
            return Some((character, byte + character.len_utf8()));
        }
        None
    }

    fn is_char_boundary(self, byte: usize) -> bool {
        if byte <= self.output.len() {
            return self.output.is_char_boundary(byte);
        }
        let refusal_start =
            self.output.len() + usize::from(self.has_separator()) * Self::SEPARATOR.len();
        if byte <= refusal_start {
            return true;
        }
        let local = byte - refusal_start;
        local <= self.refusal.len() && self.refusal.is_char_boundary(local)
    }

    fn collect_range(self, range: Range<usize>, sanitize: bool) -> String {
        let mut collected = String::with_capacity(range.len());
        let mut byte = range.start.min(self.len());
        let end = range.end.min(self.len());
        while byte < end {
            let Some((character, next)) = self.next_char(byte) else {
                break;
            };
            if !sanitize {
                collected.push(character);
            } else if let Some(character) = terminal_safe_character(character) {
                collected.push(character);
            }
            byte = next;
        }
        collected
    }

    fn bounded_tail(self, max_bytes: usize) -> Cow<'a, str> {
        if self.len() <= max_bytes {
            return self.as_cow();
        }
        let mut start = self.len() - max_bytes;
        while !self.is_char_boundary(start) {
            start += 1;
        }
        Cow::Owned(self.collect_range(start..self.len(), false))
    }
}

/// Sparse row index for oversized completed messages. It stores one byte
/// checkpoint per bounded group of visual rows and reconstructs only the
/// requested viewport. Checkpoints are compacted as the message grows, so the
/// source is scanned once while retained memory stays predictable.
struct PlainTextIndex {
    content_width: usize,
    rows: usize,
    checkpoints: Vec<PlainTextCheckpoint>,
}

impl PlainTextIndex {
    fn new(source: MessageText<'_>, content_width: usize) -> Self {
        let content_width = content_width.max(1);
        let mut checkpoints = vec![PlainTextCheckpoint { row: 0, byte: 0 }];
        let mut checkpoint_rows = PLAIN_TEXT_CHECKPOINT_ROWS;
        let mut rows = 0;
        let mut byte = 0;
        while let Some((_, next)) = next_plain_text_row(source, byte, content_width) {
            rows += 1;
            byte = next;
            if checkpoints.len() >= MAX_PLAIN_TEXT_CHECKPOINTS {
                checkpoint_rows = checkpoint_rows.saturating_mul(2);
                checkpoints.retain(|checkpoint| checkpoint.row % checkpoint_rows == 0);
            }
            if rows % checkpoint_rows == 0 && byte < source.len() {
                checkpoints.push(PlainTextCheckpoint { row: rows, byte });
            }
        }
        Self {
            content_width,
            rows,
            checkpoints,
        }
    }

    fn render(
        &self,
        source: MessageText<'_>,
        rows: Range<usize>,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    ) -> Vec<Line> {
        let rows = rows.start.min(self.rows)..rows.end.min(self.rows);
        if rows.is_empty() {
            return Vec::new();
        }
        let checkpoint = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.row <= rows.start)
            .saturating_sub(1);
        let checkpoint = self.checkpoints[checkpoint];
        let mut current_row = checkpoint.row;
        let mut byte = checkpoint.byte;
        let mut rendered = Vec::with_capacity(rows.len());
        while let Some((range, next)) = next_plain_text_row(source, byte, self.content_width) {
            if current_row >= rows.end {
                break;
            }
            if current_row >= rows.start {
                let safe = source.collect_range(range, true);
                let mut line = Line::styled(prefix, prefix_style);
                line.push(safe, normal());
                rendered.push(truncate_line(line, width));
            }
            current_row += 1;
            byte = next;
        }
        rendered
    }
}

enum BodySegment<'a> {
    Owned(Vec<Line>),
    Cached(&'a [Line]),
    Plain {
        index: &'a PlainTextIndex,
        message_id: MessageId,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    },
}

impl BodySegment<'_> {
    fn rows(&self) -> usize {
        match self {
            Self::Owned(lines) => lines.len(),
            Self::Cached(lines) => lines.len(),
            Self::Plain { index, .. } => index.rows,
        }
    }
}

#[derive(Default)]
struct VirtualBody<'a> {
    segments: Vec<BodySegment<'a>>,
    rows: usize,
    preserve_tail_anchor: bool,
    live_message_ranges: Vec<(MessageId, Range<usize>)>,
}

impl<'a> VirtualBody<'a> {
    fn is_empty(&self) -> bool {
        self.rows == 0
    }

    fn push_line(&mut self, line: Line) {
        self.extend_owned(vec![line]);
    }

    fn extend_owned(&mut self, mut lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        self.rows += lines.len();
        if let Some(BodySegment::Owned(current)) = self.segments.last_mut() {
            current.append(&mut lines);
        } else {
            self.segments.push(BodySegment::Owned(lines));
        }
    }

    fn extend_cached(&mut self, lines: &'a [Line]) {
        if lines.is_empty() {
            return;
        }
        self.rows += lines.len();
        self.segments.push(BodySegment::Cached(lines));
    }

    fn extend_plain(
        &mut self,
        index: &'a PlainTextIndex,
        message_id: MessageId,
        prefix: &'static str,
        prefix_style: Style,
        width: usize,
    ) {
        if index.rows == 0 {
            return;
        }
        self.rows += index.rows;
        self.segments.push(BodySegment::Plain {
            index,
            message_id,
            prefix,
            prefix_style,
            width,
        });
    }

    fn extend_virtual(&mut self, mut other: VirtualBody<'a>) {
        let row_offset = self.rows;
        for (_, range) in &mut other.live_message_ranges {
            range.start += row_offset;
            range.end += row_offset;
        }
        self.rows += other.rows;
        self.preserve_tail_anchor |= other.preserve_tail_anchor;
        self.live_message_ranges
            .append(&mut other.live_message_ranges);
        self.segments.append(&mut other.segments);
    }

    fn viewport(&self, app: &App, height: usize, offset: usize) -> Vec<Line> {
        let offset = offset.min(self.rows.saturating_sub(height));
        let end = self.rows.saturating_sub(offset);
        let start = end.saturating_sub(height);
        let mut rendered = Vec::with_capacity(height.min(self.rows));
        let mut segment_start = 0;
        for segment in &self.segments {
            let segment_end = segment_start + segment.rows();
            let local_start = start.saturating_sub(segment_start).min(segment.rows());
            let local_end = end.saturating_sub(segment_start).min(segment.rows());
            if local_start < local_end {
                match segment {
                    BodySegment::Owned(lines) => {
                        rendered.extend_from_slice(&lines[local_start..local_end]);
                    }
                    BodySegment::Cached(lines) => {
                        rendered.extend_from_slice(&lines[local_start..local_end]);
                    }
                    BodySegment::Plain {
                        index,
                        message_id,
                        prefix,
                        prefix_style,
                        width,
                    } => {
                        // The message can only vanish between prepare and
                        // viewport if a snapshot replaced the session inside
                        // one frame; blank rows are the safe degradation.
                        match find_message(app, *message_id) {
                            Some(message) => rendered.extend(index.render(
                                MessageText::new(message),
                                local_start..local_end,
                                prefix,
                                *prefix_style,
                                *width,
                            )),
                            None => rendered.extend(std::iter::repeat_n(
                                Line::default(),
                                local_end - local_start,
                            )),
                        }
                    }
                }
            }
            if segment_end >= end {
                break;
            }
            segment_start = segment_end;
        }
        fit_height(rendered, height)
    }
}

impl FrameRenderer {
    /// Forget the previous frame so the next draw repaints every row, after
    /// something else (an external editor) wrote to the terminal.
    pub(crate) fn invalidate(&mut self) {
        self.previous.clear();
        self.size = None;
    }

    /// Render one frame for a terminal of `actual_size` columns and rows and
    /// return the bytes that bring the terminal from the previous frame to
    /// this one. Only changed rows are emitted unless the size changed.
    pub fn draw(&mut self, app: &mut App, actual_size: (u16, u16)) -> io::Result<Vec<u8>> {
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
        theme::activate(app.theme().palette);
        if self.theme_generation != app.theme_generation {
            self.theme_generation = app.theme_generation;
            self.panes.clear();
            self.invalidate();
        }
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
        let status_lines = status_notice(app, width);
        // Header, context, two footer rows, and the optional notice are fixed.
        // The composer can grow with wrapped multi-line input, so body height is
        // computed after the composer is laid out against the remaining space.
        let fixed_chrome_rows = 4 + status_lines.len();
        let max_composer_rows = height
            .saturating_sub(fixed_chrome_rows)
            .saturating_sub(1)
            .max(1);
        let draft_lines = queued_drafts(app, width);
        let composer_lines = composer(app, width, max_composer_rows);
        let body_height = height
            .saturating_sub(fixed_chrome_rows)
            .saturating_sub(draft_lines.len())
            .saturating_sub(composer_lines.len());
        // The sidebar takes a fixed column on the right; the body renders in
        // what remains so its cache keys see one stable width per layout.
        let sidebar_width = if app.sidebar.visible(width) {
            SIDEBAR_WIDTH
        } else {
            0
        };
        let body_width = width.saturating_sub(sidebar_width);
        let mode = app.mode();
        let mut body = match mode {
            Mode::Models | Mode::Themes | Mode::Sessions | Mode::Approval => {
                for cache in self.panes.values_mut() {
                    cache.prune_all();
                }
                match mode {
                    Mode::Models => model_picker(app, body_width, body_height),
                    Mode::Themes => theme_picker(app, body_width, body_height),
                    Mode::Sessions => session_picker(app, body_width, body_height),
                    Mode::Approval | Mode::Compose => approval_prompt(app, body_width, body_height),
                }
            }
            Mode::Compose => {
                let mut body = self.panes_body(app, Rect::new(0, 2, body_width, body_height));
                overlay_slash_autocomplete(
                    &mut body,
                    slash_autocomplete(app, body_width, body_height),
                );
                body
            }
        };
        if sidebar_width > 0 {
            let sidebar = sidebar(app, sidebar_width, body_height);
            body = fit_height(body, body_height);
            for (row, column) in body.iter_mut().zip(sidebar) {
                pad_line(row, body_width);
                for span in column.spans {
                    row.push(span.text, span.style);
                }
                pad_line(row, width);
            }
        }
        lines.extend(body);
        lines.extend(status_lines);
        lines.extend(draft_lines);
        lines.extend(composer_lines);
        lines.push(footer_context(app, width));
        lines.push(footer_workspace(app, width));
        fit_height(lines, height)
    }

    /// Render every visible pane into `area` and compose them row by row,
    /// with dividers between siblings. Each pane renders through its own
    /// cache at its own width, so resizing one split re-lays only the panes
    /// whose width changed.
    fn panes_body(&mut self, app: &mut App, area: Rect) -> Vec<Line> {
        let (tiles, dividers) = app.panes.layout(area);
        let visible: HashSet<PaneId> = tiles.iter().map(|tile| tile.pane).collect();
        // Caches for closed or hidden panes are dropped; a hidden pane pays a
        // one-time relayout when it comes back, which is cheaper than holding
        // every message twice for a pane that may never return.
        self.panes.retain(|id, _| visible.contains(id));
        // One pane filling the area is the common case and the one the speed
        // budgets are written against: its rows are the body, untouched.
        if let [tile] = tiles.as_slice()
            && tile.rect == area
        {
            let tile = *tile;
            let cache = self.panes.entry(tile.pane).or_default();
            return cache.pane(&mut self.highlighter, app, tile, false);
        }
        let multiple = tiles.len() > 1;
        // Every visible piece — pane rows or a divider — in x order. Each row
        // of the canvas is then the pieces covering that row, left to right,
        // so composition is one pass with no scratch tables, and pane rows are
        // moved into place rather than copied.
        let mut pieces: Vec<(Rect, Piece)> = Vec::with_capacity(tiles.len() + dividers.len());
        for tile in tiles {
            let cache = self.panes.entry(tile.pane).or_default();
            let lines = cache.pane(&mut self.highlighter, app, tile, multiple);
            pieces.push((tile.rect, Piece::Pane(lines)));
        }
        for divider in dividers {
            let vertical = divider.width == 1 && divider.height > 1;
            pieces.push((divider, Piece::Divider(vertical)));
        }
        pieces.sort_by_key(|(rect, _)| rect.x);
        let mut canvas: Vec<Line> = Vec::with_capacity(area.height);
        for y in area.y..area.bottom() {
            let mut line = Line::default();
            // Track the column by geometry so a row's width is measured at
            // most once; measuring is the only per-cell work in composition.
            let mut column = area.x;
            for (rect, piece) in &mut pieces {
                if y < rect.y || y >= rect.bottom() {
                    continue;
                }
                if rect.x > column {
                    line.push(" ".repeat(rect.x - column), normal());
                }
                match piece {
                    Piece::Pane(lines) => {
                        let Some(source) = lines.get_mut(y - rect.y) else {
                            column = rect.right();
                            continue;
                        };
                        // A section or status row laid out at full width
                        // must not spill across the divider.
                        let mut used = source.width();
                        if used > rect.width {
                            *source = truncate_line(std::mem::take(source), rect.width);
                            used = rect.width;
                        }
                        line.spans.append(&mut source.spans);
                        column = rect.x + used;
                    }
                    Piece::Divider(vertical) => {
                        let glyph = if *vertical { "│" } else { "─" };
                        line.push(glyph.repeat(rect.width), muted());
                        column = rect.right();
                    }
                }
            }
            canvas.push(line);
        }
        canvas
    }

    /// Install a finished highlight layout in every pane cache holding that
    /// message at that width. Returns whether any frame content changed;
    /// stale results for a message that was re-laid-out or evicted are
    /// dropped.
    pub(crate) fn apply_highlight(&mut self, result: Highlighted) -> bool {
        let mut changed = false;
        for cache in self.panes.values_mut() {
            changed |= cache.apply_highlight(&result);
        }
        changed
    }

    #[cfg(test)]
    fn cache(&mut self, pane: PaneId) -> &mut TranscriptCache {
        self.panes.entry(pane).or_default()
    }

    #[cfg(test)]
    fn markdown(&mut self) -> &HashMap<MessageId, CachedMarkdown> {
        &self.cache(PaneId::default()).markdown
    }

    #[cfg(test)]
    fn render_message(&mut self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        let Self {
            highlighter, panes, ..
        } = self;
        panes
            .entry(PaneId::default())
            .or_default()
            .render_message(highlighter, message, width)
    }

    #[cfg(test)]
    fn transcript<'a>(&'a mut self, app: &App, width: usize) -> VirtualBody<'a> {
        let Self {
            highlighter, panes, ..
        } = self;
        let pane = app.panes.focused_id();
        let viewport = app.panes.focused().viewport.clone();
        panes
            .entry(pane)
            .or_default()
            .transcript(highlighter, app, app.focused(), &viewport, width)
    }

    #[cfg(test)]
    fn fold_focus<'a>(&'a mut self, app: &App, width: usize) -> VirtualBody<'a> {
        let Self {
            highlighter, panes, ..
        } = self;
        let pane = app.panes.focused_id();
        let viewport = app.panes.focused().viewport.clone();
        panes
            .entry(pane)
            .or_default()
            .fold_focus(highlighter, app, app.focused(), &viewport, width)
    }
}

enum Piece {
    Pane(Vec<Line>),
    Divider(bool),
}

impl TranscriptCache {
    /// Render one pane: an optional title row when several panes share the
    /// screen, then the session transcript scrolled to the pane's viewport.
    /// Every row is exactly `tile.rect.width` cells wide.
    fn pane(
        &mut self,
        highlighter: &mut Highlighter,
        app: &mut App,
        tile: Tile,
        titled: bool,
    ) -> Vec<Line> {
        let width = tile.rect.width;
        let session_id = app.panes.get(tile.pane).and_then(|pane| pane.session);
        let focused = app.panes.focused_id() == tile.pane;
        let mut lines = Vec::with_capacity(tile.rect.height);
        if titled {
            lines.push(pane_title(app, session_id, focused, width));
        }
        let body_height = tile.rect.height.saturating_sub(lines.len());
        let viewport = app.viewport(tile.pane).cloned().unwrap_or_default();
        let body = match app.layout {
            Layout::Threadline => self.threadline(highlighter, app, session_id, &viewport, width),
            Layout::FoldFocus => self.fold_focus(highlighter, app, session_id, &viewport, width),
        };
        app.update_viewport(tile.pane, body.rows, body_height, body.preserve_tail_anchor);
        let offset = app.viewport(tile.pane).map_or(0, Viewport::offset);
        let live_message_ranges = body.live_message_ranges.clone();
        let rows = body.viewport(app, body_height, offset);
        drop(body);
        self.live_message_ranges = live_message_ranges.into_iter().collect();
        lines.extend(rows);
        fit_height(lines, tile.rect.height)
    }

    /// Drop every cached layout, keeping live-row anchors: an overlay hides
    /// the transcript but a completion behind it must still preserve the
    /// user's viewport when the transcript returns.
    fn prune_all(&mut self) {
        self.markdown.clear();
        self.live.clear();
    }

    /// Keep only the layouts for messages the pane can show this frame.
    fn prune_markdown(&mut self, app: &App, session_id: Option<SessionId>) {
        let visible = session_id
            .and_then(|session_id| app.sessions.get(&session_id))
            .and_then(|session| {
                session
                    .messages
                    .as_ref()
                    .map(|messages| (session, messages))
            })
            .map(|(session, messages)| {
                let limit = match app.layout {
                    Layout::Threadline => MAX_VISIBLE_MESSAGES,
                    Layout::FoldFocus => 2,
                };
                let mut visible = messages
                    .iter()
                    .rev()
                    .take(limit)
                    .map(|message| message.id)
                    .collect::<HashSet<_>>();
                if app.layout == Layout::FoldFocus
                    && let Some(active_run_id) = session.summary.active_run_id
                    && let Some(message) = messages
                        .iter()
                        .rev()
                        .find(|message| message.run_id == active_run_id)
                {
                    visible.insert(message.id);
                }
                visible
            });
        match visible {
            Some(visible) => {
                self.markdown.retain(|id, _| visible.contains(id));
                self.live.retain(|id, _| visible.contains(id));
                self.live_message_ranges
                    .retain(|id, _| visible.contains(id));
            }
            None => self.prune_all(),
        }
    }

    fn prepare_markdown(
        &mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
        limit: usize,
    ) {
        self.clock += 1;
        self.prune_markdown(app, session_id);
        let Some(session) = session_id.and_then(|session_id| app.sessions.get(&session_id)) else {
            return;
        };
        let Some(messages) = session.messages.as_ref() else {
            return;
        };
        for message in messages.iter().rev().take(limit) {
            if message_is_terminal(message) {
                self.live.remove(&message.id);
                if self
                    .live_message_ranges
                    .remove(&message.id)
                    .is_some_and(|range| viewport.intersects_or_follows(&range))
                {
                    self.preserve_tail_anchor = true;
                }
                self.cache_message(highlighter, message, width, session.loaded_through);
            } else {
                self.markdown.remove(&message.id);
                self.refresh_live(message, width);
            }
        }
        if app.layout == Layout::FoldFocus
            && let Some(active_run_id) = session.summary.active_run_id
            && let Some(message) = messages
                .iter()
                .rev()
                .find(|message| message.run_id == active_run_id)
            && !messages
                .iter()
                .rev()
                .take(limit)
                .any(|visible| visible.id == message.id)
        {
            if message_is_terminal(message) {
                self.live.remove(&message.id);
                if self
                    .live_message_ranges
                    .remove(&message.id)
                    .is_some_and(|range| viewport.intersects_or_follows(&range))
                {
                    self.preserve_tail_anchor = true;
                }
                self.cache_message(highlighter, message, width, session.loaded_through);
            } else {
                self.markdown.remove(&message.id);
                self.refresh_live(message, width);
            }
        }
    }

    /// Extend the settled-prefix layout of a streaming message. Only the bytes
    /// past the previous settled boundary are examined, and only blocks that
    /// became settled since the last frame are laid out.
    fn refresh_live(&mut self, message: &MessageSnapshot, width: usize) {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let entry = self.live.entry(message.id).or_insert(LiveMarkdown {
            width,
            settled_bytes: 0,
            rows: Vec::new(),
        });
        if entry.width != width || entry.settled_bytes > source.len() {
            entry.width = width;
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        // The live view shows at most the last MAX_LIVE_MARKDOWN_BYTES; a
        // settled prefix beyond that is never displayed, so skip ahead rather
        // than lay out rows that would be dropped.
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        if entry.settled_bytes < visible_start {
            entry.settled_bytes = 0;
            entry.rows.clear();
        }
        let scan_from = entry.settled_bytes;
        let text = source.collect_range(scan_from..source.len(), false);
        let settled = settled_prefix_end(&text);
        if settled == 0 {
            return;
        }
        let rows = markdown_lines(&text[..settled], content_width, false);
        entry
            .rows
            .extend(indent_lines(rows, prefix, prefix_style, width));
        entry.settled_bytes = scan_from + settled;
        // Rows past the display bound are never shown again while streaming.
        let excess = entry.rows.len().saturating_sub(MAX_LIVE_MARKDOWN_ROWS);
        if excess > 0 {
            entry.rows.drain(..excess);
        }
    }

    fn cache_message(
        &mut self,
        highlighter: &mut Highlighter,
        message: &MessageSnapshot,
        width: usize,
        loaded_through: u64,
    ) {
        if let Some(cached) = self.markdown.get_mut(&message.id)
            && cached.width == width
            && cached.output_bytes == message.output.len()
            && cached.refusal_bytes == message.refusal.len()
            && cached.loaded_through == loaded_through
        {
            cached.last_used = self.clock;
            // Layout is current; retry a highlight request that was skipped
            // because the highlighter was saturated.
            if !cached.highlight_requested {
                let key = cached.key(message.id);
                cached.highlight_requested = Self::request_highlight(
                    highlighter,
                    key,
                    MessageText::new(message),
                    message.role,
                );
            }
            return;
        }
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        // Plain layout first so the frame never waits on tree-sitter; the
        // highlighted layout replaces it when the blocking job finishes.
        let mut needs_highlight = false;
        let body = if source.len() <= MAX_FULL_MARKDOWN_BYTES {
            let content = source.as_cow();
            let lines = markdown_lines(&content, content_width, false);
            if lines.len() <= MAX_FULL_MARKDOWN_ROWS {
                needs_highlight = has_fenced_code(&content);
                CachedMessageBody::Markdown(indent_lines(lines, prefix, prefix_style, width))
            } else {
                CachedMessageBody::Plain(PlainTextIndex::new(source, content_width))
            }
        } else {
            CachedMessageBody::Plain(PlainTextIndex::new(source, content_width))
        };
        if !self.markdown.contains_key(&message.id)
            && self.markdown.len() >= MAX_VISIBLE_MESSAGES
            && let Some(stale) = self
                .markdown
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(id, _)| *id)
        {
            self.markdown.remove(&stale);
        }
        let cached = CachedMarkdown {
            width,
            output_bytes: message.output.len(),
            refusal_bytes: message.refusal.len(),
            loaded_through,
            body,
            highlight_requested: !needs_highlight,
            last_used: self.clock,
        };
        let key = cached.key(message.id);
        let mut cached = cached;
        if needs_highlight {
            cached.highlight_requested =
                Self::request_highlight(highlighter, key, source, message.role);
        }
        self.markdown.insert(message.id, cached);
    }

    fn request_highlight(
        highlighter: &mut Highlighter,
        key: HighlightKey,
        source: MessageText<'_>,
        role: MessageRole,
    ) -> bool {
        let content = source.as_cow().into_owned();
        let content_width = key.width.saturating_sub(3).max(1);
        let width = key.width;
        let (prefix, prefix_style, _, _) = message_presentation(role);
        highlighter.request(key, move || {
            indent_lines(
                markdown_lines(&content, content_width, true),
                prefix,
                prefix_style,
                width,
            )
        })
    }

    fn apply_highlight(&mut self, result: &Highlighted) -> bool {
        let Some(cached) = self.markdown.get_mut(&result.key.message_id) else {
            return false;
        };
        if cached.key(result.key.message_id) != result.key {
            return false;
        }
        match &mut cached.body {
            CachedMessageBody::Markdown(lines) => {
                lines.clone_from(&result.lines);
                true
            }
            CachedMessageBody::Plain(_) => false,
        }
    }

    fn threadline<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
    ) -> VirtualBody<'a> {
        let transcript = self.transcript(highlighter, app, session_id, viewport, width);
        let mut body = VirtualBody::default();
        body.extend_owned(vec![
            section(
                "THREADLINE",
                "conversation with child work in one chronology",
            ),
            Line::default(),
        ]);
        body.extend_virtual(transcript);
        if let Some(focused) = session_id {
            // Children already shown under their spawn call are not repeated.
            let children: Vec<SessionId> = app
                .children_of(focused)
                .into_iter()
                .filter(|child| {
                    app.sessions[child]
                        .summary
                        .spawned_by
                        .and_then(|origin| origin.tool_call_id)
                        .is_none_or(|call| app.child_spawned_by(call) != Some(*child))
                })
                .collect();
            if !children.is_empty() {
                body.push_line(Line::default());
                body.push_line(Line::styled("  +-- related sessions", muted().bold()));
                for child in children {
                    body.push_line(session_line(app, child, width, "     "));
                }
            }
        }
        body
    }

    fn fold_focus<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
    ) -> VirtualBody<'a> {
        let content_width = width.min(96);
        self.prepare_markdown(highlighter, app, session_id, viewport, content_width, 2);
        let mut body = VirtualBody {
            preserve_tail_anchor: std::mem::take(&mut self.preserve_tail_anchor),
            ..VirtualBody::default()
        };
        body.extend_owned(vec![
            section(
                "FOLD / FOCUS",
                "history and parallel work compressed around now",
            ),
            Line::default(),
        ]);
        let Some(session_id) = session_id else {
            body.push_line(Line::styled("  Alt-N creates the first session.", muted()));
            return body;
        };
        let Some(session) = app.sessions.get(&session_id) else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let Some(messages) = session.messages.as_ref() else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let start = messages.len().saturating_sub(2);
        let mut selected = (start..messages.len()).collect::<Vec<_>>();
        if let Some(active_run_id) = session.summary.active_run_id
            && let Some(active_index) = messages
                .iter()
                .rposition(|message| message.run_id == active_run_id)
            && selected.binary_search(&active_index).is_err()
        {
            selected.push(active_index);
            selected.sort_unstable();
        }
        let folded = messages.len().saturating_sub(selected.len());
        if folded > 0 {
            body.push_line(Line::styled(
                format!("  > {folded} messages folded"),
                accent(),
            ));
            body.push_line(Line::default());
        }
        let mut focused = VirtualBody::default();
        self.append_message_indices(
            &mut focused,
            app,
            messages,
            session.tool_calls.as_deref().unwrap_or_default(),
            selected,
            content_width,
        );
        body.extend_virtual(focused);
        for prompt in app.pending_prompts(session_id) {
            let mut line = Line::styled("  YOU / PENDING  ", warning().bold());
            line.push(
                preview(prompt, content_width.saturating_sub(18)),
                muted().italic(),
            );
            body.push_line(line);
        }
        for child in app.children_of(session_id) {
            body.push_line(session_line(app, child, content_width, "  > "));
        }
        body
    }

    fn transcript<'a>(
        &'a mut self,
        highlighter: &mut Highlighter,
        app: &App,
        session_id: Option<SessionId>,
        viewport: &Viewport,
        width: usize,
    ) -> VirtualBody<'a> {
        self.prepare_markdown(
            highlighter,
            app,
            session_id,
            viewport,
            width,
            MAX_VISIBLE_MESSAGES,
        );
        let mut body = VirtualBody {
            preserve_tail_anchor: std::mem::take(&mut self.preserve_tail_anchor),
            ..VirtualBody::default()
        };
        let Some(session_id) = session_id else {
            body.push_line(Line::styled("  Alt-N creates the first session.", muted()));
            return body;
        };
        let Some(session) = app.sessions.get(&session_id) else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let Some(messages) = session.messages.as_ref() else {
            body.push_line(Line::styled(
                "  Loading session history...",
                muted().italic(),
            ));
            return body;
        };
        let hidden = messages.len().saturating_sub(MAX_VISIBLE_MESSAGES);
        if hidden > 0 {
            body.push_line(Line::styled(
                format!("  {hidden} earlier messages outside the viewport"),
                muted(),
            ));
        }
        self.append_messages(
            &mut body,
            app,
            messages,
            session.tool_calls.as_deref().unwrap_or_default(),
            hidden,
            width,
        );
        for prompt in app.pending_prompts(session_id) {
            // A pending prompt is a YOU boundary: the same two blank lines
            // that precede any user turn.
            if !body.is_empty() {
                body.push_line(Line::default());
                body.push_line(Line::default());
            }
            let mut line = Line::styled(" ▌ ", warning());
            line.push("YOU  pending", warning().bold());
            body.push_line(line);
            body.extend_owned(indent_lines(
                pending_markdown_lines(prompt, width.saturating_sub(3)),
                " ▌ ",
                warning(),
                width,
            ));
        }
        if body.is_empty() {
            body.push_line(Line::styled(
                "  Ask QQ to begin this session.",
                muted().italic(),
            ));
        }
        body
    }

    fn append_messages<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        app: &App,
        messages: &[MessageSnapshot],
        tool_calls: &[ToolCallSnapshot],
        start: usize,
        width: usize,
    ) {
        self.append_message_indices(
            body,
            app,
            messages,
            tool_calls,
            start..messages.len(),
            width,
        );
    }

    fn append_message_indices<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        app: &App,
        messages: &[MessageSnapshot],
        tool_calls: &[ToolCallSnapshot],
        indices: impl IntoIterator<Item = usize>,
        width: usize,
    ) {
        for index in indices {
            let message = &messages[index];
            if !body.is_empty() {
                body.push_line(Line::default());
                // A user prompt starts a new turn; extra spacing keeps
                // prompt/response boundaries scannable.
                if message.role == MessageRole::User {
                    body.push_line(Line::default());
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
                    body.extend_owned(render_tool_calls(
                        &run_calls[..head],
                        &app.live_tool_output,
                        app.tool_detail,
                        app.animation_tick,
                        width,
                        &|call_id, width| child_rows(app, call_id, width),
                    ));
                    body.push_line(Line::default());
                }
                if first_of_run {
                    let rows = reasoning_rows(app, message.session_id, message.run_id, width);
                    if !rows.is_empty() {
                        body.extend_owned(rows);
                        body.push_line(Line::default());
                    }
                }
                self.append_message(body, message, width);
                if run_calls.len() > head {
                    body.push_line(Line::default());
                    body.extend_owned(render_tool_calls(
                        &run_calls[head..],
                        &app.live_tool_output,
                        app.tool_detail,
                        app.animation_tick,
                        width,
                        &|call_id, width| child_rows(app, call_id, width),
                    ));
                }
            } else {
                self.append_message(body, message, width);
                let has_assistant_message = messages.iter().any(|candidate| {
                    candidate.role == MessageRole::Assistant && candidate.run_id == message.run_id
                });
                if !has_assistant_message {
                    let mut orphan_calls = tool_calls
                        .iter()
                        .filter(|tool_call| tool_call.run_id == message.run_id)
                        .collect::<Vec<_>>();
                    orphan_calls
                        .sort_by_key(|tool_call| (tool_call.turn_ordinal, tool_call.call_ordinal));
                    if !orphan_calls.is_empty() {
                        body.push_line(Line::default());
                        body.extend_owned(render_tool_calls(
                            &orphan_calls,
                            &app.live_tool_output,
                            app.tool_detail,
                            app.animation_tick,
                            width,
                            &|call_id, width| child_rows(app, call_id, width),
                        ));
                    }
                }
            }
        }
    }

    fn append_message<'a>(
        &'a self,
        body: &mut VirtualBody<'a>,
        message: &MessageSnapshot,
        width: usize,
    ) {
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        body.push_line(truncate_line(message_header(message), width));
        let content_start = body.rows;
        if message_is_terminal(message) {
            let Some(cached) = self.markdown.get(&message.id) else {
                // `prepare_markdown` caches every visible terminal message;
                // a miss means the cache was evicted under memory pressure
                // this frame. Show the header and recover next frame.
                body.push_line(message_ellipsis(prefix, prefix_style));
                return;
            };
            match &cached.body {
                CachedMessageBody::Markdown(lines) => {
                    if lines.is_empty() {
                        body.push_line(message_ellipsis(prefix, prefix_style));
                    } else {
                        body.extend_cached(lines);
                    }
                }
                CachedMessageBody::Plain(index) => {
                    if index.rows == 0 {
                        body.push_line(message_ellipsis(prefix, prefix_style));
                    } else {
                        body.extend_plain(index, message.id, prefix, prefix_style, width);
                    }
                }
            }
        } else {
            // Still streaming: the settled prefix comes from the live cache and
            // only the open trailing block is laid out this frame. Tree-sitter
            // stays off so per-frame work is bounded by one block, not the
            // message. Any hidden live prefix becomes reachable through the
            // completed-message cache once the message settles.
            let lines = self.live_lines(message, width);
            if lines.is_empty() {
                body.push_line(message_ellipsis(prefix, prefix_style));
            } else {
                body.extend_owned(lines);
            }
            body.live_message_ranges
                .push((message.id, content_start..body.rows));
        }
    }

    /// Rows for a streaming message: cached settled rows followed by the
    /// freshly laid-out open tail, bounded to the live display budget with a
    /// marker when earlier rows were dropped.
    fn live_lines(&self, message: &MessageSnapshot, width: usize) -> Vec<Line> {
        let source = MessageText::new(message);
        let content_width = width.saturating_sub(3).max(1);
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let (settled_bytes, settled_rows) = match self.live.get(&message.id) {
            Some(live) if live.width == width && live.settled_bytes <= source.len() => {
                (live.settled_bytes, live.rows.as_slice())
            }
            Some(_) | None => (0, &[][..]),
        };
        let visible_start = source.len().saturating_sub(MAX_LIVE_MARKDOWN_BYTES);
        let tail_start = settled_bytes.max(visible_start);
        let tail = if tail_start == settled_bytes {
            source.collect_range(tail_start..source.len(), false)
        } else {
            source.bounded_tail(MAX_LIVE_MARKDOWN_BYTES).into_owned()
        };
        let tail_rows = indent_lines(
            markdown_lines(&tail, content_width, false),
            prefix,
            prefix_style,
            width,
        );
        let total = settled_rows.len() + tail_rows.len();
        let truncated = tail_start > settled_bytes || total > MAX_LIVE_MARKDOWN_ROWS;
        let budget = MAX_LIVE_MARKDOWN_ROWS.saturating_sub(usize::from(truncated));
        let mut lines = Vec::with_capacity(total.min(budget) + 1);
        if truncated {
            lines.push(truncate_line(
                Line::styled(
                    "... earlier output remains available when this message completes",
                    muted().italic(),
                ),
                width,
            ));
        }
        let drop = total.saturating_sub(budget);
        let drop_settled = drop.min(settled_rows.len());
        lines.extend_from_slice(&settled_rows[drop_settled..]);
        lines.extend(tail_rows.into_iter().skip(drop - drop_settled));
        lines
    }

    #[cfg(test)]
    #[cfg(test)]
    fn render_message(
        &mut self,
        highlighter: &mut Highlighter,
        message: &MessageSnapshot,
        width: usize,
    ) -> Vec<Line> {
        if message_is_terminal(message) {
            self.live.remove(&message.id);
            self.cache_message(highlighter, message, width, 0);
        } else {
            self.refresh_live(message, width);
        }
        let (prefix, prefix_style, _, _) = message_presentation(message.role);
        let mut lines = vec![truncate_line(message_header(message), width)];
        if message_is_terminal(message) {
            match &self.markdown.get(&message.id).expect("message cached").body {
                CachedMessageBody::Markdown(body) => lines.extend_from_slice(body),
                CachedMessageBody::Plain(index) => lines.extend(index.render(
                    MessageText::new(message),
                    0..index.rows,
                    prefix,
                    prefix_style,
                    width,
                )),
            }
        } else {
            lines.extend(self.live_lines(message, width));
        }
        if lines.len() == 1 {
            lines.push(message_ellipsis(prefix, prefix_style));
        }
        lines
    }
}

/// Renders one run's tool calls: a folded count for quiet runs, otherwise one
/// gutter line per call, with errors and the expanded detail level adding
/// bounded body rows. Running calls with buffered live output show a bounded
/// tail of it at every detail level — a running command's output is the thing
/// the user is waiting for.
/// Rows rendered beneath a tool call that spawned a child session.
type ChildRows<'a> = &'a dyn Fn(ToolCallId, usize) -> Vec<Line>;

fn render_tool_calls(
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

fn find_message(app: &App, message_id: MessageId) -> Option<&MessageSnapshot> {
    app.sessions
        .values()
        .filter_map(|session| session.messages.as_ref())
        .flatten()
        .find(|message| message.id == message_id)
}

const fn message_is_terminal(message: &MessageSnapshot) -> bool {
    matches!(
        message.state,
        MessageState::Complete
            | MessageState::Cancelled
            | MessageState::Failed
            | MessageState::Interrupted
    )
}

fn message_presentation(role: MessageRole) -> (&'static str, Style, &'static str, Style) {
    match role {
        MessageRole::User => (" ▌ ", accent(), "YOU", accent().bold()),
        MessageRole::Assistant => ("   ", muted(), "QQ", normal().bold()),
    }
}

fn message_ellipsis(prefix: &'static str, prefix_style: Style) -> Line {
    let mut ellipsis = Line::styled(prefix, prefix_style);
    ellipsis.push("...", muted());
    ellipsis
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

fn status_notice(app: &App, width: usize) -> Vec<Line> {
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

fn session_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
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
        let depth = app.depth(session_id);
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
fn search_line(query: &str, placeholder: &str) -> Line {
    Line::styled(
        format!(
            "  search: {}",
            if query.is_empty() { placeholder } else { query }
        ),
        if query.is_empty() { muted() } else { accent() },
    )
}

fn model_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
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
fn theme_picker(app: &App, width: usize, height: usize) -> Vec<Line> {
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

fn approval_prompt(app: &App, width: usize, height: usize) -> Vec<Line> {
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

fn session_line(app: &App, session_id: SessionId, width: usize, prefix: &str) -> Line {
    let session = &app.sessions[&session_id].summary;
    let (marker, style) = match session.status {
        SessionStatus::Idle => match session.last_outcome.as_ref() {
            Some(qq_protocol::RunOutcome::Completed) => (".", accent()),
            Some(qq_protocol::RunOutcome::Cancelled) => ("x", warning()),
            Some(qq_protocol::RunOutcome::Interrupted) => ("!", warning()),
            Some(qq_protocol::RunOutcome::BudgetExhausted { .. }) => ("$", warning()),
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
fn pane_title(app: &App, session_id: Option<SessionId>, focused: bool, width: usize) -> Line {
    let (marker, marker_style) = if focused {
        ("▎", accent())
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
fn sidebar(app: &App, width: usize, height: usize) -> Vec<Line> {
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
    let order = app.thread_order();
    let mut rows: Vec<Line> = Vec::new();
    let mut focused_row = 0;
    for session_id in order {
        let depth = app.depth(session_id);
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
fn child_rows(app: &App, tool_call_id: ToolCallId, width: usize) -> Vec<Line> {
    let Some(child) = app.child_spawned_by(tool_call_id) else {
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
fn live_status_line(app: &App, session_id: SessionId) -> Option<(String, Style)> {
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
fn pad_line(line: &mut Line, width: usize) {
    let used = line.width();
    if used < width {
        line.push(" ".repeat(width - used), normal());
    }
}

/// Rows for a run's provider-exposed reasoning. Collapsed: one line with the
/// state and length, or the first sentence when there is room. Expanded: the
/// bounded text laid out as plain prose under a dimmed rail. Empty when the
/// run produced no reasoning.
fn reasoning_rows(app: &App, session_id: SessionId, run_id: RunId, width: usize) -> Vec<Line> {
    let Some(reasoning) = app
        .sessions
        .get(&session_id)
        .and_then(|session| session.reasoning.get(&run_id))
    else {
        return Vec::new();
    };
    if reasoning.text.is_empty() && !reasoning.streaming {
        return Vec::new();
    }
    let seconds = reasoning.ticks / 8;
    let mut header = Line::styled(" ∴ ", muted());
    if reasoning.streaming {
        header.push(
            format!(
                "{} thinking… {seconds}s",
                TOOL_SPINNER[app.animation_tick % TOOL_SPINNER.len()]
            ),
            muted().italic(),
        );
    } else {
        header.push(format!("thought for {seconds}s"), muted().italic());
    }
    match app.reasoning_detail {
        crate::app::ReasoningDetail::Collapsed => {
            // First paragraph only: the collapsed row is a glance, not the text.
            let first = reasoning.text.split("\n\n").next().unwrap_or_default();
            let summary = preview(first, width.saturating_sub(header.width() + 12));
            if !summary.is_empty() {
                header.push(format!("  {summary}"), muted());
            }
            header.push("  Ctrl-R", muted().dim());
            vec![truncate_line(header, width)]
        }
        crate::app::ReasoningDetail::Expanded => {
            let mut rows = vec![truncate_line(header, width)];
            let content_width = width.saturating_sub(3).max(1);
            for paragraph in reasoning.text.split("\n\n") {
                for line in paragraph.lines() {
                    let safe = line
                        .chars()
                        .filter_map(terminal_safe_character)
                        .collect::<String>();
                    for wrapped in wrap_line(Line::styled(safe, muted().italic()), content_width) {
                        let mut row = Line::styled(" ┆ ", muted().dim());
                        for span in wrapped.spans {
                            row.push(span.text, span.style);
                        }
                        rows.push(row);
                    }
                }
            }
            rows
        }
    }
}

/// Drafts held locally while the focused session runs, oldest first. Each
/// takes one row; the newest is the one Alt-Up brings back.
fn queued_drafts(app: &App, width: usize) -> Vec<Line> {
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
fn composer_row(part: &str) -> Line {
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

fn composer(app: &App, width: usize, max_rows: usize) -> Vec<Line> {
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

fn footer_context(app: &App, width: usize) -> Line {
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

fn footer_workspace(app: &App, width: usize) -> Line {
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

fn slash_autocomplete(app: &App, width: usize, height: usize) -> Vec<Line> {
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

/// The `▌ YOU  streaming` style row that opens a message. Steering rows keep
/// the user prefix but say what they are: injected mid-run, not a new prompt,
/// with a lifecycle (waiting for a boundary, applied, superseded) in words the
/// run's own messages never use.
fn message_header(message: &MessageSnapshot) -> Line {
    let (prefix, prefix_style, role, role_style) = message_presentation(message.role);
    let mut header = Line::styled(prefix, prefix_style);
    header.push(role, role_style);
    if message.steering {
        let (label, style) = match message.state {
            MessageState::Queued => ("steering  waiting for the next turn", warning()),
            MessageState::Complete => ("steered", muted()),
            MessageState::Cancelled => ("steering  run finished first", warning()),
            MessageState::Streaming | MessageState::Failed | MessageState::Interrupted => (
                message_state_label(message.state),
                status_style(message.state),
            ),
        };
        header.push(format!("  {label}"), style);
    } else if !matches!(message.state, MessageState::Complete) {
        header.push(
            format!("  {}", message_state_label(message.state)),
            status_style(message.state),
        );
    }
    header
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
fn pending_markdown_lines(source: &str, width: usize) -> Vec<Line> {
    let source_was_truncated = source.len() > MAX_LIVE_MARKDOWN_BYTES;
    let mut lines = markdown_lines(bounded_tail(source, MAX_LIVE_MARKDOWN_BYTES), width, false);
    let reserved_marker = usize::from(source_was_truncated || lines.len() > MAX_LIVE_MARKDOWN_ROWS);
    let excess = lines
        .len()
        .saturating_sub(MAX_LIVE_MARKDOWN_ROWS.saturating_sub(reserved_marker));
    if excess > 0 {
        lines.drain(..excess);
    }
    if reserved_marker > 0 {
        lines.insert(
            0,
            truncate_line(
                Line::styled("... earlier pending prompt omitted", muted().italic()),
                width,
            ),
        );
    }
    lines
}

fn next_plain_text_row(
    source: MessageText<'_>,
    start: usize,
    width: usize,
) -> Option<(Range<usize>, usize)> {
    if start >= source.len() {
        return None;
    }
    let width = width.max(1);
    let mut used = 0_usize;
    let mut byte = start;
    while let Some((character, next)) = source.next_char(byte) {
        if byte > start && next.saturating_sub(start) > MAX_PLAIN_TEXT_ROW_BYTES {
            return Some((start..byte, byte));
        }
        if character == '\n' {
            return Some((start..byte, next));
        }
        let character_width = terminal_safe_character(character)
            .and_then(UnicodeWidthChar::width)
            .unwrap_or_default();
        if used > 0 && used.saturating_add(character_width) > width {
            return Some((start..byte, byte));
        }
        used = used.saturating_add(character_width);
        byte = next;
    }
    Some((start..source.len(), source.len()))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
    use qq_protocol::{
        AccountingTotal, EventCursor, ModelSelection, RunId, SessionAccounting, SessionEvent,
        SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SessionSummary, StoreId,
        WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
    };

    use super::*;
    use crate::{
        ClientUpdate, ModelOption, TuiOptions,
        commands::Command,
        render::{code_keyword, success, surface, surface_color},
        theme::Palette,
        view::markdown::{code_panel_row, tests::style_of},
    };

    fn completed_message(byte: u8, output: String) -> MessageSnapshot {
        MessageSnapshot {
            id: MessageId::from_bytes([byte; 16]),
            session_id: SessionId::from_bytes([1; 16]),
            run_id: RunId::from_bytes([2; 16]),
            turn_ordinal: 0,
            role: MessageRole::Assistant,
            state: MessageState::Complete,
            steering: false,
            output,
            refusal: String::new(),
            created_at_ms: 1,
        }
    }

    fn app_with_messages(count: u8) -> App {
        let workspace_id = WorkspaceId::from_bytes([3; 16]);
        let session_id = SessionId::from_bytes([1; 16]);
        let summary = SessionSummary {
            activity: None,
            spawned_by: None,
            id: session_id,
            workspace_id,
            parent_id: None,
            title: "Session".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            profile: qq_protocol::AgentProfileId::default(),
            correlation: qq_protocol::Correlation::default(),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 1,
            last_outcome: None,
        };
        let mut app = App::new(TuiOptions::default());
        app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
            included: Vec::new(),
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

    fn transcript_lines(app: &App, width: usize) -> Vec<Line> {
        let mut renderer = FrameRenderer::default();
        let body = renderer.transcript(app, width);
        body.viewport(app, body.rows, 0)
    }

    fn fold_focus_lines(app: &App, width: usize) -> Vec<Line> {
        let mut renderer = FrameRenderer::default();
        let body = renderer.fold_focus(app, width);
        body.viewport(app, body.rows, 0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn completed_messages_render_plain_then_upgrade_to_highlighted() {
        let mut renderer = FrameRenderer::default();
        let mut message = completed_message(1, "```rust\nlet x = 1;\n```".to_owned());
        message.state = MessageState::Streaming;

        let streaming = renderer.render_message(&message, 40);

        // Re-rendered every frame while streaming: plain panel, no cache.
        assert_eq!(style_of(&streaming, "let"), Some(surface(normal())));
        assert!(renderer.markdown().is_empty());
        assert_eq!(renderer.highlighter.in_flight(), 0);

        // Completion caches a plain layout immediately and schedules
        // highlighting off the render path.
        message.state = MessageState::Complete;
        let complete = renderer.render_message(&message, 40);
        assert_eq!(style_of(&complete, "let"), Some(surface(normal())));
        assert!(renderer.markdown().contains_key(&message.id));
        assert_eq!(renderer.highlighter.in_flight(), 1);

        let highlighted = renderer.highlighter.next().await;
        assert!(renderer.apply_highlight(highlighted));
        let upgraded = renderer.render_message(&message, 40);
        assert_eq!(style_of(&upgraded, "let"), Some(surface(code_keyword())));

        // A stale result (different width) is dropped, not installed.
        let stale = Highlighted {
            key: HighlightKey {
                message_id: message.id,
                width: 41,
                output_bytes: message.output.len(),
                refusal_bytes: 0,
                loaded_through: 0,
            },
            lines: Vec::new(),
        };
        assert!(!renderer.apply_highlight(stale));
        assert_eq!(
            style_of(&renderer.render_message(&message, 40), "let"),
            Some(surface(code_keyword()))
        );
    }

    #[test]
    fn prose_only_messages_do_not_request_highlighting() {
        let mut renderer = FrameRenderer::default();
        let message = completed_message(1, "plain **prose** without code".to_owned());
        renderer.render_message(&message, 40);
        assert_eq!(renderer.highlighter.in_flight(), 0);
    }

    #[test]
    fn live_message_rendering_is_bounded_without_hiding_completed_output() {
        let mut renderer = FrameRenderer::default();
        let mut message = completed_message(
            1,
            format!(
                "BEGIN-LIVE-MESSAGE\n{}\nEND-LIVE-MESSAGE",
                "streaming row\n".repeat(MAX_LIVE_MARKDOWN_ROWS * 4)
            ),
        );
        message.state = MessageState::Streaming;

        let live = renderer.render_message(&message, 40);
        let live_text = frame_text(&live);

        assert!(live.len() <= MAX_LIVE_MARKDOWN_ROWS + 1);
        assert!(live_text.contains("earlier output remains"));
        assert!(!live_text.contains("BEGIN-LIVE-MESSAGE"));
        assert!(live_text.contains("END-LIVE-MESSAGE"));

        message.state = MessageState::Complete;
        let complete = renderer.render_message(&message, 40);
        let complete_text = frame_text(&complete);
        assert!(complete_text.contains("BEGIN-LIVE-MESSAGE"));
        assert!(complete_text.contains("END-LIVE-MESSAGE"));
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
        let session_id = app.focused().unwrap();
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
    fn call_only_run_renders_before_its_first_assistant_message() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[0].role = MessageRole::User;
        session.tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"note.txt"}"#,
            ToolCallState::Running,
            None,
            false,
        )]);

        let rows = frame_rows(&transcript_lines(&app, 100));

        assert!(
            rows.iter()
                .any(|row| row.contains("read_file note.txt") && row.contains("running"))
        );
    }

    #[test]
    fn fold_focus_renders_the_current_runs_tool_activity() {
        let mut app = app_with_messages(1);
        app.layout = Layout::FoldFocus;
        let session_id = app.focused().unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        session.summary.status = SessionStatus::Running;
        session.summary.active_run_id = Some(RunId::from_bytes([2; 16]));
        session.tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"note.txt"}"#,
            ToolCallState::Running,
            None,
            false,
        )]);

        let rows = frame_rows(&fold_focus_lines(&app, 100));

        assert!(
            rows.iter()
                .any(|row| row.contains("read_file note.txt") && row.contains("running"))
        );
    }

    #[test]
    fn steering_rows_say_what_they_are_at_every_state() {
        let mut app = app_with_messages(4);
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.status = SessionStatus::Running;
        let active_run_id = RunId::from_bytes([2; 16]);
        session.summary.active_run_id = Some(active_run_id);
        let messages = session.messages.as_mut().unwrap();
        for (index, (state, text)) in [
            (MessageState::Complete, "the model turn"),
            (MessageState::Queued, "pending steer"),
            (MessageState::Complete, "applied steer"),
            (MessageState::Cancelled, "late steer"),
        ]
        .into_iter()
        .enumerate()
        {
            messages[index].run_id = active_run_id;
            messages[index].state = state;
            messages[index].output = text.to_owned();
            if index > 0 {
                messages[index].role = MessageRole::User;
                messages[index].steering = true;
            }
        }

        let frame = FrameRenderer::default().frame(&mut app, 100, 40);
        let rows = frame_rows(&frame);
        let header_before = |needle: &str| {
            let at = rows.iter().position(|row| row.contains(needle)).unwrap();
            rows[..at]
                .iter()
                .rev()
                .find(|row| row.contains("YOU"))
                .cloned()
                .unwrap()
        };
        assert!(header_before("pending steer").contains("steering  waiting for the next turn"));
        assert!(
            header_before("applied steer")
                .trim_end()
                .ends_with("steered")
        );
        assert!(header_before("late steer").contains("steering  run finished first"));
        // A steering row never shows the plain "queued" of a queued prompt,
        // which would read as a new run waiting its turn.
        assert!(!rows.iter().any(|row| row.contains("YOU  queued")));
    }

    #[test]
    fn fold_focus_keeps_active_work_visible_ahead_of_queued_prompts() {
        let mut app = app_with_messages(4);
        app.layout = Layout::FoldFocus;
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.status = SessionStatus::Running;
        let active_run_id = RunId::from_bytes([2; 16]);
        session.summary.active_run_id = Some(active_run_id);
        let messages = session.messages.as_mut().unwrap();
        messages[0].run_id = RunId::from_bytes([9; 16]);
        messages[0].output = "folded history".to_owned();
        messages[1].run_id = active_run_id;
        messages[1].output = "active model turn".to_owned();
        messages[2].run_id = RunId::from_bytes([3; 16]);
        messages[2].role = MessageRole::User;
        messages[2].state = MessageState::Queued;
        messages[2].output = "queued prompt one".to_owned();
        messages[3].run_id = RunId::from_bytes([4; 16]);
        messages[3].role = MessageRole::User;
        messages[3].state = MessageState::Queued;
        messages[3].output = "queued prompt two".to_owned();
        session.tool_calls = Some(vec![tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"active.rs"}"#,
            ToolCallState::Running,
            None,
            false,
        )]);

        let rows = frame_rows(&fold_focus_lines(&app, 100));
        let text = rows.join("\n");

        assert!(text.contains("active model turn"));
        assert!(
            rows.iter()
                .any(|row| row.contains("read_file active.rs") && row.contains("running"))
        );
        assert!(text.contains("queued prompt one"));
        assert!(text.contains("queued prompt two"));
        assert!(!text.contains("folded history"));
    }

    #[test]
    fn fold_focus_keeps_tool_calls_between_their_model_turns() {
        let mut app = app_with_messages(3);
        app.layout = Layout::FoldFocus;
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        let messages = session.messages.as_mut().unwrap();
        messages[0].role = MessageRole::User;
        messages[1].turn_ordinal = 1;
        messages[2].turn_ordinal = 2;
        let mut first = tool_call_snapshot(
            7,
            "read_file",
            r#"{"path":"first.rs"}"#,
            ToolCallState::Completed,
            Some("first\n"),
            false,
        );
        first.turn_ordinal = 1;
        let mut second = tool_call_snapshot(
            8,
            "read_file",
            r#"{"path":"second.rs"}"#,
            ToolCallState::Completed,
            Some("second\n"),
            false,
        );
        second.turn_ordinal = 2;
        session.tool_calls = Some(vec![second, first]);

        let rows = frame_rows(&fold_focus_lines(&app, 100));
        let position = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();

        assert!(position("row 1") < position("first.rs"));
        assert!(position("first.rs") < position("row 2"));
        assert!(position("row 2") < position("second.rs"));
    }

    #[test]
    fn transcript_spacing_separates_blocks_and_doubles_before_prompts() {
        let mut app = app_with_messages(3);
        let session_id = app.focused().unwrap();
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

        let rows = frame_rows(&transcript_lines(&app, 80));

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
        let session_id = app.focused().unwrap();
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

        let rows = frame_rows(&transcript_lines(&app, 80));

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
        let session_id = app.focused().unwrap();
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

        let rows = frame_rows(&transcript_lines(&app, 80));

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
        let lines = render_tool_calls(
            &[&diff_call],
            &HashMap::new(),
            ToolDetail::Expanded,
            0,
            80,
            &|_, _| Vec::new(),
        );
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
            &|_, _| Vec::new(),
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

        let lines = render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Expanded,
            0,
            80,
            &|_, _| Vec::new(),
        );
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
        let lines = render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            80,
            &|_, _| Vec::new(),
        );
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
            let rows = frame_rows(&render_tool_calls(
                &[&call],
                &live,
                detail,
                0,
                80,
                &|_, _| Vec::new(),
            ));
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
        let session_id = app.focused().unwrap();
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
        // The modal offers all four decisions, including workspace lifetime.
        assert!(rows.iter().any(|row| {
            row.contains("[y] approve once")
                && row.contains("[a] for session")
                && row.contains("[w] for workspace")
                && row.contains("[n]/[Esc] deny")
        }));
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
                &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
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
            &|_, _| Vec::new(),
        ));
        assert!(rows.len() > 6);
    }

    #[test]
    fn detail_cycling_reveals_arguments_and_result_tails() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
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
                let lines =
                    render_tool_calls(&references, &HashMap::new(), detail, 0, width, &|_, _| {
                        Vec::new()
                    });
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
    fn panel_rows_carry_the_surface_background_through_output() {
        let row = code_panel_row(Line::styled("x", normal()), 8);
        assert!(
            row.spans
                .iter()
                .all(|span| span.style.background == Some(surface_color()))
        );
        let mut rendered = Vec::new();

        write_line(&mut rendered, &row).unwrap();

        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains('x'));
        if std::env::var_os("NO_COLOR").is_none() {
            assert!(rendered.contains("\u{1b}[48;2;38;40;48m"));
        } else {
            assert!(!rendered.contains("\u{1b}[48;2;38;40;48m"));
        }
    }

    #[test]
    fn completed_markdown_cache_is_bounded_and_keeps_one_width() {
        let mut renderer = FrameRenderer::default();
        let message = completed_message(1, "hello".to_owned());
        renderer.render_message(&message, 40);
        renderer.render_message(&message, 80);
        assert_eq!(renderer.markdown().len(), 1);
        assert_eq!(renderer.markdown()[&message.id].width, 80);

        for byte in 2..=u8::try_from(MAX_VISIBLE_MESSAGES + 8).unwrap() {
            renderer.render_message(&completed_message(byte, byte.to_string()), 80);
        }
        assert!(renderer.markdown().len() <= MAX_VISIBLE_MESSAGES);
    }

    #[test]
    fn authoritative_snapshot_generation_invalidates_same_length_cached_output() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
        app.sessions
            .get_mut(&session_id)
            .unwrap()
            .messages
            .as_mut()
            .unwrap()[0]
            .output = "old".to_owned();
        let mut renderer = FrameRenderer::default();
        let initial = renderer.transcript(&app, 80);
        assert!(frame_text(&initial.viewport(&app, initial.rows, 0)).contains("old"));
        drop(initial);

        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[0].output = "new".to_owned();
        session.loaded_through += 1;
        let refreshed = renderer.transcript(&app, 80);
        let text = frame_text(&refreshed.viewport(&app, refreshed.rows, 0));

        assert!(text.contains("new"));
        assert!(!text.contains("old"));
    }

    #[test]
    fn completed_markdown_preserves_the_beginning_and_end_of_long_messages() {
        let mut renderer = FrameRenderer::default();
        let output = (1..=10)
            .map(|phase| {
                format!(
                    "## Phase {phase}\n{}{}\n",
                    if phase == 1 {
                        "BEGIN-FIRST-PHASE\n"
                    } else {
                        ""
                    },
                    (0..80)
                        .map(|step| format!(
                            "- phase {phase} step {step}: verify the complete output"
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .collect::<String>()
            + "\nEND-FINAL-PHASE";
        let message = completed_message(1, output);
        let rendered = renderer.render_message(&message, 80);

        let text = rendered
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(text.contains("BEGIN-FIRST-PHASE"));
        assert!(text.contains("phase 5 step 40"));
        assert!(text.contains("END-FINAL-PHASE"));
    }

    #[test]
    fn oversized_completed_messages_use_a_sparse_full_history_index() {
        let mut app = app_with_messages(1);
        let message = &mut app
            .sessions
            .get_mut(&app.focused().unwrap())
            .unwrap()
            .messages
            .as_mut()
            .unwrap()[0];
        message.output = std::iter::once("BEGIN-SPARSE".to_owned())
            .chain((0..12_000).map(|row| format!("ROW-{row:05} 😀")))
            .chain(std::iter::once("END-SPARSE".to_owned()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(message.output.len() > MAX_FULL_MARKDOWN_BYTES);

        let mut renderer = FrameRenderer::default();
        let body = renderer.transcript(&app, 80);
        let (index, message_id, prefix, prefix_style, width) = body
            .segments
            .iter()
            .find_map(|segment| match segment {
                BodySegment::Plain {
                    index,
                    message_id,
                    prefix,
                    prefix_style,
                    width,
                } => Some((*index, *message_id, *prefix, *prefix_style, *width)),
                BodySegment::Owned(_) | BodySegment::Cached(_) => None,
            })
            .expect("oversized message uses sparse rendering");
        assert!(index.checkpoints.len() <= MAX_PLAIN_TEXT_CHECKPOINTS + 1);

        let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
        let tail = frame_text(&body.viewport(&app, 20, 0));
        assert!(top.contains("BEGIN-SPARSE"));
        assert!(tail.contains("END-SPARSE"));

        let source = MessageText::new(find_message(&app, message_id).unwrap());
        let middle = frame_text(&index.render(source, 6_000..6_006, prefix, prefix_style, width));
        assert!(middle.contains("ROW-05999"));
        assert!(middle.contains('😀'));
    }

    #[test]
    fn combined_output_and_refusal_preserve_both_channels() {
        let mut app = app_with_messages(1);
        let message = &mut app
            .sessions
            .get_mut(&app.focused().unwrap())
            .unwrap()
            .messages
            .as_mut()
            .unwrap()[0];
        message.output = "OUTPUT-BEGIN".to_owned() + &"o".repeat(40 * 1024);
        message.refusal = "REFUSAL-BEGIN".to_owned() + &"r".repeat(40 * 1024) + "REFUSAL-END";

        let mut renderer = FrameRenderer::default();
        let body = renderer.transcript(&app, 80);
        let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
        let tail = frame_text(&body.viewport(&app, 20, 0));

        assert!(top.contains("OUTPUT-BEGIN"));
        assert!(tail.contains("REFUSAL-END"));
    }

    #[test]
    fn completing_a_long_live_message_preserves_a_scrolled_tail_anchor() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        let message = &mut session.messages.as_mut().unwrap()[0];
        message.state = MessageState::Streaming;
        message.output = (0..2_000)
            .map(|row| format!("LIVE-ROW-{row:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut renderer = FrameRenderer::default();
        renderer.frame(&mut app, 80, 24);
        app.handle_terminal_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let live_offset = app.transcript_scroll_offset();
        assert!(live_offset > 0);

        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
        session.loaded_through += 1;
        renderer.frame(&mut app, 80, 24);

        assert_eq!(app.transcript_scroll_offset(), live_offset);
    }

    #[test]
    fn completion_behind_an_overlay_preserves_the_scrolled_live_tail() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        let message = &mut session.messages.as_mut().unwrap()[0];
        message.state = MessageState::Streaming;
        message.output = (0..2_000)
            .map(|row| format!("LIVE-ROW-{row:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut renderer = FrameRenderer::default();
        renderer.frame(&mut app, 80, 24);
        app.handle_terminal_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let live_offset = app.transcript_scroll_offset();
        app.overlay = Some(crate::input::Overlay::models());
        renderer.frame(&mut app, 80, 24);

        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
        session.loaded_through += 1;
        renderer.frame(&mut app, 80, 24);
        app.overlay = None;
        renderer.frame(&mut app, 80, 24);

        assert_eq!(app.transcript_scroll_offset(), live_offset);
    }

    #[test]
    fn completing_a_live_message_does_not_move_an_older_history_viewport() {
        let mut app = app_with_messages(2);
        let session_id = app.focused().unwrap();
        let session = app.sessions.get_mut(&session_id).unwrap();
        let messages = session.messages.as_mut().unwrap();
        messages[0].output = (0..200)
            .map(|row| format!("HISTORY-ROW-{row:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        messages[1].state = MessageState::Streaming;
        messages[1].output = (0..2_000)
            .map(|row| format!("LIVE-ROW-{row:04}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut renderer = FrameRenderer::default();
        renderer.frame(&mut app, 80, 24);
        let page_up = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ));
        while app.handle_terminal_event(page_up.clone()).0 {}
        let before = renderer.frame(&mut app, 80, 24);
        assert!(frame_text(&before).contains("HISTORY-ROW-0000"));
        let history_offset = app.transcript_scroll_offset();

        let session = app.sessions.get_mut(&session_id).unwrap();
        session.messages.as_mut().unwrap()[1].state = MessageState::Complete;
        session.loaded_through += 1;
        let after = renderer.frame(&mut app, 80, 24);

        assert!(frame_text(&after).contains("HISTORY-ROW-0000"));
        assert!(app.transcript_scroll_offset() > history_offset);
    }

    #[test]
    fn sparse_rows_have_a_byte_ceiling_for_zero_width_text() {
        let message = completed_message(
            1,
            format!(
                "a{}",
                "\u{0301}".repeat(MAX_FULL_MARKDOWN_BYTES / '\u{0301}'.len_utf8() + 1)
            ),
        );
        let source = MessageText::new(&message);
        let mut byte = 0;
        let mut rows = 0;
        while let Some((range, next)) = next_plain_text_row(source, byte, 80) {
            assert!(range.len() <= MAX_PLAIN_TEXT_ROW_BYTES);
            assert!(next > byte);
            rows += 1;
            byte = next;
        }
        assert!(rows > 1);

        let index = PlainTextIndex::new(source, 80);
        let rendered = index.render(source, 0..1, "   ", muted(), 83);
        let emitted_bytes = rendered[0]
            .spans
            .iter()
            .map(|span| span.text.len())
            .sum::<usize>();
        assert!(emitted_bytes <= MAX_PLAIN_TEXT_ROW_BYTES + 3);
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
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.context_tokens = Some(64_000);
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
    fn footer_renders_unknown_context_and_cost_without_inventing_zero_usage() {
        let mut app = app_with_messages(0);
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.estimated_cost_usd_nanos = Some(100_000_000);
        session.summary.accounting = Some(SessionAccounting {
            direct: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: Some(100_000_000),
            },
            inclusive: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: None,
            },
        });
        session.summary.context_tokens = None;
        session.context_window = Some(272_000);

        let rows = frame_rows(&[footer_context(&app, 80), footer_workspace(&app, 80)]);

        assert!(rows[0].contains("context: -- / 272000"));
        assert!(rows[1].ends_with("cost: -- "));
    }

    #[test]
    fn footer_uses_legacy_direct_cost_when_structured_accounting_is_absent() {
        let mut app = app_with_messages(0);
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.accounting = None;
        session.summary.estimated_cost_usd_nanos = Some(100_000_000);

        let rows = frame_rows(&[footer_workspace(&app, 80)]);

        assert!(rows[0].ends_with("cost: $0.10 "));
    }

    #[test]
    fn footer_displays_inclusive_accounting_cost() {
        let mut app = app_with_messages(0);
        let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
        session.summary.estimated_cost_usd_nanos = Some(100_000_000);
        session.summary.accounting = Some(SessionAccounting {
            direct: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: Some(100_000_000),
            },
            inclusive: AccountingTotal {
                usage: None,
                estimated_cost_usd_nanos: Some(250_000_000),
            },
        });

        let rows = frame_rows(&[footer_workspace(&app, 80)]);

        assert!(rows[0].ends_with("cost: $0.25 "));
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
    fn composer_renders_hard_newlines_across_multiple_rows() {
        let mut app = App::new(TuiOptions::default());
        app.composer.text = "hello\nworld".to_owned();
        app.animation_tick = 0;
        let rows = frame_rows(&composer(&app, 40, 8));
        assert_eq!(rows, vec![" > hello".to_owned(), "   world|".to_owned()]);
    }

    #[test]
    fn composer_keeps_the_tail_when_max_rows_clip() {
        let mut app = App::new(TuiOptions::default());
        app.composer.text = "one\ntwo\nthree\nfour".to_owned();
        app.animation_tick = 1; // steady caret space, simpler assertions
        let rows = frame_rows(&composer(&app, 40, 2));
        assert_eq!(rows, vec![" … three".to_owned(), "   four ".to_owned()]);
    }

    #[test]
    fn slash_autocomplete_is_filtered_above_the_composer() {
        let mut app = app_with_messages(1);
        app.composer.text = "/".to_owned();
        let frame = FrameRenderer::default().frame(&mut app, 80, 20);
        let text = frame_text(&frame);
        for command in ["/models", "/sessions", "/resume", "/new", "/quit", "/exit"] {
            assert!(text.contains(command));
        }

        app.composer.text = "/qu".to_owned();
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
                activity: None,
                spawned_by: None,
                id: session_id,
                workspace_id,
                parent_id: None,
                title: format!("Session {byte}"),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("openai/gpt-test".to_owned()),
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
                context_tokens: None,
                accounting: None,
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
        app.overlay = Some(crate::input::Overlay::sessions("", selected, None));

        let frame = FrameRenderer::default().frame(&mut app, 80, 12);
        let text = frame_text(&frame);

        assert!(text.contains("SESSIONS"));
        assert!(text.contains("search: all sessions"));
        assert!(text.contains("Session 10"));
    }

    #[test]
    fn session_picker_renders_an_empty_search_result() {
        let mut app = app_with_messages(0);
        app.overlay = Some(crate::input::Overlay::sessions("missing", None, None));

        let frame = FrameRenderer::default().frame(&mut app, 80, 12);
        let text = frame_text(&frame);

        assert!(text.contains("search: missing"));
        assert!(text.contains("No matching sessions."));
    }

    #[test]
    fn session_picker_renders_delete_and_prune_confirmations() {
        let mut app = app_with_messages(0);
        let session_id = SessionId::from_bytes([1; 16]);
        app.overlay = Some(crate::input::Overlay::sessions(
            "",
            Some(session_id),
            Some(SessionConfirm::Delete(session_id)),
        ));

        let frame = FrameRenderer::default().frame(&mut app, 100, 12);
        let text = frame_text(&frame);
        assert!(text.contains("y confirms, n or Esc cancels"));
        assert!(text.contains("delete 'Session'? y deletes, n keeps"));

        app.overlay
            .as_mut()
            .unwrap()
            .set_confirm(Some(SessionConfirm::Prune));
        let frame = FrameRenderer::default().frame(&mut app, 100, 12);
        let text = frame_text(&frame);
        assert!(text.contains("delete every empty session in this workspace?"));

        // Without a pending confirmation the hint advertises both actions.
        app.overlay.as_mut().unwrap().set_confirm(None);
        let frame = FrameRenderer::default().frame(&mut app, 100, 12);
        let text = frame_text(&frame);
        assert!(text.contains("Ctrl-D deletes, Ctrl-P prunes empty"));
    }

    #[test]
    fn model_picker_hint_reflects_apply_versus_create() {
        let mut app = app_with_messages(0);
        app.models.push(crate::app::ModelOption {
            provider: "openai".to_owned(),
            model: "gpt-test".to_owned(),
            name: Some("GPT Test".to_owned()),
            context_window: None,
            selection: ModelSelection {
                model: Some("openai/gpt-test".to_owned()),
                max_output_tokens: None,
                organization: None,
            },
        });
        app.overlay = Some(crate::input::Overlay::models());

        let frame = FrameRenderer::default().frame(&mut app, 100, 12);
        let text = frame_text(&frame);
        assert!(text.contains("Enter sets the session model, Ctrl-N creates a session"));

        app.panes.focused_mut().session = None;
        let frame = FrameRenderer::default().frame(&mut app, 100, 12);
        let text = frame_text(&frame);
        assert!(text.contains("Enter creates session"));
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

    #[test]
    fn page_up_reaches_the_beginning_of_a_long_completed_message() {
        let mut app = app_with_messages(1);
        let session_id = app.focused().unwrap();
        app.sessions
            .get_mut(&session_id)
            .unwrap()
            .messages
            .as_mut()
            .unwrap()[0]
            .output = format!(
            "BEGIN-LONG-MESSAGE\n{}\nEND-LONG-MESSAGE",
            (0..400)
                .map(|row| format!("long response row {row}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut renderer = FrameRenderer::default();
        let tail = renderer.frame(&mut app, 80, 12);

        for _ in 0..100 {
            app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            )));
        }
        let top = renderer.frame(&mut app, 80, 12);

        assert!(frame_text(&tail).contains("END-LONG-MESSAGE"));
        assert!(!frame_text(&tail).contains("BEGIN-LONG-MESSAGE"));
        assert!(frame_text(&top).contains("BEGIN-LONG-MESSAGE"));
    }

    #[test]
    fn sidebar_appears_at_wide_widths_and_shows_live_status_for_cold_sessions() {
        let mut app = app_with_messages(1);
        app.connection = crate::ConnectionState::Live;
        let workspace_id = app.workspace_id.unwrap();
        let parent = app.focused().unwrap();
        let child_id = SessionId::from_bytes([7; 16]);
        let run_id = RunId::from_bytes([8; 16]);
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([4; 16]),
                workspace_id,
                sequence: 2,
            },
            session_id: child_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionCreated {
                session: SessionSummary {
                    id: child_id,
                    workspace_id,
                    parent_id: Some(parent),
                    spawned_by: None,
                    title: "Survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(run_id),
                    activity: Some(qq_protocol::RunActivity::GeneratingResponse),
                    queued_prompts: 0,
                    model: None,
                    profile: qq_protocol::AgentProfileId::default(),
                    correlation: qq_protocol::Correlation::default(),
                    context_tokens: None,
                    accounting: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    last_outcome: None,
                },
            },
        }));
        // The child is cold (no body) but streams text; the sidebar must
        // still show its tail.
        let message = MessageSnapshot {
            id: MessageId::from_bytes([9; 16]),
            session_id: child_id,
            run_id,
            turn_ordinal: 1,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            steering: false,
            output: String::new(),
            refusal: String::new(),
            created_at_ms: 3,
        };
        for (sequence, event) in [
            (3, SessionEvent::AssistantMessageStarted { message }),
            (
                4,
                SessionEvent::TextAppended {
                    message_id: MessageId::from_bytes([9; 16]),
                    channel: qq_protocol::TextChannel::Output,
                    text: "Found twelve call sites".to_owned(),
                },
            ),
        ] {
            app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([4; 16]),
                    workspace_id,
                    sequence,
                },
                session_id: child_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            }));
        }
        assert!(!app.sessions[&child_id].is_warm());

        let rows_at = |app: &mut App, width| {
            frame_rows(&FrameRenderer::default().frame(app, width, 24)).join("\n")
        };
        let narrow = rows_at(&mut app, 100);
        assert!(
            !narrow.contains("SESSIONS  1 running"),
            "auto-hidden when narrow"
        );

        let wide_frame = FrameRenderer::default().frame(&mut app, 160, 24);
        let wide = frame_rows(&wide_frame).join("\n");
        assert!(wide.contains("SESSIONS  1 running"), "{wide}");
        assert!(wide.contains("Survey callers"));
        assert!(wide.contains("Found twelve call sites"));
        // With the sidebar glued on, every body row is exactly the terminal
        // width: the border column lines up and nothing overflows.
        for row in &wide_frame[2..wide_frame.len() - 4] {
            assert_eq!(
                row.width(),
                160,
                "{:?}",
                frame_rows(std::slice::from_ref(row))
            );
        }

        // Ctrl-B hides it even when wide; a second press shows it again.
        let toggle = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        app.handle_terminal_event(toggle.clone());
        assert!(!rows_at(&mut app, 160).contains("SESSIONS  1 running"));
        app.handle_terminal_event(toggle);
        assert!(
            rows_at(&mut app, 100).contains("SESSIONS  1 running"),
            "explicitly shown wins over width"
        );
    }

    #[test]
    fn spawned_children_render_under_their_spawn_call_and_never_fold() {
        let mut app = app_with_messages(1);
        let workspace_id = app.workspace_id.unwrap();
        let parent = app.focused().unwrap();
        let run_id = RunId::from_bytes([2; 16]);
        let spawn_call = tool_call_snapshot(
            0x21,
            "spawn_agent",
            r#"{"task":"survey callers"}"#,
            ToolCallState::Running,
            None,
            false,
        );
        // Four quiet reads plus the spawn call: without the child this run
        // would fold into one counted line at collapsed detail.
        let mut calls = vec![spawn_call.clone()];
        for byte in 0x22..0x26 {
            calls.push(tool_call_snapshot(
                byte,
                "read_file",
                r#"{"path":"a.rs"}"#,
                ToolCallState::Completed,
                Some("ok"),
                false,
            ));
        }
        app.sessions.get_mut(&parent).unwrap().tool_calls = Some(calls);
        let child_id = SessionId::from_bytes([0x30; 16]);
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([4; 16]),
                workspace_id,
                sequence: 2,
            },
            session_id: child_id,
            run_id: Some(RunId::from_bytes([0x31; 16])),
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionCreated {
                session: SessionSummary {
                    id: child_id,
                    workspace_id,
                    parent_id: Some(parent),
                    spawned_by: Some(qq_protocol::SpawnOrigin {
                        run_id,
                        tool_call_id: Some(spawn_call.id),
                    }),
                    title: "survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(RunId::from_bytes([0x31; 16])),
                    activity: Some(qq_protocol::RunActivity::Reasoning),
                    queued_prompts: 0,
                    model: None,
                    profile: qq_protocol::AgentProfileId::default(),
                    correlation: qq_protocol::Correlation::default(),
                    context_tokens: None,
                    accounting: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    last_outcome: None,
                },
            },
        }));
        app.sidebar = crate::app::Sidebar::Hidden;

        let rows = frame_rows(&FrameRenderer::default().frame(&mut app, 100, 40));
        let spawn_row = rows
            .iter()
            .position(|row| row.contains("spawn_agent"))
            .expect("spawn call is rendered, not folded");
        assert!(rows[spawn_row + 1].contains("↳"));
        assert!(rows[spawn_row + 1].contains("survey callers"));
        assert!(rows[spawn_row + 2].contains("reasoning"));
        assert!(
            rows.iter().all(|row| !row.contains("tool calls")),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("related sessions")),
            "an inline child is not repeated below"
        );

        // A child with no recorded call attaches nowhere in the transcript
        // but still appears in related sessions.
        app.sessions.get_mut(&child_id).unwrap().summary.spawned_by = None;
        let rows = frame_rows(&FrameRenderer::default().frame(&mut app, 100, 40));
        let spawn_row = rows
            .iter()
            .position(|row| row.contains("spawn_agent"))
            .unwrap();
        assert!(!rows[spawn_row + 1].contains("↳"));
        assert!(rows.iter().any(|row| row.contains("related sessions")));
    }

    #[test]
    fn background_approvals_surface_a_banner_that_ctrl_g_jumps_to() {
        let mut app = app_with_messages(1);
        app.sidebar = crate::app::Sidebar::Hidden;
        let workspace_id = app.workspace_id.unwrap();
        let parent = app.focused().unwrap();
        let child_id = SessionId::from_bytes([0x40; 16]);
        let run_id = RunId::from_bytes([0x41; 16]);
        let mut sequence = 1;
        let mut event = |session_id, event| {
            sequence += 1;
            ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([4; 16]),
                    workspace_id,
                    sequence,
                },
                session_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            })
        };
        app.apply_client_update(event(
            child_id,
            SessionEvent::SessionCreated {
                session: SessionSummary {
                    id: child_id,
                    workspace_id,
                    parent_id: Some(parent),
                    spawned_by: None,
                    title: "Deploy helper".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(run_id),
                    activity: None,
                    queued_prompts: 0,
                    model: None,
                    profile: qq_protocol::AgentProfileId::default(),
                    correlation: qq_protocol::Correlation::default(),
                    context_tokens: None,
                    accounting: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    last_outcome: None,
                },
            },
        ));
        let call = ToolCallSnapshot {
            id: ToolCallId::from_bytes([0x42; 16]),
            session_id: child_id,
            run_id,
            turn_ordinal: 1,
            call_ordinal: 0,
            provider_call_id: "c".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"rm -rf build"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };
        app.apply_client_update(event(
            child_id,
            SessionEvent::ToolApprovalRequested {
                tool_call: call,
                shell: None,
                edit: None,
            },
        ));

        // Focused on the parent: no modal, but the banner names the child.
        assert_eq!(app.mode(), Mode::Compose);
        let text = frame_rows(&FrameRenderer::default().frame(&mut app, 100, 24)).join("\n");
        assert!(text.contains("approval needed in Deploy helper"), "{text}");
        assert!(text.contains("Ctrl-G"));

        let (changed, requests) = app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )));
        assert!(changed);
        assert_eq!(app.focused(), Some(child_id));
        // The child is cold, so the jump fetches its body...
        assert_eq!(requests.len(), 1);
        // ...and the banner no longer names the session we are now in.
        let text = frame_rows(&FrameRenderer::default().frame(&mut app, 100, 24)).join("\n");
        assert!(!text.contains("approval needed in"));
    }

    #[test]
    fn alt_arrows_walk_the_session_tree_in_spawn_order() {
        let mut app = app_with_messages(0);
        app.sidebar = crate::app::Sidebar::Hidden;
        let workspace_id = app.workspace_id.unwrap();
        let root = app.focused().unwrap();
        let mut sequence = 1;
        let mut created = |app: &mut App, byte: u8, parent: Option<SessionId>, at: u64| {
            sequence += 1;
            let id = SessionId::from_bytes([byte; 16]);
            app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([4; 16]),
                    workspace_id,
                    sequence,
                },
                session_id: id,
                run_id: None,
                caused_by: None,
                occurred_at_ms: sequence,
                event: SessionEvent::SessionCreated {
                    session: SessionSummary {
                        id,
                        workspace_id,
                        parent_id: parent,
                        spawned_by: None,
                        title: format!("s{byte}"),
                        status: SessionStatus::Idle,
                        active_run_id: None,
                        activity: None,
                        queued_prompts: 0,
                        model: None,
                        profile: qq_protocol::AgentProfileId::default(),
                        correlation: qq_protocol::Correlation::default(),
                        context_tokens: None,
                        accounting: None,
                        estimated_cost_usd_nanos: None,
                        updated_at_ms: at,
                        last_outcome: None,
                    },
                },
            }));
            id
        };
        let a = created(&mut app, 0x51, Some(root), 10);
        let b = created(&mut app, 0x52, Some(root), 20);
        let c = created(&mut app, 0x53, Some(root), 30);
        let key = |code| TerminalEvent::Key(KeyEvent::new(code, KeyModifiers::ALT));

        app.handle_terminal_event(key(KeyCode::Down));
        assert_eq!(app.focused(), Some(a), "first child is the oldest");
        app.handle_terminal_event(key(KeyCode::Right));
        assert_eq!(app.focused(), Some(b));
        app.handle_terminal_event(key(KeyCode::Right));
        assert_eq!(app.focused(), Some(c));
        app.handle_terminal_event(key(KeyCode::Right));
        assert_eq!(app.focused(), Some(a), "siblings wrap");
        app.handle_terminal_event(key(KeyCode::Left));
        assert_eq!(app.focused(), Some(c));
        // Esc walks up to the parent (Alt-Up belongs to the draft queue).
        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.focused(), Some(root));
        // A lone root has no siblings; the key is a no-op.
        let (changed, _) = app.handle_terminal_event(key(KeyCode::Right));
        assert!(!changed);
        assert_eq!(app.focused(), Some(root));
    }

    #[test]
    fn reasoning_renders_collapsed_above_the_runs_message_and_expands_on_toggle() {
        let mut app = app_with_messages(0);
        app.sidebar = crate::app::Sidebar::Hidden;
        let workspace_id = app.workspace_id.unwrap();
        let session_id = app.focused().unwrap();
        let run_id = RunId::from_bytes([0x66; 16]);
        let mut sequence = 1;
        let mut event = |event: SessionEvent| {
            sequence += 1;
            ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([4; 16]),
                    workspace_id,
                    sequence,
                },
                session_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            })
        };
        let kind = qq_protocol::ReasoningKind::Summary;
        app.apply_client_update(event(SessionEvent::ReasoningStarted { run_id, kind }));
        app.apply_client_update(event(SessionEvent::ReasoningDelta {
            run_id,
            kind,
            text: "First consider the callers.\n\nThen the tests.".to_owned(),
        }));
        app.apply_client_update(event(SessionEvent::ReasoningCompleted { run_id, kind }));
        app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
            message: MessageSnapshot {
                id: MessageId::from_bytes([0x67; 16]),
                session_id,
                run_id,
                turn_ordinal: 1,
                role: MessageRole::Assistant,
                state: MessageState::Streaming,
                steering: false,
                output: "The answer.".to_owned(),
                refusal: String::new(),
                created_at_ms: 1,
            },
        }));

        let rows = frame_rows(&transcript_lines(&app, 80));
        let reasoning_row = rows
            .iter()
            .position(|row| row.contains("thought for"))
            .expect("collapsed reasoning row");
        let message_row = rows.iter().position(|row| row.contains("QQ")).unwrap();
        assert!(reasoning_row < message_row, "{rows:?}");
        assert!(rows[reasoning_row].contains("First consider the callers."));
        assert!(!rows.iter().any(|row| row.contains("Then the tests.")));
        // Reasoning never leaks into the assistant message body.
        assert!(
            !app.sessions[&session_id].messages.as_ref().unwrap()[0]
                .output
                .contains("consider")
        );

        app.execute(crate::commands::Command::ToggleReasoning);
        let rows = frame_rows(&transcript_lines(&app, 80));
        assert!(rows.iter().any(|row| row.contains("Then the tests.")));
        assert!(rows.iter().any(|row| row.contains("┆")));
    }

    /// `app_with_messages` plus a second warm session titled "Other" whose
    /// messages read `other N`.
    fn app_with_two_sessions(count: u8) -> (App, SessionId, SessionId) {
        let mut app = app_with_messages(count);
        let first = app.focused().unwrap();
        let workspace_id = app.workspace_id.unwrap();
        let other = SessionId::from_bytes([9; 16]);
        let mut summary = app.sessions[&first].summary.clone();
        summary.id = other;
        summary.title = "Other".to_owned();
        let messages = (0..count)
            .map(|row| {
                let mut message = completed_message(0x80 + row, format!("other {row}"));
                message.session_id = other;
                message
            })
            .collect();
        app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
            included: vec![SessionSnapshot {
                summary: summary.clone(),
                messages,
                runs: Vec::new(),
                tool_calls: Vec::new(),
                has_older_tool_calls: false,
                has_older_messages: false,
            }],
            cursor: EventCursor {
                store_id: StoreId::from_bytes([4; 16]),
                workspace_id,
                sequence: 2,
            },
            workspace: WorkspaceSummary {
                id: workspace_id,
                path: "/workspace".to_owned(),
            },
            sessions: vec![summary],
            focused: None,
            has_older_sessions: false,
        }));
        (app, first, other)
    }

    #[test]
    fn two_panes_render_side_by_side_with_titles_and_a_divider() {
        let (mut app, _, other) = app_with_two_sessions(3);
        app.sidebar = crate::app::Sidebar::Hidden;
        app.execute(Command::SplitBeside);
        app.focus_session(other);
        let frame = FrameRenderer::default().frame(&mut app, 101, 24);
        let rows = frame_rows(&frame);
        // Row 2 is the pane title row: the left pane is unfocused, the right
        // pane carries the focus marker and its title.
        let titles = &rows[2];
        assert!(titles.contains(" Session"), "{titles}");
        assert!(titles.contains("▎Other"), "{titles}");
        // Every body row has a divider at the split column and both
        // transcripts appear on their own side of it.
        let body = &rows[2..2 + 24 - 6];
        assert!(
            body.iter().all(|row| row.chars().nth(50) == Some('│')),
            "{body:?}"
        );
        let (left, right): (Vec<&str>, Vec<&str>) =
            body.iter().map(|row| row.split_once('│').unwrap()).unzip();
        assert!(left.iter().any(|row| row.contains("row 2")));
        assert!(!left.iter().any(|row| row.contains("other")));
        assert!(right.iter().any(|row| row.contains("other 2")));
        assert!(!right.iter().any(|row| row.contains("row 2")));
        // The composer footer describes the focused pane's session.
        assert!(frame_text(&frame).contains("Other"));
        // Rows never exceed the frame width; a wide message cannot bleed
        // across the divider into its neighbour.
        assert!(frame.iter().all(|line| line.width() <= 101));
    }

    #[test]
    fn stacked_panes_share_the_width_and_scroll_independently() {
        let (mut app, _, _) = app_with_two_sessions(40);
        app.sidebar = crate::app::Sidebar::Hidden;
        app.execute(Command::SplitBelow);
        let mut renderer = FrameRenderer::default();
        renderer.frame(&mut app, 80, 40);
        let (tiles, dividers) = app.panes.layout(crate::panes::Rect::new(0, 2, 80, 34));
        assert_eq!(tiles.len(), 2);
        assert_eq!(dividers[0].height, 1);
        assert_eq!(dividers[0].width, 80);
        let top = tiles[0].pane;
        let bottom = tiles[1].pane;
        assert_eq!(app.panes.focused_id(), bottom);

        // PageUp scrolls only the focused (bottom) pane.
        app.handle_terminal_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let frame = renderer.frame(&mut app, 80, 40);
        assert!(app.viewport(bottom).unwrap().offset() > 0);
        assert_eq!(app.viewport(top).unwrap().offset(), 0);
        let rows = frame_rows(&frame);
        let divider_row = rows
            .iter()
            .position(|row| row.starts_with("──"))
            .expect("horizontal divider");
        assert!(rows[..divider_row].iter().any(|row| row.contains("row 39")));
        assert!(!rows[divider_row..].iter().any(|row| row.contains("row 39")));
    }

    #[test]
    fn a_height_only_resize_keeps_every_pane_cache() {
        let (mut app, _, other) = app_with_two_sessions(4);
        app.sidebar = crate::app::Sidebar::Hidden;
        app.execute(Command::SplitBeside);
        app.focus_session(other);
        let mut renderer = FrameRenderer::default();
        renderer.frame(&mut app, 101, 24);
        let ids = app.panes.ids();
        let cached_before: Vec<usize> = ids
            .iter()
            .map(|id| renderer.cache(*id).markdown.len())
            .collect();
        assert_eq!(cached_before, vec![4, 4]);
        let widths: Vec<usize> = ids
            .iter()
            .map(|id| renderer.cache(*id).markdown.values().next().unwrap().width)
            .collect();

        renderer.frame(&mut app, 101, 30);
        for (id, width) in ids.iter().zip(widths) {
            let cache = renderer.cache(*id);
            assert_eq!(cache.markdown.len(), 4);
            assert!(cache.markdown.values().all(|cached| cached.width == width));
        }
        // Closing a pane drops its cache on the next frame.
        app.execute(Command::ClosePane);
        renderer.frame(&mut app, 101, 30);
        assert_eq!(renderer.panes.len(), 1);
    }

    #[test]
    fn a_narrow_frame_shows_only_the_focused_pane_and_no_divider() {
        let (mut app, _, other) = app_with_two_sessions(2);
        app.sidebar = crate::app::Sidebar::Hidden;
        app.execute(Command::SplitBeside);
        app.focus_session(other);
        let frame = FrameRenderer::default().frame(&mut app, 40, 16);
        let text = frame_text(&frame);
        assert!(text.contains("other 1"));
        assert!(!text.contains("row 1"));
        assert!(!text.contains('│'));
        assert_eq!(app.panes.len(), 2, "the tree is untouched");
    }

    #[test]
    fn switching_theme_repaints_every_row_in_the_new_palette() {
        let mut app = app_with_messages(2);
        app.themes.push(crate::Theme::from_roles(
            "magenta",
            [crate::ThemeColor::Rgb(0xff, 0x00, 0xff); 8],
        ));
        let mut renderer = FrameRenderer::default();
        renderer.draw(&mut app, (80, 24)).unwrap();
        let brand_before = renderer.previous[0].spans[0].style.color;
        assert_eq!(brand_before, Some(Palette::QQ.brand));
        // A settled frame with nothing changed writes nothing.
        let idle = renderer.draw(&mut app, (80, 24)).unwrap();
        let idle_rows = String::from_utf8_lossy(&idle).matches("\x1b[2K").count();
        assert_eq!(idle_rows, 0);

        app.execute(Command::OpenThemes);
        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.theme().name, "magenta");
        let repaint = renderer.draw(&mut app, (80, 24)).unwrap();
        let repainted_rows = String::from_utf8_lossy(&repaint).matches("\x1b[2K").count();
        assert_eq!(repainted_rows, 24, "every row is rewritten");
        let magenta = crossterm::style::Color::Rgb {
            r: 0xff,
            g: 0,
            b: 0xff,
        };
        assert_eq!(renderer.previous[0].spans[0].style.color, Some(magenta));
        // Only the picker's swatches (painted in each theme's own colors)
        // may show anything but the new palette.
        assert!(
            renderer
                .previous
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.text != "██")
                .filter_map(|span| span.style.color)
                .all(|color| color == magenta),
            "no row keeps a color from the previous theme"
        );
        // Style helpers on this thread keep the last activated palette;
        // restore the default so later tests see the compiled look.
        theme::activate(Palette::QQ);
    }
}
