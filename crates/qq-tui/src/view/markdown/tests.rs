use super::*;
use crate::render::{diff_line_style, surface_color};
use unicode_width::UnicodeWidthChar;

fn frame_rows(frame: &[Line]) -> Vec<String> {
    frame
        .iter()
        .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

#[test]
fn markdown_rows_remain_within_the_render_width() {
    let lines = markdown_lines("**Streaming** text remains narrow and readable.", 9, false);
    assert!(lines.iter().all(|line| line.width() <= 9));
}

#[test]
fn tables_render_aligned_columns_with_a_header_separator() {
    let source =
        "| Order | Source |\n| --- | --- |\n| 1 | Built-in defaults |\n| 2 | Cached manifest |";
    let lines = markdown_lines(source, 60, false);
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
    let lines = markdown_lines(source, width, false);
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
    let rows = frame_rows(&markdown_lines(source, 10, false));

    assert_eq!(
        rows,
        ["A: 1", "B: 2", "C: 3", "---", "A: 4", "B: 5", "C: 6"]
    );
}

#[test]
fn cjk_table_content_aligns_by_display_width() {
    let source = "| 名前 | 説明 |\n| --- | --- |\n| 短い | 長い説明テキスト |";
    let lines = markdown_lines(source, 40, false);
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
            let lines = markdown_lines(fragment, width, false);
            assert!(lines.iter().all(|line| line.width() <= width.max(1)));
        }
    }
}

#[test]
fn soft_breaks_reflow_paragraphs_to_the_render_width() {
    // A source-wrapped paragraph joins into one row when it fits...
    assert_eq!(
        frame_rows(&markdown_lines("alpha beta\ngamma delta", 40, false)),
        ["alpha beta gamma delta"]
    );
    // ...and rewraps at the terminal width, not the source width.
    assert_eq!(
        frame_rows(&markdown_lines("alpha beta\ngamma delta", 12, false)),
        ["alpha beta ", "gamma delta"]
    );
    // A hard break still forces an explicit line break.
    assert_eq!(
        frame_rows(&markdown_lines("alpha  \nbeta", 40, false)),
        ["alpha", "beta"]
    );
}

#[test]
fn code_blocks_keep_character_wrapping() {
    let rows = frame_rows(&markdown_lines(
        "```\nlet answer_value = 42;\n```",
        12,
        false,
    ));

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
    let lines = markdown_lines("```rust\nlet x = 1;\n```", width, false);
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
            .all(|span| span.style.background == Some(surface_color()))
    );
    assert_eq!(lines[0].spans[0].style, surface(accent().dim()));
    assert_eq!(lines[0].spans[1].style, surface(muted()));
}

#[test]
fn diff_fenced_blocks_color_lines_inside_the_panel() {
    let source = "```diff\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context\n```";
    let lines = markdown_lines(source, 30, false);

    let style_of = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("@@ -1,2 +1,2 @@"), Some(surface(muted())));
    assert_eq!(style_of("-old line"), Some(surface(diff_line_style("-"))));
    assert_eq!(style_of("+new line"), Some(surface(diff_line_style("+"))));
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
            for highlight in [false, true] {
                let lines = markdown_lines(fragment, width, highlight);
                assert!(lines.iter().all(|line| line.width() <= width.max(1)));
            }
        }
    }
    // A fence still streaming renders as a panel with the text so far.
    let rows = frame_rows(&markdown_lines("```rust\nfn main() {", 24, false));
    assert!(rows.iter().any(|row| row.starts_with("│ fn main() {")));
}

/// Finds the style of the first span whose text contains `needle`.
pub(crate) fn style_of(lines: &[Line], needle: &str) -> Option<Style> {
    lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.text.contains(needle))
        .map(|span| span.style)
}

#[test]
fn fence_tags_map_to_bundled_grammars_including_aliases() {
    for tag in [
        "rust",
        "toml",
        "json",
        "yaml",
        "bash",
        "python",
        "javascript",
        "typescript",
        "tsx",
        "jsx",
        "go",
        "c",
        "cpp",
    ] {
        assert!(
            fence_highlight_configuration(tag).is_some(),
            "{tag} maps to a grammar with a compiling query"
        );
    }
    for (alias, canonical) in [
        ("rs", "rust"),
        ("Rust", "rust"),
        ("sh", "bash"),
        ("shell", "bash"),
        ("zsh", "bash"),
        ("py", "python"),
        ("js", "javascript"),
        ("ts", "typescript"),
        ("yml", "yaml"),
        ("golang", "go"),
        ("c++", "cpp"),
        ("jsonc", "json"),
    ] {
        let (alias_configuration, canonical_configuration) = (
            fence_highlight_configuration(alias).expect("alias resolves"),
            fence_highlight_configuration(canonical).expect("canonical resolves"),
        );
        assert!(
            std::ptr::eq(alias_configuration, canonical_configuration),
            "{alias} shares {canonical}'s configuration"
        );
    }
    // Unknown tags render plain; diff keeps its dedicated coloring and
    // ron has no maintained grammar crate.
    for tag in ["", "diff", "ron", "console", "brainfuck"] {
        assert!(fence_highlight_configuration(tag).is_none(), "{tag} plain");
    }
}

