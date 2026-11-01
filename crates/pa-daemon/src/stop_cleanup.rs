//! The stop/delete lifecycle cleanup: stopping or deleting a session must cancel its
//! scheduled jobs so no schedule fire, wake pass, or goal continuation can revive it.
//! Deliberate divergence from TS: the Rust port never wakes at all — a schedule fires
//! only while its session is live; due jobs on not-running sessions report dormant.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

#[cfg(test)]
use pa_core::cron::store::AgentCronJobStore;

use crate::lease::canonical_session_path;
use crate::registry::ResidentWorker;
use crate::rlm_ledger::RlmSpawnLedger;
use crate::scheduled_jobs::session_artifact_dir;
use crate::session_store::{read_session_info, SessionFile};
use crate::supervisor::Supervisor;

/// One tree member of a stopped root: its durable session id and file.
struct TreeMember {
    session_id: String,
    /// The canonical (filesystem-resolved) identity: used for live-coverage and
    /// job-ownership matching.
    session_file: PathBuf,
    /// The spelling as discovered (the root's own argument, or the ledger's
    /// recorded child path), before canonicalization. A session's scheduled-
    /// jobs partition may have been registered under either spelling, and
    /// `session_artifact_dir`'s lexical (non-canonicalizing) computation only
    /// finds the one it is given - both are tried at cancellation time.
    raw_session_file: PathBuf,
}

/// A deferred-not-failed outcome: the caller should retry later rather than treat
/// this as a terminal refusal. Carries `std::io::ErrorKind::WouldBlock` so callers
/// (and tests) can distinguish "try again" (live coverage, an unreadable
/// descendant that might just be mid-write) from an ordinary refusal.
fn deferred(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        reason.to_string(),
    ))
}

