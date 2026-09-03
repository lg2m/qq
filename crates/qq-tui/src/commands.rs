//! The single table of user-invocable commands.
//!
//! Every command surface — slash autocomplete, key chords, the command
//! palette, the help overlay, and footer hints — reads this table. Adding a
//! command means adding one row here and one arm in `App::execute`. Slash
//! names remain reserved in `qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS`; a
//! test keeps the two in agreement without coupling them by array index.
//!
//! Chords listed here are defaults. A command with an [`Action`] can be
//! rebound through `tui.ron`; `Settings` overrides the table for those.

use crossterm::event::KeyEvent;

use crate::settings::{Action, KeyChord, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Command {
    OpenHelp,
    OpenCommands,
    OpenModels,
    OpenThemes,
    OpenSessions,
    OpenAgents,
    ToggleSessions,
    NewRootSession,
    NewChildSession,
    CompactSession,
    CancelRun,
    SelectThreadline,
    SelectFoldFocus,
    NextLayout,
    PreviousLayout,
    ToggleToolDetail,
    /// Move the transcript cursor to the previous / next tool call of the
    /// focused session; Enter then expands or collapses that call alone.
    CursorUp,
    CursorDown,
    ToggleReasoning,
    ToggleSidebar,
    ToggleMouse,
    FocusParent,
    FocusFirstChild,
    FocusNextSibling,
    FocusPreviousSibling,
    FocusNextApproval,
    /// Hold the composer text locally until the active run finishes.
    QueueDraft,
    /// Pull the newest locally queued draft back into the composer.
    DequeueDraft,
    /// Add the composer text to the active run at its next model/tool
    /// boundary. Available only while the server advertises boundary
    /// steering; `Submit` falls back to queueing otherwise.
    SteerRun,
    /// Abort the active run's in-flight turn now and steer it with the
    /// composer text. Available only while the server advertises interrupt
    /// steering.
    InterruptRun,
    /// Edit the draft in `$VISUAL` or `$EDITOR`.
    OpenEditor,
    /// Delete every empty session in the workspace after confirmation.
    PruneSessions,
    /// Split the focused pane; the new pane sits beside it.
    SplitBeside,
    /// Split the focused pane; the new pane sits below it.
    SplitBelow,
    ClosePane,
    /// Show only the focused pane until toggled again.
    ZoomPane,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    /// Move the divider enclosing the focused pane one step.
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Category {
    Help,
    Session,
    Run,
    Model,
    View,
    Panes,
    Compose,
    System,
}

impl Category {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Help => "HELP",
            Self::Session => "SESSIONS",
            Self::Run => "RUN",
            Self::Model => "MODEL",
            Self::View => "VIEW",
            Self::Panes => "PANES",
            Self::Compose => "COMPOSER",
            Self::System => "SYSTEM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub command: Command,
    pub title: &'static str,
    pub category: Category,
    /// Slash spellings that invoke the command from the composer. The first
    /// entry is canonical; the rest are aliases shown as separate rows so a
    /// user typing either prefix sees a match.
    pub slash: &'static [&'static str],
    /// Default chords in compose mode, in `KeyChord` syntax. The first is the
    /// one shown in hints.
    pub chords: &'static [&'static str],
    /// Configurable keybinding action that triggers this command, if any.
    /// When set, `Settings` chords replace `chords`.
    pub action: Option<Action>,
}

macro_rules! spec {
    ($command:ident, $title:literal, $category:ident, [$($slash:literal),*], [$($chord:literal),*] $(, $action:ident)?) => {
        CommandSpec {
            command: Command::$command,
            title: $title,
            category: Category::$category,
            slash: &[$($slash),*],
            chords: &[$($chord),*],
            action: spec!(@action $($action)?),
        }
    };
    (@action) => { None };
    (@action $action:ident) => { Some(Action::$action) };
}

