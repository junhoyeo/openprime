#![cfg(unix)]
use super::*;

#[test]
fn checked_missing_suffix_is_stable_before_and_after_creation_through_alias() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let file = alias.join("nested/session.jsonl");
    let before = checked_session_path(&file).unwrap();
    std::fs::create_dir(real.join("nested")).unwrap();
    std::fs::write(&file, b"synthetic").unwrap();
    assert_eq!(before, file.canonicalize().unwrap());
    assert_eq!(checked_session_path(&file).unwrap(), before);
    assert_eq!(canonical_session_path(&file), before);
}

#[test]
fn checked_resolver_refuses_dangling_alias_and_non_directory_component() {
    let temp = tempfile::tempdir().unwrap();
    let dangling = temp.path().join("dangling");
    std::os::unix::fs::symlink(temp.path().join("missing"), &dangling).unwrap();
    assert!(checked_session_path(&dangling).is_err());
    assert!(checked_session_path(&dangling.join("child.jsonl")).is_err());
    let file = temp.path().join("file");
    std::fs::write(&file, b"not-directory").unwrap();
    assert!(checked_session_path(&file.join("child.jsonl")).is_err());
    // The older, best-effort API remains available to unchanged lease callers.
    assert_eq!(
        canonical_session_path(&file.join("child.jsonl")),
        file.canonicalize().unwrap().join("child.jsonl")
    );
}
