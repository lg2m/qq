//! TUI theme documents: `<name>.ron` files that map the renderer's color
//! roles to concrete colors. See `docs/design/theme.md`.
//!
//! Themes are load-time configuration and fail fast: an unknown name, a
//! missing role, a bad literal, or an alias cycle is a configuration error
//! before the TUI starts. The compiled `qq` theme is always present.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

use ron::{Options, extensions::Extensions};
use serde::Deserialize;

use super::{
    ConfigError, ConfigLoader, SourceIdentity, SourceKind,
    loader::{
        Probes, canonical_working_directory, discover_file, project_directories, read_candidate,
    },
};

/// The compiled default theme name, always resolvable.
pub const DEFAULT_THEME: &str = "qq";

/// Themes shipped inside the binary as ordinary theme documents, so they go
/// through the same parser and errors as user files and double as
/// copy-and-tweak examples. Sorted by name; `qq` itself is built in code.
pub const COMPILED_THEMES: &[(&str, &str)] = &[
    ("catppuccin", include_str!("../themes/catppuccin.ron")),
    ("dracula", include_str!("../themes/dracula.ron")),
    ("ember", include_str!("../themes/ember.ron")),
    ("everforest", include_str!("../themes/everforest.ron")),
    ("gruvbox", include_str!("../themes/gruvbox.ron")),
    ("ink", include_str!("../themes/ink.ron")),
    ("kanagawa", include_str!("../themes/kanagawa.ron")),
    ("monokai", include_str!("../themes/monokai.ron")),
    ("nord", include_str!("../themes/nord.ron")),
    ("onedark", include_str!("../themes/onedark.ron")),
    ("rose-pine", include_str!("../themes/rose-pine.ron")),
    ("solarized", include_str!("../themes/solarized.ron")),
    ("tokyonight", include_str!("../themes/tokyonight.ron")),
];

/// Upper bound on theme files enumerated for the picker.
const MAX_DISCOVERED_THEMES: usize = 64;

/// A 24-bit color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    fn parse(literal: &str) -> Option<Self> {
        let hex = literal.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
        Some(Self {
            r: channel(0..2)?,
            g: channel(2..4)?,
            b: channel(4..6)?,
        })
    }
}

/// The standard terminal colors, which follow the user's terminal palette.
/// Only the compiled theme uses them; files are `#RRGGBB` so a theme looks
/// the same everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    White,
    DarkGrey,
    Cyan,
    Yellow,
    Red,
    Green,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeColor {
    Ansi(AnsiColor),
    Rgb(Rgb),
}

/// Every color role the renderer paints with, fully resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColors {
    pub text: ThemeColor,
    pub muted: ThemeColor,
    pub accent: ThemeColor,
    pub brand: ThemeColor,
    pub warning: ThemeColor,
    pub error: ThemeColor,
    pub success: ThemeColor,
    pub surface: ThemeColor,
}

/// A resolved theme with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeDocument {
    name: String,
    colors: ThemeColors,
    source: SourceIdentity,
}

impl ThemeDocument {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn colors(&self) -> &ThemeColors {
        &self.colors
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }
}

/// The compiled `qq` theme: the palette the renderer shipped with before
/// themes existed, so existing installs keep their look.
#[must_use]
pub fn compiled_theme() -> ThemeDocument {
    const fn rgb(r: u8, g: u8, b: u8) -> ThemeColor {
        ThemeColor::Rgb(Rgb { r, g, b })
    }
    ThemeDocument {
        name: DEFAULT_THEME.to_owned(),
        colors: ThemeColors {
            text: ThemeColor::Ansi(AnsiColor::White),
            muted: ThemeColor::Ansi(AnsiColor::DarkGrey),
            accent: ThemeColor::Ansi(AnsiColor::Cyan),
            brand: rgb(0xff, 0x9f, 0x43),
            warning: ThemeColor::Ansi(AnsiColor::Yellow),
            error: ThemeColor::Ansi(AnsiColor::Red),
            success: ThemeColor::Ansi(AnsiColor::Green),
            surface: rgb(0x26, 0x28, 0x30),
        },
        source: SourceIdentity::virtual_source(SourceKind::Compiled, "compiled theme qq"),
    }
}

