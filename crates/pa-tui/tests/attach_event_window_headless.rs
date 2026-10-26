//! Headless e2e for the attach window, the TUI side of the legacy TS client's
//! pending-binding hold and replay (`da68b1903`, `df0cf0752`, `0d2e4ed73`,
//! `b66ce86d3`) and of its supersession guards (`a59f857cd`, `3201db643`,
//! `4d987e700`, `50b835d49`): a mock daemon writes the wire orders the real
//! daemon can produce, and the rendered transcript is the oracle.
//!
//! - An attach's live events may cross the wire around its response. The ones
//!   the attach snapshot already covers (sequence at or below its cursor, same
//!   generation) must not render twice; the newer ones must render once.
//! - After a `session_binding` supersede notice the view rebinds to the new id:
//!   late frames for the superseded id (a live row, a `session_closed`) never
//!   touch the rebound view, while a real close of the bound session still shows.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

const COVERED_PROMPT: &str = "the prompt the snapshot already holds";
const NEWER_PROMPT: &str = "the prompt sent after the snapshot";
const FIRST_ROW: &str = "the transcript before the rebind";
const REBOUND_ROW: &str = "the rebound worker's latest row";
const STALE_ROW: &str = "a late row for the superseded id";
const LIVE_ROW: &str = "a live row on the rebound id";
const STOPPED_TEXT: &str = "The daemon stopped this agent session";
const REPLACED_TEXT: &str = "replaced this agent session";

fn session_event(active: &str, generation: &str, sequence: u64, event: &Value) -> Value {
    json!({
        "type": "session_event",
        "activeSessionId": active,
        "event": event,
        "meta": {
            "sequence": sequence,
            "cursor": { "generation": generation, "sequence": sequence },
        },
    })
}

fn user_message(text: &str, timestamp: u64) -> Value {
    json!({ "role": "user", "content": text, "timestamp": timestamp })
}

/// A user row's live frames: `message_start` then `message_end`.
fn user_row_frames(active: &str, generation: &str, first: u64, text: &str) -> Vec<Value> {
    let message = user_message(text, 100 + first);
    vec![
        session_event(
            active,
            generation,
            first,
            &json!({ "type": "message_start", "message": message }),
        ),
        session_event(
            active,
            generation,
            first + 1,
            &json!({ "type": "message_end", "message": message }),
        ),
    ]
}

fn attach_response(
    id: &str,
    active: &str,
    generation: &str,
    cursor: u64,
    messages: &[Value],
) -> Value {
    attach_response_for(id, active, "sess-1", generation, cursor, messages)
}

/// An attach answer whose snapshot names transcript `session_id`.
fn attach_response_for(
    id: &str,
    active: &str,
    session_id: &str,
    generation: &str,
    cursor: u64,
    messages: &[Value],
) -> Value {
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
                    "sessionId": session_id,
                    "sessionFile": format!("/tmp/{session_id}.jsonl"),
                    "sessionName": "attach window",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": messages,
                "lastEventSequence": cursor,
                "lastEventCursor": { "generation": generation, "sequence": cursor },
            },
            "replay": { "status": "complete", "toSequence": cursor },
            "lastEventSequence": cursor,
            "lastEventCursor": { "generation": generation, "sequence": cursor },
            "client": { "id": "mock", "capabilities": [] },
        },
    })
}

fn create_response(id: &str) -> Value {
    json!({
        "type": "response", "id": id, "command": "create",
        "success": true,
        "data": {
            "activeSessionId": "s1",
            "sessionId": "sess-1",
            "sessionFile": "/tmp/sess-1.jsonl",
        },
    })
}

