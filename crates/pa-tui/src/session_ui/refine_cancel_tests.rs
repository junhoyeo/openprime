//! The refine cancel at the real client boundary: an active `/refine` plus the interrupt keys
//! sends TS `interruptOrClearInput`'s plain `abort` off the UI loop — never a queue clear and
//! never the streaming turn's `abort_and_send_queued` — although the daemon reports the running
//! session command as streaming (`isStreaming` is its busy flag).
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{ActivityUpdates, AgentView, PromptSubmitNote, RebuildKind, SessionUi};
use crate::daemon_client::{DaemonClient, DaemonClientEvent};
use crate::interactive::SessionSelection;

/// The `sessionActions` projection of a running `/refine`.
fn refine_actions() -> Value {
    json!({
        "steering": ["human steer"],
        "followUps": ["Heartbeat prompt: nudge"],
        "active": {"kind": "session_command", "phase": "committing", "label": "/refine --global"},
    })
}

struct MockDaemon {
    session: SessionUi,
    commands: mpsc::UnboundedReceiver<String>,
    events: mpsc::UnboundedReceiver<DaemonClientEvent>,
    prompt_notes: mpsc::UnboundedReceiver<PromptSubmitNote>,
    server: tokio::task::JoinHandle<()>,
}

/// A session attached over a mock daemon. `refining` makes the attach snapshot report a running
/// `/refine` (streaming, as the daemon reports it); otherwise the session attaches idle and the
/// mock answers a submitted `prompt` by publishing the running refine's queue projection. Every
/// command type reaches `commands`; `abort` is never answered, so a UI that awaited it hangs.
async fn mock_daemon(socket: &std::path::Path, refining: bool) -> MockDaemon {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"serverCapabilities\":[\"abort_and_send_queued\"],\"clientId\":\"mock\"}\n").await.unwrap();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            let kind = request["command"]["type"].as_str().unwrap().to_string();
            let _ = commands_tx.send(kind.clone());
            let mut state = json!({"sessionId": "s1", "cwd": "/tmp", "isStreaming": refining});
            if refining {
                state["sessionActions"] = refine_actions();
            }
            let data = match kind.as_str() {
                "abort" => continue,
                "attach" => json!({
                    "activeSessionId": "s1", "snapshot": {
                        "activeSessionId": "s1", "summary": {"id": "s1", "cwd": "/tmp"},
                        "state": state,
                        "messages": [], "lastEventSequence": 0,
                    }, "replay": {"status": "complete", "toSequence": 0},
                    "lastEventSequence": 0,
                }),
                _ => json!({}),
            };
            let mut reply = serde_json::to_vec(&json!({
                "type": "response", "id": request["id"], "command": kind,
                "success": true, "data": data,
            }))
            .unwrap();
            reply.push(b'\n');
            if kind == "prompt" {
                // The worker picks the direct `/refine` up as the running session command.
                let mut frame = serde_json::to_vec(&json!({
                    "type": "session_event", "activeSessionId": "s1",
                    "event": {"type": "session_action_update", "actions": refine_actions()},
                    "meta": {"sequence": 5},
                }))
                .unwrap();
                frame.push(b'\n');
                reply.extend(frame);
            }
            write.write_all(&reply).await.unwrap();
        }
    });
    let mut options =
        crate::interactive::tests::options(crate::interactive::ModelSelection::default());
    options.socket_path = socket.to_path_buf();
    options.session = SessionSelection::Attach("s1".to_string());
    let (client, events) = DaemonClient::connect(socket).await.unwrap();
    let (prompt_tx, prompt_notes) = mpsc::unbounded_channel();
    let session = SessionUi::open(
        client,
        &options,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        prompt_tx,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        ActivityUpdates {
            heartbeats: mpsc::unbounded_channel().0,
            bash: mpsc::unbounded_channel().0,
            factory: mpsc::unbounded_channel().0,
            commands: mpsc::unbounded_channel().0,
        },
        mpsc::unbounded_channel().0,
    )
    .await
    .unwrap();
    MockDaemon {
        session,
        commands,
        events,
        prompt_notes,
        server,
    }
}

