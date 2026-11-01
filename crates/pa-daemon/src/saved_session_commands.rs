//! The saved-session catalog and peer-roster surface: the supervisor arms
//! for `rename_saved_session`, `delete_saved_session`, and
//! `list_agent_peers`; the worker arms the selector forms forward to.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use pa_types::daemon::DaemonCommand;

use crate::lease::canonical_session_path;
use crate::protocol::{response_failure, response_line, response_success, DaemonResponse};
use crate::session_store::{read_session_info, SessionFile};
use crate::supervisor::Supervisor;
use crate::worker::Worker;

mod file_delete;
pub(crate) use file_delete::delete_session_file_after_file_removed;

pub(crate) fn name_unavailable_error(name: &str, depth: u32) -> String {
    format!(
        "Agent name \"{name}\" is unavailable: an agent of that name already exists at depth {depth} under this parent"
    )
}

/// The reservation key: the JSON-encoded `[depth, parentType, parentValue, name]` tuple - the
/// parent keyed by its path, its persisted id, or the root scope.
pub(crate) fn reservation_key(scope: &NameScope) -> String {
    let (parent_type, parent_value) = match (
        scope.depth,
        scope.parent_session_path.as_deref(),
        scope.parent_session_id.as_deref(),
    ) {
        (0, _, _) | (_, None, None) => ("root".to_string(), String::new()),
        (_, Some(path), _) => (
            "path".to_string(),
            canonical_session_path(Path::new(path))
                .to_string_lossy()
                .to_string(),
        ),
        (_, None, Some(id)) => ("id".to_string(), id.to_string()),
    };
    json!([scope.depth, parent_type, parent_value, scope.name]).to_string()
}

/// The name scope a rename target carries into the availability check;
/// spawn admissions reserve under the same `[depth, parent, name]` key.
pub(crate) struct NameScope {
    /// The renamed session's own id (the availability check ignores it).
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) depth: u32,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) parent_session_path: Option<String>,
}

