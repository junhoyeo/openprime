//! The refine cancel at the real client boundary: an active `/refine` plus the interrupt keys
//! sends TS `interruptOrClearInput`'s plain `abort` off the UI loop and never a queue clear.
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{ActivityUpdates, AgentView, RebuildKind, SessionUi};
use crate::daemon_client::DaemonClient;
use crate::interactive::SessionSelection;

/// A session attached over a mock daemon whose snapshot reports a running `/refine` with a human
/// and an internal row queued. Every command type reaches `commands`; `abort` is never answered,
/// so a UI that awaited it would hang.
async fn refining_session(
    socket: &std::path::Path,
) -> (
    SessionUi,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"serverCapabilities\":[\"abort_and_send_queued\",\"refine_queue_restore\"],\"clientId\":\"mock\"}\n").await.unwrap();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            let kind = request["command"]["type"].as_str().unwrap().to_string();
            let _ = commands_tx.send(kind.clone());
            let data = match kind.as_str() {
                "abort" => continue,
                "attach" => json!({
                    "activeSessionId": "s1", "snapshot": {
                        "activeSessionId": "s1", "summary": {"id": "s1", "cwd": "/tmp"},
                        "state": {
                            "sessionId": "s1", "cwd": "/tmp", "isStreaming": false,
                            "sessionActions": {
                                "steering": ["human steer"],
                                "followUps": ["Heartbeat prompt: nudge"],
                                "active": {"kind": "session_command", "phase": "running", "label": "/refine"},
                            },
                        },
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
            write.write_all(&reply).await.unwrap();
        }
    });
    let mut options =
        crate::interactive::tests::options(crate::interactive::ModelSelection::default());
    options.socket_path = socket.to_path_buf();
    options.session = SessionSelection::Attach("s1".to_string());
    let (client, _events) = DaemonClient::connect(socket).await.unwrap();
    let session = SessionUi::open(
        client,
        &options,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
        mpsc::unbounded_channel().0,
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
    (session, commands, server)
}

/// Wait for the mock daemon to receive an `abort`; every command seen on the way is returned.
async fn commands_through_abort(commands: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Some(kind) = commands.recv().await {
        let done = kind == "abort";
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

#[tokio::test]
async fn interrupting_an_active_refine_sends_a_plain_abort_and_keeps_the_queue() {
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
            let socket = dir.path().join("refine.sock");
            let (mut session, mut commands, server) = refining_session(&socket).await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            assert!(
                session.refine_active,
                "the attach reports the running refine"
            );
            view.editor.set_text("draft in progress");
            let queued_before = view.queued.clone();
            let mut running = true;
            // The key handler returns although the daemon never answers the abort: the
            // request runs off the UI loop.
            session
                .handle_key(interrupt, &mut view, &mut running)
                .await
                .unwrap();
            assert!(running);
            let seen = commands_through_abort(&mut commands).await;
            for destructive in [
                "abort_and_clear_queue",
                "clear_queue",
                "abort_and_send_queued",
                "prompt",
                "steer",
                "follow_up",
            ] {
                assert!(
                    !seen.iter().any(|kind| kind == destructive),
                    "{interrupt:?} sent {destructive}: {seen:?}"
                );
            }
            assert_eq!(view.editor.get_text(), "draft in progress");
            assert_eq!(view.queued, queued_before);
            assert_eq!(
                view.queued.steering,
                ["human steer"],
                "the parked queue stays visible"
            );
            drop(session);
            server.abort();
        }
    })
    .await
    .expect("both interrupts reach the mock daemon within ten seconds");
}
