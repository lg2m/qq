use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use qq_protocol::{
    AccountingTotal, ModelSelection, RunId, SessionAccounting, SessionEvent, SessionEventEnvelope,
    SessionId, SessionSnapshot, SessionStatus, SessionSummary, WorkspaceSnapshot,
};

use super::*;
use crate::{
    ClientUpdate, ModelOption, TuiOptions,
    commands::Command,
    fixtures::{self, SESSION},
    render::{code_keyword, success, surface, surface_color},
    theme::Palette,
    view::markdown::{code_panel_row, tests::style_of},
};

fn completed_message(byte: u8, output: String) -> MessageSnapshot {
    MessageSnapshot {
        turn_ordinal: 0,
        ..fixtures::message(MessageId::from_bytes([byte; 16]), SESSION, &output)
    }
}

fn app_with_messages(count: u8) -> App {
    let summary = fixtures::session_summary(SESSION);
    let mut app = App::new(TuiOptions::default());
    app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
        sessions: vec![summary.clone()],
        focused: Some(SessionSnapshot {
            messages: (0..count)
                .map(|row| completed_message(row + 1, format!("row {row}")))
                .collect(),
            ..fixtures::session_snapshot(summary)
        }),
        ..fixtures::workspace_snapshot()
    }));
    app
}

fn frame_text(frame: &[Line]) -> String {
    frame
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn frame_rows(frame: &[Line]) -> Vec<String> {
    frame
        .iter()
        .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line> {
    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(app, width);
    body.viewport(app, body.rows, 0)
}

fn fold_focus_lines(app: &App, width: usize) -> Vec<Line> {
    let mut renderer = FrameRenderer::default();
    let body = renderer.fold_focus(app, width);
    body.viewport(app, body.rows, 0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn completed_messages_render_plain_then_upgrade_to_highlighted() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(1, "```rust\nlet x = 1;\n```".to_owned());
    message.state = MessageState::Streaming;

    let streaming = renderer.render_message(&message, 40);

    // Re-rendered every frame while streaming: plain panel, no cache.
    assert_eq!(style_of(&streaming, "let"), Some(surface(normal())));
    assert!(renderer.markdown().is_empty());
    assert_eq!(renderer.highlighter.in_flight(), 0);

    // Completion caches a plain layout immediately and schedules
    // highlighting off the render path.
    message.state = MessageState::Complete;
    let complete = renderer.render_message(&message, 40);
    assert_eq!(style_of(&complete, "let"), Some(surface(normal())));
    assert!(renderer.markdown().contains_key(&message.id));
    assert_eq!(renderer.highlighter.in_flight(), 1);

    let highlighted = renderer.highlighter.next().await;
    assert!(renderer.apply_highlight(highlighted));
    let upgraded = renderer.render_message(&message, 40);
    assert_eq!(style_of(&upgraded, "let"), Some(surface(code_keyword())));

    // A stale result (different width) is dropped, not installed.
    let stale = Highlighted {
        key: HighlightKey {
            message_id: message.id,
            width: 41,
            output_bytes: message.output.len(),
            refusal_bytes: 0,
            loaded_through: 0,
        },
        lines: Vec::new(),
    };
    assert!(!renderer.apply_highlight(stale));
    assert_eq!(
        style_of(&renderer.render_message(&message, 40), "let"),
        Some(surface(code_keyword()))
    );
}

#[test]
fn prose_only_messages_do_not_request_highlighting() {
    let mut renderer = FrameRenderer::default();
    let message = completed_message(1, "plain **prose** without code".to_owned());
    renderer.render_message(&message, 40);
    assert_eq!(renderer.highlighter.in_flight(), 0);
}

#[test]
fn live_message_rendering_is_bounded_without_hiding_completed_output() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(
        1,
        format!(
            "BEGIN-LIVE-MESSAGE\n{}\nEND-LIVE-MESSAGE",
            "streaming row\n".repeat(MAX_LIVE_MARKDOWN_ROWS * 4)
        ),
    );
    message.state = MessageState::Streaming;

    let live = renderer.render_message(&message, 40);
    let live_text = frame_text(&live);

    assert!(live.len() <= MAX_LIVE_MARKDOWN_ROWS + 1);
    assert!(live_text.contains("earlier output remains"));
    assert!(!live_text.contains("BEGIN-LIVE-MESSAGE"));
    assert!(live_text.contains("END-LIVE-MESSAGE"));

    message.state = MessageState::Complete;
    let complete = renderer.render_message(&message, 40);
    let complete_text = frame_text(&complete);
    assert!(complete_text.contains("BEGIN-LIVE-MESSAGE"));
    assert!(complete_text.contains("END-LIVE-MESSAGE"));
}

fn tool_call_snapshot(
    byte: u8,
    name: &str,
    arguments: &str,
    state: ToolCallState,
    result: Option<&str>,
    is_error: bool,
) -> ToolCallSnapshot {
    ToolCallSnapshot {
        arguments: arguments.to_owned(),
        state,
        result: result.map(str::to_owned),
        is_error,
        ..fixtures::tool_call(ToolCallId::from_bytes([byte; 16]), SESSION, name)
    }
}

#[test]
fn transcript_renders_replayed_tool_activity_collapsed() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some("contents"),
        false,
    )]);

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 30);
    let rows = frame_rows(&frame);

    assert!(
        rows.iter()
            .any(|row| row.contains("● read_file note.txt (1 line)"))
    );
    assert!(!frame_text(&frame).contains("contents"));
}

#[test]
fn call_only_run_renders_before_its_first_assistant_message() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].role = MessageRole::User;
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Running,
        None,
        false,
    )]);

    let rows = frame_rows(&transcript_lines(&app, 100));

    assert!(
        rows.iter()
            .any(|row| row.contains("read_file note.txt") && row.contains("running"))
    );
}

#[test]
fn fold_focus_renders_the_current_runs_tool_activity() {
    let mut app = app_with_messages(1);
    app.layout = Layout::FoldFocus;
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.summary.status = SessionStatus::Running;
    session.summary.active_run_id = Some(RunId::from_bytes([2; 16]));
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Running,
        None,
        false,
    )]);

    let rows = frame_rows(&fold_focus_lines(&app, 100));

    assert!(
        rows.iter()
            .any(|row| row.contains("read_file note.txt") && row.contains("running"))
    );
}

#[test]
fn steering_rows_say_what_they_are_at_every_state() {
    let mut app = app_with_messages(4);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.status = SessionStatus::Running;
    let active_run_id = RunId::from_bytes([2; 16]);
    session.summary.active_run_id = Some(active_run_id);
    let messages = session.messages.as_mut().unwrap();
    for (index, (state, text)) in [
        (MessageState::Complete, "the model turn"),
        (MessageState::Queued, "pending steer"),
        (MessageState::Complete, "applied steer"),
        (MessageState::Cancelled, "late steer"),
    ]
    .into_iter()
    .enumerate()
    {
        messages[index].run_id = active_run_id;
        messages[index].state = state;
        messages[index].output = text.to_owned();
        if index > 0 {
            messages[index].role = MessageRole::User;
            messages[index].steering = true;
        }
    }

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 40);
    let rows = frame_rows(&frame);
    let header_before = |needle: &str| {
        let at = rows.iter().position(|row| row.contains(needle)).unwrap();
        rows[..at]
            .iter()
            .rev()
            .find(|row| row.contains("YOU"))
            .cloned()
            .unwrap()
    };
    assert!(header_before("pending steer").contains("steering  waiting for the next turn"));
    assert!(
        header_before("applied steer")
            .trim_end()
            .ends_with("steered")
    );
    assert!(header_before("late steer").contains("steering  run finished first"));
    // A steering row never shows the plain "queued" of a queued prompt,
    // which would read as a new run waiting its turn.
    assert!(!rows.iter().any(|row| row.contains("YOU  queued")));
}

