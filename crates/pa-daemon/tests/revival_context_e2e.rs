//! Revival launch-context e2e: a session the daemon brings back after its
//! worker went away — the idle-passivation wake on a client prompt or
//! attach, or the supervisor's crash respawn — runs with the launch context
//! it was opened with, never with daemon defaults or a stale transcript's
//! directory. Legacy TS parity rows, one invariant per test:
//!
//! - b2bb37bb2 / 48fae2a5a / 843615757 / 837be6824 (+ 502e7a496): a
//!   transcript opened with an explicit fallback cwd (its recorded
//!   directory is missing) revives in that fallback instead of dead-ending,
//!   also across a supervisor restart;
//! - e32bce384 (+ 837be6824): the opened-with runtime config (system prompt
//!   append, telemetry opt-out) survives the wake, so an opted-out client
//!   can still attach and the revived worker never reports telemetry; the
//!   revived session reports for the waking client's pane, never the
//!   creator's;
//! - e32bce384 / 48fae2a5a through the update restore: a roster row is
//!   recreated from its captured launch context, not the transcript header;
//!   the session's telemetry opt-out survives the restore;
//! - c3adf81be / 1ca735370 / 48fae2a5a: a transcript switched in over a
//!   fileless or foreign launch never inherits the launch cwd on respawn;
//! - 6470a60e7: a cancellable prompt never starts a revival;
//! - d5bc1eedc: a revival never detaches a sibling window and leaves no
//!   stale attachment pinning the session.
// Stack-resident futures by design on the daemon's hot paths.
#![allow(clippy::large_futures)]
// Narrowing casts sit at OS boundaries (pid/fd/time/size) where the kernel
// bounds the values.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Style gate only, not correctness: each scenario is one linear script.
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A loopback OpenAI-compatible provider answering every turn with `answer`.
fn spawn_mock(answer: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let _ = serve(stream, answer);
            });
        }
    });
    url
}

fn chunk(delta: &Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-revival",
        "object": "chat.completion.chunk",
        "created": 1_750_000_000,
        "model": "mock-1",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
    .to_string()
}

fn serve(mut stream: TcpStream, answer: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or_default();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let mut payload = String::new();
    for data in [
        chunk(&json!({"role": "assistant", "content": answer}), None),
        chunk(&json!({}), Some("stop")),
    ] {
        write!(payload, "data: {data}\n\n").expect("write to String");
    }
    payload.push_str("data: [DONE]\n\n");
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{payload}"
        )
        .as_bytes(),
    )
}

/// The isolated agent dir: the loopback provider and the idle-eviction
/// threshold the supervisor's passivation fence re-reads.
fn agent_dir_with_mock(root: &Path) -> PathBuf {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let url = spawn_mock("revived reply");
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "prime-inference": {
                    "api": "openai-completions",
                    "baseUrl": url,
                    "apiKey": "sk-revival",
                    "models": [{
                        "id": "mock-1",
                        "name": "Mock 1",
                        "api": "openai-completions",
                        "contextWindow": 128_000,
                        "maxTokens": 4096
                    }]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "idleEvictionMinutes": 1 }).to_string(),
    )
    .expect("write settings.json");
    agent_dir
}

#[allow(clippy::zombie_processes)] // the timeout path exits the test process
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    spawn_daemon_with_env(socket, agent_dir, &[])
}

#[allow(clippy::zombie_processes)] // the timeout path exits the test process
fn spawn_daemon_with_env(socket: &Path, agent_dir: &Path, env: &[(&str, &Path)]) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        // The test owns its agent dir: an ambient session-dir override would
        // point the saved-session catalog the wake resolves through elsewhere.
        .env_remove("PRIME_AGENT_SESSION_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A supervisor killed at teardown must not leak its session workers
        // into later test binaries.
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .envs(env.iter().copied())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Daemon { child };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello", "{hello}");
        client
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        self.send_command(id, command);
        loop {
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) && line["type"] == "response" {
                return line;
            }
        }
    }

    /// The worker's real working directory, through the user-bash surface.
    fn pwd(&mut self, id: &str, active_session_id: &str) -> Value {
        self.request(
            id,
            &json!({
                "type": "execute_bash_and_wait",
                "activeSessionId": active_session_id,
                "command": "pwd -P",
            }),
        )
    }

    /// The live row hosting `session_id`, once one is registered.
    fn live_active_id(&mut self, id: &str, session_id: &str) -> Option<String> {
        let list = self.request(id, &json!({ "type": "list" }));
        list["data"]["sessions"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["sessionId"].as_str() == Some(session_id))
                .and_then(|row| {
                    row["activeSessionId"]
                        .as_str()
                        .or_else(|| row["id"].as_str())
                })
                .map(str::to_string)
        })
    }
}

