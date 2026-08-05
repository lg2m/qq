use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;

use super::{
    GuidanceError, GuidanceRequest, SelectedGuidance, Workspace, WorkspaceInstructionError,
    WorkspaceInstructions, blocking_permits,
};

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
    #[error(transparent)]
    Guidance(#[from] GuidanceError),
}

pub(crate) async fn prepare_workspace(
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
    guidance: Option<GuidanceRequest>,
) -> Result<(Workspace, WorkspaceInstructions, Option<SelectedGuidance>), WorkspacePreparationError>
{
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
        let instructions = super::instructions::load(&workspace, &cancelled)?;
        let guidance = guidance
            .map(|request| super::guidance::load(&workspace, request, &cancelled))
            .transpose()?;
        Ok((workspace, instructions, guidance))
    })
    .await
    .map_err(|source| WorkspacePreparationError::Stopped { source })?
}
