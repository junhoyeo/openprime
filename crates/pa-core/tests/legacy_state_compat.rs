//! Copied legacy state acceptance: fixtures shaped by the TypeScript writers and tests at
//! legacy develop `703367bad` load through the Rust readers (sources: `fixtures/legacy-state/README.md`).
//! Every test copies its fixture into a temp dir; nothing resolves a real home or agent dir.

use std::path::{Path, PathBuf};

use pa_core::refinement::{
    get_harness_state_path, load_harness_state, update_harness_state, HarnessScope, RefinementKind,
};
use pa_core::session::manager::SessionManager;
use pa_core::skills::{load_skills_from_dir, SkillKind};
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
fn a_legacy_v3_session_rebuilds_the_typescript_context() {
    let temp = tempfile::tempdir().unwrap();
    let manager = open_copied_session(temp.path(), "v3-compacted-branch.jsonl");
    let context = manager.active_context();
    // TS buildSessionContext: summary first, the kept tail from firstKeptEntryId, then the
    // post-compaction turns; the abandoned sibling branch and the /btw pointer stay out.
    assert_eq!(
        transcript(&context.messages),
        pairs(&[
            ("compactionSummary", "legacy compaction summary"),
            ("assistant", "legacy first answer"),
            ("user", "legacy second question"),
            ("assistant", "legacy second answer"),
        ])
    );
    assert_eq!(context.thinking_level, "high");
    assert_eq!(
        context.model,
        Some(("anthropic".to_string(), "claude-opus-4-1".to_string()))
    );
}

#[test]
fn a_legacy_v1_session_migrates_to_a_linear_context() {
    let temp = tempfile::tempdir().unwrap();
    let manager = open_copied_session(temp.path(), "v1-linear.jsonl");
    let context = manager.active_context();
    assert_eq!(
        transcript(&context.messages),
        pairs(&[
            ("user", "v1 first question"),
            ("assistant", "v1 first answer"),
            ("user", "v1 second question"),
            ("assistant", "v1 second answer"),
        ])
    );
    assert_eq!(context.thinking_level, "medium");
    assert_eq!(
        context.model,
        Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string()))
    );
    assert_eq!(
        manager.get_header().and_then(|header| header.version),
        Some(3)
    );
}

#[test]
fn a_legacy_v1_compaction_keeps_its_summary_and_kept_turns() {
    let temp = tempfile::tempdir().unwrap();
    let manager = open_copied_session(temp.path(), "v1-compaction.jsonl");
    // TS migrateV1ToV2 ids every row and resolves the top-level firstKeptEntryIndex.
    let expected = pairs(&[
        ("compactionSummary", "v1 compaction summary"),
        ("user", "v1 q2"),
        ("assistant", "v1 a2"),
        ("user", "v1 q3"),
        ("assistant", "v1 a3"),
    ]);
    assert_eq!(transcript(&manager.active_context().messages), expected);
    // The open rewrites the migrated file; the reopened file keeps the same context.
    let reopened = SessionManager::open(
        Path::new("/legacy/project"),
        &temp.path().join("sessions"),
        &temp.path().join("sessions/v1-compaction.jsonl"),
    );
    assert_eq!(transcript(&reopened.active_context().messages), expected);
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

#[test]
fn a_legacy_python_skill_loads_with_its_import_metadata() {
    let skill_dir = fixture("skills/python-skill");
    let loaded = load_skills_from_dir(&skill_dir, "test");
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    assert_eq!(loaded.skills.len(), 1);
    let skill = &loaded.skills[0];
    assert_eq!(skill.name, "python-skill");
    assert_eq!(skill.kind, SkillKind::Python);
    let python = skill.python.as_ref().expect("python metadata");
    assert_eq!(
        (
            python.import_name.as_str(),
            python.package_path.clone(),
            python.pyproject_path.clone()
        ),
        (
            "python_skill",
            skill_dir.clone(),
            skill_dir.join("pyproject.toml")
        )
    );
}

#[test]
fn a_legacy_python_skill_without_its_package_degrades_to_markdown() {
    let loaded = load_skills_from_dir(&fixture("skills/python-package-missing"), "test");
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].kind, SkillKind::Markdown);
    assert!(!loaded.diagnostics.is_empty());
}

