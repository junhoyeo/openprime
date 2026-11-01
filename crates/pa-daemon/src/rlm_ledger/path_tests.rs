//! Path identity must not split the append-only topology history.
#![cfg(unix)]

use super::*;

#[test]
fn ledger_append_before_directory_creation_replays_through_both_aliases_after_creation() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    fs::create_dir(&agent).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let raw_sessions = alias.join("nested/sessions");
    let real_sessions = agent.join("nested/sessions");
    let before = RlmSpawnLedger::new(&agent, &raw_sessions, |_| {});
    let parent = raw_sessions
        .join("parent.jsonl")
        .to_string_lossy()
        .to_string();
    let child = raw_sessions
        .join("child.jsonl")
        .to_string_lossy()
        .to_string();
    before
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent,
            child: child.clone(),
            depth: 1,
            name: "before".into(),
        })
        .unwrap();
    let bytes = fs::read(before.ledger_path()).unwrap();
    fs::create_dir_all(&real_sessions).unwrap();
    let after = RlmSpawnLedger::new(&agent, &real_sessions, |_| {});
    let alias_reader = RlmSpawnLedger::new(&agent, &raw_sessions, |_| {});
    assert_eq!(after.edges(true).unwrap(), before.edges(true).unwrap());
    assert_eq!(alias_reader.edges(true).unwrap().len(), 1);
    assert_eq!(fs::read(after.ledger_path()).unwrap(), bytes);
    after.append_rename("sub-1", &child, "after").unwrap();
    assert_eq!(before.edges(true).unwrap()[0].name, "after");
    assert_eq!(alias_reader.edges(true).unwrap()[0].name, "after");
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
    assert!(RlmSpawnLedger::new(&agent, &agent.join("other"), |_| {})
        .edges(true)
        .unwrap()
        .is_empty());
}

#[test]
fn legacy_raw_hash_history_survives_directory_creation_and_new_appends() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    fs::create_dir(&agent).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let raw_sessions = alias.join("sessions");
    let real_sessions = agent.join("sessions");
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&raw_sessions.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let bytes = format!(
        "{}\n{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":raw_sessions}),
        json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":"sub-1","parent":raw_sessions.join("parent.jsonl"),"child":raw_sessions.join("child.jsonl"),"depth":1,"name":"legacy"})
    );
    fs::write(&legacy, bytes.as_bytes()).unwrap();
    fs::create_dir(&real_sessions).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &real_sessions, |_| {});
    assert_eq!(ledger.edges(true).unwrap().len(), 1);
    assert_eq!(fs::read(&legacy).unwrap(), bytes.as_bytes());
    ledger
        .append_rename(
            "sub-1",
            &real_sessions.join("child.jsonl").to_string_lossy(),
            "retained",
        )
        .unwrap();
    let alias_reader = RlmSpawnLedger::new(&agent, &raw_sessions, |_| {});
    assert_eq!(alias_reader.edges(true).unwrap()[0].name, "retained");
    let after = fs::read(&legacy).unwrap();
    assert!(after.starts_with(bytes.as_bytes()));
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), after);
}

fn write_legacy_header(agent: &Path, scope: &Path) -> PathBuf {
    let path = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            "{}\n",
            json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":scope})
        ),
    )
    .unwrap();
    path
}

#[test]
fn a_late_conflicting_history_refuses_reads_and_appends_without_modifying_either() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "original".into(),
        })
        .unwrap();
    let original = fs::read(ledger.ledger_path()).unwrap();
    let legacy = write_legacy_header(&agent, &alias.join("sessions"));
    let other = fs::read(&legacy).unwrap();
    assert!(ledger
        .edges(true)
        .unwrap_err()
        .to_string()
        .contains("multiple matching"));
    assert!(ledger
        .append_rename(
            "sub-1",
            &sessions.join("c.jsonl").to_string_lossy(),
            "must-not-write"
        )
        .is_err());
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), original);
    assert_eq!(fs::read(&legacy).unwrap(), other);
}

#[test]
fn atomic_replacement_of_selected_legacy_path_is_seen_by_next_read_and_append() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let legacy = write_legacy_header(&agent, &alias.join("sessions"));
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    let child = sessions.join("c.jsonl").to_string_lossy().to_string();
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: child.clone(),
            depth: 1,
            name: "original".into(),
        })
        .unwrap();
    assert_eq!(ledger.ledger_path(), legacy);
    let original = fs::read(&legacy).unwrap();
    let mut replacement = original.clone();
    replacement.extend_from_slice(
        format!(
            "{}\n",
            json!({"v":1,"op":"rename","at":"t","childId":"sub-1","child":child,"name":"replaced"})
        )
        .as_bytes(),
    );
    let staged = temp.path().join("replacement");
    fs::write(&staged, replacement).unwrap();
    fs::rename(staged, &legacy).unwrap();
    assert_eq!(ledger.edges(true).unwrap()[0].name, "replaced");
    ledger
        .append_rename("sub-1", &child, "after-replacement")
        .unwrap();
    let reopened = RlmSpawnLedger::new(&agent, &alias.join("sessions"), |_| {});
    assert_eq!(reopened.edges(true).unwrap()[0].name, "after-replacement");
    assert_eq!(reopened.ledger_path(), legacy);
    assert!(fs::read(&legacy).unwrap().starts_with(&original));
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
    assert!(!rlm_ledger_path(&agent, &sessions).exists());
}

