use std::io::{BufRead, BufReader, Cursor, Read, Take};

use serde::Deserialize;

use crate::workspace::{FileState, FileStateUpdate, Workspace, content_hash};

use super::dispatch::{
    MAX_TOOL_RESULT_BYTES, TRUNCATION_MARKER, ToolCancellation, ToolExecutionResult,
};

pub(super) const MAX_READ_LINES: usize = 2_000;
const MAX_READ_OFFSET: usize = 100_000;
pub(super) const MAX_READ_SCAN_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFileArgs {
    path: String,
    #[serde(default = "default_offset")]
    offset: usize,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

const fn default_offset() -> usize {
    1
}
const fn default_read_limit() -> usize {
    200
}

#[inline]
pub(super) fn read_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: ReadFileArgs,
    cancelled: &ToolCancellation,
) -> ToolExecutionResult {
    if arguments.offset == 0 || arguments.offset > MAX_READ_OFFSET {
        return ToolExecutionResult::error(format!(
            "offset must be between 1 and {MAX_READ_OFFSET}"
        ));
    }
    if arguments.limit == 0 || arguments.limit > MAX_READ_LINES {
        return ToolExecutionResult::error(format!("limit must be between 1 and {MAX_READ_LINES}"));
    }
    let path = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error.to_string()),
    };
    if !workspace.root().is_file(&path) {
        return ToolExecutionResult::error("path is not a file");
    }
    let file = match workspace.root().open(&path) {
        Ok(file) => file,
        Err(error) => return ToolExecutionResult::error(format!("could not open file: {error}")),
    };
    // The whole content (bounded by the scan cap) is read so the session's
    // file-state map can record a full-file hash for the staleness guard.
    // Larger files record nothing: they are not editable anyway.
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_READ_SCAN_BYTES + 1).read_to_end(&mut bytes) {
        return ToolExecutionResult::error(format!("could not read file: {error}"));
    }
    let update = (bytes.len() as u64 <= MAX_READ_SCAN_BYTES).then(|| FileStateUpdate {
        path: path.to_string_lossy().into_owned(),
        hash: content_hash(&bytes),
    });
    let reader = BufReader::new(Cursor::new(bytes.as_slice())).take(MAX_READ_SCAN_BYTES);
    let mut result = window_lines(reader, arguments.offset, arguments.limit, cancelled);
    if !result.is_error
        && let Some(update) = update
    {
        file_state.record(update.path.clone(), update.hash.clone());
        result.file_state = Some(update);
    }
    result
}

fn window_lines<R: Read>(
    mut reader: Take<BufReader<R>>,
    offset: usize,
    limit: usize,
    cancelled: &ToolCancellation,
) -> ToolExecutionResult {
    let mut output = String::new();
    let mut line = Vec::new();
    let end = offset.saturating_add(limit);
    for line_number in 1..end {
        if cancelled.is_cancelled() {
            return ToolExecutionResult::error("tool execution was cancelled");
        }
        line.clear();
        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(error) => {
                return ToolExecutionResult::error(format!("could not read file: {error}"));
            }
        };
        if read == 0 {
            break;
        }
        if line_number >= offset {
            let text = match std::str::from_utf8(&line) {
                Ok(text) => text,
                // `error_len() == None` means the data ends inside a multibyte
                // character: the scan cap (or the file itself) cut it short.
                // Return the valid prefix as a truncated read, not an error.
                Err(error) if error.error_len().is_none() => {
                    output.push_str(
                        std::str::from_utf8(&line[..error.valid_up_to()])
                            .expect("the UTF-8 validator reported a valid prefix"),
                    );
                    output.push_str(TRUNCATION_MARKER);
                    return ToolExecutionResult::success(output);
                }
                Err(_) => return ToolExecutionResult::error("file is not valid UTF-8"),
            };
            output.push_str(text);
            if output.len() > MAX_TOOL_RESULT_BYTES {
                return ToolExecutionResult::success(output);
            }
        }
    }
    if reader.limit() == 0 {
        // The scan cap was consumed exactly; only mark truncation when the file
        // actually continues past it. A probe failure is treated as truncation
        // because end-of-file cannot be confirmed.
        let mut probe = [0_u8; 1];
        if !matches!(reader.get_mut().read(&mut probe), Ok(0)) {
            output.push_str(TRUNCATION_MARKER);
        }
    }
    ToolExecutionResult::success(output)
}