fn generic_response(id: &str, command: &str) -> Value {
    json!({
        "type": "response", "id": id, "command": command,
        "success": true, "data": {},
    })
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// One client command the mock daemon read.
struct Request {
    id: String,
    command_type: String,
    active: String,
    message: String,
}

impl Request {
    fn label(&self) -> String {
        if self.command_type == "prompt" {
            format!("prompt:{}:{}", self.active, self.message)
        } else {
            format!("{}:{}", self.command_type, self.active)
        }
    }
}

/// Serve one client connection: `create` answers the fresh session, every
/// command goes to `handle` first (returning `true` when it answered), and the
/// rest succeed. Returns every command's label in wire order.
fn serve(
    listener: &UnixListener,
    mut handle: impl FnMut(&Request, &mut UnixStream) -> bool,
) -> Vec<String> {
    let (stream, _) = listener.accept().expect("accept mock client");
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
    let mut commands = Vec::new();
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
        let command = &envelope["command"];
        let field = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let request = Request {
            id: field(&envelope["id"]),
            command_type: field(&command["type"]),
            active: field(&command["activeSessionId"]),
            message: field(&command["message"]),
        };
        commands.push(request.label());
        if request.command_type == "create" {
            write_json(&mut writer, &create_response(&request.id));
        } else if !handle(&request, &mut writer) {
            write_json(
                &mut writer,
                &generic_response(&request.id, &request.command_type),
            );
        }
    }
    commands
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
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

fn run_plan(socket: PathBuf, steps: Vec<HeadlessStep>) -> String {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let outcome = runtime
        .block_on(run_interactive(
            options(socket),
            UiMode::Headless(HeadlessPlan {
                steps,
                width: 100,
                height: 40,
            }),
        ))
        .expect("interactive run");
    outcome
        .frames
        .last()
        .expect("at least one rendered frame")
        .clone()
}

/// The attach's live events straddle its response: the covered ones (at or
/// below the snapshot cursor) arrive before AND after it and render once with
/// the snapshot's row; the newer ones render once.
#[test]
fn an_attach_renders_the_events_its_snapshot_covers_once() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || {
        serve(&listener, |request, writer| {
            if request.command_type != "attach" {
                return false;
            }
            // Cursor 2 covers the first prompt's frames (1, 2).
            let covered = user_row_frames(&request.active, "g1", 1, COVERED_PROMPT);
            let newer = user_row_frames(&request.active, "g1", 3, NEWER_PROMPT);
            write_json(writer, &covered[0]);
            write_json(writer, &newer[0]);
            write_json(
                writer,
                &attach_response(
                    &request.id,
                    &request.active,
                    "g1",
                    2,
                    &[user_message(COVERED_PROMPT, 101)],
                ),
            );
            // A covered frame replayed after the response, then the newer row's end.
            write_json(writer, &covered[1]);
            write_json(writer, &newer[1]);
            true
        })
    });
    let frame = run_plan(
        socket,
        vec![HeadlessStep::WaitRender {
            needle: NEWER_PROMPT.to_string(),
            timeout_ms: 5000,
        }],
    );
    handle.join().expect("mock daemon finished");
    assert_eq!(
        frame.matches(COVERED_PROMPT).count(),
        1,
        "the covered prompt renders once:\n{frame}"
    );
    assert_eq!(
        frame.matches(NEWER_PROMPT).count(),
        1,
        "the newer prompt renders once:\n{frame}"
    );
}

/// A supersede notice rebinds the view; the superseded id's late frames never
/// reach it (no stale row, no spurious replaced-close), the old id detaches only
/// after the new attach lands, and a real close of the bound session still shows.
#[test]
fn a_superseded_binding_never_reaches_the_rebound_view() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || {
        serve(&listener, |request, writer| {
            match (request.command_type.as_str(), request.active.as_str()) {
                ("attach", "s1") => {
                    write_json(
                        writer,
                        &attach_response(
                            &request.id,
                            "s1",
                            "g1",
                            2,
                            &[user_message(FIRST_ROW, 101)],
                        ),
                    );
                    write_json(
                        writer,
                        &json!({
                            "type": "session_binding",
                            "previousActiveSessionId": "s1",
                            "activeSessionId": "s2",
                        }),
                    );
                }
                ("attach", _) => {
                    write_json(
                        writer,
                        &attach_response(
                            &request.id,
                            "s2",
                            "g2",
                            4,
                            &[user_message(FIRST_ROW, 101), user_message(REBOUND_ROW, 102)],
                        ),
                    );
                    // The superseded id's late frames, then the bound session's live row.
                    for frame in user_row_frames("s1", "g1", 3, STALE_ROW) {
                        write_json(writer, &frame);
                    }
                    write_json(
                        writer,
                        &json!({ "type": "session_closed", "activeSessionId": "s1", "reason": "replaced" }),
                    );
                    for frame in user_row_frames("s2", "g2", 5, LIVE_ROW) {
                        write_json(writer, &frame);
                    }
                }
                ("detach", "s1") => {
                    // The superseded id's retirement; the bound session then really closes.
                    write_json(writer, &generic_response(&request.id, "detach"));
                    write_json(
                        writer,
                        &json!({ "type": "session_closed", "activeSessionId": "s2", "reason": "killed" }),
                    );
                }
                _ => return false,
            }
            true
        })
    });
    let frame = run_plan(
        socket,
        vec![
            HeadlessStep::WaitRender {
                needle: LIVE_ROW.to_string(),
                timeout_ms: 5000,
            },
            HeadlessStep::WaitRender {
                needle: STOPPED_TEXT.to_string(),
                timeout_ms: 5000,
            },
        ],
    );
    let commands = handle.join().expect("mock daemon finished");
    assert!(
        frame.contains(REBOUND_ROW),
        "the rebound snapshot renders:\n{frame}"
    );
    assert_eq!(
        frame.matches(LIVE_ROW).count(),
        1,
        "the live row renders once:\n{frame}"
    );
    assert!(
        !frame.contains(STALE_ROW),
        "the superseded id's late row never renders:\n{frame}"
    );
    assert!(
        !frame.contains(REPLACED_TEXT),
        "the superseded id's close is not the rebound session's close:\n{frame}"
    );
    let attach_s2 = commands.iter().position(|command| command == "attach:s2");
    let detach_s1 = commands.iter().position(|command| command == "detach:s1");
    assert!(
        matches!((attach_s2, detach_s1), (Some(attach), Some(detach)) if attach < detach),
        "the old id detaches only after the new attach lands: {commands:?}"
    );
}

