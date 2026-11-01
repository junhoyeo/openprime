//! The daemon-owned RLM spawn ledger: one append-only JSONL file per sessions
//! dir recording spawn, rename, and delete admissions. Family topology
//! (parent/child edges, depths, names) is read back from this file instead of
//! being re-derived from session files, so historical and non-resident
//! children stay roster-visible after passivation. Mirrors the record
//! grammar, bounds, and legacy-registry seeding of the TS
//! `modes/daemon/rlm-ledger.ts`; unlike TS, replay logs and skips a
//! malformed line instead of failing the whole read.
//!
//! Writers: the supervisor appends at admission moments (spawn at child
//! create, rename at subagent rename, delete at subagent delete). Readers:
//! every roster surface that must show non-resident children (`list --all`,
//! the saved-session catalog). Appends are single small `O_APPEND` writes
//! whose atomicity we rely on for cross-process interleaving; reads re-read
//! the whole file behind a stat guard, so staleness is bounded to in-flight
//! appends.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::lease::canonical_session_path;
use crate::util::now_iso;

mod replay;
mod storage_path;

#[derive(Clone, Copy)]
enum LedgerReadPolicy {
    BestEffort,
    Authority,
}

use replay::{
    canonicalize_dir, edge_key, file_identity, is_file, parse_ledger_line, sole_edge_by_child_id,
    DeletedBucket, LedgerRecord, LivePathResolver, ReplaySnapshot, ReplayState,
};

mod legacy_registry;

pub(crate) use legacy_registry::read_legacy_registry;
#[cfg(test)]
use legacy_registry::{legacy_registry_path, LEGACY_REGISTRY_HEADER_READ_MAX_BYTES};
pub use legacy_registry::{
    read_rlm_subagent_display, write_rlm_subagent_display, LegacyRlmSubagentEntry,
    RlmSubagentDisplayEntry,
};

/// Ledger files live under `<agent-dir>/rlm-ledger/`, one per sessions dir.
pub const RLM_LEDGER_DIR: &str = "rlm-ledger";
/// Bounded read: a ledger beyond these limits fails closed loudly.
pub const RLM_LEDGER_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const RLM_LEDGER_MAX_RECORDS: usize = 100_000;

/// Why a child's edge was tombstoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RlmLedgerDeleteReason {
    User,
    ParentTeardown,
    Revoked,
    Gc,
}

impl RlmLedgerDeleteReason {
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "parent-teardown" => Some(Self::ParentTeardown),
            "revoked" => Some(Self::Revoked),
            "gc" => Some(Self::Gc),
            _ => None,
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ParentTeardown => "parent-teardown",
            Self::Revoked => "revoked",
            Self::Gc => "gc",
        }
    }
}

/// One live family edge after replay (last writer wins per childId+child).
/// Edges are replay-ordered: the append order of the ledger file.
#[derive(Debug, Clone, PartialEq)]
pub struct RlmLedgerEdge {
    pub child_id: String,
    pub parent: String,
    pub child: String,
    pub depth: u32,
    pub name: String,
    pub deleted: Option<RlmLedgerDeleteReason>,
    /// The child's captured own usage at deletion, so a tombstoned child's
    /// spend stays billable; `None` on legacy tombstones and live edges.
    pub deleted_usage: Option<crate::session_usage::SessionUsageSummary>,
}

/// Inputs for `append_spawn` (validated like a record the reader would refuse to read back).
#[derive(Debug, Clone)]
pub struct RlmSpawnInput {
    pub child_id: String,
    pub parent: String,
    pub child: String,
    pub depth: u32,
    pub name: String,
}

/// How a guarded seed failure surfaces: display degrades, authority refuses.
#[derive(Clone, Copy)]
enum SeedFailure {
    Degrade,
    Refuse,
}

pub(super) enum SeedPublication {
    Absent,
    Existing,
    Published { file: File, temp_path: PathBuf },
}

