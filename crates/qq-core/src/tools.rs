#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::HashMap,
    fmt::Write as _,
    io::{BufRead, BufReader, Cursor, Read, Take, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock, PoisonError, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use cap_std::fs::Dir;
use qq_provider::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

pub(crate) const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_READ_LINES: usize = 2_000;
const MAX_READ_OFFSET: usize = 100_000;
const MAX_READ_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// Files above this size cannot be edited or overwritten; matching the read
/// scan cap means every editable file's whole-content hash is recordable.
const MAX_EDIT_FILE_BYTES: u64 = MAX_READ_SCAN_BYTES;
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_SEARCH_ENTRIES: usize = 20_000;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_FILES: usize = 10_000;
const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BLOCKING_TOOL_TASKS: usize = 8;
const TRUNCATION_MARKER: &str = "\n...[truncated by qq]\n";
static BLOCKING_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
/// One apply lock per canonical workspace path, shared by every session in
/// this process; entries are pruned once no workspace handle keeps them alive.
static APPLY_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<StdMutex<()>>>>> = OnceLock::new();
static TEMP_FILE_ORDINAL: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_EXECUTIONS_STARTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_EXECUTION_BARRIER: OnceLock<std::sync::Barrier> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Workspace {
    root: Arc<Dir>,
    path: Arc<PathBuf>,
    /// Serializes the hash-check-and-rename apply section for this workspace.
    apply_lock: Arc<StdMutex<()>>,
}

impl Workspace {
    pub(crate) fn open(path: &Path) -> Result<Self, std::io::Error> {
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace path must be absolute",
            ));
        }
        let components = path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(component) => Some(Ok(component)),
                std::path::Component::Prefix(_) | std::path::Component::RootDir => None,
                std::path::Component::CurDir | std::path::Component::ParentDir => {
                    Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "workspace path must be canonical",
                    )))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut anchor = path.to_owned();
        for _ in &components {
            anchor.pop();
        }
        let mut root = cap_primitives::fs::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
        for component in components {
            root = cap_primitives::fs::open_dir_nofollow(&root, Path::new(component))?;
        }

        Ok(Self {
            root: Arc::new(Dir::from_std_file(root)),
            apply_lock: apply_lock(path),
            path: Arc::new(path.to_owned()),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn apply_lock(path: &Path) -> Arc<StdMutex<()>> {
    let registry = APPLY_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(StdMutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

/// Content hashes for every workspace file one session has read, keyed by
/// canonical workspace-relative path. `read_file` records into it on each
/// successful read, applied edits refresh it, and a future `@` file
/// attachment records through the same [`FileState::record`] seam so pinned
/// files satisfy the read-before-write rule without a redundant read.
#[derive(Default)]
pub(crate) struct FileState {
    entries: StdMutex<HashMap<String, String>>,
}

impl FileState {
    pub(crate) fn with_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            entries: StdMutex::new(entries.into_iter().collect()),
        }
    }

    pub(crate) fn record(&self, path: String, hash: String) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, hash);
    }

    fn recorded(&self, path: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned()
    }
}

/// A file-state map entry produced by a successful tool execution, carried on
/// the tool result so session persistence can record it durably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileStateUpdate {
    pub(crate) path: String,
    pub(crate) hash: String,
}

fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

pub(crate) async fn open_workspace(
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
) -> Result<Workspace, std::io::Error> {
    let permit = blocking_permits()
        .acquire_owned()
        .await
        .map_err(|_| std::io::Error::other("tool executor is unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "workspace opening was cancelled",
            ));
        }
        let path = std::fs::canonicalize(path)?;
        let workspace = Workspace::open(&path)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "workspace opening was cancelled",
            ));
        }
        Ok(workspace)
    })
    .await
    .map_err(|_| std::io::Error::other("workspace opening stopped unexpectedly"))?
}

fn blocking_permits() -> Arc<Semaphore> {
    Arc::clone(BLOCKING_PERMITS.get_or_init(|| Arc::new(Semaphore::new(MAX_BLOCKING_TOOL_TASKS))))
}

#[derive(Clone, Copy)]
enum BuiltInTool {
    ReadFile,
    ListDir,
    Search,
    EditFile,
    WriteFile,
    #[cfg(test)]
    TestDelay,
    #[cfg(test)]
    TestMutate,
    #[cfg(test)]
    TestShell,
}