#[test]
fn malformed_or_symlink_ownership_never_creates_a_new_empty_history() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let foreign = temp.path().join("foreign-history");
    fs::write(&foreign, b"private-outside-content").unwrap();
    let ledger_dir = agent.join(RLM_LEDGER_DIR);
    fs::create_dir(&ledger_dir).unwrap();
    let ambiguous = ledger_dir.join("0123456789abcdef.jsonl");
    std::os::unix::fs::symlink(&foreign, &ambiguous).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    // Case A, unsafe identity (symlinked ledger entry): display refuses too,
    // without reading the foreign target or creating a history.
    assert!(ledger.edges(true).is_err());
    assert!(!rlm_ledger_path(&agent, &sessions).exists());
    assert_eq!(fs::read(&foreign).unwrap(), b"private-outside-content");
    fs::remove_file(&ambiguous).unwrap();
    fs::write(&ambiguous, b"not-an-owner-header\n").unwrap();
    // Case B, safe regular file with an invalid header under ANOTHER filename:
    // the approved blocker rule. No selected history, so display is empty
    // without an error; authority, admission and append refuse.
    assert!(ledger.edges(true).unwrap().is_empty());
    assert!(ledger.strict_edges().is_err());
    assert!(ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "blocked".into()
        })
        .is_err());
    assert!(ledger
        .append_rename("sub", "/synthetic-c.jsonl", "no-write")
        .is_err());
    assert_eq!(fs::read(&ambiguous).unwrap(), b"not-an-owner-header\n");
    assert!(!rlm_ledger_path(&agent, &sessions).exists());
}

#[test]
fn unrelated_valid_ledger_stays_separate_and_scan_limit_is_not_a_miss() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let unrelated = write_legacy_header(&agent, &agent.join("other"));
    let bytes = fs::read(&unrelated).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.edges(true).unwrap().is_empty());
    assert_eq!(fs::read(&unrelated).unwrap(), bytes);
    for index in 0..4096 {
        write_legacy_header(&agent, &agent.join(format!("scope-{index}")));
    }
    assert!(ledger
        .edges(true)
        .unwrap_err()
        .to_string()
        .contains("exceeds 4096 entries"));
    assert!(!rlm_ledger_path(&agent, &sessions).exists());
    assert_eq!(fs::read(&unrelated).unwrap(), bytes);
}