#[test]
fn fold_focus_keeps_active_work_visible_ahead_of_queued_prompts() {
    let mut app = app_with_messages(4);
    app.layout = Layout::FoldFocus;
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.status = SessionStatus::Running;
    let active_run_id = RunId::from_bytes([2; 16]);
    session.summary.active_run_id = Some(active_run_id);
    let messages = session.messages.as_mut().unwrap();
    messages[0].run_id = RunId::from_bytes([9; 16]);
    messages[0].output = "folded history".to_owned();
    messages[1].run_id = active_run_id;
    messages[1].output = "active model turn".to_owned();
    messages[2].run_id = RunId::from_bytes([3; 16]);
    messages[2].role = MessageRole::User;
    messages[2].state = MessageState::Queued;
    messages[2].output = "queued prompt one".to_owned();
    messages[3].run_id = RunId::from_bytes([4; 16]);
    messages[3].role = MessageRole::User;
    messages[3].state = MessageState::Queued;
    messages[3].output = "queued prompt two".to_owned();
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"active.rs"}"#,
        ToolCallState::Running,
        None,
        false,
    )]);

    let rows = frame_rows(&fold_focus_lines(&app, 100));
    let text = rows.join("\n");

    assert!(text.contains("active model turn"));
    assert!(
        rows.iter()
            .any(|row| row.contains("read_file active.rs") && row.contains("running"))
    );
    assert!(text.contains("queued prompt one"));
    assert!(text.contains("queued prompt two"));
    assert!(!text.contains("folded history"));
}

#[test]
fn fold_focus_keeps_tool_calls_between_their_model_turns() {
    let mut app = app_with_messages(3);
    app.layout = Layout::FoldFocus;
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].role = MessageRole::User;
    messages[1].turn_ordinal = 1;
    messages[2].turn_ordinal = 2;
    let mut first = tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"first.rs"}"#,
        ToolCallState::Completed,
        Some("first\n"),
        false,
    );
    first.turn_ordinal = 1;
    let mut second = tool_call_snapshot(
        8,
        "read_file",
        r#"{"path":"second.rs"}"#,
        ToolCallState::Completed,
        Some("second\n"),
        false,
    );
    second.turn_ordinal = 2;
    session.tool_calls = Some(vec![second, first]);

    let rows = frame_rows(&fold_focus_lines(&app, 100));
    let position = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();

    assert!(position("row 1") < position("first.rs"));
    assert!(position("first.rs") < position("row 2"));
    assert!(position("row 2") < position("second.rs"));
}

#[test]
fn transcript_spacing_separates_blocks_and_doubles_before_prompts() {
    let mut app = app_with_messages(3);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].role = MessageRole::User;
    messages[2].role = MessageRole::User;
    session.tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some("contents"),
        false,
    )]);

    let rows = frame_rows(&transcript_lines(&app, 80));

    assert_eq!(
        rows,
        [
            " ▌ YOU",
            " ▌ row 0",
            "",
            "   QQ",
            "   row 1",
            "",
            "   ● read_file note.txt (1 line)",
            "",
            "",
            " ▌ YOU",
            " ▌ row 2",
        ]
    );
}

#[test]
fn head_orphan_call_turns_render_before_the_runs_first_message() {
    let mut app = app_with_messages(2);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].role = MessageRole::User;
    messages[1].turn_ordinal = 2;
    let call = |byte, turn, name: &str, arguments: &str, result: &str| {
        let mut call = tool_call_snapshot(
            byte,
            name,
            arguments,
            ToolCallState::Completed,
            Some(result),
            false,
        );
        call.turn_ordinal = turn;
        call
    };
    // Arrival order is scrambled; rendering re-sorts by (turn, call).
    session.tool_calls = Some(vec![
        call(5, 2, "search", r#"{"query":"x"}"#, "No matches found.\n"),
        call(4, 1, "read_file", r#"{"path":"b.rs"}"#, "b\n"),
        call(3, 1, "read_file", r#"{"path":"a.rs"}"#, "a\n"),
    ]);

    let rows = frame_rows(&transcript_lines(&app, 80));

    // The call-only turn 1 renders before the run's first message (turn
    // 2), so the transcript reads in execution order.
    assert_eq!(
        rows,
        [
            " ▌ YOU",
            " ▌ row 0",
            "",
            "   ● read_file a.rs (1 line)",
            "   ● read_file b.rs (1 line)",
            "",
            "   QQ",
            "   row 1",
            "",
            "   ● search \"x\" (no matches)",
        ]
    );
}

#[test]
fn consecutive_call_only_turns_merge_into_one_folded_group() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].turn_ordinal = 1;
    let calls = [(2, 1), (3, 1), (4, 2), (5, 3)]
        .into_iter()
        .map(|(byte, turn)| {
            let mut call = tool_call_snapshot(
                byte,
                "read_file",
                r#"{"path":"a.rs"}"#,
                ToolCallState::Completed,
                Some("a\n"),
                false,
            );
            call.turn_ordinal = turn;
            call
        })
        .collect::<Vec<_>>();
    session.tool_calls = Some(calls);

    let rows = frame_rows(&transcript_lines(&app, 80));

    // The call-only turns 2 and 3 merge into turn 1's contiguous call
    // group, and the four quiet calls fold as one, not per turn.
    assert_eq!(
        rows,
        ["   QQ", "   row 0", "", "   ▸ 4 tool calls (read_file ×4)",]
    );
}

