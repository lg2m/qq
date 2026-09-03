//! The active color theme. The renderer paints with eight semantic roles;
//! a theme maps each to a terminal color (see `docs/design/theme.md`).
//!
//! Style helpers in `render.rs` are called hundreds of times per frame, so
//! the active palette is a `Copy` value in a thread-local read by each
//! helper, refreshed from the shared slot once per frame by the renderer.
//! Switching themes bumps a generation the renderer compares to know when
//! to drop every cached layout and repaint every row.

use std::cell::Cell;

use crossterm::style::Color;

/// One resolved theme: a name for the picker and its eight role colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

/// The eight role colors. `Copy` so a frame can snapshot it for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub brand: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
    pub surface: Color,
}

impl Palette {
    /// The compiled `qq` palette: the look the renderer shipped with.
    pub const QQ: Self = Self {
        text: Color::White,
        muted: Color::DarkGrey,
        accent: Color::Cyan,
        brand: Color::Rgb {
            r: 255,
            g: 159,
            b: 67,
        },
        warning: Color::Yellow,
        error: Color::Red,
        success: Color::Green,
        surface: Color::Rgb {
            r: 38,
            g: 40,
            b: 48,
        },
    };
}

impl Default for Palette {
    fn default() -> Self {
        Self::QQ
    }
}

/// A role color as the composition root supplies it, free of any terminal
/// library type. `Palette` converts it to the renderer's color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColor {
    /// The terminal's own palette entry; follows the user's terminal theme.
    White,
    DarkGrey,
    Cyan,
    Yellow,
    Red,
    Green,
    /// A fixed 24-bit color.
    Rgb(u8, u8, u8),
}

impl From<ThemeColor> for Color {
    fn from(color: ThemeColor) -> Self {
        match color {
            ThemeColor::White => Self::White,
            ThemeColor::DarkGrey => Self::DarkGrey,
            ThemeColor::Cyan => Self::Cyan,
            ThemeColor::Yellow => Self::Yellow,
            ThemeColor::Red => Self::Red,
            ThemeColor::Green => Self::Green,
            ThemeColor::Rgb(r, g, b) => Self::Rgb { r, g, b },
        }
    }
}

/// Eight role colors in the order of `docs/design/theme.md`: text, muted,
/// accent, brand, warning, error, success, surface.
pub type ThemeRoles = [ThemeColor; 8];

impl Theme {
    #[must_use]
    pub fn qq() -> Self {
        Self {
            name: "qq".to_owned(),
            palette: Palette::QQ,
        }
    }

    /// A theme from resolved role colors.
    #[must_use]
    pub fn from_roles(name: impl Into<String>, roles: ThemeRoles) -> Self {
        let [text, muted, accent, brand, warning, error, success, surface] = roles;
        Self {
            name: name.into(),
            palette: Palette {
                text: text.into(),
                muted: muted.into(),
                accent: accent.into(),
                brand: brand.into(),
                warning: warning.into(),
                error: error.into(),
                success: success.into(),
                surface: surface.into(),
            },
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::qq()
    }
}

thread_local! {
    static ACTIVE: Cell<Palette> = const { Cell::new(Palette::QQ) };
}

/// Install `palette` for style helpers on this thread. The renderer calls
/// this at the top of every frame; tests call it to render under a theme.
pub(crate) fn activate(palette: Palette) {
    ACTIVE.with(|active| active.set(palette));
}

/// The palette style helpers read. One thread-local load, no lock.
pub(crate) fn active() -> Palette {
    ACTIVE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_palette_is_the_compiled_qq_look() {
        assert_eq!(Theme::default().name, "qq");
        assert_eq!(Palette::default(), Palette::QQ);
        assert_eq!(active(), Palette::QQ);
    }

    #[test]
    fn activation_is_per_thread_and_repeatable() {
        let custom = Palette {
            accent: Color::Magenta,
            ..Palette::QQ
        };
        activate(custom);
        assert_eq!(active().accent, Color::Magenta);
        std::thread::spawn(|| assert_eq!(active(), Palette::QQ))
            .join()
            .unwrap();
        activate(Palette::QQ);
        assert_eq!(active(), Palette::QQ);
    }
}
