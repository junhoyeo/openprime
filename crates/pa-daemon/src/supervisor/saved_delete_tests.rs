//! #42 saved-session delete order: the tombstone commits under M before the
//! physical removal, through the real supervisor receiver. Test builds use
//! `SessionFileRemoval::Unlink`/`Fail`, so the user's Trash is never touched.

use super::*;
use crate::saved_session_commands::SessionFileRemoval;

struct Fixture {
    dir: tempfile::TempDir,
    supervisor: Supervisor,
    parent_file: String,
    child_file: String,
    ledger: Arc<crate::rlm_ledger::RlmSpawnLedger>,
}

// Kept async for caller consistency (every call site already `.await`s
// it); its body happens to be synchronous today, not a reason to churn
// callers for a style-only clippy preference.
#[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
async fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let supervisor = Supervisor::new(SupervisorOptions {
        socket_path: dir.path().join("daemon.sock"),
        agent_dir,
    })
    .expect("supervisor");
    let parent_file = sessions_dir.join("parent.jsonl");
    std::fs::write(&parent_file, "{\"type\":\"session\",\"id\":\"p\"}\n").unwrap();
    let child_file = sessions_dir.join("child-saved.jsonl");
    std::fs::write(
        &child_file,
        format!(
            "{}\n",
            json!({
                "type": "session", "version": 3, "id": "child-saved", "timestamp": "t",
                "cwd": dir.path(), "parentSession": parent_file, "rlmDepth": 1,
            })
        ),
    )
    .unwrap();
    // The same default scope the supervisor's deletion authority reads.
    let ledger = Arc::new(crate::rlm_ledger::RlmSpawnLedger::new(
        &supervisor.options.agent_dir,
        &crate::paths::sessions_dir(&supervisor.options.agent_dir).unwrap(),
        |_| {},
    ));
    let parent_file = parent_file.to_string_lossy().into_owned();
    let child_file = child_file.to_string_lossy().into_owned();
    ledger
        .append_spawn(&spawn_input(&parent_file, &child_file))
        .expect("the spawn edge appends");
    Fixture {
        dir,
        supervisor,
        parent_file,
        child_file,
        ledger,
    }
}

fn spawn_input(parent: &str, child: &str) -> crate::rlm_ledger::RlmSpawnInput {
    crate::rlm_ledger::RlmSpawnInput {
        child_id: "sub-saved".to_string(),
        parent: parent.to_string(),
        child: child.to_string(),
        depth: 1,
        name: "saved-child".to_string(),
    }
}

async fn delete(supervisor: &Arc<Supervisor>, child_file: &str) -> Value {
    let command = DaemonCommand::DeleteSavedSession {
        id: None,
        active_session_id: None,
        session_path: child_file.to_string(),
        rest: serde_json::Map::default(),
    };
    let (lines, _) = supervisor
        .handle_delete_saved_session(&command, "client-1", "d-1", "delete_saved_session")
        .await;
    lines.into_iter().next().expect("one response line")
}

fn tombstoned(ledger: &crate::rlm_ledger::RlmSpawnLedger) -> bool {
    ledger
        .strict_edges()
        .unwrap()
        .iter()
        .any(|edge| edge.child_id == "sub-saved" && edge.deleted.is_some())
}

/// RED on the old order (remove, then append): a failed tombstone append left
/// the transcript gone and the edge live, so the same child id at the same
/// path was admitted again. GREEN: the failed commit removes nothing.
#[tokio::test]
async fn issue_42_a_failed_saved_delete_tombstone_removes_nothing() {
    let Fixture {
        dir: _dir,
        supervisor,
        parent_file,
        child_file,
        ledger,
    } = fixture().await;
    let supervisor = Arc::new(supervisor);
    let bytes = std::fs::read(&child_file).unwrap();
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(Some(0)));
    let response = delete(&supervisor, &child_file).await;
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(None));
    assert_eq!(response["success"], json!(false), "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("nothing was removed"),
        "{response}"
    );
    assert_eq!(
        std::fs::read(&child_file).unwrap(),
        bytes,
        "no physical removal ran"
    );
    assert!(
        !tombstoned(&ledger),
        "an unacknowledged tombstone is not a deletion"
    );
    // The invariant #42 needs: the transcript never disappears while the edge is live.
    assert!(std::path::Path::new(&child_file).is_file() || tombstoned(&ledger));
    let _ = parent_file;
}

