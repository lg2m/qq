use std::{
    future::Future,
    io::{self, stdout},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Attribute, Print, ResetColor, SetAttribute},
    terminal::{self, Clear, ClearType, EndSynchronizedUpdate},
};
use futures_util::{Stream, StreamExt};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    time::{Duration, MissedTickBehavior, interval},
};

use crate::{
    ClientPort, ClientRequest, ClientUpdate,
    app::{App, TuiError},
    view::FrameRenderer,
};

/// Interval between frame ticks. Frames are only drawn when state changed.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(8);
const ANIMATION_INTERVAL: Duration = Duration::from_millis(125);

pub async fn run<P>(client: P, app: App) -> Result<(), TuiError>
where
    P: ClientPort,
{
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        result = &mut shutdown => return result.map_err(TuiError::from),
        _ = tokio::task::yield_now() => {}
    }

    let _terminal = TerminalGuard::enter()?;
    let events = EventStream::new();
    let output = tokio::io::stdout();
    run_loop(
        client,
        app,
        events,
        output,
        terminal::size,
        shutdown,
        external_editor,
    )
    .await
    .map(drop)
}

/// Edit `draft` in `$VISUAL` or `$EDITOR`. The terminal leaves raw mode and
/// its input modes for the editor's lifetime; the caller redraws afterwards.
/// Runs on the blocking pool because the editor owns the TTY until it exits.
/// `None` means the editor was unavailable, failed, or left the text
/// unchanged.
fn external_editor(draft: String) -> EditorFuture {
    Box::pin(async move {
        let Some(command) = std::env::var_os("VISUAL")
            .or_else(|| std::env::var_os("EDITOR"))
            .filter(|value| !value.is_empty())
        else {
            return Err(EditorError::NotConfigured);
        };
        let mut output = stdout();
        // Leave the alternate-screen chrome intact but hand the TTY over.
        let _ = disable_input_modes(&mut output);
        let _ = execute!(output, Show);
        let _ = terminal::disable_raw_mode();
        let result = tokio::task::spawn_blocking(move || {
            let file = tempfile::Builder::new()
                .prefix("qq-draft-")
                .suffix(".md")
                .tempfile()
                .map_err(EditorError::Io)?;
            std::fs::write(file.path(), &draft).map_err(EditorError::Io)?;
            // `$EDITOR` may carry arguments (`code --wait`); split on
            // whitespace, which matches how shells treat the variable.
            let command = command.to_string_lossy();
            let mut parts = command.split_whitespace();
            let program = parts.next().ok_or(EditorError::NotConfigured)?;
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(file.path())
                .status()
                .map_err(EditorError::Io)?;
            if !status.success() {
                return Err(EditorError::Exited(status.code()));
            }
            let edited = std::fs::read_to_string(file.path()).map_err(EditorError::Io)?;
            Ok((edited != draft).then_some(edited))
        })
        .await
        .unwrap_or(Err(EditorError::NotConfigured));
        let _ = terminal::enable_raw_mode();
        let _ = enable_input_modes(&mut output);
        let _ = execute!(output, Hide, Clear(ClearType::All), MoveTo(0, 0));
        result
    })
}

/// Why an external edit produced no text.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EditorError {
    #[error("set $VISUAL or $EDITOR to edit the draft externally")]
    NotConfigured,
    #[error("the editor exited with status {}", .0.map_or("unknown".to_owned(), |code| code.to_string()))]
    Exited(Option<i32>),
    #[error("the editor could not run: {0}")]
    Io(io::Error),
}

pub(crate) type EditorFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<Option<String>, EditorError>> + Send>>;

/// When the next frame is drawn relative to pending state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Redraw {
    /// Nothing changed since the last frame.
    Clean,
    /// Streamed or background changes; coalesce until the frame tick.
    Scheduled,
    /// User input; draw before waiting on anything else so typing echoes
    /// without a tick's latency.
    Immediate,
}

