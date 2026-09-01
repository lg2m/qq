use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use tokio::{io::AsyncReadExt, sync::mpsc};

use crate::workspace::Workspace;

use super::dispatch::ToolExecutionResult;

pub(super) const MAX_SHELL_OUTPUT_BYTES: usize = 128 * 1024;
const SHELL_READ_CHUNK_BYTES: usize = 8 * 1024;
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;
pub(super) const MAX_SHELL_TIMEOUT_SECS: u64 = 600;
const SHELL_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// The fate of one supervised shell command.
enum ShellOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Executes one bounded shell command on the async runtime: `sh -c` in its own
/// process group with the workspace (or a contained subdirectory) as its
/// working directory, combined stdout+stderr captured head+tail within the
/// output budget, live chunks forwarded to `output`, and the whole process
/// group killed on timeout, cancellation, or drop of the in-flight future.
pub(super) async fn run_shell(
    workspace: &Workspace,
    arguments: &ShellArgs,
    cancelled: &AtomicBool,
    output: Option<&mpsc::Sender<String>>,
) -> ToolExecutionResult {
    if cancelled.load(Ordering::Acquire) {
        return ToolExecutionResult::error("tool execution was cancelled");
    }
    if arguments.command.trim().is_empty() {
        return ToolExecutionResult::error("command must not be empty");
    }
    let timeout_seconds = arguments
        .timeout_seconds
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS);
    if timeout_seconds == 0 || timeout_seconds > MAX_SHELL_TIMEOUT_SECS {
        return ToolExecutionResult::error(format!(
            "timeout_seconds must be between 1 and {MAX_SHELL_TIMEOUT_SECS}"
        ));
    }
    let cwd = match &arguments.cwd {
        None => workspace.path().to_owned(),
        Some(requested) => {
            let relative = match workspace.contained_path(requested) {
                Ok(relative) => relative,
                Err(error) => return ToolExecutionResult::error(error.to_string()),
            };
            if !workspace.root().is_dir(&relative) {
                return ToolExecutionResult::error("cwd is not a directory");
            }
            workspace.path().join(relative)
        }
    };

    let mut command = shell_command(&arguments.command);
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ToolExecutionResult::error(format!("could not start the command: {error}"));
        }
    };
    // The child leads its own process group (pgid == pid); the guard kills the
    // entire group on every abnormal exit path — timeout, cancellation, and
    // this future being dropped mid-flight — so no descendant is orphaned.
    let mut guard = ProcessGroupGuard::new(child.id());
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
    let mut capture = BoundedCapture::new(MAX_SHELL_OUTPUT_BYTES);
    let mut streamed = 0_usize;
    let mut stdout_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; SHELL_READ_CHUNK_BYTES];
    let mut cancel_poll = tokio::time::interval(SHELL_CANCEL_POLL);
    cancel_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => break ShellOutcome::TimedOut,
            _ = cancel_poll.tick() => {
                if cancelled.load(Ordering::Acquire) {
                    break ShellOutcome::Cancelled;
                }
            }
            read = read_from(&mut stdout, &mut stdout_buffer), if stdout.is_some() => {
                match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(read) => {
                        forward_shell_chunk(output, &mut streamed, &stdout_buffer[..read]);
                        capture.push(&stdout_buffer[..read]);
                    }
                }
            }
            read = read_from(&mut stderr, &mut stderr_buffer), if stderr.is_some() => {
                match read {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(read) => {
                        forward_shell_chunk(output, &mut streamed, &stderr_buffer[..read]);
                        capture.push(&stderr_buffer[..read]);
                    }
                }
            }
            // The command is done only when both pipes reached end-of-file and
            // the child was reaped; a grandchild that inherited a pipe keeps
            // the call running (until the timeout) so its output is captured.
            status = child.wait(), if stdout.is_none() && stderr.is_none() => {
                // The child is reaped, so its pid (the group id) may be
                // recycled: killing the group now could hit an innocent
                // process. Surviving descendants closed their pipes, which is
                // as detached as the timeout policy requires.
                guard.disarm();
                break ShellOutcome::Exited(status);
            }
        }
    };

    match outcome {
        ShellOutcome::Exited(Ok(status)) => shell_result(capture, status),
        ShellOutcome::Exited(Err(error)) => {
            ToolExecutionResult::error(format!("could not observe the command exit: {error}"))
        }
        ShellOutcome::TimedOut => {
            guard.kill();
            // SIGKILL cannot be caught, so the reap completes promptly.
            let _ = child.wait().await;
            let mut content = capture.into_output();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!(
                "command timed out after {timeout_seconds} s; its process group was killed"
            ));
            ToolExecutionResult::error(content)
        }
        ShellOutcome::Cancelled => {
            guard.kill();
            let _ = child.wait().await;
            ToolExecutionResult::error("tool execution was cancelled")
        }
    }
}

