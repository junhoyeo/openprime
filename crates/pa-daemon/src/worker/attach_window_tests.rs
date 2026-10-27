//! The direct-link attach window (legacy `da68b1903` pending-binding hold and
//! replay): a session client's event stream must be open BEFORE its attach cuts
//! the snapshot. The fan-out decides per frame whether this connection streams;
//! a frame it reads while the attach is still cutting would otherwise be skipped,
//! and an event emitted after the cut is a transcript update the snapshot lacks.
//!
//! A dedicated OS thread holds the core lock (the lock the attach cuts its snapshot
//! under), so the attach is parked before its cut; the holder then emits an event
//! and the test lets the fan-out settle it. Every step is bounded and the holder
//! releases on its own when a signal never comes, so the test fails rather than
//! hangs. The connection must stream it: the client's snapshot cursor,
//! not the server, decides what the snapshot already covers.
#![cfg(unix)]

use super::*;
use tokio::io::AsyncWriteExt;

const SESSION: &str = "window-session";
const INSTANCE: &str = "window-instance";
const GENERATION: &str = "window-generation";

/// One framed connection to the worker socket.
struct Link {
    reader: crate::framing::PrivateFrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl Link {
    async fn connect(socket: &std::path::Path) -> Self {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .expect("connect the worker socket");
        let (read_half, writer) = stream.into_split();
        let mut link = Link {
            reader: crate::framing::PrivateFrameReader::new(
                read_half,
                DEFAULT_PRIVATE_FRAME_LIMITS,
            ),
            writer,
            next_id: 0,
        };
        let (header, _) = link.frame().await;
        assert_eq!(header["outboundType"], "daemon_hello");
        link
    }

    async fn frame(&mut self) -> (Value, Value) {
        let frame = tokio::time::timeout(STEP_BOUND, self.reader.read_frame())
            .await
            .expect("the worker answers")
            .expect("a frame")
            .expect("the connection stays open");
        let payload = serde_json::from_slice(&frame.payload).expect("frame payload");
        (frame.header, payload)
    }

    async fn send(&mut self, command_type: &str, payload: &Value) -> String {
        self.next_id += 1;
        let request_id = format!("req-{}", self.next_id);
        let frame = crate::framing::encode_private_frame(
            &json!({ "kind": "command", "requestId": request_id, "commandType": command_type }),
            &serde_json::to_vec(payload).expect("payload"),
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .expect("encode a command frame");
        self.writer.write_all(&frame).await.expect("write a frame");
        request_id
    }

    /// Read to the response for `request_id`; returns it and the session-event
    /// sequences streamed ahead of it.
    async fn response(&mut self, request_id: &str) -> (Value, Vec<u64>) {
        let mut streamed = Vec::new();
        loop {
            let (header, payload) = self.frame().await;
            match header["outboundType"].as_str() {
                Some("response") if header["requestId"] == request_id => {
                    return (payload, streamed);
                }
                Some("session_event") => {
                    streamed.push(payload["meta"]["sequence"].as_u64().expect("sequence"));
                }
                _ => {}
            }
        }
    }

    async fn request(&mut self, command_type: &str, payload: &Value) -> (Value, Vec<u64>) {
        let request_id = self.send(command_type, payload).await;
        self.response(&request_id).await
    }
}

/// A created worker serving its socket, plus a session client admitted through
/// the supervisor-registered grant (the product's direct-transport path).
/// The supervisor link rides along: dropping it would close a connection whose
/// release takes the core lock the window test holds.
async fn served_worker_with_session_client() -> (Arc<Worker>, Link, Link) {
    let dir = std::env::temp_dir().join(format!("pa-window-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("w.sock");
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: socket.clone(),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "window-token".to_string(),
            worker_instance_id: INSTANCE.to_string(),
            active_session_id: SESSION.to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        },
        None,
    ));
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "window" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    tokio::spawn(Arc::clone(&worker).serve());
    let deadline = std::time::Instant::now() + STEP_BOUND;
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker socket never came up"
        );
        tokio::task::yield_now().await;
    }
    let mut supervisor = Link::connect(&socket).await;
    let (auth, _) = supervisor
        .request(
            "worker_auth",
            &json!({
                "type": "worker_auth",
                "token": "window-token",
                "supervisorGeneration": GENERATION,
                "supervisorPid": 4242,
                "supervisorSocketPath": "/tmp/none.sock",
            }),
        )
        .await;
    assert_eq!(auth["success"], true, "worker auth failed: {auth}");
    let grant = json!({
        "grantId": "window-grant",
        "token": "window-grant-token",
        "expiresAt": crate::util::iso_from_unix_ms(crate::util::now_ms() + 5_000),
        "purpose": "session_client",
        "workerInstanceId": INSTANCE,
        "activeSessionId": SESSION,
        "issuerGeneration": GENERATION,
    });
    let (registered, _) = supervisor
        .request(
            "worker_register_peer_transport",
            &json!({ "type": "worker_register_peer_transport", "grant": grant }),
        )
        .await;
    assert_eq!(
        registered["success"], true,
        "grant registration failed: {registered}"
    );
    let mut client = Link::connect(&socket).await;
    let (admitted, _) = client
        .request(
            "peer_auth",
            &json!({
                "type": "peer_auth",
                "grantId": "window-grant",
                "token": "window-grant-token",
                "workerInstanceId": INSTANCE,
                "purpose": "session_client",
            }),
        )
        .await;
    assert_eq!(admitted["success"], true, "peer auth failed: {admitted}");
    (worker, client, supervisor)
}

