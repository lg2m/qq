//! Styled text primitives and the terminal writer. Everything the TUI draws
//! is a `Line` of `Span`s; the frame differ compares whole lines, so equality
//! on these types is the redraw criterion.

use std::io::{self, Write};

use crossterm::{
    queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
};
use unicode_width::UnicodeWidthChar;

use crate::{app::terminal_safe_character, theme};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    pub(crate) color: Option<Color>,
    pub(crate) background: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
}

impl Style {
    pub(crate) const fn color(color: Color) -> Self {
        Self {
            color: Some(color),
            background: None,
            bold: false,
            dim: false,
            italic: false,
        }
    }

    pub(crate) const fn on(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    pub(crate) const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub(crate) const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub(crate) const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) text: String,
    pub(crate) style: Style,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) spans: Vec<Span>,
}

impl Line {
    pub(crate) fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span {
                text: text.into(),
                style,
            }],
        }
    }

    pub(crate) fn push(&mut self, text: impl Into<String>, style: Style) {
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

    pub(crate) fn width(&self) -> usize {
        self.spans
            .iter()
            .flat_map(|span| span.text.chars())
            .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
            .sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }
}

/// Role styles read the thread's active palette (see `theme.rs`); the
/// renderer refreshes it once per frame so a theme switch repaints
/// everything without threading a theme through every leaf.
pub(crate) fn normal() -> Style {
    Style::color(theme::active().text)
}

/// Secondary text. Color alone carries the step back: `Dim` on top would
/// sink it below readability on many terminals.
pub(crate) fn muted() -> Style {
    Style::color(theme::active().muted)
}

pub(crate) fn accent() -> Style {
    Style::color(theme::active().accent)
}

pub(crate) fn brand() -> Style {
    Style::color(theme::active().brand)
}

pub(crate) fn warning() -> Style {
    Style::color(theme::active().warning)
}

pub(crate) fn failure() -> Style {
    Style::color(theme::active().error)
}

pub(crate) fn success() -> Style {
    Style::color(theme::active().success)
}

/// Running-state color for spinners and activity labels.
pub(crate) fn info() -> Style {
    Style::color(theme::active().info)
}

/// Pane dividers and rules at rest.
pub(crate) fn border() -> Style {
    Style::color(theme::active().border)
}

/// The focused pane's divider and title mark.
pub(crate) fn border_active() -> Style {
    Style::color(theme::active().border_active)
}

/// Background for the selected row in pickers and lists.
pub(crate) fn selection(style: Style) -> Style {
    style.on(theme::active().selection_bg)
}

/// Dark surface tint behind code-block panels, distinct from the terminal
/// background so a padded block reads as one solid slab.
pub(crate) fn surface_color() -> Color {
    theme::active().surface
}

pub(crate) fn surface(style: Style) -> Style {
    style.on(surface_color())
}

/// Syntax palette for highlighted code panels, derived from theme roles so
/// every theme colors code in its own voice: keywords in brand, strings in
/// success, comments in muted, functions in accent, types in warning,
/// constants in error, properties in text. Anything a grammar leaves
/// uncaptured keeps the plain panel text style.
pub(crate) fn code_keyword() -> Style {
    Style::color(theme::active().brand)
}

pub(crate) fn code_string() -> Style {
    Style::color(theme::active().success)
}

pub(crate) fn code_comment() -> Style {
    Style::color(theme::active().muted).italic()
}

pub(crate) fn code_function() -> Style {
    Style::color(theme::active().accent)
}

pub(crate) fn code_type() -> Style {
    Style::color(theme::active().warning)
}

pub(crate) fn code_constant() -> Style {
    Style::color(theme::active().error)
}

pub(crate) fn code_property() -> Style {
    Style::color(theme::active().text)
}

/// Unified-diff line coloring: additions in success on the add tint,
/// removals in error on the delete tint, hunk headers muted, context lines
/// normal. Diff lines never reflow.
pub(crate) fn diff_line_style(line: &str) -> Style {
    let palette = theme::active();
    if line.starts_with("@@") {
        muted()
    } else if line.starts_with('+') {
        success().on(palette.diff_add_bg)
    } else if line.starts_with('-') {
        failure().on(palette.diff_del_bg)
    } else {
        normal()
    }
}

pub(crate) fn write_line(output: &mut impl Write, line: &Line) -> io::Result<()> {
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
