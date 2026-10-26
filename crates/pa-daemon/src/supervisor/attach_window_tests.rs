//! The routed attach window (legacy `da68b1903` pending-binding hold and replay,
//! `df0cf0752`/`0d2e4ed73`/`b66ce86d3` catch-up during the attach await): the
//! worker cuts its attach snapshot under the core lock, and any event it emits
//! after the cut is flushed AHEAD of its response on the worker link (the
//! response's flush barrier). The supervisor publishes that event the moment it
//! reads it, so the attaching connection must already be subscribed, or the
//! transcript update the snapshot does not contain is lost until the next attach.
//! A fake worker writes the exact wire order: the post-cut event, then the
//! response whose cursor precedes it.

use super::subscribers::ClientSubscriptions;
use super::*;

const WORKER_ID: &str = "w-window";
/// The bound on every wait: a step that cannot finish within it fails the test
/// (load-tolerant, never a hang).
const STEP_BOUND: Duration = Duration::from_secs(60);
/// The attach snapshot's cursor; the racing event is the next sequence.
const SNAPSHOT_CURSOR: u64 = 7;

struct FakeWorker {
    // Held so the bound socket path stays owned for the connection's lifetime.
    #[allow(dead_code)]
    listener: Box<dyn pa_types::platform::transport::TransportListener>,
    read_half: Box<dyn pa_types::platform::transport::AsyncReadHalf>,
    write_half: Box<dyn pa_types::platform::transport::AsyncWriteHalf>,
}

