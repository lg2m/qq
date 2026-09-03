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

/// One resolved theme: a name for the picker and its role colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
}

/// The role colors. `Copy` so a frame can snapshot it for free. The first
/// eight are what a theme file declares; the rest are derived from them
/// unless the theme overrides them.
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
    /// A second surface a step further from the background, for rules and
    /// the composer frame.
    pub surface_alt: Color,
    /// Background of the selected row in pickers and the sidebar.
    pub selection_bg: Color,
    /// Rules and dividers.
    pub border: Color,
    /// Background tint behind added and removed diff lines.
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    /// Running-state color: spinners and "working" labels.
    pub info: Color,
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
        surface_alt: Color::Rgb {
            r: 48,
            g: 51,
            b: 61,
        },
        selection_bg: Color::Rgb {
            r: 48,
            g: 51,
            b: 61,
        },
        border: Color::DarkGrey,
        diff_add_bg: Color::Rgb {
            r: 28,
            g: 52,
            b: 36,
        },
        diff_del_bg: Color::Rgb {
            r: 60,
            g: 30,
            b: 34,
        },
        info: Color::Cyan,
    };

    /// Fill the derived roles from the eight declared ones: the selection and
    /// alternate surface lift the surface a step, borders come from muted and
    /// accent, diff tints from success and error at low intensity, and info
    /// follows accent.
    #[must_use]
    pub fn derive(roles: [Color; 8]) -> Self {
        let [text, muted, accent, brand, warning, error, success, surface] = roles;
        let lift = |color: Color, amount: u8| match color {
            Color::Rgb { r, g, b } => Color::Rgb {
                r: r.saturating_add(amount),
                g: g.saturating_add(amount),
                b: b.saturating_add(amount),
            },
            other => other,
        };
        let tint = |color: Color, base: Color| match (color, base) {
            (
                Color::Rgb { r, g, b },
                Color::Rgb {
                    r: br,
                    g: bg,
                    b: bb,
                },
            ) => Color::Rgb {
                r: ((u16::from(r) + u16::from(br) * 3) / 4) as u8,
                g: ((u16::from(g) + u16::from(bg) * 3) / 4) as u8,
                b: ((u16::from(b) + u16::from(bb) * 3) / 4) as u8,
            },
            (_, base) => base,
        };
        Self {
            text,
            muted,
            accent,
            brand,
            warning,
            error,
            success,
            surface,
            surface_alt: lift(surface, 10),
            selection_bg: lift(surface, 10),
            border: muted,
            diff_add_bg: tint(success, surface),
            diff_del_bg: tint(error, surface),
            info: accent,
        }
    }
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
            palette: Palette::derive([
                text.into(),
                muted.into(),
                accent.into(),
                brand.into(),
                warning.into(),
                error.into(),
                success.into(),
                surface.into(),
            ]),
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
