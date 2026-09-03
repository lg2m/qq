//! Markdown layout: prose, lists, quotes, tables, and fenced code panels with
//! optional tree-sitter highlighting. Produces styled lines already wrapped to
//! the requested width.

use std::sync::OnceLock;

use crate::{
    app::terminal_safe_character,
    render::{
        Line, Style, accent, code_comment, code_constant, code_function, code_keyword,
        code_property, code_string, code_type, diff_line_style, muted, normal, surface, warning,
    },
    view::wrap::{wrap_line, wrap_line_chars},
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

/// Byte offset where a streaming markdown source can be split so the prefix
/// renders the same whether laid out alone or as part of the whole.
///
/// The split lands after a blank line that is neither inside an open fenced
/// code block nor directly below an indented (four-space) code line, because
/// pulldown-cmark treats such a blank line as a hard block boundary: nothing
/// after it changes how earlier blocks lay out. The blank line stays with the
/// prefix so it ends at a block end. Returns 0 when no boundary exists yet.
/// Link reference definitions are the one construct this ignores; a reference
/// defined after the split renders literally until the message completes.
pub(crate) fn settled_prefix_end(source: &str) -> usize {
    let mut settled = 0;
    let mut in_fence: Option<(u8, usize)> = None;
    let mut previous_indented = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let fence_len = trimmed
            .bytes()
            .take_while(|byte| *byte == b'`' || *byte == b'~')
            .count();
        if fence_len >= 3 && line.len() - trimmed.len() < 4 {
            let marker = trimmed.as_bytes()[0];
            let uniform = trimmed.as_bytes()[..fence_len]
                .iter()
                .all(|byte| *byte == marker);
            match in_fence {
                None if uniform => in_fence = Some((marker, fence_len)),
                Some((open_marker, open_len))
                    if uniform
                        && marker == open_marker
                        && fence_len >= open_len
                        && trimmed[fence_len..].trim().is_empty() =>
                {
                    in_fence = None;
                }
                None | Some(_) => {}
            }
        }
        offset += line.len();
        if blank {
            if in_fence.is_none() && !previous_indented && line.ends_with('\n') {
                settled = offset;
            }
        } else {
            previous_indented =
                in_fence.is_none() && (line.starts_with("    ") || line.starts_with('\t'));
        }
    }
    settled
}