fn canonical(path: &Path) -> String {
    path.canonicalize()
        .expect("canonical dir")
        .to_string_lossy()
        .to_string()
}

fn pwd_output(response: &Value) -> String {
    assert_eq!(response["success"], true, "pwd failed: {response}");
    response["data"]["output"]
        .as_str()
        .expect("bash output")
        .trim()
        .to_string()
}

/// One persisted worker descriptor field (token or pid) by active id.
fn descriptor_field(agent_dir: &Path, active_session_id: &str, field: &str) -> Option<Value> {
    let instances = std::fs::read_dir(agent_dir.join("daemon-workers")).ok()?;
    for instance in instances.flatten() {
        let path = instance.path().join(format!("{active_session_id}.json"));
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(descriptor) = serde_json::from_str::<Value>(&content) {
            if let Some(value) = descriptor.get(field) {
                return Some(value.clone());
            }
        }
    }
    None
}

/// The product's idle passivation ask (the worker-side clock and gates are
/// unit-covered): the worker's own token drives the supervisor's graceful
/// stop, leaving only the saved file and the passive roster row.
fn passivate(client: &mut Client, agent_dir: &Path, active_session_id: &str, id: &str) {
    let token = descriptor_field(agent_dir, active_session_id, "authenticationToken")
        .and_then(|token| token.as_str().map(str::to_string))
        .expect("the worker's token");
    let response = client.request(
        id,
        &json!({
            "type": "worker_idle_passivation",
            "workerToken": token,
            "idleMinutes": 1,
        }),
    );
    assert_eq!(response["success"], true, "passivation failed: {response}");
    let list = client.request(&format!("{id}-list"), &json!({ "type": "list" }));
    assert_eq!(
        list["data"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "the passivated worker left the live list: {list}"
    );
}

fn wait_until<T>(what: &str, budget: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "never observed: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A saved transcript whose recorded directory no longer exists.
fn write_missing_cwd_transcript(sessions: &Path, gone: &Path) -> PathBuf {
    let saved = sessions.join("missing-cwd.jsonl");
    std::fs::write(
        &saved,
        format!(
            "{}\n",
            json!({
                "type": "session", "version": 3, "id": "missing-cwd-session",
                "timestamp": "2026-10-12T00:00:00.000Z", "cwd": gone,
            })
        ),
    )
    .expect("write saved transcript");
    saved
}

/// Rows b2bb37bb2 / 48fae2a5a / 843615757 / 837be6824 (+ 502e7a496): a
/// transcript that only opened because the user chose a fallback cwd for its
/// missing recorded directory keeps that fallback when the daemon revives
/// it — through the stale-id prompt (the attached window's next keystroke)
/// and through the attach-by-durable-id rebind — instead of dead-ending on
/// the missing directory.
#[test]
fn a_fallback_cwd_session_revives_in_its_fallback_after_idle_passivation() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let fallback = dir.path().join("fallback");
    std::fs::create_dir_all(&fallback).expect("fallback dir");
    let saved = write_missing_cwd_transcript(&sessions, &dir.path().join("gone"));
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);

    let created = client.request(
        "open",
        &json!({
            "type": "create",
            "sessionPath": saved,
            "config": {
                "cwd": fallback,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "fallback open failed: {created}");
    let active = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let first = client.request(
        "turn-1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "first" }),
    );
    assert_eq!(first["success"], true, "first turn failed: {first}");
    assert_eq!(
        pwd_output(&client.pwd("pwd-1", &active)),
        canonical(&fallback)
    );

    // The window's next prompt targets the id it holds. The passivated
    // worker's 12-hex id is not durable, so the daemon answers the exact
    // unknown-session refusal the TUI's one-shot rebind keys on
    // (`session_ui/prompt.rs`): the window re-attaches by the DURABLE
    // session id and replays the prompt once.
    passivate(&mut client, &agent_dir, &active, "passivate-1");
    let stale = client.request(
        "turn-stale",
        &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "after idle" }),
    );
    assert_eq!(
        stale["error"],
        format!("Unknown active session: {active}"),
        "the stale id answers the rebind trigger: {stale}"
    );
    for round in 0..2 {
        let attached = client.request(
            &format!("attach-durable-{round}"),
            &json!({ "type": "attach", "activeSessionId": session_id }),
        );
        assert_eq!(
            attached["success"], true,
            "the durable-id attach must revive the session instead of dead-ending: {attached}"
        );
        let woken = client
            .live_active_id(&format!("list-{round}"), &session_id)
            .expect("the revived session is live");
        let replayed = client.request(
            &format!("turn-replay-{round}"),
            &json!({ "type": "prompt_and_wait", "activeSessionId": woken, "message": "after idle" }),
        );
        assert_eq!(
            replayed["success"], true,
            "the replayed prompt runs: {replayed}"
        );
        assert_eq!(
            pwd_output(&client.pwd(&format!("pwd-revived-{round}"), &woken)),
            canonical(&fallback),
            "the revival must reuse the fallback the session was opened with"
        );
        // A second idle cycle: the revived worker's own record keeps the fallback.
        client.request(
            &format!("detach-{round}"),
            &json!({ "type": "detach", "activeSessionId": woken }),
        );
        passivate(
            &mut client,
            &agent_dir,
            &woken,
            &format!("passivate-again-{round}"),
        );
    }
}

/// Row e32bce384 (+ 837be6824): the revival recreates the session with the
/// runtime config it was opened with, not daemon defaults — the opened-with
/// system-prompt append still shapes the prompt, and the telemetry opt-out
/// survives, so the opted-out client's re-attach is admitted.
#[test]
fn a_passivated_session_revives_with_its_opened_runtime_config() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let marker = "REVIVAL_LAUNCH_CONTEXT_MARKER";

    let created = client.request(
        "open",
        &json!({
            "type": "create",
            "telemetryDisabled": true,
            "config": {
                "cwd": project,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
                "appendSystemPrompt": [marker],
            },
        }),
    );
    assert_eq!(created["success"], true, "open failed: {created}");
    let active = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let first = client.request(
        "turn-1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "first" }),
    );
    assert_eq!(first["success"], true, "first turn failed: {first}");
    let prompt = client.request(
        "system-1",
        &json!({ "type": "get_system_prompt", "activeSessionId": active }),
    );
    assert!(
        prompt["data"]["systemPrompt"]
            .as_str()
            .is_some_and(|text| text.contains(marker)),
        "the probe sees the opened-with append: {prompt}"
    );

    passivate(&mut client, &agent_dir, &active, "passivate");
    // The opted-out window's rebind attach by the durable id (the TUI sends
    // its opt-out on every attach): the woken worker must carry the same
    // opt-out, or the daemon refuses the attach after launching a worker
    // that reports telemetry the user refused.
    let attached = client.request(
        "attach",
        &json!({ "type": "attach", "activeSessionId": session_id, "telemetryDisabled": true }),
    );
    assert_eq!(
        attached["success"], true,
        "the telemetry opt-out must survive the revival: {attached}"
    );
    let woken = client
        .live_active_id("list", &session_id)
        .expect("the revived session is live");
    let prompt = client.request(
        "system-2",
        &json!({ "type": "get_system_prompt", "activeSessionId": woken }),
    );
    assert!(
        prompt["data"]["systemPrompt"]
            .as_str()
            .is_some_and(|text| text.contains(marker)),
        "the revival must keep the opened-with system prompt append: {prompt}"
    );
}

