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

#[test]
fn malformed_record_refuses_authority_without_changing_display_policy_or_bytes() {
    let (_temp, ledger, _) = fixture();
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(b"{malformed-record}\n").unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
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

#[test]
fn unterminated_valid_record_cannot_authorize_cleanup() {
    let (_temp, ledger, input) = fixture();
    let row = json!({"v":1,"op":"delete","at":"2026-01-01T00:00:00Z","childId":input.child_id,"child":input.child,"reason":"user"});
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(row.to_string().as_bytes()).unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
}

#[test]
fn invalid_utf8_never_becomes_authority_through_lossy_replacement() {
    let (_temp, ledger, _) = fixture();
    OpenOptions::new().append(true).open(ledger.ledger_path()).unwrap()
        .write_all(b"{\"v\":1,\"op\":\"meta\",\"at\":\"t\",\"sessionsDir\":\"\xff\"}\n").unwrap();
    let bytes = fs::read(ledger.ledger_path()).unwrap();
    assert!(ledger.strict_edges().is_err());
    assert_eq!(ledger.edges(false).unwrap().len(), 1);
    assert_eq!(fs::read(ledger.ledger_path()).unwrap(), bytes);
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
    for contents in [String::new(), format!("{}\n", json!({"v":1,"op":"meta","at":"t","sessionsDir":ledger.canonical_sessions_dir.with_file_name("unrelated-sessions")}))] {
        fs::write(ledger.ledger_path(), &contents).unwrap();
        assert!(ledger.strict_edges().is_err());
        assert_eq!(fs::read_to_string(ledger.ledger_path()).unwrap(), contents);
    }
}