#[test]
fn completed_edit_results_color_diff_shaped_content_at_expanded_detail() {
    let diff_call = tool_call_snapshot(
        1,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("@@ -1 +1 @@\n-old\n+new\n context"),
        false,
    );
    let lines = render_tool_calls(
        &[&diff_call],
        &HashMap::new(),
        ToolDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    let style_of = |lines: &[Line], needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of(&lines, "@@ -1 +1 @@"), Some(accent().dim()));
    assert_eq!(style_of(&lines, "-old"), Some(failure()));
    assert_eq!(style_of(&lines, "+new"), Some(success()));
    assert_eq!(style_of(&lines, " context"), Some(normal()));

    // Today's summary results are not diff-shaped and keep the raw style.
    let summary_call = tool_call_snapshot(
        2,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
        false,
    );
    let lines = render_tool_calls(
        &[&summary_call],
        &HashMap::new(),
        ToolDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    assert_eq!(style_of(&lines, "Edited src/lib.rs"), Some(normal().dim()));
}

#[test]
fn display_payload_diffs_replace_the_result_summary_at_expanded_detail() {
    let mut call = tool_call_snapshot(
        3,
        "edit_file",
        r#"{"path":"src/lib.rs"}"#,
        ToolCallState::Completed,
        Some("Edited src/lib.rs: replaced 1 occurrence(s)."),
        false,
    );
    call.display = Some(ToolCallDisplay::Diff {
        path: "src/lib.rs".to_owned(),
        diff: "- old line\n+ new line\n".to_owned(),
    });

    let lines = render_tool_calls(
        &[&call],
        &HashMap::new(),
        ToolDetail::Expanded,
        0,
        80,
        &|_, _| Vec::new(),
    );
    let style_of = |needle: &str| {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("- old line"), Some(failure()));
    assert_eq!(style_of("+ new line"), Some(success()));
    // The payload renders instead of the raw summary sentence.
    assert!(style_of("replaced 1 occurrence").is_none());

    // Collapsed detail keeps the one-liner; the payload adds no rows.
    let lines = render_tool_calls(
        &[&call],
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        80,
        &|_, _| Vec::new(),
    );
    assert_eq!(lines.len(), 1);
}

#[test]
fn running_calls_show_a_live_output_tail_of_complete_lines() {
    let call = tool_call_snapshot(
        3,
        "shell",
        r#"{"command":"cargo build"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    let mut live = HashMap::new();
    live.insert(
        call.id,
        "one\ntwo\nthree\nfour\nfive\nsix\nseven b\u{7}ell\npartial".to_owned(),
    );

    for detail in [ToolDetail::Collapsed, ToolDetail::Expanded] {
        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &live,
            detail,
            0,
            80,
            &|_, _| Vec::new(),
        ));
        assert!(rows[0].contains("shell"), "the spinner one-liner stays");
        let tail_start = rows.len() - MAX_LIVE_TAIL_ROWS;
        assert_eq!(
            &rows[tail_start..],
            [
                "     two",
                "     three",
                "     four",
                "     five",
                "     six",
                // Control characters are stripped; the mid-line chunk
                // tail stays hidden until its newline arrives.
                "     seven bell",
            ]
        );
        assert!(!rows.iter().any(|row| row.contains("partial")));
    }

    // Overlong lines wrap literally at the character level and the tail
    // stays bounded in rows.
    let mut live = HashMap::new();
    live.insert(call.id, format!("{}\n", "x".repeat(40)));
    let rows = frame_rows(&render_tool_calls(
        &[&call],
        &live,
        ToolDetail::Collapsed,
        0,
        20,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows[1], format!("     {}", "x".repeat(15)));
    assert_eq!(rows[2], format!("     {}", "x".repeat(15)));
    assert!(rows.len() <= 1 + MAX_LIVE_TAIL_ROWS);

    // Calls that are no longer running render no tail even if a stale
    // buffer lingers.
    let mut finished = call.clone();
    finished.state = ToolCallState::Completed;
    finished.result = Some("ok\n".to_owned());
    let rows = frame_rows(&render_tool_calls(
        &[&finished],
        &live,
        ToolDetail::Collapsed,
        0,
        80,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows.len(), 1);
}

#[test]
fn diff_detection_requires_hunks_or_paired_change_lines() {
    assert!(looks_like_diff("@@ -1 +1 @@\n context"));
    assert!(looks_like_diff("-old\n+new"));
    assert!(!looks_like_diff(
        "Edited src/lib.rs: replaced 1 occurrence(s)."
    ));
    assert!(!looks_like_diff("+new line only"));
    assert!(!looks_like_diff(""));
}

#[test]
fn approval_prompts_render_edit_previews_as_colored_diffs() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let tool_call = tool_call_snapshot(
        9,
        "edit_file",
        r#"{"path":"src/lib.rs","content":"new"}"#,
        ToolCallState::AwaitingApproval,
        None,
        false,
    );
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(tool_call.run_id),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            session_id,
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell: None,
                edit: Some(qq_protocol::EditPreview {
                    path: "src/lib.rs".to_owned(),
                    diff: "@@ -1 +1 @@\n-old\n+new".to_owned(),
                }),
            },
        )
    }));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 24);
    let rows = frame_rows(&frame);

    assert!(rows.iter().any(|row| row.contains("file: src/lib.rs")));
    assert!(!frame_text(&frame).contains("arguments:"));
    let style_of = |needle: &str| {
        frame
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(needle))
            .map(|span| span.style)
    };
    assert_eq!(style_of("@@ -1 +1 @@"), Some(accent().dim()));
    assert_eq!(style_of("-old"), Some(failure()));
    assert_eq!(style_of("+new"), Some(success()));
    // The modal offers all four decisions, including workspace lifetime.
    assert!(rows.iter().any(|row| {
        row.contains("[y] approve once")
            && row.contains("[a] for session")
            && row.contains("[w] for workspace")
            && row.contains("[n]/[Esc] deny")
    }));
}

#[test]
fn collapsed_summaries_curate_known_tools() {
    let cases = [
        (
            tool_call_snapshot(
                1,
                "read_file",
                r#"{"path":"src/config/loader.rs"}"#,
                ToolCallState::Completed,
                Some("a\nb\nc\n"),
                false,
            ),
            "   ● read_file src/config/loader.rs (3 lines)",
        ),
        (
            tool_call_snapshot(
                2,
                "read_file",
                r#"{"path":"big.log"}"#,
                ToolCallState::Completed,
                Some("a\n...[truncated by qq]\n"),
                false,
            ),
            "   ● read_file big.log (1 line, truncated)",
        ),
        (
            tool_call_snapshot(
                3,
                "search",
                r#"{"query":"pattern"}"#,
                ToolCallState::Completed,
                Some("src/a.rs:1:x pattern\nsrc/a.rs:9:pattern y\nsrc/b.rs: filename match\n"),
                false,
            ),
            "   ● search \"pattern\" (3 matches, 2 files)",
        ),
        (
            tool_call_snapshot(
                4,
                "search",
                r#"{"query":"absent"}"#,
                ToolCallState::Completed,
                Some("No matches found.\n"),
                false,
            ),
            "   ● search \"absent\" (no matches)",
        ),
        (
            tool_call_snapshot(
                5,
                "list_dir",
                r#"{"path":"crates/qq-core/src"}"#,
                ToolCallState::Completed,
                Some("lib.rs\nsessions.rs\ntools.rs\n"),
                false,
            ),
            "   ● list_dir crates/qq-core/src (3 entries)",
        ),
    ];
    for (call, expected) in cases {
        let rows = frame_rows(&render_tool_calls(
            &[&call],
            &HashMap::new(),
            ToolDetail::Collapsed,
            0,
            120,
            &|_, _| Vec::new(),
        ));
        assert_eq!(rows, [expected]);
    }
}

