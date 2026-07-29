use std::collections::HashSet;

use qq_protocol::{ApprovalMode, EditPreview, ToolCallDisplay};
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

const MAX_PREVIEW_SIDE_BYTES: usize = 2 * 1024;
/// One side of the display diff persisted with a completed edit result may be
/// far larger than an approval preview: it is stored once and never enters
/// model context, so the bound only protects the store and the wire.
const MAX_RESULT_DIFF_SIDE_BYTES: usize = 32 * 1024;
const PREVIEW_TRUNCATION_MARKER: &str = "[preview truncated]";

/// Builds the approval-request preview for a file-modifying call: the
/// workspace-relative path the model addressed and a bounded
/// unified-diff-style rendering of the change. Returns None for other tools
/// and for arguments the tool itself would reject.
pub(crate) fn edit_preview(name: &str, arguments: &str) -> Option<EditPreview> {
    bounded_edit_diff(name, arguments, MAX_PREVIEW_SIDE_BYTES)
}

/// Builds the display payload persisted alongside a successful
/// `edit_file`/`write_file` result so clients can render the applied change
/// as a diff. Returns None for other tools.
pub(crate) fn edit_result_display(name: &str, arguments: &str) -> Option<ToolCallDisplay> {
    bounded_edit_diff(name, arguments, MAX_RESULT_DIFF_SIDE_BYTES).map(|preview| {
        ToolCallDisplay::Diff {
            path: preview.path,
            diff: preview.diff,
        }
    })
}

fn bounded_edit_diff(name: &str, arguments: &str, side_budget: usize) -> Option<EditPreview> {
    #[derive(Deserialize)]
    struct EditArguments {
        path: String,
        old_string: String,
        new_string: String,
    }
    #[derive(Deserialize)]
    struct WriteArguments {
        path: String,
        content: String,
    }
    match name {
        "edit_file" => {
            let arguments = serde_json::from_str::<EditArguments>(arguments).ok()?;
            let mut diff = String::new();
            push_diff_lines(&mut diff, '-', &arguments.old_string, side_budget);
            push_diff_lines(&mut diff, '+', &arguments.new_string, side_budget);
            Some(EditPreview {
                path: arguments.path,
                diff,
            })
        }
        "write_file" => {
            let arguments = serde_json::from_str::<WriteArguments>(arguments).ok()?;
            let mut diff = String::new();
            push_diff_lines(&mut diff, '+', &arguments.content, side_budget);
            Some(EditPreview {
                path: arguments.path,
                diff,
            })
        }
        _ => None,
    }
}

fn push_diff_lines(diff: &mut String, sign: char, content: &str, side_budget: usize) {
    let mut remaining = side_budget;
    for line in content.lines() {
        diff.push(sign);
        diff.push(' ');
        // The sign, separator, and newline count against the side budget so
        // one side of a preview can never exceed it by more than the marker.
        if line.len() + 3 > remaining {
            let mut end = remaining.saturating_sub(3).min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            diff.push_str(&line[..end]);
            diff.push_str(PREVIEW_TRUNCATION_MARKER);
            diff.push('\n');
            return;
        }
        remaining -= line.len() + 3;
        diff.push_str(line);
        diff.push('\n');
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
    fn edit_previews_render_bounded_diffs_for_edit_and_write_calls() {
        let preview = edit_preview(
            "edit_file",
            r#"{"path":"src/lib.rs","old_string":"fn a() {}\nfn b() {}","new_string":"fn a() {}"}"#,
        )
        .unwrap();
        assert_eq!(preview.path, "src/lib.rs");
        assert_eq!(preview.diff, "- fn a() {}\n- fn b() {}\n+ fn a() {}\n");

        let preview = edit_preview(
            "write_file",
            r#"{"path":"NOTES.md","content":"line one\nline two"}"#,
        )
        .unwrap();
        assert_eq!(preview.path, "NOTES.md");
        assert_eq!(preview.diff, "+ line one\n+ line two\n");

        assert_eq!(edit_preview("shell", r#"{"command":"ls"}"#), None);
        assert_eq!(edit_preview("edit_file", r#"{"path":"x"}"#), None);

        let oversized = serde_json::to_string(&serde_json::json!({
            "path": "big.txt",
            "old_string": "x".repeat(MAX_PREVIEW_SIDE_BYTES * 2),
            "new_string": "y\n".repeat(MAX_PREVIEW_SIDE_BYTES),
        }))
        .unwrap();
        let preview = edit_preview("edit_file", &oversized).unwrap();
        // Each side may exceed its budget only by the truncation line's
        // sign, separator, marker, and newline.
        assert!(
            preview.diff.len()
                <= 2 * (MAX_PREVIEW_SIDE_BYTES + PREVIEW_TRUNCATION_MARKER.len() + 3)
        );
        assert_eq!(preview.diff.matches(PREVIEW_TRUNCATION_MARKER).count(), 2);
    }

    #[test]
    fn edit_result_displays_carry_larger_bounded_diffs_than_previews() {
        let display = edit_result_display(
            "write_file",
            r#"{"path":"NOTES.md","content":"line one\nline two"}"#,
        )
        .unwrap();
        assert_eq!(
            display,
            ToolCallDisplay::Diff {
                path: "NOTES.md".to_owned(),
                diff: "+ line one\n+ line two\n".to_owned(),
            }
        );
        assert_eq!(edit_result_display("shell", r#"{"command":"ls"}"#), None);

        // A change too large for the 2 KiB approval preview still fits the
        // result display whole; only the 32 KiB side budget truncates it.
        let sizable = serde_json::to_string(&serde_json::json!({
            "path": "big.txt",
            "old_string": "x\n".repeat(MAX_PREVIEW_SIDE_BYTES),
            "new_string": "y".repeat(MAX_RESULT_DIFF_SIDE_BYTES * 2),
        }))
        .unwrap();
        let ToolCallDisplay::Diff { diff, .. } =
            edit_result_display("edit_file", &sizable).unwrap();
        assert_eq!(
            diff.lines().filter(|line| line.starts_with('-')).count(),
            MAX_PREVIEW_SIDE_BYTES
        );
        assert_eq!(diff.matches(PREVIEW_TRUNCATION_MARKER).count(), 1);
        assert!(
            diff.len() <= 2 * (MAX_RESULT_DIFF_SIDE_BYTES + PREVIEW_TRUNCATION_MARKER.len() + 3)
        );
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