/// Rows c3adf81be / 1ca735370 / 48fae2a5a: a transcript switched into a
/// worker whose launch was fileless (`noSession`) or another transcript is
/// respawned in ITS OWN directory — the recorded cwd, or the fallback the
/// switch selected for a missing one — never the launch cwd.
#[test]
fn a_switched_transcript_respawns_in_its_own_cwd_not_the_launch_cwd() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let launch = dir.path().join("launch");
    let recorded = dir.path().join("recorded");
    let fallback = dir.path().join("fallback");
    for path in [&launch, &recorded, &fallback] {
        std::fs::create_dir_all(path).expect("dir");
    }
    let intact = sessions.join("intact-cwd.jsonl");
    std::fs::write(
        &intact,
        format!(
            "{}\n",
            json!({
                "type": "session", "version": 3, "id": "intact-cwd-session",
                "timestamp": "2026-10-12T00:00:00.000Z", "cwd": recorded,
            })
        ),
    )
    .expect("write intact transcript");
    let missing = write_missing_cwd_transcript(&sessions, &dir.path().join("gone"));
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);

    for (index, (transcript, cwd_override, expected)) in [
        (&intact, None, &recorded),
        (&missing, Some(&fallback), &fallback),
    ]
    .into_iter()
    .enumerate()
    {
        // A fileless (`--no-session`) launch in the launch directory.
        let created = client.request(
            &format!("open-{index}"),
            &json!({
                "type": "create",
                "noSession": true,
                "config": {
                    "cwd": launch,
                    "sessionDir": sessions,
                    "provider": "prime-inference",
                    "model": "mock-1",
                },
            }),
        );
        assert_eq!(created["success"], true, "fileless open failed: {created}");
        let active = created["data"]["id"]
            .as_str()
            .expect("active id")
            .to_string();
        let mut switch = json!({
            "type": "switch_session",
            "activeSessionId": active,
            "sessionPath": transcript,
        });
        if let Some(cwd) = cwd_override {
            switch["cwdOverride"] = json!(cwd);
        }
        let switched = client.request(&format!("switch-{index}"), &switch);
        assert_eq!(switched["success"], true, "switch failed: {switched}");
        assert_eq!(
            pwd_output(&client.pwd(&format!("pwd-before-{index}"), &active)),
            canonical(expected)
        );
        // The roster follow moves the durable record onto the switched-in
        // transcript before the crash.
        wait_until(
            "the durable record follows the switched-in transcript",
            Duration::from_secs(15),
            || {
                descriptor_field(&agent_dir, &active, "sessionFile")
                    .and_then(|file| file.as_str().map(str::to_string))
                    .filter(|file| Path::new(file) == transcript.as_path())
            },
        );
        let pid = descriptor_field(&agent_dir, &active, "pid")
            .and_then(|pid| pid.as_u64())
            .expect("the worker pid");
        let killed = Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .expect("run kill");
        assert!(killed.success(), "kill -9 {pid} failed");
        // The supervisor's crash respawn replays the durable create; the
        // respawned worker answers on the same active id.
        let respawned = wait_until(
            "the respawned worker answers on the same id",
            Duration::from_secs(60),
            || {
                let response = client.pwd(&format!("pwd-after-{index}"), &active);
                (response["success"] == true
                    && descriptor_field(&agent_dir, &active, "pid").and_then(|p| p.as_u64())
                        != Some(pid))
                .then_some(response)
            },
        );
        assert_eq!(
            pwd_output(&respawned),
            canonical(expected),
            "the respawn must run the switched-in transcript in its own directory"
        );
        let killed = client.request(
            &format!("kill-{index}"),
            &json!({ "type": "kill", "activeSessionId": active }),
        );
        assert_eq!(killed["success"], true, "{killed}");
    }
}

