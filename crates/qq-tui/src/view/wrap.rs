//! Width-aware line manipulation: wrapping, truncation, indentation, and
//! viewport slicing. Every function preserves span styles across breaks and
//! never emits a row wider than the requested width.

use unicode_width::UnicodeWidthChar;

use crate::render::{Line, Style, muted};

/// A run of characters that wraps as one unit: either whitespace or a word.
struct WrapToken {
    whitespace: bool,
    width: usize,
    characters: Vec<(char, Style)>,
}

/// Wraps prose at whitespace, preserving span styles across breaks. A single
/// token wider than the width falls back to character breaking, and the
/// whitespace a break lands on is dropped rather than carried over.
pub(crate) fn wrap_line(line: Line, width: usize) -> Vec<Line> {
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
pub(crate) fn wrap_line_chars(line: Line, width: usize) -> Vec<Line> {
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

pub(crate) fn indent_lines(
    lines: Vec<Line>,
    prefix: &str,
    prefix_style: Style,
    width: usize,
) -> Vec<Line> {
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

pub(crate) fn truncate_line(line: Line, width: usize) -> Line {
    if line.width() <= width {
        return line;
    }
    if width <= 3 {
        return Line::styled(".".repeat(width), muted());
    }
    let mut output = Line::default();
    let mut used = 0_usize;
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

pub(crate) fn selection_viewport(
    lines: Vec<Line>,
    height: usize,
    selected_row: usize,
) -> Vec<Line> {
    let start = selected_row
        .saturating_sub(height / 2)
        .min(lines.len().saturating_sub(height));
    lines.into_iter().skip(start).take(height).collect()
}

#[cfg(test)]
pub(crate) fn transcript_viewport(mut lines: Vec<Line>, height: usize, offset: usize) -> Vec<Line> {
    let offset = offset.min(lines.len().saturating_sub(height));
    let end = lines.len().saturating_sub(offset);
    let start = end.saturating_sub(height);
    lines.drain(end..);
    lines.drain(..start);
    fit_height(lines, height)
}

pub(crate) fn fit_height(mut lines: Vec<Line>, height: usize) -> Vec<Line> {
    lines.resize(height, Line::default());
    lines.truncate(height);
    lines
}

pub(crate) fn preview(text: &str, width: usize) -> String {
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

pub(crate) fn bounded_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::normal;

    fn frame_rows(frame: &[Line]) -> Vec<String> {
        frame
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
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
    fn truncated_rows_never_exceed_the_terminal_width() {
        for width in 0..10 {
            let line = truncate_line(Line::styled("a long row", normal()), width);
            assert!(line.width() <= width);
        }
    }
}
