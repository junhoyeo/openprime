//! Real artifact-store regressions; no workers, kernel, or system Trash.
#![cfg(unix)]

use super::tests::{read_job, write_job, write_session};
use super::*;
use pa_core::cron::JobStatus;

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
    assert!(error.is_none());
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
