//! The delete of an RLM child whose worker process is gone: the resident
//! delete completes without a worker handshake, and a retry after the
//! ledger boundary already stands succeeds only once the worker is gone.

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    supervisor: Arc<Supervisor>,
    child_file: String,
    ledger: Arc<crate::rlm_ledger::RlmSpawnLedger>,
}

async fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let supervisor = Arc::new(
        Supervisor::new(SupervisorOptions {
            socket_path: dir.path().join("daemon.sock"),
            agent_dir,
        })
        .expect("supervisor"),
    );
    let child_file = sessions_dir.join("child-dead.jsonl");
    std::fs::write(
        &child_file,
        format!("{}\n", json!({
            "type": "session", "version": 3, "id": "child-dead",
            "timestamp": "t", "cwd": dir.path(), "rlmDepth": 1,
        })),
    )
    .unwrap();
    let parent_file = sessions_dir.join("parent.jsonl");
    std::fs::write(&parent_file, "{\"type\":\"session\",\"id\":\"p\"}\n").unwrap();
    let ledger = supervisor
        .rlm_spawn_ledger_for(None)
        .await
        .expect("the spawn ledger resolves");
    let child_file = child_file.to_string_lossy().into_owned();
    ledger
        .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-dead".to_string(),
            parent: parent_file.to_string_lossy().into_owned(),
            child: child_file.clone(),
            depth: 1,
            name: "dead-worker".to_string(),
        })
        .expect("the spawn edge appends");
    Fixture {
        _dir: dir,
        supervisor,
        child_file,
        ledger,
    }
}

/// The child worker's descriptor, persisted under the supervisor's descriptor dir.
fn child_descriptor(
    supervisor: &Supervisor,
    child_file: &str,
    pid: u32,
    start_id: Option<&str>,
) -> (DaemonWorkerDescriptor, std::path::PathBuf) {
    let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
        "version": 2,
        "workerId": "w-child",
        "pid": pid,
        "processStartId": start_id,
        "socketPath": supervisor.descriptor_dir.join("w-child.sock"),
        "recoveryJournalPath": supervisor.descriptor_dir.join("w-child.recovery.jsonl"),
        "supervisorSocketPath": supervisor.options.socket_path.to_string_lossy(),
        "authenticationToken": "token",
        "rootActiveSessionId": "w-child",
        "sessionFile": child_file,
        "createdAt": "t",
        "updatedAt": "t",
        "lifecycle": "ready",
        "createCommand": { "rlmDepth": 1, "rlmChildId": "sub-dead" },
        "consecutiveFailures": 0,
    }))
    .expect("descriptor");
    let path = supervisor.descriptor_dir.join("w-child.json");
    crate::descriptor::persist_worker(&path, &descriptor).expect("descriptor persists");
    (descriptor, path)
}

/// A pid that provably belonged to a process which has exited and been reaped.
fn reaped_pid() -> (u32, Option<String>) {
    let mut child = std::process::Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn cooperative child");
    let pid = child.id();
    // The owned pipe keeps this generation alive until its identity is read.
    let start_id = crate::lease::get_process_start_id(pid);
    drop(child.stdin.take());
    child.wait().expect("reap cooperative child");
    assert!(start_id.is_some(), "owned child must have a live start identity");
    (pid, start_id)
}

async fn route_delete(supervisor: &Arc<Supervisor>) -> Value {
    let (targeted_tx, _targeted_rx) = tokio::sync::mpsc::channel::<Arc<Value>>(16);
    let attached = subscribers::ClientSubscriptions::new("c".to_string(), targeted_tx);
    let command: DaemonCommand = serde_json::from_value(json!({
        "type": "kill",
        "activeSessionId": "w-child",
        "rlmLedgerDelete": "user",
        "rlmChildId": "sub-dead",
    }))
    .expect("command");
    let (lines, _) = supervisor
        .route_client_command(
            &command,
            "c",
            &attached,
            "k-1".to_string(),
            "kill".to_string(),
            None,
        )
        .await;
    lines.into_iter().next().expect("one response line")
}