#[test]
fn unknown_tools_fall_back_to_compact_arguments_and_byte_size() {
    let result = "x".repeat(2048);
    let call = tool_call_snapshot(
        1,
        "edit_file",
        r#"{"path":"src/main.rs","content":"fn main() {}"}"#,
        ToolCallState::Completed,
        Some(&result),
        false,
    );

    let rows = frame_rows(&render_tool_calls(
        &[&call],
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        160,
        &|_, _| Vec::new(),
    ));

    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("● edit_file"));
    assert!(rows[0].contains(r#"{"path":"src/main.rs","#));
    assert!(rows[0].contains("(2.0 KB)"));
}

#[test]
fn malformed_arguments_fall_back_to_a_raw_preview() {
    let call = tool_call_snapshot(
        1,
        "read_file",
        "{not json",
        ToolCallState::Completed,
        Some("a\n"),
        false,
    );

    let rows = frame_rows(&render_tool_calls(
        &[&call],
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        120,
        &|_, _| Vec::new(),
    ));

    assert!(rows[0].contains("read_file {not json (1 line)"));
}

#[test]
fn error_results_expand_under_the_summary_by_default() {
    let call = tool_call_snapshot(
        1,
        "read_file",
        r#"{"path":"gone.txt"}"#,
        ToolCallState::Completed,
        Some("path is not a file"),
        true,
    );

    let rows = frame_rows(&render_tool_calls(
        &[&call],
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        120,
        &|_, _| Vec::new(),
    ));

    assert_eq!(rows[0], "   ✗ read_file gone.txt");
    assert_eq!(rows[1], "     path is not a file");
}

#[test]
fn pending_states_show_their_glyph_and_label() {
    let awaiting = tool_call_snapshot(
        1,
        "shell",
        r#"{"command":"cargo test"}"#,
        ToolCallState::AwaitingApproval,
        None,
        false,
    );
    let rows = frame_rows(&render_tool_calls(
        &[&awaiting],
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(
        rows,
        ["   ◇ shell {\"command\":\"cargo test\"} awaiting approval"]
    );

    let running = tool_call_snapshot(
        2,
        "search",
        r#"{"query":"x"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    let rows = frame_rows(&render_tool_calls(
        &[&running],
        &HashMap::new(),
        ToolDetail::Collapsed,
        1,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows, ["   ◓ search \"x\" running"]);
}

#[test]
fn quiet_runs_fold_into_a_single_counted_line() {
    let mut calls = Vec::new();
    for byte in 1..=4 {
        calls.push(tool_call_snapshot(
            byte,
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolCallState::Completed,
            Some("a\n"),
            false,
        ));
    }
    for byte in 5..=6 {
        calls.push(tool_call_snapshot(
            byte,
            "search",
            r#"{"query":"x"}"#,
            ToolCallState::Completed,
            Some("No matches found.\n"),
            false,
        ));
    }
    let references = calls.iter().collect::<Vec<_>>();

    let rows = frame_rows(&render_tool_calls(
        &references,
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows, ["   ▸ 6 tool calls (read_file ×4, search ×2)"]);

    // An active or failed call keeps every line visible.
    calls[5].state = ToolCallState::Running;
    let references = calls.iter().collect::<Vec<_>>();
    let rows = frame_rows(&render_tool_calls(
        &references,
        &HashMap::new(),
        ToolDetail::Collapsed,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert_eq!(rows.len(), 6);

    // Expanded detail never folds.
    calls[5].state = ToolCallState::Completed;
    let references = calls.iter().collect::<Vec<_>>();
    let rows = frame_rows(&render_tool_calls(
        &references,
        &HashMap::new(),
        ToolDetail::Expanded,
        0,
        120,
        &|_, _| Vec::new(),
    ));
    assert!(rows.len() > 6);
}

#[test]
fn detail_cycling_reveals_arguments_and_result_tails() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions.get_mut(&session_id).unwrap().tool_calls = Some(vec![tool_call_snapshot(
        7,
        "read_file",
        r#"{"path":"note.txt"}"#,
        ToolCallState::Completed,
        Some("alpha\nbeta"),
        false,
    )]);
    let mut renderer = FrameRenderer::default();
    let ctrl_o = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));

    let collapsed = frame_rows(&renderer.frame_and_commit(&mut app, 100, 30));
    assert!(!collapsed.iter().any(|row| row.contains("beta")));
    assert!(collapsed.iter().any(|row| row.contains("tools: collapsed")));

    app.handle_terminal_event(ctrl_o.clone());
    let expanded = frame_rows(&renderer.frame_and_commit(&mut app, 100, 30));
    assert!(
        expanded
            .iter()
            .any(|row| row.contains("\"path\": \"note.txt\""))
    );
    assert!(expanded.iter().any(|row| row.contains("beta")));
    assert!(expanded.iter().any(|row| row.contains("tools: expanded")));

    app.handle_terminal_event(ctrl_o);
    let collapsed = frame_rows(&renderer.frame_and_commit(&mut app, 100, 30));
    assert!(!collapsed.iter().any(|row| row.contains("beta")));
}

#[test]
fn tool_rows_respect_narrow_widths() {
    let calls = [
        tool_call_snapshot(
            1,
            "read_file",
            r#"{"path":"a/very/long/path/that/never/ends.rs"}"#,
            ToolCallState::Completed,
            Some("line one that is fairly long\nline two\n"),
            false,
        ),
        tool_call_snapshot(
            2,
            "shell",
            r#"{"command":"cargo test --workspace --all-features"}"#,
            ToolCallState::Failed,
            Some("error: a very long failure message that overflows"),
            true,
        ),
    ];
    let references = calls.iter().collect::<Vec<_>>();
    for width in 0..24 {
        for detail in [ToolDetail::Collapsed, ToolDetail::Expanded] {
            let lines =
                render_tool_calls(&references, &HashMap::new(), detail, 0, width, &|_, _| {
                    Vec::new()
                });
            assert!(lines.iter().all(|line| line.width() <= width));
        }
    }
}

#[test]
fn user_prompts_carry_an_accent_bar() {
    let mut renderer = FrameRenderer::default();
    let mut message = completed_message(1, "deploy the API".to_owned());
    message.role = MessageRole::User;

    let rows = frame_rows(&renderer.render_message(&message, 80));

    assert!(rows[0].starts_with(" ▌ YOU"));
    assert!(rows[1].starts_with(" ▌ "));
    assert_eq!(
        renderer.render_message(&message, 80)[0].spans[0].style,
        accent()
    );
}

#[test]
fn final_output_sanitizes_every_dynamic_span() {
    let line = Line::styled("title\u{1b}]52;c;Y2xpcGJvYXJk\u{7}\u{202e}", normal());
    let mut rendered = Vec::new();

    write_line(&mut rendered, &line).unwrap();

    let rendered = String::from_utf8(rendered).unwrap();
    assert!(!rendered.contains("\u{1b}]52"));
    assert!(!rendered.contains('\u{7}'));
    assert!(!rendered.contains('\u{202e}'));
}

#[test]
fn panel_rows_carry_the_surface_background_through_output() {
    let row = code_panel_row(Line::styled("x", normal()), 8);
    assert!(
        row.spans
            .iter()
            .all(|span| span.style.background == Some(surface_color()))
    );
    let mut rendered = Vec::new();

    write_line(&mut rendered, &row).unwrap();

    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains('x'));
    if std::env::var_os("NO_COLOR").is_none() {
        assert!(rendered.contains("\u{1b}[48;2;38;40;48m"));
    } else {
        assert!(!rendered.contains("\u{1b}[48;2;38;40;48m"));
    }
}

#[test]
fn completed_markdown_cache_is_bounded_and_keeps_one_width() {
    let mut renderer = FrameRenderer::default();
    let message = completed_message(1, "hello".to_owned());
    renderer.render_message(&message, 40);
    renderer.render_message(&message, 80);
    assert_eq!(renderer.markdown().len(), 1);
    assert_eq!(renderer.markdown()[&message.id].width, 80);

    for byte in 2..=u8::try_from(MAX_VISIBLE_MESSAGES + 8).unwrap() {
        renderer.render_message(&completed_message(byte, byte.to_string()), 80);
    }
    assert!(renderer.markdown().len() <= MAX_VISIBLE_MESSAGES);
}

#[test]
fn authoritative_snapshot_generation_invalidates_same_length_cached_output() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0]
        .output = "old".to_owned();
    let mut renderer = FrameRenderer::default();
    let initial = renderer.transcript(&app, 80);
    assert!(frame_text(&initial.viewport(&app, initial.rows, 0)).contains("old"));
    drop(initial);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].output = "new".to_owned();
    session.loaded_through += 1;
    let refreshed = renderer.transcript(&app, 80);
    let text = frame_text(&refreshed.viewport(&app, refreshed.rows, 0));

    assert!(text.contains("new"));
    assert!(!text.contains("old"));
}

#[test]
fn completed_markdown_preserves_the_beginning_and_end_of_long_messages() {
    let mut renderer = FrameRenderer::default();
    let output = (1..=10)
        .map(|phase| {
            format!(
                "## Phase {phase}\n{}{}\n",
                if phase == 1 {
                    "BEGIN-FIRST-PHASE\n"
                } else {
                    ""
                },
                (0..80)
                    .map(|step| format!("- phase {phase} step {step}: verify the complete output"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
        .collect::<String>()
        + "\nEND-FINAL-PHASE";
    let message = completed_message(1, output);
    let rendered = renderer.render_message(&message, 80);

    let text = rendered
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(text.contains("BEGIN-FIRST-PHASE"));
    assert!(text.contains("phase 5 step 40"));
    assert!(text.contains("END-FINAL-PHASE"));
}

#[test]
fn oversized_completed_messages_use_a_sparse_full_history_index() {
    let mut app = app_with_messages(1);
    let message = &mut app
        .sessions
        .get_mut(&app.focused().unwrap())
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0];
    message.output = std::iter::once("BEGIN-SPARSE".to_owned())
        .chain((0..12_000).map(|row| format!("ROW-{row:05} 😀")))
        .chain(std::iter::once("END-SPARSE".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(message.output.len() > MAX_FULL_MARKDOWN_BYTES);

    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(&app, 80);
    let (index, message_id, prefix, prefix_style, width) = body
        .segments
        .iter()
        .find_map(|segment| match segment {
            BodySegment::Plain {
                index,
                message_id,
                prefix,
                prefix_style,
                width,
            } => Some((*index, *message_id, *prefix, *prefix_style, *width)),
            BodySegment::Owned(_) | BodySegment::Cached(_) => None,
        })
        .expect("oversized message uses sparse rendering");
    assert!(index.checkpoints.len() <= MAX_PLAIN_TEXT_CHECKPOINTS + 1);

    let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
    let tail = frame_text(&body.viewport(&app, 20, 0));
    assert!(top.contains("BEGIN-SPARSE"));
    assert!(tail.contains("END-SPARSE"));

    let source = MessageText::new(find_message(&app, message_id).unwrap());
    let middle = frame_text(&index.render(source, 6_000..6_006, prefix, prefix_style, width));
    assert!(middle.contains("ROW-05999"));
    assert!(middle.contains('😀'));
}

#[test]
fn combined_output_and_refusal_preserve_both_channels() {
    let mut app = app_with_messages(1);
    let message = &mut app
        .sessions
        .get_mut(&app.focused().unwrap())
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0];
    message.output = "OUTPUT-BEGIN".to_owned() + &"o".repeat(40 * 1024);
    message.refusal = "REFUSAL-BEGIN".to_owned() + &"r".repeat(40 * 1024) + "REFUSAL-END";

    let mut renderer = FrameRenderer::default();
    let body = renderer.transcript(&app, 80);
    let top = frame_text(&body.viewport(&app, 20, body.rows.saturating_sub(20)));
    let tail = frame_text(&body.viewport(&app, 20, 0));

    assert!(top.contains("OUTPUT-BEGIN"));
    assert!(tail.contains("REFUSAL-END"));
}

#[test]
fn completing_a_long_live_message_preserves_a_scrolled_tail_anchor() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let message = &mut session.messages.as_mut().unwrap()[0];
    message.state = MessageState::Streaming;
    message.output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    app.handle_terminal_event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ),
    ));
    let live_offset = app.transcript_scroll_offset();
    assert!(live_offset > 0);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
    session.loaded_through += 1;
    renderer.frame_and_commit(&mut app, 80, 24);

    assert_eq!(app.transcript_scroll_offset(), live_offset);
}

#[test]
fn completion_behind_an_overlay_preserves_the_scrolled_live_tail() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let message = &mut session.messages.as_mut().unwrap()[0];
    message.state = MessageState::Streaming;
    message.output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    app.handle_terminal_event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ),
    ));
    let live_offset = app.transcript_scroll_offset();
    app.overlay = Some(crate::input::Overlay::models());
    renderer.frame_and_commit(&mut app, 80, 24);

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[0].state = MessageState::Complete;
    session.loaded_through += 1;
    renderer.frame_and_commit(&mut app, 80, 24);
    app.overlay = None;
    renderer.frame_and_commit(&mut app, 80, 24);

    assert_eq!(app.transcript_scroll_offset(), live_offset);
}