impl BuiltInTool {
    const ALL: [Self; 5] = [
        Self::ReadFile,
        Self::ListDir,
        Self::Search,
        Self::EditFile,
        Self::WriteFile,
    ];

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "read_file" => Some(Self::ReadFile),
            "list_dir" => Some(Self::ListDir),
            "search" => Some(Self::Search),
            "edit_file" => Some(Self::EditFile),
            "write_file" => Some(Self::WriteFile),
            #[cfg(test)]
            "__test_delay" => Some(Self::TestDelay),
            #[cfg(test)]
            "__test_mutate" => Some(Self::TestMutate),
            #[cfg(test)]
            "__test_shell" => Some(Self::TestShell),
            _ => None,
        }
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::ReadFile => ToolSpec::new(
                "read_file",
                "Read a UTF-8 file in the workspace by line, with a 1-based offset and bounded line count.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "minimum": 1 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::ListDir => ToolSpec::new(
                "list_dir",
                "List one workspace directory in deterministic name order.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_DIRECTORY_ENTRIES }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::Search => ToolSpec::new(
                "search",
                "Search workspace file names and UTF-8 file contents for a literal string.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            ),
            Self::EditFile => ToolSpec::new(
                "edit_file",
                "Replace an exact string in a workspace file that was read earlier in this session. Fails if old_string is missing or ambiguous; set replace_all to replace every occurrence.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "minLength": 1 },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                }),
            ),
            Self::WriteFile => ToolSpec::new(
                "write_file",
                "Create a workspace file, or fully overwrite one that was read earlier in this session.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            ),
            #[cfg(test)]
            Self::TestDelay | Self::TestMutate | Self::TestShell => {
                unreachable!("test tools are not advertised")
            }
        }
    }
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS
        .get_or_init(|| {
            BuiltInTool::ALL
                .into_iter()
                .map(BuiltInTool::spec)
                .collect()
        })
        .clone()
}

pub(crate) async fn execute(
    workspace: Workspace,
    file_state: Arc<FileState>,
    name: String,
    arguments: String,
    cancelled: Arc<AtomicBool>,
) -> ToolExecutionResult {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return ToolExecutionResult::error("tool arguments exceed the 64 KiB limit");
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolExecutionResult {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    /// Set when the execution (re)recorded a file's content hash, so the
    /// session store can persist the file-state map alongside the result.
    pub(crate) file_state: Option<FileStateUpdate>,
}

impl ToolExecutionResult {
    fn success(content: String) -> Self {
        Self {
            content: truncate_result(content),
            is_error: false,
            file_state: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            content: truncate_result(message.into()),
            is_error: true,
            file_state: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    #[serde(default = "default_offset")]
    offset: usize,
    #[serde(default = "default_read_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDirArgs {
    path: String,
    #[serde(default = "default_directory_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_search_path")]
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
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

const fn default_offset() -> usize {
    1
}

const fn default_read_limit() -> usize {
    200
}

const fn default_directory_limit() -> usize {
    MAX_DIRECTORY_ENTRIES
}

fn default_search_path() -> String {
    ".".to_owned()
}

fn execute_blocking(
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
        Some(BuiltInTool::ReadFile) => {
            let arguments = match serde_json::from_str::<ReadFileArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
            };
            read_file(workspace, file_state, arguments, cancelled)
        }
        Some(BuiltInTool::ListDir) => {
            let arguments = match serde_json::from_str::<ListDirArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
            };
            list_dir(workspace, arguments, cancelled)
        }
        Some(BuiltInTool::Search) => {
            let arguments = match serde_json::from_str::<SearchArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
            };
            search(workspace, arguments, cancelled)
        }
        Some(BuiltInTool::EditFile) => {
            let arguments = match serde_json::from_str::<EditFileArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
            };
            edit_file(workspace, file_state, &arguments)
        }
        Some(BuiltInTool::WriteFile) => {
            let arguments = match serde_json::from_str::<WriteFileArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
            };
            write_file(workspace, file_state, &arguments)
        }
        #[cfg(test)]
        Some(BuiltInTool::TestDelay) => {
            let arguments = match serde_json::from_str::<TestDelayArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
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
            let arguments = match serde_json::from_str::<TestMutateArgs>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolExecutionResult::error(format!("invalid arguments: {error}"));
                }
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

