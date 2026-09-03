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

use crate::app::terminal_safe_character;

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

pub(crate) fn normal() -> Style {
    Style::color(Color::White)
}

pub(crate) fn muted() -> Style {
    Style::color(Color::DarkGrey).dim()
}

pub(crate) fn accent() -> Style {
    Style::color(Color::Cyan)
}

pub(crate) fn brand() -> Style {
    Style::color(Color::Rgb {
        r: 255,
        g: 159,
        b: 67,
    })
}

pub(crate) fn warning() -> Style {
    Style::color(Color::Yellow)
}

pub(crate) fn failure() -> Style {
    Style::color(Color::Red)
}

pub(crate) fn success() -> Style {
    Style::color(Color::Green)
}

/// Dark surface tint behind code-block panels, distinct from the terminal
/// background so a padded block reads as one solid slab.
pub(crate) const SURFACE_COLOR: Color = Color::Rgb {
    r: 38,
    g: 40,
    b: 48,
};

pub(crate) fn surface(style: Style) -> Style {
    style.on(SURFACE_COLOR)
}

/// Syntax palette for highlighted code panels: restrained named colors that
/// stay readable on the dark surface tint. Anything a grammar leaves
/// uncaptured keeps the plain panel text style.
pub(crate) fn code_keyword() -> Style {
    Style::color(Color::Magenta)
}

pub(crate) fn code_string() -> Style {
    Style::color(Color::Green)
}

pub(crate) fn code_comment() -> Style {
    Style::color(Color::DarkGrey).italic()
}

pub(crate) fn code_function() -> Style {
    Style::color(Color::Cyan)
}

pub(crate) fn code_type() -> Style {
    Style::color(Color::Yellow)
}

pub(crate) fn code_constant() -> Style {
    Style::color(Color::DarkYellow)
}

pub(crate) fn code_property() -> Style {
    Style::color(Color::Blue)
}

/// Unified-diff line coloring: additions green, removals red, hunk headers in
/// the muted accent, context lines normal. Diff lines never reflow.
pub(crate) fn diff_line_style(line: &str) -> Style {
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
