//! Checked authority reads share the ledger grammar without display degradation.
use super::*;

fn fixture() -> (tempfile::TempDir, RlmSpawnLedger, RlmSpawnInput) {
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    let sessions = agent.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let input = RlmSpawnInput {
        child_id: "sub-1".into(),
        parent: sessions.join("parent.jsonl").to_string_lossy().into(),
        child: sessions.join("child.jsonl").to_string_lossy().into(),
        depth: 1,
        name: "synthetic-child".into(),
    };
    let ledger = RlmSpawnLedger::new(&agent, &sessions, |_| {});
    ledger.append_spawn(&input).unwrap();
    (temp, ledger, input)
}

#[test]
fn strict_authority_reads_include_deleted_edges() {
    let (_temp, ledger, input) = fixture();
    ledger.append_delete(&input.child_id, &input.child, RlmLedgerDeleteReason::User).unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    let edges = ledger.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].child_id, input.child_id);
    assert_eq!(edges[0].deleted, Some(RlmLedgerDeleteReason::User));
    assert_eq!(edges, ledger.edges(true).unwrap());
    assert!(ledger.edges(false).unwrap().is_empty());
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

/// A malformed (non-JSON) line never committed, per
/// `replay::classify_authority_line` - it is skipped and logged, not a
/// reason to refuse the whole tree (#37/#38: one ordinary crash must not
/// permanently block every future authority read in this scope).
#[test]
fn a_malformed_record_line_is_skipped_not_refused_on_authority_reads() {
    let (_temp, ledger, input) = fixture();
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(b"{malformed-record}
").unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    let edges = ledger.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].child_id, input.child_id);
    assert_eq!(edges[0].deleted, None);
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

#[test]
fn unknown_operation_is_not_known_empty_authority() {
    let (_temp, ledger, _) = fixture();
    let future = json!({"v":1, "op":"future-operation", "at":"2026-01-01T00:00:00Z"});
    writeln!(OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap(), "{future}").unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

#[test]
fn io_failure_is_not_an_empty_ledger_for_either_read_policy() {
    let (temp, ledger, _) = fixture();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    let backup = temp.path().join("history.backup");
    fs::rename(ledger.ledger_path(), &backup).unwrap();
    fs::create_dir(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
    assert!(ledger.edges(true).is_err());
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert!(ledger.ledger_path().is_dir());
}

#[test]
fn a_genuinely_missing_ledger_without_legacy_history_remains_empty() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let ledger = RlmSpawnLedger::new(temp.path(), &sessions, |_| {});
    assert!(ledger.strict_edges().unwrap().is_empty());
    assert!(ledger.edges(true).unwrap().is_empty());
    assert!(!ledger.ledger_path().exists());
}

/// An unterminated (torn) final line never committed - it is skipped, not
/// a reason to refuse the whole tree, and earlier valid records are still
/// read (#37/#38).
#[test]
fn a_torn_final_delete_is_skipped_not_refused() {
    let (_temp, ledger, input) = fixture();
    let row = json!({"v":1,"op":"delete","at":"2026-01-01T00:00:00Z","childId":input.child_id,"child":input.child,"reason":"user"});
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(row.to_string().as_bytes()).unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    let edges = ledger.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].deleted, None, "an unacknowledged delete is not a tombstone");
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

/// A non-UTF-8 line never committed and is skipped, never lossily decoded
/// into authority (which could otherwise replace invalid bytes and read a
/// forged/garbled identity as real) (#37/#38).
#[test]
fn an_invalid_utf8_line_is_skipped_never_lossily_read() {
    let (_temp, ledger, input) = fixture();
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(b"{"v":1,"op":"delete","at":"t","childId":"ÿ"}
").unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    let edges = ledger.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].child_id, input.child_id);
    assert_eq!(edges[0].deleted, None);
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

/// A well-formed but duplicate-key record is not a crash artifact - it
/// could be an ambiguous committed record - so it refuses, unlike a
/// genuinely malformed/torn line.
#[test]
fn a_well_formed_duplicate_key_record_refuses_authority() {
    let (_temp, ledger, input) = fixture();
    let line = format!(
        "{{"v":1,"op":"delete","at":"t","childId":{id},"childId":"other","child":{child},"reason":"user"}}
",
        id = serde_json::to_string(&input.child_id).unwrap(),
        child = serde_json::to_string(&input.child).unwrap(),
    );
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(line.as_bytes()).unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().unwrap_err().to_string().contains("duplicate-key"));
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

/// A well-formed record with an unsupported version may be a newer binary's
/// committed record, not a crash artifact - it refuses rather than being
/// silently dropped.
#[test]
fn a_well_formed_newer_version_record_refuses_authority() {
    let (_temp, ledger, input) = fixture();
    let row = json!({"v":2,"op":"delete","at":"t","childId":input.child_id,"child":input.child,"reason":"user"});
    writeln!(OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap(), "{row}").unwrap();
    assert!(ledger.strict_edges().unwrap_err().to_string().contains("unsupported record version"));
}

/// A present-but-wrongly-typed required field may be a newer/ambiguous
/// committed record and refuses; an ABSENT required field is incomplete
/// (never committed) and only skips.
#[test]
fn a_required_field_with_the_wrong_type_refuses_but_an_absent_one_skips() {
    let (_temp, ledger, input) = fixture();
    let wrong = json!({"v":1,"op":"delete","at":"t","childId":7,"child":input.child,"reason":"user"});
    writeln!(OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap(), "{wrong}").unwrap();
    assert!(ledger.strict_edges().unwrap_err().to_string().contains("invalid childId field"));

    let (_temp2, ledger2, input2) = fixture();
    let absent = json!({"v":1,"op":"delete","at":"t","child":input2.child,"reason":"user"});
    writeln!(OpenOptions::new().append(true).open(ledger2.ledger_path()).unwrap(), "{absent}").unwrap();
    let edges = ledger2.strict_edges().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].deleted, None);
}

#[cfg(unix)]
#[test]
fn a_dangling_ledger_entry_is_not_genuine_absence() {
    let (temp, ledger, _) = fixture();
    let backup = temp.path().join("history.backup");
    fs::rename(ledger.ledger_path(), &backup).unwrap();
    std::os::unix::fs::symlink(temp.path().join("missing-history"), ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
    assert!(backup.is_file());
    assert!(ledger.ledger_path().symlink_metadata().unwrap().file_type().is_symlink());
}

#[test]
fn missing_or_conflicting_ownership_header_cannot_authorize_mutation() {
    let (_temp, ledger, _) = fixture();
    for contents in [String::new(), format!("{}
", json!({"v":1,"op":"meta","at":"t","sessionsDir":ledger.canonical_sessions_dir.with_file_name("unrelated-sessions")}))] {
        fs::write(ledger.ledger_path(), &contents).unwrap();
        assert!(ledger.strict_edges().is_err());
        assert_eq!(fs::read_to_string(ledger.ledger_path()).unwrap(), contents);
    }
}