/// GREEN: a committed saved delete removes the file after the tombstone, and
/// the same child id can never come back at that path.
#[tokio::test]
async fn issue_42_a_saved_delete_tombstones_before_removal_and_refuses_revival() {
    let Fixture {
        dir: _dir,
        supervisor,
        parent_file,
        child_file,
        ledger,
    } = fixture().await;
    let supervisor = Arc::new(supervisor);
    let response = delete(&supervisor, &child_file).await;
    assert_eq!(response["success"], json!(true), "{response}");
    assert_eq!(response["data"]["ok"], json!(true), "{response}");
    assert_eq!(
        response["data"]["method"],
        json!("unlink"),
        "tests never reach the trash tool"
    );
    assert!(!std::path::Path::new(&child_file).exists());
    assert!(tombstoned(&ledger));
    let error = ledger
        .append_spawn(&spawn_input(&parent_file, &child_file))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Cannot revive deleted RLM child"),
        "{error:#}"
    );
}

/// A committed tombstone with a failed physical removal answers the typed
/// partial result and keeps the logical deletion.
#[tokio::test]
async fn issue_42_a_failed_physical_removal_keeps_the_committed_deletion() {
    let Fixture {
        dir: _dir,
        mut supervisor,
        parent_file,
        child_file,
        ledger,
    } = fixture().await;
    supervisor.session_file_removal = SessionFileRemoval::Fail;
    let supervisor = Arc::new(supervisor);
    let bytes = std::fs::read(&child_file).unwrap();
    let response = delete(&supervisor, &child_file).await;
    assert_eq!(response["success"], json!(true), "{response}");
    assert_eq!(response["data"]["ok"], json!(false), "{response}");
    assert_eq!(
        response["data"]["logicalDeletion"],
        json!("committed"),
        "{response}"
    );
    assert_eq!(
        std::fs::read(&child_file).unwrap(),
        bytes,
        "the transcript is retained for a retry"
    );
    assert!(tombstoned(&ledger));
    let error = ledger
        .append_spawn(&spawn_input(&parent_file, &child_file))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("Cannot revive deleted RLM child"),
        "{error:#}"
    );
}

/// The production strategy is never reachable from a test: it refuses before
/// spawning any trash process.
#[test]
#[should_panic(expected = "tests must never invoke the real trash tool")]
fn the_real_trash_strategy_is_denied_in_tests() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("never-trashed.jsonl");
    std::fs::write(&file, "{}").unwrap();
    let _ = crate::saved_session_commands::delete_session_file_with(
        &file,
        &|_| {},
        SessionFileRemoval::TrashThenUnlink,
    );
}

/// F2: a top-level saved session carries no child edge. The delete writes no
/// tombstone and answers the plain TS result.
#[tokio::test]
async fn a_top_level_saved_delete_writes_no_tombstone() {
    let Fixture {
        dir,
        supervisor,
        parent_file,
        ledger,
        ..
    } = fixture().await;
    let top = std::path::Path::new(&parent_file).with_file_name("top.jsonl");
    std::fs::write(
        &top,
        format!(
            "{}\n",
            json!({
                "type": "session", "version": 3, "id": "top", "timestamp": "t", "cwd": dir.path(),
            })
        ),
    )
    .unwrap();
    let before = std::fs::read(ledger.ledger_path()).unwrap();
    let supervisor = Arc::new(supervisor);
    let response = delete(&supervisor, &top.to_string_lossy()).await;
    assert_eq!(response["success"], json!(true), "{response}");
    assert_eq!(
        response["data"],
        json!({ "ok": true, "method": "unlink" }),
        "{response}"
    );
    assert!(!top.exists());
    assert_eq!(
        std::fs::read(ledger.ledger_path()).unwrap(),
        before,
        "no ledger record for a top-level session"
    );
}

