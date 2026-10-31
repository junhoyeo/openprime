//! Real artifact-store regressions; no workers, kernel, or system Trash.
#![cfg(unix)]

use super::tests::{read_job, write_job, write_session};
use super::*;
use pa_core::cron::JobStatus;

fn assert_deferred(error: Option<anyhow::Error>) {
    let error = error.expect("live coverage must not report completed cleanup");
    assert_eq!(error.downcast_ref::<std::io::Error>().expect("typed retryable error").kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn alias_stop_cancels_raw_root_and_descendant_jobs_only() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let (root_id, root) = write_session(&alias.join("sessions"), None);
    let (child_id, child) = write_session(&alias.join("sessions"), None);
    let (other_id, other) = write_session(&alias.join("sessions"), None);
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger
        .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: root.to_string_lossy().into(),
            child: child.to_string_lossy().into(),
            depth: 1,
            name: "child".into(),
        })
        .unwrap();
    write_job(&root, &root_id, "root");
    write_job(&child, &child_id, "child");
    write_job(&other, &other_id, "other");
    // A malformed/stale row in the registered partition must not be selected by id.
    let partition = session_artifact_dir(&root, &root_id)
        .unwrap()
        .join("scheduled-jobs.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&partition).unwrap()).unwrap();
    let mut unrelated = state["jobs"][0].clone();
    unrelated["id"] = "unrelated-same-session-id".into();
    unrelated["sessionFile"] = other.to_string_lossy().to_string().into();
    state["jobs"].as_array_mut().unwrap().push(unrelated);
    std::fs::write(&partition, serde_json::to_vec(&state).unwrap()).unwrap();

    let (count, error) = finalize_archived_stop(&agent, &sessions, &root, &HashSet::new());
    assert!(error.is_none());
    assert_eq!(count, 2);
    assert_eq!(read_job(&child, &child_id).status, JobStatus::Cancelled);
    assert_eq!(read_job(&other, &other_id).status, JobStatus::Active);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&partition).unwrap()).unwrap();
    let root_job = state["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == "root")
        .unwrap();
    assert_eq!(root_job["status"], "cancelled");
    let unrelated = state["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == "unrelated-same-session-id")
        .unwrap();
    assert_eq!(unrelated["status"], "active");
    assert_eq!(
        read_session_info(&root).unwrap().state.as_deref(),
        Some("archived")
    );
}

#[test]
fn alias_live_descendant_keeps_entire_tree_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let (root_id, root) = write_session(&alias.join("sessions"), None);
    let (child_id, child) = write_session(&alias.join("sessions"), None);
    RlmSpawnLedger::new(&agent, &sessions, |_| {})
        .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: root.to_string_lossy().into(),
            child: child.to_string_lossy().into(),
            depth: 1,
            name: "child".into(),
        })
        .unwrap();
    write_job(&root, &root_id, "root");
    write_job(&child, &child_id, "child");
    let live = [canonical_session_path(&child).to_string_lossy().to_string()]
        .into_iter()
        .collect();
    let before = std::fs::read(&root).unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &root, &live);
    assert_deferred(error);
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&root).unwrap(), before);
    assert_eq!(read_job(&root, &root_id).status, JobStatus::Active);
    assert_eq!(read_job(&child, &child_id).status, JobStatus::Active);
}

#[test]
fn missing_root_keeps_alias_identity_for_existing_schedule_partition() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let (id, file) = write_session(&alias.join("sessions"), None);
    write_job(&file, &id, "root");
    std::fs::remove_file(&file).unwrap();
    assert_eq!(
        cancel_scheduled_jobs_for_tree(&agent, &sessions, &file, &HashSet::new()).unwrap(),
        1
    );
    assert_eq!(read_job(&file, &id).status, JobStatus::Cancelled);
}