/// A reaped worker awaiting its relaunch can never answer the kill: the delete
/// succeeds from the ledger boundary alone instead of answering the route's
/// not-connected failure after the stop already ran.
#[tokio::test]
async fn a_delete_of_a_dead_workers_child_completes_without_a_handshake() {
    let Fixture {
        _dir,
        supervisor,
        child_file,
        ledger,
    } = fixture().await;
    let (pid, start_id) = reaped_pid();
    let (descriptor, descriptor_path) =
        child_descriptor(&supervisor, &child_file, pid, start_id.as_deref());
    let resident = ResidentWorker::new("w-child".to_string(), descriptor, descriptor_path.clone());
    resident.note_session_ready();
    supervisor.registry.insert(Arc::clone(&resident)).await;

    let response = route_delete(&supervisor).await;

    assert_eq!(response["success"], json!(true), "{response}");
    assert!(supervisor.registry.get("w-child").await.is_none());
    assert!(
        !descriptor_path.exists(),
        "the dead worker's descriptor retires"
    );
    assert!(ledger.live_edges().expect("edges").is_empty());
}

/// Liveness, not route state, gates the short-circuit: a live worker still
/// receives the kill.
#[tokio::test]
async fn a_delete_of_a_live_workers_child_still_routes_the_kill() {
    let Fixture {
        _dir,
        supervisor,
        child_file,
        ..
    } = fixture().await;
    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep");
    let pid = sleeper.id();
    let (descriptor, descriptor_path) = child_descriptor(
        &supervisor,
        &child_file,
        pid,
        crate::lease::get_process_start_id(pid).as_deref(),
    );
    let resident = ResidentWorker::new("w-child".to_string(), descriptor, descriptor_path);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkerRequest>(4);
    *resident.cmd_tx.lock().await = Some(cmd_tx);
    let pump_resident = Arc::clone(&resident);
    let pump = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(request) = cmd_rx.recv().await {
            seen.push(request.command_type.clone());
            if let Some(reply) = pump_resident
                .pending
                .lock()
                .await
                .remove(&request.request_id)
            {
                let _ = reply.send(WorkerReply::Typed(crate::protocol::response_success(
                    None,
                    &request.command_type,
                    None,
                )));
            }
            if request.command_type == "shutdown" {
                break;
            }
        }
        seen
    });
    resident.note_connection_live();
    resident.note_session_ready();
    supervisor.registry.insert(Arc::clone(&resident)).await;

    let response = route_delete(&supervisor).await;

    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert_eq!(response["success"], json!(true), "{response}");
    let seen = pump.await.expect("pump");
    assert_eq!(seen.first().map(String::as_str), Some("kill"), "{seen:?}");
}

/// A retry after the boundary already stands is idempotent once the worker is
/// gone, and refused while its process still runs.
#[tokio::test]
async fn a_retried_delete_after_the_tombstone_is_gated_on_the_worker_process() {
    let Fixture {
        _dir,
        supervisor,
        child_file,
        ledger,
    } = fixture().await;
    ledger
        .append_delete(
            "sub-dead",
            &child_file,
            crate::rlm_ledger::RlmLedgerDeleteReason::User,
        )
        .expect("the first attempt's tombstone");
    let retry = || {
        supervisor.tombstone_saved_rlm_child(
            "w-child",
            Some("sub-dead"),
            crate::rlm_ledger::RlmLedgerDeleteReason::User,
        )
    };

    // The first attempt stopped after the tombstone, before archiving or
    // sweeping artifacts. A successful retry must finish that cleanup.
    let artifacts = crate::scheduled_jobs::session_artifact_dir(
        std::path::Path::new(&child_file),
        "child-dead",
    )
    .expect("artifact directory");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(
        artifacts.join("unfinished.txt"),
        "left by interrupted delete",
    )
    .unwrap();
    let (pid, start_id) = reaped_pid();
    child_descriptor(&supervisor, &child_file, pid, start_id.as_deref());
    retry()
        .await
        .expect("a dead worker no longer blocks the delete");
    assert!(!artifacts.exists(), "the retry finishes the artifact sweep");
    assert_eq!(
        crate::session_store::read_session_info(std::path::Path::new(&child_file))
            .expect("session metadata")
            .state
            .as_deref(),
        Some("archived"),
        "the retry archives the stopped session"
    );

    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep");
    let live_pid = sleeper.id();
    child_descriptor(
        &supervisor,
        &child_file,
        live_pid,
        crate::lease::get_process_start_id(live_pid).as_deref(),
    );
    let ledger_before = std::fs::read(ledger.ledger_path()).unwrap();
    let refused = retry().await;
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let error = format!(
        "{:#}",
        refused.expect_err("a live worker refuses the delete")
    );
    assert!(error.contains(&format!("(pid {live_pid})")), "{error}");
    assert_eq!(std::fs::read(ledger.ledger_path()).unwrap(), ledger_before);
}

