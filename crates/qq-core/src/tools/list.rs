use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;

use crate::workspace::Workspace;

use super::dispatch::{TRUNCATION_MARKER, ToolExecutionResult};

pub(super) const MAX_DIRECTORY_ENTRIES: usize = 1_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDirArgs {
    path: String,
    #[serde(default = "default_directory_limit")]
    limit: usize,
}

const fn default_directory_limit() -> usize {
    MAX_DIRECTORY_ENTRIES
}

pub(super) fn list_dir(
    workspace: &Workspace,
    arguments: ListDirArgs,
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    if arguments.limit == 0 || arguments.limit > MAX_DIRECTORY_ENTRIES {
        return ToolExecutionResult::error(format!(
            "limit must be between 1 and {MAX_DIRECTORY_ENTRIES}"
        ));
    }
    let path = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error.to_string()),
    };
    if !workspace.root().is_dir(&path) {
        return ToolExecutionResult::error("path is not a directory");
    }
    let read_dir = match workspace.root().read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            return ToolExecutionResult::error(format!("could not list directory: {error}"));
        }
    };
    let mut entries = Vec::with_capacity(arguments.limit.min(MAX_DIRECTORY_ENTRIES));
    for entry in read_dir.take(MAX_DIRECTORY_ENTRIES + 1) {
        if cancelled.load(Ordering::Acquire) {
            return ToolExecutionResult::error("tool execution was cancelled");
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return ToolExecutionResult::error(format!("could not list directory: {error}"));
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                return ToolExecutionResult::error(format!(
                    "could not inspect directory entry: {error}"
                ));
            }
        };
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            name.push('/');
        } else if file_type.is_symlink() {
            name.push('@');
        }
        entries.push(name);
    }
    if entries.len() > MAX_DIRECTORY_ENTRIES {
        return ToolExecutionResult::error(format!(
            "directory contains more than {MAX_DIRECTORY_ENTRIES} entries"
        ));
    }
    entries.sort_unstable();
    let truncated = entries.len() > arguments.limit;
    entries.truncate(arguments.limit);
    let mut output = entries.join("\n");
    if truncated {
        output.push_str(TRUNCATION_MARKER);
    } else if !output.is_empty() {
        output.push('\n');
    }
    ToolExecutionResult::success(output)
}