/// The roster summary's name-reservation fields, the one projection both
/// rename scopes read (a saved row by file, a live row by active id).
fn name_scope_from_summary(summary: &Value, name: String) -> NameScope {
    NameScope {
        id: summary
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name,
        depth: summary.get("rlmDepth").and_then(Value::as_u64).unwrap_or(0) as u32,
        parent_session_id: summary
            .get("parentSessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_session_path: summary
            .get("parentSessionPath")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// One family-catalog row (TS `AgentFamilyCatalogEntry`): the fields the
/// name-availability assertion reads.
struct FamilyRow {
    id: String,
    name: Option<String>,
    depth: u32,
    parent_session_path: Option<String>,
}

/// Depth-0 rows share one scope; deeper rows must share the parent path.
fn same_name_parent(left: &FamilyRow, right: &NameScope) -> bool {
    if left.depth == 0 && right.depth == 0 {
        return true;
    }
    if left.depth != right.depth {
        return false;
    }
    left.parent_session_path.is_some() && left.parent_session_path == right.parent_session_path
}

pub(crate) fn append_saved_session_name(path: &Path, name: &str) -> anyhow::Result<()> {
    let mut session = SessionFile::open(path)?;
    session.append_session_info(name);
    session.rewrite()
}

/// How a saved-session delete removes the file. It is chosen when the
/// receiver is built, never by an environment variable or setting.
/// Production always uses `TrashThenUnlink`. Test builds default to an
/// explicit unlink, so no test can move a real file to the user's Trash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionFileRemoval {
    /// The shared #41 Trash helper: the installed `trash` tool, or an
    /// explicit unlink only when no tool is installed.
    TrashThenUnlink,
    /// Test only: unlink without looking for the trash tool.
    #[cfg(test)]
    Unlink,
    /// Test only: the physical removal fails and the file stays.
    #[cfg(test)]
    Fail,
}

// Not derivable: the default VARIES by build config (test vs production) -
// a #[derive(Default)] + #[default] attribute cannot express that split.
#[allow(clippy::derivable_impls)]
impl Default for SessionFileRemoval {
    fn default() -> Self {
        #[cfg(test)]
        {
            Self::Unlink
        }
        #[cfg(not(test))]
        {
            Self::TrashThenUnlink
        }
    }
}

/// `deleteSessionFile` with the construction-time removal strategy. The
/// production strategy is the shared #41 Trash helper (`file_delete`): an
/// installed tool failure keeps the file and errors, an absent tool unlinks
/// explicitly (reported as `unlink`), and a missing entry is never reported
/// as trashed. Test strategies never resolve or run the real tool.
pub(crate) fn delete_session_file_with(
    path: &Path,
    after_file_removed: &dyn Fn(&Path),
    removal: SessionFileRemoval,
) -> Value {
    match removal {
        SessionFileRemoval::TrashThenUnlink => {
            // Hermetic deny guard: a unit test must never reach the user's
            // Trash - a RUNTIME check deliberately, not a const block: this
            // must only fire if this branch is actually reached during a
            // test build, never unconditionally at compile time (a const
            // block evaluates immediately regardless of whether
            // TrashThenUnlink is ever matched, which would make every test
            // build fail to compile at all, not just refuse this branch).
            #[allow(clippy::assertions_on_constants)]
            {
                assert!(
                    !cfg!(test),
                    "tests must never invoke the real trash tool; use SessionFileRemoval::Unlink"
                );
            }
            delete_session_file_after_file_removed(path, after_file_removed)
        }
        #[cfg(test)]
        SessionFileRemoval::Unlink => match std::fs::remove_file(path) {
            Ok(()) => {
                after_file_removed(path);
                remove_session_artifacts(path);
                json!({ "ok": true, "method": "unlink" })
            }
            Err(error) => json!({ "ok": false, "error": error.to_string() }),
        },
        #[cfg(test)]
        SessionFileRemoval::Fail => {
            json!({ "ok": false, "error": "injected physical removal failure" })
        }
    }
}

/// Remove the session's artifact partition
/// `<root>/session-artifacts/<id>`, only once the session file is gone.
pub(crate) fn remove_session_artifacts(session_path: &Path) {
    let Some(stem) = session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
    else {
        return;
    };
    let Some(artifacts) = crate::scheduled_jobs::session_artifact_dir(session_path, stem) else {
        return;
    };
    let _ = std::fs::remove_dir_all(artifacts);
}

/// The RLM ledger rename for an offline saved-session rename (best effort).
pub(crate) fn ledger_rename_by_child_path(
    agent_dir: &Path,
    sessions_dir: &Path,
    child_path: &str,
    name: &str,
) {
    let ledger = crate::rlm_ledger::RlmSpawnLedger::new(agent_dir, sessions_dir, |_| {});
    if let Err(error) = ledger.append_rename_by_child_path(child_path, name) {
        eprintln!("failed to append RLM ledger rename: {error:#}");
    }
}

/// The pre-delete capture (phase 1 — the reads need the file ALIVE):
/// `TopLevel` never tombstones; a child carries its whole-file own usage
/// (absent when no billable work — the tombstone still lands bare).
pub(crate) enum SavedDeleteCapture {
    TopLevel,
    Child {
        usage: Option<crate::session_usage::SessionUsageSummary>,
        /// The canonical session key captured while the file still existed:
        /// a symlink delete unlinks the LINK, so the tombstone uses the pre-unlink key.
        canonical_path: String,
    },
}

pub(crate) fn capture_saved_session_delete(
    session_path: &str,
    known_runtime_kind: Option<&str>,
) -> SavedDeleteCapture {
    // A non-regular file (a FIFO, a device, a directory) is never a
    // session, and opening one for reading BLOCKS indefinitely on Linux —
    // the capture reads nothing there.
    let regular = std::fs::metadata(Path::new(session_path)).is_ok_and(|meta| meta.is_file());
    let canonical_path = canonical_session_path(Path::new(session_path))
        .to_string_lossy()
        .to_string();
    let deleted_info = if regular {
        read_session_info(Path::new(session_path))
    } else {
        None
    };
    let known_child = known_runtime_kind == Some("subagent")
        || deleted_info
            .as_ref()
            .is_some_and(|info| info.parent_session_path.is_some() || info.rlm_depth > 0);
    let positively_top_level =
        !known_child && (known_runtime_kind.is_some() || deleted_info.is_some());
    if positively_top_level {
        return SavedDeleteCapture::TopLevel;
    }
    let usage = if regular {
        crate::session_usage::read_own_usage_summary(Path::new(session_path))
    } else {
        None
    };
    SavedDeleteCapture::Child {
        usage,
        canonical_path,
    }
}

/// What one saved-delete tombstone commit wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SavedTombstone {
    /// Ledger edges at the path that now carry a tombstone.
    pub(crate) edges: usize,
    /// Edges that also received the captured usage snapshot.
    pub(crate) captured: usize,
}

/// Append the captured child's tombstone to one ledger.
fn append_saved_session_tombstone(
    ledger: &crate::rlm_ledger::RlmSpawnLedger,
    capture: &SavedDeleteCapture,
) -> anyhow::Result<SavedTombstone> {
    let SavedDeleteCapture::Child {
        usage,
        canonical_path,
    } = capture
    else {
        return Ok(SavedTombstone::default());
    };
    let edges = ledger.tombstone_child_path_with_usage(
        canonical_path,
        crate::rlm_ledger::RlmLedgerDeleteReason::User,
        usage.as_ref(),
    )?;
    // The returned edges are the pre-append replay state: every
    // matching edge received the snapshot when one was captured.
    Ok(SavedTombstone {
        edges: edges.len(),
        captured: if usage.is_some() { edges.len() } else { 0 },
    })
}

/// #42 saved-delete commit: under the target's common fence M, prove the
/// ledger reads strictly and no live runtime owns the file, then append the
/// tombstone. Only an Ok proves the tombstone committed. The physical
/// removal runs after M is released, so a failure there leaves a committed
/// logical deletion (a partial result), never a removed file without a
/// tombstone.
pub(crate) fn commit_saved_session_tombstone(
    authority: &crate::lease::RuntimeSessionAuthority,
    capture: &SavedDeleteCapture,
) -> anyhow::Result<SavedTombstone> {
    let SavedDeleteCapture::Child { canonical_path, .. } = capture else {
        return Ok(SavedTombstone::default());
    };
    let ledger = std::sync::Arc::clone(authority.ledger());
    crate::lease::with_runtime_session_fence(
        authority.agent_dir(),
        Path::new(canonical_path),
        |held| {
            ledger.strict_edges()?;
            match held.inspect_ownership()? {
                crate::lease::RuntimeLeaseOwnership::Absent
                | crate::lease::RuntimeLeaseOwnership::Stale => {}
                crate::lease::RuntimeLeaseOwnership::Unknown => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "Runtime ownership is unknown; deletion deferred",
                    )
                    .into());
                }
                crate::lease::RuntimeLeaseOwnership::Held(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "Another runtime owns this session; deletion deferred",
                    )
                    .into());
                }
            }
            append_saved_session_tombstone(&ledger, capture)
        },
    )
}