fn read_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: ReadFileArgs,
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    if arguments.offset == 0 || arguments.offset > MAX_READ_OFFSET {
        return ToolExecutionResult::error(format!(
            "offset must be between 1 and {MAX_READ_OFFSET}"
        ));
    }
    if arguments.limit == 0 || arguments.limit > MAX_READ_LINES {
        return ToolExecutionResult::error(format!("limit must be between 1 and {MAX_READ_LINES}"));
    }
    let path = match contained_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error),
    };
    if !workspace.root.is_file(&path) {
        return ToolExecutionResult::error("path is not a file");
    }
    let file = match workspace.root.open(&path) {
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
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    let mut output = String::new();
    let mut line = Vec::new();
    let end = offset.saturating_add(limit);
    for line_number in 1..end {
        if cancelled.load(Ordering::Acquire) {
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

fn edit_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &EditFileArgs,
) -> ToolExecutionResult {
    if arguments.old_string.is_empty() {
        return ToolExecutionResult::error("old_string must not be empty");
    }
    if arguments.old_string == arguments.new_string {
        return ToolExecutionResult::error(
            "old_string and new_string are identical; there is nothing to change",
        );
    }
    let path = match contained_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error),
    };
    if !workspace.root.is_file(&path) {
        return ToolExecutionResult::error("path is not a file");
    }
    let key = path.to_string_lossy().into_owned();
    let Some(recorded) = file_state.recorded(&key) else {
        return ToolExecutionResult::error(format!(
            "{} has not been read in this session; call read_file on it first, then retry the edit",
            arguments.path
        ));
    };

    // The exclusive apply section: re-hash, validate, and rename while no
    // other session can interleave a write to this workspace.
    let guard = workspace
        .apply_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let current = match read_editable(workspace, &path) {
        Ok(current) => current,
        Err(error) => return ToolExecutionResult::error(error),
    };
    if content_hash(&current.bytes) != recorded {
        return ToolExecutionResult::error(stale_file_error(&arguments.path));
    }
    let content = match std::str::from_utf8(&current.bytes) {
        Ok(content) => content,
        Err(_) => return ToolExecutionResult::error("file is not valid UTF-8"),
    };
    let occurrences = content.matches(&arguments.old_string).count();
    if occurrences == 0 {
        return ToolExecutionResult::error(format!(
            "old_string was not found in {}; re-read the file and match its current content exactly",
            arguments.path
        ));
    }
    if occurrences > 1 && !arguments.replace_all {
        return ToolExecutionResult::error(format!(
            "old_string occurs {occurrences} times in {}; extend it until it is unique, or set replace_all",
            arguments.path
        ));
    }
    let new_content = if arguments.replace_all {
        content.replace(&arguments.old_string, &arguments.new_string)
    } else {
        content.replacen(&arguments.old_string, &arguments.new_string, 1)
    };
    if new_content.len() as u64 > MAX_EDIT_FILE_BYTES {
        return ToolExecutionResult::error(format!(
            "the edited content exceeds the {} MiB file size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    if let Err(error) = apply_atomically(
        workspace,
        &path,
        new_content.as_bytes(),
        Some(current.permissions),
    ) {
        return ToolExecutionResult::error(error);
    }
    drop(guard);

    let replaced = if arguments.replace_all {
        occurrences
    } else {
        1
    };
    let hash = content_hash(new_content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let mut result = ToolExecutionResult::success(format!(
        "Edited {}: replaced {replaced} occurrence(s).",
        arguments.path
    ));
    result.file_state = Some(FileStateUpdate { path: key, hash });
    result
}

fn write_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &WriteFileArgs,
) -> ToolExecutionResult {
    if arguments.content.len() as u64 > MAX_EDIT_FILE_BYTES {
        return ToolExecutionResult::error(format!(
            "content exceeds the {} MiB file size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let path = match resolve_write_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error),
    };
    let key = path.to_string_lossy().into_owned();

    let guard = workspace
        .apply_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let created = match workspace.root.symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            // Overwrites follow the same read-before-write and staleness
            // rules as edits; only brand-new files are exempt.
            let Some(recorded) = file_state.recorded(&key) else {
                return ToolExecutionResult::error(format!(
                    "{} already exists but has not been read in this session; call read_file on it first, then retry the overwrite",
                    arguments.path
                ));
            };
            let current = match read_editable(workspace, &path) {
                Ok(current) => current,
                Err(error) => return ToolExecutionResult::error(error),
            };
            if content_hash(&current.bytes) != recorded {
                return ToolExecutionResult::error(stale_file_error(&arguments.path));
            }
            if let Err(error) = apply_atomically(
                workspace,
                &path,
                arguments.content.as_bytes(),
                Some(current.permissions),
            ) {
                return ToolExecutionResult::error(error);
            }
            false
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ToolExecutionResult::error("path is a symlink; address its target directly");
        }
        Ok(_) => return ToolExecutionResult::error("path is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) =
                apply_atomically(workspace, &path, arguments.content.as_bytes(), None)
            {
                return ToolExecutionResult::error(error);
            }
            true
        }
        Err(error) => {
            return ToolExecutionResult::error(format!("could not inspect path: {error}"));
        }
    };
    drop(guard);

    let hash = content_hash(arguments.content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let mut result = ToolExecutionResult::success(format!(
        "{} {} ({} bytes).",
        if created { "Created" } else { "Wrote" },
        arguments.path,
        arguments.content.len()
    ));
    result.file_state = Some(FileStateUpdate { path: key, hash });
    result
}

fn stale_file_error(path: &str) -> String {
    format!("{path} changed since it was last read in this session; read it again and retry")
}

struct EditableFile {
    bytes: Vec<u8>,
    permissions: cap_std::fs::Permissions,
}

fn read_editable(workspace: &Workspace, path: &Path) -> Result<EditableFile, String> {
    let file = workspace
        .root
        .open(path)
        .map_err(|error| format!("could not open file: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect file: {error}"))?;
    if metadata.len() > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "file exceeds the {} MiB editable size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(MAX_EDIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read file: {error}"))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "file exceeds the {} MiB editable size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok(EditableFile {
        bytes,
        permissions: metadata.permissions(),
    })
}

