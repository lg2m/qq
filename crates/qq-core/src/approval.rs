use std::collections::HashSet;

use qq_protocol::ApprovalMode;
use serde::Deserialize;

pub(crate) const POLICY_DENIED_RESULT: &str =
    "This session's approval mode is read-only; the tool call was denied without prompting.";
pub(crate) const USER_DENIED_RESULT: &str = "The user denied this tool call.";
pub(crate) const TIMEOUT_DENIED_RESULT: &str = "No client resolved this tool approval within the configured wait; the call was denied by timeout.";
pub(crate) const UNATTENDED_DENIED_RESULT: &str =
    "Tool approval is unavailable for this run; the call was denied.";

/// How one requested tool call relates to the workspace and the outside world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolClass {
    ReadOnly,
    Mutating,
    Shell {
        command: String,
        cwd: Option<String>,
    },
    Mcp,
    /// Names no policy rule recognizes execute directly so the dispatcher can
    /// return its precise unknown-tool error to the model.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
    Execute,
    RequireApproval,
    Deny,
}

/// Session-scoped approvals recorded by approve-for-session decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionGrants {
    pub(crate) tools: HashSet<String>,
    pub(crate) shell_prefixes: Vec<String>,
}

impl SessionGrants {
    fn covers(&self, name: &str, class: &ToolClass) -> bool {
        if self.tools.contains(name) {
            return true;
        }
        match class {
            ToolClass::Shell { command, .. } => self
                .shell_prefixes
                .iter()
                .any(|prefix| shell_prefix_matches(prefix, command)),
            ToolClass::ReadOnly | ToolClass::Mutating | ToolClass::Mcp | ToolClass::Unknown => {
                false
            }
        }
    }
}

/// Matches an allowlisted prefix against a shell command at word granularity,
/// so "cargo test" covers "cargo test -p qq-core" but not "cargo testify".
pub(crate) fn shell_prefix_matches(prefix: &str, command: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return false;
    }
    let command = command.trim_start();
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

pub(crate) fn classify(name: &str, arguments: &str) -> ToolClass {
    match name {
        "read_file" | "list_dir" | "search" => ToolClass::ReadOnly,
        "edit_file" | "write_file" => ToolClass::Mutating,
        "shell" => shell_class(arguments),
        #[cfg(test)]
        "__test_delay" => ToolClass::ReadOnly,
        #[cfg(test)]
        "__test_mutate" => ToolClass::Mutating,
        #[cfg(test)]
        "__test_shell" => shell_class(arguments),
        _ if name.starts_with("mcp__") => ToolClass::Mcp,
        _ => ToolClass::Unknown,
    }
}

fn shell_class(arguments: &str) -> ToolClass {
    #[derive(Deserialize)]
    struct ShellArguments {
        #[serde(default)]
        command: String,
        #[serde(default)]
        cwd: Option<String>,
    }
    match serde_json::from_str::<ShellArguments>(arguments) {
        Ok(arguments) => ToolClass::Shell {
            command: arguments.command,
            cwd: arguments.cwd,
        },
        Err(_) => ToolClass::Shell {
            command: String::new(),
            cwd: None,
        },
    }
}

