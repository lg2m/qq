//! Deterministic fixtures and a driver for the render benchmark.
//!
//! Enabled by the `bench-support` feature so `benches/render.rs` can drive the
//! crate-private `App` and `FrameRenderer` without widening the public API.
//! Nothing here is stable and nothing here should be used outside benchmarks.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{
    EventCursor, MessageId, MessageRole, MessageSnapshot, MessageState, RunActivity, RunId,
    SessionEvent, SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
    StoreId, TextChannel, WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};

use crate::{ClientUpdate, TuiOptions, app::App, view::FrameRenderer};

/// One TUI instance driven directly, without a terminal or client transport.
pub struct BenchHarness {
    app: App,
    renderer: FrameRenderer,
    size: (u16, u16),
    next_sequence: u64,
}

/// Prose long enough to wrap and exercise markdown, but short enough to keep
/// every message under the full-markdown cache threshold.
const PARAGRAPH: &str = "The renderer must keep steady-state frames cheap: completed \
messages are cached per width, so a frame with no new content should touch only the \
chrome and the row diff. `inline code`, **emphasis**, and a list:\n\n- first item\n- second \
item\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";

impl BenchHarness {
    /// A workspace with `sessions` root sessions. The first is focused and has
    /// `messages` completed assistant messages loaded; the rest are summaries.
    #[must_use]
    pub fn new(size: (u16, u16), sessions: u8, messages: u8) -> Self {
        assert!(sessions >= 1, "at least one session is required");
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let summaries: Vec<SessionSummary> = (0..sessions)
            .map(|index| summary(workspace_id, session_id(index), SessionStatus::Idle))
            .collect();
        let focused = SessionSnapshot {
            summary: summaries[0].clone(),
            messages: (0..messages)
                .map(|index| {
                    let mut message = assistant_message(session_id(0), index, PARAGRAPH);
                    message.state = MessageState::Complete;
                    message
                })
                .collect(),
            runs: Vec::new(),
            tool_calls: Vec::new(),
            has_older_tool_calls: false,
            has_older_messages: false,
        };
        let mut app = App::new(TuiOptions::default());
        app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
            included: Vec::new(),
            cursor: EventCursor {
                store_id: StoreId::from_bytes([3; 16]),
                workspace_id,
                sequence: 1,
            },
            workspace: WorkspaceSummary {
                id: workspace_id,
                path: "/workspace".to_owned(),
            },
            sessions: summaries,
            focused: Some(focused),
            has_older_sessions: false,
        }));
        app.apply_client_update(ClientUpdate::Connection(crate::ConnectionState::Live));
        Self {
            app,
            renderer: FrameRenderer::default(),
            size,
            next_sequence: 2,
        }
    }

    /// Start a streaming assistant message in session `index` and return its
    /// id. Session 0 is the focused session.
    pub fn start_stream(&mut self, index: u8) -> MessageId {
        let message = assistant_message(session_id(index), 200 + index, "");
        let id = message.id;
        let run_id = message.run_id;
        self.apply(
            index,
            SessionEvent::RunStarted {
                session: summary(
                    WorkspaceId::from_bytes([1; 16]),
                    session_id(index),
                    SessionStatus::Running,
                ),
                run_id,
            },
        );
        self.apply(
            index,
            SessionEvent::RunActivityChanged {
                run_id,
                activity: RunActivity::GeneratingResponse,
            },
        );
        self.apply(index, SessionEvent::AssistantMessageStarted { message });
        id
    }

    /// Append `text` to a streaming message in session `index`.
    pub fn append(&mut self, index: u8, message_id: MessageId, text: &str) -> bool {
        self.apply(
            index,
            SessionEvent::TextAppended {
                message_id,
                channel: TextChannel::Output,
                text: text.to_owned(),
            },
        )
    }

    /// Type one character into the composer.
    pub fn keystroke(&mut self, character: char) -> bool {
        let (changed, _) = self.app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
        changed
    }

    /// Build and diff one frame, returning the terminal bytes it would emit.
    pub fn draw(&mut self) -> Vec<u8> {
        self.renderer
            .draw(&mut self.app, self.size)
            .expect("in-memory frame rendering cannot fail")
    }

    /// Install any finished off-tick highlight results, as the event loop
    /// would between frames. Returns how many were applied.
    pub fn apply_finished_highlights(&mut self) -> usize {
        let mut applied = 0;
        while let Some(result) = self.renderer.highlighter.try_next() {
            if self.renderer.apply_highlight(result) {
                applied += 1;
            }
        }
        applied
    }

    /// Force the session sidebar on regardless of width.
    pub fn show_sidebar(&mut self) {
        self.app.sidebar = crate::app::Sidebar::Shown;
    }

    /// Force the session sidebar off regardless of width.
    pub fn hide_sidebar(&mut self) {
        self.app.sidebar = crate::app::Sidebar::Hidden;
    }

    /// Load `messages` completed assistant messages into session `index`
    /// through an included body, as the client's pre-warm does.
    pub fn warm_session(&mut self, index: u8, messages: u8) {
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let body = SessionSnapshot {
            summary: summary(workspace_id, session_id(index), SessionStatus::Idle),
            messages: (0..messages)
                .map(|row| {
                    let mut message = assistant_message(session_id(index), row, PARAGRAPH);
                    message.state = MessageState::Complete;
                    message
                })
                .collect(),
            runs: Vec::new(),
            tool_calls: Vec::new(),
            has_older_tool_calls: false,
            has_older_messages: false,
        };
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.app
            .apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([3; 16]),
                    workspace_id,
                    sequence,
                },
                workspace: WorkspaceSummary {
                    id: workspace_id,
                    path: "/workspace".to_owned(),
                },
                sessions: Vec::new(),
                focused: None,
                included: vec![body],
                has_older_sessions: false,
            }));
    }

    /// Split the focused pane side by side and show session `index` in the
    /// new pane. The session must already be warm.
    pub fn split_beside_showing(&mut self, index: u8) {
        self.app.execute(crate::commands::Command::SplitBeside);
        let (_, requests) = self.app.focus_session(session_id(index));
        assert!(requests.is_empty(), "bench sessions must be warm");
    }

    /// Draw until every scheduled highlight has landed, as the event loop
    /// does between frames. Steady-state samples should reflect the
    /// highlighted cache, not a stream of upgrade frames.
    pub fn settle_highlights(&mut self) {
        loop {
            let applied = self.apply_finished_highlights();
            if applied > 0 {
                self.draw();
            }
            if !self.highlights_pending() && applied == 0 {
                self.draw();
                if !self.highlights_pending() {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    /// Whether highlight jobs are still running.
    pub fn highlights_pending(&self) -> bool {
        self.renderer.highlighter.in_flight() > 0
    }

    fn apply(&mut self, index: u8, event: SessionEvent) -> bool {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.app
            .apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: StoreId::from_bytes([3; 16]),
                    workspace_id: WorkspaceId::from_bytes([1; 16]),
                    sequence,
                },
                session_id: session_id(index),
                run_id: Some(run_id(index)),
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            }))
    }
}

fn session_id(index: u8) -> SessionId {
    let mut bytes = [0x10; 16];
    bytes[15] = index;
    SessionId::from_bytes(bytes)
}

fn run_id(index: u8) -> RunId {
    let mut bytes = [0x20; 16];
    bytes[15] = index;
    RunId::from_bytes(bytes)
}

fn summary(workspace_id: WorkspaceId, id: SessionId, status: SessionStatus) -> SessionSummary {
    SessionSummary {
        activity: None,
        spawned_by: None,
        id,
        workspace_id,
        parent_id: None,
        title: format!("Session {}", id.as_bytes()[15]),
        status,
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

fn assistant_message(session_id: SessionId, index: u8, output: &str) -> MessageSnapshot {
    let mut bytes = [0x30; 16];
    bytes[14] = session_id.as_bytes()[15];
    bytes[15] = index;
    MessageSnapshot {
        id: MessageId::from_bytes(bytes),
        session_id,
        run_id: run_id(session_id.as_bytes()[15]),
        turn_ordinal: 1,
        role: MessageRole::Assistant,
        state: MessageState::Streaming,
        output: output.to_owned(),
        refusal: String::new(),
        created_at_ms: u64::from(index),
    }
}
