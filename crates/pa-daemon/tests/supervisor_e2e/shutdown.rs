//! The supervisor process-exit paths - the shutdown command and the SIGTERM
//! graceful drain - with the owned-descriptor worker lookup and the
//! product's own portable liveness probe they lean on.

use std::path::Path;

use super::*;

/// The one worker spawned for `session_id`, read from its durable
/// descriptor (`agent_dir/daemon-workers/<socket-key>/<session_id>.json`),
/// the same lookup `idle_passivation_e2e.rs::worker_pid` already uses.
/// Reading the owned descriptor instead of walking every host process
/// for children of the supervisor's pid is both portable (no `/proc`
/// dependency) and a strictly more precise invariant: it names the
/// worker this test's own session actually has, rather than whatever the
/// host census of the supervisor's children happens to turn up.
fn worker_pid(agent_dir: &Path, session_id: &str) -> Option<u32> {
    let instances = std::fs::read_dir(agent_dir.join("daemon-workers")).ok()?;
    for instance in instances.flatten() {
        let descriptor_path = instance.path().join(format!("{session_id}.json"));
        let Ok(content) = std::fs::read_to_string(&descriptor_path) else {
            continue;
        };
        if let Ok(descriptor) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(pid) = descriptor.get("pid").and_then(serde_json::Value::as_u64) {
                return u32::try_from(pid).ok();
            }
        }
    }
    None
}

/// Portable liveness (the product's own probe - Linux `/proc` state,
/// macOS `kill(0)` + the Darwin zombie check): the assertions below keep
/// their meaning on every platform instead of reading a `/proc` that
/// does not exist on macOS.
fn process_alive(pid: u32) -> bool {
    pa_daemon::lease::is_process_alive(pid).expect("process liveness probe")
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The shutdown must stop every worker and exit the supervisor, cleaning
/// up its socket: a supervisor parked on its socket blocks replacement.
#[test]
fn shutdown_command_exits_the_supervisor_process() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let (mut client, _hello) = Client::connect(&socket);

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [ { "text": "x" } ] }).to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        &serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("create response names the session id")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = worker_pid(&agent_dir, &session_id) {
            break pid;
        }
        assert!(Instant::now() < deadline, "worker never spawned");
        std::thread::sleep(Duration::from_millis(50));
    };

    client.send_command("sd", &serde_json::json!({ "type": "shutdown" }));
    let shutdown = client.read_response("sd");
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");

    let exit = wait_child_exit(&mut daemon.child, Duration::from_secs(10))
        .expect("the supervisor process exited after shutdown");
    assert!(exit.success(), "supervisor exit: {exit:?}");
    assert!(!socket.exists(), "the socket file is removed on exit");

    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "worker {pid} leaked after shutdown"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM to the live supervisor is the graceful drain, not the default
/// signal death: the process exits 0 and cleans its socket file up. A
/// connected client rides the drain.
#[test]
fn a_sigterm_exits_the_supervisor_through_the_graceful_drain() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let (_client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(daemon.child.id().to_string())
        .status()
        .expect("SIGTERM the supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the drained supervisor must exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "the drain must end in a clean exit 0, not a signal death: {status}"
    );
    assert!(
        !socket.exists(),
        "the drained supervisor cleans its socket file up"
    );
}