#[tokio::test]
async fn scope_lock_serializes_alias_selection_and_creation_across_processes() {
    const CHILD_ROOT: &str = "PA_LEDGER_SCOPE_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let agent = root.join("agent");
        let alias_sessions = root.join("alias/sessions");
        let ledger = RlmSpawnLedger::new(&agent, &alias_sessions, |_| {});
        assert!(ledger
            .append_rename("sub", "/synthetic-child.jsonl", "blocked")
            .is_err());
        assert!(!rlm_ledger_path(&agent, &alias_sessions).exists());
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    std::os::unix::fs::symlink(&agent, temp.path().join("alias")).unwrap();
    let guard = storage_path::StorageGuard::acquire(&agent, &sessions).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rlm_ledger::path_tests::scope_lock_serializes_alias_selection_and_creation_across_processes", "--nocapture"])
        .env(CHILD_ROOT, temp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn().unwrap();
    let pid = child.id().unwrap();
    match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await {
        Ok(result) => {
            result.unwrap();
        }
        Err(error) => {
            child.kill().await.unwrap();
            panic!("owned ledger child {pid} timed out and was reaped: {error}");
        }
    }
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "child {pid}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    drop(guard);
    // The observed release, not a timed retry, permits the next write.
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger
        .append_rename("sub", "/synthetic-child.jsonl", "unblocked")
        .unwrap();
    assert!(ledger.ledger_path().is_file());
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
}

#[test]
fn concurrent_alias_appends_preserve_every_acknowledged_record_and_busy_is_retryable() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&agent, &alias).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    let child = sessions.join("c.jsonl").to_string_lossy().to_string();
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: child.clone(),
            depth: 1,
            name: "initial".into(),
        })
        .unwrap();
    let alias_sessions = alias.join("sessions");
    let barrier = std::sync::Barrier::new(8);
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let writer = RlmSpawnLedger::new(
                    &agent,
                    if index % 2 == 0 {
                        &sessions
                    } else {
                        &alias_sessions
                    },
                    |_| {},
                );
                let child = &child;
                let barrier = &barrier;
                scope.spawn(move || {
                    let name = format!("rename-{index}");
                    barrier.wait();
                    let result = writer.append_rename("sub-1", child, &name);
                    (name, result)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut acknowledged = std::collections::BTreeSet::new();
    for (name, outcome) in outcomes {
        match outcome {
            Ok(()) => {
                acknowledged.insert(name);
            }
            Err(error) => {
                assert_eq!(
                    error.downcast_ref::<std::io::Error>().unwrap().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                assert!(error.to_string().contains("retry this operation"));
            }
        }
    }
    assert!(!acknowledged.is_empty());
    let content = fs::read_to_string(ledger.ledger_path()).unwrap();
    let persisted: std::collections::BTreeSet<_> = content
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["op"] == "rename")
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(persisted, acknowledged);
    assert_eq!(ledger.edges(true).unwrap().len(), 1);
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn symlink_traversal_cannot_create_a_second_authority_for_the_same_physical_root() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let physical_parent = temp.path().join("physical");
    let lexical_parent = temp.path().join("lexical");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(physical_parent.join("nested")).unwrap();
    fs::create_dir_all(&lexical_parent).unwrap();
    std::os::unix::fs::symlink(physical_parent.join("nested"), lexical_parent.join("link"))
        .unwrap();
    let ambiguous = lexical_parent.join("link/../sessions");
    let physical = physical_parent.join("sessions");
    for create_root in [false, true] {
        if create_root {
            fs::create_dir_all(&physical).unwrap();
        }
        let ledger = RlmSpawnLedger::new(&agent, &physical, |_| {});
        ledger
            .append_spawn(&RlmSpawnInput {
                child_id: format!("sub-{create_root}"),
                parent: physical.join("parent.jsonl").to_string_lossy().into(),
                child: physical
                    .join(format!("child-{create_root}.jsonl"))
                    .to_string_lossy()
                    .into(),
                depth: 1,
                name: "child".into(),
            })
            .unwrap();
        ledger
            .append_delete(
                &format!("sub-{create_root}"),
                &physical
                    .join(format!("child-{create_root}.jsonl"))
                    .to_string_lossy(),
                RlmLedgerDeleteReason::User,
            )
            .unwrap();
        let before = fs::read_dir(agent.join(RLM_LEDGER_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let selected = ledger.ledger_path().to_path_buf();
        let selected_bytes = fs::read(&selected).unwrap();
        let alias = RlmSpawnLedger::new(&agent, &ambiguous, |_| {});
        let error = alias.strict_edges().unwrap_err().to_string();
        assert!(error.contains("ambiguous sessions path traversal"));
        assert!(error.contains(&ambiguous.display().to_string()));
        assert!(error.contains("without `..` after a symlink"));
        assert!(alias
            .append_rename(
                &format!("sub-{create_root}"),
                "/synthetic-child.jsonl",
                "must-not-write"
            )
            .is_err());
        assert_eq!(
            fs::read_dir(agent.join(RLM_LEDGER_DIR))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(fs::read(&selected).unwrap(), selected_bytes);
    }
}

/// v12.5 crash recovery: under the namespace guard, with no ledger, an exact
/// own-stem, regular, self-owned seed scratch comes from a dead attempt. It is
/// moved aside into `rlm-ledger-retained/` with its exact bytes, never deleted.
/// With no legacy registry to import, the strict read publishes nothing; the
/// next append then publishes a new ledger whole (header plus that record).
#[test]
fn an_unpublished_seed_candidate_is_retained_aside_never_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let proposed = rlm_ledger_path(&agent, &sessions);
    fs::create_dir_all(proposed.parent().unwrap()).unwrap();
    let candidate = proposed.with_extension("seed-123-456");
    fs::write(&candidate, b"incomplete-unpublished-seed").unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.strict_edges().unwrap().is_empty());
    assert!(!candidate.exists(), "the dead scratch name is moved aside");
    let retained: Vec<PathBuf> = fs::read_dir(agent.join("rlm-ledger-retained"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(retained.len(), 1);
    assert_eq!(
        fs::read(&retained[0]).unwrap(),
        b"incomplete-unpublished-seed",
        "retained, never deleted"
    );
    assert!(
        !proposed.exists(),
        "nothing to seed, so no ledger is published by the read"
    );
    ledger
        .append_rename("sub", "/synthetic-child.jsonl", "renamed")
        .unwrap();
    let content = fs::read_to_string(&proposed).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"op\":\"meta\"") && lines[1].contains("\"op\":\"rename\""));
    assert_eq!(
        fs::read(&retained[0]).unwrap(),
        b"incomplete-unpublished-seed"
    );
}

/// Only an exact own-stem, regular, self-owned `seed-<digits>-<digits>`
/// scratch is recovered. An unknown own-stem scratch name or a symlinked one
/// is left in place, and authority still refuses ("retained seed candidate").
#[cfg(unix)]
#[test]
fn unknown_or_symlinked_seed_scratch_still_refuses_and_is_not_moved() {
    for case in ["unknown-name", "symlink"] {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let sessions = agent.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let proposed = rlm_ledger_path(&agent, &sessions);
        fs::create_dir_all(proposed.parent().unwrap()).unwrap();
        let entry = if case == "unknown-name" {
            let path = proposed.with_extension("seed-abc");
            fs::write(&path, b"unknown").unwrap();
            path
        } else {
            let target = temp.path().join("elsewhere");
            fs::write(&target, b"outside").unwrap();
            let path = proposed.with_extension("seed-1-1");
            std::os::unix::fs::symlink(&target, &path).unwrap();
            path
        };
        let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
        let error = ledger.strict_edges().unwrap_err().to_string();
        assert!(error.contains("retained seed candidate"), "{case}: {error}");
        assert!(
            entry.symlink_metadata().is_ok(),
            "{case}: entry left in place"
        );
        assert!(
            !agent.join("rlm-ledger-retained").exists(),
            "{case}: nothing moved"
        );
        assert!(!proposed.exists());
    }
}

fn tombstoned_ledger(agent: &Path, sessions: &Path) -> (PathBuf, Vec<u8>) {
    let ledger = RlmSpawnLedger::new(agent, sessions, |_| {});
    let child = sessions.join("child.jsonl").to_string_lossy().to_string();
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: sessions.join("parent.jsonl").to_string_lossy().into(),
            child: child.clone(),
            depth: 1,
            name: "child".into(),
        })
        .unwrap();
    ledger
        .append_delete("sub-1", &child, RlmLedgerDeleteReason::User)
        .unwrap();
    (
        ledger.ledger_path().to_path_buf(),
        fs::read(ledger.ledger_path()).unwrap(),
    )
}

fn assert_same_tombstone(agent: &Path, spelling: &Path, selected: &Path, bytes: &[u8]) {
    let reader = RlmSpawnLedger::new(agent, spelling, |_| {});
    let edges = reader.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].child_id, "sub-1");
    assert_eq!(edges[0].deleted, Some(RlmLedgerDeleteReason::User));
    assert_eq!(reader.ledger_path(), selected);
    assert_eq!(fs::read(selected).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn existing_symlinked_configured_root_converges_on_the_same_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let physical = temp.path().join("physical-sessions");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&physical).unwrap();
    let alias = temp.path().join("configured-sessions");
    std::os::unix::fs::symlink(&physical, &alias).unwrap();
    let (selected, bytes) = tombstoned_ledger(&agent, &alias);
    for spelling in [&alias, &physical] {
        assert_same_tombstone(&agent, spelling, &selected, &bytes);
    }
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
}

