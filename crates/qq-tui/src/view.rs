//! Frame assembly: composes chrome, transcript, and overlays into the lines
//! the renderer diffs against the previous frame.

mod chrome;
mod highlight;
mod markdown;
mod overlay;
mod sidebar;
mod tools;
mod transcript;
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
    app::{App, SessionView, ToolDetail, terminal_safe_character},
    input::{Mode, SessionConfirm},
    panes::{PaneId, Rect, Tile, Viewport},
    render::{
        Line, Style, accent, brand, diff_line_style, failure, muted, normal, warning, write_line,
    },
    theme,
};
use chrome::*;
use highlight::HighlightKey;
pub(crate) use highlight::{Highlighted, Highlighter};
use markdown::{has_fenced_code, markdown_lines, settled_prefix_end};
use overlay::*;
use sidebar::*;
use tools::*;
use transcript::*;
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
    /// Viewports reconciled while building the last frame. `draw` hands them
    /// back to the app after the frame is composed; `frame` itself never
    /// mutates the model.
    viewport_updates: Vec<ViewportUpdate>,
    /// Tiles laid out for the last frame, for the same hand-back.
    tiles: Vec<Tile>,
}

/// A pane's viewport after reconciling it with the body drawn this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewportUpdate {
    pub(crate) pane: PaneId,
    pub(crate) viewport: Viewport,
}

impl FrameRenderer {
    /// Hand the geometry decided while building the last frame back to the
    /// model: reconciled viewports and the tiles on screen.
    pub(crate) fn commit(&mut self, app: &mut App) {
        for update in self.viewport_updates.drain(..) {
            app.set_viewport(update.pane, update.viewport);
        }
        app.panes.remember_tiles(std::mem::take(&mut self.tiles));
    }

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
        self.commit(app);
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

    /// Build one frame from the model without changing it. Viewport clamps
    /// computed along the way are queued in `viewport_updates` for `draw`.
    fn frame(&mut self, app: &App, width: usize, height: usize) -> Vec<Line> {
        theme::activate(app.theme().palette);
        self.viewport_updates.clear();
        self.tiles.clear();
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
            // Overlays hide the transcript; its caches stay warm so closing
            // one costs no relayout or highlight storm. Memory stays bounded
            // by the per-pane byte budget, not by pruning here.
            Mode::Models => model_picker(app, body_width, body_height),
            Mode::Themes => theme_picker(app, body_width, body_height),
            Mode::Sessions => session_picker(app, body_width, body_height),
            Mode::Commands => command_picker(app, body_width, body_height),
            Mode::Approval => approval_prompt(app, body_width, body_height),
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
    fn panes_body(&mut self, app: &App, area: Rect) -> Vec<Line> {
        let (tiles, dividers) = app.panes.layout(area);
        self.tiles.clone_from(&tiles);
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
            let (lines, update) = cache.pane(&mut self.highlighter, app, tile, false);
            self.viewport_updates.push(update);
            return lines;
        }
        let multiple = tiles.len() > 1;
        // Every visible piece — pane rows or a divider — in x order. Each row
        // of the canvas is then the pieces covering that row, left to right,
        // so composition is one pass with no scratch tables, and pane rows are
        // moved into place rather than copied.
        let mut pieces: Vec<(Rect, Piece)> = Vec::with_capacity(tiles.len() + dividers.len());
        for tile in tiles {
            let cache = self.panes.entry(tile.pane).or_default();
            let (lines, update) = cache.pane(&mut self.highlighter, app, tile, multiple);
            self.viewport_updates.push(update);
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

#[cfg(test)]
mod tests;

#[cfg(test)]
impl FrameRenderer {
    /// Build a frame and hand its geometry back to the app, as `draw` does.
    pub(crate) fn frame_and_commit(
        &mut self,
        app: &mut App,
        width: usize,
        height: usize,
    ) -> Vec<Line> {
        let frame = self.frame(app, width, height);
        self.commit(app);
        frame
    }
}