/// The saved delete's answer when the tombstone committed but the physical
/// removal failed: the TS failure object plus the committed logical deletion.
fn partial_saved_delete_result(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("logicalDeletion".to_string(), json!("committed"));
    }
    result
}

/// Best-effort tombstone append for the capture unit tests.
#[cfg(test)]
pub(crate) fn tombstone_saved_session_delete_captured(
    agent_dir: &Path,
    sessions_dir: &Path,
    capture: &SavedDeleteCapture,
) -> usize {
    let ledger = crate::rlm_ledger::RlmSpawnLedger::new(agent_dir, sessions_dir, |_| {});
    match append_saved_session_tombstone(&ledger, capture) {
        Ok(tombstone) => tombstone.captured,
        Err(error) => {
            eprintln!("failed to append RLM ledger delete tombstone: {error:#}");
            0
        }
    }
}

impl Supervisor {
    /// The family-catalog rows: the roster rows plus the saved depth-0
    /// sessions the roster does not know.
    fn family_rows(&self, extra_sessions_dir: Option<&Path>) -> Vec<FamilyRow> {
        let mut rows = Vec::new();
        {
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for entry in roster.entries() {
                let summary = &entry.summary;
                rows.push(FamilyRow {
                    id: summary
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: summary
                        .get("sessionName")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    depth: summary.get("rlmDepth").and_then(Value::as_u64).unwrap_or(0) as u32,
                    parent_session_path: summary
                        .get("parentSessionPath")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        if let Some(dir) = extra_sessions_dir {
            // The catalog scan takes depth-0 rows only: a parented file
            // without a recorded depth reads as -1 and stays out.
            for info in crate::session_store::list_sessions(dir) {
                if info.rlm_depth != 0 || info.parent_session_path.is_some() {
                    continue;
                }
                rows.push(FamilyRow {
                    id: info.id.clone(),
                    name: info.name.clone(),
                    depth: 0,
                    parent_session_path: None,
                });
            }
        }
        rows
    }

    /// A same-name, same-depth, same-parent row that is not the renamed session itself conflicts.
    fn assert_family_name_available(&self, scope: &NameScope) -> Result<(), String> {
        let rows = self.family_rows(self.sessions_dir_path().as_deref());
        for row in rows {
            if row.id == scope.id || row.name.as_deref() != Some(scope.name.as_str()) {
                continue;
            }
            if row.depth == scope.depth && same_name_parent(&row, scope) {
                return Err(name_unavailable_error(&scope.name, scope.depth));
            }
        }
        Ok(())
    }

    /// The supervisor's sessions dir (the catalog's scan root).
    fn sessions_dir_path(&self) -> Option<PathBuf> {
        crate::paths::sessions_dir(&self.options.agent_dir).ok()
    }

    /// The name-reservation input: the live roster row for the path, else
    /// the saved session info; a miss answers `Session not found`.
    fn saved_session_name_scope(
        &self,
        session_path: &str,
        name: String,
    ) -> Result<NameScope, String> {
        let canonical = canonical_session_path(Path::new(session_path))
            .to_string_lossy()
            .to_string();
        let roster_row = {
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            roster
                .by_session_file(&canonical)
                .map(|entry| name_scope_from_summary(&entry.summary, name.clone()))
        };
        if let Some(scope) = roster_row {
            return Ok(scope);
        }
        match read_session_info(Path::new(session_path)) {
            Some(info) => Ok(NameScope {
                id: info.id.clone(),
                name,
                depth: info.rlm_depth,
                parent_session_id: None,
                parent_session_path: info.parent_session_path,
            }),
            None => Err(format!("Session not found: {session_path}")),
        }
    }

    /// `rename_saved_session` (TS supervisor arm): the reservation ladder,
    /// then the offline catalog rename or the forward to the live worker.
    pub(crate) async fn handle_rename_saved_session(
        self: &Arc<Self>,
        command: &DaemonCommand,
        client_id: &str,
        attached: &Arc<crate::supervisor::subscribers::ClientSubscriptions>,
        command_id: &str,
        type_name: &str,
    ) -> (Vec<Value>, bool) {
        let DaemonCommand::RenameSavedSession {
            active_session_id,
            session_path,
            name,
            ..
        } = command
        else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "invalid command",
                    None,
                ))],
                false,
            );
        };
        let scope = match self.saved_session_name_scope(session_path, name.trim().to_string()) {
            Ok(scope) => scope,
            Err(error) => {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &error,
                        None,
                    ))],
                    false,
                )
            }
        };
        // The reservation ladder wraps the whole arm — the offline catalog
        // rename and the live forward alike (TS `withSessionNameReservation`
        // holds the key across both).
        match self
            .with_session_name_reservation(&scope, async {
                if active_session_id.is_none() {
                    // The offline catalog rename (TS `catalog.rename`):
                    // append the session_info entry, the ledger rename,
                    // and the roster row rewrite.
                    let path = Path::new(session_path);
                    if let Err(error) = append_saved_session_name(path, &scope.name) {
                        return (
                            vec![response_line(&response_failure(
                                Some(command_id),
                                type_name,
                                &error.to_string(),
                                None,
                            ))],
                            false,
                        );
                    }
                    if let Some(sessions_dir) = self.sessions_dir_path() {
                        ledger_rename_by_child_path(
                            &self.options.agent_dir,
                            &sessions_dir,
                            session_path,
                            &scope.name,
                        );
                    }
                    self.rewrite_roster_session_name(session_path, &scope.name);
                    // The renamed session file can carry passive scheduled
                    // rows: the catalog snapshot's rows key off the session
                    // file (TS #2487 invalidates the shared snapshot on the
                    // saved-session rename).
                    self.invalidate_passive_catalog();
                    return (
                        vec![response_line(&response_success(
                            Some(command_id),
                            type_name,
                            None,
                        ))],
                        false,
                    );
                }
                // The live form forwards to the owning worker (TS rewrites
                // the selector onto the resolved summary's ids).
                self.route_client_command(
                    command,
                    client_id,
                    attached,
                    command_id.to_string(),
                    type_name.to_string(),
                    None,
                )
                .await
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    &error,
                    None,
                ))],
                false,
            ),
        }
    }

    /// The rename reservation ladder (TS `withSessionNameReservation` +
    /// `assertSupervisorSessionNameAvailable`, the supervisor's
    /// daemon-owned name uniqueness): reserve the scope's
    /// `[depth, parent, name]` key, assert family name availability, run
    /// `action` under the reservation, and release the key on every exit —
    /// a same-key concurrent rename fails with the unavailability error,
    /// and a family conflict fails before the action runs. The live
    /// `rename`/`set_session_name` forwards and the saved-session arm
    /// share this one ladder.
    pub(crate) async fn with_session_name_reservation<T>(
        &self,
        scope: &NameScope,
        action: impl std::future::Future<Output = T>,
    ) -> Result<T, String> {
        let key = reservation_key(scope);
        let reserved = {
            let mut pending = self
                .pending_session_names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.insert(key.clone())
        };
        if !reserved {
            return Err(name_unavailable_error(&scope.name, scope.depth));
        }
        let output = match self.assert_family_name_available(scope) {
            Ok(()) => Ok(action.await),
            Err(error) => Err(error),
        };
        self.pending_session_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        output
    }

    /// The name-reservation scope for a LIVE session's rename forward
    /// (TS `summaryNameReservationInput` over the routed worker's matched
    /// summary): the target's roster row, the same fields
    /// [`Self::saved_session_name_scope`] reads for a saved row.
    pub(crate) fn live_session_name_scope(
        &self,
        active_session_id: &str,
        name: String,
    ) -> Result<NameScope, String> {
        let scope = {
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            roster
                .by_active_session_id(active_session_id)
                .map(|entry| name_scope_from_summary(&entry.summary, name))
        };
        scope.ok_or_else(|| format!("Unknown active session: {active_session_id}"))
    }

    fn rewrite_roster_session_name(&self, session_path: &str, name: &str) {
        let canonical = canonical_session_path(Path::new(session_path))
            .to_string_lossy()
            .to_string();
        let changed = {
            let mut roster = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(entry) = roster.by_session_file(&canonical).cloned() else {
                return;
            };
            let mut summary = entry.summary.clone();
            summary["sessionName"] = json!(name);
            let worker_id = entry.worker_id.clone();
            let status_label = entry.status_label;
            Some(roster.write_summary(summary, worker_id.as_deref(), status_label.as_deref()))
        };
        if let Some(entry) = changed {
            self.push_roster_update(vec![entry], Vec::new());
        }
    }

    /// `delete_saved_session` selector-less: refuse the active session, let a live connected worker
    /// own the delete, tombstone the ledger, delete the file, drop the roster row.
    pub(crate) async fn handle_delete_saved_session(
        self: &Arc<Self>,
        command: &DaemonCommand,
        client_id: &str,
        command_id: &str,
        type_name: &str,
    ) -> (Vec<Value>, bool) {
        let DaemonCommand::DeleteSavedSession { session_path, .. } = command else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "invalid command",
                    None,
                ))],
                false,
            );
        };
        let _child_lifecycle = match self.child_lifecycles.lock(session_path).await {
            Ok(guard) => guard,
            Err(error) => {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &format!("Session delete could not acquire lifecycle ownership: {error:#}"),
                        None,
                    ))],
                    false,
                )
            }
        };
        let canonical = canonical_session_path(Path::new(session_path))
            .to_string_lossy()
            .to_string();
        let roster_entry = {
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            roster.by_session_file(&canonical).cloned()
        };
        if let Some(entry) = roster_entry.as_ref() {
            let active = entry
                .summary
                .get("activeSessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty());
            if active {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        "Cannot delete the currently active session",
                        None,
                    ))],
                    false,
                );
            }
        }
        // A worker still hosting the file owns the delete; another
        // client's worker is invisible (the unknown-target error).
        if let Some(owner) = self.registry.find_by_session_file(session_path).await {
            let owner_id = {
                let descriptor = owner.descriptor.lock().await;
                descriptor.owner_client_id.clone()
            };
            if owner_id.is_some_and(|owner| owner != client_id) {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &format!("Unknown active session: {session_path}"),
                        None,
                    ))],
                    false,
                );
            }
            if self.is_live_connected_worker(&owner).await {
                let mut response = self
                    .forward_with_catalog_timeout(&owner, command, client_id)
                    .await;
                response.id = Some(command_id.to_string());
                // The owning worker deleted the file: its binding dies with
                // it (no successor worker can ever take the file over).
                if response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("ok"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    self.session_bindings.forget_file(&canonical);
                    if let Err(error) = self.passivated_launches.forget_deleted(session_path) {
                        self.log_line(&format!(
                            "deleted session launch record retained: {error:#}"
                        ));
                        response = response_failure(
                            Some(command_id),
                            type_name,
                            &format!(
                                "Session deleted, but launch record cleanup failed: {error:#}"
                            ),
                            None,
                        );
                    }
                }
                return (vec![response_line(&response)], false);
            }
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "Session worker is recovering; retry the delete once it is reachable",
                    None,
                ))],
                false,
            );
        }
        // Phase 1 while the file is alive: the capture reads the transcript.
        let capture = capture_saved_session_delete(
            session_path,
            roster_entry
                .as_ref()
                .and_then(|entry| entry.summary.get("runtimeKind"))
                .and_then(Value::as_str),
        );
        // #42: the tombstone commits under M BEFORE any physical removal. A
        // failed commit removes nothing, so the transcript and its ledger
        // edge stay consistent and the delete can be retried.
        let committed = match crate::lease::RuntimeSessionAuthority::for_supervisor(
            &self.options.agent_dir,
            &self.options.socket_path,
        )
        .and_then(|authority| commit_saved_session_tombstone(&authority, &capture))
        {
            Ok(tombstone) => tombstone,
            Err(error) => {
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &format!(
                            "Session delete was not committed; nothing was removed: {error:#}"
                        ),
                        None,
                    ))],
                    false,
                );
            }
        };
        if committed.captured > 0 {
            self.note_deleted_child_usage_captured("saved_delete", committed.captured);
        }
        let logical = committed.edges > 0;
        let result =
            delete_session_file_with(Path::new(session_path), &|_| {}, self.session_file_removal);
        let removed = result.get("ok").and_then(Value::as_bool) == Some(true);
        if removed {
            // A deleted session file can carry passive scheduled rows: the catalog must rescan.
            self.invalidate_passive_catalog();
            // The deleted file's binding dies with it: a stale id for the
            // session can never rebind again. `canonical` was resolved while
            // the file still existed - the same key the table stores.
            self.session_bindings.forget_file(&canonical);
            // The tombstone above changed the ledger, whether or not the
            // deleted session had a roster row (a subagent of a stopped
            // parent has none - its only row was the saved listing's):
            // every successful delete refreshes the bucket, and only the
            // row removal stays conditional. The refreshed rows and the
            // removal ship in one push.
            let changed = self.refresh_deleted_descendant_usage().await;
            let removed_ids = match roster_entry {
                Some(entry) => {
                    let agent_id = entry.agent_id;
                    self.roster
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .delete(&agent_id);
                    vec![agent_id]
                }
                None => Vec::new(),
            };
            self.push_roster_update(changed, removed_ids);
            if let Err(error) = self.passivated_launches.forget_deleted(session_path) {
                self.log_line(&format!(
                    "deleted session launch record retained: {error:#}"
                ));
                return (
                    vec![response_line(&response_failure(
                        Some(command_id),
                        type_name,
                        &format!("Session deleted, but launch record cleanup failed: {error:#}"),
                        None,
                    ))],
                    false,
                );
            }
        }
        let result = if !removed && logical {
            // Committed tombstone, failed physical removal: the child is
            // deleted (no revival) and the transcript is retained for a retry.
            if let Err(error) = self.passivated_launches.forget_deleted(session_path) {
                self.log_line(&format!(
                    "deleted session launch record retained: {error:#}"
                ));
            }
            partial_saved_delete_result(result)
        } else {
            result
        };
        (
            vec![response_line(&response_success(
                Some(command_id),
                type_name,
                Some(result),
            ))],
            false,
        )
    }

    /// `list_agent_peers`: the requester authenticates with its worker
    /// token; every other live ready connected worker answers as a peer.
    pub(crate) async fn handle_list_agent_peers(
        self: &Arc<Self>,
        command: &DaemonCommand,
        command_id: &str,
        type_name: &str,
    ) -> (Vec<Value>, bool) {
        let DaemonCommand::ListAgentPeers { worker_token, .. } = command else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "invalid command",
                    None,
                ))],
                false,
            );
        };
        let Some(requester) = self.registry.find_by_token(worker_token).await else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "Worker authentication failed",
                    None,
                ))],
                false,
            );
        };
        let mut peers = Vec::new();
        for resident in self.registry.list().await {
            if resident.worker_id == requester.worker_id
                || !self.is_live_connected_worker(&resident).await
            {
                continue;
            }
            let (root_active_session_id, worker_instance_id) = {
                let descriptor = resident.descriptor.lock().await;
                (
                    descriptor.root_active_session_id.clone(),
                    descriptor.worker_instance_id.clone(),
                )
            };
            if root_active_session_id.is_empty() {
                continue;
            }
            let summary = {
                let roster = self
                    .roster
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                roster
                    .by_active_session_id(&root_active_session_id)
                    .map(|entry| entry.summary.clone())
            };
            if let Some(summary) = summary {
                // A replacement may not have pushed yet. Never serve the old
                // process's snapshot as the new resident's live state.
                if summary
                    .get("workerInstanceId")
                    .and_then(Value::as_str)
                    .is_some_and(|instance| Some(instance) != worker_instance_id.as_deref())
                {
                    continue;
                }
                peers.push(agent_peer_summary(&summary));
            }
        }
        (
            vec![response_line(&response_success(
                Some(command_id),
                type_name,
                Some(json!({ "peers": peers })),
            ))],
            false,
        )
    }
}