/// #42: deletion retires only the child's revival context. A restarted
/// supervisor must refuse every create over its surviving transcript.
#[tokio::test]
async fn issue_42_deleted_child_record_is_retired_and_restart_create_is_refused() {
    tokio::time::timeout(Duration::from_secs(15), async {
    let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
    let launch = crate::passivated_launch::PassivatedLaunch::new(
        &crate::descriptor::durable_create_command(&json!({
            "sessionPath": child_file,
            "cwd": dir.path(),
            "rlmDepth": 1,
        })),
        Some(true),
    );
    supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
    let parent_file = supervisor.options.agent_dir.join("sessions/parent.jsonl");
    let parent_launch = crate::passivated_launch::PassivatedLaunch::new(
        &crate::descriptor::durable_create_command(&json!({
            "sessionPath": parent_file, "cwd": dir.path(), "appendSystemPrompt": "parent context"
        })), None,
    );
    supervisor.passivated_launches.remember(parent_file.to_str().unwrap(), &parent_launch).unwrap();
    let (pid, start_id) = reaped_pid();
    let (descriptor, path) = child_descriptor(&supervisor, &child_file, pid, start_id.as_deref());
    let resident = ResidentWorker::new("w-child".to_string(), descriptor, path);
    resident.note_session_ready();
    supervisor.registry.insert(resident).await;
    let response = route_delete(&supervisor).await;
    assert_eq!(response["success"], true, "{response}");
    assert!(ledger.edges(true).unwrap()[0].deleted.is_some());
    assert!(std::path::Path::new(&child_file).is_file(), "the transcript stays");
    assert!(supervisor.passivated_launches.get(&child_file).is_none(), "deleted child revival context must retire");
    assert!(supervisor.passivated_launches.get(parent_file.to_str().unwrap()).is_some(), "parent revival context survives");

    let restarted = Arc::new(Supervisor::new(SupervisorOptions {
        socket_path: supervisor.options.socket_path.clone(),
        agent_dir: supervisor.options.agent_dir.clone(),
    }).unwrap());
    // Recreate an old-format leftover from an interrupted metadata cleanup.
    restarted.passivated_launches.remember(&child_file, &launch).unwrap();
    restarted.pin_worker_connect_budget_for_tests(Duration::ZERO);
    let create: DaemonCommand = serde_json::from_value(json!({
        "type": "create", "sessionPath": child_file,
        "config": {"cwd": dir.path()},
    })).unwrap();
    let error = restarted.handle_create(&create, "c".to_string()).await.unwrap_err();
    assert!(error.to_string().contains("deleted RLM child"), "{error:#}");
    assert!(restarted.registry.list().await.is_empty());
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let attached = subscribers::ClientSubscriptions::new("wake-client".into(), tx);
    for kind in ["prompt", "attach", "wait_for_idle"] {
        let command: DaemonCommand = serde_json::from_value(json!({
            "type": kind, "activeSessionId": "child-dead", "message": "synthetic"
        })).unwrap();
        let (response, _) = restarted.route_client_command(
            &command, "wake-client", &attached, "wake".into(), kind.into(), None,
        ).await;
        assert_eq!(response[0]["success"], false, "{response:?}");
        assert!(response[0]["error"].as_str().unwrap().contains("deleted RLM child"), "{response:?}");
    }
    assert!(route_delete(&restarted).await["success"].as_bool().unwrap());
    assert!(route_delete(&restarted).await["success"].as_bool().unwrap());
    assert!(restarted.passivated_launches.get(&child_file).is_none());
    assert!(restarted.registry.list().await.is_empty());
    }).await.expect("bounded deleted-child lifecycle");
}