/// Row 6470a60e7: a cancelled prompt never revives a passivated session. A
/// cancellable prompt carries an admission id; the daemon's admission route
/// resolves only live residents and stale-id bindings, so it refuses with the
/// unknown-session error instead of launching a worker, and the cancellation
/// that follows finds nothing to race. The plain durable-id attach (the TUI
/// rebind) is the control: the same session is revivable.
#[test]
fn a_cancellable_prompt_never_revives_a_passivated_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let created = client.request(
        "open",
        &json!({
            "type": "create",
            "config": {
                "cwd": project,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "open failed: {created}");
    let active = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let first = client.request(
        "turn-1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "first" }),
    );
    assert_eq!(first["success"], true, "first turn failed: {first}");
    passivate(&mut client, &agent_dir, &active, "passivate");

    for selector in [active.as_str(), session_id.as_str()] {
        let admitted = client.request(
            &format!("admitted-{selector}"),
            &json!({
                "type": "prompt",
                "activeSessionId": selector,
                "message": "cancel me",
                "admissionId": format!("admission-{selector}"),
            }),
        );
        assert_eq!(
            admitted["error"],
            format!("Unknown active session: {selector}"),
            "the cancellable prompt is refused, not revived: {admitted}"
        );
        let cancelled = client.request(
            &format!("cancel-{selector}"),
            &json!({
                "type": "cancel_prompt_admission",
                "activeSessionId": selector,
                "admissionId": format!("admission-{selector}"),
            }),
        );
        assert_eq!(cancelled["success"], true, "{cancelled}");
        assert_eq!(
            client.live_active_id(&format!("list-{selector}"), &session_id),
            None,
            "no worker was launched for the cancellable prompt"
        );
    }
    // The control: the plain durable-id attach revives the same session.
    let attached = client.request(
        "attach-durable",
        &json!({ "type": "attach", "activeSessionId": session_id }),
    );
    assert_eq!(attached["success"], true, "{attached}");
    assert!(client.live_active_id("list-after", &session_id).is_some());
}

/// Row d5bc1eedc's invariants on the Rust attach model (the legacy-daemon
/// ownership negotiation itself has no Rust peer: a Rust client only talks
/// to a Rust daemon, whose attachments are per connection): a revival never
/// detaches a SIBLING window attached to the same session — not the
/// reviving window's detach of its superseded id, not its own detach — and
/// once the last window leaves, no attachment lingers, so the session stays
/// eligible for the next idle passivation.
#[test]
fn a_revival_keeps_a_sibling_attached_and_leaves_no_stale_attachment() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut driver = Client::connect(&socket);
    let created = driver.request(
        "open",
        &json!({
            "type": "create",
            "config": {
                "cwd": project,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "open failed: {created}");
    let stale = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let first = driver.request(
        "turn-1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": stale, "message": "first" }),
    );
    assert_eq!(first["success"], true, "first turn failed: {first}");
    passivate(&mut driver, &agent_dir, &stale, "passivate");

    let attached_clients = |driver: &mut Client, id: &str| -> u64 {
        let list = driver.request(id, &json!({ "type": "list" }));
        list["data"]["sessions"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["sessionId"].as_str() == Some(session_id.as_str()))
                    .and_then(|row| row["attachedClients"].as_u64())
            })
            .expect("the revived session's live row")
    };
    // The reviving window and a sibling window both rebind by the durable id.
    let mut reviver = Client::connect(&socket);
    let mut sibling = Client::connect(&socket);
    for (client, id) in [
        (&mut reviver, "attach-reviver"),
        (&mut sibling, "attach-sibling"),
    ] {
        let attached = client.request(
            id,
            &json!({ "type": "attach", "activeSessionId": session_id }),
        );
        assert_eq!(attached["success"], true, "{attached}");
    }
    let woken = driver
        .live_active_id("list-woken", &session_id)
        .expect("the revived session is live");
    assert_eq!(attached_clients(&mut driver, "count-2"), 2);

    // The reviving window retires its superseded id, then leaves.
    for (id, target) in [("detach-stale", &stale), ("detach-woken", &woken)] {
        let detached = reviver.request(id, &json!({ "type": "detach", "activeSessionId": target }));
        assert_eq!(detached["success"], true, "{detached}");
    }
    assert_eq!(
        attached_clients(&mut driver, "count-1"),
        1,
        "the sibling's attachment survives the reviver's cleanup"
    );
    let turn = driver.request(
        "turn-2",
        &json!({ "type": "prompt_and_wait", "activeSessionId": woken, "message": "second" }),
    );
    assert_eq!(turn["success"], true, "{turn}");
    let event = wait_until(
        "the sibling receives the revived session's events",
        Duration::from_secs(30),
        || {
            let line = sibling.read_line();
            (line["type"] == "session_event" && line["activeSessionId"] == woken.as_str())
                .then_some(line)
        },
    );
    assert_eq!(event["activeSessionId"], woken.as_str());

    let detached = sibling.request(
        "detach-sibling",
        &json!({ "type": "detach", "activeSessionId": woken }),
    );
    assert_eq!(detached["success"], true, "{detached}");
    assert_eq!(
        attached_clients(&mut driver, "count-0"),
        0,
        "no stale attachment pins the session against idle passivation"
    );
}

