//! Serialize file-bound launches with committed RLM child deletion. The
//! ledger, never a revival record or roster row, owns deletion authority.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{bail, Result};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::Supervisor;

#[derive(Default)]
pub(crate) struct ChildLifecycles {
    gates: Mutex<HashMap<std::path::PathBuf, Weak<AsyncMutex<()>>>>,
}

impl ChildLifecycles {
    pub(crate) async fn lock(&self, file: &str) -> Result<OwnedMutexGuard<()>> {
        let path = crate::paths::expand_tilde(file)?;
        let key = crate::lease::canonical_session_path(&std::path::absolute(path)?);
        let gate = {
            let mut gates = self.gates.lock().expect("child lifecycle gates");
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(AsyncMutex::new(()));
                gates.insert(key, Arc::downgrade(&gate));
                gate
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), gate.lock_owned())
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("Session lifecycle operation is still pending: {file}"),
                )
                .into()
            })
    }
}

impl Supervisor {
    pub(crate) async fn ensure_child_not_deleted(self: &Arc<Self>, file: &str) -> Result<()> {
        if self.child_is_deleted(file).await? {
            bail!("Cannot revive deleted RLM child: {file}");
        }
        Ok(())
    }

    pub(super) async fn child_is_deleted(self: &Arc<Self>, file: &str) -> Result<bool> {
        let target = crate::lease::canonical_session_path(&std::path::absolute(
            crate::paths::expand_tilde(file)?,
        )?);
        let ledger = self.rlm_spawn_ledger_for(None).await?;
        Ok(ledger.strict_edges()?.iter().any(|edge| {
            edge.deleted.is_some()
                && crate::lease::canonical_session_path(Path::new(&edge.child)) == target
        }))
    }

    /// A persisted worker may predate the delete's stop descriptor. The ledger
    /// wins after restart too; unknown ledger state refuses adoption untouched.
    pub(super) async fn stop_ledger_deleted_worker(
        self: &Arc<Self>,
        resident: &Arc<crate::registry::ResidentWorker>,
    ) -> Result<bool> {
        let file = resident.descriptor.lock().await.session_file.clone();
        let Some(file) = file else {
            return Ok(false);
        };
        if !self.child_is_deleted(&file).await? {
            return Ok(false);
        }
        self.persist_stop_tombstone(resident).await?;
        if let Err(error) = self.passivated_launches.forget_deleted(&file) {
            self.log_line(&format!("deleted child launch record retained: {error:#}"));
        }
        self.finish_tombstoned_stop(resident, true).await;
        Ok(true)
    }
    /// The first logical delete commits under the same process-shared fence
    /// that every checked runtime acquisition uses. Registry presence is not
    /// runtime ownership; an unrelated or unknown holder always refuses.
    // Kept async for caller consistency (5 call sites across production and
    // tests already `.await` it); its body happens to be synchronous today,
    // not a reason to churn every caller for a style-only clippy preference.
    #[allow(clippy::unused_async_trait_impl, clippy::unused_async)]
    pub(super) async fn commit_child_deletion(
        self: &Arc<Self>,
        file: &str,
        child_id: &str,
        reason: crate::rlm_ledger::RlmLedgerDeleteReason,
        resident: Option<&Arc<crate::registry::ResidentWorker>>,
    ) -> Result<()> {
        let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
            &self.options.agent_dir,
            &self.options.socket_path,
        )?;
        let ledger = Arc::clone(authority.ledger());
        crate::lease::with_runtime_session_fence(authority.agent_dir(), Path::new(file), |held| {
            let target = held.canonical_path().to_path_buf();
            let mut matching = Vec::new();
            for edge in ledger.strict_edges()? {
                let path = crate::lease::checked_session_path(Path::new(&edge.child))?;
                if edge.child_id == child_id && path == target {
                    matching.push(edge);
                }
            }
            anyhow::ensure!(
                matching.len() == 1,
                "RLM child identity is missing or ambiguous in the spawn ledger"
            );
            let edge = matching.pop().expect("one verified edge");
            let ownership = held.inspect_ownership()?;
            let mut descriptor = resident
                .map(|resident| {
                    resident.descriptor.try_lock().map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "Worker identity is changing; delete deferred",
                        )
                    })
                })
                .transpose()?;
            if let Some(descriptor) = descriptor.as_ref() {
                let file = descriptor
                    .session_file
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Worker has no file identity"))?;
                anyhow::ensure!(
                    crate::lease::checked_session_path(Path::new(file))? == target,
                    "Worker identity changed before deletion"
                );
            }
            match ownership {
                crate::lease::RuntimeLeaseOwnership::Absent
                | crate::lease::RuntimeLeaseOwnership::Stale => {}
                crate::lease::RuntimeLeaseOwnership::Unknown => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "Runtime ownership is unknown; deletion deferred",
                    )
                    .into());
                }
                crate::lease::RuntimeLeaseOwnership::Held(owner) => {
                    let Some(descriptor) = descriptor.as_ref() else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "Another runtime owns this child; deletion deferred",
                        )
                        .into());
                    };
                    anyhow::ensure!(
                        descriptor.pid == u64::from(owner.pid)
                            && descriptor.process_start_id.as_deref()
                                == Some(owner.process_start_id.as_str())
                            && owner.active_session_id.as_deref()
                                == Some(descriptor.root_active_session_id.as_str()),
                        "Runtime ownership does not match the authenticated child worker"
                    );
                }
            }
            // #42: the logical deletion commits first. Only an Ok append
            // proves the tombstone; a failed append leaves the worker live
            // and unquarantined, because nothing was deleted.
            if edge.deleted.is_none() {
                ledger.append_delete(child_id, file, reason)?;
            }
            #[cfg(test)]
            if self
                .lifecycle_test_hooks
                .crash_after_tombstone
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                bail!("injected crash after the tombstone commit");
            }
            if let (Some(resident), Some(descriptor)) = (resident, descriptor.as_mut()) {
                let mut stopped = (**descriptor).clone();
                stopped
                    .stop_requested_at
                    .get_or_insert_with(crate::util::now_iso);
                stopped.archive_on_stop = Some(true);
                crate::descriptor::persist_worker(&resident.descriptor_path, &stopped)?;
                **descriptor = stopped;
                resident.mark_identity_quarantined();
                resident
                    .intentional_stop
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