/// #42: a stale wake captured before delete must wait, then reread the
/// tombstone. Polling the futures once is an explicit lock-arrival barrier.
#[tokio::test]
async fn issue_42_delete_and_stale_wake_serialize_on_the_transcript() {
    tokio::time::timeout(Duration::from_secs(15), async {
    use std::future::Future;
    use std::task::Poll;
    let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
    let gate = supervisor.child_lifecycles.lock(&child_file).await.unwrap();
    let mut deletion = Box::pin(supervisor.tombstone_saved_rlm_child(
        "child-dead", Some("sub-dead"), crate::rlm_ledger::RlmLedgerDeleteReason::User,
    ));
    std::future::poll_fn(|cx| {
        assert!(deletion.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }).await;
    assert!(ledger.edges(true).unwrap()[0].deleted.is_none(), "delete waits at the gate");
    let create: DaemonCommand = serde_json::from_value(json!({
        "type": "create", "sessionPath": child_file, "config": {"cwd": dir.path()}
    })).unwrap();
    let mut wake = Box::pin(supervisor.launch_worker(&create, None));
    std::future::poll_fn(|cx| {
        assert!(wake.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }).await;
    assert!(supervisor.registry.list().await.is_empty(), "wake never registers across the gate");
    drop(gate);
    let (deleted, woken) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(deletion, wake)
    }).await.expect("bounded lifecycle barrier");
    deleted.unwrap();
    let error = woken.err().expect("the stale wake is refused");
    assert!(error.to_string().contains("deleted RLM child"), "{error:#}");
    assert!(supervisor.registry.list().await.is_empty());
    }).await.expect("bounded deleted-child lifecycle");
}

/// #42: if revival won the file gate, the stopped-child delete discovers
/// and retires that resident rather than acknowledging a live deleted child.
#[tokio::test]
async fn issue_42_a_wake_that_won_the_gate_is_stopped_by_delete() {
    tokio::time::timeout(Duration::from_secs(15), async {
    use std::future::Future;
    use std::task::Poll;
    let Fixture { _dir, supervisor, child_file, ledger } = fixture().await;
    let gate = supervisor.child_lifecycles.lock(&child_file).await.unwrap();
    let mut deletion = Box::pin(supervisor.tombstone_saved_rlm_child(
        "child-dead", Some("sub-dead"), crate::rlm_ledger::RlmLedgerDeleteReason::User,
    ));
    std::future::poll_fn(|cx| {
        assert!(deletion.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }).await;
    let (pid, start_id) = reaped_pid();
    let (descriptor, path) = child_descriptor(&supervisor, &child_file, pid, start_id.as_deref());
    let resident = ResidentWorker::new("w-child".into(), descriptor, path);
    resident.note_session_ready();
    supervisor.registry.insert(resident).await;
    drop(gate);
    tokio::time::timeout(Duration::from_secs(5), deletion).await.unwrap().unwrap();
    assert!(ledger.edges(true).unwrap()[0].deleted.is_some());
    assert!(supervisor.registry.list().await.is_empty(), "raced resident cannot survive successful delete");
    }).await.expect("bounded deleted-child lifecycle");
}

/// #42: a committed logical child delete still owns its absent transcript.
/// Generic physical deletion may fail; that cannot authorize recreation.
#[tokio::test]
async fn issue_42_deleted_missing_transcript_cannot_recreate_from_a_stale_record() {
    tokio::time::timeout(Duration::from_secs(15), async {
    let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
    let launch = crate::passivated_launch::PassivatedLaunch::new(
        &crate::descriptor::durable_create_command(&json!({
            "sessionPath": child_file, "cwd": dir.path(), "rlmDepth": 2
        })), None,
    );
    supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
    std::fs::remove_file(&child_file).unwrap();
    let deleted = route_delete(&supervisor).await;
    assert_eq!(deleted["success"], true, "{deleted}");
    assert!(ledger.edges(true).unwrap()[0].deleted.is_some());
    assert!(supervisor.passivated_launches.get(&child_file).is_none());
    supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
    // Physical missing-file behavior is covered by the trash owner. This
    // lane never invokes the baseline helper, which unconditionally runs trash.
    supervisor.pin_worker_connect_budget_for_tests(Duration::ZERO);
    let create: DaemonCommand = serde_json::from_value(json!({
        "type": "create", "sessionPath": child_file, "config": {"cwd": dir.path()}
    })).unwrap();
    let error = supervisor.handle_create(&create, "c".into()).await.unwrap_err();
    assert!(error.to_string().contains("deleted RLM child"), "{error:#}");
    assert!(!Path::new(&child_file).exists());
    assert!(supervisor.registry.list().await.is_empty());
    }).await.expect("bounded deleted-child lifecycle");
}

/// #42: cleanup errors remain visible after the logical tombstone, and
/// never authorize artifact loss or recreation from retained metadata.
#[tokio::test]
async fn issue_42_failed_finalization_retains_artifacts_and_blocks_revival() {
    tokio::time::timeout(Duration::from_secs(15), async {
    let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
    let launch = crate::passivated_launch::PassivatedLaunch::new(
        &crate::descriptor::durable_create_command(&json!({
            "sessionPath": child_file, "cwd": dir.path()
        })), None,
    );
    supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
    let artifacts = crate::scheduled_jobs::session_artifact_dir(Path::new(&child_file), "child-dead").unwrap();
    std::fs::create_dir_all(&artifacts).unwrap();
    let sentinel = artifacts.join("retained.txt");
    std::fs::write(&sentinel, "owned fixture evidence").unwrap();
    std::fs::write(&child_file, [0xff, 0xfe]).unwrap();
    let response = route_delete(&supervisor).await;
    assert_eq!(response["success"], false, "{response}");
    assert!(ledger.edges(true).unwrap()[0].deleted.is_some());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"owned fixture evidence");
    assert_eq!(std::fs::read(&child_file).unwrap(), [0xff, 0xfe]);
    let create: DaemonCommand = serde_json::from_value(json!({
        "type": "create", "sessionPath": child_file, "config": {"cwd": dir.path()}
    })).unwrap();
    supervisor.pin_worker_connect_budget_for_tests(Duration::ZERO);
    let error = supervisor.handle_create(&create, "c".into()).await.unwrap_err();
    assert!(error.to_string().contains("deleted RLM child"), "{error:#}");
    }).await.expect("bounded deleted-child lifecycle");
}