/// The per-sessions-dir spawn ledger. Reads are guarded by a file stat snapshot; a missing ledger
/// is seeded from the legacy registries (seed failure degrades to empty, never fail-closed).
pub struct RlmSpawnLedger {
    path: PathBuf,
    selected_path: OnceLock<PathBuf>,
    had_history: AtomicBool,
    configured_agent_dir: PathBuf,
    configured_sessions_dir: PathBuf,
    agent_dir: PathBuf,
    canonical_sessions_dir: PathBuf,
    session_storage_dir: PathBuf,
    cache: Mutex<Option<ReplaySnapshot>>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

/// Proposed path for a NEW ledger. This pure naming helper does not select
/// legacy history. All production I/O must use `RlmSpawnLedger`; its
/// `ledger_path()` returns the selected actual pathname after resolution.
#[must_use]
pub fn rlm_ledger_path(agent_dir: &Path, sessions_dir: &Path) -> PathBuf {
    let canonical = canonicalize_dir(sessions_dir);
    let hash = crate::paths::hash_key(&canonical.to_string_lossy(), 16);
    agent_dir.join(RLM_LEDGER_DIR).join(format!("{hash}.jsonl"))
}

/// The liveness pass shared by the live-edge reads: deleted edges never
/// resolve; the survivors' recorded endpoints do - a dead endpoint drops
/// the edge, a moved path resolves through its durable session id - and
/// the resolved paths replace the recorded ones.
fn resolve_live_edges(
    state: &ReplayState,
    resolver: &mut LivePathResolver,
) -> Result<Vec<RlmLedgerEdge>> {
    let mut edges = Vec::with_capacity(state.edges.len());
    for edge in &state.edges {
        if edge.deleted.is_some() {
            continue;
        }
        let (Some(child), Some(parent)) = (
            resolver.resolve(&edge.child)?,
            resolver.resolve(&edge.parent)?,
        ) else {
            continue;
        };
        edges.push(RlmLedgerEdge {
            parent: parent.to_string_lossy().to_string(),
            child: child.to_string_lossy().to_string(),
            ..edge.clone()
        });
    }
    Ok(edges)
}

/// Fold parsed records into the replayed edge set, keeping only the
/// records whose child id `keep` selects.
fn fold_records(records: Vec<LedgerRecord>, keep: impl Fn(&str) -> bool) -> ReplayState {
    let mut state = ReplayState::default();
    for record in records {
        match record {
            LedgerRecord::Spawn {
                child_id,
                parent,
                child,
                depth,
                name,
            } if keep(&child_id) => {
                let key = edge_key(&child_id, &child);
                if let Some(at) = state.index.get(&key).copied() {
                    // A late admission/re-registration cannot undo deletion.
                    // A new child has a new durable transcript identity.
                    if state.edges[at].deleted.is_some() {
                        continue;
                    }
                    state.edges[at] = RlmLedgerEdge {
                        child_id,
                        parent,
                        child,
                        depth,
                        name,
                        deleted: None,
                        deleted_usage: None,
                    };
                } else {
                    state.index.insert(key.clone(), state.edges.len());
                    state.edges.push(RlmLedgerEdge {
                        child_id,
                        parent,
                        child,
                        depth,
                        name,
                        deleted: None,
                        deleted_usage: None,
                    });
                }
            }
            LedgerRecord::Rename {
                child_id,
                child,
                name,
            } if keep(&child_id) => {
                let key = edge_key(&child_id, &child);
                if let Some(&at) = state.index.get(&key) {
                    state.edges[at].name = name;
                } else if let Some(at) = sole_edge_by_child_id(&state, &child_id) {
                    state.edges[at].name = name;
                }
            }
            LedgerRecord::Delete {
                child_id,
                child,
                reason,
                usage,
            } if keep(&child_id) => {
                let key = edge_key(&child_id, &child);
                let at = match state.index.get(&key).copied() {
                    Some(at) => Some(at),
                    None => sole_edge_by_child_id(&state, &child_id),
                };
                if let Some(at) = at {
                    state.edges[at].deleted = Some(reason);
                    // The snapshot is sticky: a re-tombstone without a usage
                    // block never clears it; a fresh capture replaces it.
                    if let Some(usage) = usage {
                        state.edges[at].deleted_usage = Some(usage);
                    }
                }
            }
            _ => {}
        }
    }
    state
}

impl RlmSpawnLedger {
    /// Ledger over one sessions dir, with a caller-supplied log sink for
    /// degraded reads and seed skips.
    pub fn new(
        agent_dir: &Path,
        sessions_dir: &Path,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        // Storage is a filesystem target; the sessions-dir namespace keeps
        // shipping lexical resolution BEFORE stable ancestor canonicalization.
        let configured_agent_dir =
            std::path::absolute(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
        let configured_sessions_dir =
            std::path::absolute(sessions_dir).unwrap_or_else(|_| sessions_dir.to_path_buf());
        let agent_dir = canonical_session_path(&configured_agent_dir);
        let canonical_sessions_dir = replay::checked_scope(&configured_sessions_dir)
            .unwrap_or_else(|_| canonicalize_dir(&configured_sessions_dir));
        let session_storage_dir = canonical_session_path(&configured_sessions_dir);
        Self {
            path: rlm_ledger_path(&agent_dir, &canonical_sessions_dir),
            selected_path: OnceLock::new(),
            had_history: AtomicBool::new(false),
            configured_agent_dir,
            configured_sessions_dir,
            agent_dir,
            canonical_sessions_dir,
            session_storage_dir,
            cache: Mutex::new(None),
            log: Box::new(log),
        }
    }

    /// The selected actual filename after resolution; before that, only the
    /// proposed new path. Construction may read canonical paths, but takes no
    /// namespace lock and writes no files. It never caches a transient busy error.
    pub fn ledger_path(&self) -> &Path {
        self.selected_path.get().unwrap_or(&self.path)
    }

    fn storage_guard(&self) -> Result<storage_path::StorageGuard> {
        let agent = crate::lease::checked_session_path(&self.configured_agent_dir)?;
        let scope = replay::checked_scope(&self.configured_sessions_dir)?;
        let storage = crate::lease::checked_session_path(&self.configured_sessions_dir)?;
        if agent != self.agent_dir
            || scope != self.canonical_sessions_dir
            || storage != self.session_storage_dir
        {
            bail!("RLM ledger authority paths changed; reopen with a validated authority context");
        }
        let guard = storage_path::StorageGuard::acquire(&agent, &scope)?;
        for recovery in &guard.recovered {
            match recovery {
                storage_path::SeedScratchRecovery::Retained { from, to } => self.log(&format!(
                    "RLM ledger: retained dead seed scratch {} as {}",
                    from.display(),
                    to.display()
                )),
                storage_path::SeedScratchRecovery::RetiredPublishedName { name } => {
                    self.log(&format!(
                        "RLM ledger: retired extra published seed name {}",
                        name.display()
                    ));
                }
            }
        }
        self.pin_selection(&guard)?;
        Ok(guard)
    }

    fn pin_selection(&self, guard: &storage_path::StorageGuard) -> Result<()> {
        if guard.has_file() {
            self.had_history.store(true, Ordering::SeqCst);
        } else if self.had_history.load(Ordering::SeqCst) {
            bail!("known RLM ledger disappeared; refusing empty authority");
        }
        let selected = guard.path();
        let pinned = self.selected_path.get_or_init(|| selected.to_path_buf());
        if pinned != selected {
            bail!("RLM ledger selection changed; refusing a second history");
        }
        Ok(())
    }

    fn refresh_storage(&self, guard: &mut storage_path::StorageGuard) -> Result<()> {
        guard.refresh(&self.agent_dir, &self.canonical_sessions_dir)?;
        self.pin_selection(guard)
    }

    fn log(&self, message: &str) {
        (self.log)(message);
    }

    /// Record a spawn admission. The child session path must be unique among
    /// live edges (a per-process advisory check).
    ///
    /// # Errors
    ///
    /// Returns an error when the spawn input is invalid (an empty child
    /// id, parent, or child session path, or a zero depth), when another
    /// live edge already claims the child session path, when the ledger
    /// replay fails (an oversized ledger), or when the record cannot
    /// be appended.
    pub fn append_spawn(&self, input: &RlmSpawnInput) -> Result<()> {
        if input.child_id.is_empty()
            || input.parent.is_empty()
            || input.child.is_empty()
            || input.depth < 1
        {
            bail!(
                "RLM ledger: invalid spawn for {} (depth {})",
                if input.child_id.is_empty() {
                    "<missing childId>"
                } else {
                    &input.child_id
                },
                input.depth
            );
        }
        let child_path = canonical_session_path(Path::new(&input.child));
        let child_path_text = child_path.to_string_lossy().to_string();
        let mut guard = self.storage_guard()?;
        self.seed_once_guarded(&mut guard)?;
        // Only an Ok append proves a record committed: append_record_guarded
        // writes one whole line and syncs before returning Ok, and callers
        // act only after Ok. A torn line was never acknowledged, so it never
        // committed a tombstone; it is skipped and logged by file and line.
        let state = self.replay_cached_guarded(&guard)?;
        for edge in &state.edges {
            let edge_child = canonical_session_path(Path::new(&edge.child));
            // #42 forbids the same durable child identity at the same path.
            // A different child id may reuse a path after a tombstone.
            if edge.deleted.is_some() && edge_child == child_path && edge.child_id == input.child_id
            {
                bail!("Cannot revive deleted RLM child: {child_path_text}");
            }
            if edge.deleted.is_none() && edge_child == child_path && edge.child_id != input.child_id
            {
                bail!(
                    "RLM ledger: duplicate child session path {child_path_text} (already {})",
                    edge.child_id
                );
            }
        }
        self.append_record_guarded(
            &mut guard,
            &json!({
                "v": 1,
                "op": "spawn",
                "at": now_iso(),
                "childId": input.child_id,
                "parent": canonical_session_path(Path::new(&input.parent)).to_string_lossy(),
                "child": child_path_text,
                "depth": input.depth,
                "name": input.name,
            }),
        )
    }

    /// Record a rename for a known child edge.
    ///
    /// # Errors
    ///
    /// Returns an error when the rename record cannot be appended.
    pub fn append_rename(&self, child_id: &str, child: &str, name: &str) -> Result<()> {
        let child_path = canonical_session_path(Path::new(child));
        self.append_record(&json!({
            "v": 1,
            "op": "rename",
            "at": now_iso(),
            "childId": child_id,
            "child": child_path.to_string_lossy(),
            "name": name,
        }))
    }

    /// Rename by child session path alone (an offline rename knows no
    /// childId): one rename record for every live edge at that path.
    ///
    /// # Errors
    ///
    /// Returns an error when the replay fails (an oversized ledger)
    /// or one of the rename records cannot be appended.
    pub fn append_rename_by_child_path(&self, child: &str, name: &str) -> Result<()> {
        let target = canonical_session_path(Path::new(child));
        let state = self.replay_cached()?;
        for edge in &state.edges {
            if edge.deleted.is_none() && canonical_session_path(Path::new(&edge.child)) == target {
                self.append_record(&json!({
                    "v": 1,
                    "op": "rename",
                    "at": now_iso(),
                    "childId": edge.child_id,
                    "child": target.to_string_lossy(),
                    "name": name,
                }))?;
            }
        }
        Ok(())
    }

    /// Tombstone a child's edge.
    ///
    /// # Errors
    ///
    /// Returns an error when the tombstone record cannot be appended.
    pub fn append_delete(
        &self,
        child_id: &str,
        child: &str,
        reason: RlmLedgerDeleteReason,
    ) -> Result<()> {
        let child_path = canonical_session_path(Path::new(child));
        self.append_record(&json!({
            "v": 1,
            "op": "delete",
            "at": now_iso(),
            "childId": child_id,
            "child": child_path.to_string_lossy(),
            "reason": reason.wire_name(),
        }))
    }

    /// The deletion's durable usage amendment (deliberate TS divergence: TS re-reads the
    /// tombstoned transcript, which its delete removes): the captured usage rides a second record.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be serialized or the tombstone cannot be appended.
    pub fn append_delete_with_usage(
        &self,
        child_id: &str,
        child: &str,
        reason: RlmLedgerDeleteReason,
        usage: &crate::session_usage::SessionUsageSummary,
    ) -> Result<()> {
        let child_path = canonical_session_path(Path::new(child));
        // The writer never records what the reader refuses: replay
        // rejects a negative or NaN usage cost outright, which would
        // drop the whole tombstone record. That includes negative
        // zero - `is_sign_negative()` is how replay reads it, and
        // `>= 0.0` alone would have passed `-0.0` through. A session
        // file may carry such a cost (the file's own summary read
        // preserves it), so a snapshot the reader would reject rides as
        // absent - the tombstone still lands bare (the historical-gap
        // zero).
        if !usage.cost.is_finite() || usage.cost.is_sign_negative() {
            return self.append_delete(child_id, child, reason);
        }
        let usage = serde_json::to_value(usage)
            .with_context(|| "serialize the deleted child usage snapshot")?;
        self.append_record(&json!({
            "v": 1,
            "op": "delete",
            "at": now_iso(),
            "childId": child_id,
            "child": child_path.to_string_lossy(),
            "reason": reason.wire_name(),
            "usage": usage,
        }))
    }

    /// Tombstone every edge for one child session path (duplicate edges from raced or corrupt
    /// appends; a live one would resurrect a later recreation at that path as a subagent).
    ///
    /// # Errors
    ///
    /// Returns an error when the replay fails or one of the tombstone records cannot be appended.
    pub fn tombstone_child_path(
        &self,
        child: &str,
        reason: RlmLedgerDeleteReason,
    ) -> Result<Vec<RlmLedgerEdge>> {
        self.tombstone_child_path_with_usage(child, reason, None)
    }

    /// The saved-session delete's tombstone with the captured usage snapshot (the file dies right
    /// after this, so the bucket's lazy file fallback has nothing to read).
    ///
    /// # Errors
    ///
    /// Returns an error when the replay fails or a per-edge tombstone cannot be appended.
    pub fn tombstone_child_path_with_usage(
        &self,
        child: &str,
        reason: RlmLedgerDeleteReason,
        usage: Option<&crate::session_usage::SessionUsageSummary>,
    ) -> Result<Vec<RlmLedgerEdge>> {
        let target = canonical_session_path(Path::new(child));
        let state = self.replay_cached()?;
        let matching: Vec<RlmLedgerEdge> = state
            .edges
            .iter()
            .filter(|edge| canonical_session_path(Path::new(&edge.child)) == target)
            .cloned()
            .collect();
        for edge in &matching {
            match usage {
                Some(usage) => {
                    self.append_delete_with_usage(&edge.child_id, &edge.child, reason, usage)?;
                }
                None => self.append_delete(&edge.child_id, &edge.child, reason)?,
            }
        }
        Ok(matching)
    }

    /// Recursive spend of tombstoned descendants keyed by the parent's
    /// canonical session path (TS `deletedDescendantUsageByParent`, the
    /// agents-view cost rollup's bucket): a deleted subagent keeps no row
    /// anywhere, so nothing re-adds its spend once the tombstone drops its
    /// row - this folds the captured spend back per family so cost rollups
    /// bill it to the parent that spent it. Live paths never contribute
    /// (their own rows carry their spend); a tombstoned path that still
    /// has a catalog row bills through that row instead (see the skip
    /// below); the first tombstoned edge claims a path (a raced ledger
    /// must not bill one child to two parents); the fold is an iterative
    /// post-order walk (a pathological chain must not overflow the
    /// stack) that reads each tombstoned child's captured own usage
    /// first and falls back to the transcript's own-usage fold (the
    /// resumable `read_session_info` scan) for legacy tombstones that
    /// predate the capture - a path with neither a snapshot nor a
    /// readable transcript is the documented historical gap (no
    /// fabricated backfill: zero).
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger replay fails (an oversized
    /// ledger) or a prior panic poisoned the deleted-usage cache.
    pub fn deleted_descendant_usage_by_parent(
        &self,
    ) -> Result<HashMap<String, crate::session_usage::SessionUsageSummary>> {
        self.seed_once()?;
        let state = self.replay_cached()?;
        let inputs = state
            .edges
            .iter()
            .filter(|edge| edge.deleted.is_some())
            .map(|edge| file_identity(Path::new(&edge.child)))
            .collect::<Result<Vec<_>>>()?;
        let mut cached = state
            .deleted_bucket
            .lock()
            .map_err(|_| anyhow::anyhow!("deleted bucket lock poisoned"))?;
        if let Some(bucket) = cached.as_ref().filter(|bucket| bucket.inputs == inputs) {
            return Ok(bucket.usage.clone());
        }
        let usage = self.fold_deleted_descendant_usage(&state.edges);
        *cached = Some(DeletedBucket {
            inputs,
            usage: usage.clone(),
        });
        Ok(usage)
    }

    fn fold_deleted_descendant_usage(
        &self,
        edges: &[RlmLedgerEdge],
    ) -> HashMap<String, crate::session_usage::SessionUsageSummary> {
        use crate::session_usage::SessionUsageSummary;
        let canonical = |path: &str| {
            canonical_session_path(Path::new(path))
                .to_string_lossy()
                .to_string()
        };
        let mut live_paths: HashSet<String> = HashSet::new();
        for edge in edges {
            if edge.deleted.is_none() {
                live_paths.insert(canonical(&edge.child));
            }
        }
        // First writer wins per path: the earliest tombstoned edge claims
        // the child (and its snapshot) for its parent.
        let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
        let mut snapshot_by_path: HashMap<String, Option<SessionUsageSummary>> = HashMap::new();
        for edge in edges {
            if edge.deleted.is_none() {
                continue;
            }
            let child = canonical(&edge.child);
            if live_paths.contains(&child) || snapshot_by_path.contains_key(&child) {
                continue;
            }
            // A tombstoned path that still has a catalog row bills
            // through that row instead (the rollup sums the child row
            // AND the parent bucket, so billing both would double the
            // spend): the flat catalog scans this ledger's sessions dir,
            // so the row exists exactly while the file sits directly in
            // it. Real RLM children persist under session-artifacts,
            // where the flat catalog never lists them - the RLM delete
            // keeps that transcript and no row anywhere bills it, so the
            // bucket is the only surface that keeps its spend.
            let child_file = Path::new(&child);
            if child_file.is_file()
                && child_file.parent() == Some(self.canonical_sessions_dir.as_path())
            {
                continue;
            }
            let parent = canonical(&edge.parent);
            snapshot_by_path.insert(child.clone(), edge.deleted_usage.clone());
            children_by_parent.entry(parent).or_default().push(child);
        }
        let zero = || SessionUsageSummary {
            input_tokens: 0,
            output_tokens: 0,
            cost: 0.0,
        };
        let add = |mut left: SessionUsageSummary, right: SessionUsageSummary| {
            // Saturating: the bucket feeds billable rollups — a huge
            // snapshot must not panic or wrap to an underbill in release.
            left.input_tokens = left.input_tokens.saturating_add(right.input_tokens);
            left.output_tokens = left.output_tokens.saturating_add(right.output_tokens);
            left.cost += right.cost;
            left
        };
        let tombstone_usage = |path: &str| -> SessionUsageSummary {
            match snapshot_by_path.get(path).cloned().flatten() {
                Some(snapshot) => snapshot,
                // The legacy fallback is the same own-usage fold, read
                // through the resumable scan: a repeat bucket fold on a
                // surviving legacy transcript is one stat per child.
                None => crate::session_store::read_session_info(Path::new(path))
                    .and_then(|info| info.usage)
                    .unwrap_or_else(zero),
            }
        };
        // Iterative post-order fold with per-path memoization.
        let mut contribution: HashMap<String, SessionUsageSummary> = HashMap::new();
        let mut on_stack: HashSet<String> = HashSet::new();
        for children in children_by_parent.values() {
            for root in children {
                if contribution.contains_key(root) {
                    continue;
                }
                let mut stack = vec![root.clone()];
                on_stack.insert(root.clone());
                while let Some(current) = stack.last().cloned() {
                    if contribution.contains_key(&current) {
                        stack.pop();
                        on_stack.remove(&current);
                        continue;
                    }
                    let pending: Vec<String> = children_by_parent
                        .get(&current)
                        .map(|children| {
                            children
                                .iter()
                                .filter(|path| {
                                    !contribution.contains_key(*path) && !on_stack.contains(*path)
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    if !pending.is_empty() {
                        for path in pending {
                            on_stack.insert(path.clone());
                            stack.push(path);
                        }
                        continue;
                    }
                    stack.pop();
                    on_stack.remove(&current);
                    let mut total = tombstone_usage(&current);
                    for descendant in children_by_parent
                        .get(&current)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                    {
                        if let Some(folded) = contribution.get(descendant) {
                            total = add(total, folded.clone());
                        }
                    }
                    contribution.insert(current, total);
                }
            }
        }
        let mut usage_by_parent: HashMap<String, SessionUsageSummary> = HashMap::new();
        for (parent, children) in &children_by_parent {
            let mut total = zero();
            for child in children {
                if let Some(folded) = contribution.get(child) {
                    total = add(total, folded.clone());
                }
            }
            if total.input_tokens > 0 || total.output_tokens > 0 || total.cost > 0.0 {
                usage_by_parent.insert(parent.clone(), total);
            }
        }
        usage_by_parent
    }

    /// Replay edges without liveness reconciliation. Deleted edges are
    /// filtered by default; tombstones carry their delete reason.
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger replay fails (an oversized
    /// ledger); a missing ledger replays empty.
    pub fn edges(&self, include_deleted: bool) -> Result<Vec<RlmLedgerEdge>> {
        self.seed_once()?;
        let state = self.replay_cached()?;
        Ok(state
            .edges
            .iter()
            .filter(|edge| include_deleted || edge.deleted.is_none())
            .cloned()
            .collect())
    }

    /// Authority reads must not interpret unreadable or partially understood
    /// topology as an empty tree. Keep ordinary display reads best-effort; this
    /// uncached path propagates seeding, I/O, malformed and unknown-op errors.
    pub(crate) fn strict_edges(&self) -> Result<Vec<RlmLedgerEdge>> {
        let mut guard = self.storage_guard()?;
        guard.require_authority()?;
        self.seed_once_guarded(&mut guard)?;
        let state = fold_records(
            self.read_records_with(&guard, LedgerReadPolicy::Authority)?,
            |_| true,
        );
        Ok(state.edges)
    }

    /// Live edges reconciled by liveness of their recorded endpoints: a dead endpoint drops the
    /// edge; a MOVED path resolves through its durable session id first.
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger replay fails (an oversized
    /// ledger); a missing ledger replays empty, and only the liveness
    /// resolution drops edges.
    pub fn live_edges(&self) -> Result<Vec<RlmLedgerEdge>> {
        self.seed_once()?;
        let state = self.replay_cached()?;
        let mut resolver = LivePathResolver::new(
            self.agent_dir.clone(),
            self.session_storage_dir.clone(),
            &state.artifact_index,
        );
        resolve_live_edges(&state, &mut resolver)
    }

    /// The opened parent file's live edges: only the records of children
    /// spawned by that parent file (matched by its durable session id,
    /// the file-name stem) fold and resolve, and only those whose
    /// resolved parent IS the parent file remain. A parent's read never
    /// stats, or walks the artifacts tree for, another parent's edges.
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger replay fails (an oversized
    /// ledger); a missing ledger replays empty, and only the liveness
    /// resolution drops edges.
    pub fn live_edges_of_parent(&self, parent_file: &Path) -> Result<Vec<RlmLedgerEdge>> {
        self.seed_once()?;
        let parent_file = canonical_session_path(parent_file);
        let state = self.replay_of_parent(&parent_file)?;
        let mut resolver = LivePathResolver::new(
            self.agent_dir.clone(),
            self.session_storage_dir.clone(),
            &state.artifact_index,
        );
        let mut edges = resolve_live_edges(&state, &mut resolver)?;
        edges.retain(|edge| canonical_session_path(Path::new(&edge.parent)) == parent_file);
        Ok(edges)
    }

    /// Whether the given spawn edge is still live (not tombstoned, and
    /// both its child and parent transcripts present - the same
    /// reconciliation `live_edges` applies) - the seed arms' per-write
    /// liveness revalidation: an edge deleted (or a file removed) while
    /// a seed was mid-read never writes its row. The child id is
    /// matched together with the child path: ids can be shared by
    /// edges with different paths, and only the exact edge a seed
    /// snapshotted counts as live. An unreadable (oversized) ledger reads
    /// as not-live. `false` means "not proven live", never "proven deleted":
    /// it is display-only (roster seed revalidation) and must not authorize
    /// admission or deletion. Authority callers use `strict_edges`.
    pub fn edge_is_live(&self, child_id: &str, child: &str) -> bool {
        let canonical_child = canonical_session_path(Path::new(child));
        self.seed_once().is_ok()
            && self.replay_cached().is_ok_and(|state| {
                let is_live = |edge: &RlmLedgerEdge| {
                    edge.child_id == child_id
                        && edge.deleted.is_none()
                        && canonical_session_path(Path::new(&edge.child)) == canonical_child
                        && is_file(Path::new(&edge.child))
                        && is_file(Path::new(&edge.parent))
                };
                // Most reads prove liveness through the index. Its keys
                // were canonicalized at replay, though: a file replaced
                // by a symlink does not change the ledger's stat identity.
                // A stale key or tombstoned alias cannot prove absence;
                // preserve the original read-time path comparison then.
                state
                    .index
                    .get(&edge_key(child_id, child))
                    .is_some_and(|&at| is_live(&state.edges[at]))
                    || state.edges.iter().any(is_live)
            })
    }

    /// Display reads: a failed legacy seed degrades to no seeded edges and is
    /// logged. It is not memoized; the next read seeds again after a repair.
    fn seed_once(&self) -> Result<()> {
        let mut guard = self.storage_guard()?;
        self.seed_once_with(&mut guard, SeedFailure::Degrade)
    }

    /// Authority, admission and append: a failed legacy seed refuses.
    fn seed_once_guarded(&self, guard: &mut storage_path::StorageGuard) -> Result<()> {
        self.seed_once_with(guard, SeedFailure::Refuse)
    }

    fn seed_once_with(
        &self,
        guard: &mut storage_path::StorageGuard,
        failure: SeedFailure,
    ) -> Result<()> {
        if guard.has_file() {
            return Ok(());
        }
        // No selected history plus a mixed-history blocker: never publish a
        // seed. Display then reads this namespace as empty without an error;
        // authority, admission and append still refuse via require_authority.
        if guard.authority_blocked() {
            return Ok(());
        }
        // Empty/missing and failed seeds are never memoized: new or repaired
        // legacy inputs may appear later.
        let publication = match (self.seed(), failure) {
            (Ok(publication), _) => publication,
            (Err(error), SeedFailure::Refuse) => return Err(error),
            (Err(error), SeedFailure::Degrade) => {
                self.log(&format!("RLM ledger seeding failed: {error:#}"));
                return Ok(());
            }
        };
        match publication {
            SeedPublication::Absent => {
                self.refresh_storage(guard)?;
                if guard.has_file() {
                    bail!("RLM ledger appeared without this operation's publication receipt");
                }
            }
            SeedPublication::Existing => {
                bail!("RLM ledger appeared during guarded seed; refusing unknown publication")
            }
            SeedPublication::Published { file, temp_path } => {
                guard.refresh_with_seed(
                    &self.agent_dir,
                    &self.canonical_sessions_dir,
                    Some(&temp_path),
                )?;
                self.pin_selection(guard)?;
                guard.retire_published_seed(&file, &temp_path)?;
                self.refresh_storage(guard)?;
            }
        }
        Ok(())
    }

    /// Replay behind the stat guard: a file whose identity snapshot is
    /// unchanged reuses the cached edges. A missing file replays empty.
    fn replay_cached(&self) -> Result<Arc<ReplayState>> {
        let guard = self.storage_guard()?;
        self.replay_cached_guarded(&guard)
    }

    fn replay_cached_guarded(
        &self,
        guard: &storage_path::StorageGuard,
    ) -> Result<Arc<ReplayState>> {
        let identity = guard.file().map(replay::opened_file_stat).transpose()?;
        let mut cache = self.cache.lock().expect("ledger cache lock");
        if let (Some(identity), Some(cached)) = (&identity, cache.as_ref()) {
            if *identity == cached.identity
                && storage_path::same_open_file(
                    &cached.file,
                    guard.file().expect("stat implies retained file"),
                )?
            {
                return Ok(Arc::clone(&cached.state));
            }
        }
        let state = Arc::new(self.replay(guard)?);
        if let Some(identity) = identity {
            *cache = Some(ReplaySnapshot {
                identity,
                file: guard
                    .file()
                    .expect("stat implies retained file")
                    .try_clone()?,
                state: Arc::clone(&state),
            });
        }
        Ok(state)
    }

    fn replay(&self, guard: &storage_path::StorageGuard) -> Result<ReplayState> {
        Ok(fold_records(self.read_records(guard)?, |_| true))
    }

    /// The opened parent file's subset of the replayed ledger: fold only
    /// the records of children spawned by `parent_file` (matched by its
    /// durable session id, the file-name stem). Every join, including
    /// the sole-child-id fallback, stays within one child id, so the
    /// subset fold equals the full fold restricted to that parent's
    /// children.
    fn replay_of_parent(&self, parent_file: &Path) -> Result<ReplayState> {
        let guard = self.storage_guard()?;
        let records = self.read_records(&guard)?;
        let own: HashSet<String> = records
            .iter()
            .filter_map(|record| match record {
                LedgerRecord::Spawn {
                    child_id, parent, ..
                } if Path::new(parent).file_stem() == parent_file.file_stem() => {
                    Some(child_id.clone())
                }
                _ => None,
            })
            .collect();
        Ok(fold_records(records, |child_id| own.contains(child_id)))
    }

    /// The whole ledger's parsed records within its read bounds: the
    /// byte and record-count caps apply to the whole file; a missing
    /// ledger reads empty, and one torn or unknown line skips that
    /// record only.
    fn read_records(&self, guard: &storage_path::StorageGuard) -> Result<Vec<LedgerRecord>> {
        self.read_records_with(guard, LedgerReadPolicy::BestEffort)
    }

    fn read_records_with(
        &self,
        guard: &storage_path::StorageGuard,
        policy: LedgerReadPolicy,
    ) -> Result<Vec<LedgerRecord>> {
        let Some(bytes) = guard.read_bytes(RLM_LEDGER_MAX_BYTES)? else {
            return Ok(Vec::new());
        };
        if bytes.len() as u64 > RLM_LEDGER_MAX_BYTES {
            bail!(
                "RLM ledger {} exceeds {RLM_LEDGER_MAX_BYTES} bytes; refusing to read",
                self.ledger_path().display()
            );
        }
        if matches!(policy, LedgerReadPolicy::Authority) {
            return self.read_authority_records(&bytes);
        }
        // A torn write inside a multibyte name must spoil one line, not
        // the whole file for ordinary display reads. Like the authority
        // reader, an unterminated final segment is an unacknowledged record.
        let terminated = bytes.is_empty() || bytes.ends_with(b"\n");
        let content = String::from_utf8_lossy(&bytes);
        let mut records = Vec::new();
        let mut lines = 0usize;
        let mut segments = content.lines().enumerate().peekable();
        while let Some((index, line)) = segments.next() {
            if line.trim().is_empty() {
                continue;
            }
            lines += 1;
            if lines > RLM_LEDGER_MAX_RECORDS {
                bail!(
                    "RLM ledger {} exceeds {RLM_LEDGER_MAX_RECORDS} records; refusing to read",
                    self.ledger_path().display()
                );
            }
            if !terminated && segments.peek().is_none() {
                self.log(&format!(
                    "RLM ledger {}: skipped torn record on line {}",
                    self.ledger_path().display(),
                    index + 1
                ));
                continue;
            }
            match parse_ledger_line(line, index) {
                Ok(Some(record)) => records.push(record),
                Ok(None) => {
                    self.log(&format!(
                        "RLM ledger {}: skipped record with unknown op on line {}",
                        self.ledger_path().display(),
                        index + 1
                    ));
                }
                Err(error) => {
                    // File name and line number only: record content is never logged.
                    let _ = error;
                    self.log(&format!(
                        "RLM ledger {}: skipped malformed or torn record on line {}",
                        self.ledger_path().display(),
                        index + 1
                    ));
                }
            }
        }
        Ok(records)
    }

    /// Authority reads, per `replay::classify_authority_line`: torn,
    /// non-UTF-8, non-JSON and absent-field lines never committed and are
    /// skipped and logged by file and line number, never content.
    /// Present-but-unknown or wrongly typed fields, unknown op or version,
    /// and duplicate keys refuse. The header, size caps and I/O stay fatal.
    /// This never decodes the whole file as UTF-8 (that would let a single
    /// bad-UTF-8 line poison byte-identical neighboring lines via lossy
    /// replacement, or hard-fail everything): it splits raw bytes and
    /// decodes per segment instead.
    fn read_authority_records(&self, bytes: &[u8]) -> Result<Vec<LedgerRecord>> {
        let path = self.ledger_path().display().to_string();
        let mut lines = bytes.split(|byte| *byte == b'\n').enumerate().peekable();
        let terminated = bytes.is_empty() || bytes.ends_with(b"\n");
        let header = loop {
            let Some((_, line)) = lines.next() else {
                bail!("authority ledger {path} has no ownership header");
            };
            if !line.iter().all(u8::is_ascii_whitespace) {
                break line;
            }
        };
        let header_str = std::str::from_utf8(header).map_err(|_| {
            anyhow::anyhow!("authority ledger {path} has a malformed ownership header")
        })?;
        replay::validate_unique_json(header_str).map_err(|_| {
            anyhow::anyhow!("authority ledger {path} has an ambiguous ownership header")
        })?;
        let header: Value = serde_json::from_str(header_str).map_err(|_| {
            anyhow::anyhow!("authority ledger {path} has a malformed ownership header")
        })?;
        let scope = header
            .get("sessionsDir")
            .and_then(Value::as_str)
            .filter(|scope| Path::new(scope).is_absolute());
        if header.get("op").and_then(Value::as_str) != Some("meta")
            || !scope.is_some_and(|scope| {
                canonicalize_dir(Path::new(scope)) == self.canonical_sessions_dir
            })
        {
            bail!("authority ledger {path} ownership does not match its sessions directory");
        }
        let mut records = Vec::new();
        let mut count = 1usize;
        while let Some((index, raw)) = lines.next() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            count += 1;
            if count > RLM_LEDGER_MAX_RECORDS {
                bail!(
                    "RLM ledger {path} exceeds {RLM_LEDGER_MAX_RECORDS} records; refusing to read"
                );
            }
            // The last segment without a trailing newline is a torn tail.
            let torn = !terminated && lines.peek().is_none();
            // One message carries both the file and the refusal reason: a
            // plain Display of an anyhow context would otherwise hide the
            // reason (the outer context alone, not the wrapped chain).
            let classified = match replay::classify_authority_line(raw, index, torn) {
                Ok(classified) => classified,
                Err(error) => bail!("RLM ledger {path}: {error:#}"),
            };
            match classified {
                replay::AuthorityLine::Record(record) => records.push(record),
                replay::AuthorityLine::Meta => {}
                replay::AuthorityLine::Skip(reason) => self.log(&format!(
                    "RLM ledger {path}: skipped {reason} record on line {}",
                    index + 1
                )),
            }
        }
        Ok(records)
    }

    /// One durable append; the first record in a fresh file is the meta
    /// header (the same line `seed` publishes).
    fn append_record(&self, record: &Value) -> Result<()> {
        let mut guard = self.storage_guard()?;
        self.seed_once_guarded(&mut guard)?;
        self.append_record_guarded(&mut guard, record)
    }

    fn append_record_guarded(
        &self,
        guard: &mut storage_path::StorageGuard,
        record: &Value,
    ) -> Result<()> {
        guard.require_authority()?;
        if let Some(parent) = self.ledger_path().parent() {
            crate::paths::ensure_dir(parent)?;
        }
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        if !guard.has_file() {
            // A new ledger is published whole: header plus this first record
            // through the seed scratch and no-clobber link, never a bare
            // header that a crash could leave torn.
            return match self.publish_new_ledger(&line)? {
                SeedPublication::Published { file, temp_path } => {
                    guard.refresh_with_seed(
                        &self.agent_dir,
                        &self.canonical_sessions_dir,
                        Some(&temp_path),
                    )?;
                    self.pin_selection(guard)?;
                    guard.retire_published_seed(&file, &temp_path)?;
                    self.finish_append();
                    Ok(())
                }
                SeedPublication::Existing => {
                    bail!("RLM ledger appeared during append; refusing unknown publication")
                }
                SeedPublication::Absent => unreachable!("publication always carries the record"),
            };
        }
        let mut file = guard
            .open_append()
            .with_context(|| format!("open RLM ledger {}", self.ledger_path().display()))?;
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                self.retain_and_truncate_torn_tail(&mut file, length)?;
            }
        }
        #[cfg(test)]
        if let Some(prefix) = APPEND_WRITE_FAULT.with(std::cell::Cell::take) {
            file.write_all(&line.as_bytes()[..prefix.min(line.len())])?;
            bail!("injected ledger append fault after {prefix} bytes");
        }
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        self.finish_append();
        Ok(())
    }

    fn finish_append(&self) {
        self.had_history.store(true, Ordering::SeqCst);
        // Our own writes must not be served stale from the stat guard.
        self.cache.lock().expect("ledger cache lock").take();
    }

    /// A tail after the last newline is an unacknowledged, torn record. Like
    /// the final TS writer (event-log.ts repairTailSync), truncate it before
    /// appending, so every reader sees only complete lines. Its bytes are first
    /// copied and synced into the private retained directory; if that copy
    /// fails, the append refuses and nothing is truncated. Complete lines are
    /// never truncated.
    fn retain_and_truncate_torn_tail(&self, file: &mut File, length: u64) -> Result<()> {
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        reader
            .take(RLM_LEDGER_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 == length,
            "RLM ledger {} changed while repairing a torn tail",
            self.ledger_path().display()
        );
        let keep = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        let retained =
            storage_path::retain_torn_tail(&self.agent_dir, self.ledger_path(), &bytes[keep..])
                .with_context(|| {
                    format!(
                        "RLM ledger {}: torn tail not retained; append refused",
                        self.ledger_path().display()
                    )
                })?;
        // The append handle cannot truncate; reopen the same selected object.
        let writer = OpenOptions::new().write(true).open(self.ledger_path())?;
        anyhow::ensure!(
            storage_path::same_open_file(file, &writer)?,
            "RLM ledger {} changed before truncating its torn tail",
            self.ledger_path().display()
        );
        writer.set_len(keep as u64)?;
        writer.sync_all()?;
        self.log(&format!(
            "RLM ledger {}: retained torn tail as {} and truncated it before appending",
            self.ledger_path().display(),
            retained.display()
        ));
        Ok(())
    }

    /// Seed a missing ledger from the legacy per-parent registries, then publish atomically (an
    /// interrupted seed leaves nothing to mis-read; a concurrent append wins).
    fn seed(&self) -> Result<SeedPublication> {
        match fs::symlink_metadata(self.ledger_path()) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "existing ledger is not a regular authority file"
                );
                return Ok(SeedPublication::Existing);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect ledger before legacy seed"),
        }
        let root_entries = match fs::read_dir(&self.configured_sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SeedPublication::Absent)
            }
            Err(error) => return Err(error).context("read legacy seed roots"),
        };
        let mut queue = Vec::new();
        for (index, entry) in root_entries.enumerate() {
            anyhow::ensure!(
                index < RLM_LEDGER_MAX_RECORDS,
                "legacy seed exceeds root enumeration limit"
            );
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                queue.push((path, 0u32));
            }
        }
        queue.sort();
        let mut visited: Vec<PathBuf> = queue
            .iter()
            .map(|(path, _)| canonical_session_path(path))
            .collect();
        let mut tombstoned: Vec<(String, PathBuf)> = Vec::new();
        let mut records = String::new();
        let mut record_count = 0usize;
        let mut push_record = |records: &mut String, record: Value| -> Result<()> {
            records.push_str(&serde_json::to_string(&record)?);
            records.push('\n');
            record_count += 1;
            anyhow::ensure!(
                records.len() as u64 <= RLM_LEDGER_MAX_BYTES
                    && record_count < RLM_LEDGER_MAX_RECORDS,
                "legacy seed exceeds authority limits; refusing partial history"
            );
            Ok(())
        };
        let mut budget = legacy_registry::LegacySeedBudget::default();
        while let Some((session_file, depth)) = queue.pop() {
            let entries = legacy_registry::read_legacy_registry_checked(&session_file, &mut budget)
                .with_context(|| format!("legacy root {}", session_file.display()))?;
            for entry in entries {
                let child_path = canonical_session_path(Path::new(&entry.session_file));
                // A registry depth < 1 (legacy 0-depth entries exist in real
                // data) is unwritable; derive parent depth + 1 instead of
                // skipping the edge.
                let child_depth = if entry.rlm_depth >= 1 {
                    entry.rlm_depth
                } else {
                    depth + 1
                };
                let spawn = json!({
                    "v": 1,
                    "op": "spawn",
                    "at": now_iso(),
                    "childId": entry.child_id,
                    "parent": canonical_session_path(&session_file).to_string_lossy(),
                    "child": child_path.to_string_lossy(),
                    "depth": child_depth,
                    "name": entry.session_name,
                });
                if entry.status == "deleted" {
                    // Pre-ledger TS (v0.3.2-v0.7.2) recorded deletions only in
                    // this registry. Import the tombstone on its own edge key.
                    // The edge only carries the tombstone: the deleted child's
                    // transcript is never opened, copied or walked, and no
                    // usage is invented. The row has no cause field; "user" is
                    // the shipped user-delete reason (see README).
                    // Like dcc and final TS, a deleted row does not mark the
                    // path visited and its registry is not descended; no
                    // grandchild tombstone is inferred from it.
                    let key = (entry.child_id.clone(), child_path.clone());
                    if tombstoned.contains(&key) {
                        continue;
                    }
                    tombstoned.push(key);
                    push_record(&mut records, spawn)?;
                    push_record(
                        &mut records,
                        json!({
                            "v": 1,
                            "op": "delete",
                            "at": now_iso(),
                            "childId": entry.child_id,
                            "child": child_path.to_string_lossy(),
                            "reason": RlmLedgerDeleteReason::User.wire_name(),
                            "source": "legacy-registry",
                        }),
                    )?;
                    continue;
                }
                if visited.contains(&child_path) {
                    continue;
                }
                visited.push(child_path.clone());
                push_record(&mut records, spawn)?;
                queue.push((PathBuf::from(&entry.session_file), child_depth));
            }
        }
        if records.is_empty() {
            return Ok(SeedPublication::Absent);
        }
        if records.len() as u64 > RLM_LEDGER_MAX_BYTES || record_count + 1 > RLM_LEDGER_MAX_RECORDS
        {
            bail!("legacy seed exceeds authority limits; refusing partial history");
        }
        self.publish_new_ledger(&records)
    }

    /// The one writer that creates a ledger: the meta header plus `records`
    /// go to a synced `seed-<pid>-<ms>` scratch, published with a no-clobber
    /// hard link. A crash can therefore never leave a ledger with only a
    /// header. The namespace caller retires only this exact temp link after
    /// proving the selected final object is the retained candidate.
    fn publish_new_ledger(&self, records: &str) -> Result<SeedPublication> {
        let mut payload = serde_json::to_string(&json!({
            "v": 1,
            "op": "meta",
            "at": now_iso(),
            "sessionsDir": self.canonical_sessions_dir.to_string_lossy(),
        }))?;
        payload.push('\n');
        payload.push_str(records);
        let dir = self
            .ledger_path()
            .parent()
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir)?;
        let temp_path = self.ledger_path().with_extension(format!(
            "seed-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        // A retained temporary name is evidence, never a truncation target.
        let mut candidate = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&temp_path)?;
        candidate.write_all(payload.as_bytes())?;
        candidate.sync_all()?;
        #[cfg(test)]
        if SEED_PUBLISH_FAULT.with(|fault| fault.replace(false)) {
            bail!("injected legacy seed publication fault");
        }
        // Atomic no-clobber publish via a hard link: EEXIST means a live
        // append created the real file meanwhile and wins.
        match fs::hard_link(&temp_path, self.ledger_path()) {
            Ok(()) => Ok(SeedPublication::Published {
                file: candidate,
                temp_path,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(SeedPublication::Existing)
            }
            Err(error) => Err(error).context("publish validated legacy seed"),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Writes only this many bytes of the next appended line, then fails.
    pub(crate) static APPEND_WRITE_FAULT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Fails the next seed publication after its scratch is durable.
    pub(crate) static SEED_PUBLISH_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod path_tests;
#[cfg(test)]
mod strict_tests;
#[cfg(test)]
mod tests;