/// The roster row's summary projected onto the agent-message peer shape.
fn agent_peer_summary(summary: &Value) -> Value {
    let mut peer = json!({
        "activeSessionId": summary
            .get("activeSessionId")
            .and_then(Value::as_str)
            .or_else(|| summary.get("id").and_then(Value::as_str))
            .unwrap_or_default(),
        "sessionId": summary
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "runtimeKind": summary
            .get("runtimeKind")
            .and_then(Value::as_str)
            .unwrap_or("top-level"),
        "cwd": summary.get("cwd").and_then(Value::as_str).unwrap_or_default(),
        "isStreaming": summary
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "unfinishedActionCount": summary
            .get("unfinishedActionCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    });
    if let Some(name) = summary.get("sessionName").and_then(Value::as_str) {
        peer["sessionName"] = json!(name);
    }
    for key in [
        "parentActiveSessionId",
        "parentSessionId",
        "parentSessionPath",
        // The child's live note and tool count ride the peer rows: the
        // parent registry's roster read joins them onto its subagent rows.
        "progressNote",
        "toolUseCount",
        "workerInstanceId",
        "rosterDeltaSequence",
    ] {
        if let Some(value) = summary.get(key).filter(|value| !value.is_null()) {
            peer[key] = value.clone();
        }
    }
    if let Some(file) = summary.get("sessionFile").and_then(Value::as_str) {
        peer["sessionPath"] = json!(file);
    }
    if let Some(depth) = summary.get("rlmDepth").and_then(Value::as_u64) {
        peer["rlmDepth"] = json!(depth);
    }
    if let Some(kind) = summary.get("rosterStatus").and_then(Value::as_str) {
        peer["status"] = json!(kind);
    } else {
        peer["status"] = json!("idle");
    }
    if let Some(child_id) = summary.get("rlmChildId").and_then(Value::as_str) {
        peer["rlmChildId"] = json!(child_id);
    }
    peer
}

impl Worker {
    /// `rename_saved_session`: a live target renames through the session's
    /// own rename path (answering with no data); an offline file gets the catalog append.
    pub(crate) fn handle_rename_saved_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("rename_saved_session") {
            return response;
        }
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            return response_failure(
                None,
                "rename_saved_session",
                "Session name cannot be empty",
                None,
            );
        }
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let own_file = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            core.store.as_ref().is_some_and(|store| {
                !store.path.as_os_str().is_empty()
                    && canonical_session_path(&store.path)
                        == canonical_session_path(Path::new(session_path))
            })
        };
        if own_file {
            // The live form answers success with no data, unlike the
            // `rename` command's summary.
            let mut payload = Map::new();
            payload.insert("name".to_string(), json!(name));
            let mut response = self.handle_rename("rename_saved_session", &Value::Object(payload));
            response.data = None;
            return response;
        }
        let path = Path::new(session_path);
        if read_session_info(path).is_none() {
            return response_failure(
                None,
                "rename_saved_session",
                &format!("Session not found: {session_path}"),
                None,
            );
        }
        match append_saved_session_name(path, &name) {
            Ok(()) => response_success(None, "rename_saved_session", None),
            Err(error) => response_failure(None, "rename_saved_session", &error.to_string(), None),
        }
    }

    /// `delete_saved_session`: refuse the live session, tombstone the
    /// ledger, delete the file and its artifacts, answer the delete result.
    pub(crate) async fn handle_delete_saved_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("delete_saved_session") {
            return response;
        }
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let path = Path::new(session_path);
        let own_file = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            core.store.as_ref().is_some_and(|store| {
                !store.path.as_os_str().is_empty()
                    && canonical_session_path(&store.path) == canonical_session_path(path)
            })
        };
        if own_file {
            return response_failure(
                None,
                "delete_saved_session",
                "Cannot delete the currently active session",
                None,
            );
        }
        // Phase 1 while the file is alive: the capture reads the transcript.
        let capture = capture_saved_session_delete(session_path, None);
        // #42: the tombstone commits under M before any physical removal; a
        // failed commit removes nothing (same order as the supervisor arm).
        let authority = match &self.authority {
            Ok(authority) => authority,
            Err(error) => {
                return response_failure(
                    None,
                    "delete_saved_session",
                    &format!("Session delete was not committed; nothing was removed: {error}"),
                    None,
                );
            }
        };
        let committed = match commit_saved_session_tombstone(authority, &capture) {
            Ok(tombstone) => tombstone,
            Err(error) => {
                return response_failure(
                    None,
                    "delete_saved_session",
                    &format!("Session delete was not committed; nothing was removed: {error:#}"),
                    None,
                );
            }
        };
        // The hook runs between the file's removal and the partition's
        // removal — the durable job cancel (needs the partition registered).
        let result = delete_session_file_with(
            path,
            &|deleted| {
                self.cancel_deleted_session_jobs(deleted);
            },
            SessionFileRemoval::default(),
        );
        let removed = result.get("ok").and_then(Value::as_bool) == Some(true);
        if removed {
            self.scheduled.wake().await;
        }
        let result = if !removed && committed.edges > 0 {
            partial_saved_delete_result(result)
        } else {
            result
        };
        response_success(None, "delete_saved_session", Some(result))
    }
}