/// #42: partially understood topology cannot authorize launch or deletion.
///
/// Review correction: the original parametrization also iterated a
/// "malformed" ledger-corruption case (a line that fails to parse as JSON
/// at all) and expected it to refuse deletion/create the same way as
/// "unknown" and "io". That no longer holds under the canonical,
/// independently-reviewed Authority-read disposition (skip+log a
/// torn/unparseable line, never bail the whole read) this lane adopted
/// earlier in this same effort. Dropped "malformed" from this refusing
/// test; its own, qualitatively different expected flow (the delete
/// SUCCEEDS once the bad line is skipped) is its own test below,
/// `issue_42_a_malformed_ledger_line_is_skipped_so_the_valid_spawn_still_deletes`.
#[tokio::test]
async fn issue_42_unknown_ledger_authority_refuses_create_and_preserves_delete_evidence() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for (corruption, reason) in [
            ("unknown", "unknown operation"),
            ("io", "not a regular file"),
        ] {
            let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
            let launch = crate::passivated_launch::PassivatedLaunch::new(
                &crate::descriptor::durable_create_command(&json!({
                    "sessionPath": child_file, "cwd": dir.path()
                })), None,
            );
            supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
            let mut bytes = std::fs::read(ledger.ledger_path()).unwrap();
            let evidence_path = if corruption == "io" {
                let backup = dir.path().join("ledger-evidence.jsonl");
                std::fs::rename(ledger.ledger_path(), &backup).unwrap();
                std::fs::create_dir(ledger.ledger_path()).unwrap();
                backup
            } else {
                bytes.extend_from_slice(b"{\"v\":1,\"op\":\"future-delete\",\"at\":\"t\"}\n");
                std::fs::write(ledger.ledger_path(), &bytes).unwrap();
                ledger.ledger_path().to_path_buf()
            };
            let transcript = std::fs::read(&child_file).unwrap();
            let deleted = route_delete(&supervisor).await;
            assert_eq!(deleted["success"], false, "{corruption}: {deleted}");
            assert_eq!(std::fs::read(&evidence_path).unwrap(), bytes);
            assert_eq!(std::fs::read(&child_file).unwrap(), transcript);
            assert!(supervisor.passivated_launches.get(&child_file).is_some());
            supervisor.pin_worker_connect_budget_for_tests(Duration::ZERO);
            let create: DaemonCommand = serde_json::from_value(json!({
                "type": "create", "sessionPath": child_file, "config": {"cwd": dir.path()}
            })).unwrap();
            let error = supervisor.handle_create(&create, "c".into()).await.unwrap_err();
            assert!(error.to_string().contains(reason), "authority must refuse before a spawn probe: {error:#}");
            assert!(supervisor.registry.list().await.is_empty());
            assert_eq!(std::fs::read(evidence_path).unwrap(), bytes);
            if corruption == "io" { assert!(ledger.ledger_path().is_dir()); }
        }
    }).await.expect("bounded deleted-child authority check");
}