/// Scoped model of macOS `var -> private/var`. The literal system /var path is
/// covered only by the coordinator-owned handoff fixture, not by this test.
#[cfg(unix)]
#[test]
fn scoped_var_alias_missing_root_then_created_finds_the_same_legacy_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    fs::create_dir_all(temp.path().join("private/var/folders/user")).unwrap();
    fs::create_dir_all(&agent).unwrap();
    std::os::unix::fs::symlink(temp.path().join("private/var"), temp.path().join("var")).unwrap();
    let raw = temp.path().join("var/folders/user/sessions");
    let physical = temp.path().join("private/var/folders/user/sessions");
    // Shipping code hashed this missing root lexically: write that legacy bucket.
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&raw.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let child = raw.join("child.jsonl").to_string_lossy().to_string();
    let bytes = format!("{}\n{}\n{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":raw}),
        json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":"sub-1","parent":raw.join("parent.jsonl"),"child":child,"depth":1,"name":"child"}),
        json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":"sub-1","child":child,"reason":"user"})).into_bytes();
    fs::write(&legacy, &bytes).unwrap();
    assert_same_tombstone(&agent, &raw, &legacy, &bytes);
    fs::create_dir_all(&physical).unwrap();
    for spelling in [&raw, &physical] {
        assert_same_tombstone(&agent, spelling, &legacy, &bytes);
    }
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn legacy_lexical_history_blocks_plain_physical_reader_from_hiding_its_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let physical_parent = temp.path().join("physical");
    let lexical_parent = temp.path().join("lexical");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(physical_parent.join("nested")).unwrap();
    fs::create_dir_all(&lexical_parent).unwrap();
    std::os::unix::fs::symlink(physical_parent.join("nested"), lexical_parent.join("link"))
        .unwrap();
    let ambiguous = lexical_parent.join("link/../sessions");
    let lexical = lexical_parent.join("sessions");
    let physical = physical_parent.join("sessions");
    for create_root in [false, true] {
        if create_root {
            fs::create_dir_all(&physical).unwrap();
        }
        let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
            "{}.jsonl",
            crate::paths::hash_key(&lexical.to_string_lossy(), 16)
        ));
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let child = ambiguous.join("child.jsonl").to_string_lossy().to_string();
        let bytes = format!("{}\n{}\n{}\n",
            json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":lexical}),
            json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":"sub-1","parent":ambiguous.join("parent.jsonl"),"child":child,"depth":1,"name":"child"}),
            json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":"sub-1","child":child,"reason":"user"})).into_bytes();
        fs::write(&legacy, &bytes).unwrap();
        for spelling in [&ambiguous, &physical] {
            let ledger = RlmSpawnLedger::new(&agent, spelling, |_| {});
            assert!(ledger.strict_edges().is_err());
            assert!(ledger
                .append_spawn(&RlmSpawnInput {
                    child_id: "sub-1".into(),
                    parent: physical.join("parent.jsonl").to_string_lossy().into(),
                    child: physical.join("child.jsonl").to_string_lossy().into(),
                    depth: 1,
                    name: "revived".into()
                })
                .is_err());
        }
        assert_eq!(fs::read(&legacy).unwrap(), bytes);
        assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
        fs::remove_file(&legacy).unwrap();
    }
}

#[test]
fn plain_parent_component_without_symlink_preserves_existing_hash() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(agent.join("unused")).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let spelling = agent.join("unused/../sessions");
    let (selected, bytes) = tombstoned_ledger(&agent, &spelling);
    assert_eq!(selected, rlm_ledger_path(&agent, &sessions));
    assert_same_tombstone(&agent, &spelling, &selected, &bytes);
    assert_same_tombstone(&agent, &sessions, &selected, &bytes);
}

