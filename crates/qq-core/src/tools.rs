#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
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
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_SEARCH_ENTRIES: usize = 20_000;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_FILES: usize = 10_000;
const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BLOCKING_TOOL_TASKS: usize = 8;
const TRUNCATION_MARKER: &str = "\n...[truncated by qq]\n";
static BLOCKING_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
#[cfg(test)]
static TEST_EXECUTIONS_STARTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_EXECUTION_BARRIER: OnceLock<std::sync::Barrier> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Workspace {
    root: Arc<Dir>,
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
        })
    }
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
    #[cfg(test)]
    TestDelay,
}

impl BuiltInTool {
    const ALL: [Self; 3] = [Self::ReadFile, Self::ListDir, Self::Search];

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "read_file" => Some(Self::ReadFile),
            "list_dir" => Some(Self::ListDir),
            "search" => Some(Self::Search),
            #[cfg(test)]
            "__test_delay" => Some(Self::TestDelay),
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
            #[cfg(test)]
            Self::TestDelay => unreachable!("test tools are not advertised"),
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
        execute_blocking(&workspace, &name, &arguments, &cancelled)
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
}

impl ToolExecutionResult {
    fn success(content: String) -> Self {
        Self {
            content: truncate_result(content),
            is_error: false,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            content: truncate_result(message.into()),
            is_error: true,
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
            read_file(workspace, arguments, cancelled)
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
        None => ToolExecutionResult::error(format!("unknown tool {name:?}")),
    }
}

fn read_file(
    workspace: &Workspace,
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
    let mut reader = BufReader::new(file).take(MAX_READ_SCAN_BYTES);
    let mut output = String::new();
    let mut line = Vec::new();
    let end = arguments.offset.saturating_add(arguments.limit);
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
        if line_number >= arguments.offset {
            let text = match std::str::from_utf8(&line) {
                Ok(text) => text,
                Err(_) => return ToolExecutionResult::error("file is not valid UTF-8"),
            };
            output.push_str(text);
            if output.len() > MAX_TOOL_RESULT_BYTES {
                return ToolExecutionResult::success(output);
            }
        }
    }
    if reader.limit() == 0 {
        output.push_str(TRUNCATION_MARKER);
    }
    ToolExecutionResult::success(output)
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
        if matches.len() >= MAX_SEARCH_RESULTS
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

fn truncate_result(mut content: String) -> String {
    if content.len() <= MAX_TOOL_RESULT_BYTES {
        return content;
    }
    let marker_bytes = TRUNCATION_MARKER.len();
    let mut end = MAX_TOOL_RESULT_BYTES.saturating_sub(marker_bytes);
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

    #[test]
    fn read_and_list_are_bounded_and_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("b.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(directory.path().join("a.txt"), "a").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let cancelled = AtomicBool::new(false);
        let listed = execute_blocking(&workspace, "list_dir", r#"{"path":"."}"#, &cancelled);
        assert_eq!(listed.content, "a.txt\nb.txt\n");

        let read = execute_blocking(
            &workspace,
            "read_file",
            r#"{"path":"b.txt","offset":2,"limit":1}"#,
            &cancelled,
        );
        assert_eq!(read.content, "two\n");
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

        let result = execute_blocking(
            &workspace,
            "read_file",
            r#"{"path":"large.txt"}"#,
            &AtomicBool::new(false),
        );

        assert!(!result.is_error);
        assert!(result.content.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(result.content.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn rejects_parent_and_symlink_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "hidden").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let cancelled = AtomicBool::new(false);
        let parent = execute_blocking(
            &workspace,
            "read_file",
            r#"{"path":"../secret"}"#,
            &cancelled,
        );
        assert!(parent.is_error);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
            let symlink = execute_blocking(
                &workspace,
                "read_file",
                r#"{"path":"outside/secret"}"#,
                &cancelled,
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

        let result = execute_blocking(
            &workspace,
            "search",
            r#"{"query":"needle"}"#,
            &AtomicBool::new(false),
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

        let result = execute_blocking(
            &workspace,
            "search",
            r#"{"query":"needle"}"#,
            &AtomicBool::new(false),
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
        let cancelled = AtomicBool::new(true);
        let result = execute_blocking(
            &workspace,
            "read_file",
            r#"{"path":"note.txt"}"#,
            &cancelled,
        );
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));

        for index in 0..=MAX_DIRECTORY_ENTRIES {
            fs::write(directory.path().join(format!("entry-{index}")), "").unwrap();
        }
        let result = execute_blocking(
            &workspace,
            "list_dir",
            r#"{"path":"."}"#,
            &AtomicBool::new(false),
        );
        assert!(result.is_error);
        assert!(result.content.contains("more than"));
    }
}
