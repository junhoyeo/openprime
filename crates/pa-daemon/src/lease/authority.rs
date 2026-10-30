//! Immutable deletion authority for runtime session ownership.
//!
//! Storage overrides never select a different ledger. Supervised workers use
//! their supervisor's persisted default scope, including when initially fileless.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::descriptor::{PersistedSupervisorConfig, SUPERVISOR_CONFIG_FILE_NAME};
use crate::rlm_ledger::RlmSpawnLedger;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

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
        let agent_dir = super::checked_session_path(agent_dir)?;
        let sessions_dir = super::checked_session_path(&crate::paths::sessions_dir(&agent_dir)?)?;
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
        let sessions_dir = super::checked_session_path(Path::new(configured))?;
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
                bail!("Cannot revive deleted RLM child: {}", target.display());
            }
        }
        Ok(())
    }
}