/// Coordinator-provided literal macOS `/var` fixture. Its alias is created only
/// inside the owned handoff tree and named by the guarded run environment.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires PA_NAMESPACE_VAR_FIXTURE from the coordinator-owned literal /var alias fixture"]
fn literal_var_fixture_missing_root_then_created_converges() {
    use std::os::unix::fs::MetadataExt;
    let manifest = std::env::var_os("PA_NAMESPACE_VAR_FIXTURE")
        .expect("selected literal /var fixture test requires PA_NAMESPACE_VAR_FIXTURE");
    let fixture: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    let raw = PathBuf::from(fixture["namespace_input"].as_str().unwrap());
    let alias = PathBuf::from(fixture["raw_alias"].as_str().unwrap());
    let target = PathBuf::from(fixture["resolved_target"].as_str().unwrap());
    let physical = target.join(raw.file_name().unwrap());
    let agent = target.join("agent");
    assert!(raw.starts_with("/var/") && raw.starts_with(&alias));
    assert_eq!(
        fs::canonicalize(&alias).unwrap(),
        fs::canonicalize(&target).unwrap()
    );
    assert_eq!(fixture["currently_missing"], true);
    assert!(
        fs::read_dir(&target).unwrap().next().is_none(),
        "literal /var fixture target must be fresh and empty"
    );
    let alias_metadata = fs::symlink_metadata(&alias).unwrap();
    let target_metadata = fs::metadata(&target).unwrap();
    let observed = json!({
        "alias_device": alias_metadata.dev(), "alias_inode": alias_metadata.ino(), "alias_uid": alias_metadata.uid(),
        "target_device": target_metadata.dev(), "target_inode": target_metadata.ino(), "target_uid": target_metadata.uid(),
        "target_mode": format!("{:#o}", target_metadata.mode() & 0o777), "target_empty_before": true,
    });
    for key in [
        "alias_device",
        "alias_inode",
        "alias_uid",
        "target_device",
        "target_inode",
        "target_uid",
        "target_mode",
    ] {
        assert_eq!(
            observed[key], fixture[key],
            "literal /var fixture {key} differs from its recorded identity"
        );
    }
    assert!(!physical.exists());
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&raw.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let child = raw.join("child.jsonl").to_string_lossy().to_string();
    let bytes = format!("{}\n{}\n{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":raw}),
        json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":"sub-1","parent":raw.join("parent.jsonl"),"child":child,"depth":1,"name":"child"}),
        json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":"sub-1","child":child,"reason":"user"})).into_bytes();
    {
        use std::io::Write;
        let mut legacy_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&legacy)
            .unwrap();
        legacy_file.write_all(&bytes).unwrap();
        legacy_file.sync_all().unwrap();
    }
    assert_same_tombstone(&agent, &raw, &legacy, &bytes);
    fs::create_dir_all(&physical).unwrap();
    assert_same_tombstone(&agent, &raw, &legacy, &bytes);
    assert_same_tombstone(&agent, &physical, &legacy, &bytes);
    println!(
        "literal-var-fixture-executed {}",
        json!({
            "raw_alias": alias, "namespace_input": raw, "resolved_target": target,
            "physical_sessions": fs::canonicalize(&physical).unwrap(), "recorded": {
                "alias_device": fixture["alias_device"], "alias_inode": fixture["alias_inode"], "alias_uid": fixture["alias_uid"],
                "target_device": fixture["target_device"], "target_inode": fixture["target_inode"], "target_uid": fixture["target_uid"],
                "target_mode": fixture["target_mode"]
            }, "observed": observed,
        })
    );
}

#[cfg(unix)]
#[test]
fn child_only_agent_artifact_history_blocks_custom_root_reader() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let custom_parent = temp.path().join("custom");
    let sessions = custom_parent.join("sessions");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let parent = sessions.join("parent-id.jsonl");
    fs::write(&parent, b"{}\n").unwrap();
    let child = agent.join("session-artifacts/parent-id/sub-1/child-id.jsonl");
    fs::create_dir_all(child.parent().unwrap()).unwrap();
    fs::write(&child, b"{}\n").unwrap();
    let unrelated_scope = temp.path().join("old-lexical/sessions");
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&unrelated_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let bytes = format!("{}\n{}\n{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":unrelated_scope}),
        json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":"sub-1","parent":"/old-unresolved-parent/parent-id.jsonl","child":child,"depth":1,"name":"child"}),
        json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":"sub-1","child":child,"reason":"user"})).into_bytes();
    fs::write(&legacy, &bytes).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains("legacy authority"));
    assert!(ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub-1".into(),
            parent: parent.to_string_lossy().into(),
            child: child.to_string_lossy().into(),
            depth: 1,
            name: "revived".into()
        })
        .is_err());
    assert_eq!(fs::read(&legacy).unwrap(), bytes);
}

#[test]
fn duplicate_key_foreign_record_refuses_instead_of_proving_unrelated() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("custom/sessions");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let foreign_scope = temp.path().join("foreign/sessions");
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&foreign_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let meta = json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":foreign_scope})
        .to_string();
    let duplicate = format!(
        r#"{{"v":1,"op":"delete","at":"2026-01-01T00:00:00Z","childId":"sub-1","child":"{}","child":"/unrelated/child.jsonl","reason":"user"}}"#,
        sessions.join("child.jsonl").display()
    );
    let bytes = format!("{meta}\n{duplicate}\n").into_bytes();
    fs::write(&legacy, &bytes).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.strict_edges().is_err());
    assert!(ledger
        .append_rename("sub-1", "/synthetic-child.jsonl", "blocked")
        .is_err());
    assert_eq!(fs::read(&legacy).unwrap(), bytes);
}

#[test]
fn artifact_delete_without_spawn_relation_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("custom/sessions");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    let child = agent.join("session-artifacts/unknown-parent/sub-1/child.jsonl");
    let foreign_scope = temp.path().join("foreign/sessions");
    let legacy = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&foreign_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let bytes = format!("{}\n{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":foreign_scope}),
        json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":"sub-1","child":child,"reason":"user"})).into_bytes();
    fs::write(&legacy, &bytes).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains("legacy authority"));
    assert_eq!(fs::read(&legacy).unwrap(), bytes);
}

fn write_history(agent: &Path, scope: &Path, records: &[Value]) -> (PathBuf, Vec<u8>) {
    use std::io::Write;
    let path = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = format!(
        "{}\n",
        json!({"v":1,"op":"meta","at":"2026-01-01T00:00:00Z","sessionsDir":scope})
    )
    .into_bytes();
    for record in records {
        bytes.extend_from_slice(format!("{record}\n").as_bytes());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(&bytes).unwrap();
    (path, bytes)
}

fn spawn_row(parent: &Path, child: &Path, id: &str) -> Value {
    json!({"v":1,"op":"spawn","at":"2026-01-01T00:00:00Z","childId":id,"parent":parent,"child":child,"depth":1,"name":id})
}

fn delete_row(child: &Path, id: &str) -> Value {
    json!({"v":1,"op":"delete","at":"2026-01-01T00:00:01Z","childId":id,"child":child,"reason":"user"})
}

#[test]
fn grandchild_tombstone_follows_parent_edge_from_selected_history() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("custom/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let parent = sessions.join("P.jsonl");
    let child = agent.join("session-artifacts/P/c/C.jsonl");
    let grandchild = agent.join("session-artifacts/C/g/G.jsonl");
    let selected = write_history(
        &agent,
        &replay::checked_scope(&sessions).unwrap(),
        &[spawn_row(&parent, &child, "c")],
    );
    let foreign = write_history(
        &agent,
        &temp.path().join("old/sessions"),
        &[
            spawn_row(&child, &grandchild, "g"),
            delete_row(&grandchild, "g"),
        ],
    );
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains("legacy authority"));
    assert!(ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "g".into(),
            parent: child.to_string_lossy().into(),
            child: grandchild.to_string_lossy().into(),
            depth: 2,
            name: "revived".into()
        })
        .is_err());
    assert_eq!(fs::read(&selected.0).unwrap(), selected.1);
    assert_eq!(fs::read(&foreign.0).unwrap(), foreign.1);
}