#[cfg(test)]
mod tombstone_usage_tests {
    use super::*;
    use serde_json::json;

    /// The two phases in one call; the real delete handler keeps them
    /// SPLIT. Only the tests use the combined shape.
    fn tombstone_saved_session_delete(
        agent_dir: &Path,
        sessions_dir: &Path,
        session_path: &str,
        known_runtime_kind: Option<&str>,
    ) -> usize {
        let capture = capture_saved_session_delete(session_path, known_runtime_kind);
        tombstone_saved_session_delete_captured(agent_dir, sessions_dir, &capture)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pa-saved-del-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_child_with_usage(dir: &Path) -> PathBuf {
        let mut session = SessionFile::create("/work", None, 0);
        let path = dir.join(format!("{}.jsonl", session.session_id()));
        session.set_path(path.clone());
        session.append_message(&json!({"role": "user", "content": "hi", "timestamp": 1u64}));
        session.rewrite().unwrap();
        let usage_row = json!({
            "type": "message", "id": "m1",
            "message": {
                "role": "assistant",
                "usage": {
                    "input": 2_000, "output": 200, "cacheRead": 0, "cacheWrite": 0,
                    "totalTokens": 2_200,
                    "cost": { "input": 0.35, "output": 0.05, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.4 }
                }
            }
        });
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{usage_row}").unwrap();
        }
        path
    }

