//! Durable `/btw` transcripts. Side turns are stored under the parent session's
//! artifact tree and linked from one immutable parent custom row. The parent
//! conversation never receives the side messages themselves.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_agent::types::AssistantMessage;
use serde_json::json;

use crate::session_store::{session_file_name, SessionFile};
use crate::worker::SessionCore;

pub(crate) const SIDE_QUESTION_POINTER_TYPE: &str = "side_question";
pub(crate) const SIDE_QUESTION_STATUS_TYPE: &str = "side_question_status";
const MAX_PANES: usize = 32;

#[derive(Clone, Copy)]
pub(crate) enum SideQuestionRecordStatus {
    Cancelled,
    Error,
}

impl SideQuestionRecordStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

/// One pane's best-effort writer. The first failure disables only this pane;
/// answering remains independent from transcript storage.
pub(crate) struct SideQuestionRecorder {
    core: Arc<Mutex<SessionCore>>,
    agent_dir: PathBuf,
    btw_id: String,
    state: Mutex<RecorderState>,
}

#[derive(Default)]
struct RecorderState {
    transcript: Option<SessionFile>,
    disabled: bool,
}

fn assistant_message_value(message: &AssistantMessage) -> serde_json::Value {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert("role".to_string(), json!("assistant"));
    }
    value
}

fn resume_linked_transcript(
    parent: &SessionFile,
    agent_dir: &std::path::Path,
    btw_id: &str,
) -> anyhow::Result<Option<SessionFile>> {
    if parent.path.as_os_str().is_empty() || !parent.path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&parent.path)?;
    let mut linked = content.lines().filter_map(|line| {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        (row.get("type").and_then(serde_json::Value::as_str) == Some("custom")
            && row.get("customType").and_then(serde_json::Value::as_str)
                == Some(SIDE_QUESTION_POINTER_TYPE)
            && row
                .get("data")
                .and_then(|data| data.get("btwId"))
                .and_then(serde_json::Value::as_str)
                == Some(btw_id))
        .then(|| {
            row.get("data")
                .and_then(|data| data.get("sessionFile"))
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        })
        .flatten()
    });
    let Some(path) = linked.next_back() else {
        return Ok(None);
    };
    let expected_root = agent_dir
        .join("session-artifacts")
        .join(parent.session_id());
    anyhow::ensure!(
        path.starts_with(&expected_root),
        "linked side-question transcript escapes the parent artifact directory"
    );
    let transcript = SessionFile::open(&path)?;
    anyhow::ensure!(
        transcript.header.parent_session.as_deref() == Some(parent.path.to_string_lossy().as_ref()),
        "linked side-question transcript has the wrong parent"
    );
    anyhow::ensure!(
        transcript.header.rlm_depth == Some(0),
        "linked side-question transcript is not depth zero"
    );
    Ok(Some(transcript))
}

impl SideQuestionRecorder {
    fn new(core: Arc<Mutex<SessionCore>>, agent_dir: PathBuf, btw_id: String) -> Self {
        Self {
            core,
            agent_dir,
            btw_id,
            state: Mutex::new(RecorderState::default()),
        }
    }

    pub(crate) fn record_turn(
        &self,
        question: &str,
        assistant: &AssistantMessage,
        auxiliary: &[AssistantMessage],
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.disabled {
            return;
        }
        let result = self.record_turn_inner(&mut state, question, assistant, auxiliary);
        if let Err(error) = result {
            state.disabled = true;
            eprintln!(
                "pa-daemon: side-question transcript {} disabled: {error:#}",
                self.btw_id
            );
        }
    }

    fn record_turn_inner(
        &self,
        state: &mut RecorderState,
        question: &str,
        assistant: &AssistantMessage,
        auxiliary: &[AssistantMessage],
    ) -> anyhow::Result<()> {
        if state.transcript.is_none() {
            let mut core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parent = core
                .store
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("parent session is not persisted"))?;
            anyhow::ensure!(
                !parent.path.as_os_str().is_empty(),
                "parent session has no durable path"
            );
            state.transcript = resume_linked_transcript(parent, &self.agent_dir, &self.btw_id)?;
            if state.transcript.is_none() {
                let parent_file = parent.path.display().to_string();
                let parent_id = parent.session_id().to_string();
                let cwd = parent.header.cwd.clone();
                let mut transcript = SessionFile::create(&cwd, Some(&parent_file), 0);
                let pane_dir = self
                    .agent_dir
                    .join("session-artifacts")
                    .join(&parent_id)
                    .join(format!("btw-{}", uuid::Uuid::new_v4()));
                transcript.path = pane_dir.join(session_file_name(transcript.session_id()));
                let pointer = json!({
                    "customType": SIDE_QUESTION_POINTER_TYPE,
                    "data": {
                        "btwId": self.btw_id,
                        "sessionFile": transcript.path.display().to_string(),
                        "sessionId": transcript.session_id(),
                        "question": question,
                        "startedAt": crate::util::now_iso(),
                    },
                });
                // `persist_entry` writes before indexing. A failed pointer append
                // therefore cannot move the parent's in-memory leaf off disk.
                parent.persist_entry("custom", pointer)?;
                transcript.persist_entry(
                    "model_change",
                    json!({ "provider": assistant.provider, "modelId": assistant.model }),
                )?;
                state.transcript = Some(transcript);
            }
        }