/// Resolves a `write_file` target, which may not exist yet: an existing path
/// resolves through the same containment as every other tool, and a new file
/// resolves its parent directory and re-attaches the final component.
fn resolve_write_path(workspace: &Workspace, requested: &str) -> Result<PathBuf, String> {
    let resolve_error = match contained_path(workspace, requested) {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let requested_path = Path::new(requested);
    if requested.is_empty() || requested_path.is_absolute() {
        return Err(resolve_error);
    }
    let Some(file_name) = requested_path.file_name() else {
        return Err(resolve_error);
    };
    let parent = match requested_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ".".to_owned(),
    };
    let parent = contained_path(workspace, &parent)
        .map_err(|error| format!("parent directory could not be resolved: {error}"))?;
    if !workspace.root.is_dir(&parent) {
        return Err("parent path is not a directory".to_owned());
    }
    if parent.as_os_str().is_empty() || parent == Path::new(".") {
        Ok(PathBuf::from(file_name))
    } else {
        Ok(parent.join(file_name))
    }
}

/// Writes `bytes` to a temporary file in the target's directory through the
/// workspace capability, preserves permissions when replacing an existing
/// file, and renames into place so readers never observe a partial write.
fn apply_atomically(
    workspace: &Workspace,
    path: &Path,
    bytes: &[u8],
    permissions: Option<cap_std::fs::Permissions>,
) -> Result<(), String> {
    let temp_name = format!(
        ".qq-apply-{}-{}.tmp",
        std::process::id(),
        TEMP_FILE_ORDINAL.fetch_add(1, Ordering::Relaxed),
    );
    let temp_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(&temp_name),
        _ => PathBuf::from(&temp_name),
    };
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut temp = workspace
        .root
        .open_with(&temp_path, &options)
        .map_err(|error| format!("could not create a temporary file: {error}"))?;
    let written = temp
        .write_all(bytes)
        .and_then(|()| temp.sync_all())
        .map_err(|error| format!("could not write the temporary file: {error}"));
    drop(temp);
    let applied = written
        .and_then(|()| match permissions {
            Some(permissions) => workspace
                .root
                .set_permissions(&temp_path, permissions)
                .map_err(|error| format!("could not preserve file permissions: {error}")),
            None => Ok(()),
        })
        .and_then(|()| {
            workspace
                .root
                .rename(&temp_path, &workspace.root, path)
                .map_err(|error| format!("could not apply the change: {error}"))
        });
    if applied.is_err() {
        let _ = workspace.root.remove_file(&temp_path);
    }
    applied
}