#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("/bin/sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(not(unix))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

/// Reads from a pipe that may already be closed; a closed side never resolves,
/// letting `select!` disable it without re-arming.
async fn read_from<R>(reader: &mut Option<R>, buffer: &mut [u8]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match reader.as_mut() {
        Some(reader) => reader.read(buffer).await,
        None => std::future::pending().await,
    }
}

/// Forwards one raw output chunk as a lossy UTF-8 delta, bounded by the same
/// budget as the captured result so a runaway command cannot flood clients.
fn forward_shell_chunk(output: Option<&mpsc::Sender<String>>, streamed: &mut usize, bytes: &[u8]) {
    let Some(sender) = output else {
        return;
    };
    if *streamed >= MAX_SHELL_OUTPUT_BYTES {
        return;
    }
    let remaining = MAX_SHELL_OUTPUT_BYTES - *streamed;
    let bytes = &bytes[..bytes.len().min(remaining)];
    let chunk = String::from_utf8_lossy(bytes).into_owned();
    match sender.try_send(chunk) {
        Ok(()) => *streamed += bytes.len(),
        // Live output is a best-effort view of the same bounded bytes carried
        // by the terminal result. A slow renderer may miss deltas, but it can
        // never stall process timeout, cancellation, or final persistence.
        Err(mpsc::error::TrySendError::Full(_)) => *streamed += bytes.len(),
        Err(mpsc::error::TrySendError::Closed(_)) => *streamed = MAX_SHELL_OUTPUT_BYTES,
    }
}

fn shell_result(capture: BoundedCapture, status: std::process::ExitStatus) -> ToolExecutionResult {
    let mut content = capture.into_output();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    match status.code() {
        Some(0) => {
            content.push_str("exit code: 0");
            ToolExecutionResult::success(content)
        }
        Some(code) => {
            content.push_str(&format!("exit code: {code}"));
            ToolExecutionResult::error(content)
        }
        None => {
            #[cfg(unix)]
            let detail = {
                use std::os::unix::process::ExitStatusExt;
                status
                    .signal()
                    .map(|signal| format!("command was terminated by signal {signal}"))
            };
            #[cfg(not(unix))]
            let detail: Option<String> = None;
            content.push_str(
                &detail.unwrap_or_else(|| "command was terminated without an exit code".to_owned()),
            );
            ToolExecutionResult::error(content)
        }
    }
}

/// Kills a spawned command's whole process group when dropped, unless
/// disarmed after a normal exit. Kill-on-drop is what guarantees run
/// cancellation leaves no orphaned children even though cancellation drops
/// the in-flight execution future without polling it further.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
}

impl ProcessGroupGuard {
    fn new(child_id: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: child_id
                    .and_then(|id| i32::try_from(id).ok())
                    .and_then(rustix::process::Pid::from_raw),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child_id;
            Self {}
        }
    }

    /// Forgets the group after a normal exit; the reaped leader's pid may be
    /// recycled, so killing the group then would be unsound.
    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Bounded head+tail capture of combined command output: the first half of
/// the budget keeps the start of the output, the second half keeps a rolling
/// window of its end, and everything between is counted as omitted.
pub(super) struct BoundedCapture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: u64,
    half: usize,
}

impl BoundedCapture {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            half: budget / 2,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let head_room = self.half.saturating_sub(self.head.len());
        let take = head_room.min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        let rest = &bytes[take..];
        if rest.is_empty() {
            return;
        }
        self.tail.extend(rest.iter().copied());
        if self.tail.len() > self.half {
            let excess = self.tail.len() - self.half;
            self.tail.drain(..excess);
        }
    }

    pub(super) fn into_output(self) -> String {
        let omitted = self.total - self.head.len() as u64 - self.tail.len() as u64;
        let tail = self.tail.into_iter().collect::<Vec<_>>();
        if omitted == 0 {
            let mut bytes = self.head;
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        format!(
            "{}\n...[truncated by qq: {omitted} bytes omitted]...\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&tail),
        )
    }
}