        let transcript = state.transcript.as_mut().expect("initialized above");
        transcript.persist_entry(
            "message",
            json!({
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": question }],
                    "timestamp": crate::util::now_ms(),
                }
            }),
        )?;
        for completion in auxiliary {
            transcript.persist_entry(
                "message",
                json!({ "message": assistant_message_value(completion) }),
            )?;
        }
        transcript.persist_entry(
            "message",
            json!({ "message": assistant_message_value(assistant) }),
        )?;
        Ok(())
    }

    pub(crate) fn record_status(
        &self,
        status: SideQuestionRecordStatus,
        error_message: Option<&str>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.disabled {
            return;
        }
        let Some(transcript) = state.transcript.as_mut() else {
            return;
        };
        let mut data = json!({ "status": status.as_str() });
        if let Some(error) = error_message.filter(|value| !value.is_empty()) {
            data["errorMessage"] = json!(error);
        }
        if let Err(error) = transcript.persist_entry(
            "custom",
            json!({ "customType": SIDE_QUESTION_STATUS_TYPE, "data": data }),
        ) {
            state.disabled = true;
            eprintln!(
                "pa-daemon: side-question status {} persistence disabled: {error:#}",
                self.btw_id
            );
        }
    }
}

struct RegistryState {
    panes: HashMap<String, Arc<SideQuestionRecorder>>,
    order: VecDeque<String>,
}

/// Bounded registry joining separate side-question runs into one pane transcript.
pub(crate) struct SideQuestionPaneRegistry {
    core: Arc<Mutex<SessionCore>>,
    agent_dir: PathBuf,
    state: Mutex<RegistryState>,
}