impl FakeWorker {
    async fn next_request(&mut self) -> (String, String) {
        let mut reader = PrivateFrameReader::new(&mut self.read_half, DEFAULT_PRIVATE_FRAME_LIMITS);
        let frame = tokio::time::timeout(STEP_BOUND, reader.read_frame())
            .await
            .expect("the supervisor sends its next request")
            .expect("the supervisor's frame")
            .expect("the connection stays open");
        let field = |name: &str| {
            frame
                .header
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        (field("requestId"), field("commandType"))
    }

    async fn write(&mut self, header: &Value, payload: &[u8]) {
        write_frame(
            &mut self.write_half,
            header,
            payload,
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .await
        .expect("write a worker frame");
    }

    async fn answer(&mut self, request_id: &str, response: &DaemonResponse) {
        let mut header = json!({
            "kind": "outbound",
            "requestId": request_id,
            "outboundType": "response",
        });
        // The real worker's attach header carries the routing scalars.
        if response.command == "attach" {
            header["ok"] = json!(response.success);
            if response.success {
                header["activeSessionId"] = json!(WORKER_ID);
            }
        }
        self.write(&header, &crate::protocol::response_line_bytes(response))
            .await;
    }

    /// One live session event, as the worker's fan-out frames it.
    async fn emit(&mut self, sequence: u64, text: &str) {
        let payload = json!({
            "type": "session_event",
            "activeSessionId": WORKER_ID,
            "event": {
                "type": "message_end",
                "message": { "role": "user", "content": text },
            },
            "meta": { "sequence": sequence, "cursor": { "generation": "g1", "sequence": sequence } },
        });
        self.write(
            &json!({
                "kind": "outbound",
                "outboundType": "session_event",
                "activeSessionId": WORKER_ID,
            }),
            &serde_json::to_vec(&payload).expect("event payload"),
        )
        .await;
    }
}

/// A supervisor with one resident whose worker link the test drives by hand.
async fn supervisor_with_fake_worker() -> (Arc<Supervisor>, FakeWorker) {
    let dir = std::env::temp_dir().join(format!("pa-attach-window-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("worker.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir,
        })
        .expect("supervisor"),
    );
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
        "version": 2,
        "workerId": WORKER_ID,
        "pid": 4242,
        "socketPath": socket_path.to_string_lossy(),
        "recoveryJournalPath": "/tmp/none.jsonl",
        "supervisorSocketPath": "/tmp/none.sock",
        "authenticationToken": "window-token",
        "rootActiveSessionId": WORKER_ID,
        "createdAt": "2026-10-27T00:00:00Z",
        "updatedAt": "2026-10-27T00:00:00Z",
        "lifecycle": "ready",
        "createCommand": {},
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let resident = ResidentWorker::new(WORKER_ID.to_string(), descriptor, dir.join("w.json"));
    supervisor.registry.insert(Arc::clone(&resident)).await;
    let listener = pa_types::platform::transport::bind_transport(&socket_path)
        .await
        .expect("bind the fake worker socket");
    let connect = {
        let supervisor = Arc::clone(&supervisor);
        let resident = Arc::clone(&resident);
        tokio::spawn(async move {
            supervisor
                .connect_worker(&resident, worker_connect_deadline())
                .await
        })
    };
    let stream = listener.accept().await.expect("accept the supervisor");
    let (read_half, write_half) = stream.split();
    let mut worker = FakeWorker {
        listener,
        read_half,
        write_half,
    };
    let (request_id, command) = worker.next_request().await;
    assert_eq!(command, "worker_auth");
    worker
        .answer(
            &request_id,
            &DaemonResponse {
                id: None,
                command: "worker_auth".to_string(),
                success: true,
                data: Some(json!({ "capabilities": ["agent_roster"] })),
                error: None,
                error_info: None,
            },
        )
        .await;
    connect
        .await
        .expect("the connect task lives")
        .expect("the handshake completes");
    // The create boundary: client commands route only to a session-ready worker.
    resident.note_session_ready();
    (supervisor, worker)
}

/// The interactive client's attach command.
fn tui_attach() -> DaemonCommand {
    serde_json::from_value(json!({
        "type": "attach",
        "activeSessionId": WORKER_ID,
        "capabilities": [
            "attach_snapshot",
            "event_sequence",
            "slim_attach",
            "elide_snapshot_images",
            "windowed_snapshot",
        ],
    }))
    .expect("attach command")
}

fn attach_success() -> DaemonResponse {
    DaemonResponse {
        id: None,
        command: "attach".to_string(),
        success: true,
        data: Some(json!({
            "activeSessionId": WORKER_ID,
            "snapshot": {
                "activeSessionId": WORKER_ID,
                "messages": [],
                "lastEventSequence": SNAPSHOT_CURSOR,
                "lastEventCursor": { "generation": "g1", "sequence": SNAPSHOT_CURSOR },
            },
            "lastEventSequence": SNAPSHOT_CURSOR,
            "lastEventCursor": { "generation": "g1", "sequence": SNAPSHOT_CURSOR },
        })),
        error: None,
        error_info: None,
    }
}

/// Route one client attach over the connection's raw relay (the product's
/// connection dispatch path); the fake worker answers via `serve`.
async fn route_attach(
    supervisor: &Arc<Supervisor>,
    attached: &Arc<ClientSubscriptions>,
) -> Vec<Value> {
    let (raw_tx, mut raw_rx) = mpsc::channel(8);
    let (lines, _) = supervisor
        .route_client_command(
            &tui_attach(),
            "window-client",
            attached,
            "c1".to_string(),
            "attach".to_string(),
            Some(&raw_tx),
        )
        .await;
    let mut relayed = Vec::new();
    while let Ok((frames, _)) = raw_rx.try_recv() {
        relayed.extend(frames);
    }
    assert!(
        !lines.is_empty() || !relayed.is_empty(),
        "the attach answered the client"
    );
    lines
}

fn sequences(queue: &mut mpsc::Receiver<Arc<Value>>) -> Vec<u64> {
    let mut heard = Vec::new();
    while let Ok(frame) = queue.try_recv() {
        heard.push(frame["meta"]["sequence"].as_u64().expect("sequence"));
    }
    heard
}

/// The post-cut event the worker flushes ahead of its attach response reaches the
/// attaching connection: the subscription is in place before the route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_cut_after_the_attach_snapshot_reaches_the_attaching_client() {
    let (supervisor, mut worker) = supervisor_with_fake_worker().await;
    let (queue_tx, mut queue) = mpsc::channel(64);
    let attached = ClientSubscriptions::new("conn-window".to_string(), queue_tx);
    let route = {
        let supervisor = Arc::clone(&supervisor);
        let attached = Arc::clone(&attached);
        tokio::spawn(async move { route_attach(&supervisor, &attached).await })
    };
    let (request_id, command) = worker.next_request().await;
    assert_eq!(command, "attach");
    // The real worker's order: the event emitted after the snapshot cut is flushed
    // before the response that carries the older cursor.
    worker
        .emit(SNAPSHOT_CURSOR + 1, "the update the snapshot lacks")
        .await;
    worker.answer(&request_id, &attach_success()).await;
    tokio::time::timeout(STEP_BOUND, route)
        .await
        .expect("the attach route settles")
        .expect("the route task lives");
    assert_eq!(
        sequences(&mut queue),
        vec![SNAPSHOT_CURSOR + 1],
        "the attaching client hears the event its snapshot does not cover"
    );
    assert_eq!(attached.session_ids(), vec![WORKER_ID.to_string()]);
    // Live events keep flowing to the attached connection afterwards.
    worker.emit(SNAPSHOT_CURSOR + 2, "a later update").await;
    let later = tokio::time::timeout(STEP_BOUND, queue.recv())
        .await
        .expect("the attached client hears live events")
        .expect("the queue stays open");
    assert_eq!(later["meta"]["sequence"], json!(SNAPSHOT_CURSOR + 2));
}