#[test]
fn completing_a_live_message_does_not_move_an_older_history_viewport() {
    let mut app = app_with_messages(2);
    let session_id = app.focused().unwrap();
    let session = app.sessions.get_mut(&session_id).unwrap();
    let messages = session.messages.as_mut().unwrap();
    messages[0].output = (0..200)
        .map(|row| format!("HISTORY-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    messages[1].state = MessageState::Streaming;
    messages[1].output = (0..2_000)
        .map(|row| format!("LIVE-ROW-{row:04}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 24);
    let page_up = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::PageUp,
        crossterm::event::KeyModifiers::NONE,
    ));
    while app.handle_terminal_event(page_up.clone()).split().0 {}
    let before = renderer.frame_and_commit(&mut app, 80, 24);
    assert!(frame_text(&before).contains("HISTORY-ROW-0000"));
    let history_offset = app.transcript_scroll_offset();

    let session = app.sessions.get_mut(&session_id).unwrap();
    session.messages.as_mut().unwrap()[1].state = MessageState::Complete;
    session.loaded_through += 1;
    let after = renderer.frame_and_commit(&mut app, 80, 24);

    assert!(frame_text(&after).contains("HISTORY-ROW-0000"));
    assert!(app.transcript_scroll_offset() > history_offset);
}

#[test]
fn sparse_rows_have_a_byte_ceiling_for_zero_width_text() {
    let message = completed_message(
        1,
        format!(
            "a{}",
            "\u{0301}".repeat(MAX_FULL_MARKDOWN_BYTES / '\u{0301}'.len_utf8() + 1)
        ),
    );
    let source = MessageText::new(&message);
    let mut byte = 0;
    let mut rows = 0;
    while let Some((range, next)) = next_plain_text_row(source, byte, 80) {
        assert!(range.len() <= MAX_PLAIN_TEXT_ROW_BYTES);
        assert!(next > byte);
        rows += 1;
        byte = next;
    }
    assert!(rows > 1);

    let index = PlainTextIndex::new(source, 80);
    let rendered = index.render(source, 0..1, "   ", muted(), 83);
    let emitted_bytes = rendered[0]
        .spans
        .iter()
        .map(|span| span.text.len())
        .sum::<usize>();
    assert!(emitted_bytes <= MAX_PLAIN_TEXT_ROW_BYTES + 3);
}

#[test]
fn refreshed_chrome_shows_identity_status_and_session_metrics() {
    let mut app = app_with_messages(1);
    app.connection = crate::ConnectionState::Live;
    app.models.push(ModelOption {
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        name: Some("GPT Test".to_owned()),
        context_window: Some(128_000),
        selection: ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        },
    });
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.context_tokens = Some(64_000);
    session.context_window = Some(128_000);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let rows = frame_rows(&frame);

    assert!(rows[0].contains(&format!("qq  {VERSION} {GIT_COMMIT}")));
    assert!(rows[0].ends_with("local"));
    assert!(!rows[0].contains("Threadline"));
    assert!(rows[9].contains("> Ask QQ..."));
    assert!(rows[10].contains("context: 50.0% / 128000"));
    assert!(rows[10].ends_with("model: openai/gpt-test "));
    assert!(rows[11].contains("cwd: /workspace"));
    assert!(rows[11].ends_with("cost: $0.00 "));
    assert_eq!(frame[0].spans[0].style, brand().bold());
}

#[test]
fn footer_renders_unknown_context_and_cost_without_inventing_zero_usage() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);
    session.summary.accounting = Some(SessionAccounting {
        direct: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(100_000_000),
        },
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        },
    });
    session.summary.context_tokens = None;
    session.context_window = Some(272_000);

    let rows = frame_rows(&[footer_context(&app, 80), footer_workspace(&app, 80)]);

    assert!(rows[0].contains("context: -- / 272000"));
    assert!(rows[1].ends_with("cost: -- "));
}

#[test]
fn footer_uses_legacy_direct_cost_when_structured_accounting_is_absent() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.accounting = None;
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);

    let rows = frame_rows(&[footer_workspace(&app, 80)]);

    assert!(rows[0].ends_with("cost: $0.10 "));
}

#[test]
fn footer_displays_inclusive_accounting_cost() {
    let mut app = app_with_messages(0);
    let session = app.sessions.get_mut(&app.focused().unwrap()).unwrap();
    session.summary.estimated_cost_usd_nanos = Some(100_000_000);
    session.summary.accounting = Some(SessionAccounting {
        direct: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(100_000_000),
        },
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(250_000_000),
        },
    });

    let rows = frame_rows(&[footer_workspace(&app, 80)]);

    assert!(rows[0].ends_with("cost: $0.25 "));
}

#[test]
fn header_only_qualifies_local_when_the_connection_has_a_problem() {
    let mut app = app_with_messages(0);
    for (connection, expected) in [
        (crate::ConnectionState::Connecting, "local  connecting"),
        (crate::ConnectionState::Replaying, "local  reconnecting"),
        (crate::ConnectionState::Offline, "local  offline"),
    ] {
        app.connection = connection;
        assert!(frame_rows(&[header(&app, 80)])[0].ends_with(expected));
    }
}

#[test]
fn threadline_has_no_vertical_message_rails() {
    let mut app = app_with_messages(2);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 14);

    assert!(frame_rows(&frame).iter().all(|row| !row.contains("  |  ")));
}

#[test]
fn composer_renders_hard_newlines_across_multiple_rows() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "hello\nworld".to_owned();
    app.animation_tick = 0;
    let rows = frame_rows(&composer(&app, 40, 8));
    assert_eq!(rows, vec![" > hello".to_owned(), "   world|".to_owned()]);
}

#[test]
fn composer_keeps_the_tail_when_max_rows_clip() {
    let mut app = App::new(TuiOptions::default());
    app.composer.text = "one\ntwo\nthree\nfour".to_owned();
    app.animation_tick = 1; // steady caret space, simpler assertions
    let rows = frame_rows(&composer(&app, 40, 2));
    assert_eq!(rows, vec![" … three".to_owned(), "   four ".to_owned()]);
}

