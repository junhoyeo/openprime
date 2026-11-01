//! The root-identity follow (the fork-isolation seam, operator bug #5): a
//! whole-session replacement moves the worker onto a NEW durable session under
//! its UNCHANGED active session id; the descriptor, persisted record, and
//! binding table follow the roster under the descriptor lock (older never wins).

use std::sync::Arc;

use serde_json::Value;

use crate::registry::ResidentWorker;

use super::Supervisor;

/// The retry cadence: doubling to the 5s cap (a slow-but-alive worker's boot pull
/// times out; the retry converges on its answer).
const RECONCILIATION_BACKOFF_MS: u64 = 250;
const RECONCILIATION_BACKOFF_MAX_MS: u64 = 5_000;

impl Supervisor {
    /// The boot's durable-pending repair: apply the side record's moved-to identity to
    /// the resident BEFORE any routing or relaunch uses the stale record.
    pub(crate) async fn apply_identity_pending(&self, resident: &Arc<ResidentWorker>) {
        let Some((session_id, session_file, moved_at)) =
            crate::descriptor::read_identity_pending(&resident.descriptor_path)
        else {
            return;
        };
        let mut descriptor = resident.descriptor.lock().await;
        // The freshness gate: the side record only wins while the record is STRICTLY OLDER
        // than the move — a later swap whose persist succeeded must never roll back onto
        // this older pending (obsolete at equality).
        if descriptor.updated_at.as_str() >= moved_at.as_str() {
            drop(descriptor);
            if let Err(error) = crate::descriptor::clear_identity_pending(&resident.descriptor_path)
            {
                self.log_line(&format!(
                    "the obsolete identity-pending record for worker {} did not clear: {error:#}",
                    resident.worker_id
                ));
            }
            return;
        }
        if descriptor.root_session_id.as_deref() != Some(session_id.as_str())
            || descriptor.session_file.as_deref() != Some(session_file.as_str())
        {
            descriptor.root_session_id = Some(session_id.clone());
            descriptor.session_file = Some(session_file.clone());
            descriptor.create_command.session_path = Some(session_file.clone());
            descriptor.create_command.no_session = None;
            // The moved-to session's cwd (the live follow's cwd rule): the relaunch
            // must not replay the superseded session's directory.
            if let Some(cwd) =
                crate::descriptor::read_identity_pending_cwd(&resident.descriptor_path)
            {
                descriptor
                    .create_command
                    .rest
                    .insert("cwd".to_string(), Value::from(cwd));
            }
        }
        match crate::descriptor::persist_worker(&resident.descriptor_path, &descriptor) {
            Ok(()) => {
                resident.clear_identity_persist_pending();
                if let Err(error) =
                    crate::descriptor::clear_identity_pending(&resident.descriptor_path)
                {
                    self.log_line(&format!(
                        "the identity-pending record for worker {} did not clear after the boot repair: {error:#}",
                        resident.worker_id
                    ));
                }
            }
            Err(error) => {
                resident.mark_identity_persist_pending();
                self.log_line(&format!(
                    "the boot applied worker {}'s pending identity but the record persist failed (repairing on the first roster write): {error:#}",
                    resident.worker_id
                ));
            }
        }
    }