/// The split-out "malformed" case: a line that fails to parse as JSON at
/// all is the SAME shape the ledger's own append-then-fsync-before-ack
/// protocol already treats as "never committed" for Authority reads
/// elsewhere in this effort (the earlier-resolved rlm_ledger.rs skip-vs-
/// refuse conflict). `strict_edges()` skips it and still reads the
/// fixture's valid "sub-dead" spawn record, so deletion authority proceeds
/// and genuinely commits. Confirmed via direct source read (not guessed):
/// `commit_child_deletion` only appends to the ledger and (if a resident
/// exists) persists its descriptor - it never touches the transcript file
/// - and its caller (`routing.rs`'s delete-by-selector path) calls
/// `passivated_launches.forget_deleted(...)` immediately after a
/// successful commit, so the remembered launch is cleared, not retained.
#[tokio::test]
async fn issue_42_a_malformed_ledger_line_is_skipped_so_the_valid_spawn_still_deletes() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let Fixture { _dir: dir, supervisor, child_file, ledger } = fixture().await;
        let launch = crate::passivated_launch::PassivatedLaunch::new(
            &crate::descriptor::durable_create_command(&json!({
                "sessionPath": child_file, "cwd": dir.path()
            })), None,
        );
        supervisor.passivated_launches.remember(&child_file, &launch).unwrap();
        let mut with_malformed = std::fs::read(ledger.ledger_path()).unwrap();
        with_malformed.extend_from_slice(b"{malformed-record}\n");
        std::fs::write(ledger.ledger_path(), &with_malformed).unwrap();
        let transcript = std::fs::read(&child_file).unwrap();

        let deleted = route_delete(&supervisor).await;
        assert_eq!(deleted["success"], true, "a skippable malformed line must not block a real deletion: {deleted}");

        // The pre-existing bytes (the spawn record and the malformed line) are
        // an unchanged prefix; the commit only ever appends, never rewrites.
        let after = std::fs::read(ledger.ledger_path()).unwrap();
        assert!(
            after.starts_with(&with_malformed),
            "the spawn record and the skipped malformed line must survive untouched"
        );
        assert!(
            after.len() > with_malformed.len(),
            "a real tombstone record must be appended after the skipped line"
        );
        let edges = ledger.strict_edges().unwrap();
        assert!(
            edges.iter().any(|edge| edge.child_id == "sub-dead" && edge.deleted.is_some()),
            "the committed tombstone must be readable past the skipped malformed line"
        );

        assert_eq!(std::fs::read(&child_file).unwrap(), transcript, "a logical delete never touches the transcript file");
        assert!(
            supervisor.passivated_launches.get(&child_file).is_none(),
            "a genuinely committed deletion clears the remembered launch, unlike a refused one"
        );

        supervisor.pin_worker_connect_budget_for_tests(Duration::ZERO);
        let create: DaemonCommand = serde_json::from_value(json!({
            "type": "create", "sessionPath": child_file, "config": {"cwd": dir.path()}
        })).unwrap();
        let error = supervisor.handle_create(&create, "c".into()).await.unwrap_err();
        assert!(
            error.to_string().contains("Cannot revive deleted RLM child"),
            "a re-create at a now-deleted path must refuse as a revival, not a ledger-readability error: {error:#}"
        );
        assert!(supervisor.registry.list().await.is_empty());
    }).await.expect("bounded deleted-child authority check");
}