/// F2: the worker receiver follows the same order. A failed commit removes
/// nothing; a committed delete removes the file and refuses revival.
#[tokio::test]
async fn issue_42_the_worker_saved_delete_commits_the_tombstone_first() {
    let Fixture {
        dir,
        supervisor,
        parent_file,
        child_file,
        ledger,
    } = fixture().await;
    let worker = crate::worker::Worker::new(worker_config(&supervisor, &dir), None);
    let created = worker
        .dispatch("create", &json!({ "noSession": true, "cwd": dir.path() }))
        .await;
    assert!(created.success, "{created:?}");
    let bytes = std::fs::read(&child_file).unwrap();
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(Some(0)));
    let failed = worker
        .handle_delete_saved_session(&json!({ "sessionPath": child_file }))
        .await;
    crate::rlm_ledger::APPEND_WRITE_FAULT.with(|fault| fault.set(None));
    assert!(!failed.success, "{failed:?}");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("nothing was removed"),
        "{failed:?}"
    );
    assert_eq!(std::fs::read(&child_file).unwrap(), bytes);
    assert!(!tombstoned(&ledger));
    let deleted = worker
        .handle_delete_saved_session(&json!({ "sessionPath": child_file }))
        .await;
    assert!(deleted.success, "{deleted:?}");
    assert_eq!(
        deleted.data.as_ref().unwrap()["method"],
        json!("unlink"),
        "{deleted:?}"
    );
    assert!(!std::path::Path::new(&child_file).exists());
    assert!(tombstoned(&ledger));
    assert!(ledger
        .append_spawn(&spawn_input(&parent_file, &child_file))
        .is_err());
}

/// Case B (#42-adjacent safety): a session file whose lease another live
/// runtime holds is never removed under it. The worker answers the deferred
/// ownership refusal and keeps the file and the live edge.
#[tokio::test]
async fn a_worker_saved_delete_refuses_a_file_another_runtime_holds() {
    let Fixture {
        dir,
        supervisor,
        child_file,
        ledger,
        ..
    } = fixture().await;
    let worker = crate::worker::Worker::new(worker_config(&supervisor, &dir), None);
    let created = worker
        .dispatch("create", &json!({ "noSession": true, "cwd": dir.path() }))
        .await;
    assert!(created.success, "{created:?}");
    // Another runtime (a different worker or CLI process) owns the file.
    let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
        &supervisor.options.agent_dir,
        &supervisor.options.socket_path,
    )
    .unwrap();
    let held = authority
        .acquire(std::path::Path::new(&child_file))
        .expect("the other runtime holds the lease");
    let bytes = std::fs::read(&child_file).unwrap();
    let refused = worker
        .handle_delete_saved_session(&json!({ "sessionPath": child_file }))
        .await;
    assert!(!refused.success, "{refused:?}");
    let error = refused.error.unwrap_or_default();
    assert!(
        error.contains("Session delete was not committed; nothing was removed")
            && error.contains("Another runtime owns this session; deletion deferred"),
        "{error}"
    );
    assert!(
        !error.contains("recovering"),
        "distinct from the recovering-worker retry: {error}"
    );
    assert_eq!(
        std::fs::read(&child_file).unwrap(),
        bytes,
        "the transcript stays under its live holder"
    );
    assert!(!tombstoned(&ledger));
    drop(held);
}

fn worker_config(supervisor: &Supervisor, dir: &tempfile::TempDir) -> crate::worker::WorkerConfig {
    crate::worker::WorkerConfig {
        socket_path: dir.path().join("worker.sock"),
        supervisor_socket_path: supervisor.options.socket_path.clone(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "deleter".to_string(),
        agent_dir: supervisor.options.agent_dir.clone(),
        recovery_journal_path: dir.path().join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({ "responses": ["ack"] })),
    }
}
