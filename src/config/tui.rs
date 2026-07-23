use std::{collections::BTreeMap, path::Path};

use qq_tui::{Action, KeyChord, Layout, Settings, SettingsBuilder};
use ron::{Options, extensions::Extensions};
use serde::Deserialize;

use super::{
    ConfigError, ConfigLoader, SourceIdentity, SourceKind,
    loader::{canonical_working_directory, discover_file, project_directories, read_candidate},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiConfigKey {
    Layout,
    Binding(Action),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiSourceReport {
    source: SourceIdentity,
    touched: Vec<TuiConfigKey>,
}

impl TuiSourceReport {
    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub fn touched(&self) -> &[TuiConfigKey] {
        &self.touched
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigProvenance {
    layout: SourceIdentity,
    bindings: BTreeMap<Action, SourceIdentity>,
}

impl TuiConfigProvenance {
    #[must_use]
    pub const fn layout(&self) -> &SourceIdentity {
        &self.layout
    }

    #[must_use]
    pub fn binding(&self, action: Action) -> &SourceIdentity {
        self.bindings
            .get(&action)
            .expect("every TUI action has a compiled default")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfigSnapshot {
    settings: Settings,
    reports: Vec<TuiSourceReport>,
    provenance: TuiConfigProvenance,
}

impl TuiConfigSnapshot {
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    #[must_use]
    pub fn source_reports(&self) -> &[TuiSourceReport] {
        &self.reports
    }

    #[must_use]
    pub const fn provenance(&self) -> &TuiConfigProvenance {
        &self.provenance
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
enum ConfigLayout {
    Threadline,
    FoldFocus,
}

impl From<ConfigLayout> for Layout {
    fn from(value: ConfigLayout) -> Self {
        match value {
            ConfigLayout::Threadline => Self::Threadline,
            ConfigLayout::FoldFocus => Self::FoldFocus,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BindingsDocument {
    select_threadline: Option<Vec<String>>,
    select_fold_focus: Option<Vec<String>>,
    next_layout: Option<Vec<String>>,
    previous_layout: Option<Vec<String>>,
    toggle_navigator: Option<Vec<String>>,
    create_root_session: Option<Vec<String>>,
    create_child_session: Option<Vec<String>>,
    cancel_run: Option<Vec<String>>,
}

impl BindingsDocument {
    fn entries(&self) -> [(Action, Option<&[String]>); 8] {
        [
            (Action::SelectThreadline, self.select_threadline.as_deref()),
            (Action::SelectFoldFocus, self.select_fold_focus.as_deref()),
            (Action::NextLayout, self.next_layout.as_deref()),
            (Action::PreviousLayout, self.previous_layout.as_deref()),
            (Action::ToggleNavigator, self.toggle_navigator.as_deref()),
            (
                Action::CreateRootSession,
                self.create_root_session.as_deref(),
            ),
            (
                Action::CreateChildSession,
                self.create_child_session.as_deref(),
            ),
            (Action::CancelRun, self.cancel_run.as_deref()),
        ]
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    version: u32,
    layout: Option<ConfigLayout>,
    bindings: BindingsDocument,
}

impl Document {
    fn parse(content: &str, source: &SourceIdentity) -> Result<Self, ConfigError> {
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
        for (_, values) in document.bindings.entries() {
            let Some(values) = values else {
                continue;
            };
            for value in values {
                value
                    .parse::<KeyChord>()
                    .map_err(|error| ConfigError::Parse {
                        origin: source.clone(),
                        message: error.to_string(),
                    })?;
            }
        }
        Ok(document)
    }

    fn touched(&self) -> Vec<TuiConfigKey> {
        let mut touched = Vec::new();
        if self.layout.is_some() {
            touched.push(TuiConfigKey::Layout);
        }
        touched.extend(
            self.bindings
                .entries()
                .into_iter()
                .filter_map(|(action, values)| values.map(|_| TuiConfigKey::Binding(action))),
        );
        touched
    }
}

pub(super) fn load(loader: &ConfigLoader, cwd: &Path) -> Result<TuiConfigSnapshot, ConfigError> {
    let cwd = canonical_working_directory(cwd)?;
    let defaults = Settings::default();
    let compiled = SourceIdentity::virtual_source(SourceKind::Compiled, "compiled TUI defaults");
    let mut layout = defaults.initial_layout();
    let mut bindings: BTreeMap<_, _> = defaults
        .bindings()
        .iter()
        .map(|(action, chords)| (*action, chords.clone()))
        .collect();
    let mut provenance = TuiConfigProvenance {
        layout: compiled.clone(),
        bindings: bindings
            .keys()
            .map(|action| (*action, compiled.clone()))
            .collect(),
    };
    let mut reports = vec![TuiSourceReport {
        source: compiled,
        touched: std::iter::once(TuiConfigKey::Layout)
            .chain(bindings.keys().copied().map(TuiConfigKey::Binding))
            .collect(),
    }];

    let mut candidates = Vec::new();
    if let Some(global) = discover_file(
        loader.paths.global_dir.join("tui.ron"),
        SourceKind::Global,
        false,
    )? {
        candidates.push(global);
    }
    for directory in project_directories(&cwd) {
        if let Some(project) =
            discover_file(directory.join(".qq/tui.ron"), SourceKind::Project, false)?
        {
            candidates.push(project);
        }
    }

    for candidate in candidates {
        let (source, content) = read_candidate(&candidate)?;
        let document = Document::parse(&content, &source)?;
        if let Some(incoming) = document.layout {
            layout = incoming.into();
            provenance.layout = source.clone();
        }
        for (action, values) in document.bindings.entries() {
            let Some(values) = values else {
                continue;
            };
            let chords = values
                .iter()
                .map(|value| value.parse::<KeyChord>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ConfigError::Parse {
                    origin: source.clone(),
                    message: error.to_string(),
                })?;
            bindings.insert(action, chords);
            provenance.bindings.insert(action, source.clone());
        }
        reports.push(TuiSourceReport {
            source,
            touched: document.touched(),
        });
    }

    let mut builder = SettingsBuilder::default().initial_layout(layout);
    for (action, chords) in bindings {
        builder = builder.bindings(action, chords);
    }
    let settings = builder
        .build()
        .map_err(|error| ConfigError::InvalidTuiSettings {
            message: error.to_string(),
        })?;
    Ok(TuiConfigSnapshot {
        settings,
        reports,
        provenance,
    })
}