/// Rows b2bb37bb2 / 48fae2a5a / 843615757 / 837be6824 / e32bce384 across a
/// supervisor restart: the TS window held its revive config in the client
/// process, so a daemon restart never lost it. The passivated session's
/// launch context is durable here too: after the supervisor restarts, the
/// durable-id rebind attach still revives the session in its fallback cwd
/// and under the opted-out telemetry.
#[test]
fn a_passivated_launch_context_survives_a_supervisor_restart() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let fallback = dir.path().join("fallback");
    std::fs::create_dir_all(&fallback).expect("fallback dir");
    let saved = write_missing_cwd_transcript(&sessions, &dir.path().join("gone"));
    let socket = dir.path().join("daemon.sock");
    let daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let created = client.request(
        "open",
        &json!({
            "type": "create",
            "sessionPath": saved,
            "telemetryDisabled": true,
            "config": {
                "cwd": fallback,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "fallback open failed: {created}");
    let active = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let first = client.request(
        "turn-1",
        &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "first" }),
    );
    assert_eq!(first["success"], true, "first turn failed: {first}");
    passivate(&mut client, &agent_dir, &active, "passivate");

    // The supervisor restarts (an update, a crash): nothing in memory survives.
    drop(client);
    drop(daemon);
    let _ = std::fs::remove_file(&socket);
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let attached = client.request(
        "attach-durable",
        &json!({ "type": "attach", "activeSessionId": session_id, "telemetryDisabled": true }),
    );
    assert_eq!(
        attached["success"], true,
        "the restarted daemon must revive with the opened-with telemetry opt-out: {attached}"
    );
    let woken = client
        .live_active_id("list", &session_id)
        .expect("the revived session is live");
    assert_eq!(
        pwd_output(&client.pwd("pwd", &woken)),
        canonical(&fallback),
        "the restarted daemon must revive in the fallback the session was opened with"
    );
}