fn list_dir(
    workspace: &Workspace,
    arguments: ListDirArgs,
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    if arguments.limit == 0 || arguments.limit > MAX_DIRECTORY_ENTRIES {
        return ToolExecutionResult::error(format!(
            "limit must be between 1 and {MAX_DIRECTORY_ENTRIES}"
        ));
    }
    let path = match contained_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error),
    };
    if !workspace.root.is_dir(&path) {
        return ToolExecutionResult::error("path is not a directory");
    }
    let read_dir = match workspace.root.read_dir(&path) {
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

fn search(
    workspace: &Workspace,
    arguments: SearchArgs,
    cancelled: &AtomicBool,
) -> ToolExecutionResult {
    if arguments.query.is_empty() || arguments.query.len() > 1_024 {
        return ToolExecutionResult::error("query must contain between 1 and 1024 bytes");
    }
    let root = match contained_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolExecutionResult::error(error),
    };
    let mut pending = vec![root];
    let mut files = 0_usize;
    let mut scanned_bytes = 0_u64;
    let mut visited_entries = 0_usize;
    let mut matches = Vec::new();
    let mut match_bytes = 0_usize;
    let mut bounded = false;

    while let Some(path) = pending.pop() {
        if cancelled.load(Ordering::Acquire) {
            return ToolExecutionResult::error("tool execution was cancelled");
        }
        // `bounded` also stops the walk: once the result buffer is full no
        // further match can be reported, so scanning more files is wasted work.
        if bounded
            || matches.len() >= MAX_SEARCH_RESULTS
            || files >= MAX_SEARCH_FILES
            || scanned_bytes >= MAX_SEARCH_BYTES
            || visited_entries >= MAX_SEARCH_ENTRIES
        {
            bounded = true;
            break;
        }
        visited_entries += 1;
        let metadata = match workspace.root.symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return ToolExecutionResult::error(format!(
                    "could not inspect {}: {error}",
                    path.display()
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let entries = match workspace.root.read_dir(&path) {
                Ok(entries) => entries,
                Err(error) => {
                    return ToolExecutionResult::error(format!(
                        "could not list {}: {error}",
                        path.display()
                    ));
                }
            };
            let mut children = Vec::new();
            for entry in entries {
                if cancelled.load(Ordering::Acquire) {
                    return ToolExecutionResult::error("tool execution was cancelled");
                }
                if visited_entries
                    .saturating_add(pending.len())
                    .saturating_add(children.len())
                    >= MAX_SEARCH_ENTRIES
                {
                    bounded = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        return ToolExecutionResult::error(format!(
                            "could not read an entry in {}: {error}",
                            path.display()
                        ));
                    }
                };
                children.push(path.join(entry.file_name()));
            }
            children.sort_unstable();
            pending.extend(children.into_iter().rev());
            if bounded {
                break;
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        files += 1;
        let relative = path.to_string_lossy();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(&arguments.query))
        {
            let entry = format!("{relative}: filename match");
            if match_bytes.saturating_add(entry.len() + 1) > MAX_TOOL_RESULT_BYTES {
                bounded = true;
                break;
            }
            match_bytes += entry.len() + 1;
            matches.push(entry);
            if matches.len() >= MAX_SEARCH_RESULTS {
                bounded = true;
                break;
            }
        }
        let remaining = MAX_SEARCH_BYTES.saturating_sub(scanned_bytes);
        if remaining == 0 {
            bounded = true;
            break;
        }
        let file = match workspace.root.open(&path) {
            Ok(file) => file,
            Err(error) => {
                return ToolExecutionResult::error(format!(
                    "could not open {}: {error}",
                    path.display()
                ));
            }
        };
        let mut bytes = Vec::new();
        let file_truncated = metadata.len() > remaining;
        let mut file = file.take(remaining.min(metadata.len()));
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            if cancelled.load(Ordering::Acquire) {
                return ToolExecutionResult::error("tool execution was cancelled");
            }
            let read = match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    return ToolExecutionResult::error(format!(
                        "could not read {}: {error}",
                        path.display()
                    ));
                }
            };
            bytes.extend_from_slice(&chunk[..read]);
        }
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .expect("the UTF-8 validator reported a valid prefix")
            }
            Err(_) => continue,
        };
        for (index, line) in content.lines().enumerate() {
            if line.contains(&arguments.query) {
                let prefix = format!("{relative}:{}:", index + 1);
                let available = MAX_TOOL_RESULT_BYTES
                    .saturating_sub(match_bytes)
                    .saturating_sub(prefix.len() + 1);
                if available == 0 {
                    bounded = true;
                    break;
                }
                let mut end = line.len().min(available);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                let line_truncated = end < line.len();
                let entry = format!("{prefix}{}", &line[..end]);
                match_bytes += entry.len() + 1;
                matches.push(entry);
                if line_truncated {
                    bounded = true;
                    break;
                }
                if matches.len() >= MAX_SEARCH_RESULTS {
                    bounded = true;
                    break;
                }
            }
        }
        bounded |= file_truncated;
    }

    let mut output = if matches.is_empty() {
        "No matches found.\n".to_owned()
    } else {
        let mut output = matches.join("\n");
        output.push('\n');
        output
    };
    if bounded {
        output.push_str(TRUNCATION_MARKER);
    }
    ToolExecutionResult::success(output)
}