/// #42 T1: only an Ok append commits a deletion. A delete line torn mid-write
/// returns Err, the strict read skips it, and the child was never deleted:
/// the same child id at the same path may be admitted again.
#[tokio::test]
async fn issue_42_a_torn_commit_child_deletion_never_committed() {
    let Fixture { _dir, supervisor, child_file, ledger } = fixture().await;
    let parent_file = supervisor.options.agent_dir.join("sessions/parent.jsonl");
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(Some(12)));
    let result = supervisor
        .commit_child_deletion(&child_file, "sub-dead", crate::rlm_ledger::RlmLedgerDeleteReason::User, None)
        .await;
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(None));
    assert!(result.is_err(), "a torn delete is not acknowledged");
    // A fresh reader with a captured log: the torn line is skipped and
    // logged by file and line number, never by content.
    let logs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&logs);
    let logged = crate::rlm_ledger::RlmSpawnLedger::new(
        &supervisor.options.agent_dir,
        &crate::paths::sessions_dir(&supervisor.options.agent_dir).unwrap(),
        move |line| sink.lock().unwrap().push(line.to_string()),
    );
    assert_eq!(logged.strict_edges().expect("the torn line is skipped, not fatal").len(), 1);
    {
        let logs = logs.lock().unwrap();
        let entry = logs.iter().find(|line| line.contains("skipped")).expect("a skipped-line log entry");
        assert!(entry.contains(&logged.ledger_path().display().to_string()) && entry.contains("on line"), "{entry}");
        assert!(!entry.contains("sub-dead"), "record content is never logged: {entry}");
    }
    let edges = ledger.strict_edges().expect("the torn line is skipped, not fatal");
    assert_eq!(edges.len(), 1);
    assert!(edges[0].deleted.is_none(), "the torn delete is not a tombstone");
    assert!(std::path::Path::new(&child_file).is_file(), "no physical cleanup ran");
    ledger
        .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-dead".to_string(),
            parent: parent_file.to_string_lossy().into_owned(),
            child: child_file.clone(),
            depth: 1,
            name: "dead-worker".to_string(),
        })
        .expect("a never-deleted child is admitted again");
}

/// #42 ordering: a failed tombstone append leaves a resident child live. It is
/// neither quarantined nor marked for a stop, because nothing was deleted.
#[tokio::test]
async fn issue_42_a_failed_commit_child_deletion_leaves_the_resident_live() {
    let Fixture { _dir, supervisor, child_file, ledger } = fixture().await;
    let (pid, start_id) = reaped_pid();
    let (descriptor, descriptor_path) = child_descriptor(&supervisor, &child_file, pid, start_id.as_deref());
    let before = std::fs::read(&descriptor_path).unwrap();
    let resident = ResidentWorker::new("w-child".to_string(), descriptor, descriptor_path.clone());
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(Some(12)));
    let result = supervisor
        .commit_child_deletion(&child_file, "sub-dead", crate::rlm_ledger::RlmLedgerDeleteReason::User, Some(&resident))
        .await;
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(None));
    assert!(result.is_err(), "the append failed");
    assert!(!resident.identity_quarantined(), "no quarantine without a committed tombstone");
    assert!(!resident.intentional_stop.load(std::sync::atomic::Ordering::SeqCst), "no stop intent");
    assert!(resident.descriptor.lock().await.stop_requested_at.is_none());
    assert_eq!(std::fs::read(&descriptor_path).unwrap(), before, "the persisted descriptor is unchanged");
    assert!(ledger.strict_edges().unwrap().iter().all(|edge| edge.deleted.is_none()));
}

