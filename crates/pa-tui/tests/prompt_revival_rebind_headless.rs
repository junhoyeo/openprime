//! Headless client half of legacy TS row 502e7a496 ("revive archived
//! sessions on prompt instead of dead-ending"): the window's prompt to a
//! session whose worker the daemon passivated is refused with
//! `Unknown active session: <held id>` (the daemon half, pinned in
//! `pa-daemon/tests/revival_context_e2e.rs`); the client re-attaches by the
//! DURABLE session id — the attach the daemon answers by waking the saved
//! session — and replays the prompt exactly once to the revived id, so the
//! turn runs and no error row dead-ends the window.
#![cfg(unix)]
// The mock is one flat request table; splitting it adds indirection.
#![allow(clippy::too_many_lines)]
// Futures are bounded by the surface's lifetime.
#![allow(clippy::large_futures)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

/// The held id the passivated worker had, and the revived worker's id.
const HELD: &str = "s1";
const REVIVED: &str = "s2";
const DURABLE: &str = "sess-1";

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

fn write_session_event(writer: &mut UnixStream, active: &str, event: &Value) {
    write_json(
        writer,
        &json!({ "type": "session_event", "activeSessionId": active, "event": event }),
    );
}

/// The slim attach result for `active` hosting the durable session.
fn attach_data(id: &str, active: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": active,
            "snapshot": {
                "activeSessionId": active,
                "summary": { "id": active, "cwd": "/tmp" },
                "state": {
                    "activeSessionId": active,
                    "cwd": "/tmp",
                    "sessionId": DURABLE,
                    "sessionName": "revival session",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

/// The daemon after an idle passivation: the held id is gone, the durable
/// id's attach wakes a fresh worker, and only the revived id runs turns.
fn serve(listener: &UnixListener, requests: &Arc<Mutex<Vec<Value>>>) {
    let (stream, _) = listener.accept().expect("accept");
    let mut writer = stream.try_clone().expect("clone mock socket");
    let mut reader = BufReader::new(stream);
    write_json(
        &mut writer,
        &json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
            "clientId": "mock",
        }),
    );
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
        let command = envelope.get("command").cloned().unwrap_or(Value::Null);
        let command_type = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target = command
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if matches!(command_type.as_str(), "attach" | "prompt" | "create") {
            requests.lock().unwrap().push(command.clone());
        }
        match (command_type.as_str(), target.as_str()) {
            ("create", _) => write_json(
                &mut writer,
                &json!({
                    "type": "response", "id": id, "command": "create", "success": true,
                    "data": {
                        "activeSessionId": HELD, "id": HELD,
                        "sessionId": DURABLE, "sessionFile": "/tmp/sess-1.jsonl",
                    },
                }),
            ),
            ("attach", HELD) => write_json(&mut writer, &attach_data(id, HELD)),
            ("attach", DURABLE) => write_json(&mut writer, &attach_data(id, REVIVED)),
            ("prompt", HELD) => write_json(
                &mut writer,
                &json!({
                    "type": "response", "id": id, "command": "prompt", "success": false,
                    "error": format!("Unknown active session: {HELD}"),
                }),
            ),
            ("prompt", REVIVED) => {
                write_json(
                    &mut writer,
                    &json!({ "type": "response", "id": id, "command": "prompt", "success": true }),
                );
                let question = command
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let answer = json!({
                    "role": "assistant",
                    "stopReason": "stop",
                    "content": [{ "type": "text", "text": "the revived answer" }],
                });
                write_session_event(&mut writer, REVIVED, &json!({ "type": "turn_start" }));
                write_session_event(
                    &mut writer,
                    REVIVED,
                    &json!({
                        "type": "message_start",
                        "message": { "role": "user", "content": question },
                    }),
                );
                write_session_event(
                    &mut writer,
                    REVIVED,
                    &json!({ "type": "message_start", "message": answer }),
                );
                write_session_event(
                    &mut writer,
                    REVIVED,
                    &json!({ "type": "message_end", "message": answer }),
                );
                write_session_event(&mut writer, REVIVED, &json!({ "type": "turn_end" }));
            }
            _ => write_json(
                &mut writer,
                &json!({
                    "type": "response", "id": id, "command": command_type,
                    "success": true, "data": {},
                }),
            ),
        }
    }
}

fn options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: false,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

#[test]
fn a_prompt_to_a_passivated_session_rebinds_by_its_durable_id_and_replays_once() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let served = Arc::clone(&requests);
    let handle = std::thread::spawn(move || serve(&listener, &served));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::Submit("continue after idle".to_string()),
            HeadlessStep::WaitRender {
                needle: "the revived answer".to_string(),
                timeout_ms: 15_000,
            },
        ],
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    let _ = handle.join();
    let requests = requests.lock().unwrap().clone();
    let route: Vec<(String, String)> = requests
        .iter()
        .map(|command| {
            (
                command["type"].as_str().unwrap_or_default().to_string(),
                command["activeSessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    let pair = |kind: &str, target: &str| (kind.to_string(), target.to_string());
    assert_eq!(
        route,
        vec![
            pair("create", ""),
            pair("attach", HELD),
            pair("prompt", HELD),
            pair("attach", DURABLE),
            pair("prompt", REVIVED),
        ],
        "the refused prompt rebinds by the durable id and replays exactly once"
    );
    assert_eq!(
        requests[2]["message"], requests[4]["message"],
        "the replay carries the same prompt"
    );
    let text = outcome.frames.join("\n");
    assert!(
        !text.contains("Unknown active session"),
        "the revival never surfaces the dead-end error:\n{text}"
    );
}