/// A refused attach leaves no subscription behind: the early subscription is
/// retired with the refusal, so the connection stays deaf to the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_attach_retires_its_early_subscription() {
    let (supervisor, mut worker) = supervisor_with_fake_worker().await;
    let (queue_tx, mut queue) = mpsc::channel(64);
    let attached = ClientSubscriptions::new("conn-refused".to_string(), queue_tx);
    let route = {
        let supervisor = Arc::clone(&supervisor);
        let attached = Arc::clone(&attached);
        tokio::spawn(async move { route_attach(&supervisor, &attached).await })
    };
    let (request_id, command) = worker.next_request().await;
    assert_eq!(command, "attach");
    worker
        .answer(
            &request_id,
            &DaemonResponse {
                id: None,
                command: "attach".to_string(),
                success: false,
                data: None,
                error: Some("Session is shutting down".to_string()),
                error_info: None,
            },
        )
        .await;
    tokio::time::timeout(STEP_BOUND, route)
        .await
        .expect("the attach route settles")
        .expect("the route task lives");
    assert!(
        attached.session_ids().is_empty(),
        "a refused attach subscribes nothing: {:?}",
        attached.session_ids()
    );
    // The session's next publish does not reach the refused connection.
    supervisor.publish_session_event(WORKER_ID, &Arc::new(json!({ "probe": true })));
    assert!(
        queue.try_recv().is_err(),
        "the refused client hears nothing"
    );
}

/// A refused re-attach keeps the subscription the connection already held: the
/// rollback retires only what this attach created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_reattach_keeps_the_existing_subscription() {
    let (supervisor, mut worker) = supervisor_with_fake_worker().await;
    let (queue_tx, mut queue) = mpsc::channel(64);
    let attached = ClientSubscriptions::new("conn-held".to_string(), queue_tx);
    attached.attach(&supervisor.session_subscribers, WORKER_ID);
    let route = {
        let supervisor = Arc::clone(&supervisor);
        let attached = Arc::clone(&attached);
        tokio::spawn(async move { route_attach(&supervisor, &attached).await })
    };
    let (request_id, _) = worker.next_request().await;
    worker
        .answer(
            &request_id,
            &DaemonResponse {
                id: None,
                command: "attach".to_string(),
                success: false,
                data: None,
                error: Some("Session is shutting down".to_string()),
                error_info: None,
            },
        )
        .await;
    tokio::time::timeout(STEP_BOUND, route)
        .await
        .expect("the attach route settles")
        .expect("the route task lives");
    assert_eq!(attached.session_ids(), vec![WORKER_ID.to_string()]);
    worker
        .emit(SNAPSHOT_CURSOR + 1, "still for this client")
        .await;
    let event = tokio::time::timeout(STEP_BOUND, queue.recv())
        .await
        .expect("the held subscription still delivers")
        .expect("the queue stays open");
    assert_eq!(event["meta"]["sequence"], json!(SNAPSHOT_CURSOR + 1));
}
