//! The launch contexts of idle-passivated sessions (legacy TS rows
//! e32bce384 / 48fae2a5a / 843615757 / 837be6824): the passivation stop
//! retires the worker's descriptor, so without this record a later wake
//! recreated the session from its file header alone — the header's
//! recorded cwd (a missing directory for a transcript the user reopened
//! with a fallback cwd), daemon-default telemetry (an opted-out client's
//! attach then fails), and no opened-with runtime config. The wake replays
//! the durable create command the stopped worker ran with instead.
//!
//! In memory only, like the update roster's launch snapshot: a supervisor
//! restart forgets the record and a later wake falls back to the header.

use std::collections::HashMap;
use std::sync::Mutex;

use pa_types::daemon::{DaemonCommand, DurableDaemonCreateCommand};
use serde_json::{Map, Value};

/// The durable create-command keys that are not runtime config: the client
/// env and the runtime metadata ride their own create fields, and the name
/// already lives in the session file (a wake must not re-run the name
/// admission).
const NON_CONFIG_KEYS: [&str; 4] = ["env", "runtimeMetadata", "name", "rlmChildId"];

/// One passivated session's opened-with launch context.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PassivatedLaunch {
    create_command: DurableDaemonCreateCommand,
    telemetry_disabled: Option<bool>,
}

impl PassivatedLaunch {
    pub(crate) fn new(
        create_command: DurableDaemonCreateCommand,
        telemetry_disabled: Option<bool>,
    ) -> Self {
        Self {
            create_command,
            telemetry_disabled,
        }
    }

    /// The wake create over `session_path`: the stopped worker's runtime
    /// config, client env, and telemetry opt-out, with the wake path's own
    /// `rlmDepth` (the passivation fence reads it) and runtime metadata.
    pub(crate) fn wake_create(
        &self,
        session_path: &str,
        rlm_depth: u64,
        runtime_metadata: Option<Value>,
    ) -> DaemonCommand {
        let mut config: Map<String, Value> = self
            .create_command
            .rest
            .iter()
            .filter(|(key, _)| !NON_CONFIG_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        config.insert("rlmDepth".to_string(), Value::from(rlm_depth));
        let env = self
            .create_command
            .rest
            .get("env")
            .and_then(|env| serde_json::from_value(env.clone()).ok());
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
            env,
            // The client launch env never outlives a resident's create (see
            // `ResidentWorker::launch_env`): a wake runs on the daemon's env,
            // exactly like a crash relaunch.
            launch_env: None,
            rest: Map::default(),
        }
    }
}

/// The passivated launch contexts, keyed by canonical session file.
#[derive(Debug, Default)]
pub(crate) struct PassivatedLaunches {
    by_session_file: Mutex<HashMap<String, PassivatedLaunch>>,
}

impl PassivatedLaunches {
    fn key(session_file: &str) -> String {
        crate::lease::canonical_session_path(std::path::Path::new(session_file))
            .to_string_lossy()
            .to_string()
    }

    /// Record the context a passivated worker ran `session_file` with.
    pub(crate) fn remember(&self, session_file: &str, launch: PassivatedLaunch) {
        self.by_session_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(Self::key(session_file), launch);
    }

    /// The recorded context for `session_file`, if its worker passivated.
    pub(crate) fn get(&self, session_file: &str) -> Option<PassivatedLaunch> {
        self.by_session_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&Self::key(session_file))
            .cloned()
    }

    /// Drop the record once a worker hosts the session again (any launch
    /// over the file): the new resident's descriptor owns the context.
    pub(crate) fn forget(&self, session_file: &str) {
        self.by_session_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&Self::key(session_file));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wake create is the stopped worker's create with the wake path's
    /// identity: config keys survive, the env moves to its own field, the
    /// name and metadata never ride the config.
    #[test]
    fn the_wake_create_replays_the_opened_with_context() {
        let durable = crate::descriptor::durable_create_command(&json!({
            "type": "create",
            "sessionPath": "/old/path.jsonl",
            "cwd": "/fallback",
            "sessionDir": "/sessions",
            "model": "mock-1",
            "appendSystemPrompt": ["marker"],
            "name": "alpha",
            "env": {"HERDR_PANE_ID": "pane-1"},
            "runtimeMetadata": {"kind": "subagent"},
            "rlmDepth": 0,
        }));
        let launch = PassivatedLaunch::new(durable, Some(true));
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
            env: Some(std::collections::BTreeMap::from([(
                "HERDR_PANE_ID".to_string(),
                "pane-1".to_string(),
            )])),
            launch_env: None,
            rest: Map::default(),
        };
        assert_eq!(create, expected);
    }
}