impl SideQuestionPaneRegistry {
    pub(crate) fn new(core: Arc<Mutex<SessionCore>>, agent_dir: PathBuf) -> Self {
        Self {
            core,
            agent_dir,
            state: Mutex::new(RegistryState {
                panes: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    pub(crate) fn resolve(
        &self,
        key: &str,
        btw_id: &str,
        reuse_existing: bool,
    ) -> Arc<SideQuestionRecorder> {
        let parent_id = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .store
            .as_ref()
            .map_or_else(
                || "fileless".to_string(),
                |store| store.session_id().to_string(),
            );
        let scoped_key = format!("{parent_id}:{key}");
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reuse_existing {
            if let Some(recorder) = state.panes.get(&scoped_key) {
                return Arc::clone(recorder);
            }
        }
        state.panes.remove(&scoped_key);
        state.order.retain(|entry| entry != &scoped_key);
        let recorder = Arc::new(SideQuestionRecorder::new(
            Arc::clone(&self.core),
            self.agent_dir.clone(),
            btw_id.to_string(),
        ));
        state
            .panes
            .insert(scoped_key.clone(), Arc::clone(&recorder));
        state.order.push_back(scoped_key);
        while state.panes.len() > MAX_PANES {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            state.panes.remove(&oldest);
        }
        recorder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn assistant(text: &str, total_tokens: u64) -> AssistantMessage {
        serde_json::from_value(json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "openai-completions",
            "provider": "test-provider",
            "model": "test-model",
            "usage": {
                "input": total_tokens - 1,
                "output": 1,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": total_tokens,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "stop",
            "timestamp": 1
        }))
        .unwrap()
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<Mutex<SessionCore>>,
        SideQuestionPaneRegistry,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let mut parent = SessionFile::create("/work", None, 0);
        parent.path = temp
            .path()
            .join("sessions")
            .join(session_file_name(parent.session_id()));
        parent.rewrite().unwrap();
        let core = Arc::new(Mutex::new(SessionCore::test_core(
            Some(parent),
            "/work".to_string(),
        )));
        let registry = SideQuestionPaneRegistry::new(Arc::clone(&core), temp.path().to_path_buf());
        (temp, core, registry)
    }

    fn jsonl(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn one_pointer_groups_follow_ups_and_preserves_usage_outside_parent_context() {
        let (_temp, core, registry) = fixture();
        let recorder = registry.resolve("pane:p1", "p1", true);
        recorder.record_turn("first?", &assistant("one", 11), &[]);
        let same = registry.resolve("pane:p1", "p1", true);
        assert!(Arc::ptr_eq(&recorder, &same));
        same.record_turn("second?", &assistant("two", 23), &[assistant("summary", 7)]);

        let parent_path = core.lock().unwrap().store.as_ref().unwrap().path.clone();
        let parent_rows = jsonl(&parent_path);
        let pointers: Vec<&Value> = parent_rows
            .iter()
            .filter(|row| row["customType"] == SIDE_QUESTION_POINTER_TYPE)
            .collect();
        assert_eq!(pointers.len(), 1);
        let side_path = PathBuf::from(pointers[0]["data"]["sessionFile"].as_str().unwrap());
        let side_rows = jsonl(&side_path);
        assert_eq!(
            side_rows[0]["parentSession"],
            parent_path.display().to_string()
        );
        assert_eq!(side_rows[0]["rlmDepth"], 0);
        let messages: Vec<&Value> = side_rows
            .iter()
            .filter(|row| row["type"] == "message")
            .collect();
        assert_eq!(
            messages.len(),
            5,
            "two user rows, one auxiliary, two answers"
        );
        assert_eq!(messages[3]["message"]["usage"]["totalTokens"], 7);
        assert_eq!(messages[4]["message"]["usage"]["totalTokens"], 23);
        assert!(!parent_rows.iter().any(|row| row["type"] == "message"));
    }

    #[test]
    fn legacy_pointer_and_transcript_resume_without_a_duplicate_parent_link() {
        let (temp, core, registry) = fixture();
        let (parent_path, parent_id) = {
            let core = core.lock().unwrap();
            let parent = core.store.as_ref().unwrap();
            (parent.path.clone(), parent.session_id().to_string())
        };
        let side_dir = temp
            .path()
            .join("session-artifacts")
            .join(&parent_id)
            .join("btw-legacy-pane");
        let mut side = SessionFile::create("/work", Some(&parent_path.display().to_string()), 0);
        side.path = side_dir.join(session_file_name(side.session_id()));
        side.persist_entry(
            "model_change",
            json!({ "provider": "test-provider", "modelId": "test-model" }),
        )
        .unwrap();
        side.persist_entry(
            "message",
            json!({ "message": { "role": "user", "content": [{"type":"text","text":"old?"}], "timestamp": 1 } }),
        )
        .unwrap();
        side.persist_entry(
            "message",
            json!({ "message": assistant_message_value(&assistant("old answer", 13)) }),
        )
        .unwrap();
        let legacy_pointer = json!({
            "type": "custom",
            "id": "legacy-pointer",
            "parentId": null,
            "timestamp": "2026-08-16T00:00:00.000Z",
            "customType": "side_question",
            "data": {
                "btwId": "legacy-pane",
                "sessionFile": side.path.display().to_string(),
                "sessionId": side.session_id(),
                "question": "old?",
                "startedAt": "2026-08-16T00:00:00.000Z"
            }
        });
        use std::io::Write as _;
        let mut parent = std::fs::OpenOptions::new()
            .append(true)
            .open(&parent_path)
            .unwrap();
        writeln!(parent, "{legacy_pointer}").unwrap();
        parent.sync_all().unwrap();

        let recorder = registry.resolve("pane:legacy-pane", "legacy-pane", true);
        recorder.record_turn("follow-up?", &assistant("new answer", 17), &[]);

        let parent_rows = jsonl(&parent_path);
        assert_eq!(
            parent_rows
                .iter()
                .filter(|row| row["customType"] == SIDE_QUESTION_POINTER_TYPE)
                .count(),
            1,
            "resume must reuse the legacy pointer"
        );
        let side_rows = jsonl(&side.path);
        let messages: Vec<&Value> = side_rows
            .iter()
            .filter(|row| row["type"] == "message")
            .collect();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["message"]["usage"]["totalTokens"], 13);
        assert_eq!(messages[3]["message"]["usage"]["totalTokens"], 17);
    }

    #[test]
    fn legacy_first_turn_starts_a_fresh_transcript_and_status_is_durable() {
        let (_temp, core, registry) = fixture();
        let first = registry.resolve("legacy:client", "run-1", false);
        first.record_turn("one?", &assistant("one", 3), &[]);
        first.record_status(SideQuestionRecordStatus::Error, Some("provider failed"));
        let second = registry.resolve("legacy:client", "run-2", false);
        assert!(!Arc::ptr_eq(&first, &second));
        second.record_turn("new pane?", &assistant("two", 4), &[]);

        let parent_path = core.lock().unwrap().store.as_ref().unwrap().path.clone();
        let parent_rows = jsonl(&parent_path);
        let pointers: Vec<&Value> = parent_rows
            .iter()
            .filter(|row| row["customType"] == SIDE_QUESTION_POINTER_TYPE)
            .collect();
        assert_eq!(pointers.len(), 2);
        let first_side = PathBuf::from(pointers[0]["data"]["sessionFile"].as_str().unwrap());
        assert!(jsonl(&first_side).iter().any(|row| {
            row["customType"] == SIDE_QUESTION_STATUS_TYPE
                && row["data"]["status"] == "error"
                && row["data"]["errorMessage"] == "provider failed"
        }));
    }
}