#[test]
fn slash_autocomplete_is_filtered_above_the_composer() {
    let mut app = app_with_messages(1);
    app.composer.text = "/".to_owned();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 20);
    let text = frame_text(&frame);
    for command in ["/models", "/sessions", "/resume", "/new", "/quit", "/exit"] {
        assert!(text.contains(command));
    }

    app.composer.text = "/qu".to_owned();
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 14);
    let text = frame_text(&frame);

    assert!(text.contains("/quit"));
    assert!(!text.contains("/models"));
    assert!(!text.contains("/sessions"));
}

#[test]
fn session_picker_pins_search_and_keeps_the_selection_visible() {
    let mut app = app_with_messages(0);
    let mut selected = None;
    for byte in 2..20 {
        let session_id = SessionId::from_bytes([byte; 16]);
        if byte == 10 {
            selected = Some(session_id);
        }
        let summary = SessionSummary {
            title: format!("Session {byte}"),
            updated_at_ms: u64::from(byte),
            ..fixtures::session_summary(session_id)
        };
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            occurred_at_ms: u64::from(byte),
            ..fixtures::envelope(
                u64::from(byte),
                session_id,
                SessionEvent::SessionCreated { session: summary },
            )
        }));
    }
    app.overlay = Some(crate::input::Overlay::sessions("", selected, None));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let text = frame_text(&frame);

    assert!(text.contains("SESSIONS"));
    assert!(text.contains("search: all sessions"));
    assert!(text.contains("Session 10"));
}

#[test]
fn session_picker_renders_an_empty_search_result() {
    let mut app = app_with_messages(0);
    app.overlay = Some(crate::input::Overlay::sessions("missing", None, None));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 80, 12);
    let text = frame_text(&frame);

    assert!(text.contains("search: missing"));
    assert!(text.contains("No matching sessions."));
}

#[test]
fn session_picker_renders_delete_and_prune_confirmations() {
    let mut app = app_with_messages(0);
    let session_id = SESSION;
    app.overlay = Some(crate::input::Overlay::sessions(
        "",
        Some(session_id),
        Some(SessionConfirm::Delete(session_id)),
    ));

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("y confirms, n or Esc cancels"));
    assert!(text.contains("delete 'Session'? y deletes, n keeps"));

    app.overlay
        .as_mut()
        .unwrap()
        .set_confirm(Some(SessionConfirm::Prune));
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("delete every empty session in this workspace?"));

    // Without a pending confirmation the hint advertises both actions.
    app.overlay.as_mut().unwrap().set_confirm(None);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Ctrl-D deletes, Ctrl-P prunes empty"));
}

#[test]
fn model_picker_hint_reflects_apply_versus_create() {
    let mut app = app_with_messages(0);
    app.models.push(crate::app::ModelOption {
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        name: Some("GPT Test".to_owned()),
        context_window: None,
        selection: ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: None,
            organization: None,
        },
    });
    app.overlay = Some(crate::input::Overlay::models());

    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Enter sets the session model, Ctrl-N creates a session"));

    app.panes.focused_mut().session = None;
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 100, 12);
    let text = frame_text(&frame);
    assert!(text.contains("Enter creates session"));
}

#[test]
fn transcript_viewport_renders_rows_above_the_tail_and_clamps_at_the_top() {
    let lines = (0..8)
        .map(|row| Line::styled(row.to_string(), normal()))
        .collect::<Vec<_>>();

    let scrolled = transcript_viewport(lines.clone(), 3, 2);
    let top = transcript_viewport(lines, 3, usize::MAX);
    let text = |rows: &[Line]| {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(text(&scrolled), ["3", "4", "5"]);
    assert_eq!(text(&top), ["0", "1", "2"]);
}

#[test]
fn page_up_replaces_the_rendered_live_tail_with_older_transcript_rows() {
    let mut app = app_with_messages(10);
    let mut renderer = FrameRenderer::default();
    let tail = renderer.frame_and_commit(&mut app, 80, 12);

    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));
    let scrolled = renderer.frame_and_commit(&mut app, 80, 12);

    assert!(frame_text(&tail).contains("row 9"));
    assert!(!frame_text(&scrolled).contains("row 9"));
    assert!(frame_text(&scrolled).contains("row 6"));
}

#[test]
fn page_up_reaches_the_beginning_of_a_long_completed_message() {
    let mut app = app_with_messages(1);
    let session_id = app.focused().unwrap();
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .messages
        .as_mut()
        .unwrap()[0]
        .output = format!(
        "BEGIN-LONG-MESSAGE\n{}\nEND-LONG-MESSAGE",
        (0..400)
            .map(|row| format!("long response row {row}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut renderer = FrameRenderer::default();
    let tail = renderer.frame_and_commit(&mut app, 80, 12);

    for _ in 0..100 {
        app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
    }
    let top = renderer.frame_and_commit(&mut app, 80, 12);

    assert!(frame_text(&tail).contains("END-LONG-MESSAGE"));
    assert!(!frame_text(&tail).contains("BEGIN-LONG-MESSAGE"));
    assert!(frame_text(&top).contains("BEGIN-LONG-MESSAGE"));
}

#[test]
fn sidebar_appears_at_wide_widths_and_shows_live_status_for_cold_sessions() {
    let mut app = app_with_messages(1);
    app.connection = crate::ConnectionState::Live;
    let parent = app.focused().unwrap();
    let child_id = SessionId::from_bytes([7; 16]);
    let run_id = RunId::from_bytes([8; 16]);
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(run_id),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            child_id,
            SessionEvent::SessionCreated {
                session: SessionSummary {
                    parent_id: Some(parent),
                    title: "Survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(run_id),
                    activity: Some(qq_protocol::RunActivity::GeneratingResponse),
                    model: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    ..fixtures::session_summary(child_id)
                },
            },
        )
    }));
    // The child is cold (no body) but streams text; the sidebar must
    // still show its tail.
    let message = MessageSnapshot {
        run_id,
        state: MessageState::Streaming,
        created_at_ms: 3,
        ..fixtures::message(MessageId::from_bytes([9; 16]), child_id, "")
    };
    for (sequence, event) in [
        (3, SessionEvent::AssistantMessageStarted { message }),
        (
            4,
            SessionEvent::TextAppended {
                message_id: MessageId::from_bytes([9; 16]),
                channel: qq_protocol::TextChannel::Output,
                text: "Found twelve call sites".to_owned(),
            },
        ),
    ] {
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, child_id, event)
        }));
    }
    assert!(!app.sessions[&child_id].is_warm());

    let rows_at = |app: &mut App, width| {
        frame_rows(&FrameRenderer::default().frame_and_commit(app, width, 24)).join("\n")
    };
    let narrow = rows_at(&mut app, 100);
    assert!(
        !narrow.contains("SESSIONS  1 running"),
        "auto-hidden when narrow"
    );

    let wide_frame = FrameRenderer::default().frame_and_commit(&mut app, 160, 24);
    let wide = frame_rows(&wide_frame).join("\n");
    assert!(wide.contains("SESSIONS  1 running"), "{wide}");
    assert!(wide.contains("Survey callers"));
    assert!(wide.contains("Found twelve call sites"));
    // With the sidebar glued on, every body row is exactly the terminal
    // width: the border column lines up and nothing overflows.
    for row in &wide_frame[2..wide_frame.len() - 4] {
        assert_eq!(
            row.width(),
            160,
            "{:?}",
            frame_rows(std::slice::from_ref(row))
        );
    }

    // Ctrl-B hides it even when wide; a second press shows it again.
    let toggle = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    app.handle_terminal_event(toggle.clone());
    assert!(!rows_at(&mut app, 160).contains("SESSIONS  1 running"));
    app.handle_terminal_event(toggle);
    assert!(
        rows_at(&mut app, 100).contains("SESSIONS  1 running"),
        "explicitly shown wins over width"
    );
}