#[test]
fn sessions_root_sibling_artifact_delete_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("custom/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let child = temp
        .path()
        .join("custom/session-artifacts/root/child.jsonl");
    let foreign = write_history(
        &agent,
        &temp.path().join("old/sessions"),
        &[delete_row(&child, "sub")],
    );
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.strict_edges().is_err());
    assert_eq!(fs::read(&foreign.0).unwrap(), foreign.1);
}

#[test]
fn unrelated_foreign_root_with_its_own_artifact_children_does_not_refuse() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    let other = temp.path().join("other/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&other).unwrap();
    let parent = other.join("O.jsonl");
    let child = agent.join("session-artifacts/O/x/X.jsonl");
    let foreign = write_history(
        &agent,
        &replay::checked_scope(&other).unwrap(),
        &[spawn_row(&parent, &child, "x"), delete_row(&child, "x")],
    );
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.strict_edges().unwrap().is_empty());
    assert_eq!(fs::read(&foreign.0).unwrap(), foreign.1);
}

#[test]
fn duplicate_sessions_dir_header_refuses_before_selection_read_or_append() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let scope = replay::checked_scope(&sessions).unwrap();
    let path = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = format!(
        r#"{{"v":1,"op":"meta","at":"t","sessionsDir":"/unrelated/sessions","sessionsDir":{}}}"#,
        serde_json::to_string(&scope).unwrap()
    ) + "\n";
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap()
        .write_all(bytes.as_bytes())
        .unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.strict_edges().is_err());
    assert!(ledger
        .append_rename("sub", "/synthetic-child.jsonl", "blocked")
        .is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
}

#[test]
fn over_budget_foreign_history_refuses_instead_of_proving_unrelated() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let foreign_scope = temp.path().join("old/sessions");
    let path = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&foreign_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({"v":1,"op":"meta","at":"t","sessionsDir":foreign_scope})
    )
    .unwrap();
    file.set_len(RLM_LEDGER_MAX_BYTES + 1).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains(&path.display().to_string()));
    assert!(ledger
        .append_rename("sub", "/synthetic-child.jsonl", "blocked")
        .is_err());
}

#[test]
fn malformed_selected_history_keeps_display_policy_without_foreign_candidates() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&logs);
    let ledger = RlmSpawnLedger::new(&agent, &sessions, move |line| {
        sink.lock().unwrap().push(line.to_string());
    });
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "child".into(),
        })
        .unwrap();
    OpenOptions::new()
        .append(true)
        .open(ledger.ledger_path())
        .unwrap()
        .write_all(b"{malformed-display-row}\n")
        .unwrap();
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    // A malformed line never committed: strict reads skip it too and log the
    // file and line number, never the content.
    assert_eq!(ledger.strict_edges().unwrap().len(), 1);
    let logs = logs.lock().unwrap();
    assert!(
        logs.iter().any(
            |line| line.contains(&ledger.ledger_path().display().to_string())
                && line.contains("on line 3")
        ),
        "{logs:?}"
    );
    assert!(logs
        .iter()
        .all(|line| !line.contains("malformed-display-row")));
}

#[test]
fn relative_foreign_topology_path_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let foreign = write_history(
        &agent,
        &temp.path().join("old/sessions"),
        &[
            json!({"v":1,"op":"spawn","at":"t","childId":"x","parent":"relative/parent.jsonl","child":"relative/child.jsonl","depth":1,"name":"x"}),
        ],
    );
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains("relative"));
    assert_eq!(fs::read(&foreign.0).unwrap(), foreign.1);
}

#[test]
fn broken_unrelated_root_blocks_authority_but_not_healthy_display() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("healthy/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "healthy".into(),
        })
        .unwrap();
    let broken_scope = temp.path().join("broken/sessions");
    let broken = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&broken_scope.to_string_lossy(), 16)
    ));
    let bytes = format!(
        "{}\n{}\n",
        json!({"v":1,"op":"meta","at":"t","sessionsDir":broken_scope}),
        json!({"v":1,"op":"future","at":"t"})
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&broken)
        .unwrap()
        .write_all(bytes.as_bytes())
        .unwrap();
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    let error = ledger.strict_edges().unwrap_err().to_string();
    assert!(error.contains(&broken.display().to_string()));
    assert!(error.contains("repair that ledger"));
    assert!(ledger
        .append_rename(
            "sub",
            &sessions.join("c.jsonl").to_string_lossy(),
            "blocked"
        )
        .is_err());
    assert_eq!(fs::read_to_string(&broken).unwrap(), bytes);
}

