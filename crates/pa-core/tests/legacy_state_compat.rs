//! Copied legacy state acceptance: fixtures shaped by the TypeScript writers and tests at
//! legacy develop `703367bad` load through the Rust readers (sources: `fixtures/legacy-state/README.md`).
//! Every test copies its fixture into a temp dir; nothing resolves a real home or agent dir.

use std::path::{Path, PathBuf};

use pa_core::session::manager::SessionManager;
use pa_types::session::AgentMessage;
use serde_json::Value;

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/legacy-state")
        .join(relative)
}

/// Copy one fixture session into a fresh session dir so a migrating open can never touch the
/// checked-in bytes.
fn open_copied_session(temp: &Path, name: &str) -> SessionManager {
    let sessions = temp.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let copy = sessions.join(name);
    std::fs::copy(fixture(&format!("sessions/{name}")), &copy).unwrap();
    SessionManager::open(Path::new("/legacy/project"), &sessions, &copy)
}

/// The visible transcript: each context message as `(role, text)`.
fn transcript(messages: &[AgentMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| {
            let value = serde_json::to_value(message).unwrap();
            let role = value["role"].as_str().unwrap_or_default().to_string();
            let text = match &value["content"] {
                Value::String(text) => text.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                _ => value["summary"].as_str().unwrap_or_default().to_string(),
            };
            (role, text)
        })
        .collect()
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(role, text)| ((*role).to_string(), (*text).to_string()))
        .collect()
}

#[test]
fn a_legacy_v2_hook_message_migrates_to_a_custom_message() {
    let temp = tempfile::tempdir().unwrap();
    let manager = open_copied_session(temp.path(), "v2-hook-message.jsonl");
    let context = manager.active_context();
    // TS migrateV2ToV3 renames the `hookMessage` role to `custom`; the turn stays whole.
    assert_eq!(
        transcript(&context.messages),
        pairs(&[
            ("user", "v2 question"),
            ("custom", "v2 hook note"),
            ("assistant", "v2 answer"),
        ])
    );
}