#[test]
fn spawned_children_render_under_their_spawn_call_and_never_fold() {
    let mut app = app_with_messages(1);
    let parent = app.focused().unwrap();
    let run_id = RunId::from_bytes([2; 16]);
    let spawn_call = tool_call_snapshot(
        0x21,
        "spawn_agent",
        r#"{"task":"survey callers"}"#,
        ToolCallState::Running,
        None,
        false,
    );
    // Four quiet reads plus the spawn call: without the child this run
    // would fold into one counted line at collapsed detail.
    let mut calls = vec![spawn_call.clone()];
    for byte in 0x22..0x26 {
        calls.push(tool_call_snapshot(
            byte,
            "read_file",
            r#"{"path":"a.rs"}"#,
            ToolCallState::Completed,
            Some("ok"),
            false,
        ));
    }
    app.sessions.get_mut(&parent).unwrap().tool_calls = Some(calls);
    let child_id = SessionId::from_bytes([0x30; 16]);
    app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
        run_id: Some(RunId::from_bytes([0x31; 16])),
        occurred_at_ms: 2,
        ..fixtures::envelope(
            2,
            child_id,
            SessionEvent::SessionCreated {
                session: SessionSummary {
                    parent_id: Some(parent),
                    spawned_by: Some(qq_protocol::SpawnOrigin {
                        run_id,
                        tool_call_id: Some(spawn_call.id),
                    }),
                    title: "survey callers".to_owned(),
                    status: SessionStatus::Running,
                    active_run_id: Some(RunId::from_bytes([0x31; 16])),
                    activity: Some(qq_protocol::RunActivity::Reasoning),
                    model: None,
                    estimated_cost_usd_nanos: None,
                    updated_at_ms: 2,
                    ..fixtures::session_summary(child_id)
                },
            },
        )
    }));
    app.sidebar = crate::app::Sidebar::Hidden;

    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 40));
    let spawn_row = rows
        .iter()
        .position(|row| row.contains("spawn_agent"))
        .expect("spawn call is rendered, not folded");
    assert!(rows[spawn_row + 1].contains("↳"));
    assert!(rows[spawn_row + 1].contains("survey callers"));
    assert!(rows[spawn_row + 2].contains("reasoning"));
    assert!(
        rows.iter().all(|row| !row.contains("tool calls")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("related sessions")),
        "an inline child is not repeated below"
    );

    // A child with no recorded call attaches nowhere in the transcript
    // but still appears in related sessions.
    app.sessions.get_mut(&child_id).unwrap().summary.spawned_by = None;
    let rows = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 40));
    let spawn_row = rows
        .iter()
        .position(|row| row.contains("spawn_agent"))
        .unwrap();
    assert!(!rows[spawn_row + 1].contains("↳"));
    assert!(rows.iter().any(|row| row.contains("related sessions")));
}

#[test]
fn background_approvals_surface_a_banner_that_ctrl_g_jumps_to() {
    let mut app = app_with_messages(1);
    app.sidebar = crate::app::Sidebar::Hidden;
    let parent = app.focused().unwrap();
    let child_id = SessionId::from_bytes([0x40; 16]);
    let run_id = RunId::from_bytes([0x41; 16]);
    let mut sequence = 1;
    let mut event = |session_id, event| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    app.apply_client_update(event(
        child_id,
        SessionEvent::SessionCreated {
            session: SessionSummary {
                parent_id: Some(parent),
                title: "Deploy helper".to_owned(),
                status: SessionStatus::Running,
                active_run_id: Some(run_id),
                model: None,
                estimated_cost_usd_nanos: None,
                updated_at_ms: 2,
                ..fixtures::session_summary(child_id)
            },
        },
    ));
    let call = ToolCallSnapshot {
        run_id,
        call_ordinal: 0,
        provider_call_id: "c".to_owned(),
        arguments: r#"{"command":"rm -rf build"}"#.to_owned(),
        state: ToolCallState::AwaitingApproval,
        ..fixtures::tool_call(ToolCallId::from_bytes([0x42; 16]), child_id, "shell")
    };
    app.apply_client_update(event(
        child_id,
        SessionEvent::ToolApprovalRequested {
            tool_call: call,
            shell: None,
            edit: None,
        },
    ));

    // Focused on the parent: no modal, but the banner names the child.
    assert_eq!(app.mode(), Mode::Compose);
    let text = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24)).join("\n");
    assert!(text.contains("approval needed in Deploy helper"), "{text}");
    assert!(text.contains("Ctrl-G"));

    let (changed, requests) = app
        .handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )))
        .split();
    assert!(changed);
    assert_eq!(app.focused(), Some(child_id));
    // The child is cold, so the jump fetches its body...
    assert_eq!(requests.len(), 1);
    // ...and the banner no longer names the session we are now in.
    let text = frame_rows(&FrameRenderer::default().frame_and_commit(&mut app, 100, 24)).join("\n");
    assert!(!text.contains("approval needed in"));
}

#[test]
fn alt_arrows_walk_the_session_tree_in_spawn_order() {
    let mut app = app_with_messages(0);
    app.sidebar = crate::app::Sidebar::Hidden;
    let root = app.focused().unwrap();
    let mut sequence = 1;
    let mut created = |app: &mut App, byte: u8, parent: Option<SessionId>, at: u64| {
        sequence += 1;
        let id = SessionId::from_bytes([byte; 16]);
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            occurred_at_ms: sequence,
            ..fixtures::envelope(
                sequence,
                id,
                SessionEvent::SessionCreated {
                    session: SessionSummary {
                        parent_id: parent,
                        title: format!("s{byte}"),
                        model: None,
                        estimated_cost_usd_nanos: None,
                        updated_at_ms: at,
                        ..fixtures::session_summary(id)
                    },
                },
            )
        }));
        id
    };
    let a = created(&mut app, 0x51, Some(root), 10);
    let b = created(&mut app, 0x52, Some(root), 20);
    let c = created(&mut app, 0x53, Some(root), 30);
    let key = |code| TerminalEvent::Key(KeyEvent::new(code, KeyModifiers::ALT));

    app.handle_terminal_event(key(KeyCode::Down));
    assert_eq!(app.focused(), Some(a), "first child is the oldest");
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(b));
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(c));
    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(app.focused(), Some(a), "siblings wrap");
    app.handle_terminal_event(key(KeyCode::Left));
    assert_eq!(app.focused(), Some(c));
    // Esc walks up to the parent (Alt-Up belongs to the draft queue).
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.focused(), Some(root));
    // A lone root has no siblings; the key is a no-op.
    let (changed, _) = app.handle_terminal_event(key(KeyCode::Right)).split();
    assert!(!changed);
    assert_eq!(app.focused(), Some(root));
}

#[test]
fn reasoning_renders_collapsed_above_the_runs_message_and_expands_on_toggle() {
    let mut app = app_with_messages(0);
    app.sidebar = crate::app::Sidebar::Hidden;
    let session_id = app.focused().unwrap();
    let run_id = RunId::from_bytes([0x66; 16]);
    let mut sequence = 1;
    let mut event = |event: SessionEvent| {
        sequence += 1;
        ClientUpdate::Event(SessionEventEnvelope {
            run_id: Some(run_id),
            occurred_at_ms: sequence,
            ..fixtures::envelope(sequence, session_id, event)
        })
    };
    let kind = qq_protocol::ReasoningKind::Summary;
    app.apply_client_update(event(SessionEvent::ReasoningStarted { run_id, kind }));
    app.apply_client_update(event(SessionEvent::ReasoningDelta {
        run_id,
        kind,
        text: "First consider the callers.\n\nThen the tests.".to_owned(),
    }));
    app.apply_client_update(event(SessionEvent::ReasoningCompleted { run_id, kind }));
    app.apply_client_update(event(SessionEvent::AssistantMessageStarted {
        message: MessageSnapshot {
            run_id,
            state: MessageState::Streaming,
            ..fixtures::message(MessageId::from_bytes([0x67; 16]), session_id, "The answer.")
        },
    }));

    let rows = frame_rows(&transcript_lines(&app, 80));
    let reasoning_row = rows
        .iter()
        .position(|row| row.contains("thought for"))
        .expect("collapsed reasoning row");
    let message_row = rows.iter().position(|row| row.contains("QQ")).unwrap();
    assert!(reasoning_row < message_row, "{rows:?}");
    assert!(rows[reasoning_row].contains("First consider the callers."));
    assert!(!rows.iter().any(|row| row.contains("Then the tests.")));
    // Reasoning never leaks into the assistant message body.
    assert!(
        !app.sessions[&session_id].messages.as_ref().unwrap()[0]
            .output
            .contains("consider")
    );

    app.execute(crate::commands::Command::ToggleReasoning);
    let rows = frame_rows(&transcript_lines(&app, 80));
    assert!(rows.iter().any(|row| row.contains("Then the tests.")));
    assert!(rows.iter().any(|row| row.contains("┆")));
}