fn contained_path(workspace: &Workspace, requested: &str) -> Result<PathBuf, String> {
    if requested.is_empty() {
        return Err("path must not be empty".to_owned());
    }
    let requested = Path::new(requested);
    if requested.is_absolute() {
        return Err("path must be relative to the workspace".to_owned());
    }
    let canonical = workspace
        .root
        .canonicalize(requested)
        .map_err(|error| format!("path could not be resolved: {error}"))?;
    if canonical.is_absolute()
        || canonical
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("path escapes the workspace".to_owned());
    }
    Ok(canonical)
}

/// The size of `byte` once serde_json escapes it inside a JSON string.
const fn escaped_byte_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
        byte if byte < 0x20 => 6,
        _ => 1,
    }
}

fn escaped_len(content: &str) -> usize {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn run_tool(
        workspace: &Workspace,
        state: &FileState,
        name: &str,
        arguments: &str,
    ) -> ToolExecutionResult {
        execute_blocking(workspace, state, name, arguments, &AtomicBool::new(false))
    }

    #[test]
    fn read_and_list_are_bounded_and_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("b.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(directory.path().join("a.txt"), "a").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let listed = run_tool(&workspace, &state, "list_dir", r#"{"path":"."}"#);
        assert_eq!(listed.content, "a.txt\nb.txt\n");

        let read = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"b.txt","offset":2,"limit":1}"#,
        );
        assert_eq!(read.content, "two\n");
        let update = read.file_state.unwrap();
        assert_eq!(update.path, "b.txt");
        assert_eq!(update.hash, content_hash(b"one\ntwo\nthree\n"));
        assert_eq!(state.recorded("b.txt"), Some(update.hash));
    }

    #[test]
    fn read_file_marks_oversized_results_as_truncated() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("large.txt"),
            "x".repeat(MAX_TOOL_RESULT_BYTES + 1),
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"large.txt"}"#,
        );

        assert!(!result.is_error);
        assert!(result.content.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(result.content.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn control_dense_results_are_bounded_by_their_json_escaped_size() {
        let directory = tempfile::tempdir().unwrap();
        // Raw size stays under the result cap, but every ESC escapes 6:1 so the
        // escaped size would far exceed the persisted-event budget.
        fs::write(
            directory.path().join("ansi.log"),
            "\u{1b}".repeat(MAX_TOOL_RESULT_BYTES - 16 * 1024),
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"ansi.log"}"#,
        );

        assert!(!result.is_error);
        assert!(result.content.ends_with(TRUNCATION_MARKER));
        assert!(escaped_len(&result.content) <= MAX_TOOL_RESULT_BYTES);
        assert!(serde_json::to_string(&result.content).unwrap().len() <= MAX_TOOL_RESULT_BYTES + 2);
    }

    #[test]
    fn read_file_accepts_a_multibyte_char_split_by_the_scan_cap() {
        let directory = tempfile::tempdir().unwrap();
        let cap = usize::try_from(MAX_READ_SCAN_BYTES).unwrap();
        let mut content = Vec::with_capacity(cap + 2);
        content.resize(cap - 4, b'x');
        content.push(b'\n');
        content.extend_from_slice("ab\u{e9}".as_bytes());
        assert_eq!(content.len(), cap + 1);
        fs::write(directory.path().join("split.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"split.txt","offset":2,"limit":1}"#,
        );

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert_eq!(result.content, format!("ab{TRUNCATION_MARKER}"));
    }

    #[test]
    fn read_file_does_not_mark_an_exactly_cap_sized_file_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let cap = usize::try_from(MAX_READ_SCAN_BYTES).unwrap();
        let mut content = Vec::with_capacity(cap);
        content.resize(cap - 2, b'x');
        content.push(b'\n');
        content.push(b'y');
        assert_eq!(content.len(), cap);
        fs::write(directory.path().join("exact.txt"), &content).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"exact.txt","offset":2,"limit":1}"#,
        );

        assert!(!result.is_error);
        assert_eq!(result.content, "y");
    }

    #[test]
    fn rejects_parent_and_symlink_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "hidden").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let state = FileState::default();
        let parent = run_tool(&workspace, &state, "read_file", r#"{"path":"../secret"}"#);
        assert!(parent.is_error);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
            let symlink = run_tool(
                &workspace,
                &state,
                "read_file",
                r#"{"path":"outside/secret"}"#,
            );
            assert!(symlink.is_error);
        }
    }

    #[test]
    fn search_matches_names_and_content_without_following_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/needle.rs"), "hay\nneedle here\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );
        assert!(!result.is_error);
        assert!(result.content.contains("src/needle.rs: filename match"));
        assert!(result.content.contains("src/needle.rs:2:needle here"));
    }

    #[test]
    fn search_marks_a_single_oversized_file_as_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized-needle.txt");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_SEARCH_BYTES + 1).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let result = run_tool(
            &workspace,
            &FileState::default(),
            "search",
            r#"{"query":"needle"}"#,
        );

        assert!(!result.is_error);
        assert!(result.content.contains("filename match"));
        assert!(result.content.contains(TRUNCATION_MARKER));
    }

    #[test]
    fn cancellation_and_large_directories_stop_at_explicit_bounds() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "content").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let result = execute_blocking(
            &workspace,
            &FileState::default(),
            "read_file",
            r#"{"path":"note.txt"}"#,
            &AtomicBool::new(true),
        );
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));

        for index in 0..=MAX_DIRECTORY_ENTRIES {
            fs::write(directory.path().join(format!("entry-{index}")), "").unwrap();
        }
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "list_dir",
            r#"{"path":"."}"#,
        );
        assert!(result.is_error);
        assert!(result.content.contains("more than"));
    }

    #[test]
    fn edit_replaces_exact_strings_and_refreshes_the_recorded_hash() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("main.rs"),
            "fn one() {}\nfn two() {}\n",
        )
        .unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"main.rs"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"main.rs","old_string":"fn one() {}","new_string":"fn one() { start() }"}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.content);
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs")).unwrap(),
            "fn one() { start() }\nfn two() {}\n"
        );
        let update = edited.file_state.unwrap();
        assert_eq!(update.path, "main.rs");
        assert_eq!(
            update.hash,
            content_hash(b"fn one() { start() }\nfn two() {}\n")
        );

        // The recorded hash was refreshed by the apply, so a follow-up edit
        // needs no intervening read.
        let followup = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"main.rs","old_string":"fn two() {}","new_string":"fn two() { end() }"}"#,
        );
        assert!(!followup.is_error, "unexpected error: {}", followup.content);
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs")).unwrap(),
            "fn one() { start() }\nfn two() { end() }\n"
        );
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("list.txt"), "item\nitem\nitem\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"list.txt"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"list.txt","old_string":"item","new_string":"entry","replace_all":true}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.content);
        assert!(edited.content.contains("3 occurrence"));
        assert_eq!(
            fs::read_to_string(directory.path().join("list.txt")).unwrap(),
            "entry\nentry\nentry\n"
        );
    }

    #[test]
    fn edit_fails_precisely_on_absent_and_ambiguous_old_strings() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("list.txt"), "item\nitem\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"list.txt"}"#);

        let absent = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"list.txt","old_string":"missing","new_string":"other"}"#,
        );
        assert!(absent.is_error);
        assert!(absent.content.contains("not found"), "{}", absent.content);

        let ambiguous = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"list.txt","old_string":"item","new_string":"entry"}"#,
        );
        assert!(ambiguous.is_error);
        assert!(
            ambiguous.content.contains("2 times") && ambiguous.content.contains("replace_all"),
            "{}",
            ambiguous.content
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("list.txt")).unwrap(),
            "item\nitem\n"
        );
    }

    #[test]
    fn edits_and_overwrites_require_a_prior_read_in_this_session() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "content\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let edit = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"note.txt","old_string":"content","new_string":"changed"}"#,
        );
        assert!(edit.is_error);
        assert!(edit.content.contains("read_file"), "{}", edit.content);

        let overwrite = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"note.txt","content":"replaced\n"}"#,
        );
        assert!(overwrite.is_error);
        assert!(
            overwrite.content.contains("read_file"),
            "{}",
            overwrite.content
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "content\n"
        );
    }

    #[test]
    fn stale_files_fail_the_apply_until_they_are_reread() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "original\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"note.txt"}"#);

        // An external writer (editor, another process) changes the file
        // between the read and the apply.
        fs::write(directory.path().join("note.txt"), "external change\n").unwrap();
        let stale = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"note.txt","old_string":"original","new_string":"edited"}"#,
        );
        assert!(stale.is_error);
        assert!(stale.content.contains("changed since"), "{}", stale.content);
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "external change\n"
        );

        run_tool(&workspace, &state, "read_file", r#"{"path":"note.txt"}"#);
        let retried = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"note.txt","old_string":"external change","new_string":"edited"}"#,
        );
        assert!(!retried.is_error, "unexpected error: {}", retried.content);
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn concurrent_sessions_conflict_at_file_granularity() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("shared.txt"), "base\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let winner = FileState::default();
        let loser = FileState::default();
        run_tool(&workspace, &winner, "read_file", r#"{"path":"shared.txt"}"#);
        run_tool(&workspace, &loser, "read_file", r#"{"path":"shared.txt"}"#);

        let won = run_tool(
            &workspace,
            &winner,
            "edit_file",
            r#"{"path":"shared.txt","old_string":"base","new_string":"winner"}"#,
        );
        assert!(!won.is_error, "unexpected error: {}", won.content);

        let lost = run_tool(
            &workspace,
            &loser,
            "edit_file",
            r#"{"path":"shared.txt","old_string":"base","new_string":"loser"}"#,
        );
        assert!(lost.is_error);
        assert!(lost.content.contains("changed since"), "{}", lost.content);

        run_tool(&workspace, &loser, "read_file", r#"{"path":"shared.txt"}"#);
        let reconciled = run_tool(
            &workspace,
            &loser,
            "edit_file",
            r#"{"path":"shared.txt","old_string":"winner","new_string":"reconciled"}"#,
        );
        assert!(
            !reconciled.is_error,
            "unexpected error: {}",
            reconciled.content
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("shared.txt")).unwrap(),
            "reconciled\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_applies_preserve_permissions_and_leave_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("run.sh");
        fs::write(&target, "echo one\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o754)).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run_tool(&workspace, &state, "read_file", r#"{"path":"run.sh"}"#);

        let edited = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"path":"run.sh","old_string":"echo one","new_string":"echo two"}"#,
        );
        assert!(!edited.is_error, "unexpected error: {}", edited.content);
        assert_eq!(fs::read_to_string(&target).unwrap(), "echo two\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754
        );

        let overwritten = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"run.sh","content":"echo three\n"}"#,
        );
        assert!(
            !overwritten.is_error,
            "unexpected error: {}",
            overwritten.content
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754
        );
        let leftovers = fs::read_dir(directory.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".qq-apply-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn write_file_creates_new_files_without_a_prior_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("docs")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let created = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"first\n"}"#,
        );
        assert!(!created.is_error, "unexpected error: {}", created.content);
        assert!(created.content.starts_with("Created"));
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "first\n"
        );
        let update = created.file_state.unwrap();
        assert_eq!(update.path, "docs/NOTES.md");
        assert_eq!(update.hash, content_hash(b"first\n"));

        // The create recorded the written content, so the same session may
        // overwrite it without an intervening read.
        let overwritten = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"docs/NOTES.md","content":"second\n"}"#,
        );
        assert!(
            !overwritten.is_error,
            "unexpected error: {}",
            overwritten.content
        );
        assert!(overwritten.content.starts_with("Wrote"));
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/NOTES.md")).unwrap(),
            "second\n"
        );

        let missing_parent = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"missing/NOTES.md","content":"first\n"}"#,
        );
        assert!(missing_parent.is_error);
    }

    #[test]
    fn edit_and_write_reject_containment_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim.txt"), "untouched\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        for arguments in [
            r#"{"path":"../victim.txt","old_string":"untouched","new_string":"changed"}"#,
            r#"{"path":"/etc/hosts","old_string":"localhost","new_string":"changed"}"#,
        ] {
            let result = run_tool(&workspace, &state, "edit_file", arguments);
            assert!(result.is_error, "escape accepted: {arguments}");
        }
        for arguments in [
            r#"{"path":"../victim.txt","content":"changed\n"}"#,
            r#"{"path":"/tmp/qq-escape.txt","content":"changed\n"}"#,
            r#"{"path":"..","content":"changed\n"}"#,
        ] {
            let result = run_tool(&workspace, &state, "write_file", arguments);
            assert!(result.is_error, "escape accepted: {arguments}");
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
            let through_symlink = run_tool(
                &workspace,
                &state,
                "write_file",
                r#"{"path":"outside/victim.txt","content":"changed\n"}"#,
            );
            assert!(through_symlink.is_error);
            std::os::unix::fs::symlink(
                outside.path().join("victim.txt"),
                directory.path().join("link.txt"),
            )
            .unwrap();
            let onto_symlink = run_tool(
                &workspace,
                &state,
                "edit_file",
                r#"{"path":"link.txt","old_string":"untouched","new_string":"changed"}"#,
            );
            assert!(onto_symlink.is_error);
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
            "untouched\n"
        );
    }
}