fn create_artifact_job(path: &Path, id: &str, label: &str) -> pa_core::cron::AgentCronJob {
    // The real worker binding uses this raw path to register its partition.
    let store = AgentCronJobStore::for_session_artifacts();
    store.register_session_artifact(id, &session_artifact_dir(path, id).unwrap());
    create_store_job(&store, path, id, label)
}

fn create_store_job(store: &AgentCronJobStore, path: &Path, id: &str, label: &str) -> pa_core::cron::AgentCronJob {
    store.create(&pa_core::cron::store::CreateAgentCronJobInput {
        active_session_id: format!("live-{id}"),
        session_id: id.into(), session_file: path.to_string_lossy().into(),
        cwd: "/synthetic-work".into(), label: Some(label.into()),
        prompt: "synthetic heartbeat".into(), schedule_text: "every 2m".into(),
        source: Some("rlm_heartbeat".into()), runtime_kind: Some("subagent".into()),
        now: Some(1_700_000_000_000), ..Default::default()
    }).unwrap()
}

#[test]
fn sessions_directory_alias_cancels_raw_and_canonical_partitions_without_merging_them() {
    for canonical_command in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let external_sessions = temp.path().join("external/sessions");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&external_sessions).unwrap();
        let sessions = agent.join("sessions");
        std::os::unix::fs::symlink(&external_sessions, &sessions).unwrap();
        let (root_id, root) = write_session(&sessions, None);
        let (child_id, child) = write_session(&sessions, None);
        let (other_id, other) = write_session(&sessions, None);
        RlmSpawnLedger::new(&agent, &sessions, |_| {}).append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-1".into(), parent: root.to_string_lossy().into(),
            child: child.to_string_lossy().into(), depth: 1, name: "child".into(),
        }).unwrap();
        let raw_job = create_artifact_job(&root, &root_id, "raw-root");
        create_artifact_job(&child, &child_id, "raw-child");
        create_artifact_job(&other, &other_id, "unrelated");
        // Raw and canonical workers may have left distinct partitions for the
        // same transcript. Cancel each in place; never merge or relocate it.
        let canonical_root = canonical_session_path(&root);
        let canonical_job = create_artifact_job(&canonical_root, &root_id, "canonical-root");
        let raw_partition = session_artifact_dir(&root, &root_id).unwrap();
        let canonical_partition = session_artifact_dir(&canonical_root, &root_id).unwrap();
        assert_ne!(raw_partition, canonical_partition);
        std::fs::write(raw_partition.join("snapshot.keep"), b"raw-snapshot").unwrap();
        std::fs::write(canonical_partition.join("snapshot.keep"), b"canonical-snapshot").unwrap();
        let command_path = if canonical_command { &canonical_root } else { &root };
        let covered = [canonical_session_path(&child).to_string_lossy().to_string()].into_iter().collect();
        let root_before = std::fs::read(&root).unwrap();
        let raw_before = std::fs::read(raw_partition.join("scheduled-jobs.json")).unwrap();
        let canonical_before = std::fs::read(canonical_partition.join("scheduled-jobs.json")).unwrap();
        let (count, error) = finalize_archived_stop(&agent, &sessions, command_path, &covered);
        assert_deferred(error);
        assert_eq!(count, 0);
        assert_eq!(std::fs::read(&root).unwrap(), root_before);
        assert_eq!(std::fs::read(raw_partition.join("scheduled-jobs.json")).unwrap(), raw_before);
        assert_eq!(std::fs::read(canonical_partition.join("scheduled-jobs.json")).unwrap(), canonical_before);
        assert_eq!(read_job(&child, &child_id).status, JobStatus::Active);

        let (count, error) = finalize_archived_stop(&agent, &sessions, command_path, &HashSet::new());
        assert!(error.is_none());
        assert_eq!(count, 3, "canonical command: {canonical_command}");
        assert_eq!(read_job(&root, &root_id).status, JobStatus::Cancelled);
        assert_eq!(read_job(&child, &child_id).status, JobStatus::Cancelled);
        assert_eq!(read_job(&canonical_root, &root_id).status, JobStatus::Cancelled);
        assert_eq!(read_job(&root, &root_id).id, raw_job.id);
        assert_eq!(read_job(&canonical_root, &root_id).id, canonical_job.id);
        assert_eq!(read_job(&other, &other_id).status, JobStatus::Active);
        assert_eq!(std::fs::read(raw_partition.join("snapshot.keep")).unwrap(), b"raw-snapshot");
        assert_eq!(std::fs::read(canonical_partition.join("snapshot.keep")).unwrap(), b"canonical-snapshot");
        assert_eq!(read_session_info(&root).unwrap().state.as_deref(), Some("archived"));
        assert!(root.is_file() && child.is_file() && other.is_file());
    }
}

