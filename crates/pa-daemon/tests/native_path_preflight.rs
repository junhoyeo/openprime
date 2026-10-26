//! Read-only native acceptance path preflight.
//!
//! This ignored verifier calls product path resolvers only. It never creates a
//! directory, starts a daemon, constructs a manager, or opens product state for
//! writing. Native acceptance harnesses must run it and review the JSON before
//! any write-capable process starts.
#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use serde_json::json;

fn env_path(name: &str) -> PathBuf {
    let value = std::env::var_os(name).unwrap_or_else(|| panic!("{name} must be set"));
    assert!(!value.is_empty(), "{name} must not be empty");
    PathBuf::from(value)
}

fn assert_absolute_contained(path: &Path, roots: &[&Path], label: &str) {
    assert!(
        path.is_absolute(),
        "{label} must be absolute: {}",
        path.display()
    );
    assert!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "{label} contains parent traversal: {}",
        path.display()
    );
    assert!(
        roots.iter().any(|root| path.starts_with(root)),
        "{label} escaped the owned root aliases {:?}: {}",
        roots,
        path.display()
    );
}

#[test]
#[ignore = "run explicitly before the one authorized native acceptance case"]
fn resolved_native_paths_stay_inside_the_owned_root() {
    let root = env_path("PA_NATIVE_PREFLIGHT_ROOT");
    let canonical_root = root
        .canonicalize()
        .expect("owned preflight root must exist");
    let owned_roots = [root.as_path(), canonical_root.as_path()];
    let cwd = std::env::current_dir().expect("current dir");
    assert_absolute_contained(&cwd, &owned_roots, "cwd");

    let agent_dir = pa_daemon::paths::agent_dir().expect("resolve agent dir");
    let sessions_dir = pa_daemon::paths::sessions_dir(&agent_dir).expect("resolve sessions dir");
    let socket_dir = pa_daemon::socket::socket_dir();
    let default_daemon_socket = pa_daemon::socket::default_daemon_socket_path();
    let requested_daemon_socket = env_path("PRIME_AGENT_DAEMON_SOCKET");
    let worker_socket = pa_daemon::socket::worker_socket_path(
        &requested_daemon_socket,
        "ffffffffffffffffffffffffffffffff",
    );
    let kernel_venv = pa_core::kernel::kernel_venv_dir();

    let write_paths = [
        ("agentDir", agent_dir.clone()),
        ("sessionsDir", sessions_dir.clone()),
        ("sessionArtifacts", agent_dir.join("session-artifacts")),
        ("daemonWorkers", agent_dir.join("daemon-workers")),
        ("logs", pa_daemon::paths::logs_dir(&agent_dir)),
        ("settings", agent_dir.join("settings.json")),
        ("models", agent_dir.join("models.json")),
        ("auth", agent_dir.join("auth.json")),
        ("harness", agent_dir.join("harness_state.json")),
        ("kernelVenv", kernel_venv.clone()),
        ("socketDir", socket_dir.clone()),
        ("defaultDaemonSocket", default_daemon_socket.clone()),
        ("requestedDaemonSocket", requested_daemon_socket.clone()),
        ("workerSocket", worker_socket.clone()),
    ];
    for (label, path) in &write_paths {
        assert_absolute_contained(path, &owned_roots, label);
    }

    for name in ["auth.json", "settings.json", "models.json"] {
        let path = agent_dir.join(name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("read owned synthetic {}: {error}", path.display()));
        assert!(
            matches!(bytes.as_slice(), b"{}" | b"{}\n"),
            "{name} must be the empty synthetic JSON object"
        );
    }

    let runtime_python = env_path("PRIME_AGENT_KERNEL_PYTHON")
        .canonicalize()
        .expect("read-only runtime Python must exist");
    let runtime_root = env_path("PA_NATIVE_READONLY_RUNTIME_ROOT")
        .canonicalize()
        .expect("read-only runtime root must exist");
    assert!(
        runtime_python.starts_with(&runtime_root),
        "runtime Python is outside the reviewed read-only exception: {}",
        runtime_python.display()
    );

    let socket_address = std::os::unix::net::SocketAddr::from_pathname(&worker_socket)
        .expect("longest worker endpoint must fit the native AF_UNIX limit");
    assert!(socket_address.as_pathname().is_some());
    assert!(
        !socket_dir.starts_with(format!(
            "/tmp/prime-agent-{}",
            pa_core::platform::process::current_user_id()
        )),
        "socket resolver fell back to the global per-user directory: {}",
        socket_dir.display()
    );

    let session_artifacts = agent_dir.join("session-artifacts");
    let worker_socket_bytes = worker_socket.as_os_str().as_bytes().len();
    let manifest = json!({
        "root": root,
        "canonicalRoot": canonical_root,
        "cwd": cwd,
        "agentDir": agent_dir,
        "sessionsDir": sessions_dir,
        "sessionArtifacts": session_artifacts,
        "kernelVenv": kernel_venv,
        "socketDir": socket_dir,
        "defaultDaemonSocket": default_daemon_socket,
        "requestedDaemonSocket": requested_daemon_socket,
        "workerSocket": worker_socket,
        "workerSocketBytes": worker_socket_bytes,
        "runtimePython": runtime_python,
        "runtimeRoot": runtime_root,
        "syntheticConfig": true,
        "writeCapablePathsContained": true,
    });
    println!(
        "{}",
        serde_json::to_string(&manifest).expect("serialize manifest")
    );
}