    /// The quarantine's retry path: a slow-but-alive worker's boot reconciliation pull can
    /// time out — re-run it on a backoff until the live word lands (an accepted roster
    /// write clears the quarantine) or the worker leaves the registry.
    pub(crate) fn spawn_identity_reconciliation_retry(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
    ) {
        let supervisor = Arc::clone(self);
        let resident = Arc::clone(resident);
        tokio::spawn(async move {
            let mut backoff_ms = RECONCILIATION_BACKOFF_MS;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RECONCILIATION_BACKOFF_MAX_MS);
                if supervisor
                    .shutting_down
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    // The shutdown exit: a stopping daemon never leaves the retry spinning.
                    return;
                }
                if !resident.identity_quarantined() {
                    // The live word already landed elsewhere (its own roster push or a pull).
                    return;
                }
                if supervisor.registry.get(&resident.worker_id).await.is_none() {
                    // The death verdict: the persisted identity serves the
                    // lazy re-open, never this quarantined resident.
                    return;
                }
                if supervisor.refresh_roster_entry(&resident).await {
                    // The pull landed: the roster write carried the
                    // identity follow and cleared the quarantine.
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pa_types::daemon::DaemonWorkerDescriptor;
    use serde_json::json;

    use crate::registry::ResidentWorker;
    use crate::supervisor::Supervisor;
    use crate::supervisor_roster::WorkerRosterDelta;

    /// A supervisor with one registered resident whose address matches its
    /// roster row, carrying a persisted root identity the deltas can move.
    /// #42 Kind-1 fix: `descriptor.pid`/`process_start_id`/`worker_instance_id`
    /// are set to match what a REAL `authority.acquire()` call (made later,
    /// per-delta, by `ensure_session_leased` below, against the delta's OWN
    /// `sessionFile` - never this descriptor's initial one) will actually
    /// publish as the owner/generation. A bare `pid: 0` (the original setup)
    /// can never match a real acquiring process's identity, so every later
    /// root-targeted publication correctly refused. Every test's original
    /// assertions/intent are unchanged; only the identity fields this new
    /// model's checks compare against are set to match THIS TEST PROCESS.
    async fn supervisor_with_movable_worker(dir: &std::path::Path) -> Arc<Supervisor> {
        let supervisor = Arc::new(
            Supervisor::new(crate::supervisor::SupervisorOptions {
                socket_path: dir.join("daemon.sock"),
                agent_dir: dir.join("agent"),
            })
            .expect("supervisor"),
        );
        let pid = std::process::id();
        let process_start_id = crate::lease::get_process_start_id(pid);
        let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
            "version": 2,
            "workerId": "w-a",
            "pid": pid,
            "processStartId": process_start_id,
            "socketPath": "/tmp/none.sock",
            "recoveryJournalPath": "/tmp/none.jsonl",
            "supervisorSocketPath": "/tmp/none.sock",
            "authenticationToken": "token-a",
            "workerInstanceId": "i1",
            "rootActiveSessionId": "w-a",
            "rootSessionId": "s0",
            "sessionFile": dir.join("s0.jsonl").to_string_lossy(),
            "createdAt": "2026-09-30T00:00:00Z",
            "updatedAt": "2026-09-30T00:00:00Z",
            "lifecycle": "ready",
            "createCommand": { "sessionPath": dir.join("s0.jsonl").to_string_lossy() },
            "consecutiveFailures": 0,
        }))
        .expect("descriptor");
        let resident = ResidentWorker::new("w-a".to_string(), descriptor, dir.join("w-a.json"));
        resident.note_session_ready();
        supervisor.registry.insert(resident).await;
        supervisor
    }

    /// #42 Kind-1 fix: `publish_verified_summary`'s M-fence ownership check
    /// targets the DELTA's own `summary.sessionFile` (confirmed via
    /// `with_runtime_session_fence(authority.agent_dir(), Path::new(file), ...)`
    /// in `session_publication.rs`), never a resident's session file at setup
    /// time - so every root-targeted delta this test suite drives needs a
    /// REAL, fence-acquired lease on that exact file before the delta is
    /// sent, or the `is_root` ownership check sees `Absent` and refuses.
    ///
    /// Tolerates being called more than once for the SAME file within one
    /// test (several tests swap back onto a session they already visited,
    /// e.g. `successive_swaps_land_the_identity_on_the_final_row`'s second
    /// `"a.jsonl"` delta): a fresh `acquire()` on a file this same process
    /// already holds fails with `SessionAlreadyActiveError` (the lease is
    /// intentionally never released - see below); that specific, provably-
    /// self-held case is treated as already-satisfied, not an error.
    ///
    /// The owner record's `active_session_id` is read from the process-wide
    /// `SESSION_LEASE_OWNER_ID_ENV` var by `acquire()` internally; set here
    /// for the narrow span of the call only, under the crate's shared
    /// `test_support::TELEMETRY_ENV_MUTEX` (already used by 2 other test
    /// modules for exactly this "serialize process-env mutation across
    /// concurrent test threads" purpose, despite its telemetry-flavored
    /// name - its own doc comment: "one lock across the crate's test
    /// modules"). The owner record is fully durable the moment `acquire()`
    /// returns, so the env var and the lock are released immediately after.
    // Kept async for caller consistency (every call site already
    // `.await`s it); its body happens to be synchronous today, not a
    // reason to churn callers for a style-only clippy preference.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn ensure_session_leased(supervisor: &Arc<Supervisor>, summary: &serde_json::Value) {
        let Some(file) = summary
            .get("sessionFile")
            .and_then(serde_json::Value::as_str)
            .filter(|f| !f.is_empty())
        else {
            return;
        };
        let active = summary
            .get("activeSessionId")
            .or_else(|| summary.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let session_path = std::path::PathBuf::from(file);
        if !session_path.exists() {
            let mut sf = crate::session_store::SessionFile::create(
                &session_path.parent().unwrap().to_string_lossy(),
                None,
                0,
            );
            sf.set_path(session_path.clone());
            sf.rewrite().unwrap();
        }
        let authority = crate::lease::RuntimeSessionAuthority::for_supervisor(
            &supervisor.options.agent_dir,
            &supervisor.options.socket_path,
        )
        .expect("supervisor authority resolves");
        let outcome = {
            let _env_lock = crate::test_support::TELEMETRY_ENV_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::env::var_os(crate::lease::SESSION_LEASE_OWNER_ID_ENV);
            std::env::set_var(crate::lease::SESSION_LEASE_OWNER_ID_ENV, active);
            let acquired = authority.acquire(&session_path);
            match previous {
                Some(value) => std::env::set_var(crate::lease::SESSION_LEASE_OWNER_ID_ENV, value),
                None => std::env::remove_var(crate::lease::SESSION_LEASE_OWNER_ID_ENV),
            }
            acquired
        };
        match outcome {
            Ok(lease) => std::mem::forget(lease),
            Err(error) => {
                let already_mine = error
                    .downcast_ref::<crate::lease::SessionAlreadyActiveError>()
                    .is_some_and(|held| held.holder_pid == Some(std::process::id()));
                assert!(already_mine, "the session file leases for real: {error:#}");
            }
        }
    }

    fn swap_summary(dir: &std::path::Path, session: &str, file: &str) -> serde_json::Value {
        json!({
            "id": "w-a",
            "lifecycle": "live",
            "activity": "idle",
            "isSessionActive": false,
            "activeSessionId": "w-a",
            "sessionId": session,
            "sessionFile": dir.join(file).to_string_lossy(),
            "sessionName": "movable",
            "cwd": dir.to_string_lossy(),
            "rlmDepth": 0,
            "runtimeKind": "top-level",
            "messageCount": 1,
            "attachedClients": 0,
            "thinkingLevel": "default",
            "workerState": "ready",
        })
    }

    /// Drive one accepted roster delta for the worker (a sequenced
    /// generation, the real handler path).
    async fn drive_delta(supervisor: &Arc<Supervisor>, summary: serde_json::Value, sequence: u64) {
        ensure_session_leased(supervisor, &summary).await;
        let response = supervisor
            .handle_worker_roster_delta(
                "d",
                "worker_roster_delta",
                WorkerRosterDelta {
                    worker_token: "token-a".to_string(),
                    summary,
                    removed: Vec::new(),
                    sequence: Some(sequence),
                    worker_instance_id: Some("i1".to_string()),
                },
            )
            .await;
        assert!(response.success, "the delta applied: {response:?}");
    }

    /// The full identity state: the live descriptor's root session id and session file,
    /// the persisted record's root session id, plus the binding table's session id.
    async fn identity_state(
        supervisor: &Arc<Supervisor>,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let resident = supervisor.registry.get("w-a").await.expect("resident");
        let (root_session_id, session_file) = {
            let descriptor = resident.descriptor.lock().await;
            (
                descriptor.root_session_id.clone(),
                descriptor.session_file.clone(),
            )
        };
        let persisted: Option<String> = std::fs::read_to_string(&resident.descriptor_path)
            .ok()
            .and_then(|content| serde_json::from_str::<DaemonWorkerDescriptor>(&content).ok())
            .and_then(|record| record.root_session_id);
        let binding = supervisor
            .session_bindings
            .binding_for("w-a")
            .and_then(|binding| binding.session_id.clone());
        (root_session_id, session_file, persisted, binding)
    }

    /// The expected (descriptor id, file, record id, binding id) for one
    /// moved-to session.
    fn expect_state(
        dir: &std::path::Path,
        session: &str,
        file: &str,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let file = dir.join(file).to_string_lossy().to_string();
        (
            Some(session.to_string()),
            Some(file),
            Some(session.to_string()),
            Some(session.to_string()),
        )
    }

    #[tokio::test]
    async fn successive_swaps_land_the_identity_on_the_final_row() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;

        // A → B: each accepted row moves the whole triple.
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 1).await;
        assert_eq!(
            identity_state(&supervisor).await,
            expect_state(&dir, "sA", "a.jsonl"),
            "the first swap moved the descriptor, the record, and the binding"
        );
        drive_delta(&supervisor, swap_summary(&dir, "sB", "b.jsonl"), 2).await;
        assert_eq!(
            identity_state(&supervisor).await,
            expect_state(&dir, "sB", "b.jsonl"),
            "the second swap moved the whole triple"
        );

        // The A → B → A chain: the final row wins.
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 3).await;
        assert_eq!(
            identity_state(&supervisor).await,
            expect_state(&dir, "sA", "a.jsonl"),
            "the return swap landed on the final row"
        );

        // A CONCURRENT burst of three swaps (F1, F2, F1 again): the triple must name the
        // roster's final accepted row.
        let burst = tokio::join!(
            drive_delta(&supervisor, swap_summary(&dir, "sF1", "f1.jsonl"), 4),
            drive_delta(&supervisor, swap_summary(&dir, "sF2", "f2.jsonl"), 5),
            drive_delta(&supervisor, swap_summary(&dir, "sF1", "f1.jsonl"), 6),
        );
        let _ = burst;
        let (roster_session, roster_file) = {
            let roster = supervisor.roster.lock().unwrap();
            let entry = roster
                .by_active_session_id("w-a")
                .expect("the worker's root row");
            (
                entry.summary["sessionId"].as_str().unwrap().to_string(),
                entry.summary["sessionFile"].as_str().unwrap().to_string(),
            )
        };
        let (root_session_id, session_file, persisted, binding) = identity_state(&supervisor).await;
        assert_eq!(
            root_session_id.as_deref(),
            Some(roster_session.as_str()),
            "the descriptor names the roster's final row (the values print above)"
        );
        assert_eq!(session_file.as_deref(), Some(roster_file.as_str()));
        assert_eq!(persisted.as_deref(), Some(roster_session.as_str()));
        assert_eq!(binding.as_deref(), Some(roster_session.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_identity_persist_is_repaired_by_the_next_roster_write() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");

        // The record path cannot take a file: the atomic write's rename
        // onto a directory fails deterministically.
        std::fs::remove_file(&resident.descriptor_path).ok();
        std::fs::create_dir_all(&resident.descriptor_path).unwrap();

        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 1).await;
        assert_eq!(
            resident.descriptor.lock().await.root_session_id.as_deref(),
            Some("sA"),
            "the live descriptor moved even though the record write failed"
        );
        assert!(
            resident.identity_persist_pending(),
            "the unresolved persist is marked for repair"
        );
        assert!(
            supervisor
                .session_bindings
                .binding_for("w-a")
                .is_some_and(|binding| binding.session_id.as_deref() == Some("sA")),
            "the binding follows the live identity (the routing is correct while the record lags)"
        );
        let pending = crate::descriptor::read_identity_pending(&resident.descriptor_path)
            .expect("the durable pending landed");
        assert_eq!(pending.0, "sA");
        assert_eq!(
            pending.1,
            dir.join("a.jsonl").to_string_lossy().to_string(),
            "the double-failed persist durably recorded the moved-to identity beside the record"
        );

        // The repair: the next write re-runs the persist from the live state — even a
        // NO-CHANGE row (the pending marker forces the retry).
        std::fs::remove_dir_all(&resident.descriptor_path).unwrap();
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 2).await;
        assert!(
            !resident.identity_persist_pending(),
            "the repair cleared the marker"
        );
        let record: DaemonWorkerDescriptor = serde_json::from_str(
            &std::fs::read_to_string(&resident.descriptor_path).expect("the repaired record"),
        )
        .expect("the record parses");
        assert_eq!(record.root_session_id.as_deref(), Some("sA"));
        assert_eq!(
            record.session_file.as_deref(),
            Some(dir.join("a.jsonl").to_string_lossy().to_string().as_str())
        );
        assert!(
            crate::descriptor::read_identity_pending(&resident.descriptor_path).is_none(),
            "the repair removed the pending side record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_durable_pending_moves_the_boot_identity_before_any_routing() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");

        // The double-failed persist leaves the durable pending beside the
        // stale record.
        std::fs::remove_file(&resident.descriptor_path).ok();
        std::fs::create_dir_all(&resident.descriptor_path).unwrap();
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 1).await;
        assert!(
            resident.identity_persist_pending(),
            "the in-memory marker armed"
        );
        assert!(
            crate::descriptor::read_identity_pending(&resident.descriptor_path).is_some(),
            "the side record landed"
        );

        // THE RESTART: the stale record back on disk, the side record still carrying the
        // moved-to identity. The boot moves the resident onto the moved-to identity —
        // never the superseded one — and its persist retry repairs the record.
        std::fs::remove_dir_all(&resident.descriptor_path).unwrap();
        let stale_record = std::fs::read_to_string(dir.join("stale.json"))
            .ok()
            .unwrap_or_else(|| {
                // The fixture's stale record: the original identity
                // (s0 / s0.jsonl) the restart must not serve.
                serde_json::json!({
                    "version": 2,
                    "workerId": "w-a",
                    "pid": 0,
                    "socketPath": "/tmp/none.sock",
                    "recoveryJournalPath": "/tmp/none.jsonl",
                    "supervisorSocketPath": "/tmp/none.sock",
                    "authenticationToken": "token-a",
                    "rootActiveSessionId": "w-a",
                    "rootSessionId": "s0",
                    "sessionFile": dir.join("s0.jsonl").to_string_lossy(),
                    "createdAt": "2026-09-30T00:00:00Z",
                    "updatedAt": "2026-09-30T00:00:00Z",
                    "lifecycle": "ready",
                    "createCommand": { "sessionPath": dir.join("s0.jsonl").to_string_lossy() },
                    "consecutiveFailures": 0,
                })
                .to_string()
            });
        std::fs::write(&resident.descriptor_path, &stale_record).unwrap();
        supervisor.apply_identity_pending(&resident).await;
        let (root_session_id, session_file) = {
            let descriptor = resident.descriptor.lock().await;
            (
                descriptor.root_session_id.clone(),
                descriptor.session_file.clone(),
            )
        };
        assert_eq!(
            root_session_id.as_deref(),
            Some("sA"),
            "the boot applies the moved-to identity, never the superseded one"
        );
        assert_eq!(
            session_file.as_deref(),
            Some(dir.join("a.jsonl").to_string_lossy().to_string().as_str())
        );
        assert!(
            crate::descriptor::read_identity_pending(&resident.descriptor_path).is_none(),
            "the boot's repair removed the side record"
        );
        assert!(
            !resident.identity_persist_pending(),
            "the repair cleared the marker"
        );
        let record: DaemonWorkerDescriptor =
            serde_json::from_str(&std::fs::read_to_string(&resident.descriptor_path).unwrap())
                .unwrap();
        assert_eq!(record.root_session_id.as_deref(), Some("sA"));

        // THE FRESHNESS GATE (the stale-rollback class): an OLDER side record left
        // behind by a failed removal must never roll a newer persisted identity
        // back onto its move — the boot ignores it and clears it.
        let mut newer = record.clone();
        newer.updated_at = crate::util::now_iso();
        std::fs::write(
            &resident.descriptor_path,
            serde_json::to_string(&newer).unwrap(),
        )
        .unwrap();
        crate::descriptor::write_identity_pending(
            &resident.descriptor_path,
            "sOld",
            &dir.join("old.jsonl").to_string_lossy(),
            /*cwd*/ None,
            "2020-01-01T00:00:00.000Z",
        )
        .unwrap();
        supervisor.apply_identity_pending(&resident).await;
        let (root_session_id, session_file) = {
            let descriptor = resident.descriptor.lock().await;
            (
                descriptor.root_session_id.clone(),
                descriptor.session_file.clone(),
            )
        };
        assert_eq!(
            root_session_id.as_deref(),
            Some("sA"),
            "the newer persisted identity wins over an older pending"
        );
        assert_eq!(
            session_file.as_deref(),
            Some(dir.join("a.jsonl").to_string_lossy().to_string().as_str())
        );
        assert!(
            crate::descriptor::read_identity_pending(&resident.descriptor_path).is_none(),
            "the obsolete pending is cleared, not applied"
        );

        // THE EQUAL-TIMESTAMP EDGE: a repair persist landing at the exact moment
        // of a stale pending's stamp carries the moved identity — the pending is
        // obsolete at equality too, never applied.
        let equal_stamped = newer.updated_at.clone();
        {
            let mut descriptor = resident.descriptor.lock().await;
            descriptor.updated_at = equal_stamped.clone();
        }
        crate::descriptor::write_identity_pending(
            &resident.descriptor_path,
            "sOld",
            &dir.join("old.jsonl").to_string_lossy(),
            /*cwd*/ None,
            &equal_stamped,
        )
        .unwrap();
        supervisor.apply_identity_pending(&resident).await;
        let root_session_id = resident.descriptor.lock().await.root_session_id.clone();
        assert_eq!(
            root_session_id.as_deref(),
            Some("sA"),
            "an equal-timestamp pending never rolls the record back"
        );
        assert!(
            crate::descriptor::read_identity_pending(&resident.descriptor_path).is_none(),
            "the equal-timestamp pending is cleared, not applied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot repair carries the moved-to session's cwd (legacy TS rows
    /// c3adf81be / 48fae2a5a through the double-failed persist): a restart
    /// that applies the pending identity relaunches the switched-in
    /// transcript in its own directory, never the stale record's launch cwd.
    #[tokio::test]
    async fn a_durable_pending_carries_the_moved_to_cwd_into_the_boot_relaunch() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");
        // Both persists of the move fail: only the side record lands.
        std::fs::remove_file(&resident.descriptor_path).ok();
        std::fs::create_dir_all(&resident.descriptor_path).unwrap();
        let mut moved = swap_summary(&dir, "sA", "a.jsonl");
        moved["cwd"] = json!("/moved/session/cwd");
        drive_delta(&supervisor, moved, 1).await;
        assert!(crate::descriptor::read_identity_pending(&resident.descriptor_path).is_some());

        // THE RESTART: a fresh supervisor loads the stale record (the launch cwd) and
        // applies the side record before any relaunch.
        std::fs::remove_dir_all(&resident.descriptor_path).unwrap();
        let stale = json!({
            "version": 2,
            "workerId": "w-a",
            "pid": 0,
            "socketPath": "/tmp/none.sock",
            "recoveryJournalPath": "/tmp/none.jsonl",
            "supervisorSocketPath": "/tmp/none.sock",
            "authenticationToken": "token-a",
            "rootActiveSessionId": "w-a",
            "rootSessionId": "s0",
            "sessionFile": dir.join("s0.jsonl").to_string_lossy(),
            "createdAt": "2026-09-30T00:00:00Z",
            "updatedAt": "2026-09-30T00:00:00Z",
            "lifecycle": "ready",
            "createCommand": {
                "sessionPath": dir.join("s0.jsonl").to_string_lossy(),
                "cwd": "/launch/cwd",
            },
            "consecutiveFailures": 0,
        });
        std::fs::write(&resident.descriptor_path, stale.to_string()).unwrap();
        let restarted = Supervisor::new(crate::supervisor::SupervisorOptions {
            socket_path: dir.join("daemon.sock"),
            agent_dir: dir.join("agent"),
        })
        .expect("restarted supervisor");
        let fresh = ResidentWorker::new(
            "w-a".to_string(),
            serde_json::from_value(stale).expect("stale descriptor"),
            resident.descriptor_path.clone(),
        );
        restarted.apply_identity_pending(&fresh).await;
        let create_command = fresh.descriptor.lock().await.create_command.clone();
        assert_eq!(
            (
                create_command.session_path,
                create_command.rest.get("cwd").cloned()
            ),
            (
                Some(dir.join("a.jsonl").to_string_lossy().to_string()),
                Some(json!("/moved/session/cwd"))
            ),
            "the relaunch replays the moved-to session and its own directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_child_roster_delta_never_lifts_the_root_quarantine() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");
        resident.mark_identity_quarantined();
        assert!(resident.identity_quarantined());

        // A CHILD summary under the root's token: the row keys under the child's
        // address, so the root's quarantine holds.
        let child_summary = json!({
            "id": "child-1",
            "lifecycle": "live",
            "activity": "idle",
            "isSessionActive": false,
            "activeSessionId": "child-1",
            "sessionId": "s-child",
            "sessionFile": dir.join("c.jsonl").to_string_lossy(),
            "sessionName": "child",
            "cwd": dir.to_string_lossy(),
            "rlmDepth": 1,
            "runtimeKind": "subagent",
            "rlmChildId": "c1",
            "parentActiveSessionId": "w-a",
            "messageCount": 0,
            "attachedClients": 0,
            "thinkingLevel": "default",
            "workerState": "ready",
        });
        let response = supervisor
            .handle_worker_roster_delta(
                "d1",
                "worker_roster_delta",
                WorkerRosterDelta {
                    worker_token: "token-a".to_string(),
                    summary: child_summary,
                    removed: Vec::new(),
                    sequence: Some(1),
                    worker_instance_id: Some("i1".to_string()),
                },
            )
            .await;
        assert!(response.success, "the child delta applied: {response:?}");
        assert!(
            resident.identity_quarantined(),
            "a child delta never lifts the root's quarantine"
        );

        // The root's own roster write lifts the fence.
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 2).await;
        assert!(
            !resident.identity_quarantined(),
            "the root's own write lifts the quarantine"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_path_backed_replacement_clears_the_no_session_replay_flag() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = Arc::new(
            Supervisor::new(crate::supervisor::SupervisorOptions {
                socket_path: dir.join("daemon.sock"),
                agent_dir: dir.join("agent"),
            })
            .expect("supervisor"),
        );
        // #42 Kind-1 fix: this test's own inline descriptor needs the same
        // identity-matching `supervisor_with_movable_worker` already carries
        // (review-record-lifecycle-kind1-root-identity flagged this test as
        // "same mechanism, different fixture, out of scope" - applying the
        // identical fix here now). The delta's own `drive_delta` call below
        // targets `sessionFile: "a.jsonl"` - `publish_verified_summary`'s
        // ownership check runs against THAT file (not this descriptor's own,
        // initially-fileless, `createCommand.noSession` state), so it still
        // needs a real, fence-acquired lease via `ensure_session_leased`.
        let pid = std::process::id();
        let process_start_id = crate::lease::get_process_start_id(pid);
        let descriptor: DaemonWorkerDescriptor = serde_json::from_value(json!({
            "version": 2,
            "workerId": "w-a",
            "pid": pid,
            "processStartId": process_start_id,
            "socketPath": "/tmp/none.sock",
            "recoveryJournalPath": "/tmp/none.jsonl",
            "supervisorSocketPath": "/tmp/none.sock",
            "authenticationToken": "token-a",
            "workerInstanceId": "i1",
            "rootActiveSessionId": "w-a",
            "createdAt": "2026-09-30T00:00:00Z",
            "updatedAt": "2026-09-30T00:00:00Z",
            "lifecycle": "ready",
            "createCommand": { "noSession": true },
            "consecutiveFailures": 0,
        }))
        .expect("descriptor");
        let resident = ResidentWorker::new("w-a".to_string(), descriptor, dir.join("w-a.json"));
        resident.note_session_ready();
        supervisor.registry.insert(resident).await;

        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 1).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");
        let (session_path, no_session) = {
            let descriptor = resident.descriptor.lock().await;
            (
                descriptor.create_command.session_path.clone(),
                descriptor.create_command.no_session,
            )
        };
        assert_eq!(
            session_path.as_deref(),
            Some(dir.join("a.jsonl").to_string_lossy().to_string().as_str())
        );
        assert_eq!(
            no_session, None,
            "the replay command drops the in-memory flag with the path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_quarantined_resident_never_serves_until_the_live_word_lands() {
        let dir = std::env::temp_dir().join(format!("pa-root-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let supervisor = supervisor_with_movable_worker(&dir).await;
        let resident = supervisor.registry.get("w-a").await.expect("resident");
        let stale_file = dir.join("s0.jsonl").to_string_lossy().to_string();

        // The boot outcome: the reconciliation pull refused (the resident
        // has no worker channel), the quarantine fenced it.
        assert!(
            !supervisor.refresh_roster_entry(&resident).await,
            "the pull refuses on the channel-less resident"
        );
        resident.mark_identity_quarantined();
        assert!(resident.identity_quarantined());

        // THE FENCES: the address, the stale file stem, and the by-file reuse all
        // refuse — the failure reads as the unknown session, never the stale
        // identity.
        assert!(
            supervisor.registry.resolve("w-a").await.is_err(),
            "the quarantined address never resolves"
        );
        assert!(
            supervisor.registry.resolve("s0").await.is_err(),
            "the stale persisted file stem never resolves"
        );
        assert!(
            supervisor
                .registry
                .find_by_session_file(&stale_file)
                .await
                .is_none(),
            "the stale persisted file never reuses the quarantined worker"
        );

        // THE LIVE WORD: the worker's own roster push (the delta path —
        // no command channel involved) carries the identity follow.
        drive_delta(&supervisor, swap_summary(&dir, "sA", "a.jsonl"), 1).await;
        assert!(
            !resident.identity_quarantined(),
            "the accepted roster write cleared the quarantine"
        );

        // THE ROUTING OPENS on the reconciled identity.
        let resolved = supervisor
            .registry
            .resolve("w-a")
            .await
            .expect("the address resolves once reconciled");
        let (root_session_id, session_file) = {
            let descriptor = resolved.descriptor.lock().await;
            (
                descriptor.root_session_id.clone(),
                descriptor.session_file.clone(),
            )
        };
        assert_eq!(root_session_id.as_deref(), Some("sA"));
        assert_eq!(
            session_file.as_deref(),
            Some(dir.join("a.jsonl").to_string_lossy().to_string().as_str())
        );
        let reconciled_file = dir.join("a.jsonl").to_string_lossy().to_string();
        assert!(
            supervisor
                .registry
                .find_by_session_file(&reconciled_file)
                .await
                .is_some(),
            "the reconciled file reuses the worker"
        );
        assert!(
            supervisor.registry.resolve("s0").await.is_err(),
            "the superseded persisted stem stays unresolvable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