#[test]
fn sessions_directory_alias_missing_root_still_cancels_its_raw_partition() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let external_sessions = temp.path().join("external/sessions");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&external_sessions).unwrap();
    let sessions = agent.join("sessions");
    std::os::unix::fs::symlink(&external_sessions, &sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    create_artifact_job(&file, &id, "root");
    std::fs::remove_file(&file).unwrap();
    assert_eq!(cancel_scheduled_jobs_for_tree(&agent, &sessions, &file, &HashSet::new()).unwrap(), 1);
    assert_eq!(read_job(&file, &id).status, JobStatus::Cancelled);
}

#[test]
fn canonical_selection_does_not_widen_through_lexical_symlink_parent_components() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    let external_sessions = temp.path().join("external/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(external_sessions.join("nested")).unwrap();
    let (root_id, root) = write_session(&external_sessions, None);
    let (_, unrelated_original) = write_session(&sessions, None);
    let unrelated = sessions.join(root.file_name().unwrap());
    std::fs::rename(unrelated_original, &unrelated).unwrap();
    std::os::unix::fs::symlink(external_sessions.join("nested"), sessions.join("link")).unwrap();
    let selected_raw = sessions.join("link/..").join(root.file_name().unwrap());
    assert_eq!(canonical_session_path(&selected_raw), canonical_session_path(&root));
    assert_ne!(canonical_session_path(&selected_raw), canonical_session_path(&unrelated));
    // A previously registered canonical partition can contain legacy/stale
    // raw path bindings. Real store creation preserves those serialized keys.
    let store = AgentCronJobStore::for_session_artifacts();
    store.register_session_artifact(&root_id, &session_artifact_dir(&root, &root_id).unwrap());
    let selected = create_store_job(&store, &selected_raw, &root_id, "selected");
    let other = create_store_job(&store, &unrelated, &root_id, "stale-unrelated-binding");
    let unrelated_before = std::fs::read(&unrelated).unwrap();
    let jobs_path = session_artifact_dir(&root, &root_id).unwrap().join("scheduled-jobs.json");
    let mut raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&jobs_path).unwrap()).unwrap();
    raw["futureMetadata"] = serde_json::json!({"keep": [1, 2, 3]});
    for row in raw["jobs"].as_array_mut().unwrap() {
        row["futureJobField"] = serde_json::json!({"preserve": row["id"]});
    }
    raw["dispatches"] = serde_json::json!([{
        "id": "dispatch-1", "jobId": other.id, "claimedAt": "2026-01-01T00:00:00Z",
        "scheduledFor": "2026-01-01T00:01:00Z", "futureDispatchField": "keep"
    }]);
    let original_raw = raw.clone();
    std::fs::write(&jobs_path, serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(cancel_scheduled_jobs_for_tree(&agent, &sessions, &root, &HashSet::new()).unwrap(), 1);
    let jobs = store.list();
    assert_eq!(jobs.iter().find(|job| job.id == selected.id).unwrap().status, JobStatus::Cancelled);
    assert_eq!(jobs.iter().find(|job| job.id == other.id).unwrap(), &other);
    assert_eq!(std::fs::read(&unrelated).unwrap(), unrelated_before);
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&jobs_path).unwrap()).unwrap();
    let unrelated_row = |value: &serde_json::Value| value["jobs"].as_array().unwrap().iter()
        .find(|row| row["id"] == other.id).unwrap().clone();
    assert_eq!(unrelated_row(&after), unrelated_row(&original_raw));
    assert_eq!(after["dispatches"], original_raw["dispatches"]);
    assert_eq!(after["futureMetadata"], original_raw["futureMetadata"]);
    assert_eq!(after["jobs"].as_array().unwrap().iter().find(|row| row["id"] == selected.id).unwrap()["futureJobField"],
        original_raw["jobs"].as_array().unwrap().iter().find(|row| row["id"] == selected.id).unwrap()["futureJobField"]);
}