#[test]
fn malformed_selected_row_with_foreign_candidate_keeps_display_but_refuses_authority() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    let other = temp.path().join("other/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&other).unwrap();
    let current = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    current
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "current".into(),
        })
        .unwrap();
    RlmSpawnLedger::new(&agent, &other, |_| {})
        .append_spawn(&RlmSpawnInput {
            child_id: "other".into(),
            parent: other.join("p.jsonl").to_string_lossy().into(),
            child: other.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "other".into(),
        })
        .unwrap();
    OpenOptions::new()
        .append(true)
        .open(current.ledger_path())
        .unwrap()
        .write_all(format!("{}\n", json!({"v":1,"op":"future","at":"t"})).as_bytes())
        .unwrap();
    assert_eq!(current.edges(false).unwrap().len(), 1);
    assert!(current
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains(&current.ledger_path().display().to_string()));
}

#[test]
fn relation_verdict_cache_invalidates_on_foreign_append_or_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    let other = temp.path().join("other/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&other).unwrap();
    let current = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    current
        .append_spawn(&RlmSpawnInput {
            child_id: "current".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "current".into(),
        })
        .unwrap();
    let foreign = RlmSpawnLedger::new(&agent, &other, |_| {});
    foreign
        .append_spawn(&RlmSpawnInput {
            child_id: "other".into(),
            parent: other.join("p.jsonl").to_string_lossy().into(),
            child: other.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "other".into(),
        })
        .unwrap();
    assert_eq!(current.strict_edges().unwrap().len(), 1);
    // Same cached path, changed size: a reaching foreign tombstone must be seen.
    let linked = agent.join("session-artifacts/p/x/X.jsonl");
    foreign
        .append_spawn(&RlmSpawnInput {
            child_id: "x".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: linked.to_string_lossy().into(),
            depth: 1,
            name: "linked".into(),
        })
        .unwrap();
    assert!(current.strict_edges().is_err());
    // Replacement with a fresh unrelated object: the old verdict cannot be reused.
    let path = foreign.ledger_path().to_path_buf();
    let replacement = temp.path().join("replacement.jsonl");
    fs::write(
        &replacement,
        format!(
            "{}\n",
            json!({"v":1,"op":"meta","at":"t","sessionsDir":replay::checked_scope(&other).unwrap()})
        ),
    )
    .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_eq!(current.strict_edges().unwrap().len(), 1);
}

#[test]
fn large_valid_selected_and_foreign_histories_stay_within_relation_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    let other = temp.path().join("other/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&other).unwrap();
    let current = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    let foreign = RlmSpawnLedger::new(&agent, &other, |_| {});
    for index in 0..512 {
        current
            .append_spawn(&RlmSpawnInput {
                child_id: format!("c{index}"),
                parent: sessions.join("p.jsonl").to_string_lossy().into(),
                child: sessions
                    .join(format!("c{index}.jsonl"))
                    .to_string_lossy()
                    .into(),
                depth: 1,
                name: "current".into(),
            })
            .unwrap();
        foreign
            .append_spawn(&RlmSpawnInput {
                child_id: format!("o{index}"),
                parent: other.join("p.jsonl").to_string_lossy().into(),
                child: other
                    .join(format!("o{index}.jsonl"))
                    .to_string_lossy()
                    .into(),
                depth: 1,
                name: "other".into(),
            })
            .unwrap();
    }
    assert_eq!(current.strict_edges().unwrap().len(), 512);
}

/// A healthy current root plus one malformed unrelated ledger (a blocker).
fn healthy_root_with_blocker() -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    RlmSpawnLedger,
    PathBuf,
    Vec<u8>,
) {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("healthy/sessions");
    fs::create_dir_all(&sessions).unwrap();
    // Production session files: real header with a durable session id.
    let write_session = |depth: u32| {
        let mut session = crate::session_store::SessionFile::create("/work", None, depth);
        let path = sessions.join(crate::session_store::session_file_name(
            session.session_id(),
        ));
        session.set_path(path.clone());
        session.rewrite().unwrap();
        path
    };
    let parent = write_session(0);
    let child = write_session(1);
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: parent.to_string_lossy().into(),
            child: child.to_string_lossy().into(),
            depth: 1,
            name: "healthy".into(),
        })
        .unwrap();
    let broken_scope = temp.path().join("broken/sessions");
    let broken = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&broken_scope.to_string_lossy(), 16)
    ));
    let bytes = format!(
        "{}\n{}\n",
        json!({"v":1,"op":"meta","at":"t","sessionsDir":broken_scope}),
        json!({"v":1,"op":"future","at":"t"})
    )
    .into_bytes();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&broken)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    (temp, parent, child, ledger, broken, bytes)
}

#[test]
fn blocker_keeps_edges_display_working() {
    let (_temp, _, _, ledger, broken, bytes) = healthy_root_with_blocker();
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains(&broken.display().to_string()));
    assert_eq!(fs::read(&broken).unwrap(), bytes);
}

#[test]
fn blocker_keeps_live_edges_display_working() {
    let (_temp, _, _, ledger, _, _) = healthy_root_with_blocker();
    assert_eq!(ledger.live_edges().unwrap().len(), 1);
}

#[test]
fn blocker_keeps_live_edges_of_parent_display_working() {
    let (_temp, parent, _, ledger, _, _) = healthy_root_with_blocker();
    assert_eq!(ledger.live_edges_of_parent(&parent).unwrap().len(), 1);
}

#[test]
fn blocker_keeps_deleted_descendant_usage_display_working() {
    let (_temp, _, _, ledger, _, _) = healthy_root_with_blocker();
    assert!(ledger
        .deleted_descendant_usage_by_parent()
        .unwrap()
        .is_empty());
}