const REVIVAL_PROMPT: &str = "a prompt the dead worker refused";
const FIRST_PROMPT: &str = "a prompt the moved binding refused";
const SECOND_PROMPT: &str = "the next prompt after the move";
const REFUSAL_TEXT: &str = "Unknown active session: s1";

/// An admitted prompt's turn: the user row, then the turn's end.
fn settled_turn_frames(active: &str, generation: &str, text: &str) -> Vec<Value> {
    let mut frames = user_row_frames(active, generation, 1, text);
    frames.push(session_event(
        active,
        generation,
        3,
        &json!({ "type": "turn_end" }),
    ));
    frames.push(session_event(
        active,
        generation,
        4,
        &json!({ "type": "agent_end" }),
    ));
    frames
}

fn prompt_refusal(id: &str, active: &str) -> Value {
    json!({
        "type": "response", "id": id, "command": "prompt",
        "success": false,
        "error": format!("Unknown active session: {active}"),
    })
}

/// The startup attach every revival test opens with.
fn initial_attach(request: &Request, writer: &mut UnixStream) {
    write_json(
        writer,
        &attach_response(&request.id, "s1", "g1", 0, &[user_message(FIRST_ROW, 101)]),
    );
}

/// The prompt labels the mock saw, in wire order.
fn prompts(commands: &[String]) -> Vec<&str> {
    commands
        .iter()
        .filter(|command| command.starts_with("prompt:"))
        .map(String::as_str)
        .collect()
}

/// A prompt refused because its worker is gone re-attaches by the durable
/// transcript id and replays exactly once to the worker now serving the SAME
/// transcript (the revival the TS client ran by hand).
#[test]
fn a_refused_prompt_replays_once_on_its_revived_transcript() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || {
        serve(&listener, |request, writer| {
            match (request.command_type.as_str(), request.active.as_str()) {
                ("attach", "s1") => initial_attach(request, writer),
                ("prompt", "s1") => write_json(writer, &prompt_refusal(&request.id, "s1")),
                ("attach", "sess-1") => write_json(
                    writer,
                    &attach_response_for(
                        &request.id,
                        "s3",
                        "sess-1",
                        "g3",
                        0,
                        &[user_message(FIRST_ROW, 101)],
                    ),
                ),
                ("prompt", "s3") => {
                    write_json(writer, &generic_response(&request.id, "prompt"));
                    for frame in settled_turn_frames("s3", "g3", REVIVAL_PROMPT) {
                        write_json(writer, &frame);
                    }
                }
                _ => return false,
            }
            true
        })
    });
    let frame = run_plan(
        socket,
        vec![
            HeadlessStep::Submit(REVIVAL_PROMPT.to_string()),
            HeadlessStep::WaitRender {
                needle: REVIVAL_PROMPT.to_string(),
                timeout_ms: 5000,
            },
        ],
    );
    let commands = handle.join().expect("mock daemon finished");
    assert_eq!(
        prompts(&commands),
        vec![
            format!("prompt:s1:{REVIVAL_PROMPT}"),
            format!("prompt:s3:{REVIVAL_PROMPT}"),
        ],
        "one refused attempt, one replay on the revived transcript: {commands:?}"
    );
    assert!(
        commands.iter().any(|command| command == "attach:sess-1"),
        "the replay re-attaches by the durable transcript id: {commands:?}"
    );
    assert_eq!(frame.matches(REVIVAL_PROMPT).count(), 1, "{frame}");
    assert!(
        !frame.contains(REFUSAL_TEXT),
        "the replay hides the refusal:\n{frame}"
    );
}

