//! Control-plane priority e2e (design R3): a supervisor booting over a sessions
//! dir full of dead workers must keep hello/list responsive while the
//! background recovery (capped relaunch fan-out) runs, and every queued
//! recovery must complete. `AF_UNIX` sockets and `kill -9` are portable
//! POSIX surfaces, not actually Linux-specific; the only platform
//! dependency this file ever had was its worker-discovery helper, which
//! read `/proc` directly. That helper now reads each session's own
//! durable worker descriptor instead, so `#![cfg(unix)]` is accurate:
//! this suite runs on macOS too, not just Linux.
#![allow(clippy::large_futures)]
// 64-bit-only targets; the narrowing casts sit at bounded OS boundaries.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
// API-shape opinions, not defects; the surfaces are deliberate.
#![allow(
    clippy::unnecessary_wraps,
    clippy::zero_sized_map_values,
    clippy::struct_excessive_bools,
    clippy::struct_field_names
)]
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let child = Command::new(binary)
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A supervisor killed at teardown must not leak its session workers: the
        // supervisor-lost exit runs on this short window, not the 5-minute default.
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon { child }
}

/// Wait until the socket accepts connections (file existence is not readiness:
/// a restarted supervisor replaces a stale socket file).
fn wait_socket_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The worker pid for one session, read from its durable descriptor
/// (`agent_dir/daemon-workers/<socket-key>/<session-id>.json`, same
/// lookup `idle_passivation_e2e.rs::worker_pid` already uses). Portable,
/// and a more precise match to what each call site below actually needs
/// to know (THIS fixture's own sessions, not a census of whatever
/// children the supervisor's pid happens to have) - unlike the old
/// `/proc`-wide `child_pids_of()`, which also hard-panicked on macOS
/// (`read_dir("/proc")`).
fn worker_pid(agent_dir: &Path, session_id: &str) -> Option<u32> {
    let instances = std::fs::read_dir(agent_dir.join("daemon-workers")).ok()?;
    for instance in instances.flatten() {
        let descriptor_path = instance.path().join(format!("{session_id}.json"));
        let Ok(content) = std::fs::read_to_string(&descriptor_path) else {
            continue;
        };
        if let Ok(descriptor) = serde_json::from_str::<Value>(&content) {
            if let Some(pid) = descriptor.get("pid").and_then(Value::as_u64) {
                return u32::try_from(pid).ok();
            }
        }
    }
    None
}

/// How many of `session_ids` currently have a live worker pid (the
/// portable replacement for counting a supervisor's `/proc` children:
/// a session not yet recovered still has no descriptor, or a descriptor
/// whose last-known pid already died; a recovered session's descriptor
/// now names the freshly adopted, live pid).
fn live_worker_count(agent_dir: &Path, session_ids: &[String]) -> usize {
    session_ids
        .iter()
        .filter(|session_id| {
            worker_pid(agent_dir, session_id)
                .map(|pid| pa_daemon::lease::is_process_alive(pid).unwrap_or(false))
                .unwrap_or(false)
        })
        .count()
}

/// Worker ids with a registration log line at or after `since` (the log line lands
/// after the registry records the worker — the "routable" signal).
fn workers_registered_since(log_path: &Path, since: &str) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((timestamp, message)) = rest.split_once(']') else {
            continue;
        };
        if timestamp < since {
            continue;
        }
        let message = message.trim();
        if let Some(id) = message.strip_prefix("session worker ") {
            if let Some((worker_id, tail)) = id.split_once(" re-registered (epoch") {
                if tail.contains(')') {
                    ids.push(worker_id.to_string());
                }
            } else if let Some((worker_id, tail)) = id.split_once(" registered (epoch") {
                if tail.contains(')') {
                    ids.push(worker_id.to_string());
                }
            }
        }
    }
    ids
}

fn distinct(values: Vec<String>) -> Vec<String> {
    let mut values = values;
    values.sort();
    values.dedup();
    values
}