/// The #42 crash window: the child's tombstone committed, then the supervisor
/// "crashed" before the resident's stop descriptor persisted. Returns the
/// persisted descriptor path; the descriptor carries no stop intent.
async fn crash_after_child_tombstone(fixture: &Fixture) -> std::path::PathBuf {
    let launch = crate::passivated_launch::PassivatedLaunch::new(
        &crate::descriptor::durable_create_command(&json!({
            "sessionPath": fixture.child_file, "cwd": fixture._dir.path(), "rlmDepth": 1,
        })),
        Some(true),
    );
    fixture.supervisor.passivated_launches.remember(&fixture.child_file, &launch).unwrap();
    let (pid, start_id) = reaped_pid();
    let (descriptor, descriptor_path) =
        child_descriptor(&fixture.supervisor, &fixture.child_file, pid, start_id.as_deref());
    let resident = ResidentWorker::new("w-child".to_string(), descriptor, descriptor_path.clone());
    fixture.supervisor.lifecycle_test_hooks.crash_after_tombstone.store(true, std::sync::atomic::Ordering::SeqCst);
    let crashed = fixture
        .supervisor
        .commit_child_deletion(&fixture.child_file, "sub-dead", crate::rlm_ledger::RlmLedgerDeleteReason::User, Some(&resident))
        .await;
    assert!(format!("{:#}", crashed.unwrap_err()).contains("injected crash"));
    assert!(fixture.ledger.strict_edges().unwrap().iter().all(|edge| edge.deleted.is_some()), "the tombstone committed");
    let persisted: DaemonWorkerDescriptor = serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
    assert!(persisted.stop_requested_at.is_none(), "the stop never persisted");
    descriptor_path
}

fn restarted(fixture: &Fixture) -> Arc<Supervisor> {
    let restarted = Arc::new(Supervisor::new(SupervisorOptions {
        socket_path: fixture.supervisor.options.socket_path.clone(),
        agent_dir: fixture.supervisor.options.agent_dir.clone(),
    }).unwrap());
    restarted.pin_worker_connect_budget_for_tests(Duration::ZERO);
    restarted
}

/// GREEN: boot adoption on a fresh supervisor finds the committed tombstone
/// through `stop_ledger_deleted_worker` and finishes the stop. RED (adoption
/// without that call): the descriptor has no stop intent, so the outcome is
/// not `Stopped` and the deleted child's descriptor survives.
#[tokio::test]
async fn issue_42_boot_adoption_finishes_the_stop_after_a_crash_past_the_tombstone() {
    let fixture = fixture().await;
    let descriptor_path = crash_after_child_tombstone(&fixture).await;
    let restarted = restarted(&fixture);
    let mut outcomes = Vec::new();
    for (path, descriptor) in load_descriptors(&restarted.descriptor_dir, &restarted.options.socket_path) {
        outcomes.push(restarted.adopt_persisted_worker_for_tests(path, descriptor, AdoptionBoot::PlainStartup).await);
    }
    let count = |outcome| outcomes.iter().filter(|seen| **seen == outcome).count();
    assert_eq!(count(super::adoption::AdoptionOutcome::Stopped), 1, "{outcomes:?}");
    assert_eq!(count(super::adoption::AdoptionOutcome::Revived), 0, "{outcomes:?}");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(restarted.registry.get("w-child").await.is_none(), "the deleted child is not adopted");
    assert!(!descriptor_path.exists(), "boot adoption finished the tombstoned stop");
    assert!(restarted.passivated_launches.get(&fixture.child_file).is_none(), "its revival context retired");
    assert!(std::path::Path::new(&fixture.child_file).is_file(), "the stop never removes the transcript");
}

/// The same crash window through self-registration on a fresh supervisor:
/// the registrant is refused definitively and nothing is adopted.
#[tokio::test]
async fn issue_42_registration_after_a_crash_past_the_tombstone_is_refused() {
    let fixture = fixture().await;
    let descriptor_path = crash_after_child_tombstone(&fixture).await;
    let persisted: DaemonWorkerDescriptor = serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
    let restarted = restarted(&fixture);
    let command = DaemonCommand::WorkerRegister {
        id: None,
        active_session_id: "w-child".to_string(),
        session_id: None,
        socket_path: persisted.socket_path.clone(),
        worker_instance_id: String::new(),
        token: persisted.authentication_token.clone(),
        pid: persisted.pid,
        rest: serde_json::Map::default(),
    };
    let response = restarted.handle_worker_register("r-1", "worker_register", &command).await;
    assert!(!response.success, "{response:?}");
    let error = response.error.unwrap_or_default();
    assert!(error.starts_with(crate::registration::UNKNOWN_SESSION_WORKER_PREFIX) && error.contains("registration refused"), "{error}");
    assert!(restarted.registry.get("w-child").await.is_none());
    assert!(!descriptor_path.exists(), "the refused registrant's stop finished");
    assert!(restarted.passivated_launches.get(&fixture.child_file).is_none(), "its revival context retired");
}
