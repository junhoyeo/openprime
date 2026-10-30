//! Immutable deletion authority for runtime session ownership.
//!
//! Storage overrides never select a different ledger. Supervised workers use
//! their supervisor's persisted default scope, including when initially fileless.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::descriptor::{PersistedSupervisorConfig, SUPERVISOR_CONFIG_FILE_NAME};
use crate::rlm_ledger::RlmSpawnLedger;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("Cannot revive deleted RLM child: {path}")]
pub(crate) struct DeletedSessionError {
    path: String,
}

/// The agent root and default RLM ledger scope of one runtime invocation.
/// Clones retain the same scope; switching a transcript never changes it.
#[derive(Clone)]
pub struct RuntimeSessionAuthority {
    agent_dir: PathBuf,
    sessions_dir: PathBuf,
    ledger: Arc<RlmSpawnLedger>,
}

impl fmt::Debug for RuntimeSessionAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RuntimeSessionAuthority")
            .field("agent_dir", &self.agent_dir)
            .field("sessions_dir", &self.sessions_dir)
            .finish_non_exhaustive()
    }
}

impl RuntimeSessionAuthority {
    /// Resolve standalone authority from the configured agent/default sessions
    /// root, not a transcript parent or a per-session storage override.
    ///
    /// # Errors
    /// Returns an error when the configured path identity cannot be verified.
    pub fn for_agent_dir(agent_dir: &Path) -> Result<Self> {
        // Ledger namespaces retain the configured spelling. Canonical target
        // identity is not a migration of legacy missing-directory ledger keys.
        let sessions_dir = crate::paths::sessions_dir(agent_dir)?;
        super::checked_session_path(&sessions_dir)?;
        let agent_dir = super::checked_session_path(agent_dir)?;
        Ok(Self::resolved(agent_dir, sessions_dir))
    }