/// `kill -9` an arbitrary pid: the harness holds Child handles only for the
/// supervisors; the workers are adopted pids on the wire.
fn kill9(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "kill -9 {pid} failed: {status}");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        self.send(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(20);
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

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// One adoption wave relaunches at most `ADOPTION_CONCURRENCY` workers, so twelve
/// dead descriptors is three capped waves — a recovery observably long enough to
/// race the control plane.
const SESSIONS: usize = 12;

#[test]
fn control_plane_stays_responsive_while_a_large_adoption_pass_recovers() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        json!({ "responses": [ { "text": "one", "delayMs": 10 } ] }).to_string(),
    )
    .expect("write script");

    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let mut session_ids = Vec::new();
    for index in 0..SESSIONS {
        client.send_command(
            &format!("c{index}"),
            &json!({
                "type": "create",
                "config": {
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                    "script": script_path.to_string_lossy(),
                },
            }),
        );
        let created = client.read_response(&format!("c{index}"));
        assert_eq!(created["success"], true, "create {index} failed: {created}");
        session_ids.push(
            created["data"]["id"]
                .as_str()
                .or_else(|| created["data"]["sessionId"].as_str())
                .expect("session id")
                .to_string(),
        );
    }
    let deadline = Instant::now() + Duration::from_mins(1);
    let worker_pids = loop {
        let pids: Vec<u32> = session_ids
            .iter()
            .filter_map(|session_id| worker_pid(&agent_dir, session_id))
            .collect();
        if pids.len() == SESSIONS {
            break pids;
        }
        assert!(Instant::now() < deadline, "twelve workers never spawned");
        std::thread::sleep(Duration::from_millis(50));
    };

    daemon.child.kill().expect("kill -9 supervisor");
    let _ = daemon.child.wait();
    for pid in &worker_pids {
        kill9(*pid);
    }

    let restart_before = pa_daemon::util::now_iso();
    let _daemon2 = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);

    // The control plane answers mid-recovery: hello within the latency bound while
    // the pass is visibly unfinished (fewer than twelve relaunched children).
    let hello_start = Instant::now();
    let (mut client, hello) = Client::connect(&socket);
    let hello_latency = hello_start.elapsed();
    assert_eq!(hello["type"], "daemon_hello");
    assert!(
        hello_latency < Duration::from_secs(2),
        "hello starved behind the recovery: {hello_latency:?}"
    );
    assert!(
        live_worker_count(&agent_dir, &session_ids) < SESSIONS,
        "the recovery finished before the first hello; the responsiveness check is vacuous (live: {})",
        live_worker_count(&agent_dir, &session_ids)
    );

    // list answers mid-recovery too: it serves the registered rows instead of
    // queueing behind the pass.
    let list_start = Instant::now();
    client.send_command("list-mid-recovery", &json!({ "type": "list" }));
    let list_response = client.read_response("list-mid-recovery");
    let list_latency = list_start.elapsed();
    assert_eq!(
        list_response["success"], true,
        "list mid-recovery failed: {list_response}"
    );
    assert!(
        list_latency < Duration::from_secs(2),
        "list starved behind the recovery: {list_latency:?}"
    );

    // Queued recoveries complete: every descriptor relaunches and registers within a
    // bounded window. The wait is on registrations (the log line lands after the registry
    // records the worker), not on child processes.
    let log_path = pa_daemon::paths::daemon_log_path(&socket, &agent_dir);
    let deadline = Instant::now() + Duration::from_mins(2);
    loop {
        if distinct(workers_registered_since(&log_path, &restart_before)).len() == SESSIONS {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the adoption pass did not re-register every session; log: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // A routed command lands through the rebuilt roster: attach to one of the
    // recovered sessions.
    client.send_command(
        "attach-recovered",
        &json!({ "type": "attach", "activeSessionId": session_ids[0] }),
    );
    let attached = client.read_response("attach-recovered");
    assert_eq!(
        attached["success"], true,
        "attach to a recovered session failed: {attached}"
    );
}
