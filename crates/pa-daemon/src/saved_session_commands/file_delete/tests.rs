#![cfg(unix)]

use super::*;
use std::cell::Cell;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _temp: tempfile::TempDir,
    file: PathBuf,
    artifacts: PathBuf,
    tool: PathBuf,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let file = sessions.join("-session.jsonl");
        std::fs::write(&file, b"transcript").unwrap();
        let artifacts = temp.path().join("session-artifacts/-session");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::write(artifacts.join("snapshot"), b"snapshot").unwrap();
        let tool = temp.path().join("fake-trash");
        std::fs::write(&tool, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { _temp: temp, file, artifacts, tool }
    }
}

#[test]
fn successful_trash_uses_one_absolute_operand_and_hook_precedes_artifact_removal() {
    // A local stand-in moves only into this fixture, never into any OS Trash.
    let fixture = Fixture::new(r#"test "$#" -eq 1
case "$1" in /*) ;; *) exit 9;; esac
printf '%s\n' "$1" > "$0.argv"
/bin/mv "$1" "$1.removed""#);
    let calls = Cell::new(0);
    let result = delete_with_program(&fixture.file, &|removed| {
        assert_eq!(removed, fixture.file);
        assert!(fixture.artifacts.join("snapshot").is_file());
        assert!(std::fs::symlink_metadata(removed).is_err());
        calls.set(calls.get() + 1);
    }, Some(&fixture.tool));
    assert_eq!(result, json!({"ok": true, "method": "trash"}));
    assert_eq!(calls.get(), 1);
    assert!(!fixture.artifacts.exists());
    assert_eq!(std::fs::read(fixture.file.with_file_name("-session.jsonl.removed")).unwrap(), b"transcript");
    assert_eq!(std::fs::read_to_string(fixture.tool.with_extension("argv")).unwrap(), format!("{}\n", fixture.file.display()));
}

#[test]
fn installed_trash_failure_preserves_transcript_and_artifacts() {
    let fixture = Fixture::new("echo permission-denied >&2; exit 1");
    let result = delete_with_program(&fixture.file, &|_| panic!("failed removal hook"), Some(&fixture.tool));
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("permanent deletion was not attempted"));
    assert_eq!(std::fs::read(&fixture.file).unwrap(), b"transcript");
    assert_eq!(std::fs::read(fixture.artifacts.join("snapshot")).unwrap(), b"snapshot");
}

#[test]
fn success_exit_without_removal_is_not_success_and_keeps_artifacts() {
    let fixture = Fixture::new("exit 0");
    let result = delete_with_program(&fixture.file, &|_| panic!("no-op hook"), Some(&fixture.tool));
    assert_eq!(result["ok"], false);
    assert_eq!(std::fs::read(&fixture.file).unwrap(), b"transcript");
    assert!(fixture.artifacts.join("snapshot").is_file());
}

#[test]
fn missing_entry_never_invokes_trash_or_cleanup() {
    let fixture = Fixture::new(r#"touch "$0.invoked""#);
    std::fs::remove_file(&fixture.file).unwrap();
    let result = delete_with_program(&fixture.file, &|_| panic!("missing hook"), Some(&fixture.tool));
    assert_eq!(result["ok"], false);
    assert!(!fixture.tool.with_extension("invoked").exists());
    assert!(fixture.artifacts.join("snapshot").is_file());
}

#[test]
fn concurrent_removal_during_failed_trash_is_not_reported_as_trashed() {
    let fixture = Fixture::new(r#"/bin/mv "$1" "$1.removed"; exit 1"#);
    let result = delete_with_program(&fixture.file, &|_| panic!("uncertain removal hook"), Some(&fixture.tool));
    assert_eq!(result["ok"], false);
    assert!(fixture.artifacts.join("snapshot").is_file());
    assert_eq!(std::fs::read(fixture.file.with_file_name("-session.jsonl.removed")).unwrap(), b"transcript");
}

#[test]
fn absent_tool_uses_documented_permanent_unlink_not_trash() {
    let fixture = Fixture::new("exit 99");
    let result = delete_with_program(&fixture.file, &|_| {}, None);
    assert_eq!(result, json!({"ok": true, "method": "unlink"}));
    assert!(!fixture.file.exists());
    assert!(!fixture.artifacts.exists());
}

#[test]
fn installed_but_unlaunchable_tool_does_not_fall_back_to_unlink() {
    let fixture = Fixture::new("exit 0");
    std::fs::write(&fixture.tool, "#!/nonexistent-synthetic-interpreter\n").unwrap();
    let result = delete_with_program(&fixture.file, &|_| panic!("exec failure hook"), Some(&fixture.tool));
    assert_eq!(result["ok"], false);
    assert_eq!(std::fs::read(&fixture.file).unwrap(), b"transcript");
    assert!(fixture.artifacts.join("snapshot").is_file());
}

#[test]
fn symlink_entry_moves_without_touching_target_even_when_dangling() {
    for dangling in [false, true] {
        let fixture = Fixture::new(r#"/bin/mv "$1" "$1.removed""#);
        let target = fixture._temp.path().join("outside-transcript");
        if !dangling { std::fs::write(&target, b"target").unwrap(); }
        std::fs::remove_file(&fixture.file).unwrap();
        std::os::unix::fs::symlink(&target, &fixture.file).unwrap();
        let result = delete_with_program(&fixture.file, &|_| {}, Some(&fixture.tool));
        assert_eq!(result, json!({"ok": true, "method": "trash"}));
        let removed = fixture.file.with_file_name("-session.jsonl.removed");
        assert_eq!(std::fs::read_link(removed).unwrap(), target);
        if !dangling { assert_eq!(std::fs::read(&target).unwrap(), b"target"); }
        assert!(!fixture.artifacts.exists());
    }
}