/// A command the peer gate refuses in the reader loop itself: its answer proves
/// every earlier frame was read, and (through the response flush barrier) that
/// the fan-out settled every event emitted before it.
fn reader_barrier() -> Value {
    json!({ "type": "list" })
}

/// The bound on every step of the held-lock seam: a step that cannot finish within
/// it fails the test instead of hanging it.
const STEP_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// One step the lock holder takes on the test's signal.
enum HolderStep {
    /// Emit one session event under the held lock (an emit path's exact framing).
    Emit,
    /// Release the lock.
    Release,
}

/// Hold the worker's core lock on a dedicated OS thread — never inside a runtime
/// task, so no runtime worker is parked on it — until told to release, or until
/// `STEP_BOUND` passes without a signal (the fail-closed release). Returns the
/// signal sender and a receiver for the emitted sequence.
fn hold_core_lock(
    worker: &Arc<Worker>,
) -> (
    std::sync::mpsc::Sender<HolderStep>,
    std::sync::mpsc::Receiver<u64>,
    std::thread::JoinHandle<()>,
) {
    let (step_tx, step_rx) = std::sync::mpsc::channel::<HolderStep>();
    let (emitted_tx, emitted_rx) = std::sync::mpsc::channel::<u64>();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let worker = Arc::clone(worker);
    let holder = std::thread::spawn(move || {
        let mut core = worker.core.lock().unwrap();
        held_tx.send(()).expect("the test waits for the hold");
        while let Ok(HolderStep::Emit) = step_rx.recv_timeout(STEP_BOUND) {
            let sequence = core.last_event_sequence + 1;
            core.last_event_sequence = sequence;
            let meta = crate::protocol::create_daemon_event_meta(
                &core.active_session_id,
                sequence,
                None,
                Some(&core.generation),
            );
            let outbound = DaemonOutbound::SessionEvent {
                active_session_id: core.active_session_id.clone(),
                event: json!({ "type": "session_info_changed", "name": "racing update" }),
                meta: Some(meta),
                rest: Map::default(),
            };
            worker.events.send(OutboundFrame::session_event(
                serde_json::to_vec(&outbound).expect("event payload"),
            ));
            let _ = emitted_tx.send(sequence);
        }
    });
    held_rx
        .recv_timeout(STEP_BOUND)
        .expect("the holder takes the core lock");
    (step_tx, emitted_rx, holder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_attach_streams_an_event_emitted_while_its_snapshot_is_pending() {
    let (worker, mut client, _supervisor) = served_worker_with_session_client().await;
    // The attach parks on the held core lock, before its snapshot cut.
    let (step, emitted, holder) = hold_core_lock(&worker);
    let attach = client
        .send(
            "attach",
            &json!({
                "type": "attach",
                "activeSessionId": SESSION,
                "clientId": "window-client",
                "capabilities": ["attach_snapshot", "event_sequence", "slim_attach", "windowed_snapshot"],
            }),
        )
        .await;
    // PROBE ONLY (throwaway branch): widen the gap between the attach and list writes.
    if let Some(ms) = std::env::var("ATTACH_PROBE_GAP_MS").ok().and_then(|ms| ms.parse().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let (refused, _) = client.request("list", &reader_barrier()).await;
    assert_eq!(refused["success"], false, "the peer gate refuses list");
    // An event emitted while the attach waits to cut its snapshot.
    step.send(HolderStep::Emit).expect("the holder is waiting");
    let sequence = emitted
        .recv_timeout(STEP_BOUND)
        .expect("the holder emits under the lock");
    let (_, streamed) = client.request("list", &reader_barrier()).await;
    step.send(HolderStep::Release)
        .expect("the holder is waiting");
    holder.join().expect("the holder thread");
    let (attached, after) = client.response(&attach).await;
    assert_eq!(attached["success"], true, "attach failed: {attached}");
    assert_eq!(
        streamed,
        vec![sequence],
        "the attaching client streams the event emitted while its snapshot was pending"
    );
    assert!(after.is_empty(), "nothing else streamed: {after:?}");
    assert_eq!(
        attached["data"]["lastEventSequence"],
        json!(sequence),
        "the snapshot cut after the event covers it (the client's cursor discards the replay)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_direct_attach_leaves_the_stream_closed() {
    let (worker, mut client, _supervisor) = served_worker_with_session_client().await;
    worker.core.lock().unwrap().shutdown_requested = true;
    let (refused, _) = client
        .request(
            "attach",
            &json!({ "type": "attach", "activeSessionId": SESSION, "clientId": "window-client" }),
        )
        .await;
    assert_eq!(
        refused["success"], false,
        "the closing session refuses the attach"
    );
    worker.events.send(OutboundFrame::session_event(
        serde_json::to_vec(&json!({
            "type": "session_event",
            "activeSessionId": SESSION,
            "event": { "type": "session_info_changed", "name": "not streamed" },
            "meta": { "sequence": 99 },
        }))
        .expect("event payload"),
    ));
    let (_, streamed) = client.request("list", &reader_barrier()).await;
    assert!(
        streamed.is_empty(),
        "a refused attach streams nothing: {streamed:?}"
    );
}