/// Presentation order is invocation frequency within a category, and the
/// palette shows categories in this order too.
pub(crate) const COMMANDS: [CommandSpec; 45] = [
    spec!(
        OpenHelp,
        "show every command and key",
        Help,
        ["/help"],
        ["F1"]
    ),
    spec!(
        OpenCommands,
        "open the command palette",
        Help,
        ["/commands"],
        ["Ctrl-K"]
    ),
    spec!(
        OpenSessions,
        "open sessions",
        Session,
        ["/sessions", "/resume"],
        []
    ),
    spec!(
        OpenAgents,
        "open the focused session's agent tree",
        Session,
        ["/agents"],
        []
    ),
    spec!(
        ToggleSessions,
        "toggle the session navigator",
        Session,
        [],
        [],
        ToggleNavigator
    ),
    spec!(
        NewRootSession,
        "create a session",
        Session,
        ["/new"],
        [],
        CreateRootSession
    ),
    spec!(
        NewChildSession,
        "create a child session",
        Session,
        [],
        [],
        CreateChildSession
    ),
    spec!(
        CompactSession,
        "compact session context",
        Session,
        ["/compact"],
        []
    ),
    spec!(
        PruneSessions,
        "delete every empty session",
        Session,
        ["/prune"],
        []
    ),
    spec!(FocusParent, "focus the parent session", Session, [], []),
    spec!(
        FocusFirstChild,
        "focus the first child session",
        Session,
        [],
        ["Alt-Down"]
    ),
    spec!(
        FocusNextSibling,
        "focus the next sibling session",
        Session,
        [],
        ["Alt-Right"]
    ),
    spec!(
        FocusPreviousSibling,
        "focus the previous sibling session",
        Session,
        [],
        ["Alt-Left"]
    ),
    spec!(
        FocusNextApproval,
        "jump to the next session awaiting approval",
        Session,
        [],
        ["Ctrl-G"]
    ),
    spec!(CancelRun, "cancel the active run", Run, [], [], CancelRun),
    spec!(SteerRun, "steer the active run with the draft", Run, [], []),
    spec!(
        InterruptRun,
        "interrupt the active run and steer it with the draft",
        Run,
        [],
        [],
        InterruptRun
    ),
    spec!(
        QueueDraft,
        "queue the draft until the run finishes",
        Run,
        [],
        ["Ctrl-Enter", "Ctrl-Q"]
    ),
    spec!(
        DequeueDraft,
        "edit the newest queued draft",
        Run,
        [],
        ["Alt-Up"]
    ),
    spec!(OpenModels, "choose a model", Model, ["/models"], []),
    spec!(OpenThemes, "choose a theme", View, ["/theme"], []),
    spec!(
        SelectThreadline,
        "threadline layout",
        View,
        [],
        [],
        SelectThreadline
    ),
    spec!(
        SelectFoldFocus,
        "fold-focus layout",
        View,
        [],
        [],
        SelectFoldFocus
    ),
    spec!(NextLayout, "next layout", View, ["/layout"], [], NextLayout),
    spec!(
        PreviousLayout,
        "previous layout",
        View,
        [],
        [],
        PreviousLayout
    ),
    spec!(
        ToggleToolDetail,
        "toggle tool call detail",
        View,
        [],
        ["Ctrl-O"]
    ),
    spec!(
        CursorUp,
        "select the previous tool call",
        View,
        [],
        ["Ctrl-Up"]
    ),
    spec!(
        CursorDown,
        "select the next tool call",
        View,
        [],
        ["Ctrl-Down"]
    ),
    spec!(
        ToggleReasoning,
        "toggle reasoning detail",
        View,
        [],
        ["Ctrl-R"]
    ),
    spec!(
        ToggleSidebar,
        "toggle the session sidebar",
        View,
        [],
        ["Ctrl-\\"]
    ),
    spec!(ToggleMouse, "toggle mouse capture", View, ["/mouse"], []),
    spec!(
        SplitBeside,
        "split the pane side by side",
        Panes,
        ["/split"],
        ["Alt-\\"]
    ),
    spec!(
        SplitBelow,
        "split the pane top and bottom",
        Panes,
        ["/stack"],
        ["Alt--"]
    ),
    spec!(
        ClosePane,
        "close the focused pane",
        Panes,
        ["/close"],
        ["Alt-W"]
    ),
    spec!(
        ZoomPane,
        "toggle showing only the focused pane",
        Panes,
        ["/zoom"],
        ["Alt-Z"]
    ),
    spec!(
        FocusPaneLeft,
        "focus the pane to the left",
        Panes,
        [],
        ["Alt-H"]
    ),
    spec!(
        FocusPaneRight,
        "focus the pane to the right",
        Panes,
        [],
        ["Alt-L"]
    ),
    spec!(FocusPaneUp, "focus the pane above", Panes, [], ["Alt-K"]),
    spec!(FocusPaneDown, "focus the pane below", Panes, [], ["Alt-J"]),
    spec!(
        ResizePaneLeft,
        "move the pane divider left",
        Panes,
        [],
        ["Alt-Shift-H"]
    ),
    spec!(
        ResizePaneRight,
        "move the pane divider right",
        Panes,
        [],
        ["Alt-Shift-L"]
    ),
    spec!(
        ResizePaneUp,
        "move the pane divider up",
        Panes,
        [],
        ["Alt-Shift-K"]
    ),
    spec!(
        ResizePaneDown,
        "move the pane divider down",
        Panes,
        [],
        ["Alt-Shift-J"]
    ),
    spec!(
        OpenEditor,
        "edit the draft in $EDITOR",
        Compose,
        ["/editor"],
        ["Alt-E"]
    ),
    spec!(Quit, "exit QQ", System, ["/quit", "/exit"], ["Ctrl-C"]),
];

/// One slash spelling of a command, as shown in the autocomplete list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashEntry {
    pub name: &'static str,
    pub title: &'static str,
    pub command: Command,
}

/// Every slash spelling in presentation order.
pub(crate) fn slash_entries() -> impl Iterator<Item = SlashEntry> {
    COMMANDS.iter().flat_map(|spec| {
        spec.slash.iter().map(move |name| SlashEntry {
            name,
            title: spec.title,
            command: spec.command,
        })
    })
}

