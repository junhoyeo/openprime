//! The launch contexts of stopped sessions — idle-passivated or archived by
//! a kill (legacy TS rows e32bce384 / 48fae2a5a / 843615757 / 837be6824):
//! the stop retires the worker's descriptor, so without this record a later wake
//! recreated the session from its file header alone — the header's
//! recorded cwd (a missing directory for a transcript the user reopened
//! with a fallback cwd), daemon-default telemetry (an opted-out client's
//! attach then fails), and no opened-with runtime config. The wake replays
//! the durable create command the stopped worker ran with instead.
//!
//! Durable beside the descriptors (one record per canonical session file):
//! the TS window held its revive config in the client process, so a daemon
//! restart never lost it, and a supervisor restart must not lose it here.

use std::path::{Path, PathBuf};

use pa_types::daemon::{DaemonCommand, DurableDaemonCreateCommand};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The non-secret launch context a record keeps (an allowlist of the
/// durable create's runtime keys): never a CLI `apiKey` and never the
/// client env — the record must not become a new secret-at-rest location
/// (the descriptor holding them dies with the stop). A revived session
/// resolves its provider credential the normal way at wake time.
const PERSISTED_LAUNCH_KEYS: [&str; 18] = [
    "cwd",
    "sessionDir",
    "script",
    "provider",
    "model",
    "thinking",
    "models",
    "rlmDepth",
    "rlmMaxDepth",
    "parentSessionPath",
    "childScript",
    "spawnedByRequestId",
    "systemPrompt",
    "appendSystemPrompt",
    "skills",
    "promptTemplates",
    "autonomous",
    "executionMode",
];

/// One passivated session's opened-with launch context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PassivatedLaunch {
    create_command: DurableDaemonCreateCommand,
    pub(crate) telemetry_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

impl PassivatedLaunch {
    /// Keep only the allowlisted non-secret launch context of `create_command`.
    pub(crate) fn new(
        create_command: &DurableDaemonCreateCommand,
        telemetry_disabled: Option<bool>,
    ) -> Self {
        let rest = create_command
            .rest
            .iter()
            .filter(|(key, _)| PERSISTED_LAUNCH_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            create_command: DurableDaemonCreateCommand {
                session_path: create_command.session_path.clone(),
                no_session: None,
                rest,
            },
            telemetry_disabled,
            session_id: None,
        }
    }

    /// The wake create over `session_path`: the stopped worker's runtime
    /// config and telemetry opt-out, with the wake path's own `rlmDepth`
    /// (the passivation fence reads it) and runtime metadata.
    pub(crate) fn wake_create(
        &self,
        session_path: &str,
        rlm_depth: u64,
        runtime_metadata: Option<Value>,
    ) -> DaemonCommand {
        // The allowlist applies on read too: a record written before it (the
        // never-released whole-create format) can never replay a credential.
        let mut config: Map<String, Value> = self
            .create_command
            .rest
            .iter()
            .filter(|(key, _)| PERSISTED_LAUNCH_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        config.insert("rlmDepth".to_string(), Value::from(rlm_depth));
        DaemonCommand::Create {
            id: None,
            session_path: Some(session_path.to_string()),
            continue_recent: Some(false),
            no_session: None,
            name: None,
            config: Some(Value::Object(config)),
            telemetry_disabled: self.telemetry_disabled.filter(|disabled| *disabled),
            runtime_metadata,
            lifecycle: None,
            // Never the creator's pane identity: passivation requires zero
            // attached clients, so that pane may be closed or reused. The
            // waking client's attach adopts its own pane (adopt-if-absent),
            // as the TS revival sent the reviving client's env.
            env: None,
            // The client launch env never outlives a resident's create (see
            // `ResidentWorker::launch_env`): a wake runs on the daemon's env,
            // exactly like a crash relaunch.
            launch_env: None,
            rest: Map::default(),
        }
    }
}

/// The passivated launch records: `<dir>/<sha256(canonical session file)>.json`.
#[derive(Debug)]
pub(crate) struct PassivatedLaunches {
    dir: PathBuf,
}

impl PassivatedLaunches {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn record_path(&self, session_file: &str) -> PathBuf {
        let canonical = crate::lease::canonical_session_path(Path::new(session_file));
        let name = crate::paths::hash_key(&canonical.to_string_lossy(), 64);
        self.dir.join(format!("{name}.json"))
    }

    /// Record the context a passivated worker ran `session_file` with. A
    /// failed write keeps the header fallback for that session's wake.
    pub(crate) fn remember(
        &self,
        session_file: &str,
        launch: &PassivatedLaunch,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(Path::new(session_file).is_absolute(), "launch record requires an absolute session path");
        let canonical = crate::lease::canonical_session_path(Path::new(session_file));
        if let Some(recorded) = launch.create_command.session_path.as_deref() {
            anyhow::ensure!(
                Path::new(recorded).is_absolute()
                    && crate::lease::canonical_session_path(Path::new(recorded)) == canonical,
                "launch record belongs to a different session"
            );
        }
        self.get_checked(session_file)?;
        crate::paths::ensure_dir(&self.dir)?;
        let mut stored = PassivatedLaunch::new(&launch.create_command, launch.telemetry_disabled);
        stored.create_command.session_path = Some(canonical.to_string_lossy().into_owned());
        stored.session_id = pa_core::session::manager::read_session_header(&canonical).map(|header| header.id);
        let content = serde_json::to_string_pretty(&stored)?;
        crate::descriptor::write_file_atomic(&self.record_path(session_file), &content)
    }

    /// The recorded context for `session_file`, if its worker passivated.
    pub(crate) fn get(&self, session_file: &str) -> Option<PassivatedLaunch> {
        self.get_checked(session_file).ok().flatten()
    }

    /// Drop the record once a worker hosts the session again (any launch
    /// over the file: the new resident's descriptor owns the context), when
    /// a passivation stop fails, or when the session is deleted.
    pub(crate) fn forget(&self, session_file: &str) {
        let _ = self.forget_deleted(session_file);
    }

    /// Only app-owned records with the addressed durable identity may retire.
    /// Unknown, malformed, replaced, and foreign metadata stays for inspection.
    pub(crate) fn forget_deleted(&self, session_file: &str) -> anyhow::Result<()> {
        if self.get_checked(session_file)?.is_some() {
            match std::fs::remove_file(self.record_path(session_file)) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// A wake must distinguish genuine absence from unreadable or foreign context.
    pub(crate) fn get_checked(&self, session_file: &str) -> anyhow::Result<Option<PassivatedLaunch>> {
        anyhow::ensure!(Path::new(session_file).is_absolute(), "launch record requires an absolute session path");
        let path = self.record_path(session_file);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => anyhow::ensure!(metadata.file_type().is_file(), "launch record is not a regular file: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let launch: PassivatedLaunch = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        let canonical = crate::lease::canonical_session_path(Path::new(session_file));
        anyhow::ensure!(launch.create_command.session_path.as_deref().is_some_and(|recorded| {
            Path::new(recorded).is_absolute()
                && crate::lease::canonical_session_path(Path::new(recorded)) == canonical
        }), "launch record belongs to a different session");
        if let Some(expected) = launch.session_id.as_deref() {
            match pa_core::session::manager::read_session_header(&canonical) {
                Some(header) => anyhow::ensure!(header.id == expected, "launch record session identity changed"),
                None => anyhow::ensure!(!canonical.try_exists()?, "launch record session identity cannot be verified"),
            }
        }
        Ok(Some(launch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wake create is the stopped worker's create with the wake path's
    /// identity: config keys survive; the api key, the creator's pane env,
    /// the name, and the metadata never ride along.
    #[test]
    fn the_wake_create_replays_the_opened_with_context() {
        let durable = crate::descriptor::durable_create_command(&json!({
            "type": "create",
            "sessionPath": "/old/path.jsonl",
            "cwd": "/fallback",
            "sessionDir": "/sessions",
            "model": "mock-1",
            "apiKey": "sk-secret",
            "appendSystemPrompt": ["marker"],
            "name": "alpha",
            "env": {"HERDR_PANE_ID": "pane-1"},
            "runtimeMetadata": {"kind": "subagent"},
            "rlmDepth": 0,
        }));
        let launch = PassivatedLaunch::new(&durable, Some(true));
        let create = launch.wake_create("/saved/file.jsonl", 2, None);
        let expected = DaemonCommand::Create {
            id: None,
            session_path: Some("/saved/file.jsonl".to_string()),
            continue_recent: Some(false),
            no_session: None,
            name: None,
            config: Some(json!({
                "cwd": "/fallback",
                "sessionDir": "/sessions",
                "model": "mock-1",
                "appendSystemPrompt": ["marker"],
                "rlmDepth": 2,
            })),
            telemetry_disabled: Some(true),
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: Map::default(),
        };
        assert_eq!(create, expected);
    }

    /// The record is durable: a fresh store over the same directory (a
    /// restarted supervisor) reads it back, by any spelling of the file.
    #[test]
    fn a_record_survives_a_fresh_store_and_is_spent_by_forget() {
        let dir = tempfile::tempdir().unwrap();
        let session_file = dir.path().join("session.jsonl");
        std::fs::write(&session_file, "").unwrap();
        let launch = PassivatedLaunch::new(
            &crate::descriptor::durable_create_command(&json!({ "cwd": "/fallback" })),
            Some(true),
        );
        let records = dir.path().join("passivated-launches");
        PassivatedLaunches::new(records.clone())
            .remember(&session_file.to_string_lossy(), &launch)
            .unwrap();
        let restarted = PassivatedLaunches::new(records);
        let spelled = dir.path().join(".").join("session.jsonl");
        assert_eq!(
            restarted.get(&spelled.to_string_lossy()).unwrap().wake_create("/wake.jsonl", 0, None),
            launch.wake_create("/wake.jsonl", 0, None),
        );
        restarted.forget(&session_file.to_string_lossy());
        assert_eq!(restarted.get(&session_file.to_string_lossy()), None);
    }
    #[test]
    fn issue_42_old_records_filter_secrets_and_unknown_identity_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("child.jsonl");
        let other = dir.path().join("sibling.jsonl");
        std::fs::write(&file, "{\"type\":\"session\",\"id\":\"child-id\",\"version\":3,\"cwd\":\"/missing\",\"timestamp\":\"t\"}\n").unwrap();
        let records = PassivatedLaunches::new(dir.path().join("records"));
        let old = json!({
            "createCommand": {"sessionPath": file, "cwd": "/fallback", "apiKey": "synthetic-secret", "env": {"TOKEN": "synthetic-secret"}},
            "telemetryDisabled": true,
        });
        std::fs::create_dir_all(&records.dir).unwrap();
        let path = records.record_path(file.to_str().unwrap());
        std::fs::write(&path, old.to_string()).unwrap();
        let legacy = records.get(file.to_str().unwrap()).unwrap();
        let wake = serde_json::to_value(legacy.wake_create(file.to_str().unwrap(), 1, None)).unwrap();
        assert!(!wake.to_string().contains("synthetic-secret"));
        records.remember(file.to_str().unwrap(), &legacy).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("synthetic-secret"));
        for recorded in [json!(other), json!("../child.jsonl"), Value::Null] {
            let mut foreign = old.clone();
            foreign["createCommand"]["sessionPath"] = recorded;
            let bytes = foreign.to_string();
            std::fs::write(&path, &bytes).unwrap();
            assert!(records.get(file.to_str().unwrap()).is_none());
            assert!(records.forget_deleted(file.to_str().unwrap()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
        std::fs::write(&path, old.to_string()).unwrap();
        records.remember(file.to_str().unwrap(), &legacy).unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::write(&file, "{\"type\":\"session\",\"id\":\"replaced-id\",\"version\":3,\"cwd\":\"/missing\",\"timestamp\":\"t\"}\n").unwrap();
        assert!(records.get(file.to_str().unwrap()).is_none());
        assert!(records.forget_deleted(file.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

}