/// The fake Herdr pane-state socket: one JSON request per connection, every
/// request recorded in arrival order (the connector's wire contract).
fn fake_herdr(socket: &Path) -> Arc<Mutex<Vec<Value>>> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(socket).expect("bind the fake herdr socket");
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let Ok(mut writer) = stream.try_clone() else {
                continue;
            };
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            if let Ok(request) = serde_json::from_str::<Value>(line.trim()) {
                seen.lock().unwrap().push(request.clone());
                let response = json!({ "id": request.get("id").cloned().unwrap_or(Value::Null), "result": { "type": "ok" } });
                let _ = writer.write_all(format!("{response}\n").as_bytes());
                let _ = writer.flush();
            }
        }
    });
    requests
}

/// One pane identity as a client sends it (the connector's allowlist).
fn pane_env(herdr_socket: &Path, pane_id: &str) -> Value {
    json!({
        "HERDR_ENV": "1",
        "HERDR_SOCKET_PATH": herdr_socket.to_string_lossy(),
        "HERDR_PANE_ID": pane_id,
        "HERDR_TAB_ID": "t1",
        "HERDR_WORKSPACE_ID": "ws1",
    })
}

/// Row e32bce384's client-env half: the TS revival sent the REVIVING
/// client's own pane identity (`collectDaemonClientEnv`), never the
/// creator's. Passivation requires zero attached clients, so the creator's
/// pane may be closed or reused by another program: the wake must not
/// report into it. The rebind attach's pane identity is the one the revived
/// session adopts and reports for.
#[test]
fn a_revival_reports_for_the_waking_pane_never_the_creators() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let herdr_socket = dir.path().join("herdr.sock");
    let requests = fake_herdr(&herdr_socket);
    let socket = dir.path().join("daemon.sock");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let created = client.request(
        "open",
        &json!({
            "type": "create",
            "env": pane_env(&herdr_socket, "pane:creator"),
            "config": {
                "cwd": project,
                "sessionDir": sessions,
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "open failed: {created}");
    let active = created["data"]["id"]
        .as_str()
        .expect("active id")
        .to_string();
    let session_id = created["data"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    // The probe sees the creator's pane while the creator's window owns it.
    wait_until("the creator pane report", Duration::from_secs(20), || {
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["params"]["pane_id"] == "pane:creator")
            .then_some(())
    });
    passivate(&mut client, &agent_dir, &active, "passivate");
    let before_wake = requests.lock().unwrap().len();

    let attached = client.request(
        "attach-durable",
        &json!({
            "type": "attach",
            "activeSessionId": session_id,
            "env": pane_env(&herdr_socket, "pane:waker"),
        }),
    );
    assert_eq!(attached["success"], true, "{attached}");
    let after_wake = wait_until(
        "a report for the waking pane",
        Duration::from_secs(20),
        || {
            let frames = requests.lock().unwrap()[before_wake..].to_vec();
            frames
                .iter()
                .any(|request| request["params"]["pane_id"] == "pane:waker")
                .then_some(frames)
        },
    );
    let panes: Vec<&Value> = after_wake
        .iter()
        .map(|request| &request["params"]["pane_id"])
        .collect();
    assert!(
        panes.iter().all(|pane| *pane == "pane:waker"),
        "the revival reported into the creator's stale pane: {after_wake:?}"
    );
}

/// Rows e32bce384 / 48fae2a5a through the update restore: a roster row whose
/// session no worker came back for is recreated from the durable create
/// command the roster captured — its explicit cwd and runtime config — not
/// from the transcript header (TS restores with its durable `createCommand`
/// config).
#[test]
fn an_update_restore_recreates_a_session_with_its_captured_launch_context() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let fallback = dir.path().join("fallback");
    std::fs::create_dir_all(&fallback).expect("fallback dir");
    let saved = write_missing_cwd_transcript(&sessions, &dir.path().join("gone"));
    let socket = dir.path().join("daemon.sock");
    // The captured row: the Rust roster stores the worker's durable create
    // command (flat runtime keys) under `runtime_config.create`.
    let roster = dir.path().join("update-roster.json");
    std::fs::write(
        &roster,
        json!({
            "format_version": 1,
            "update_id": "u-revival-1",
            "socket_path": socket.to_string_lossy(),
            "created_at": "2026-10-12T00:00:00Z",
            "supervisor": { "pid": 1, "process_start_id": "p", "generation": "g" },
            "binary": { "from_version": "0.1", "to_version": "0.2" },
            "sessions": [{
                "session_id": "missing-cwd-session",
                "active_session_id": "gone-worker",
                "session_file": saved.to_string_lossy(),
                "kind": "top-level",
                "rlm_depth": 0,
                "cwd": fallback.to_string_lossy(),
                "runtime_config": {
                    "create": {
                        "sessionPath": saved.to_string_lossy(),
                        "cwd": fallback.to_string_lossy(),
                        "sessionDir": sessions.to_string_lossy(),
                        "provider": "prime-inference",
                        "model": "mock-1",
                    },
                },
                "queue": { "next_turn": [], "actions": {} },
                "in_flight": {
                    "streaming": false, "compacting": false, "bash_running": false,
                    "rlm_children": false, "retrying": false, "prompt_in_flight": false
                },
                "should_resume": false
            }],
        })
        .to_string(),
    )
    .expect("write roster");
    let _daemon = spawn_daemon_with_env(
        &socket,
        &agent_dir,
        &[(
            pa_types::daemon::update_flow::UPDATE_ROSTER_ENV,
            roster.as_path(),
        )],
    );
    let mut client = Client::connect(&socket);
    let restored = wait_until(
        "the restored session is live",
        Duration::from_secs(30),
        || client.live_active_id("list", "missing-cwd-session"),
    );
    assert_eq!(
        pwd_output(&client.pwd("pwd", &restored)),
        canonical(&fallback),
        "the restore must run in the captured explicit cwd"
    );
}

/// Row e32bce384's telemetry half through the update restore: a telemetry
/// opt-out is a privacy setting, so a restored session keeps it — from the
/// row's own `telemetryDisabled` (written by current daemons), from a TS
/// row's `config.telemetryDisabled` (TS keeps it in the runtime config), or,
/// for a row written before the field existed, from the session's surviving
/// worker descriptor. Without evidence the daemon default applies (the
/// control row: the opted-out attach is refused there).
#[test]
fn an_update_restore_keeps_a_sessions_telemetry_opt_out() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = agent_dir_with_mock(dir.path());
    let sessions = agent_dir.join("sessions");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let socket = dir.path().join("daemon.sock");
    let transcript = |id: &str| {
        let path = sessions.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({
                    "type": "session", "version": 3, "id": id,
                    "timestamp": "2026-10-12T00:00:00.000Z", "cwd": project,
                })
            ),
        )
        .expect("write transcript");
        path
    };
    let flat_create = |path: &Path| {
        json!({
            "sessionPath": path, "cwd": project, "sessionDir": sessions,
            "provider": "prime-inference", "model": "mock-1",
        })
    };
    let row = |id: &str, worker: &str, runtime_config: Value| {
        json!({
            "session_id": id,
            "active_session_id": worker,
            "session_file": sessions.join(format!("{id}.jsonl")),
            "kind": "top-level",
            "rlm_depth": 0,
            "cwd": project,
            "runtime_config": runtime_config,
            "queue": { "next_turn": [], "actions": {} },
            "in_flight": {
                "streaming": false, "compacting": false, "bash_running": false,
                "rlm_children": false, "retrying": false, "prompt_in_flight": false
            },
            "should_resume": false
        })
    };
    let explicit = transcript("explicit-opt-out");
    let ts_shaped = transcript("ts-config-opt-out");
    let legacy = transcript("descriptor-opt-out");
    let control = transcript("no-evidence");
    // The pre-field row's surviving descriptor (a dead, unkept worker the
    // update boot leaves on disk) is its only opt-out evidence.
    let descriptor_dir = pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket);
    std::fs::create_dir_all(&descriptor_dir).expect("descriptor dir");
    std::fs::write(
        descriptor_dir.join("legacy-worker.json"),
        json!({
            "version": 2,
            "workerId": "legacy-worker",
            "pid": 999_999,
            "socketPath": dir.path().join("legacy-worker.sock"),
            "recoveryJournalPath": dir.path().join("legacy-worker.recovery.jsonl"),
            "supervisorSocketPath": socket,
            "authenticationToken": "legacy-token",
            "rootActiveSessionId": "legacy-worker",
            "rootSessionId": "descriptor-opt-out",
            "sessionFile": legacy,
            "telemetryDisabled": true,
            "createdAt": "2026-10-12T00:00:00.000Z",
            "updatedAt": "2026-10-12T00:00:00.000Z",
            "lifecycle": "ready",
            "createCommand": flat_create(&legacy),
            "consecutiveFailures": 0,
        })
        .to_string(),
    )
    .expect("write the legacy descriptor");
    let roster = dir.path().join("update-roster.json");
    std::fs::write(
        &roster,
        json!({
            "format_version": 1,
            "update_id": "u-telemetry-1",
            "socket_path": socket,
            "created_at": "2026-10-12T00:00:00Z",
            "supervisor": { "pid": 1, "process_start_id": "p", "generation": "g" },
            "binary": { "from_version": "0.1", "to_version": "0.2" },
            "sessions": [
                row("explicit-opt-out", "explicit-worker",
                    json!({ "create": flat_create(&explicit), "telemetryDisabled": true })),
                row("ts-config-opt-out", "ts-worker", json!({ "create": {
                    "sessionPath": ts_shaped,
                    "config": {
                        "cwd": project, "sessionDir": sessions, "telemetryDisabled": true,
                        "provider": "prime-inference", "model": "mock-1",
                    },
                }})),
                row("descriptor-opt-out", "legacy-worker", json!({ "create": flat_create(&legacy) })),
                row("no-evidence", "control-worker", json!({ "create": flat_create(&control) })),
            ],
        })
        .to_string(),
    )
    .expect("write roster");
    let _daemon = spawn_daemon_with_env(
        &socket,
        &agent_dir,
        &[(
            pa_types::daemon::update_flow::UPDATE_ROSTER_ENV,
            roster.as_path(),
        )],
    );
    let mut client = Client::connect(&socket);
    for session_id in [
        "explicit-opt-out",
        "ts-config-opt-out",
        "descriptor-opt-out",
        "no-evidence",
    ] {
        wait_until(
            "the restored session is live",
            Duration::from_secs(30),
            || client.live_active_id(&format!("list-{session_id}"), session_id),
        );
        let attached = client.request(
            &format!("attach-{session_id}"),
            &json!({ "type": "attach", "activeSessionId": session_id, "telemetryDisabled": true }),
        );
        if session_id == "no-evidence" {
            assert_eq!(
                attached["success"], false,
                "without evidence the daemon default applies: {attached}"
            );
        } else {
            assert_eq!(
                attached["success"], true,
                "the restore must keep {session_id}'s telemetry opt-out: {attached}"
            );
        }
    }
}