/// Like `canonical_session_path`, but refuses instead of silently falling back to
/// treating a dangling symlink along the path as an ordinary not-yet-created one.
/// An existing symlink that itself resolves fine (its target exists) is NOT
/// dangling; only a component whose own resolution fails is. The genuinely
/// not-yet-created case (nothing exists at any level) is unchanged and still
/// resolves via `canonical_session_path`'s nearest-existing-ancestor fallback.
fn strict_session_identity(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let mut cursor = path;
    loop {
        if let Ok(metadata) = std::fs::symlink_metadata(cursor) {
            if metadata.file_type().is_symlink() && cursor.canonicalize().is_err() {
                return Err(deferred(format!(
                    "cannot resolve a broken symlink along {}: {}",
                    path.display(),
                    cursor.display()
                )));
            }
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    Ok(canonical_session_path(path))
}

/// The ledger's parent→child map: non-deleted edges keyed by canonical parent path.
///
/// Uses the ledger's authority-grade `strict_edges()`, not the lenient `edges(false)`:
/// this map drives cleanup/cancellation decisions, which must refuse on a well-formed
/// but unrecognized record (an unknown op or version, a duplicate key), or an I/O or
/// cap error - never on a torn, non-JSON, or non-UTF-8 line, which the authority reader
/// skips and logs instead (per `replay::classify_authority_line`'s established rule).
/// Silently reading a truncated topology as complete is exactly what this must avoid.
fn children_by_parent(
    agent_dir: &Path,
    sessions_dir: &Path,
) -> Result<HashMap<String, Vec<(String, PathBuf)>>> {
    let ledger = RlmSpawnLedger::new(agent_dir, sessions_dir, |_| {});
    let mut map: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
    let edges = ledger.strict_edges()?;
    for edge in edges.into_iter().filter(|edge| edge.deleted.is_none()) {
        let child = PathBuf::from(&edge.child);
        let parent = canonical_session_path(Path::new(&edge.parent));
        map.entry(parent.to_string_lossy().to_string())
            .or_default()
            .push((edge.child_id.clone(), child));
    }
    Ok(map)
}

/// The stopped root's session tree: the root file plus every descendant the ledger
/// reaches from it (the ledger is the only topology store).
///
/// # Errors
///
/// Returns a deferred error when a descendant's session metadata cannot be read
/// (it may be mid-write, or genuinely unreadable) rather than silently omitting
/// it: an omitted live descendant would hide live coverage, and an omitted
/// scheduled-jobs partition would let that descendant's jobs survive.
fn session_tree(
    agent_dir: &Path,
    sessions_dir: &Path,
    root_session_file: &Path,
) -> Result<Vec<TreeMember>> {
    let mut members = Vec::new();
    let root = canonical_session_path(root_session_file);
    let root_id = read_session_info(&root).map_or_else(
        || {
            root.file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default()
        },
        |info| info.id,
    );
    if root_id.is_empty() {
        return Ok(members);
    }
    members.push(TreeMember {
        session_id: root_id,
        session_file: root.clone(),
        raw_session_file: root_session_file.to_path_buf(),
    });
    let by_parent = children_by_parent(agent_dir, sessions_dir)?;
    let mut queue = vec![root.to_string_lossy().to_string()];
    let mut visited: HashSet<String> = queue.iter().cloned().collect();
    while let Some(parent) = queue.pop() {
        for (_child_id, child_file) in by_parent.get(&parent).cloned().unwrap_or_default() {
            let child = canonical_session_path(&child_file);
            let child_key = child.to_string_lossy().to_string();
            if visited.contains(&child_key) {
                continue;
            }
            visited.insert(child_key.clone());
            let Some(info) = read_session_info(&child) else {
                return Err(deferred(format!(
                    "descendant session metadata unreadable: {}",
                    child.display()
                )));
            };
            members.push(TreeMember {
                session_id: info.id,
                session_file: child.clone(),
                raw_session_file: child_file,
            });
            queue.push(child_key);
        }
    }
    Ok(members)
}

/// Cancel the stopped session tree's scheduled jobs: a member cancels by session file.
/// A live worker covering any tree member owns its stores again, so the walk aborts on
/// a coverage hit (`live_files`, collected before the walk).
pub(crate) fn cancel_scheduled_jobs_for_tree(
    agent_dir: &Path,
    sessions_dir: &Path,
    root_session_file: &Path,
    live_files: &HashSet<String>,
) -> Result<usize> {
    let members = session_tree(agent_dir, sessions_dir, root_session_file)?;
    if members.iter().any(|member| {
        live_files.contains(
            &canonical_session_path(&member.session_file)
                .to_string_lossy()
                .to_string(),
        )
    }) {
        return Err(deferred(
            "a live worker still owns a member of this session tree",
        ));
    }
    // `session_artifact_dir` is lexical (two `.parent()` calls, no canonicalization),
    // and the store registers at most one directory per session id, so it cannot
    // represent a raw and a canonical partition for one session at the same time.
    // Collect every candidate partition FILE directly instead, then cancel
    // file-by-file: this also sidesteps the store's own `cancel_jobs_for_session`,
    // whose lexical matcher can conflate two different jobs whose paths happen to
    // collapse to the same string. Three spellings per member: its canonical
    // identity, the spelling it was actually discovered with (the root's own
    // argument, or the ledger's recorded child path), and that same canonical
    // identity re-expressed through `sessions_dir`'s own (possibly aliased)
    // spelling - the third one recovers a partition created through the alias
    // even when THIS call's entry argument happened to use the canonical form.
    let canonical_sessions_dir = canonical_session_path(sessions_dir);
    let mut candidate_files: HashSet<PathBuf> = HashSet::new();
    for member in &members {
        let mut spellings: Vec<&Path> = vec![&member.session_file, &member.raw_session_file];
        let aliased = member
            .session_file
            .strip_prefix(&canonical_sessions_dir)
            .ok()
            .map(|relative| sessions_dir.join(relative));
        if let Some(aliased) = &aliased {
            spellings.push(aliased);
        }
        for session_file in spellings {
            let Some(dir) = session_artifact_dir(session_file, &member.session_id) else {
                continue;
            };
            candidate_files.insert(dir.join(pa_core::cron::store::SESSION_SCHEDULED_JOBS_FILENAME));
        }
    }
    let member_paths: HashSet<PathBuf> = members
        .iter()
        .map(|member| canonical_session_path(&member.session_file))
        .collect();
    let mut any_partition = false;
    let mut cancelled = 0usize;
    let now = crate::util::now_ms();
    for partition in candidate_files {
        if !partition.is_file() {
            continue;
        }
        any_partition = true;
        // Refuse rather than silently treat corrupt/malformed state as empty: the
        // store's ordinary read tolerates exactly that, which is right for display
        // but wrong for this authority path.
        let jobs = pa_core::cron::store::read_scheduled_jobs_artifact_strict(&partition)
            .with_context(|| format!("cron schedule partition {}", partition.display()))?;
        let mut matching_ids: HashSet<String> = HashSet::new();
        for job in &jobs {
            if !matches!(
                job.status,
                pa_core::cron::JobStatus::Active | pa_core::cron::JobStatus::Paused
            ) {
                continue;
            }
            // A job whose identity cannot be resolved (a dangling symlink along its
            // path) refuses rather than being silently assumed to belong to someone
            // else's tree. Never widen this to a session-id OR match: a stale binding
            // must not cancel a different transcript merely because a copied job
            // reused its id.
            let resolved = strict_session_identity(Path::new(&job.session_file))?;
            if member_paths.contains(&resolved) {
                matching_ids.insert(job.id.clone());
            }
        }
        cancelled +=
            pa_core::cron::store::cancel_matching_ids_in_file(&partition, &matching_ids, now)?;
    }
    if !any_partition {
        return Ok(0);
    }
    Ok(cancelled)
}

/// The killed session's `archived` state belt: when the worker died before its close
/// wrote the state, the supervisor appends it here.
pub(crate) fn ensure_archived_state(root_session_file: &Path) -> Result<()> {
    let path = canonical_session_path(root_session_file);
    if !path.is_file() {
        return Ok(());
    }
    if read_session_info(&path)
        .and_then(|info| info.state)
        .as_deref()
        == Some("archived")
    {
        return Ok(());
    }
    let mut session = SessionFile::open(&path)?;
    session.append_session_state("archived");
    session.rewrite()
}

/// The finalize of a root-kill stop: the tree's scheduled jobs cancel durably
/// and the root file carries the `archived` state. `live_files` is collected
/// after the stopped worker left the registry.
pub(crate) fn finalize_archived_stop(
    agent_dir: &Path,
    sessions_dir: &Path,
    root_session_file: &Path,
    live_files: &HashSet<String>,
) -> (usize, Option<anyhow::Error>) {
    let cancelled =
        cancel_scheduled_jobs_for_tree(agent_dir, sessions_dir, root_session_file, live_files);
    let (cancelled, cancel_error) = match cancelled {
        Ok(count) => (count, None),
        Err(error) => (0, Some(error)),
    };
    // Any cancellation refusal or deferral (live coverage, an unreadable descendant,
    // corrupt/malformed schedule state, an unresolved job identity) blocks the
    // archive belt too: the tree is not settled, so the root must not be mutated.
    if let Some(error) = cancel_error {
        return (cancelled, Some(error));
    }
    let archive_error = ensure_archived_state(root_session_file).err();
    (cancelled, archive_error)
}

/// The kill route's deleted-child finalize (the `rlmLedgerDelete` marker):
/// the `rlmChildId` whose ledger tombstone was persisted before the kill —
/// it narrows a raced path to the exact edge the tombstone recorded.
pub(crate) struct DeletedChild {
    pub child_id: String,
}

/// The capture core: every tombstoned edge at the child's session path receives the
/// amendment delete record carrying the transcript's own-usage snapshot (read
/// post-settlement — the kill reply is the flush barrier). `None` when not a deleted child.
pub(crate) fn append_deleted_child_usage_amendments(
    ledger: &RlmSpawnLedger,
    session_file: &str,
    child_id: &str,
) -> Option<usize> {
    let path = canonical_session_path(Path::new(session_file));
    let edges = ledger.edges(true).ok()?;
    let mut tombstoned = Vec::new();
    for edge in edges {
        let edge_deleted = edge.deleted.is_some();
        let edge_at_path = canonical_session_path(Path::new(&edge.child)) == path;
        let edge_named = child_id.is_empty() || edge.child_id == child_id;
        if edge_deleted && edge_at_path && edge_named {
            tombstoned.push(edge);
        }
    }
    if tombstoned.is_empty() {
        // Not a deleted child: nothing to capture, no sweep to arm.
        return None;
    }
    let summary = crate::session_usage::read_session_usage(&path)
        .and_then(|totals| crate::session_usage::session_usage_summary_from(&totals.own));
    let mut captured = 0usize;
    for edge in &tombstoned {
        let Some(reason) = edge.deleted else {
            continue;
        };
        let Some(summary) = &summary else {
            // No billable work: a zero snapshot adds nothing - skip the
            // amendment (the bucket reads zero either way).
            continue;
        };
        if ledger
            .append_delete_with_usage(&edge.child_id, &edge.child, reason, summary)
            .is_err()
        {
            continue;
        }
        captured += 1;
    }
    Some(captured)
}

impl Supervisor {
    /// The registry's live coverage set: every resident worker's session file,
    /// canonical, collected after the stopped worker left the registry.
    pub(crate) async fn live_session_files(self: &Arc<Self>) -> HashSet<String> {
        let mut live: HashSet<String> = HashSet::new();
        for resident in self.registry.list().await {
            let owned = resident
                .descriptor
                .lock()
                .await
                .session_file
                .clone()
                .unwrap_or_default();
            if owned.is_empty() {
                continue;
            }
            live.insert(
                canonical_session_path(Path::new(&owned))
                    .to_string_lossy()
                    .to_string(),
            );
        }
        live
    }

    /// TS `persistWorkerStopTombstone(worker, true)`: the root-kill stop's durable intent,
    /// persisted BEFORE the worker is told — a mid-stop death adopts the tombstone
    /// instead of relaunching the killed worker.
    pub(crate) async fn persist_stop_tombstone(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
    ) -> anyhow::Result<()> {
        let mut descriptor = resident.descriptor.lock().await;
        if descriptor.stop_requested_at.is_none() {
            descriptor.stop_requested_at = Some(crate::util::now_iso());
        }
        descriptor.archive_on_stop = Some(true);
        crate::descriptor::persist_worker(&resident.descriptor_path, &descriptor)?;
        drop(descriptor);
        resident
            .intentional_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// TS `persistWorkerStopTombstone(worker, false)`: the per-session stop's durable
    /// intent (the stopped session stays resumable); `archive_on_stop = false` routes boot
    /// adoption to the graceful shutdown + schedule-cancel half, never the kill's cascade.
    /// EXCEPT when the durable intent already exists: a re-write failure must not abort
    /// the stop (the worker would run on its lease until the next boot).
    pub(crate) async fn persist_stop_tombstone_stop(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
    ) -> anyhow::Result<()> {
        let mut descriptor = resident.descriptor.lock().await;
        let old_stop_requested_at = descriptor.stop_requested_at.clone();
        let old_archive_on_stop = descriptor.archive_on_stop;
        if descriptor.stop_requested_at.is_none() {
            descriptor.stop_requested_at = Some(crate::util::now_iso());
        }
        if descriptor.archive_on_stop.is_none() {
            descriptor.archive_on_stop = Some(false);
        }
        if let Err(error) =
            crate::descriptor::persist_worker(&resident.descriptor_path, &descriptor)
        {
            if old_stop_requested_at.is_none() {
                // No durable intent anywhere: roll the in-memory mutation back (a later
                // descriptor write must not carry a tombstone the rejected stop never
                // durably set) and fail the stop.
                descriptor.stop_requested_at = old_stop_requested_at;
                descriptor.archive_on_stop = old_archive_on_stop;
                return Err(error);
            }
        }
        drop(descriptor);
        resident
            .intentional_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    pub(crate) async fn finalize_worker_stop(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        deleted_child: Option<&DeletedChild>,
    ) -> bool {
        let root_session_file = resident
            .descriptor
            .lock()
            .await
            .session_file
            .clone()
            .unwrap_or_default();
        if root_session_file.is_empty() {
            return true;
        }
        let sessions_dir = match crate::paths::sessions_dir(&self.options.agent_dir) {
            Ok(dir) => dir,
            Err(error) => {
                self.log_line(&format!(
                    "stop finalize: could not resolve the sessions dir: {error:#}"
                ));
                return false;
            }
        };
        let live = self.live_session_files().await;
        let (cancelled, archive_error) = finalize_archived_stop(
            &self.options.agent_dir,
            &sessions_dir,
            Path::new(&root_session_file),
            &live,
        );
        if cancelled > 0 {
            self.log_line(&format!(
                "stop finalize: cancelled {cancelled} scheduled job(s) of {root_session_file}"
            ));
        }
        let settled = match archive_error {
            None => true,
            Some(error) => {
                self.log_line(&format!(
                    "stop finalize: could not archive {root_session_file}: {error:#}"
                ));
                false
            }
        };
        match deleted_child {
            Some(deleted) => {
                // The durable usage capture: the kill reply was the flush barrier, so the
                // frozen transcript holds the child's final own spend. Capture it BEFORE
                // the sweep; a failed capture never fails the stop.
                self.capture_deleted_child_usage(
                    &root_session_file,
                    &deleted.child_id,
                    "rlm_delete",
                )
                .await;
                // An unsettled finalize (live coverage, an unreadable descendant, corrupt
                // or malformed schedule state) must retain every artifact for the retry -
                // the capture already ran (it is read-only and best-effort), but the sweep
                // itself must not run until cancellation/archival actually completed.
                if settled {
                    crate::saved_session_commands::remove_session_artifacts(Path::new(
                        &root_session_file,
                    ));
                }
            }
            None => {
                // The adoption finalize (an interrupted stop re-runs here): a crash
                // between the ledger tombstone and this sweep leaves the capture undone;
                // the durable tombstone reconstructs the delete — the capture and sweep finish.
                if let Some(child_id) = self.tombstoned_child_at(&root_session_file).await {
                    self.capture_deleted_child_usage(&root_session_file, &child_id, "adoption")
                        .await;
                    if settled {
                        crate::saved_session_commands::remove_session_artifacts(Path::new(
                            &root_session_file,
                        ));
                    }
                }
            }
        }
        settled
    }

    /// The tombstoned edges' child id at one session path (the adoption
    /// finalize's ledger consult): `None` when nothing is tombstoned
    /// there - a plain kill, an untouched subagent, a top-level session.
    /// #42/#37 follow-up: the crash-recovery sweep's own discovery read stays
    /// best-effort (a plain lookup, tolerant of a torn/malformed line
    /// elsewhere), but a FOUND candidate is never trusted on that read
    /// alone before this destructive sweep runs (it captures usage and
    /// removes session artifacts). `check_committed_deletion` re-verifies
    /// the SAME candidate through the strict authority read
    /// (`strict_edges`/`checked_session_path`) - an unrecognized record
    /// anywhere in this scope (a newer binary's future op, say) makes the
    /// WHOLE scope unverifiable for authority purposes, not just that one
    /// line, so the best-effort read's tolerant "skip it and carry on"
    /// must not be the final word before acting destructively on what it
    /// found. A refused verification answers `None` here, exactly like
    /// "nothing tombstoned" - the normal retry/backoff path already
    /// covers that case; this never panics or escalates beyond declining.
    async fn tombstoned_child_at(self: &Arc<Self>, session_file: &str) -> Option<String> {
        let ledger = self.rlm_spawn_ledger_for(None).await.ok()?;
        let path = crate::lease::canonical_session_path(Path::new(session_file));
        let edges = ledger.edges(true).ok()?;
        let candidate = edges
            .iter()
            .find(|edge| {
                edge.deleted.is_some()
                    && crate::lease::canonical_session_path(Path::new(&edge.child)) == path
            })
            .map(|edge| edge.child_id.clone())?;
        let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
            &self.options.agent_dir,
            &self.options.socket_path,
        )
        .ok()?;
        authority
            .check_committed_deletion(Path::new(session_file), &candidate)
            .ok()?;
        Some(candidate)
    }

    /// The deleted child's durable usage capture: read the frozen transcript's own usage
    /// and append the amendment delete record carrying the snapshot (the spend survives
    /// the transcript's removal). Best effort — a failure logs and leaves the lazy fallback.
    pub(crate) async fn capture_deleted_child_usage(
        self: &Arc<Self>,
        session_file: &str,
        child_id: &str,
        source: &'static str,
    ) {
        let Ok(ledger) = self.rlm_spawn_ledger_for(None).await else {
            self.log_line("deleted-child capture: could not resolve the spawn ledger");
            return;
        };
        // The capture reads and JSON-parses the whole frozen transcript — blocking work
        // that must not stall an async runtime worker, so it runs on the blocking executor.
        let session_file = session_file.to_string();
        let child_id = child_id.to_string();
        let captured = tokio::task::spawn_blocking(move || {
            append_deleted_child_usage_amendments(&ledger, &session_file, &child_id)
        })
        .await
        .unwrap_or(None);
        self.note_deleted_child_usage_captured(source, captured.unwrap_or(0));
    }

    /// The ephemeral stop's schedule cancel: a client-owned worker's schedules die with
    /// the registration. The stopping worker's own file is excluded from the live
    /// coverage set (it is still in the registry at this point of the stop).
    pub(crate) async fn finalize_owned_stop(self: &Arc<Self>, resident: &Arc<ResidentWorker>) {
        let root_session_file = resident
            .descriptor
            .lock()
            .await
            .session_file
            .clone()
            .unwrap_or_default();
        if root_session_file.is_empty() {
            return;
        }
        let Ok(sessions_dir) = crate::paths::sessions_dir(&self.options.agent_dir) else {
            return;
        };
        let mut live = self.live_session_files().await;
        live.remove(
            &canonical_session_path(Path::new(&root_session_file))
                .to_string_lossy()
                .to_string(),
        );
        let cancelled = cancel_scheduled_jobs_for_tree(
            &self.options.agent_dir,
            &sessions_dir,
            Path::new(&root_session_file),
            &live,
        );
        let cancelled = match cancelled {
            Ok(count) => count,
            Err(error) => {
                self.log_line(&format!("owned stop: cron cancel failed: {error}"));
                return;
            }
        };
        if cancelled > 0 {
            self.log_line(&format!(
                "owned stop: cancelled {cancelled} scheduled job(s) of {root_session_file}"
            ));
        }
    }
}

#[cfg(test)]
mod path_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{session_file_name, SessionFile};
    use pa_core::cron::AgentCronJob;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-stop-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(super) fn write_session(dir: &Path, name: Option<&str>) -> (String, PathBuf) {
        let mut session = SessionFile::create("/work", None, 0);
        if let Some(name) = name {
            session.append_session_info(name);
        }
        session.append_message(&serde_json::json!({
            "role": "user", "content": "hi", "timestamp": 1u64
        }));
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        // The live state a fresh session carries (the worker's create
        // appends it): the wake-scan gate requires it.
        let _ = session.append_session_state("active");
        session.rewrite().unwrap();
        (session.session_id().to_string(), path)
    }

    pub(super) fn write_job(session_file: &Path, session_id: &str, job_id: &str) {
        let dir = session_artifact_dir(session_file, session_id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let jobs = serde_json::json!([{
            "id": job_id,
            "status": "active",
            "source": "rlm_heartbeat",
            "runtimeKind": "subagent",
            "activeSessionId": session_id,
            "sessionId": session_id,
            "sessionFile": session_file.to_string_lossy(),
            "cwd": "/work",
            "prompt": "liveness ping",
            "label": "lane-liveness",
            "schedule": { "kind": "interval", "expression": "every 2m", "intervalMs": 120_000 },
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "runCount": 0,
            "nextRunAt": "2026-01-01T00:02:00Z",
        }]);
        std::fs::write(
            dir.join("scheduled-jobs.json"),
            serde_json::json!({ "jobs": jobs, "dispatches": [] }).to_string(),
        )
        .unwrap();
    }

    pub(super) fn read_job(session_file: &Path, session_id: &str) -> AgentCronJob {
        let dir = session_artifact_dir(session_file, session_id).unwrap();
        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("scheduled-jobs.json")).unwrap(),
        )
        .unwrap();
        serde_json::from_value(raw["jobs"][0].clone()).unwrap()
    }

    #[tokio::test]
    async fn tombstoned_child_at_trusts_a_best_effort_read_an_unknown_op_line_should_refuse() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let supervisor = Arc::new(
            Supervisor::new(crate::supervisor::SupervisorOptions {
                socket_path: root.join("daemon.sock"),
                agent_dir: agent_dir.clone(),
            })
            .expect("supervisor"),
        );
        let (_, parent_file) = write_session(&sessions_dir, Some("orchestrator"));
        let (_, child_file) = write_session(&sessions_dir, Some("lane"));
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-1".to_string(),
                parent: parent_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();
        ledger
            .append_delete(
                "sub-1",
                &child_file.to_string_lossy(),
                crate::rlm_ledger::RlmLedgerDeleteReason::User,
            )
            .unwrap();
        // A future binary's record this reader does not recognize - present and
        // well-formed otherwise, so the authority/strict read must refuse the
        // whole scope rather than silently drop it.
        let mut bytes = std::fs::read(ledger.ledger_path()).unwrap();
        bytes.extend_from_slice(
            b"{\"v\":1,\"op\":\"future-operation\",\"at\":\"2026-01-01T00:00:00Z\"}\n",
        );
        std::fs::write(ledger.ledger_path(), &bytes).unwrap();

        // RED: the best-effort read `tombstoned_child_at` uses today silently
        // tolerates the unknown-op line and still reports the tombstone - the
        // crash-recovery sweep would act on a deletion the authority/strict
        // read (`check_committed_deletion`) would refuse to confirm, because
        // an unrecognized record anywhere in scope makes the WHOLE scope
        // unverifiable, not just that one line.
        let found = supervisor
            .tombstoned_child_at(&child_file.to_string_lossy())
            .await;
        assert_eq!(
            found, None,
            "an unknown-op line anywhere in scope must refuse the tombstone finding, \
             not silently tolerate it and report the deletion as committed anyway"
        );
    }

    #[test]
    fn finalize_cancels_the_tree_and_archives_the_root() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let (root_id, root_file) = write_session(&sessions_dir, Some("orchestrator"));
        let (child_id, child_file) = write_session(&sessions_dir, Some("lane"));
        // The ledger edge parent -> child.
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "child-1".to_string(),
                parent: root_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();

        write_job(&root_file, &root_id, "job-root");
        write_job(&child_file, &child_id, "job-child");

        let live: HashSet<String> = HashSet::new();
        let (cancelled, archive_error) =
            finalize_archived_stop(&agent_dir, &sessions_dir, &root_file, &live);
        assert!(archive_error.is_none());
        assert_eq!(cancelled, 2, "the root and child jobs both cancel");
        assert_eq!(
            read_job(&root_file, &root_id).status,
            pa_core::cron::JobStatus::Cancelled
        );
        assert_eq!(
            read_job(&child_file, &child_id).status,
            pa_core::cron::JobStatus::Cancelled
        );

        let info = read_session_info(&root_file).unwrap();
        assert_eq!(info.state.as_deref(), Some("archived"));
    }

    #[test]
    fn finalize_skips_a_tree_a_live_worker_covers() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let (root_id, root_file) = write_session(&sessions_dir, None);
        write_job(&root_file, &root_id, "job-root");

        let live: HashSet<String> = [canonical_session_path(&root_file)
            .to_string_lossy()
            .to_string()]
        .into_iter()
        .collect();
        let (cancelled, _) = finalize_archived_stop(&agent_dir, &sessions_dir, &root_file, &live);
        assert_eq!(cancelled, 0, "a covered tree keeps its schedules");
        assert_eq!(
            read_job(&root_file, &root_id).status,
            pa_core::cron::JobStatus::Active
        );
    }

    #[test]
    fn capture_reads_the_frozen_transcript_before_any_sweep() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let (_, parent_file) = write_session(&sessions_dir, Some("orchestrator"));
        let (_, child_file) = write_session(&sessions_dir, Some("lane"));
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-1".to_string(),
                parent: parent_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();
        // The tombstone lands before the kill (the revival window); the
        // capture only runs after the kill reply settles the file.
        ledger
            .append_delete(
                "sub-1",
                &child_file.to_string_lossy(),
                crate::rlm_ledger::RlmLedgerDeleteReason::User,
            )
            .unwrap();
        // A killed-mid-turn child: an aborted assistant row with partial
        // usage is already durable (the kill reply was the barrier).
        let usage_row = serde_json::json!({
            "type": "message", "id": "m-aborted",
            "message": {
                "role": "assistant",
                "stopReason": "aborted",
                "usage": {
                    "input": 4_000, "output": 400, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 4_400,
                    "cost": { "input": 0.05, "output": 0.01, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.06 }
                }
            }
        });
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&child_file)
                .unwrap();
            writeln!(file, "{usage_row}").unwrap();
        }
        let captured =
            append_deleted_child_usage_amendments(&ledger, &child_file.to_string_lossy(), "sub-1");
        assert_eq!(captured, Some(1), "the one tombstoned edge is amended");
        let edges = ledger.edges(true).unwrap();
        assert_eq!(edges.len(), 1);
        let snapshot = edges[0]
            .deleted_usage
            .clone()
            .expect("the snapshot rides the edge");
        assert!((snapshot.cost - 0.06).abs() < 1e-9);
        // A transcript directly in the sessions dir keeps its catalog
        // row, so the bucket skips it (the rollup sums the row AND the
        // parent bucket; billing both would double the spend).
        let live_bucket = ledger.deleted_descendant_usage_by_parent().unwrap();
        let parent_key = canonical_session_path(&parent_file)
            .to_string_lossy()
            .to_string();
        assert!(
            !live_bucket.contains_key(&parent_key),
            "a live transcript's spend rides its own row, not the bucket"
        );
        // The sweep never ran: the transcript is the lazy fallback's copy.
        assert!(child_file.is_file());
        // The transcript dies (the sweep or a later delete): the bucket's
        // snapshot is now the only carrier — the spend survives.
        std::fs::remove_file(&child_file).unwrap();
        ledger
            .append_delete(
                "sub-1",
                &child_file.to_string_lossy(),
                crate::rlm_ledger::RlmLedgerDeleteReason::User,
            )
            .unwrap();
        let bucket = ledger.deleted_descendant_usage_by_parent().unwrap();
        assert!((bucket[&parent_key].cost - 0.06).abs() < 1e-9);
    }

    /// The snapshot is OWN usage: the bucket's post-order fold re-adds the grandchild's
    /// own snapshot, and an aggregate would double count it.
    #[test]
    fn capture_is_own_only_never_the_attribution_aggregate() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let (_, parent_file) = write_session(&sessions_dir, None);
        let (_, child_file) = write_session(&sessions_dir, None);
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-1".to_string(),
                parent: parent_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();
        ledger
            .append_delete(
                "sub-1",
                &child_file.to_string_lossy(),
                crate::rlm_ledger::RlmLedgerDeleteReason::User,
            )
            .unwrap();
        let usage_block = |total: f64| {
            serde_json::json!({
                "input": 1_000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 1_100,
                "cost": { "input": total, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": total }
            })
        };
        // The child's own row (0.40) + the grandchild's attribution folded
        // onto it (childUsage 0.10, aggregate 0.50).
        let rows = [
            serde_json::json!({
                "type": "message", "id": "m1",
                "message": { "role": "assistant", "usage": usage_block(0.40) }
            }),
            serde_json::json!({
                "type": "child_usage_attributed", "targetId": "m1",
                "childUsage": usage_block(0.10), "aggregateUsage": usage_block(0.50)
            }),
        ];
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&child_file)
                .unwrap();
            for row in rows {
                writeln!(file, "{row}").unwrap();
            }
        }
        let totals = crate::session_usage::read_session_usage(&child_file).unwrap();
        assert!(
            (totals.total.cost.total.as_f64() - 0.50).abs() < 1e-9,
            "the file's fold"
        );
        assert!(
            (totals.own.cost.total.as_f64() - 0.40).abs() < 1e-9,
            "the own read"
        );
        append_deleted_child_usage_amendments(&ledger, &child_file.to_string_lossy(), "sub-1")
            .unwrap();
        let snapshot = ledger.edges(true).unwrap()[0]
            .deleted_usage
            .clone()
            .expect("captured");
        assert!(
            (snapshot.cost - 0.40).abs() < 1e-9,
            "own-only snapshot (0.40), never the aggregate (0.50): got {}",
            snapshot.cost
        );
    }

    #[test]
    fn capture_skips_live_children_and_plain_kills() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let (_, parent_file) = write_session(&sessions_dir, None);
        let (_, child_file) = write_session(&sessions_dir, None);
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-1".to_string(),
                parent: parent_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();
        // Still live: no capture (kill-failure retryability — the child
        // keeps its transcript, its row, and its retryability).
        assert_eq!(
            append_deleted_child_usage_amendments(&ledger, &child_file.to_string_lossy(), "sub-1"),
            None
        );
        // A path the ledger never knew: nothing to capture.
        assert_eq!(
            append_deleted_child_usage_amendments(&ledger, "/nowhere/x.jsonl", ""),
            None
        );
    }

    #[test]
    fn capture_with_no_child_id_amends_every_tombstoned_edge() {
        let root = temp_dir();
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let (_, parent_file) = write_session(&sessions_dir, None);
        let (_, child_file) = write_session(&sessions_dir, None);
        let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        for child_id in ["sub-1", "sub-2"] {
            ledger
                .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                    child_id: child_id.to_string(),
                    parent: parent_file.to_string_lossy().to_string(),
                    child: child_file.to_string_lossy().to_string(),
                    depth: 1,
                    name: "lane".to_string(),
                })
                .unwrap();
            ledger
                .append_delete(
                    child_id,
                    &child_file.to_string_lossy(),
                    crate::rlm_ledger::RlmLedgerDeleteReason::User,
                )
                .unwrap();
        }
        let captured =
            append_deleted_child_usage_amendments(&ledger, &child_file.to_string_lossy(), "");
        assert_eq!(captured, Some(0), "no billable usage: no amendment");
        for edge in ledger.edges(true).unwrap() {
            assert_eq!(edge.deleted_usage, None);
        }
    }
}
