//! The single table of user-invocable commands.
//!
//! Every command surface — slash autocomplete today, the command palette and
//! footer hints later — reads this table. Adding a command means adding one
//! row here and one arm in `App::execute`. Slash names remain reserved in
//! `qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS`; a test keeps the two in
//! agreement without coupling them by array index.

use crate::settings::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Command {
    OpenModels,
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
    ToggleSidebar,
    FocusParent,
    FocusFirstChild,
    FocusNextSibling,
    FocusPreviousSibling,
    FocusNextApproval,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    Session,
    Model,
    View,
    Run,
    System,
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
    /// Configurable keybinding action that triggers this command, if any.
    pub action: Option<Action>,
}

/// Presentation order is invocation frequency, not alphabetical.
pub(crate) const COMMANDS: [CommandSpec; 20] = [
    CommandSpec {
        command: Command::OpenModels,
        title: "choose a model",
        category: Category::Model,
        slash: &["/models"],
        action: None,
    },
    CommandSpec {
        command: Command::OpenSessions,
        title: "open sessions",
        category: Category::Session,
        slash: &["/sessions", "/resume"],
        action: None,
    },
    CommandSpec {
        command: Command::OpenAgents,
        title: "open the focused session's agent tree",
        category: Category::Session,
        slash: &["/agents"],
        action: None,
    },
    CommandSpec {
        command: Command::ToggleSessions,
        title: "toggle the session navigator",
        category: Category::Session,
        slash: &[],
        action: Some(Action::ToggleNavigator),
    },
    CommandSpec {
        command: Command::NewRootSession,
        title: "create a session",
        category: Category::Session,
        slash: &["/new"],
        action: Some(Action::CreateRootSession),
    },
    CommandSpec {
        command: Command::NewChildSession,
        title: "create a child session",
        category: Category::Session,
        slash: &[],
        action: Some(Action::CreateChildSession),
    },
    CommandSpec {
        command: Command::CompactSession,
        title: "compact session context",
        category: Category::Session,
        slash: &["/compact"],
        action: None,
    },
    CommandSpec {
        command: Command::CancelRun,
        title: "cancel the active run",
        category: Category::Run,
        slash: &[],
        action: Some(Action::CancelRun),
    },
    CommandSpec {
        command: Command::SelectThreadline,
        title: "threadline layout",
        category: Category::View,
        slash: &[],
        action: Some(Action::SelectThreadline),
    },
    CommandSpec {
        command: Command::SelectFoldFocus,
        title: "fold-focus layout",
        category: Category::View,
        slash: &[],
        action: Some(Action::SelectFoldFocus),
    },
    CommandSpec {
        command: Command::NextLayout,
        title: "next layout",
        category: Category::View,
        slash: &[],
        action: Some(Action::NextLayout),
    },
    CommandSpec {
        command: Command::PreviousLayout,
        title: "previous layout",
        category: Category::View,
        slash: &[],
        action: Some(Action::PreviousLayout),
    },
    CommandSpec {
        command: Command::ToggleToolDetail,
        title: "toggle tool call detail",
        category: Category::View,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::ToggleSidebar,
        title: "toggle the session sidebar",
        category: Category::View,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::FocusParent,
        title: "focus the parent session",
        category: Category::Session,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::FocusFirstChild,
        title: "focus the first child session",
        category: Category::Session,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::FocusNextSibling,
        title: "focus the next sibling session",
        category: Category::Session,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::FocusPreviousSibling,
        title: "focus the previous sibling session",
        category: Category::Session,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::FocusNextApproval,
        title: "jump to the next session awaiting approval",
        category: Category::Run,
        slash: &[],
        action: None,
    },
    CommandSpec {
        command: Command::Quit,
        title: "exit QQ",
        category: Category::System,
        slash: &["/quit", "/exit"],
        action: None,
    },
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

/// Slash entries whose name starts with `prefix`. `prefix` must start with
/// `/` and contain no whitespace, otherwise nothing matches: a slash token
/// followed by arguments is a prompt for the runtime, not a client command.
pub(crate) fn matching_slash_entries(prefix: &str) -> Vec<SlashEntry> {
    if !prefix.starts_with('/') || prefix.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    slash_entries()
        .filter(|entry| entry.name.starts_with(prefix))
        .collect()
}

/// The command bound to a configurable keybinding action.
pub(crate) fn command_for_action(action: Action) -> Command {
    COMMANDS
        .iter()
        .find(|spec| spec.action == Some(action))
        .map(|spec| spec.command)
        .expect("every keybinding action has a command row")
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
        assert_eq!(names[0], "/models");
        let quit: Vec<_> = matching_slash_entries("/q")
            .into_iter()
            .map(|entry| entry.command)
            .collect();
        assert_eq!(quit, vec![Command::Quit]);
    }
}