#[test]
fn unreadable_descendant_metadata_cannot_hide_live_tree_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (root_id, root) = write_session(&sessions, None);
    let (_, child) = write_session(&sessions, None);
    let (_, grandchild) = write_session(&sessions, None);
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    for (id, parent, target, depth) in [("child", &root, &child, 1), ("grandchild", &child, &grandchild, 2)] {
        ledger.append_spawn(&crate::rlm_ledger::RlmSpawnInput { child_id:id.into(), parent:parent.to_string_lossy().into(), child:target.to_string_lossy().into(), depth, name:id.into() }).unwrap();
    }
    let original_job = create_artifact_job(&root, &root_id, "root");
    let before = std::fs::read(&root).unwrap();
    // A malformed transcript is a real read_session_info failure on every OS,
    // without depending on root privileges bypassing a chmod fixture.
    std::fs::write(&child, b"unreadable-session-metadata\n").unwrap();
    assert!(read_session_info(&child).is_none());
    for covered in [&child, &grandchild] {
        let live = [canonical_session_path(covered).to_string_lossy().to_string()].into_iter().collect();
        let (count, error) = finalize_archived_stop(&agent, &sessions, &root, &live);
        assert_deferred(error);
        assert_eq!(count, 0);
        assert_eq!(std::fs::read(&root).unwrap(), before);
        assert_eq!(read_job(&root, &root_id), original_job);
    }
}

#[test]
fn unknown_nonlive_descendant_identity_refuses_root_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (root_id, root) = write_session(&sessions, None);
    let (_, child) = write_session(&sessions, None);
    RlmSpawnLedger::new(&agent, &sessions, |_| {}).append_spawn(&crate::rlm_ledger::RlmSpawnInput { child_id:"child".into(), parent:root.to_string_lossy().into(), child:child.to_string_lossy().into(), depth:1, name:"child".into() }).unwrap();
    let original_job = create_artifact_job(&root, &root_id, "root");
    let before = std::fs::read(&root).unwrap();
    std::fs::write(&child, b"unreadable-session-metadata\n").unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &root, &HashSet::new());
    assert!(error.is_some());
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&root).unwrap(), before);
    assert_eq!(read_job(&root, &root_id), original_job);
}

#[test]
fn corrupt_schedule_partition_is_not_successfully_archived_as_empty() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    create_artifact_job(&file, &id, "root");
    let before = std::fs::read(&file).unwrap();
    let jobs = session_artifact_dir(&file, &id).unwrap().join("scheduled-jobs.json");
    let corrupt = b"{unreadable-scheduled-state}\n";
    std::fs::write(&jobs, corrupt).unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &file, &HashSet::new());
    assert!(error.is_some());
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(std::fs::read(&jobs).unwrap(), corrupt);
}

#[test]
fn malformed_job_row_keeps_entire_partition_and_root_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    create_artifact_job(&file, &id, "root");
    let before = std::fs::read(&file).unwrap();
    let jobs = session_artifact_dir(&file, &id).unwrap().join("scheduled-jobs.json");
    let mut raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&jobs).unwrap()).unwrap();
    raw["jobs"].as_array_mut().unwrap().push(serde_json::json!({"id":"unknown-row"}));
    let bytes = serde_json::to_vec(&raw).unwrap();
    std::fs::write(&jobs, &bytes).unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &file, &HashSet::new());
    assert!(error.is_some());
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(std::fs::read(&jobs).unwrap(), bytes);
}