/// Lays markdown out as styled lines. `highlight` enables tree-sitter syntax
/// coloring inside fenced code panels; it is only worth paying for content
/// that renders once (terminal-state messages on the cached path), so
/// streaming render paths pass `false` and get plain panels.
pub(crate) fn markdown_lines(source: &str, width: usize, highlight: bool) -> Vec<Line> {
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
                        let rendered = layout_code_panel(&buffer, width.max(1), highlight);
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

/// Bytes past which a code block skips tree-sitter and renders plain;
/// highlighting must never stall a frame.
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

/// A recognized highlight capture name paired with its theme style.
type HighlightCapture = (&'static str, fn() -> Style);

/// Highlight capture names recognized in grammar queries, each mapped to a
/// theme style. `HighlightConfiguration::configure` resolves query captures
/// against these by longest dotted prefix, so `keyword.control.repeat` lands
/// on `keyword`; captures matching nothing keep the plain panel style.
const HIGHLIGHT_CAPTURES: &[HighlightCapture] = &[
    ("attribute", code_constant),
    ("comment", code_comment),
    ("constant", code_constant),
    ("constructor", code_type),
    ("escape", code_constant),
    ("function", code_function),
    ("keyword", code_keyword),
    ("label", code_constant),
    ("number", code_constant),
    ("property", code_property),
    ("string", code_string),
    ("string.special.key", code_property),
    ("tag", code_function),
    ("type", code_type),
    ("variable.builtin", code_keyword),
];

/// Builds one grammar's highlight configuration on first use. Compiling the
/// highlight query is the expensive step, so each grammar pays it once; a
/// grammar whose query fails to compile stays `None` and its blocks render
/// plain forever rather than retrying every frame.
fn highlight_configuration(
    cell: &'static OnceLock<Option<HighlightConfiguration>>,
    name: &str,
    language: Language,
    highlights: &str,
) -> Option<&'static HighlightConfiguration> {
    cell.get_or_init(|| {
        let mut configuration =
            HighlightConfiguration::new(language, name, highlights, "", "").ok()?;
        let names = HIGHLIGHT_CAPTURES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        configuration.configure(&names);
        Some(configuration)
    })
    .as_ref()
}

/// Maps a fence tag (with common aliases) to a bundled grammar's highlight
/// configuration. Unknown or absent tags return `None` and render plain.
/// TypeScript, TSX, JSX, and C++ queries only extend their base language's
/// query, so those arms concatenate the extension ahead of the base (earlier
/// patterns win in tree-sitter queries).
pub(crate) fn fence_highlight_configuration(tag: &str) -> Option<&'static HighlightConfiguration> {
    macro_rules! grammar {
        ($name:literal, $language:expr, $highlights:expr) => {{
            static CELL: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
            highlight_configuration(&CELL, $name, $language.into(), $highlights)
        }};
    }
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => grammar!(
            "rust",
            tree_sitter_rust::LANGUAGE,
            tree_sitter_rust::HIGHLIGHTS_QUERY
        ),
        "toml" => grammar!(
            "toml",
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY
        ),
        "json" | "jsonc" | "json5" => grammar!(
            "json",
            tree_sitter_json::LANGUAGE,
            tree_sitter_json::HIGHLIGHTS_QUERY
        ),
        "yaml" | "yml" => grammar!(
            "yaml",
            tree_sitter_yaml::LANGUAGE,
            tree_sitter_yaml::HIGHLIGHTS_QUERY
        ),
        "bash" | "sh" | "shell" | "zsh" => grammar!(
            "bash",
            tree_sitter_bash::LANGUAGE,
            tree_sitter_bash::HIGHLIGHT_QUERY
        ),
        "python" | "py" | "python3" => grammar!(
            "python",
            tree_sitter_python::LANGUAGE,
            tree_sitter_python::HIGHLIGHTS_QUERY
        ),
        "javascript" | "js" | "mjs" | "cjs" => grammar!(
            "javascript",
            tree_sitter_javascript::LANGUAGE,
            tree_sitter_javascript::HIGHLIGHT_QUERY
        ),
        "jsx" => grammar!(
            "jsx",
            tree_sitter_javascript::LANGUAGE,
            &format!(
                "{}\n{}",
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "typescript" | "ts" => grammar!(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            &format!(
                "{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "tsx" => grammar!(
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX,
            &format!(
                "{}\n{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            )
        ),
        "go" | "golang" => grammar!(
            "go",
            tree_sitter_go::LANGUAGE,
            tree_sitter_go::HIGHLIGHTS_QUERY
        ),
        "c" | "h" => grammar!("c", tree_sitter_c::LANGUAGE, tree_sitter_c::HIGHLIGHT_QUERY),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" => grammar!(
            "cpp",
            tree_sitter_cpp::LANGUAGE,
            &format!(
                "{}\n{}",
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                tree_sitter_c::HIGHLIGHT_QUERY
            )
        ),
        _ => None,
    }
}

/// Runs tree-sitter highlighting over one code block, returning one styled
/// line per source line. Tree-sitter is error-tolerant, so partial or invalid
/// code still highlights; any highlighter failure returns `None` and the
/// caller falls back to plain panel text.
fn highlighted_code_lines(configuration: &HighlightConfiguration, text: &str) -> Option<Vec<Line>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(configuration, text.as_bytes(), None, |_| None)
        .ok()?;
    let mut lines = vec![Line::default()];
    let mut active: Vec<Highlight> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => active.push(highlight),
            HighlightEvent::HighlightEnd => {
                active.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = active
                    .last()
                    .and_then(|highlight| HIGHLIGHT_CAPTURES.get(highlight.0))
                    .map_or_else(normal, |(_, style)| style());
                for (index, part) in text.get(start..end)?.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(Line::default());
                    }
                    let safe = part
                        .chars()
                        .filter_map(terminal_safe_character)
                        .collect::<String>();
                    lines
                        .last_mut()
                        .expect("lines starts populated")
                        .push(safe, style);
                }
            }
        }
    }
    // The block's trailing newline would otherwise read as an extra empty
    // content row that the plain `str::lines` path never produces.
    if text.ends_with('\n') && lines.last().is_some_and(Line::is_empty) {
        lines.pop();
    }
    Some(lines)
}

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
///
/// With `highlight` set, a recognized fence tag colors the content through
/// its tree-sitter grammar; diff fences keep their dedicated coloring, and
/// oversized blocks or highlighter failures fall back to plain text.
fn layout_code_panel(block: &CodeBlockBuffer, width: usize, highlight: bool) -> Vec<Line> {
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
    let highlighted = if highlight && !diff && block.text.len() <= MAX_HIGHLIGHT_BYTES {
        block
            .language
            .as_deref()
            .and_then(fence_highlight_configuration)
            .and_then(|configuration| highlighted_code_lines(configuration, &block.text))
    } else {
        None
    };
    match highlighted {
        Some(content) => {
            for line in content {
                for wrapped in wrap_line_chars(line, content_width) {
                    output.push(code_panel_row(wrapped, width));
                }
            }
        }
        None => {
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
        }
    }
    output.push(code_panel_row(Line::default(), width));
    output
}

/// One physical panel row: the bordered gutter, the content, and enough
/// trailing padding to carry the background tint to the full width.
pub(crate) fn code_panel_row(content: Line, width: usize) -> Line {
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

pub(crate) fn append_safe_text(lines: &mut Vec<Line>, text: &str, style: Style) {
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

pub(crate) fn ensure_line(lines: &mut Vec<Line>) {
    if !lines.last().is_none_or(Line::is_empty) {
        lines.push(Line::default());
    }
}

/// Whether `source` contains a fenced code block that highlighting could
/// color. Cheap line scan; false positives only cost one skipped job.
pub(crate) fn has_fenced_code(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("```") || trimmed.starts_with("~~~")
    })
}

#[cfg(test)]
pub(super) mod tests;