/// Parse one of `COMPILED_THEMES`. A shipped document that fails to parse
/// is a build defect, surfaced as the same `ConfigError` a user file gets.
fn compiled_document(name: &str, content: &str) -> Result<ThemeDocument, ConfigError> {
    let source =
        SourceIdentity::virtual_source(SourceKind::Compiled, format!("compiled theme {name}"));
    let colors = Document::parse(content, &source)?;
    Ok(ThemeDocument {
        name: name.to_owned(),
        colors,
        source,
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesDocument {
    text: String,
    muted: String,
    accent: String,
    brand: String,
    warning: String,
    error: String,
    success: String,
    surface: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    #[serde(default)]
    defs: BTreeMap<String, String>,
    colors: RolesDocument,
}

impl Document {
    fn parse(content: &str, source: &SourceIdentity) -> Result<ThemeColors, ConfigError> {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| ConfigError::Parse {
                origin: source.clone(),
                message: error.to_string(),
            })?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: source.clone(),
                version: document.version,
            });
        }
        let resolve = |role: &str, value: &str| -> Result<ThemeColor, ConfigError> {
            // Follow aliases until a literal, refusing to revisit a name so
            // a cycle is an error rather than a hang.
            let mut seen = BTreeSet::new();
            let mut current = value;
            loop {
                if let Some(rgb) = Rgb::parse(current) {
                    return Ok(ThemeColor::Rgb(rgb));
                }
                if !seen.insert(current.to_owned()) {
                    return Err(ConfigError::Parse {
                        origin: source.clone(),
                        message: format!(
                            "theme role `{role}` has an alias cycle through `{current}`"
                        ),
                    });
                }
                match document.defs.get(current) {
                    Some(next) => current = next,
                    None => {
                        return Err(ConfigError::Parse {
                            origin: source.clone(),
                            message: format!(
                                "theme role `{role}` refers to `{current}`, which is neither a \
                                 `defs` alias nor a `#RRGGBB` literal"
                            ),
                        });
                    }
                }
            }
        };
        let roles = &document.colors;
        Ok(ThemeColors {
            text: resolve("text", &roles.text)?,
            muted: resolve("muted", &roles.muted)?,
            accent: resolve("accent", &roles.accent)?,
            brand: resolve("brand", &roles.brand)?,
            warning: resolve("warning", &roles.warning)?,
            error: resolve("error", &roles.error)?,
            success: resolve("success", &roles.success)?,
            surface: resolve("surface", &roles.surface)?,
        })
    }
}

/// Where a theme file may live, in resolution order after the compiled set.
fn theme_paths(
    loader: &ConfigLoader,
    cwd: &Path,
    probes: &mut Probes,
) -> Vec<(std::path::PathBuf, SourceKind)> {
    let mut directories = vec![(loader.paths.global_dir.join("themes"), SourceKind::Global)];
    for directory in project_directories(cwd, probes) {
        directories.push((directory.join(".qq/themes"), SourceKind::Project));
    }
    directories
}

/// Resolve theme `name`: the global `themes/` directory, then project
/// `.qq/themes/` directories nearest-last so the nearest wins, falling back
/// to the compiled set so a user file may shadow a shipped theme.
pub(super) fn load(
    loader: &ConfigLoader,
    cwd: &Path,
    name: &str,
) -> Result<ThemeDocument, ConfigError> {
    if name == DEFAULT_THEME {
        return Ok(compiled_theme());
    }
    validate_name(name)?;
    let cwd = canonical_working_directory(cwd)?;
    let mut probes = Probes::default();
    let mut found = None;
    for (directory, kind) in theme_paths(loader, &cwd, &mut probes) {
        if let Some(candidate) = discover_file(
            directory.join(format!("{name}.ron")),
            kind,
            false,
            &mut probes,
        )? {
            found = Some(candidate);
        }
    }
    let Some(candidate) = found else {
        return match COMPILED_THEMES
            .iter()
            .find(|(compiled, _)| *compiled == name)
        {
            Some((compiled, content)) => compiled_document(compiled, content),
            None => Err(ConfigError::UnknownTheme {
                name: name.to_owned(),
            }),
        };
    };
    let (source, content) = read_candidate(&candidate)?;
    let colors = Document::parse(&content, &source)?;
    Ok(ThemeDocument {
        name: name.to_owned(),
        colors,
        source,
    })
}

/// Every theme resolvable from `cwd`: the compiled set plus each valid
/// `*.ron` under the theme directories, nearest layer winning on a name.
/// Invalid files are skipped here (they fail loudly only when selected) so
/// one broken experiment does not hide the picker.
pub(super) fn discover(
    loader: &ConfigLoader,
    cwd: &Path,
) -> Result<Vec<ThemeDocument>, ConfigError> {
    let cwd = canonical_working_directory(cwd)?;
    let mut probes = Probes::default();
    let mut themes: BTreeMap<String, ThemeDocument> = BTreeMap::new();
    themes.insert(DEFAULT_THEME.to_owned(), compiled_theme());
    for (name, content) in COMPILED_THEMES {
        let document = compiled_document(name, content)?;
        themes.insert((*name).to_owned(), document);
    }
    for (directory, kind) in theme_paths(loader, &cwd, &mut probes) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("ron") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?.to_owned();
                validate_name(&stem).ok().map(|()| stem)
            })
            .collect();
        names.sort();
        for name in names.into_iter().take(MAX_DISCOVERED_THEMES) {
            if name == DEFAULT_THEME {
                continue;
            }
            let Some(candidate) = discover_file(
                directory.join(format!("{name}.ron")),
                kind,
                false,
                &mut probes,
            )?
            else {
                continue;
            };
            let Ok((source, content)) = read_candidate(&candidate) else {
                continue;
            };
            if let Ok(colors) = Document::parse(&content, &source) {
                themes.insert(
                    name.clone(),
                    ThemeDocument {
                        name,
                        colors,
                        source,
                    },
                );
            }
        }
    }
    Ok(themes.into_values().collect())
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ConfigError::UnknownTheme {
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}