#[test]
fn unresolved_job_alias_refuses_archival_instead_of_assuming_empty_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    let dangling = temp.path().join("dangling");
    std::os::unix::fs::symlink(temp.path().join("missing-target"), &dangling).unwrap();
    let store = AgentCronJobStore::for_session_artifacts();
    let partition = session_artifact_dir(&file, &id).unwrap();
    store.register_session_artifact(&id, &partition);
    create_store_job(&store, &dangling.join("child.jsonl"), &id, "unknown-binding");
    let before = std::fs::read(&file).unwrap();
    let jobs = partition.join("scheduled-jobs.json");
    let job_bytes = std::fs::read(&jobs).unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &file, &HashSet::new());
    assert!(error.is_some());
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(std::fs::read(&jobs).unwrap(), job_bytes);
}

fn fixture_resident(directory: &Path, file: &Path, name: &str) -> Arc<ResidentWorker> {
    let descriptor = serde_json::from_value(serde_json::json!({
        "version": 2, "workerId": name, "pid": 0,
        "socketPath": directory.join(format!("{name}.sock")),
        "recoveryJournalPath": directory.join(format!("{name}.journal")),
        "supervisorSocketPath": directory.join("daemon.sock"),
        "authenticationToken": "synthetic-token", "rootActiveSessionId": name,
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z",
        "lifecycle": "ready", "createCommand": {}, "consecutiveFailures": 0, "sessionFile": file
    })).unwrap();
    ResidentWorker::new(name.into(), descriptor, directory.join(format!("{name}.descriptor.json")))
}

enum ChildMetadata {
    Readable,
    Malformed,
}

#[tokio::test]
async fn worker_finalize_retains_artifacts_for_unreadable_live_descendant() {
    assert_live_child_retains_artifacts(ChildMetadata::Malformed).await;
}

#[tokio::test]
async fn worker_finalize_retains_artifacts_for_readable_live_descendant() {
    assert_live_child_retains_artifacts(ChildMetadata::Readable).await;
}

async fn assert_live_child_retains_artifacts(metadata: ChildMetadata) {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (root_id, root) = write_session(&sessions, None);
    let (_, child) = write_session(&sessions, None);
    RlmSpawnLedger::new(&agent, &sessions, |_| {}).append_spawn(&crate::rlm_ledger::RlmSpawnInput {
        child_id:"child".into(), parent:root.to_string_lossy().into(), child:child.to_string_lossy().into(), depth:1, name:"child".into()
    }).unwrap();
    create_artifact_job(&root, &root_id, "root");
    let artifacts = session_artifact_dir(&root, &root_id).unwrap();
    std::fs::write(artifacts.join("snapshot.keep"), b"retained-snapshot").unwrap();
    let root_before = std::fs::read(&root).unwrap();
    let jobs_before = std::fs::read(artifacts.join("scheduled-jobs.json")).unwrap();
    if matches!(metadata, ChildMetadata::Malformed) {
        std::fs::write(&child, b"unreadable-session-metadata\n").unwrap();
    }
    let supervisor = Arc::new(Supervisor::new(crate::supervisor::SupervisorOptions {
        socket_path: temp.path().join("daemon.sock"), agent_dir: agent
    }).unwrap());
    let stopped = fixture_resident(temp.path(), &root, "stopped");
    supervisor.registry.insert(fixture_resident(temp.path(), &child, "live-child")).await;
    let deleted = DeletedChild { child_id:"deleted-root".into() };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.finalize_worker_stop(&stopped, Some(&deleted))).await.unwrap();
    assert!(!result);
    assert_eq!(std::fs::read(&root).unwrap(), root_before);
    assert_eq!(std::fs::read(artifacts.join("scheduled-jobs.json")).unwrap(), jobs_before);
    assert_eq!(std::fs::read(artifacts.join("snapshot.keep")).unwrap(), b"retained-snapshot");
}