/// `app_with_messages` plus a second warm session titled "Other" whose
/// messages read `other N`.
fn app_with_two_sessions(count: u8) -> (App, SessionId, SessionId) {
    let mut app = app_with_messages(count);
    let first = app.focused().unwrap();
    let other = SessionId::from_bytes([9; 16]);
    let mut summary = app.sessions[&first].summary.clone();
    summary.id = other;
    summary.title = "Other".to_owned();
    let messages = (0..count)
        .map(|row| {
            let mut message = completed_message(0x80 + row, format!("other {row}"));
            message.session_id = other;
            message
        })
        .collect();
    app.apply_client_update(ClientUpdate::Snapshot(WorkspaceSnapshot {
        included: vec![SessionSnapshot {
            messages,
            ..fixtures::session_snapshot(summary.clone())
        }],
        cursor: fixtures::cursor(2),
        sessions: vec![summary],
        focused: None,
        ..fixtures::workspace_snapshot()
    }));
    (app, first, other)
}

#[test]
fn two_panes_render_side_by_side_with_titles_and_a_divider() {
    let (mut app, _, other) = app_with_two_sessions(3);
    app.sidebar = crate::app::Sidebar::Hidden;
    app.execute(Command::SplitBeside);
    app.focus_session(other);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 101, 24);
    let rows = frame_rows(&frame);
    // Row 2 is the pane title row: the left pane is unfocused, the right
    // pane carries the focus marker and its title.
    let titles = &rows[2];
    assert!(titles.contains(" Session"), "{titles}");
    assert!(titles.contains("▎Other"), "{titles}");
    // Every body row has a divider at the split column and both
    // transcripts appear on their own side of it.
    let body = &rows[2..2 + 24 - 6];
    assert!(
        body.iter().all(|row| row.chars().nth(50) == Some('│')),
        "{body:?}"
    );
    let (left, right): (Vec<&str>, Vec<&str>) =
        body.iter().map(|row| row.split_once('│').unwrap()).unzip();
    assert!(left.iter().any(|row| row.contains("row 2")));
    assert!(!left.iter().any(|row| row.contains("other")));
    assert!(right.iter().any(|row| row.contains("other 2")));
    assert!(!right.iter().any(|row| row.contains("row 2")));
    // The composer footer describes the focused pane's session.
    assert!(frame_text(&frame).contains("Other"));
    // Rows never exceed the frame width; a wide message cannot bleed
    // across the divider into its neighbour.
    assert!(frame.iter().all(|line| line.width() <= 101));
}

#[test]
fn stacked_panes_share_the_width_and_scroll_independently() {
    let (mut app, _, _) = app_with_two_sessions(40);
    app.sidebar = crate::app::Sidebar::Hidden;
    app.execute(Command::SplitBelow);
    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 80, 40);
    let (tiles, dividers) = app.panes.layout(crate::panes::Rect::new(0, 2, 80, 34));
    assert_eq!(tiles.len(), 2);
    assert_eq!(dividers[0].height, 1);
    assert_eq!(dividers[0].width, 80);
    let top = tiles[0].pane;
    let bottom = tiles[1].pane;
    assert_eq!(app.panes.focused_id(), bottom);

    // PageUp scrolls only the focused (bottom) pane.
    app.handle_terminal_event(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::PageUp,
            crossterm::event::KeyModifiers::NONE,
        ),
    ));
    let frame = renderer.frame_and_commit(&mut app, 80, 40);
    assert!(app.viewport(bottom).unwrap().offset() > 0);
    assert_eq!(app.viewport(top).unwrap().offset(), 0);
    let rows = frame_rows(&frame);
    let divider_row = rows
        .iter()
        .position(|row| row.starts_with("──"))
        .expect("horizontal divider");
    assert!(rows[..divider_row].iter().any(|row| row.contains("row 39")));
    assert!(!rows[divider_row..].iter().any(|row| row.contains("row 39")));
}

#[test]
fn a_height_only_resize_keeps_every_pane_cache() {
    let (mut app, _, other) = app_with_two_sessions(4);
    app.sidebar = crate::app::Sidebar::Hidden;
    app.execute(Command::SplitBeside);
    app.focus_session(other);
    let mut renderer = FrameRenderer::default();
    renderer.frame_and_commit(&mut app, 101, 24);
    let ids = app.panes.ids();
    let cached_before: Vec<usize> = ids
        .iter()
        .map(|id| renderer.cache(*id).markdown.len())
        .collect();
    assert_eq!(cached_before, vec![4, 4]);
    let widths: Vec<usize> = ids
        .iter()
        .map(|id| renderer.cache(*id).markdown.values().next().unwrap().width)
        .collect();

    renderer.frame_and_commit(&mut app, 101, 30);
    for (id, width) in ids.iter().zip(widths) {
        let cache = renderer.cache(*id);
        assert_eq!(cache.markdown.len(), 4);
        assert!(cache.markdown.values().all(|cached| cached.width == width));
    }
    // Closing a pane drops its cache on the next frame.
    app.execute(Command::ClosePane);
    renderer.frame_and_commit(&mut app, 101, 30);
    assert_eq!(renderer.panes.len(), 1);
}

#[test]
fn a_narrow_frame_shows_only_the_focused_pane_and_no_divider() {
    let (mut app, _, other) = app_with_two_sessions(2);
    app.sidebar = crate::app::Sidebar::Hidden;
    app.execute(Command::SplitBeside);
    app.focus_session(other);
    let frame = FrameRenderer::default().frame_and_commit(&mut app, 40, 16);
    let text = frame_text(&frame);
    assert!(text.contains("other 1"));
    assert!(!text.contains("row 1"));
    assert!(!text.contains('│'));
    assert_eq!(app.panes.len(), 2, "the tree is untouched");
}

#[test]
fn switching_theme_repaints_every_row_in_the_new_palette() {
    let mut app = app_with_messages(2);
    app.themes.push(crate::Theme::from_roles(
        "magenta",
        [crate::ThemeColor::Rgb(0xff, 0x00, 0xff); 8],
    ));
    let mut renderer = FrameRenderer::default();
    renderer.draw(&mut app, (80, 24)).unwrap();
    let brand_before = renderer.previous[0].spans[0].style.color;
    assert_eq!(brand_before, Some(Palette::QQ.brand));
    // A settled frame with nothing changed writes nothing.
    let idle = renderer.draw(&mut app, (80, 24)).unwrap();
    let idle_rows = String::from_utf8_lossy(&idle).matches("\x1b[2K").count();
    assert_eq!(idle_rows, 0);

    app.execute(Command::OpenThemes);
    app.handle_terminal_event(TerminalEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.theme().name, "magenta");
    let repaint = renderer.draw(&mut app, (80, 24)).unwrap();
    let repainted_rows = String::from_utf8_lossy(&repaint).matches("\x1b[2K").count();
    assert_eq!(repainted_rows, 24, "every row is rewritten");
    let magenta = crossterm::style::Color::Rgb {
        r: 0xff,
        g: 0,
        b: 0xff,
    };
    assert_eq!(renderer.previous[0].spans[0].style.color, Some(magenta));
    // Only the picker's swatches (painted in each theme's own colors)
    // may show anything but the new palette.
    assert!(
        renderer
            .previous
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.text != "██")
            .filter_map(|span| span.style.color)
            .all(|color| color == magenta),
        "no row keeps a color from the previous theme"
    );
    // Style helpers on this thread keep the last activated palette;
    // restore the default so later tests see the compiled look.
    theme::activate(Palette::QQ);
}
