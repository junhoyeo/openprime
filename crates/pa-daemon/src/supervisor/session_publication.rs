//! Final-identity publication under the process-shared session fence.
//! Network state collection happens before this synchronous, try-lock commit.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use pa_types::daemon::agent_roster::AgentRosterEntry;
use pa_types::daemon::DaemonWorkerLifecycle;
use serde_json::Value;

use crate::registry::ResidentWorker;
use crate::session_bindings::SessionBinding;
use crate::supervisor::Supervisor;

#[derive(Clone, Copy)]
pub(crate) enum SummaryOrder<'a> {
    Pull { instance: &'a str, sequence: Option<u64> },
    Delta { instance: &'a str, sequence: u64 },
}

pub(crate) struct PublishedSummary {
    pub(crate) entry: AgentRosterEntry,
    pub(crate) removed: Vec<String>,
    pub(crate) superseded: Option<(Vec<String>, Arc<SessionBinding>)>,
}

impl Supervisor {
    pub(crate) fn publish_verified_summary(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        summary: &Value,
        order: SummaryOrder<'_>,
        removed: &[String],
    ) -> Result<Option<PublishedSummary>> {
        let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
            &self.options.agent_dir, &self.options.socket_path,
        )?;
        let active = summary.get("activeSessionId").or_else(|| summary.get("id"))
            .and_then(Value::as_str).unwrap_or_default();
        let is_root = active == resident.worker_id;
        anyhow::ensure!(!matches!(order, SummaryOrder::Pull { .. }) || is_root,
            "Authoritative worker summary does not identify its authenticated root");
        let file = summary.get("sessionFile").and_then(Value::as_str).filter(|file| !file.is_empty());
        let busy = || std::io::Error::new(std::io::ErrorKind::WouldBlock, "Session publication is contended; retry");
        let commit = |held: Option<&mut crate::lease::HeldRuntimeSessionFence<'_>>| -> Result<Option<PublishedSummary>> {
            let owner = match held.as_ref() {
                Some(held) => {
                    authority.check_admission(held.canonical_path())?;
                    Some(held.inspect_ownership()?)
                }
                None => None,
            };
            // M -> descriptor -> registry -> roster -> binding slots. Every
            // acquisition is a try-lock; no data guard waits for another.
            let mut descriptor = resident.descriptor.try_lock().map_err(|_| busy())?;
            let mut workers = self.registry.try_publication()?;
            let mut roster = self.roster.try_lock().map_err(|_| busy())?;
            let mut bindings = self.session_bindings.try_writer()?;
            anyhow::ensure!(descriptor.stop_requested_at.is_none()
                && !resident.intentional_stop.load(std::sync::atomic::Ordering::SeqCst),
                "Session worker is stopping; publication refused");
            if let Some(owner) = owner {
                if is_root {
                    let crate::lease::RuntimeLeaseOwnership::Held(owner) = owner else {
                        anyhow::bail!("Final session runtime ownership is not verified");
                    };
                    anyhow::ensure!(descriptor.pid == u64::from(owner.pid)
                        && descriptor.process_start_id.as_deref() == Some(owner.process_start_id.as_str())
                        && owner.active_session_id.as_deref() == Some(resident.worker_id.as_str()),
                        "Final session belongs to another runtime generation");
                }
            } else {
                anyhow::ensure!(is_root && descriptor.create_command.no_session == Some(true)
                    && descriptor.session_file.as_deref().is_none_or(str::is_empty),
                    "Missing final session file is not verified fileless state");
            }
            let instance = match order {
                SummaryOrder::Pull { instance, .. } | SummaryOrder::Delta { instance, .. } => instance,
            };
            anyhow::ensure!(descriptor.worker_instance_id.as_deref().unwrap_or("") == instance,
                "Worker generation changed before publication");
            let accepted = match order {
                SummaryOrder::Pull { instance, sequence } => roster.pull_is_current(&resident.worker_id, instance, sequence),
                SummaryOrder::Delta { instance, sequence } => roster.delta_is_current(&resident.worker_id, instance, sequence),
            };
            if !accepted { return Ok(None); }
            if let Some(other) = workers.get(&resident.worker_id) {
                anyhow::ensure!(Arc::ptr_eq(other, resident), "Registry generation changed before publication");
            }
            let mut next = descriptor.clone();
            if is_root {
                if let Some(file) = file {
                    let session_id = summary.get("sessionId").and_then(Value::as_str)
                        .filter(|id| !id.is_empty()).ok_or_else(|| anyhow::anyhow!("Final session has no durable id"))?;
                    next.root_session_id = Some(session_id.to_string());
                    next.session_file = Some(file.to_string());
                    next.create_command.session_path = Some(file.to_string());
                    next.create_command.no_session = None;
                    if let Some(cwd) = summary.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) {
                        next.create_command.rest.insert("cwd".to_string(), Value::from(cwd));
                    }
                }
                next.lifecycle = DaemonWorkerLifecycle::Ready;
                crate::descriptor::persist_worker(&resident.descriptor_path, &next)
                    .context("persist final session identity before publication")?;
                *descriptor = next;
            }
            match order {
                SummaryOrder::Pull { instance, sequence } => { roster.accept_roster_pull(&resident.worker_id, instance, sequence); },
                SummaryOrder::Delta { instance, sequence } => { roster.accept_delta_sequence(&resident.worker_id, instance, sequence); },
            }
            let entry = roster.write_summary(summary.clone(), Some(&resident.worker_id), None);
            let mut removed_ids = roster.swapped_out_root_rows(&resident.worker_id, &entry);
            removed_ids.extend(removed.iter().filter(|id| roster.get(id).is_some()).cloned());
            for id in &removed_ids { roster.delete(id); }
            let superseded = if is_root {
                let result = bindings.record(&resident.worker_id, descriptor.root_session_id.as_deref(), descriptor.session_file.as_deref());
                workers.insert(resident.worker_id.clone(), Arc::clone(resident));
                resident.clear_identity_persist_pending();
                resident.clear_identity_quarantine();
                resident.note_session_ready();
                result
            } else { None };
            Ok(Some(PublishedSummary { entry, removed: removed_ids, superseded }))
        };
        match file {
            Some(file) => crate::lease::with_runtime_session_fence(authority.agent_dir(), Path::new(file), |held| commit(Some(held))),
            None => commit(None),
        }
    }
    pub(crate) async fn reject_failed_publication(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        reported: Option<&Value>,
        error: &anyhow::Error,
    ) -> Result<bool> {
        if reported.is_some_and(|summary| summary.get("activeSessionId").or_else(|| summary.get("id"))
            .and_then(Value::as_str).is_some_and(|id| id != resident.worker_id)) {
            return Ok(false);
        }
        let Some(target) = crate::lease::RuntimeSessionAuthority::deleted_target(error) else {
            resident.mark_identity_quarantined();
            return Ok(false);
        };
        let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
            &self.options.agent_dir, &self.options.socket_path,
        )?;
        let stopped = crate::lease::with_runtime_session_fence(authority.agent_dir(), Path::new(target), |held| {
            let denied = authority.check_admission(held.canonical_path()).err()
                .ok_or_else(|| anyhow::anyhow!("Deleted-session authority changed before retirement"))?;
            anyhow::ensure!(crate::lease::RuntimeSessionAuthority::is_deleted_refusal(&denied),
                "Session retirement authority is unknown");
            let mut descriptor = resident.descriptor.try_lock().map_err(|_| std::io::Error::new(
                std::io::ErrorKind::WouldBlock, "Worker identity is changing; retirement deferred"))?;
            let roster = self.roster.try_lock().map_err(|_| std::io::Error::new(
                std::io::ErrorKind::WouldBlock, "Worker roster is changing; retirement deferred"))?;
            if let Some(summary) = reported {
                let Some(instance) = summary.get("workerInstanceId").and_then(Value::as_str) else {
                    resident.mark_identity_quarantined();
                    return Ok(false);
                };
                if descriptor.worker_instance_id.as_deref() != Some(instance)
                    || !roster.pull_is_current(&resident.worker_id, instance,
                        summary.get("rosterDeltaSequence").and_then(Value::as_u64)) {
                    return Ok(false);
                }
            } else if descriptor.session_file.as_deref().map(|file| crate::lease::checked_session_path(Path::new(file)))
                .transpose()?.as_deref() != Some(held.canonical_path()) {
                return Ok(false);
            }
            let mut next = descriptor.clone();
            next.session_file = Some(target.to_string());
            next.create_command.session_path = Some(target.to_string());
            next.create_command.no_session = None;
            if let Some(id) = reported.and_then(|summary| summary.get("sessionId")).and_then(Value::as_str) {
                next.root_session_id = Some(id.to_string());
            }
            next.stop_requested_at.get_or_insert_with(crate::util::now_iso);
            next.archive_on_stop = Some(true);
            // Quarantine even if persistence refuses, but never claim durable
            // retirement or send a definitive refusal until the write succeeds.
            resident.mark_identity_quarantined();
            crate::descriptor::persist_worker(&resident.descriptor_path, &next)?;
            *descriptor = next;
            resident.intentional_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            resident.note_retired();
            Ok(true)
        })?;
        if stopped {
            let supervisor = Arc::clone(self);
            let resident = Arc::clone(resident);
            tokio::spawn(async move {
                if let Err(error) = supervisor.stop_worker(&resident).await {
                    supervisor.log_line(&format!("deleted worker retirement remains pending: {error:#}"));
                }
            });
        }
        Ok(stopped)
    }

    pub(crate) fn announce_verified_summary(&self, published: &PublishedSummary) {
        if let Some((previous_ids, binding)) = &published.superseded {
            for previous in previous_ids {
                self.publish_session_event(previous, &Arc::new(serde_json::json!({
                    "type":"session_binding", "previousActiveSessionId":previous,
                    "activeSessionId":binding.active_session_id, "sessionId":binding.session_id,
                    "sessionFile":binding.session_file,
                })));
            }
        }
        self.push_roster_update(vec![published.entry.clone()], published.removed.clone());
    }

}