/// The durable re-attach lands on a worker now showing ANOTHER transcript (an
/// in-worker switch moved it): the refused prompt is never re-sent there, and
/// its original refusal surfaces (legacy `5a19fe961`, `ae2db8c32`).
#[test]
fn a_refused_prompt_never_replays_into_a_moved_transcript() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || {
        serve(&listener, |request, writer| {
            match (request.command_type.as_str(), request.active.as_str()) {
                ("attach", "s1") => initial_attach(request, writer),
                ("prompt", "s1") => write_json(writer, &prompt_refusal(&request.id, "s1")),
                ("attach", "sess-1") => write_json(
                    writer,
                    &attach_response_for(
                        &request.id,
                        "s3",
                        "sess-moved",
                        "g3",
                        0,
                        &[user_message(REBOUND_ROW, 102)],
                    ),
                ),
                // A replay would land here: admitted, so the run settles and the
                // prompt count names it.
                ("prompt", active) => {
                    write_json(writer, &generic_response(&request.id, "prompt"));
                    for frame in settled_turn_frames(active, "g3", &request.message) {
                        write_json(writer, &frame);
                    }
                }
                _ => return false,
            }
            true
        })
    });
    let frame = run_plan(
        socket,
        vec![
            HeadlessStep::Submit(REVIVAL_PROMPT.to_string()),
            HeadlessStep::WaitRender {
                needle: REFUSAL_TEXT.to_string(),
                timeout_ms: 5000,
            },
        ],
    );
    let commands = handle.join().expect("mock daemon finished");
    assert_eq!(
        prompts(&commands),
        vec![format!("prompt:s1:{REVIVAL_PROMPT}")],
        "the prompt is never re-sent into the moved transcript: {commands:?}"
    );
    assert!(
        frame.contains(REFUSAL_TEXT),
        "the original refusal surfaces:\n{frame}"
    );
}

/// The binding moves (a supersede notice) while a prompt's refusal is in
/// flight: the refusal surfaces instead of a replay, and the NEXT prompt follows
/// the live binding (legacy `966c11aa7` late rebind, `324f9b637`).
#[test]
fn a_prompt_refused_after_its_binding_moved_is_not_replayed() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || {
        let mut held_prompt: Option<String> = None;
        serve(&listener, move |request, writer| {
            match (request.command_type.as_str(), request.active.as_str()) {
                ("attach", "s1") => initial_attach(request, writer),
                ("prompt", "s1") => {
                    // The refusal waits until the client has rebound to s2.
                    held_prompt = Some(request.id.clone());
                    write_json(
                        writer,
                        &json!({
                            "type": "session_binding",
                            "previousActiveSessionId": "s1",
                            "activeSessionId": "s2",
                        }),
                    );
                }
                ("attach", "s2") => {
                    write_json(
                        writer,
                        &attach_response(
                            &request.id,
                            "s2",
                            "g2",
                            0,
                            &[user_message(FIRST_ROW, 101)],
                        ),
                    );
                    if let Some(id) = held_prompt.take() {
                        write_json(writer, &prompt_refusal(&id, "s1"));
                    }
                }
                ("prompt", "s2") => {
                    write_json(writer, &generic_response(&request.id, "prompt"));
                    for frame in settled_turn_frames("s2", "g2", SECOND_PROMPT) {
                        write_json(writer, &frame);
                    }
                }
                _ => return false,
            }
            true
        })
    });
    let frame = run_plan(
        socket,
        vec![
            HeadlessStep::Submit(FIRST_PROMPT.to_string()),
            HeadlessStep::WaitRender {
                needle: REFUSAL_TEXT.to_string(),
                timeout_ms: 5000,
            },
            HeadlessStep::Submit(SECOND_PROMPT.to_string()),
            HeadlessStep::WaitRender {
                needle: SECOND_PROMPT.to_string(),
                timeout_ms: 5000,
            },
        ],
    );
    let commands = handle.join().expect("mock daemon finished");
    assert_eq!(
        prompts(&commands),
        vec![
            format!("prompt:s1:{FIRST_PROMPT}"),
            format!("prompt:s2:{SECOND_PROMPT}"),
        ],
        "the refused prompt is not replayed; the next one follows the binding: {commands:?}"
    );
    assert!(
        frame.contains(REFUSAL_TEXT),
        "the refusal surfaces:\n{frame}"
    );
}