/// Decides whether one classified tool call executes, waits for approval, or
/// is denied outright under the session's approval mode and recorded grants.
pub(crate) fn evaluate(
    mode: ApprovalMode,
    name: &str,
    class: &ToolClass,
    grants: &SessionGrants,
) -> PolicyDecision {
    match class {
        ToolClass::ReadOnly | ToolClass::Unknown => PolicyDecision::Execute,
        ToolClass::Mutating | ToolClass::Shell { .. } | ToolClass::Mcp => match mode {
            ApprovalMode::ReadOnly => PolicyDecision::Deny,
            ApprovalMode::Ask => {
                if grants.covers(name, class) {
                    PolicyDecision::Execute
                } else {
                    PolicyDecision::RequireApproval
                }
            }
            ApprovalMode::Auto => {
                if matches!(class, ToolClass::Mutating) || grants.covers(name, class) {
                    PolicyDecision::Execute
                } else {
                    PolicyDecision::RequireApproval
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(tools: &[&str], prefixes: &[&str]) -> SessionGrants {
        SessionGrants {
            tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
            shell_prefixes: prefixes.iter().map(|prefix| (*prefix).to_owned()).collect(),
        }
    }

    fn shell(command: &str) -> ToolClass {
        ToolClass::Shell {
            command: command.to_owned(),
            cwd: None,
        }
    }

    #[test]
    fn read_only_tools_never_require_approval_in_any_mode() {
        for mode in [
            ApprovalMode::ReadOnly,
            ApprovalMode::Ask,
            ApprovalMode::Auto,
        ] {
            assert_eq!(
                evaluate(mode, "read_file", &ToolClass::ReadOnly, &grants(&[], &[])),
                PolicyDecision::Execute
            );
            assert_eq!(
                evaluate(mode, "no_such_tool", &ToolClass::Unknown, &grants(&[], &[])),
                PolicyDecision::Execute
            );
        }
    }

    #[test]
    fn read_only_mode_denies_everything_else_without_prompting() {
        for (name, class) in [
            ("write_file", ToolClass::Mutating),
            ("shell", shell("cargo test")),
            ("mcp__server__tool", ToolClass::Mcp),
        ] {
            assert_eq!(
                evaluate(
                    ApprovalMode::ReadOnly,
                    name,
                    &class,
                    &grants(&[name], &["cargo"]),
                ),
                PolicyDecision::Deny,
                "read-only must deny {name} even when granted"
            );
        }
    }

    #[test]
    fn ask_mode_requires_approval_unless_granted() {
        for (name, class) in [
            ("edit_file", ToolClass::Mutating),
            ("shell", shell("cargo test")),
            ("mcp__server__tool", ToolClass::Mcp),
        ] {
            assert_eq!(
                evaluate(ApprovalMode::Ask, name, &class, &grants(&[], &[])),
                PolicyDecision::RequireApproval
            );
        }
        assert_eq!(
            evaluate(
                ApprovalMode::Ask,
                "edit_file",
                &ToolClass::Mutating,
                &grants(&["edit_file"], &[]),
            ),
            PolicyDecision::Execute
        );
        assert_eq!(
            evaluate(
                ApprovalMode::Ask,
                "shell",
                &shell("cargo test -p qq-core"),
                &grants(&[], &["cargo test"]),
            ),
            PolicyDecision::Execute
        );
    }

    #[test]
    fn auto_mode_runs_edits_and_allowlisted_shell_but_still_asks_otherwise() {
        assert_eq!(
            evaluate(
                ApprovalMode::Auto,
                "write_file",
                &ToolClass::Mutating,
                &grants(&[], &[]),
            ),
            PolicyDecision::Execute
        );
        assert_eq!(
            evaluate(
                ApprovalMode::Auto,
                "shell",
                &shell("cargo test"),
                &grants(&[], &["cargo test"]),
            ),
            PolicyDecision::Execute
        );
        assert_eq!(
            evaluate(
                ApprovalMode::Auto,
                "shell",
                &shell("rm -rf /"),
                &grants(&[], &["cargo test"]),
            ),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            evaluate(
                ApprovalMode::Auto,
                "mcp__server__tool",
                &ToolClass::Mcp,
                &grants(&[], &[]),
            ),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn shell_prefixes_match_whole_words_only() {
        assert!(shell_prefix_matches("cargo test", "cargo test"));
        assert!(shell_prefix_matches("cargo test", "cargo test --workspace"));
        assert!(shell_prefix_matches("cargo", "  cargo build"));
        assert!(!shell_prefix_matches("cargo test", "cargo testify"));
        assert!(!shell_prefix_matches("cargo test", "cargo"));
        assert!(!shell_prefix_matches("", "anything"));
    }

    #[test]
    fn classification_reads_shell_arguments_and_namespaces() {
        assert_eq!(classify("search", "{}"), ToolClass::ReadOnly);
        assert_eq!(classify("edit_file", "{}"), ToolClass::Mutating);
        assert_eq!(
            classify("shell", r#"{"command":"cargo test","cwd":"crates"}"#),
            ToolClass::Shell {
                command: "cargo test".to_owned(),
                cwd: Some("crates".to_owned()),
            }
        );
        assert_eq!(classify("mcp__github__create_issue", "{}"), ToolClass::Mcp);
        assert_eq!(classify("mystery", "{}"), ToolClass::Unknown);
    }
}