/// Wait for the mock daemon to receive an abort-family command; every command seen on the way is
/// returned, the abort-family command last.
async fn commands_through_abort(commands: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Some(kind) = commands.recv().await {
        let done = matches!(
            kind.as_str(),
            "abort" | "abort_and_send_queued" | "abort_and_clear_queue" | "clear_queue"
        );
        seen.push(kind);
        if done {
            return seen;
        }
    }
    panic!("the mock daemon closed before an abort arrived: {seen:?}");
}

fn key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, modifiers)
}

fn theme_view() -> AgentView {
    AgentView::new(crate::theme::Theme::builtin(
        "prime",
        crate::theme::ColorMode::TrueColor,
    ))
}

/// Interrupt the running refine and require the plain `abort` as the first abort-family command;
/// the draft and the parked queue strip stay as they were.
async fn assert_interrupt_sends_plain_abort(
    mock: &mut MockDaemon,
    view: &mut AgentView,
    interrupt: crossterm::event::KeyEvent,
) {
    assert!(mock.session.refine_active, "the refine is running");
    assert!(mock.session.turn_active, "the daemon reports it streaming");
    assert_eq!(
        view.working.as_ref().map(|working| working.activity),
        Some("Refining"),
        "the Refining loader owns the frame"
    );
    view.editor.set_text("draft in progress");
    let queued_before = view.queued.clone();
    let mut running = true;
    // The key handler returns although the daemon never answers the abort.
    mock.session
        .handle_key(interrupt, view, &mut running)
        .await
        .unwrap();
    assert!(running);
    let seen = commands_through_abort(&mut mock.commands).await;
    assert_eq!(seen.last().map(String::as_str), Some("abort"), "{seen:?}");
    for unexpected in ["prompt", "steer", "follow_up"] {
        assert!(!seen.iter().any(|kind| kind == unexpected), "{seen:?}");
    }
    assert_eq!(view.editor.get_text(), "draft in progress");
    assert_eq!(view.queued, queued_before);
    assert_eq!(view.queued.steering, ["human steer"]);
}

#[tokio::test]
async fn interrupting_an_attached_refine_sends_a_plain_abort() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for interrupt in [
            key(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            key(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut mock = mock_daemon(&dir.path().join("refine.sock"), true).await;
            let mut view = theme_view();
            mock.session.rebuild_view(&mut view, &RebuildKind::Rebind);
            assert_interrupt_sends_plain_abort(&mut mock, &mut view, interrupt).await;
            mock.server.abort();
        }
    })
    .await
    .expect("both interrupts reach the mock daemon within ten seconds");
}

#[tokio::test]
async fn interrupting_a_submitted_refine_sends_a_plain_abort() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let mut mock = mock_daemon(&dir.path().join("submit.sock"), false).await;
        let mut view = theme_view();
        mock.session.rebuild_view(&mut view, &RebuildKind::Rebind);
        assert!(!mock.session.refine_active);
        view.editor.set_text("/refine --global");
        let mut running = true;
        mock.session
            .handle_key(
                key(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE,
                ),
                &mut view,
                &mut running,
            )
            .await
            .unwrap();
        // The live submit: the prompt ACK starts the turn, then the worker's projection marks
        // the running refine.
        let note = mock.prompt_notes.recv().await.expect("the prompt outcome");
        mock.session
            .apply_prompt_outcome(note, &mut view)
            .await
            .unwrap();
        let event = mock.events.recv().await.expect("the queue projection");
        mock.session.apply_client_event(event, &mut view);
        while let Some(kind) = mock.commands.recv().await {
            if kind == "prompt" {
                break;
            }
        }
        assert_interrupt_sends_plain_abort(
            &mut mock,
            &mut view,
            key(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        )
        .await;
        mock.server.abort();
    })
    .await
    .expect("the submitted refine is interrupted within ten seconds");
}