#[test]
fn highlighted_rust_panels_style_keywords_strings_and_comments() {
    let source = "```rust\n// note\nlet x = \"hi\";\n```";
    let width = 40;
    let lines = markdown_lines(source, width, true);

    assert_eq!(style_of(&lines, "// note"), Some(surface(code_comment())));
    assert_eq!(style_of(&lines, "let"), Some(surface(code_keyword())));
    assert_eq!(style_of(&lines, "\"hi\""), Some(surface(code_string())));
    // Every highlighted span still carries the panel tint, every row pads
    // to the full width, and the panel structure (label row, gutter,
    // padding rows) matches the plain rendering exactly.
    assert!(lines.iter().all(|line| line.width() == width));
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.background == Some(surface_color()))
    );
    assert_eq!(
        frame_rows(&lines),
        frame_rows(&markdown_lines(source, width, false))
    );
    assert_eq!(lines[0].spans[0].style, surface(accent().dim()));
    assert_eq!(lines[0].spans[1].style, surface(muted()));
}

#[test]
fn highlighted_long_lines_keep_character_wrapping_and_span_styles() {
    let source = "```rust\nlet answer = \"abcdefghijklmnopqrst\";\n```";
    let width = 16;
    let lines = markdown_lines(source, width, true);

    assert!(lines.iter().all(|line| line.width() == width));
    // Character wrapping, not reflow: the rows match the plain panel.
    assert_eq!(
        frame_rows(&lines),
        frame_rows(&markdown_lines(source, width, false))
    );
    // The wrapped string literal keeps its style on every row it spans.
    let string_rows = lines
        .iter()
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| span.style == surface(code_string()))
        })
        .count();
    assert!(string_rows >= 2, "string literal spans wrapped rows");
}

#[test]
fn oversized_code_blocks_fall_back_to_plain_panel_text() {
    let source = format!(
        "```rust\nlet x = 1;\n{}```",
        "// pad\n".repeat(MAX_HIGHLIGHT_BYTES / 7 + 1)
    );
    let lines = markdown_lines(&source, 40, true);

    assert_eq!(style_of(&lines, "let"), Some(surface(normal())));
    assert_eq!(style_of(&lines, "// pad"), Some(surface(normal())));
}

#[test]
fn diff_fences_keep_diff_coloring_when_highlighting_is_enabled() {
    let source = "```diff\n@@ -1 +1 @@\n-old line\n+new line\n```";
    let lines = markdown_lines(source, 30, true);

    assert_eq!(style_of(&lines, "@@ -1 +1 @@"), Some(surface(muted())));
    assert_eq!(
        style_of(&lines, "-old line"),
        Some(surface(diff_line_style("-")))
    );
    assert_eq!(
        style_of(&lines, "+new line"),
        Some(surface(diff_line_style("+")))
    );
}

#[test]
fn headings_get_a_blank_line_above_and_lists_stay_tight() {
    let rows = frame_rows(&markdown_lines(
        "intro\n# Title\n- alpha\n- beta",
        40,
        false,
    ));

    assert_eq!(rows, ["intro", "", "Title", "- alpha", "- beta"]);
}

#[test]
fn markdown_entities_cannot_emit_terminal_controls() {
    let lines = markdown_lines("&#27;]52;c;Y2xpcGJvYXJk&#7;", 80, false);
    assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
        span.text
            .chars()
            .all(|character| terminal_safe_character(character) == Some(character))
    }));
}

mod settled_prefix_tests {
    use super::*;

    /// Render the prefix and suffix independently and concatenate them the
    /// way the live renderer does.
    fn split_render(source: &str, width: usize) -> Vec<Line> {
        let split = settled_prefix_end(source);
        let mut lines = markdown_lines(&source[..split], width, false);
        lines.extend(markdown_lines(&source[split..], width, false));
        lines
    }

    const CORPUS: &[&str] = &[
        "plain paragraph without a boundary",
        "first paragraph\n\nsecond paragraph",
        "first paragraph\n\nsecond paragraph\n\nthird",
        "# Heading\n\ntext\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```\n\ntail",
        "```rust\nlet x = 1;\n\nlet y = 2;\n```\n\nafter",
        "```\nunterminated\n\nstill code",
        "~~~\ntilde fence\n\n~~~\n\nafter tilde",
        "````\n```\nnested\n```\n````\n\nafter nested",
        "| a | b |\n| - | - |\n| 1 | 2 |\n\nafter table",
        "> quoted\n> more\n\nafter quote",
        "    indented code\n\n    still code\n\nafter",
        "1. one\n2. two\n\n   continued\n\nafter list",
        "trailing blank\n\n",
        "\n\nleading blank",
        "a\n\n\n\nb",
        "text **bold\n\nstill open",
    ];

    #[test]
    fn splitting_at_the_settled_prefix_does_not_change_layout() {
        for source in CORPUS {
            for width in [12, 40, 80] {
                let whole = markdown_lines(source, width, false);
                let split = split_render(source, width);
                assert_eq!(split, whole, "source={source:?} width={width}");
            }
        }
    }

    #[test]
    fn settled_prefix_never_splits_inside_a_fence() {
        let source = "```\ncode\n\nmore code\n";
        assert_eq!(settled_prefix_end(source), 0);
        let source = "before\n\n```\ncode\n\nmore\n```\n\nafter";
        assert_eq!(
            &source[..settled_prefix_end(source)],
            "before\n\n```\ncode\n\nmore\n```\n\n"
        );
    }

    #[test]
    fn settled_prefix_grows_monotonically_as_text_streams() {
        let full =
            "# Heading\n\npara one\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\npara two\n\nlast";
        let mut previous = 0;
        for end in 0..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let settled = settled_prefix_end(&full[..end]);
            assert!(
                settled >= previous,
                "end={end} settled={settled} previous={previous}"
            );
            assert!(settled <= end);
            previous = settled;
        }
    }
}
