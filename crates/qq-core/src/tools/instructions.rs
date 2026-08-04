use std::{
    io::{ErrorKind, Read},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use qq_protocol::InstructionHash;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{Workspace, WorkspacePathError, blocking_permits, contained_path};

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const MAX_INSTRUCTION_FILE_BYTES: usize = 64 * 1024;
#[cfg(test)]
static TEST_WORKSPACE_OPEN_HOOK: std::sync::OnceLock<
    std::sync::Mutex<Option<TestWorkspaceOpenHook>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
struct TestWorkspaceOpenHook {
    target: std::sync::Weak<AtomicBool>,
    opened: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct TestWorkspaceOpenPause {
    opened: std::sync::mpsc::Receiver<()>,
    resume: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
pub(crate) fn test_pause_after_workspace_open(
    cancelled: &Arc<AtomicBool>,
) -> TestWorkspaceOpenPause {
    let (opened_sender, opened) = std::sync::mpsc::sync_channel(1);
    let (resume, resume_receiver) = std::sync::mpsc::sync_channel(1);
    let mut hook = TEST_WORKSPACE_OPEN_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap();
    assert!(
        hook.is_none(),
        "a workspace-open test hook is already active"
    );
    *hook = Some(TestWorkspaceOpenHook {
        target: Arc::downgrade(cancelled),
        opened: opened_sender,
        resume: resume_receiver,
    });
    TestWorkspaceOpenPause { opened, resume }
}

#[cfg(test)]
impl TestWorkspaceOpenPause {
    pub(crate) fn wait_until_opened(&self) -> Result<(), std::sync::mpsc::RecvTimeoutError> {
        self.opened.recv_timeout(std::time::Duration::from_secs(5))
    }

    pub(crate) fn resume(self) -> Result<(), std::sync::mpsc::SendError<()>> {
        self.resume.send(())
    }
}

#[cfg(test)]
fn pause_after_workspace_open(cancelled: &Arc<AtomicBool>) {
    let hook = {
        let mut slot = TEST_WORKSPACE_OPEN_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap();
        let matches = slot
            .as_ref()
            .is_some_and(|hook| hook.target.as_ptr() == Arc::as_ptr(cancelled));
        matches.then(|| slot.take().unwrap())
    };
    if let Some(hook) = hook
        && hook.opened.send(()).is_ok()
    {
        let _ = hook.resume.recv_timeout(std::time::Duration::from_secs(5));
    }
}

#[derive(Debug, Error)]
pub(crate) enum WorkspacePreparationError {
    #[error("workspace preparation was cancelled")]
    Cancelled,
    #[error("workspace preparation executor is unavailable")]
    Unavailable {
        #[source]
        source: tokio::sync::AcquireError,
    },
    #[error("workspace preparation stopped unexpectedly")]
    Stopped {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("could not resolve workspace path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not open workspace path {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Instructions(#[from] WorkspaceInstructionError),
}

#[derive(Debug, Error)]
pub(crate) enum WorkspaceInstructionError {
    #[error("workspace instruction loading was cancelled")]
    Cancelled,
    #[error("could not inspect workspace instruction {path}: {source}")]
    Inspect {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} could not be resolved: {source}")]
    Resolve {
        path: &'static str,
        #[source]
        source: WorkspacePathError,
    },
    #[error("workspace instruction {path} is not a regular file")]
    NotAFile { path: &'static str },
    #[error("workspace instruction {path} exceeds the {limit}-byte file limit")]
    FileTooLarge { path: &'static str, limit: usize },
    #[error("could not read workspace instruction {path}: {source}")]
    Read {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} is not valid UTF-8")]
    InvalidUtf8 { path: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceInstructions {
    selected: Option<SelectedInstruction>,
    hash: InstructionHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedInstruction {
    path: &'static str,
    content: String,
}

impl WorkspaceInstructions {
    pub(crate) fn empty() -> Self {
        Self {
            selected: None,
            hash: hash_instruction(None),
        }
    }

    pub(crate) fn hash(&self) -> InstructionHash {
        self.hash
    }

    pub(crate) fn append_to_prompt(&self, prompt: &mut String) {
        let Some(selected) = &self.selected else {
            return;
        };
        prompt.push_str("\n\nWorkspace instructions from ");
        prompt.push_str(selected.path);
        prompt.push_str(":\n--- BEGIN WORKSPACE INSTRUCTIONS ---\n");
        prompt.push_str(&selected.content);
        if !selected.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END WORKSPACE INSTRUCTIONS ---");
    }
}

pub(crate) async fn prepare_workspace(
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
) -> Result<(Workspace, WorkspaceInstructions), WorkspacePreparationError> {
    let permit = blocking_permits()
        .acquire_owned()
        .await
        .map_err(|source| WorkspacePreparationError::Unavailable { source })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.load(Ordering::Acquire) {
            return Err(WorkspacePreparationError::Cancelled);
        }
        let canonical = std::fs::canonicalize(&path).map_err(|source| {
            WorkspacePreparationError::Canonicalize {
                path: path.clone(),
                source,
            }
        })?;
        let workspace =
            Workspace::open(&canonical).map_err(|source| WorkspacePreparationError::Open {
                path: canonical,
                source,
            })?;
        #[cfg(test)]
        pause_after_workspace_open(&cancelled);
        if cancelled.load(Ordering::Acquire) {
            return Err(WorkspacePreparationError::Cancelled);
        }
        let instructions = load(&workspace, &cancelled)?;
        Ok((workspace, instructions))
    })
    .await
    .map_err(|source| WorkspacePreparationError::Stopped { source })?
}

fn load(
    workspace: &Workspace,
    cancelled: &AtomicBool,
) -> Result<WorkspaceInstructions, WorkspaceInstructionError> {
    for path in [AGENTS_FILE, CLAUDE_FILE] {
        let Some(bytes) = read_candidate(workspace, path, cancelled)? else {
            continue;
        };
        let content = String::from_utf8(bytes)
            .map_err(|_| WorkspaceInstructionError::InvalidUtf8 { path })?;
        let selected = SelectedInstruction { path, content };
        return Ok(WorkspaceInstructions {
            hash: hash_instruction(Some(&selected)),
            selected: Some(selected),
        });
    }
    Ok(WorkspaceInstructions::empty())
}

fn read_candidate(
    workspace: &Workspace,
    path: &'static str,
    cancelled: &AtomicBool,
) -> Result<Option<Vec<u8>>, WorkspaceInstructionError> {
    match workspace.root.symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WorkspaceInstructionError::Inspect { path, source }),
    }
    let resolved = contained_path(workspace, path)
        .map_err(|source| WorkspaceInstructionError::Resolve { path, source })?;
    let metadata = workspace
        .root
        .metadata(&resolved)
        .map_err(|source| WorkspaceInstructionError::Inspect { path, source })?;
    if !metadata.is_file() {
        return Err(WorkspaceInstructionError::NotAFile { path });
    }
    if metadata.len() > MAX_INSTRUCTION_FILE_BYTES as u64 {
        return Err(WorkspaceInstructionError::FileTooLarge {
            path,
            limit: MAX_INSTRUCTION_FILE_BYTES,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkspaceInstructionError::Cancelled);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    workspace
        .root
        .open(&resolved)
        .map_err(|source| WorkspaceInstructionError::Read { path, source })?
        .take(MAX_INSTRUCTION_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| WorkspaceInstructionError::Read { path, source })?;
    if bytes.len() > MAX_INSTRUCTION_FILE_BYTES {
        return Err(WorkspaceInstructionError::FileTooLarge {
            path,
            limit: MAX_INSTRUCTION_FILE_BYTES,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkspaceInstructionError::Cancelled);
    }
    Ok(Some(bytes))
}

fn hash_instruction(selected: Option<&SelectedInstruction>) -> InstructionHash {
    let mut digest = Sha256::new();
    if let Some(selected) = selected {
        let path = selected.path.as_bytes();
        digest.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(path);
        let content = selected.content.as_bytes();
        digest.update(
            u64::try_from(content.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(content);
    }
    InstructionHash::from_bytes(digest.finalize().into())
}