/// The event loop with every terminal dependency injected so it runs without a
/// TTY in tests and benchmarks. Returns the final application state so callers
/// can inspect it after the loop exits.
///
/// `size` is queried once at start and again after each `Resize` event rather
/// than every frame.
pub(crate) async fn run_loop<P, E, W, S, F, X>(
    mut client: P,
    mut app: App,
    mut terminal_events: E,
    mut output: W,
    mut size: S,
    shutdown: F,
    mut editor: X,
) -> Result<App, TuiError>
where
    P: ClientPort,
    E: Stream<Item = io::Result<Event>> + Unpin,
    W: AsyncWrite + Unpin,
    S: FnMut() -> io::Result<(u16, u16)>,
    F: Future<Output = io::Result<()>>,
    X: FnMut(String) -> EditorFuture,
{
    tokio::pin!(shutdown);
    let mut renderer = FrameRenderer::default();
    let mut frame_tick = interval(FRAME_INTERVAL);
    let mut animation_tick = interval(ANIMATION_INTERVAL);
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    animation_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut terminal_size = size()?;
    let mut redraw = Redraw::Immediate;

    loop {
        // An external edit owns the terminal until it returns; nothing else
        // can usefully happen meanwhile, so it runs inline here rather than
        // as a select arm. Client updates queue in the channel and drain
        // afterwards.
        if let Some(draft) = app.take_editor_request() {
            match editor(draft).await {
                Ok(text) => {
                    app.apply_editor_result(text);
                }
                Err(error) => {
                    app.note_editor_failure(&error.to_string());
                }
            }
            renderer.invalidate();
            redraw = Redraw::Immediate;
        }
        if let Some(attention) = app.take_attention() {
            output.write_all(&attention_bytes(&attention)).await?;
            output.flush().await?;
        }
        if redraw == Redraw::Immediate {
            let bytes = renderer.draw(&mut app, terminal_size)?;
            output.write_all(&bytes).await?;
            output.flush().await?;
            redraw = Redraw::Clean;
            // The tick would otherwise fire right after this frame for a
            // change already drawn.
            frame_tick.reset();
        }
        tokio::select! {
            biased;
            result = &mut shutdown => {
                result?;
                break;
            }
            event = terminal_events.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Event::Resize(columns, rows) = event {
                            terminal_size = (columns, rows);
                        }
                        let (changed, requests) = app.handle_terminal_event(event);
                        if changed {
                            redraw = Redraw::Immediate;
                        }
                        for request in requests {
                            if let Err(error) = client.try_send(request.clone())
                                && apply_send_failure(&mut app, request, error)
                            {
                                redraw = Redraw::Immediate;
                            }
                        }
                    }
                    Some(Err(error)) => return Err(TuiError::Terminal(error)),
                    None => break,
                }
            }
            update = client.recv() => {
                let Some(update) = update else {
                    return Err(TuiError::ClientStopped);
                };
                if app.apply_client_update(update) {
                    redraw = redraw.max(Redraw::Scheduled);
                }
                for request in app.take_requests() {
                    if let Err(error) = client.try_send(request.clone())
                        && apply_send_failure(&mut app, request, error)
                    {
                        redraw = redraw.max(Redraw::Scheduled);
                    }
                }
            }
            highlighted = renderer.highlighter.next() => {
                if renderer.apply_highlight(highlighted) {
                    redraw = redraw.max(Redraw::Scheduled);
                }
            }
            _ = animation_tick.tick(), if app.has_activity() => {
                if app.advance_animation() {
                    redraw = redraw.max(Redraw::Scheduled);
                }
            }
            _ = frame_tick.tick(), if redraw != Redraw::Clean => {
                let bytes = renderer.draw(&mut app, terminal_size)?;
                output.write_all(&bytes).await?;
                output.flush().await?;
                redraw = Redraw::Clean;
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(app)
}

/// Terminal bytes that ask for the user's attention: BEL for the terminal's
/// own bell or visual flash, then an OSC 9 desktop notification, which
/// iTerm2, WezTerm, kitty, ghostty, and Windows Terminal show and every
/// other terminal ignores. Text is scrubbed so a session title cannot
/// terminate or extend the escape sequence.
fn attention_bytes(attention: &crate::app::Attention) -> Vec<u8> {
    let text: String = attention
        .summary()
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    format!("\x07\x1b]9;{text}\x07").into_bytes()
}