#[test]
fn blocker_keeps_edge_is_live_display_working() {
    let (_temp, _, child, ledger, _, _) = healthy_root_with_blocker();
    assert!(ledger.edge_is_live("sub", &child.to_string_lossy()));
}

#[test]
fn blocker_without_selected_history_displays_empty_and_refuses_authority() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("empty/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let broken_scope = temp.path().join("broken/sessions");
    let broken = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&broken_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(broken.parent().unwrap()).unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&broken)
        .unwrap()
        .write_all(
            format!(
                "{}\n{}\n",
                json!({"v":1,"op":"meta","at":"t","sessionsDir":broken_scope}),
                json!({"v":1,"op":"future","at":"t"})
            )
            .as_bytes(),
        )
        .unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger.edges(true).unwrap().is_empty());
    assert!(ledger.live_edges().unwrap().is_empty());
    assert!(!ledger.edge_is_live("sub", "/synthetic/child.jsonl"));
    assert!(ledger.strict_edges().is_err());
    assert!(ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "sub".into(),
            parent: sessions.join("p.jsonl").to_string_lossy().into(),
            child: sessions.join("c.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "blocked".into()
        })
        .is_err());
    assert!(!rlm_ledger_path(&agent, &sessions).exists());
}

#[test]
fn shared_relation_cap_crossing_names_blocking_ledger_and_remedy() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    // Each foreign ledger stays below the test per-history limit; together
    // they cross the shared namespace cap (4x the per-history limit).
    let padding = "x".repeat(4096);
    let mut last = PathBuf::new();
    for index in 0..6 {
        let scope = temp.path().join(format!("other-{index}/sessions"));
        let records: Vec<Value> = (0..200).map(|row| json!({"v":1,"op":"rename","at":"t","childId":format!("n{row}"),"child":scope.join(format!("c{row}.jsonl")),"name":padding})).collect();
        last = write_history(&agent, &scope, &records).0;
    }
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    let error = ledger.strict_edges().unwrap_err().to_string();
    assert!(error.contains("shared namespace work limit"), "{error}");
    assert!(error.contains("repair that ledger"), "{error}");
    assert!(
        error.contains(&agent.join(RLM_LEDGER_DIR).display().to_string()),
        "{error}"
    );
    assert!(last.is_file());
}

#[cfg(target_os = "macos")]
fn open_fd_count() -> usize {
    fs::read_dir("/dev/fd").unwrap().count()
}

#[cfg(target_os = "macos")]
#[test]
fn relation_check_keeps_open_handles_bounded_with_many_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    for index in 0..256 {
        let scope = temp.path().join(format!("other-{index}/sessions"));
        write_history(&agent, &scope, &[]);
    }
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    let before = open_fd_count();
    assert!(ledger.strict_edges().unwrap().is_empty());
    let after = open_fd_count();
    // Lock + at most one selected and one foreign handle per operation.
    // Other test threads may open a few descriptors; 256 retained handles would not fit.
    assert!(
        after <= before + 8,
        "open descriptors grew from {before} to {after}"
    );
}

#[test]
fn own_filename_with_invalid_header_displays_but_refuses_authority_and_never_rewrites() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let parent = sessions.join("p.jsonl");
    let child = sessions.join("c.jsonl");
    fs::write(&parent, b"{}\n").unwrap();
    fs::write(&child, b"{}\n").unwrap();
    let own = rlm_ledger_path(&agent, &sessions);
    fs::create_dir_all(own.parent().unwrap()).unwrap();
    let bytes = format!("{}\n{}\n",
        json!({"v":1,"op":"meta","at":"t","sessionsDir":"relative/sessions"}),
        json!({"v":1,"op":"spawn","at":"t","childId":"sub","parent":parent,"child":child,"depth":1,"name":"kept"})).into_bytes();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&own)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert!(ledger.edge_is_live("sub", &child.to_string_lossy()));
    let error = ledger.strict_edges().unwrap_err().to_string();
    assert!(error.contains(&own.display().to_string()), "{error}");
    assert!(ledger
        .append_spawn(&RlmSpawnInput {
            child_id: "next".into(),
            parent: parent.to_string_lossy().into(),
            child: sessions.join("n.jsonl").to_string_lossy().into(),
            depth: 1,
            name: "blocked".into()
        })
        .is_err());
    assert!(ledger
        .append_rename("sub", &child.to_string_lossy(), "blocked")
        .is_err());
    assert_eq!(fs::read(&own).unwrap(), bytes);
    assert_eq!(fs::read_dir(agent.join(RLM_LEDGER_DIR)).unwrap().count(), 1);
}

#[test]
fn a_torn_middle_line_in_a_foreign_ledger_does_not_hide_a_later_relation() {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = temp.path().join("current/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let scope = replay::checked_scope(&sessions).unwrap();
    let foreign_scope = temp.path().join("old/sessions");
    let path = agent.join(RLM_LEDGER_DIR).join(format!(
        "{}.jsonl",
        crate::paths::hash_key(&foreign_scope.to_string_lossy(), 16)
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A crash left a torn record; the writer then started the next record on
    // a new line. The later well-formed edge points into this namespace.
    let bytes = format!(
        "{}\n{{\"v\":1,\"op\":\"spa\n{}\n",
        json!({"v":1,"op":"meta","at":"t","sessionsDir":foreign_scope}),
        json!({"v":1,"op":"spawn","at":"t","childId":"x","parent":scope.join("P.jsonl"),"child":agent.join("session-artifacts/P/x/X.jsonl"),"depth":1,"name":"x"}),
    );
    fs::write(&path, &bytes).unwrap();
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    assert!(ledger
        .strict_edges()
        .unwrap_err()
        .to_string()
        .contains("legacy authority"));
    assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
}