    /// Resolve supervised authority from the configuration bound to this socket.
    /// Missing, malformed, or mismatched configuration is not a default scope.
    ///
    /// # Errors
    /// Returns an error when the configuration or its identities cannot be verified.
    pub fn for_supervisor(agent_dir: &Path, socket: &Path) -> Result<Self> {
        anyhow::ensure!(!socket.as_os_str().is_empty() && socket.is_absolute(),
            "supervised session authority requires an absolute supervisor socket");
        let agent_dir = super::checked_session_path(agent_dir)?;
        let path = crate::descriptor::descriptor_dir(&agent_dir, socket)
            .join(SUPERVISOR_CONFIG_FILE_NAME);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect supervisor authority {}", path.display()))?;
        anyhow::ensure!(metadata.file_type().is_file(), "supervisor authority is not a regular file");
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= MAX_CONFIG_BYTES, "supervisor authority configuration is oversized");
        let config: PersistedSupervisorConfig = serde_json::from_slice(&bytes)
            .context("invalid supervisor authority configuration")?;
        anyhow::ensure!(config.version == 1 && Path::new(&config.socket_path).is_absolute()
            && super::checked_session_path(Path::new(&config.socket_path))?
                == super::checked_session_path(socket)?,
            "supervisor authority does not match the selected socket");
        let configured = config.default_session_dir.as_deref()
            .filter(|path| Path::new(path).is_absolute())
            .ok_or_else(|| anyhow::anyhow!("supervisor authority has no absolute default sessions directory"))?;
        let sessions_dir = PathBuf::from(configured);
        super::checked_session_path(&sessions_dir)?;
        Ok(Self::resolved(agent_dir, sessions_dir))
    }

    fn resolved(agent_dir: PathBuf, sessions_dir: PathBuf) -> Self {
        let ledger = Arc::new(RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {}));
        Self { agent_dir, sessions_dir, ledger }
    }

    /// The configured agent root. It is independent of transcript storage.
    #[must_use]
    pub(crate) fn agent_dir(&self) -> &Path { &self.agent_dir }

    pub(crate) fn ledger(&self) -> &Arc<RlmSpawnLedger> { &self.ledger }

    /// Acquire checked runtime ownership before the target is opened or written.
    ///
    /// # Errors
    /// Returns an error on a deleted/unknown authority, a held/unknown lease,
    /// unstable target identity, or coordination failure.
    pub fn acquire(&self, target: &Path) -> Result<super::SessionLease> {
        super::with_runtime_session_fence(&self.agent_dir, target, |held| {
            held.acquire_runtime_lease(self)
        })
    }

    /// Cleanup proof for one already-committed child deletion, called under M.
    /// This is not admission permission or authorization to discard private data.
    pub(crate) fn check_committed_deletion(&self, target: &Path, child_id: &str) -> Result<()> {
        anyhow::ensure!(!child_id.is_empty() && !target.as_os_str().is_empty(), "Deleted child identity is incomplete");
        let target = super::checked_session_path(target)?;
        let mut matches = Vec::new();
        for edge in self.ledger.strict_edges()? {
            anyhow::ensure!(Path::new(&edge.child).is_absolute(), "Deletion authority contains a non-absolute child identity");
            if super::checked_session_path(Path::new(&edge.child))? == target { matches.push(edge); }
        }
        anyhow::ensure!(matches.len() == 1, "Committed child deletion is missing or ambiguous");
        let edge = &matches[0];
        anyhow::ensure!(edge.child_id == child_id && edge.deleted.is_some(),
            "Exact child deletion is not committed in this authority scope");
        Ok(())
    }

    pub(crate) fn is_deleted_refusal(error: &anyhow::Error) -> bool {
        error.downcast_ref::<DeletedSessionError>().is_some()
    }

    /// Called only while the canonical target's common lease fence is held.
    pub(crate) fn check_admission(&self, target: &Path) -> Result<()> {
        anyhow::ensure!(!target.as_os_str().is_empty(), "Fileless session has no runtime target");
        let target = super::checked_session_path(target)?;
        match std::fs::metadata(&target) {
            Ok(metadata) => anyhow::ensure!(metadata.is_file(), "Session target is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).context("inspect session target"),
        }
        for edge in self.ledger.strict_edges()? {
            anyhow::ensure!(Path::new(&edge.child).is_absolute(),
                "session authority contains a non-absolute child identity");
            let child = super::checked_session_path(Path::new(&edge.child))?;
            if edge.deleted.is_some() && child == target {
                return Err(DeletedSessionError { path: target.display().to_string() }.into());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn issue_42_authority_keeps_a_missing_default_roots_legacy_alias_bucket() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let default_sessions = alias.join("missing-default");
        let agent = root.path().join("agent");
        let socket = root.path().join("supervisor.sock");
        crate::descriptor::persist_supervisor_config(
            &crate::descriptor::descriptor_dir(&agent, &socket).join(SUPERVISOR_CONFIG_FILE_NAME),
            &PersistedSupervisorConfig { version: 1, socket_path: socket.to_string_lossy().into_owned(),
                default_session_dir: Some(default_sessions.to_string_lossy().into_owned()) },
        ).unwrap();
        let storage = root.path().join("custom-storage");
        std::fs::create_dir(&storage).unwrap();
        let child = storage.join("child.jsonl");
        let parent = storage.join("parent.jsonl");
        for path in [&child, &parent] {
            std::fs::write(path, format!("{}\n", serde_json::json!({
                "type": "session", "version": 3,
                "id": path.file_stem().unwrap().to_string_lossy(),
                "timestamp": "t", "cwd": root.path(),
            }))).unwrap();
        }
        let ledger = RlmSpawnLedger::new(&agent, &default_sessions, |_| {});
        ledger.append_spawn(&crate::rlm_ledger::RlmSpawnInput {
            child_id: "sub-child".into(), parent: parent.to_string_lossy().into_owned(),
            child: child.to_string_lossy().into_owned(), depth: 1, name: "child".into(),
        }).unwrap();
        ledger.append_delete("sub-child", child.to_str().unwrap(), crate::rlm_ledger::RlmLedgerDeleteReason::User).unwrap();
        let before = std::fs::read(ledger.ledger_path()).unwrap();
        assert!(!default_sessions.exists(), "custom storage must leave the default root missing");
        let authority = RuntimeSessionAuthority::for_supervisor(&agent, &socket).unwrap();
        assert_eq!(authority.ledger.ledger_path(), ledger.ledger_path());
        assert!(authority.acquire(&child).is_err(), "the real acquire cannot shed the legacy tombstone");
        assert_eq!(std::fs::read(ledger.ledger_path()).unwrap(), before);
        // Namespace continuity is also required after the formerly missing root
        // appears. This remains a real regression until legacy-key selection
        // composes; raw caller spelling alone is not a complete fix.
        std::fs::create_dir_all(&default_sessions).unwrap();
        assert!(authority.acquire(&child).is_err(), "captured authority must retain or refuse its namespace");
        let reopened = RuntimeSessionAuthority::for_supervisor(&agent, &socket).unwrap();
        assert!(reopened.acquire(&child).is_err(), "fresh authority must not select an empty bucket after root creation");
        assert_eq!(std::fs::read(ledger.ledger_path()).unwrap(), before);
    }
}
