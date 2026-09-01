#[cfg(test)]
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(test)]
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::workspace::{FileState, FileStateUpdate, Workspace, blocking_permits};

use super::{
    edit::edit_file,
    list::list_dir,
    read::read_file,
    search::search,
    shell::{ShellArgs, run_shell},
    specs::BuiltInTool,
    write::write_file,
};

pub(crate) const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
pub(super) const TRUNCATION_MARKER: &str = "\n...[truncated by qq]\n";
#[cfg(test)]
static TEST_EXECUTIONS_STARTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_EXECUTION_BARRIER: OnceLock<std::sync::Barrier> = OnceLock::new();

pub(crate) async fn execute(
    workspace: Workspace,
    file_state: Arc<FileState>,
    name: String,
    arguments: String,
    cancelled: Arc<AtomicBool>,
    output: Option<mpsc::Sender<String>>,
) -> ToolExecutionResult {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolExecutionResult::error("tool arguments exceed the 64 KiB limit");
    }
    // Shell executes on the async runtime directly: it awaits a child process
    // rather than doing blocking filesystem work, so it must neither occupy a
    // blocking permit for its full (possibly 120 s) lifetime nor block a
    // worker thread.
    if matches!(BuiltInTool::from_name(&name), Some(BuiltInTool::Shell)) {
        let arguments = match serde_json::from_str::<ShellArgs>(&arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolExecutionResult::error(format!("invalid arguments: {error}"));
            }
        };
        return run_shell(&workspace, &arguments, &cancelled, output.as_ref()).await;
    }
    let permit = match blocking_permits().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return ToolExecutionResult::error("tool executor is unavailable"),
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        execute_blocking(&workspace, &file_state, &name, &arguments, &cancelled)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => ToolExecutionResult::error("tool execution stopped unexpectedly"),
    }
}

/// Wraps an externally produced result (an MCP call outcome) in the same
/// bounded-result truncation as built-in tool executions.
pub(crate) fn bounded_result(content: String, is_error: bool) -> ToolExecutionResult {
    if is_error {
        ToolExecutionResult::error(content)
    } else {
        ToolExecutionResult::success(content)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolExecutionResult {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    /// Set when the execution (re)recorded a file's content hash, so the
    /// session store can persist the file-state map alongside the result.
    pub(crate) file_state: Option<FileStateUpdate>,
}

impl ToolExecutionResult {
    #[inline]
    pub(super) fn success(content: String) -> Self {
        Self {
            content: truncate_result(content),
            is_error: false,
            file_state: None,
        }
    }

    #[inline]
    pub(super) fn error(message: impl Into<String>) -> Self {
        Self {
            content: truncate_result(message.into()),
            is_error: true,
            file_state: None,
        }
    }
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestDelayArgs {
    delay_ms: u64,
    result: String,
    #[serde(default)]
    synchronize: bool,
}

#[cfg(test)]
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TestMutateArgs {
    delay_ms: u64,
    result: Option<String>,
}

#[cfg(test)]
pub(crate) fn test_executions_started() -> usize {
    TEST_EXECUTIONS_STARTED.load(Ordering::Acquire)
}

#[inline]
pub(super) fn execute_blocking(
    workspace: &Workspace,
    file_state: &FileState,
    name: &str,
    arguments: &str,
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    if cancelled.load(Ordering::Acquire) {
        return ToolExecutionResult::error("tool execution was cancelled");
    }
    match BuiltInTool::from_name(name) {
        Some(BuiltInTool::ReadFile) => deserialize(arguments)
            .map_or_else(ToolExecutionResult::error, |args| {
                read_file(workspace, file_state, args, cancelled)
            }),
        Some(BuiltInTool::ListDir) => deserialize(arguments)
            .map_or_else(ToolExecutionResult::error, |args| {
                list_dir(workspace, args, cancelled)
            }),
        Some(BuiltInTool::Search) => deserialize(arguments)
            .map_or_else(ToolExecutionResult::error, |args| {
                search(workspace, args, cancelled)
            }),
        Some(BuiltInTool::EditFile) => deserialize(arguments)
            .map_or_else(ToolExecutionResult::error, |args| {
                edit_file(workspace, file_state, &args)
            }),
        Some(BuiltInTool::WriteFile) => deserialize(arguments)
            .map_or_else(ToolExecutionResult::error, |args| {
                write_file(workspace, file_state, &args)
            }),
        Some(BuiltInTool::Shell) => {
            ToolExecutionResult::error("shell commands must execute asynchronously")
        }
        #[cfg(test)]
        Some(BuiltInTool::TestDelay) => {
            let arguments: TestDelayArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolExecutionResult::error(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            if arguments.synchronize {
                TEST_EXECUTION_BARRIER
                    .get_or_init(|| std::sync::Barrier::new(2))
                    .wait();
            }
            for _ in 0..arguments.delay_ms {
                if cancelled.load(Ordering::Acquire) {
                    return ToolExecutionResult::error("tool execution was cancelled");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolExecutionResult::success(arguments.result)
        }
        #[cfg(test)]
        Some(BuiltInTool::TestMutate) => {
            let arguments: TestMutateArgs = match deserialize(arguments) {
                Ok(arguments) => arguments,
                Err(error) => return ToolExecutionResult::error(error),
            };
            TEST_EXECUTIONS_STARTED.fetch_add(1, Ordering::Release);
            for _ in 0..arguments.delay_ms {
                if cancelled.load(Ordering::Acquire) {
                    return ToolExecutionResult::error("tool execution was cancelled");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ToolExecutionResult::success(arguments.result.unwrap_or_else(|| "mutated".to_owned()))
        }
        #[cfg(test)]
        Some(BuiltInTool::TestShell) => ToolExecutionResult::success("shell ran".to_owned()),
        None => ToolExecutionResult::error(format!("unknown tool {name:?}")),
    }
}

fn deserialize<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

/// The size of `byte` once serde_json escapes it inside a JSON string.
const fn escaped_byte_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
        byte if byte < 0x20 => 6,
        _ => 1,
    }
}

pub(super) fn escaped_len(content: &str) -> usize {
    content.bytes().map(escaped_byte_len).sum()
}

/// Bounds a tool result by its JSON-escaped size, not its raw size. Results are
/// embedded in persisted event envelopes with a hard byte cap; control-heavy
/// content (for example ANSI logs) escapes up to 6:1, so budgeting the raw size
/// could make persistence fail on legitimate workspace file content.
fn truncate_result(mut content: String) -> String {
    if escaped_len(&content) <= MAX_TOOL_RESULT_BYTES {
        return content;
    }
    let available = MAX_TOOL_RESULT_BYTES.saturating_sub(escaped_len(TRUNCATION_MARKER));
    let mut escaped = 0_usize;
    let mut end = 0_usize;
    for (index, byte) in content.bytes().enumerate() {
        escaped += escaped_byte_len(byte);
        if escaped > available {
            break;
        }
        end = index + 1;
    }
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    content.push_str(TRUNCATION_MARKER);
    content
}