#[tokio::test]
async fn worker_finalize_retains_artifacts_when_schedule_state_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    create_artifact_job(&file, &id, "root");
    let artifacts = session_artifact_dir(&file, &id).unwrap();
    std::fs::write(artifacts.join("snapshot.keep"), b"retained-snapshot").unwrap();
    let before = std::fs::read(&file).unwrap();
    std::fs::write(artifacts.join("scheduled-jobs.json"), b"unknown-state\n").unwrap();
    let supervisor = Arc::new(Supervisor::new(crate::supervisor::SupervisorOptions {
        socket_path: temp.path().join("daemon.sock"), agent_dir: agent
    }).unwrap());
    let stopped = fixture_resident(temp.path(), &file, "stopped");
    let deleted = DeletedChild { child_id:"deleted-root".into() };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.finalize_worker_stop(&stopped, Some(&deleted))).await.unwrap();
    assert!(!result);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(std::fs::read(artifacts.join("scheduled-jobs.json")).unwrap(), b"unknown-state\n");
    assert_eq!(std::fs::read(artifacts.join("snapshot.keep")).unwrap(), b"retained-snapshot");
}

#[tokio::test]
async fn worker_finalize_completes_a_fully_stopped_tree() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (root_id, root) = write_session(&sessions, None);
    let (child_id, child) = write_session(&sessions, None);
    RlmSpawnLedger::new(&agent, &sessions, |_| {}).append_spawn(&crate::rlm_ledger::RlmSpawnInput {
        child_id:"child".into(), parent:root.to_string_lossy().into(), child:child.to_string_lossy().into(), depth:1, name:"child".into()
    }).unwrap();
    create_artifact_job(&root, &root_id, "root");
    create_artifact_job(&child, &child_id, "child");
    let artifacts = session_artifact_dir(&root, &root_id).unwrap();
    std::fs::write(artifacts.join("snapshot.keep"), b"deleted-snapshot").unwrap();
    let supervisor = Arc::new(Supervisor::new(crate::supervisor::SupervisorOptions {
        socket_path: temp.path().join("daemon.sock"), agent_dir: agent
    }).unwrap());
    let stopped = fixture_resident(temp.path(), &root, "stopped");
    let deleted = DeletedChild { child_id:"deleted-root".into() };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.finalize_worker_stop(&stopped, Some(&deleted))).await.unwrap();
    assert!(result);
    assert_eq!(read_session_info(&root).unwrap().state.as_deref(), Some("archived"));
    assert_eq!(read_job(&child, &child_id).status, JobStatus::Cancelled);
    assert!(!artifacts.exists());
}

#[test]
fn malformed_ledger_topology_refuses_cleanup_but_display_reads_stay_lenient() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let (id, file) = write_session(&sessions, None);
    let (_, child) = write_session(&sessions, None);
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger.append_spawn(&crate::rlm_ledger::RlmSpawnInput { child_id:"child".into(), parent:file.to_string_lossy().into(), child:child.to_string_lossy().into(), depth:1, name:"child".into() }).unwrap();
    create_artifact_job(&file, &id, "root");
    let before = std::fs::read(&file).unwrap();
    let jobs = session_artifact_dir(&file, &id).unwrap().join("scheduled-jobs.json");
    let job_bytes = std::fs::read(&jobs).unwrap();
    std::fs::OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap().write_all(b"{unknown-broken-topology}\n").unwrap();
    let ledger_before = std::fs::read(ledger.ledger_path()).unwrap();
    let (count, error) = finalize_archived_stop(&agent, &sessions, &file, &HashSet::new());
    assert!(error.is_some());
    assert_eq!(count, 0);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(std::fs::read(&jobs).unwrap(), job_bytes);
    assert_eq!(std::fs::read(ledger.ledger_path()).unwrap(), ledger_before);
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
}