/// Slash entries matching `token` as a subsequence after the `/`. `token`
/// must start with `/` and contain no whitespace, otherwise nothing matches:
/// a slash token followed by arguments is a prompt for the runtime, not a
/// client command. Prefix matches sort first so `/s` still lists `/sessions`
/// ahead of `/models`.
pub(crate) fn matching_slash_entries(token: &str) -> Vec<SlashEntry> {
    if !token.starts_with('/') || token.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let query = &token[1..];
    let mut prefix = Vec::new();
    let mut fuzzy = Vec::new();
    for entry in slash_entries() {
        let name = &entry.name[1..];
        if name.starts_with(query) {
            prefix.push(entry);
        } else if crate::picker::fuzzy_matches(query, name) {
            fuzzy.push(entry);
        }
    }
    prefix.extend(fuzzy);
    prefix
}

/// The specification row for `command`.
pub(crate) fn spec(command: Command) -> &'static CommandSpec {
    COMMANDS
        .iter()
        .find(|spec| spec.command == command)
        .expect("every command has a registry row")
}

/// The command bound to a configurable keybinding action.
pub(crate) fn command_for_action(action: Action) -> Command {
    COMMANDS
        .iter()
        .find(|spec| spec.action == Some(action))
        .map(|spec| spec.command)
        .expect("every keybinding action has a command row")
}

/// The command `key` invokes in compose mode: a configured action first,
/// then a default chord from the table.
pub(crate) fn command_for_key(settings: &Settings, key: KeyEvent) -> Option<Command> {
    if let Some(action) = settings.action_for(key) {
        return Some(command_for_action(action));
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.action.is_none())
        .find(|spec| {
            spec.chords
                .iter()
                .any(|chord| default_chord(chord).matches(key))
        })
        .map(|spec| spec.command)
}

/// The chord shown for `command` in hints and the palette: the configured
/// one for actions, otherwise the first default.
pub(crate) fn chord_label(settings: &Settings, command: Command) -> Option<String> {
    let spec = spec(command);
    match spec.action {
        Some(action) => settings.binding_label(action),
        None => spec
            .chords
            .first()
            .map(|chord| default_chord(chord).to_string()),
    }
}

fn default_chord(chord: &str) -> KeyChord {
    chord
        .parse()
        .unwrap_or_else(|error| panic!("default chord {chord:?} is valid: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn slash_names_match_the_protocol_reservation_exactly() {
        let here: BTreeSet<&str> = slash_entries().map(|entry| entry.name).collect();
        let reserved: BTreeSet<&str> = qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS
            .iter()
            .copied()
            .collect();
        assert_eq!(here, reserved);
    }

    #[test]
    fn every_action_has_exactly_one_command() {
        for action in Action::all() {
            let rows = COMMANDS
                .iter()
                .filter(|spec| spec.action == Some(action))
                .count();
            assert_eq!(rows, 1, "{action:?}");
        }
    }

    #[test]
    fn every_command_has_one_row_reachable_from_the_palette_and_a_way_in() {
        // Commands whose only direct key is contextual (Enter steers while a
        // run is active; Esc walks to the parent) are reachable from the
        // palette, which lists every row.
        let contextual = [Command::SteerRun, Command::FocusParent];
        let mut seen = BTreeSet::new();
        for spec in &COMMANDS {
            assert!(seen.insert(spec.command), "{:?} listed twice", spec.command);
            assert!(!spec.title.is_empty());
            let bound = spec.action.is_some()
                || !spec.chords.is_empty()
                || !spec.slash.is_empty()
                || contextual.contains(&spec.command);
            assert!(bound, "{:?} has no chord, action, or slash", spec.command);
        }
    }

    #[test]
    fn default_chords_parse_and_do_not_collide() {
        let mut chords: Vec<(KeyChord, Command)> = Vec::new();
        for spec in &COMMANDS {
            for chord in spec.chords {
                let parsed = default_chord(chord);
                if let Some((_, other)) = chords.iter().find(|(existing, _)| *existing == parsed) {
                    panic!("{chord} bound to both {other:?} and {:?}", spec.command);
                }
                chords.push((parsed, spec.command));
            }
        }
    }

    #[test]
    fn slash_matching_requires_a_bare_slash_token() {
        assert!(matching_slash_entries("").is_empty());
        assert!(matching_slash_entries("hello").is_empty());
        assert!(matching_slash_entries("/new ").is_empty());
        assert!(matching_slash_entries("/new arg").is_empty());
        let names: Vec<_> = matching_slash_entries("/")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names.len(),
            qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS.len()
        );
        assert_eq!(names[0], "/help");
        let quit: Vec<_> = matching_slash_entries("/qu")
            .into_iter()
            .map(|entry| entry.command)
            .collect();
        assert_eq!(quit, vec![Command::Quit]);
    }

    #[test]
    fn slash_matching_prefers_prefixes_then_falls_back_to_subsequences() {
        let names: Vec<_> = matching_slash_entries("/s")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names[0], "/sessions", "prefix matches lead");
        assert!(names.contains(&"/models"), "subsequence matches follow");
        let names: Vec<_> = matching_slash_entries("/mdl")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, vec!["/models"]);
    }
}