fn apply_send_failure(app: &mut App, request: ClientRequest, error: crate::ClientFailure) -> bool {
    let update = match request {
        ClientRequest::Command(command) => ClientUpdate::CommandResult {
            command_id: command.command_id,
            result: Err(error),
        },
        ClientRequest::Snapshot(_) => ClientUpdate::SnapshotFailed(error),
    };
    app.apply_client_update(update)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        let mut output = stdout();
        enable_input_modes(&mut output)?;
        execute!(output, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let mut output = stdout();
        let _ = execute!(
            output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            EndSynchronizedUpdate
        );
        let _ = disable_input_modes(&mut output);
        let _ = execute!(
            output,
            MoveTo(0, height.saturating_sub(1)),
            Clear(ClearType::CurrentLine),
            Show,
            Print("\r\n")
        );
    }
}

fn enable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    // Kitty keyboard progressive enhancement lets compatible terminals report
    // modified keys such as Shift-Enter. Unsupported terminals ignore the CSI.
    // Always push/pop rather than probing: the support query blocks on stdin
    // and races the async event loop.
    execute!(
        output,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange
    )
}

fn disable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    execute!(
        output,
        DisableFocusChange,
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    )
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use qq_protocol::{
        EventCursor, MessageId, MessageRole, MessageSnapshot, MessageState, RunId, SessionEvent,
        SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SessionSummary, StoreId,
        TextChannel, WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::{ClientFailure, ConnectionState, TuiOptions};

    /// Deterministic `ClientPort`: updates are fed from a channel, requests are
    /// recorded, and `try_send` fails once the configured capacity is reached.
    struct FakePort {
        updates: mpsc::UnboundedReceiver<ClientUpdate>,
        sent: Arc<Mutex<Vec<ClientRequest>>>,
        capacity: usize,
    }

    impl ClientPort for FakePort {
        fn try_send(&self, request: ClientRequest) -> Result<(), ClientFailure> {
            let mut sent = self.sent.lock().expect("request log lock");
            if sent.len() >= self.capacity {
                return Err(ClientFailure::new("request queue is full"));
            }
            sent.push(request);
            Ok(())
        }

        async fn recv(&mut self) -> Option<ClientUpdate> {
            self.updates.recv().await
        }
    }

    /// A shared async byte sink that records every frame written by the loop.
    #[derive(Clone, Default)]
    struct FrameLog(Arc<Mutex<Vec<Vec<u8>>>>);

    impl FrameLog {
        fn frames(&self) -> Vec<Vec<u8>> {
            self.0.lock().expect("frame log lock").clone()
        }
    }

    impl AsyncWrite for FrameLog {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().expect("frame log lock").push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Adapts an unbounded receiver into the `Stream` the loop expects.
    struct EventQueue(mpsc::UnboundedReceiver<io::Result<Event>>);

    impl Stream for EventQueue {
        type Item = io::Result<Event>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    /// Next scripted editor outcome, plus every draft the loop handed over.
    type EditorScript = Arc<Mutex<(Option<Result<Option<String>, EditorError>>, Vec<String>)>>;

    struct Harness {
        updates: mpsc::UnboundedSender<ClientUpdate>,
        events: mpsc::UnboundedSender<io::Result<Event>>,
        sent: Arc<Mutex<Vec<ClientRequest>>>,
        frames: FrameLog,
        editor_script: EditorScript,
        port: Option<FakePort>,
        event_stream: Option<EventQueue>,
    }

    impl Harness {
        fn new(capacity: usize) -> Self {
            let (updates, update_rx) = mpsc::unbounded_channel();
            let (events, event_rx) = mpsc::unbounded_channel();
            let sent = Arc::new(Mutex::new(Vec::new()));
            Self {
                updates,
                events,
                sent: Arc::clone(&sent),
                frames: FrameLog::default(),
                editor_script: Arc::new(Mutex::new((None, Vec::new()))),
                port: Some(FakePort {
                    updates: update_rx,
                    sent,
                    capacity,
                }),
                event_stream: Some(EventQueue(event_rx)),
            }
        }

        fn key(&self, code: KeyCode, modifiers: KeyModifiers) {
            self.events
                .send(Ok(Event::Key(KeyEvent::new(code, modifiers))))
                .expect("event channel open");
        }

        fn update(&self, update: ClientUpdate) {
            self.updates.send(update).expect("update channel open");
        }

        /// Spawn the loop on the paused runtime. Test steps then interleave
        /// sends with [`Harness::settle`] so ordering is explicit rather than a
        /// consequence of `select!` bias.
        fn spawn(&mut self, app: App) -> tokio::task::JoinHandle<Result<App, TuiError>> {
            let port = self.port.take().expect("harness runs once");
            let events = self.event_stream.take().expect("harness runs once");
            let frames = self.frames.clone();
            let scripted = std::sync::Arc::clone(&self.editor_script);
            tokio::spawn(run_loop(
                port,
                app,
                events,
                frames,
                || Ok((100, 30)),
                std::future::pending(),
                move |draft| {
                    let scripted = std::sync::Arc::clone(&scripted);
                    Box::pin(async move {
                        let mut guard = scripted.lock().expect("editor script lock");
                        guard.1.push(draft);
                        guard.0.take().unwrap_or(Err(EditorError::NotConfigured))
                    })
                },
            ))
        }

        /// Advance paused time far enough for pending input to be handled and
        /// any dirty frame to be drawn.
        async fn settle(&self) {
            // Let the loop consume queued input, then let a frame tick fire,
            // then let it draw.
            tokio::task::yield_now().await;
            tokio::time::advance(FRAME_INTERVAL * 3).await;
            tokio::task::yield_now().await;
        }

        fn quit(&self) {
            self.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        }

        fn sent(&self) -> Vec<ClientRequest> {
            self.sent.lock().expect("request log lock").clone()
        }
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::from_bytes([1; 16])
    }

    fn session_id() -> SessionId {
        SessionId::from_bytes([2; 16])
    }

    fn summary() -> SessionSummary {
        SessionSummary {
            activity: None,
            spawned_by: None,
            id: session_id(),
            workspace_id: workspace_id(),
            parent_id: None,
            title: "Session".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 1,
            last_outcome: None,
        }
    }

    fn snapshot(sequence: u64, messages: Vec<MessageSnapshot>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            included: Vec::new(),
            cursor: EventCursor {
                store_id: StoreId::from_bytes([3; 16]),
                workspace_id: workspace_id(),
                sequence,
            },
            workspace: WorkspaceSummary {
                id: workspace_id(),
                path: "/workspace".to_owned(),
            },
            sessions: vec![summary()],
            focused: Some(SessionSnapshot {
                summary: summary(),
                messages,
                runs: Vec::new(),
                tool_calls: Vec::new(),
                has_older_tool_calls: false,
                has_older_messages: false,
            }),
            has_older_sessions: false,
        }
    }

    fn assistant_message(byte: u8, output: &str) -> MessageSnapshot {
        MessageSnapshot {
            id: MessageId::from_bytes([byte; 16]),
            session_id: session_id(),
            run_id: RunId::from_bytes([9; 16]),
            turn_ordinal: 1,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            output: output.to_owned(),
            refusal: String::new(),
            created_at_ms: 1,
        }
    }

    fn envelope(sequence: u64, event: SessionEvent) -> SessionEventEnvelope {
        SessionEventEnvelope {
            cursor: EventCursor {
                store_id: StoreId::from_bytes([3; 16]),
                workspace_id: workspace_id(),
                sequence,
            },
            session_id: session_id(),
            run_id: Some(RunId::from_bytes([9; 16])),
            caused_by: None,
            occurred_at_ms: 1,
            event,
        }
    }

    fn frame_text(frame: &[u8]) -> String {
        String::from_utf8_lossy(frame).into_owned()
    }

    #[test]
    fn terminal_input_modes_enable_and_restore_keyboard_mouse_and_paste() {
        let mut entered = Vec::new();
        let mut restored = Vec::new();

        enable_input_modes(&mut entered).unwrap();
        disable_input_modes(&mut restored).unwrap();

        let entered = String::from_utf8(entered).unwrap();
        let restored = String::from_utf8(restored).unwrap();
        // Kitty keyboard protocol: DISAMBIGUATE | REPORT_EVENT_TYPES => 3
        assert!(entered.contains("\x1b[>3u"));
        assert!(entered.contains("\x1b[?1000h"));
        assert!(entered.contains("\x1b[?2004h"));
        assert!(restored.contains("\x1b[?1000l"));
        assert!(restored.contains("\x1b[?2004l"));
        assert!(restored.contains("\x1b[<1u"));
    }

    #[tokio::test(start_paused = true)]
    async fn loop_draws_first_frame_then_only_when_state_changes() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.settle().await;
        assert_eq!(harness.frames.frames().len(), 1, "initial frame");

        // Idle: many ticks pass and nothing is redrawn.
        tokio::time::advance(FRAME_INTERVAL * 20).await;
        tokio::task::yield_now().await;
        assert_eq!(harness.frames.frames().len(), 1, "no redraw while idle");

        // A state change redraws exactly once.
        harness.update(ClientUpdate::Connection(ConnectionState::Live));
        harness.settle().await;
        let frames = harness.frames.frames();
        assert_eq!(frames.len(), 2, "one frame per dirty interval");
        assert!(frame_text(&frames[0]).contains("connecting"));
        assert!(!frame_text(&frames[1]).contains("connecting"));

        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");
        assert!(app.quit);
    }

    #[tokio::test(start_paused = true)]
    async fn loop_records_send_failures_as_local_updates() {
        // Capacity zero: every outbound request fails at the port, which the
        // loop must turn into a local failure update instead of dropping it.
        let mut harness = Harness::new(0);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.update(ClientUpdate::Connection(ConnectionState::Live));
        harness.settle().await;

        harness.key(KeyCode::Char('h'), KeyModifiers::NONE);
        harness.key(KeyCode::Char('i'), KeyModifiers::NONE);
        harness.key(KeyCode::Enter, KeyModifiers::NONE);
        harness.settle().await;
        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");

        assert!(harness.sent().is_empty());
        // The rejected submit is restored into the composer for the user.
        assert_eq!(app.composer.text, "hi");
        let (status, _) = app.visible_status().expect("failure notice is shown");
        assert!(status.contains("request queue is full"), "{status}");
    }

    #[tokio::test(start_paused = true)]
    async fn loop_sends_requests_when_the_port_accepts_them() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.update(ClientUpdate::Connection(ConnectionState::Live));
        harness.settle().await;

        harness.key(KeyCode::Char('h'), KeyModifiers::NONE);
        harness.key(KeyCode::Enter, KeyModifiers::NONE);
        harness.settle().await;
        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");

        let sent = harness.sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(matches!(sent[0], ClientRequest::Command(_)));
        assert!(app.composer.text.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn loop_marks_replaying_on_sequence_gap_and_recovers_from_reset_snapshot() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.update(ClientUpdate::Connection(ConnectionState::Live));
        harness.update(ClientUpdate::Event(envelope(
            2,
            SessionEvent::AssistantMessageStarted {
                message: assistant_message(7, ""),
            },
        )));
        // Sequence 3 is missing: the loop must surface a replay state rather
        // than silently render a transcript with a hole in it.
        harness.update(ClientUpdate::Event(envelope(
            4,
            SessionEvent::TextAppended {
                message_id: MessageId::from_bytes([7; 16]),
                channel: TextChannel::Output,
                text: "late".to_owned(),
            },
        )));
        harness.settle().await;
        let frames = harness.frames.frames();
        assert!(
            frames
                .last()
                .is_some_and(|frame| frame_text(frame).contains("reconnecting")),
            "the latest frame should show the replay state"
        );

        // The client recovers by replacing every loaded body with a reset
        // snapshot at the new cursor.
        let mut complete = assistant_message(7, "late");
        complete.state = MessageState::Complete;
        harness.update(ClientUpdate::ResetSnapshot(snapshot(4, vec![complete])));
        harness.update(ClientUpdate::Connection(ConnectionState::Live));
        harness.settle().await;
        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");

        assert_eq!(app.connection, ConnectionState::Live);
        let session = app.sessions.get(&session_id()).expect("session loaded");
        let messages = session.messages.as_ref().expect("body loaded");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].output, "late");
        assert_eq!(messages[0].state, MessageState::Complete);
        assert!(
            !frame_text(harness.frames.frames().last().expect("frame")).contains("reconnecting")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn loop_reports_client_stop() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        drop(std::mem::replace(
            &mut harness.updates,
            mpsc::unbounded_channel().0,
        ));
        let result = task.await.expect("loop task");
        assert!(matches!(result, Err(TuiError::ClientStopped)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loop_upgrades_completed_code_to_highlighted_off_the_render_path() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        let mut message = assistant_message(7, "```rust\nlet x = 1;\n```");
        message.state = MessageState::Complete;
        harness.update(ClientUpdate::Snapshot(snapshot(1, vec![message])));
        harness.update(ClientUpdate::Connection(ConnectionState::Live));

        // Real time here: the highlight runs on the blocking pool. Poll until a
        // frame carries the keyword color (crossterm encodes `Magenta` as
        // `38;5;13`), bounded so a regression fails fast. Frames are row
        // diffs, so the upgraded frame holds only the code row.
        let keyword = "\x1b[38;5;13m\x1b[48;2;38;40;48mlet";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut plain_seen = false;
        let mut highlighted_seen = false;
        while std::time::Instant::now() < deadline && !highlighted_seen {
            tokio::time::sleep(FRAME_INTERVAL).await;
            for frame in harness.frames.frames() {
                let text = frame_text(&frame);
                if text.contains(keyword) {
                    highlighted_seen = true;
                } else if text.contains("let x = 1;") {
                    plain_seen = true;
                }
            }
        }
        harness.quit();
        task.await.expect("loop task").expect("loop exits cleanly");
        assert!(plain_seen, "a plain frame should have been drawn first");
        assert!(highlighted_seen, "the highlighted frame never arrived");
    }

    #[tokio::test(start_paused = true)]
    async fn loop_hands_the_draft_to_the_external_editor_and_installs_the_result() {
        let mut harness = Harness::new(64);
        harness.editor_script.lock().unwrap().0 = Some(Ok(Some("edited\nprompt\n".to_owned())));
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.settle().await;
        for character in "draft".chars() {
            harness.key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        harness.key(KeyCode::Char('e'), KeyModifiers::ALT);
        harness.settle().await;
        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");

        let script = harness.editor_script.lock().unwrap();
        assert_eq!(script.1, ["draft"], "the editor received the draft");
        assert_eq!(app.composer.text, "edited\nprompt");
        // The screen is repainted in full after the editor owned the TTY.
        let frames = harness.frames.frames();
        let last = frame_text(frames.last().unwrap());
        assert!(last.contains("\x1b[2J"), "full clear after external edit");
    }

    #[tokio::test(start_paused = true)]
    async fn loop_reports_a_missing_editor_and_keeps_the_draft() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.settle().await;
        harness.key(KeyCode::Char('x'), KeyModifiers::NONE);
        harness.key(KeyCode::Char('e'), KeyModifiers::ALT);
        harness.settle().await;
        harness.quit();
        let app = task.await.expect("loop task").expect("loop exits cleanly");
        assert_eq!(app.composer.text, "x");
        let (status, _) = app.visible_status().expect("a warning is shown");
        assert!(status.contains("$EDITOR"), "{status}");
    }

    #[tokio::test(start_paused = true)]
    async fn loop_rings_the_terminal_for_an_unfocused_run_finish_only() {
        let mut harness = Harness::new(64);
        let task = harness.spawn(App::new(TuiOptions::default()));
        harness.update(ClientUpdate::Snapshot(snapshot(1, Vec::new())));
        harness.settle().await;
        let finished = |sequence| {
            ClientUpdate::Event(envelope(
                sequence,
                SessionEvent::RunFinished {
                    session: summary(),
                    run_id: RunId::from_bytes([9; 16]),
                    outcome: qq_protocol::RunOutcome::Completed,
                    usage: None,
                    context_tokens: None,
                },
            ))
        };
        // Focused: no bell.
        harness.update(finished(2));
        harness.settle().await;
        let before = harness.frames.frames().len();
        assert!(
            harness
                .frames
                .frames()
                .iter()
                .all(|frame| !frame.contains(&0x07)),
            "no bell while focused"
        );

        harness
            .events
            .send(Ok(Event::FocusLost))
            .expect("event channel open");
        harness.update(finished(3));
        harness.settle().await;
        let frames = harness.frames.frames();
        let bell = frames[before..]
            .iter()
            .find(|frame| frame.starts_with(b"\x07\x1b]9;"))
            .expect("a bell and OSC 9 notification were written");
        assert_eq!(frame_text(bell), "\x07\x1b]9;qq: Session finished\x07");

        harness.quit();
        task.await.expect("loop task").expect("loop exits cleanly");
    }
}