#[test]
fn a_legacy_manual_only_skill_keeps_disable_model_invocation() {
    let loaded = load_skills_from_dir(&fixture("skills/disable-model-invocation"), "test");
    assert_eq!(loaded.skills.len(), 1);
    assert!(loaded.skills[0].disable_model_invocation);
}

fn copied_harness_dir(temp: &Path, name: &str) -> PathBuf {
    let dir = temp.join("harness");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture(name), get_harness_state_path(&dir)).unwrap();
    dir
}

fn entry_ids(path: &Path) -> Vec<(String, String)> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut ids = Vec::new();
    for (kind, records) in state["entries"].as_object().unwrap() {
        for id in records.as_object().unwrap().keys() {
            ids.push((kind.clone(), id.clone()));
        }
    }
    ids.sort();
    ids
}

#[test]
fn a_legacy_kernel_written_harness_state_loads_every_entry() {
    let temp = tempfile::tempdir().unwrap();
    let dir = copied_harness_dir(temp.path(), "harness_state.python-writer.json");
    let state = load_harness_state(&dir, HarnessScope::Global);
    let loaded: Vec<(RefinementKind, String, String)> = state
        .entries
        .iter()
        .flat_map(|(kind, records)| {
            records
                .values()
                .map(|entry| (*kind, entry.id.clone(), entry.path.clone()))
        })
        .collect();
    assert_eq!(
        loaded,
        vec![
            (
                RefinementKind::Prompt,
                "legacy_prompt_note".to_string(),
                "policies/style".to_string()
            ),
            (
                RefinementKind::Memory,
                "legacy_memory".to_string(),
                "facts/legacy".to_string()
            ),
            (
                RefinementKind::Skill,
                "legacy_skill".to_string(),
                "skills/legacy".to_string()
            ),
            (
                RefinementKind::Subagent,
                "legacy_reviewer".to_string(),
                "orchestration/review".to_string()
            ),
        ]
    );
    assert_eq!(
        state.entries[&RefinementKind::Skill]["legacy_skill"].reference["import"],
        "legacy_skill"
    );
    assert_eq!(state.refinements.len(), 1);
}

#[test]
fn a_topic_era_harness_entry_loads_under_path() {
    let temp = tempfile::tempdir().unwrap();
    let dir = copied_harness_dir(temp.path(), "harness_state.topic-era.json");
    let state = load_harness_state(&dir, HarnessScope::Local);
    let entry = state.entries[&RefinementKind::Memory]
        .get("topic_entry")
        .expect("TS loadHarnessState migrates a topic-era entry instead of dropping it");
    assert_eq!(entry.path, "window/era");
    assert_eq!(entry.content, "Window-era content.");
}

#[test]
fn saving_after_a_legacy_load_never_erases_an_entry() {
    let temp = tempfile::tempdir().unwrap();
    let dir = copied_harness_dir(temp.path(), "harness_state.topic-era.json");
    let path = get_harness_state_path(&dir);
    let before: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    update_harness_state(&dir, HarnessScope::Local, |state| {
        state.refinements.clear();
    })
    .unwrap();
    assert_eq!(
        entry_ids(&path),
        vec![
            ("memory".to_string(), "hand_edited".to_string()),
            ("memory".to_string(), "topic_entry".to_string()),
        ]
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // A row this build cannot type survives byte-for-byte as JSON.
    assert_eq!(
        after["entries"]["memory"]["hand_edited"],
        before["entries"]["memory"]["hand_edited"]
    );
    assert_eq!(
        after["entries"]["memory"]["topic_entry"]["path"],
        "window/era"
    );
    assert!(after["entries"]["memory"]["topic_entry"]
        .get("topic")
        .is_none());
}

#[test]
fn a_typed_entry_wins_over_a_preserved_row_with_the_same_id() {
    let temp = tempfile::tempdir().unwrap();
    let dir = copied_harness_dir(temp.path(), "harness_state.topic-era.json");
    update_harness_state(&dir, HarnessScope::Local, |state| {
        let mut entry = state.entries[&RefinementKind::Memory]["topic_entry"].clone();
        entry.id = "hand_edited".to_string();
        entry.title = "Typed replacement".to_string();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("hand_edited".to_string(), entry);
    })
    .unwrap();
    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(get_harness_state_path(&dir)).unwrap())
            .unwrap();
    assert_eq!(
        after["entries"]["memory"]["hand_edited"]["title"],
        "Typed replacement"
    );
}