    /// The deleted-descendant bucket still bills the parent after the
    /// transcript is gone (the Rust-side instance of TS #2506's Macroscope race).
    #[test]
    fn saved_session_delete_captures_usage_before_the_unlink() {
        let root = temp_dir("capture");
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let parent = sessions_dir.join("p.jsonl");
        let child = write_child_with_usage(&sessions_dir);
        std::fs::write(&parent, "{}").unwrap();
        let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-1".to_string(),
                parent: parent.to_string_lossy().to_string(),
                child: child.to_string_lossy().to_string(),
                depth: 1,
                name: "lane".to_string(),
            })
            .unwrap();
        let captured = tombstone_saved_session_delete(
            &agent_dir,
            &sessions_dir,
            &child.to_string_lossy(),
            Some("subagent"),
        );
        assert_eq!(captured, 1, "the one edge received the snapshot");
        let edges = ledger.edges(true).unwrap();
        assert_eq!(edges.len(), 1);
        let snapshot = edges[0]
            .deleted_usage
            .clone()
            .expect("the snapshot rides the tombstone");
        assert!((snapshot.cost - 0.4).abs() < 1e-9);
        // The file goes (the trash/unlink below the tombstone): the bucket
        // survives the removal - the snapshot is the source, not the file.
        std::fs::remove_file(&child).unwrap();
        let bucket = ledger.deleted_descendant_usage_by_parent().unwrap();
        let parent_key = crate::lease::canonical_session_path(&parent)
            .to_string_lossy()
            .to_string();
        assert!(
            (bucket[&parent_key].cost - 0.4).abs() < 1e-9,
            "the spend survives the transcript's removal"
        );
    }

    #[test]
    fn top_level_deletes_never_capture() {
        let root = temp_dir("top-level");
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let top = write_child_with_usage(&sessions_dir);
        let captured = tombstone_saved_session_delete(
            &agent_dir,
            &sessions_dir,
            &top.to_string_lossy(),
            Some("top-level"),
        );
        assert_eq!(captured, 0, "nothing tombstoned, nothing captured");
        let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        assert!(
            ledger.edges(true).unwrap().is_empty(),
            "no edges, no tombstone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_delete_tombstones_the_edge_keyed_at_the_target() {
        let root = temp_dir("symlink-del");
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let (parent, child) = {
            let parent = sessions_dir.join("p.jsonl");
            let child = write_child_with_usage(&sessions_dir);
            std::fs::write(&parent, "{}").unwrap();
            (parent, child)
        };
        let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-9".into(),
                parent: parent.to_string_lossy().into(),
                child: child.to_string_lossy().into(),
                depth: 1,
                name: "w".into(),
            })
            .unwrap();
        // The delete goes through a final-component symlink: the capture
        // happens while the link exists, then phase 2 must still key the
        // edge at the target.
        let link = sessions_dir.join("link-to-child.jsonl");
        std::os::unix::fs::symlink(&child, &link).unwrap();
        let capture = capture_saved_session_delete(&link.to_string_lossy(), Some("subagent"));
        std::fs::remove_file(&link).unwrap();
        let captured = tombstone_saved_session_delete_captured(&agent_dir, &sessions_dir, &capture);
        assert_eq!(
            captured, 1,
            "the pre-unlink key finds the edge the symlink pointed at"
        );
        let edges = ledger.edges(true).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].deleted,
            Some(crate::rlm_ledger::RlmLedgerDeleteReason::User),
            "the edge is tombstoned, not left live"
        );
        assert!(
            edges[0].deleted_usage.is_some(),
            "the captured usage rode the tombstone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_regular_session_path_captures_nothing() {
        let root = temp_dir("fifo-del");
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let parent = sessions_dir.join("p.jsonl");
        std::fs::write(&parent, "{}").unwrap();
        let fifo = sessions_dir.join("fifo.jsonl");
        std::os::unix::net::UnixListener::bind(&fifo).unwrap();
        let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_spawn(&crate::rlm_ledger::RlmSpawnInput {
                child_id: "sub-9".into(),
                parent: parent.to_string_lossy().into(),
                child: fifo.to_string_lossy().into(),
                depth: 1,
                name: "w".into(),
            })
            .unwrap();
        let capture = capture_saved_session_delete(&fifo.to_string_lossy(), None);
        match &capture {
            SavedDeleteCapture::Child {
                usage,
                canonical_path,
            } => {
                assert!(
                    usage.is_none(),
                    "a non-regular path captures no usage (no blocking read)"
                );
                assert_eq!(canonical_path, &fifo.to_string_lossy());
            }
            SavedDeleteCapture::TopLevel => {
                panic!("an unreadable non-regular path is never positively top-level")
            }
        }
        let captured = tombstone_saved_session_delete_captured(&agent_dir, &sessions_dir, &capture);
        assert_eq!(captured, 0, "no usage snapshot to carry");
        let edges = ledger.edges(true).unwrap();
        assert_eq!(
            edges[0].deleted,
            Some(crate::rlm_ledger::RlmLedgerDeleteReason::User),
            "the tombstone still lands bare"
        );
    }
}
