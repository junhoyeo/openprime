//! Session leases: one process may host one runtime per canonical session
//! file — a lease is `<agent-dir>/session-leases/<sha256(path)>.lock`
//! containing `owner.json`, acquired by an atomic rename; stale owners are
//! reclaimed, and a guard lock serializes lease updates.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod authority;
mod fence;

pub(crate) use authority::RuntimeSessionAuthority;
pub(crate) use fence::{with_runtime_session_fence, HeldRuntimeSessionFence};

pub const SESSION_LEASES_ENABLED_ENV: &str = "PRIME_AGENT_INTERNAL_SESSION_LEASES";
pub const SESSION_LEASE_OWNER_ID_ENV: &str = "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseOwner {
    version: u32,
    token: String,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_session_id: Option<String>,
    session_path: String,
    created_at: String,
}

/// Error matching the TS wire shape (`session_already_active`): `Display`
/// keeps the TS message byte-identical; the rendering lives in
/// [`crate::hold_refusal`].
#[derive(Debug, thiserror::Error)]
#[error("Session is already active in {owner}: {session_path}")]
pub struct SessionAlreadyActiveError {
    pub session_path: String,
    pub active_session_id: Option<String>,
    pub owner: String,
    /// The live holder's pid: what the refusal's holder classification
    /// resolves into a product flavor.
    pub holder_pid: Option<u32>,
}

impl SessionAlreadyActiveError {
    /// The typed wire info for the refusal (`session_already_active`,
    /// the TS `serializeDaemonError` shape).
    #[must_use]
    pub fn error_info(&self) -> pa_types::daemon::DaemonErrorInfo {
        pa_types::daemon::DaemonErrorInfo::SessionAlreadyActive {
            session_path: self.session_path.clone(),
            active_session_id: self.active_session_id.clone(),
        }
    }

    fn for_owner(session_path: &str, owner: Option<&LeaseOwner>) -> Self {
        SessionAlreadyActiveError {
            session_path: session_path.to_string(),
            active_session_id: owner
                .and_then(|o| o.active_session_id.clone())
                .filter(|id| !id.is_empty()),
            // An owner without a session id is still identifiable by its pid.
            owner: owner
                .and_then(|o| o.active_session_id.clone())
                .filter(|id| !id.is_empty())
                .or_else(|| owner.map(|o| format!("another process (pid {})", o.pid)))
                .unwrap_or_else(|| "another process".to_string()),
            holder_pid: owner.map(|o| o.pid),
        }
    }
}

#[must_use]
pub fn canonical_session_path(path: &Path) -> PathBuf {
    resolve_session_path(path, PathResolutionPolicy::BestEffort)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Destructive cleanup needs a proven path, not a best-effort spelling after an
/// I/O error. Only genuinely missing suffixes may use the shared ancestor walk;
/// dangling symlinks, permission failures and unresolvable components refuse.
pub(crate) fn checked_session_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    resolve_session_path(&absolute, PathResolutionPolicy::Checked)
}

#[derive(Clone, Copy)]
enum PathResolutionPolicy {
    BestEffort,
    Checked,
}

fn resolve_session_path(path: &Path, policy: PathResolutionPolicy) -> std::io::Result<PathBuf> {
    // The same nearest-existing-ancestor algorithm serves leases and checked
    // cleanup; the legacy caller retains its historical best-effort fallback.
    let mut cursor = path;
    let mut suffix = Vec::new();
    loop {
        let error = match cursor.canonicalize() {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) => error,
        };
        if matches!(policy, PathResolutionPolicy::Checked) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
            match cursor.symlink_metadata() {
                // An existing entry that cannot resolve is not an absent
                // suffix (notably a dangling symlink). Do not guess its owner.
                Ok(_) => return Err(error),
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                Err(other) => return Err(other),
            }
        }
        let Some(name) = cursor.file_name() else {
            return match policy {
                PathResolutionPolicy::BestEffort => Ok(path.to_path_buf()),
                PathResolutionPolicy::Checked => Err(error),
            };
        };
        suffix.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return match policy {
                PathResolutionPolicy::BestEffort => Ok(path.to_path_buf()),
                PathResolutionPolicy::Checked => Err(error),
            };
        };
        cursor = parent;
    }
}

/// `proc:<starttime>` start identity (TS `getProcessStartId`); shared with
/// pa-core through `pa_types::platform`.
#[must_use]
pub fn get_process_start_id(pid: u32) -> Option<String> {
    pa_types::platform::process::process_start_id(pid)
}

/// True only for a process that is actually running: zombies do not count.
///
/// # Errors
///
/// Returns an error when the platform cannot answer the liveness check.
pub fn is_process_alive(pid: u32) -> anyhow::Result<bool> {
    pa_types::platform::process::is_process_alive(pid)
}

fn lease_directory(agent_dir: &Path, session_path: &Path) -> PathBuf {
    lease_directory_for_canonical(agent_dir, &canonical_session_path(session_path))
}

/// Same key derivation as [`lease_directory`], for a caller (the M fence)
/// that already holds a resolved, revalidated canonical target and must
/// not re-derive it from a possibly-stale raw path.
pub(crate) fn lease_directory_for_canonical(agent_dir: &Path, canonical: &Path) -> PathBuf {
    let key = Sha256::digest(canonical.to_string_lossy().as_bytes())
        .iter()
        .fold(String::new(), |mut key, b| {
            use std::fmt::Write;
            write!(key, "{b:02x}").expect("write to String");
            key
        });
    agent_dir.join("session-leases").join(format!("{key}.lock"))
}

fn leases_enabled() -> bool {
    matches!(
        std::env::var(SESSION_LEASES_ENABLED_ENV).as_deref(),
        Ok("1" | "true" | "yes")
    )
}

fn read_owner(directory: &Path) -> Result<Option<LeaseOwner>> {
    let owner_path = directory.join("owner.json");
    let content = match fs::read_to_string(&owner_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let owner: LeaseOwner = serde_json::from_str(&content).map_err(|e| {
        anyhow!(
            "Corrupt session lease owner file: {} - {e}",
            owner_path.display()
        )
    })?;
    Ok(Some(owner))
}

/// Tri-state proof of a session lease's ownership, distinguishing "no
/// lease file at all" from "an owner file exists but cannot be proven live
/// or gone": unknown must never be treated as absent, and must never be
/// treated as a dead owner safe to reclaim (#37/#38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseOwnerProof {
    /// No owner record exists at this lease directory (it may not even
    /// exist, or it may be an ownerless directory with no owner.json).
    Absent,
    /// An owner record exists and its process is confirmed exited, or a
    /// comparable start token proves the pid was recycled: safe to reclaim.
    Gone,
    /// An owner record exists, is readable, and its process is confirmed live.
    Held(LiveLeaseOwner),
    /// An owner record exists but cannot be proven live or gone: corrupt
    /// JSON, unreadable, or an unverifiable (ambiguous/legacy/malformed)
    /// process-identity token comparison.
    Unknown,
}

fn classify_owner(directory: &Path) -> LeaseOwnerProof {
    let owner = match read_owner(directory) {
        Ok(Some(owner)) => owner,
        Ok(None) => return LeaseOwnerProof::Absent,
        Err(_) => return LeaseOwnerProof::Unknown,
    };
    let alive = match pa_types::platform::process::is_process_alive(owner.pid) {
        Ok(alive) => alive,
        Err(_) => return LeaseOwnerProof::Unknown,
    };
    if !alive {
        return LeaseOwnerProof::Gone;
    }
    // The pid is confirmed alive. A provable mismatch (both sides a
    // comparable format that actually differs) proves this pid number was
    // recycled, so the owner THIS RECORD describes is gone even though the
    // number is live. An unverifiable comparison - an ambiguous/legacy
    // format, a malformed payload, or simply no recorded identity at all -
    // cannot prove either way, but a live pid with SOME recorded identity
    // is still a holder worth reporting (#37/#38: this is a weaker,
    // display-oriented claim than "confirmed the same process" - the
    // reclaim/revival decisions elsewhere (owner_alive/lease_reclaimable)
    // use the full tri-state directly and are unaffected by this).
    let Some(recorded) = owner.process_start_id.as_deref() else {
        return LeaseOwnerProof::Unknown;
    };
    let observed = pa_types::platform::process::process_start_id(owner.pid);
    if matches!(
        pa_types::platform::process::compare_process_start_ids(Some(recorded), observed.as_deref()),
        pa_types::platform::process::ProcessStartIdComparison::Mismatch
    ) {
        return LeaseOwnerProof::Gone;
    }
    LeaseOwnerProof::Held(LiveLeaseOwner {
        pid: owner.pid,
        active_session_id: owner.active_session_id.filter(|id| !id.is_empty()),
        process_start_id: recorded.to_string(),
    })
}

/// Whether the recorded owner is live, unknown, or gone, for a lease
/// directory known to already exist (an `Absent` classification here
/// means "owner record missing", i.e. ambiguous, not "nothing to reclaim";
/// callers that must distinguish a never-created directory use
/// [`classify_owner`]/[`LeaseOwnerProof`] directly).
fn owner_alive(owner: &LeaseOwner) -> bool {
    match pa_types::platform::process::observe_process_identity(
        owner.pid,
        owner.process_start_id.as_deref(),
    ) {
        pa_types::platform::process::ProcessIdentity::Gone => false,
        pa_types::platform::process::ProcessIdentity::Matching
        | pa_types::platform::process::ProcessIdentity::Unknown => true,
    }
}

/// Only a positively-proven-dead owner is reclaimable. A missing/corrupt
/// owner record (`None`) at an existing lease directory is ambiguous, not
/// proof the lease is free — it must never be silently replaced or swept.
fn lease_reclaimable(owner: Option<&LeaseOwner>) -> bool {
    owner.is_some_and(|owner| !owner_alive(owner))
}

/// Strict proof of a runtime lease's ownership at the moment the M fence is
/// held: `Unknown` must never be treated as `Absent` by a caller deciding
/// whether admission/revival may proceed (the v3 common-fence contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeLeaseOwnership {
    /// No owner record exists for this canonical target (never published,
    /// or a prior holder's cleanup fully removed it).
    Absent,
    /// An owner record exists and its process is confirmed exited, or a
    /// comparable start token proves the pid was recycled.
    Stale,
    /// An owner record exists and is confirmed live.
    Held(LiveLeaseOwner),
    /// An owner record exists but cannot be proven live or gone.
    Unknown,
}

/// Called only while the canonical target's M fence is held
/// ([`HeldRuntimeSessionFence::inspect_ownership`]).
fn inspect_runtime_ownership_at(target: &fence::RuntimeSessionTarget) -> Result<RuntimeLeaseOwnership> {
    Ok(match classify_owner(&target.directory) {
        LeaseOwnerProof::Absent => RuntimeLeaseOwnership::Absent,
        LeaseOwnerProof::Gone => RuntimeLeaseOwnership::Stale,
        LeaseOwnerProof::Held(owner) => RuntimeLeaseOwnership::Held(owner),
        LeaseOwnerProof::Unknown => RuntimeLeaseOwnership::Unknown,
    })
}

/// Called only while the canonical target's M fence is held
/// ([`HeldRuntimeSessionFence::acquire_runtime_lease`]): checks the
/// authority's deletion ledger before publishing a new owner, then
/// publishes directly via `publish_owner_at` — never through
/// `acquire_runtime_session_lease`, which takes the legacy directory
/// `.guard` lock itself. A real (blocking, retrying) lock acquisition
/// must never nest inside a held M closure; M's own exclusivity (plus
/// `coordinate()`'s existing non-blocking legacy-`.guard` existence
/// check, which refuses if a live legacy holder is found) is what
/// serializes this path instead.
fn publish_runtime_owner(
    target: &fence::RuntimeSessionTarget,
    authority: &RuntimeSessionAuthority,
) -> Result<SessionLease> {
    authority.check_admission(&target.canonical)?;
    // M is already held for this exact canonical target (by the caller's
    // `coordinate()`), which has already refused if a live legacy `.guard`
    // exists for this same key (a non-blocking existence read, not a lock
    // acquisition). Publish directly here instead of calling
    // `acquire_runtime_session_lease`: that function takes the legacy
    // directory-guard lock itself, and a real (blocking, retrying) lock
    // acquisition must never nest inside a held M closure.
    publish_owner_at(&target.directory, &target.canonical, target.opened_path())
}

/// Called only while the canonical target's M fence is held
/// ([`HeldRuntimeSessionFence::revalidate_lease`]): strict identity/path
/// checks against the on-disk owner record, no recursive owner acquisition.
fn revalidate_owned_lease(target: &fence::RuntimeSessionTarget, lease: &SessionLease) -> Result<()> {
    anyhow::ensure!(
        !lease.released.load(std::sync::atomic::Ordering::SeqCst),
        "runtime session lease was already released"
    );
    anyhow::ensure!(
        lease.directory == target.directory && lease.session_path == target.canonical,
        "runtime session lease does not match this fence's canonical target"
    );
    let owner = read_owner(&lease.directory)?
        .ok_or_else(|| anyhow!("runtime session lease owner record is missing"))?;
    anyhow::ensure!(
        owner.token == lease.token && owner.pid == std::process::id(),
        "runtime session lease owner record no longer matches this process"
    );
    Ok(())
}

/// Whether a failed candidate-onto-lease-directory rename means the lease
/// directory already exists (TS `isRenameTargetContention`): POSIX raises
/// EEXIST/ENOTEMPTY; Windows surfaces the same race as EPERM/EACCES, so
/// those count only when the target actually exists - a real permission
/// problem must still propagate.
fn is_rename_target_contention(
    directory: &Path,
    error: &std::io::Error,
    platform_windows: bool,
) -> bool {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty => true,
        std::io::ErrorKind::PermissionDenied => {
            platform_windows && directory.try_exists().unwrap_or(false)
        }
        _ => false,
    }
}

/// Total attempts before a transient stale-reclaim rename failure surfaces
/// (TS `reclaimStaleLease` caps at 8 with a `10ms * attempt` backoff).
const WIN32_RECLAIM_ATTEMPTS: u32 = 8;

/// Whether a failed stale-reclaim rename deserves another attempt, and
/// how long to wait first (`10ms * attempt`); `None` means the failure
/// surfaces. Only the win32 destination-busy family retries.
fn reclaim_retry_delay_ms(
    platform_windows: bool,
    error: &std::io::Error,
    attempt: u32,
) -> Option<u64> {
    if !platform_windows || attempt >= WIN32_RECLAIM_ATTEMPTS {
        return None;
    }
    let transient = error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(32 | 33));
    transient.then(|| 10 * u64::from(attempt))
}

fn reclaim_stale(directory: &Path) -> bool {
    let stale = directory.with_extension(format!(
        "lock.stale-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let platform_windows = cfg!(windows);
    let mut attempt = 1;
    loop {
        match fs::rename(directory, &stale) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&stale);
                return true;
            }
            // The lease path is already free: nothing to reclaim.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(error) => match reclaim_retry_delay_ms(platform_windows, &error, attempt) {
                Some(delay) => {
                    std::thread::sleep(Duration::from_millis(delay));
                    attempt += 1;
                }
                None => return false,
            },
        }
    }
}

/// A guard older than this is stale and gets reclaimed (TS `stale: 5000`).
const STALE_GUARD_AFTER: Duration = Duration::from_secs(5);

/// Attempts before a fast guard acquisition surfaces its failure: the TS
/// `withLeaseGuard` budget of 100 retries at ~12ms.
const FAST_GUARD_ATTEMPTS: u32 = 100;

/// Wait past the stale window, covering the retry cadence between a
/// stale reclaim and the next acquisition attempt.
const THROUGH_STALE_SLACK: Duration = Duration::from_millis(500);

/// How long `release` drains an append already inside its write before the
/// lease directory is removed anyway: appends are file writes at most, so
/// this bounds a wedged device, not a normal turn.
const APPEND_DRAIN_AFTER: Duration = Duration::from_secs(10);

/// How long a guard acquisition waits for a foreign holder.
#[derive(Clone, Copy)]
enum GuardWait {
    /// The fast budget: the release path holds the guard only for
    /// sub-millisecond bookkeeping; a longer-busy guard fails fast.
    Fast,
    /// Outlast the stale window: a holder killed mid-mutation can never
    /// release its guard, so the acquisition survives until the reclaim.
    ThroughStale,
}

/// Serialize lease-directory mutations with a guard directory lock: a
/// bare `<lease>.guard` directory (it must stay empty — TS reclaims a
/// foreign guard by rmdir); only the `STALE_GUARD_AFTER` mtime window
/// reclaims one left by a dead holder.
fn with_lease_guard<T>(
    directory: &Path,
    wait: GuardWait,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let guard = PathBuf::from(format!("{}.guard", directory.display()));
    let deadline = match wait {
        GuardWait::Fast => None,
        GuardWait::ThroughStale => {
            Some(std::time::Instant::now() + STALE_GUARD_AFTER + THROUGH_STALE_SLACK)
        }
    };
    let mut acquired = false;
    let mut attempt = 0u32;
    loop {
        match fs::create_dir(&guard) {
            Ok(()) => {
                acquired = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Every contention consumes the budget, stale or not: a
                // reclaim that keeps failing must burn out instead of
                // spinning past the deadline.
                attempt += 1;
                let exhausted = match deadline {
                    Some(deadline) => std::time::Instant::now() >= deadline,
                    None => attempt >= FAST_GUARD_ATTEMPTS,
                };
                if exhausted {
                    break;
                }
                // Stale guard: a holder that died without cleanup. The
                // bare guard carries no holder identity, so the lease's
                // owner is the stand-in: steal only when the owner is
                // provably dead (a live owner may hold the guard mid-release).
                if crate::paths::mtime_age(&guard).is_some_and(|age| age > STALE_GUARD_AFTER)
                    && match read_owner(directory) {
                        Ok(Some(owner)) => !owner_alive(&owner),
                        Ok(None) => true,
                        Err(_) => false,
                    }
                    && fs::remove_dir_all(&guard).is_ok()
                {
                    continue;
                }
                std::thread::sleep(Duration::from_millis(10 + u64::from(attempt % 5)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    if !acquired {
        return Err(anyhow!(
            "Could not coordinate session lease: {}",
            directory.display()
        ));
    }
    let result = action();
    let _ = fs::remove_dir_all(&guard);
    result
}

/// A held session lease; release removes the directory when still owned.
#[derive(Debug)]
pub struct SessionLease {
    pub session_path: PathBuf,
    /// The path form the holder opened the file by: the process's window
    /// and scan caches key the file by it, so the release flushes both
    /// under it (`session_path` is the canonical identity; a symlinked
    /// sessions dir or macOS `/var` makes the two differ).
    opened_path: PathBuf,
    directory: PathBuf,
    token: String,
    released: std::sync::atomic::AtomicBool,
    /// Appends between `append`'s released-check and its write: `release`
    /// drains this before removing the lease directory, so no row lands
    /// behind a successor's acquire.
    append_in_flight: std::sync::atomic::AtomicUsize,
    /// Guards the destructive cleanup step only (owner reclaim/directory
    /// removal), separate from `released` (which gates new admissions as
    /// soon as the first `release()` call starts). Reset to `false` when
    /// a cleanup attempt defers past the drain deadline, so a later
    /// explicit `release()` (or `Drop`) retries the full protocol instead
    /// of silently no-op'ing forever (#37).
    cleanup_claimed: std::sync::atomic::AtomicBool,
}

impl SessionLease {
    /// Write both sidecars while the lease is still held: the window
    /// snapshot (current by construction) and the info sidecar, whose scan
    /// state this folds current — rows appended since the last read (the
    /// close's own final rows) fold here instead of at the next open.
    /// Best-effort like the window flush: only the lease holder writes,
    /// and a failed write costs the next open its warm resume, nothing
    /// more. `release` runs this before the reclaim; the orphan exit,
    /// which keeps its lease through process death, runs it alone.
    pub(crate) fn write_sidecars(&self) {
        let _ = pa_core::session::window::flush_cache(&self.opened_path);
        let _ = crate::session_store::read_session_info(&self.opened_path);
        crate::session_store::persist_info_sidecar(&self.opened_path);
    }

    pub fn release(&self) {
        let _ = self.release_attempt();
    }

    /// The fallible, synchronous sibling of [`release`](Self::release):
    /// reports whether this exact call actually freed ownership, instead
    /// of silently deferring to a later explicit `release()`/`Drop` retry.
    /// A caller that needs to know synchronously whether a fresh lease is
    /// actually free before handing its target off (e.g. a fork-replace
    /// RPC deciding whether to proceed) uses this instead of `release()`.
    pub(crate) fn try_release(&self) -> LeaseReleaseOutcome {
        self.release_attempt()
    }

    fn release_attempt(&self) -> LeaseReleaseOutcome {
        // Stop admitting new owned mutations immediately and idempotently;
        // this is independent of whether the destructive cleanup below
        // actually runs on this call.
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self
            .cleanup_claimed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // Cleanup is already claimed: either a concurrent call is
            // mid-drain right now, or a previous call already finished it.
            // This exact call neither performed nor confirmed the release,
            // so the honest answer is Deferred, not an unconfirmed Released.
            return LeaseReleaseOutcome::Deferred;
        }
        // Fence against a concurrent append: one already past its
        // released-check must land before the lease directory is removed,
        // or its row reaches the file after a successor acquires and the
        // window cache certifies a generation the new owner never saw.
        // New appends are refused from the store above; this only drains
        // writes already in flight.
        let drain_deadline = Instant::now() + APPEND_DRAIN_AFTER;
        while self
            .append_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
            && Instant::now() < drain_deadline
        {
            std::thread::yield_now();
        }
        if self
            .append_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            // The admitted mutation is still unsettled past the drain
            // bound: this attempt must not hand off or erase ownership it
            // cannot prove is safe to release. Allow a later explicit
            // `release()`/`try_release()` (or `Drop`) to retry the full
            // protocol.
            self.cleanup_claimed
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return LeaseReleaseOutcome::Deferred;
        }
        self.write_sidecars();
        let _ = with_lease_guard(&self.directory, GuardWait::Fast, || {
            if let Ok(Some(owner)) = read_owner(&self.directory) {
                if owner.token == self.token {
                    reclaim_stale(&self.directory);
                }
            }
            Ok(())
        });
        LeaseReleaseOutcome::Released
    }
}

/// The outcome of [`SessionLease::try_release`]: whether this exact call
/// actually freed ownership, or had to defer because an admitted mutation
/// was still unsettled past the drain deadline (#37) - never silently
/// treat a deferred release as a freed target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseReleaseOutcome {
    /// Ownership was actually released by this call.
    Released,
    /// This call did not release ownership (an admitted mutation was
    /// still unsettled, or cleanup was already claimed elsewhere). A later
    /// `try_release`/`release`/`Drop` retries.
    Deferred,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Counts an admitted mutation through errors and unwinding. This participates
/// in the existing bounded release drain; it does not extend its deadline.
struct OwnedMutationFlight<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for OwnedMutationFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SessionLease {
    pub(crate) fn acquire_target(&self, path: &Path) -> Result<std::sync::Arc<Self>> {
        acquire_runtime_session_lease(path, self.agent_dir()).map(std::sync::Arc::new)
    }

    /// The agent root this lease's directory lives under (derived from the
    /// lease directory itself, the same way [`acquire_target`] always has -
    /// not a stored field, so it can never drift from the directory).
    fn agent_dir(&self) -> &Path {
        self.directory
            .parent()
            .and_then(Path::parent)
            .expect("lease directory has agent root")
    }

    /// Re-verify this lease is still the confirmed owner, under a fresh M
    /// acquisition (self-locking: never call this from inside an
    /// already-held fence closure for this same canonical path - the v3
    /// contract's "same-path transfer/reuse OUTSIDE M only" convenience).
    /// A caller that already holds this `SessionLease` and wants to confirm
    /// it before reusing it for a new target/import uses this instead of
    /// re-deriving a [`RuntimeSessionAuthority`] itself.
    ///
    /// # Errors
    /// Returns an error when this lease's canonical identity can no longer
    /// be resolved, or ownership can no longer be confirmed (released,
    /// superseded, or the on-disk owner record no longer matches this
    /// process).
    pub(crate) fn revalidate(&self) -> Result<()> {
        with_runtime_session_fence(self.agent_dir(), &self.session_path, |held| {
            held.revalidate_lease(self)
        })
    }

    /// #42 fix (RECURRENCE SWEEP, site 3 of 3): `revalidate()` alone only
    /// proves the lease is still unreleased and identity-matched; it never
    /// re-reads the deletion ledger. A same-path-reuse receiver that has no
    /// ambient, already-correctly-scoped [`RuntimeSessionAuthority`] of its
    /// own (unlike the daemon's supervised `Worker`, which already holds
    /// one) needs both checks bundled under ONE held M. Resolves a strict,
    /// standalone authority scope for this lease's own agent root - the
    /// matching scope for an unsupervised caller (e.g. the CLI's direct RPC
    /// session), not a substitute for a supervised caller's own authority.
    ///
    /// # Errors
    /// Returns an error when this lease's canonical identity can no longer
    /// be resolved, ownership can no longer be confirmed, or the deletion
    /// ledger now names this target as deleted.
    pub(crate) fn revalidate_and_check_admission(&self) -> Result<()> {
        let authority = RuntimeSessionAuthority::for_agent_dir(self.agent_dir())?;
        with_runtime_session_fence(self.agent_dir(), &self.session_path, |held| {
            held.revalidate_lease(self)?;
            authority.check_admission(held.canonical_path())
        })
    }

    /// Hold the existing append admission fence across proof, mutation and sync.
    /// Release retains its existing ten-second drain policy: a wedged IO call
    /// or suspended process beyond that bound is not absolute handoff safety.
    pub(crate) fn with_owned_mutation<T>(
        &self,
        path: &Path,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.append_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _flight = OwnedMutationFlight(&self.append_in_flight);
        anyhow::ensure!(
            !self.released.load(std::sync::atomic::Ordering::SeqCst)
                && canonical_session_path(path) == self.session_path,
            "session lease does not own append target"
        );
        action()
    }

    pub(crate) fn append(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.with_owned_mutation(path, || {
            pa_core::session::window::append_cached(
                path,
                bytes,
                pa_core::session::window::AppendOwnership::SessionLeaseHeld,
            )?;
            Ok(())
        })
    }
}

/// The live owner of a session file's runtime lease, when one exists: the
/// process identity another supervisor — or a surviving worker of any
/// daemon sharing this agent dir — would collide with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLeaseOwner {
    pub pid: u32,
    /// The owner's recorded active session id: the holder identity the
    /// session-hold refusal names.
    pub active_session_id: Option<String>,
    /// The owner's recorded process-start identity (`ps2:`/`win2:`/`proc:`,
    /// or an older `ps:`/`win:` token on a lease published before this
    /// field existed): callers that need to re-verify this exact owner
    /// later (rather than re-reading the owner file) use this, not a
    /// fresh `get_process_start_id` call, which answers the pid's
    /// *current* identity, not the one this lease was published under.
    /// Never optional: `Held` itself (classify_owner's only production
    /// construction site) is reachable only via a confirmed `Matching`
    /// verdict, which `compare_process_start_ids` can only reach when both
    /// the recorded and observed identities are `Some` - so a `Held` value
    /// always carries a known start id by construction, and a looser
    /// `Option<String>` here would only invite dead code or a future
    /// construction-site bug the type system should catch instead.
    pub process_start_id: String,
}

/// Whether a live process holds the session file's runtime lease: read
/// the shared `session-leases` record (never acquiring, never reclaiming);
/// a dead, missing, or unreadable owner answers `None`.
#[must_use]
pub fn live_lease_owner(agent_dir: &Path, session_path: &Path) -> Option<LiveLeaseOwner> {
    match classify_owner(&lease_directory(agent_dir, session_path)) {
        LeaseOwnerProof::Held(owner) => Some(owner),
        LeaseOwnerProof::Absent | LeaseOwnerProof::Gone | LeaseOwnerProof::Unknown => None,
    }
}

/// Tri-state ownership proof for a boot-time veto (the automatic-revival
/// gate): unlike [`live_lease_owner`], this keeps `Unknown` distinguishable
/// from `Absent`, because a revival veto must defer on uncertainty rather
/// than treat unreadable/corrupt owner metadata as "no lease" and relaunch
/// (#37).
#[must_use]
pub(crate) fn lease_owner_proof(agent_dir: &Path, session_path: &Path) -> LeaseOwnerProof {
    classify_owner(&lease_directory(agent_dir, session_path))
}

/// Acquire the lease for one session file. Returns `None` when leases are
/// disabled (default) or `session_path` is empty.
///
/// # Errors
///
/// Returns an error when the runtime acquire fails (another live owner,
/// a busy guard past its budget, or unwritable lease files).
pub fn acquire_session_lease(
    session_path: Option<&Path>,
    agent_dir: &Path,
) -> Result<Option<SessionLease>> {
    let Some(session_path) = session_path.filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(None);
    };
    if !leases_enabled() {
        return Ok(None);
    }
    acquire_runtime_session_lease(session_path, agent_dir).map(Some)
}

/// Acquire mandatory runtime ownership before opening or writing a
/// session: the runtime acquire the daemon's workers use (ungated by
/// the test env flag). The CLI print-mode guard shares it, so a resume
/// either atomically owns the file or answers the refusal.
///
/// # Errors
///
/// Returns an error when another live owner holds the lease, the guard
/// stays busy past its budget, or the lease files cannot be created.
pub fn acquire_runtime_session_lease(
    session_path: &Path,
    agent_dir: &Path,
) -> Result<SessionLease> {
    let canonical = canonical_session_path(session_path);
    let root = agent_dir.join("session-leases");
    fs::create_dir_all(&root)?;
    let directory = lease_directory(agent_dir, &canonical);

    // The open path outlasts the stale window (see `GuardWait::ThroughStale`).
    // This legacy directory-guard lock is for callers outside the M fence
    // (the TS-compatible path and every pre-fence Rust caller); it must
    // never be acquired from inside a held M closure (see
    // `publish_owner_at`, which the M-fence path calls directly instead).
    with_lease_guard(&directory, GuardWait::ThroughStale, || {
        publish_owner_at(&directory, &canonical, session_path)
    })
}

/// The actual candidate-write-then-atomic-rename publication, shared by the
/// legacy caller above (under its own `.guard` directory lock) and the M
/// fence's `publish_runtime_owner` (under M instead - never both at once
/// for the same acquisition; M's `coordinate()` already refuses when a
/// live legacy `.guard` exists, so the two paths cannot race on the same
/// directory without one of them refusing first).
fn publish_owner_at(
    directory: &Path,
    canonical: &Path,
    opened_path: &Path,
) -> Result<SessionLease> {
    for _ in 0..3 {
        // POSIX `rename()` silently replaces an existing *empty*
        // directory target: a blind candidate-then-rename would steal
        // an ownerless (or ambiguous) directory without ever checking
        // its owner. Resolve a known pre-existing target through the
        // same checked-owner path contention already uses, instead of
        // ever renaming onto it.
        if directory.try_exists().unwrap_or(true) {
            let existing = read_owner(directory)?;
            if !lease_reclaimable(existing.as_ref()) {
                return Err(SessionAlreadyActiveError::for_owner(
                    &canonical.to_string_lossy(),
                    existing.as_ref(),
                )
                .into());
            }
            reclaim_stale(directory);
            continue;
        }
        let token = uuid::Uuid::new_v4().to_string();
        let candidate = directory.with_extension(format!(
            "lock.candidate-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&candidate)?;
        let owner = LeaseOwner {
            version: 1,
            token: token.clone(),
            pid: std::process::id(),
            process_start_id: get_process_start_id(std::process::id()),
            active_session_id: std::env::var(SESSION_LEASE_OWNER_ID_ENV).ok(),
            session_path: canonical.to_string_lossy().to_string(),
            created_at: crate::util::now_iso(),
        };
        let owner_path = candidate.join("owner.json");
        fs::write(&owner_path, serde_json::to_string_pretty(&owner)? + "\n")?;
        match fs::rename(&candidate, directory) {
            Ok(()) => {
                return Ok(SessionLease {
                    session_path: canonical.to_path_buf(),
                    opened_path: opened_path.to_path_buf(),
                    directory: directory.to_path_buf(),
                    token,
                    released: std::sync::atomic::AtomicBool::new(false),
                    append_in_flight: std::sync::atomic::AtomicUsize::new(0),
                    cleanup_claimed: std::sync::atomic::AtomicBool::new(false),
                });
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&candidate);
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                if is_rename_target_contention(directory, &error, cfg!(windows)) {
                    let existing = read_owner(directory)?;
                    if !lease_reclaimable(existing.as_ref()) {
                        return Err(SessionAlreadyActiveError::for_owner(
                            &canonical.to_string_lossy(),
                            existing.as_ref(),
                        )
                        .into());
                    }
                    reclaim_stale(directory);
                    continue;
                }
                return Err(error.into());
            }
        }
    }
    let existing = read_owner(directory)?;
    if !lease_reclaimable(existing.as_ref()) {
        return Err(SessionAlreadyActiveError::for_owner(
            &canonical.to_string_lossy(),
            existing.as_ref(),
        )
        .into());
    }
    Err(anyhow!(
        "Could not acquire session lease: {}",
        canonical.display()
    ))
}

pub(crate) fn reclaim_dead_owner_leases(agent_dir: &Path) -> usize {
    let root = agent_dir.join("session-leases");
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if std::path::Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
        {
            let reclaimed = with_lease_guard(&path, GuardWait::Fast, || {
                let existing = read_owner(&path)?;
                Ok(lease_reclaimable(existing.as_ref()) && reclaim_stale(&path))
            });
            if reclaimed.unwrap_or(false) {
                removed += 1;
            }
        } else if name.contains(".lock.stale-")
            && fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none())
            && fs::remove_dir(&path).is_ok()
        {
            // An EMPTY `.lock.stale-*` quarantine directory is exactly what
            // `reclaim_stale` leaves behind on the rare race where its own
            // rename succeeded but its own synchronous remove_dir_all did
            // not complete (a crash or kill in that narrow window) - safe
            // to finish sweeping, since an empty directory cannot carry any
            // retained state. A NON-EMPTY one has no such provenance (it
            // may be an unrelated/adversarial name, not evidence of a dead
            // owner) and a bare name match alone is never an ownership
            // certificate for it (#37) - only emptiness, which this checks
            // directly, not the name.
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod path_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn missing_nested_path_keeps_one_canonical_identity_after_creation() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        symlink(&real, &alias).unwrap();
        let session = alias.join("nested").join("session.jsonl");
        let before = canonical_session_path(&session);
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(&session, "{}\n").unwrap();
        assert_eq!(before, canonical_session_path(&session));
        assert!(before.starts_with(real.canonicalize().unwrap()));
    }

    #[test]
    fn issue_37_release_retains_ownership_until_admitted_mutation_settles() {
        use std::io::Write;
        struct FinishMutation {
            signal: Option<std::sync::mpsc::Sender<()>>,
            writer: Option<std::thread::JoinHandle<Result<()>>>,
        }
        impl FinishMutation {
            fn finish(&mut self) -> std::thread::Result<Result<()>> {
                if let Some(signal) = self.signal.take() { let _ = signal.send(()); }
                self.writer.take().map_or(Ok(Ok(())), std::thread::JoinHandle::join)
            }
        }
        impl Drop for FinishMutation {
            fn drop(&mut self) { let _ = self.finish(); }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.jsonl");
        fs::write(&path, b"initial\n").unwrap();
        let lease = std::sync::Arc::new(acquire_runtime_session_lease(&path, dir.path()).unwrap());
        let owner_path = lease.directory.join("owner.json");
        let before = fs::read(&owner_path).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let writing_lease = std::sync::Arc::clone(&lease);
        let writing_path = path.clone();
        let writer = std::thread::spawn(move || writing_lease.with_owned_mutation(&writing_path, || {
            started_tx.send(()).map_err(|error| anyhow!("mutation admission: {error}"))?;
            finish_rx.recv_timeout(Duration::from_secs(30)).map_err(|error| anyhow!("mutation completion: {error}"))?;
            fs::OpenOptions::new().append(true).open(&writing_path)?.write_all(b"settled\n")?;
            Ok(())
        }));
        let mut finish = FinishMutation { signal: Some(finish_tx), writer: Some(writer) };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The existing ten-second release deadline expires while the admitted
        // writer is held by a concrete barrier, not a readiness sleep.
        lease.release();
        assert_eq!(fs::read(&owner_path).unwrap(), before);
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        finish.finish().unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"initial\nsettled\n");
        lease.release();
        assert!(!lease.directory.exists());
    }

    #[test]
    fn issue_37_existing_ownerless_lease_is_not_replaced_by_candidate_rename() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("ownerless.jsonl");
        fs::write(&session, "{}\n").unwrap();
        let lease = lease_directory(dir.path(), &session);
        fs::create_dir_all(&lease).unwrap();
        assert!(acquire_runtime_session_lease(&session, dir.path()).is_err());
        assert!(lease.is_dir());
        assert_eq!(fs::read_dir(&lease).unwrap().count(), 0);
        assert_eq!(reclaim_dead_owner_leases(dir.path()), 0);
        assert!(lease.is_dir());
    }

    #[test]
    fn issue_37_ownerless_quarantine_is_not_deleted_from_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine = dir.path().join("session-leases/unknown.lock.stale-123-token");
        fs::create_dir_all(&quarantine).unwrap();
        fs::write(quarantine.join("retained"), b"unknown ownership").unwrap();
        assert_eq!(reclaim_dead_owner_leases(dir.path()), 0);
        assert_eq!(fs::read(quarantine.join("retained")).unwrap(), b"unknown ownership");
    }

    #[cfg(unix)]
    #[test]
    fn issue_37_unknown_and_legacy_start_formats_cannot_steal_a_live_lease() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = OwnedChild(Command::new("sh")
            .args(["-c", "printf 'ready\n'; read answer"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
        let stdout = child.0.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = std::io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = ready_tx.send(result);
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(5));
        if ready.is_err() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        reader.join().unwrap();
        assert_eq!(ready.unwrap().unwrap(), "ready\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, "{}\n").unwrap();
        let directory = lease_directory(dir.path(), &path);
        fs::create_dir_all(&directory).unwrap();
        let observed = get_process_start_id(child.0.id()).unwrap();
        let mut starts = match observed.strip_prefix("ps:").or_else(|| observed.strip_prefix("ps2:")) {
            Some(payload) => vec![format!("ps:{payload}"), format!("ps2:{payload}")],
            None => vec![observed],
        };
        starts.extend(["future:opaque", "unsupported:identity", "proc:malformed"].map(str::to_string));
        for start in &starts {
            let owner = LeaseOwner {
                version: 1, token: "original-holder".into(), pid: child.0.id(),
                process_start_id: Some(start.clone()), active_session_id: Some("legacy".into()),
                session_path: canonical_session_path(&path).to_string_lossy().into_owned(),
                created_at: crate::util::now_iso(),
            };
            let bytes = serde_json::to_vec(&owner).unwrap();
            fs::write(directory.join("owner.json"), &bytes).unwrap();
            assert!(acquire_runtime_session_lease(&path, dir.path()).is_err(), "stole {start}");
            assert_eq!(reclaim_dead_owner_leases(dir.path()), 0);
            assert_eq!(fs::read(directory.join("owner.json")).unwrap(), bytes);
            assert_eq!(live_lease_owner(dir.path(), &path).unwrap().pid, child.0.id());
            assert!(child.0.try_wait().unwrap().is_none());
        }
    }

    #[test]
    fn process_start_id_reflects_the_platform_ladder() {
        // Linux answers from /proc (`proc:`); macOS/BSD answer
        // `ps:<lstart>` - the same ladder TS `getProcessStartId` walks.
        let start = get_process_start_id(std::process::id());
        assert!(start.is_some());
        let id = start.unwrap();
        assert!(id.starts_with("proc:") || id.starts_with("ps2:"));
        assert!(get_process_start_id(0).is_none());
    }

    #[test]
    fn runtime_lease_is_mandatory_and_shared_until_last_owner_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let lease = std::sync::Arc::new(acquire_runtime_session_lease(&path, dir.path()).unwrap());
        let shared = lease.clone();
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        drop(lease);
        assert!(acquire_runtime_session_lease(&path, dir.path()).is_err());
        drop(shared);
        let next = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        assert_eq!(next.session_path, canonical_session_path(&path));
    }

    #[test]
    fn lease_conflicts_and_releases() {
        let dir = std::env::temp_dir().join(format!("pa-lease-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(SESSION_LEASES_ENABLED_ENV, "1");
        let session = dir.join("s.jsonl");
        std::fs::write(&session, "{}").unwrap();
        let lease = acquire_session_lease(Some(&session), &dir)
            .unwrap()
            .unwrap();
        let err = acquire_session_lease(Some(&session), &dir).unwrap_err();
        assert!(err.to_string().contains("already active"));
        lease.release();
        let second = acquire_session_lease(Some(&session), &dir)
            .unwrap()
            .unwrap();
        second.release();
        std::env::remove_var(SESSION_LEASES_ENABLED_ENV);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dead_owner_sweep_spares_live_and_unreadable_leases() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path();
        let live_path = agent_dir.join("live.jsonl");
        std::fs::write(&live_path, "{}\n").unwrap();
        let live = acquire_runtime_session_lease(&live_path, agent_dir).unwrap();
        let dead_directory = {
            let dead_path = agent_dir.join("dead.jsonl");
            std::fs::write(&dead_path, "{}\n").unwrap();
            let directory = lease_directory(agent_dir, &canonical_session_path(&dead_path));
            fs::create_dir_all(&directory).unwrap();
            let owner = LeaseOwner {
                version: 1,
                token: "dead-holder".to_owned(),
                pid: 0,
                process_start_id: get_process_start_id(0),
                active_session_id: None,
                session_path: canonical_session_path(&dead_path)
                    .to_string_lossy()
                    .to_string(),
                created_at: crate::util::now_iso(),
            };
            fs::write(
                directory.join("owner.json"),
                serde_json::to_string_pretty(&owner).unwrap() + "\n",
            )
            .unwrap();
            directory
        };
        let stale_leftover = dead_directory.with_extension("lock.stale-1-abc");
        fs::create_dir_all(&stale_leftover).unwrap();
        let corrupt_directory = {
            let corrupt_path = agent_dir.join("corrupt.jsonl");
            std::fs::write(&corrupt_path, "{}\n").unwrap();
            let directory = lease_directory(agent_dir, &canonical_session_path(&corrupt_path));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("owner.json"), "not json").unwrap();
            directory
        };

        assert_eq!(
            reclaim_dead_owner_leases(agent_dir),
            2,
            "the dead owner and the stale leftover are the only removals"
        );
        assert!(!dead_directory.exists());
        assert!(!stale_leftover.exists());
        assert!(
            corrupt_directory.exists(),
            "an unreadable owner keeps its lease, like the acquire path"
        );
        live.append(&live_path, b"row\n")
            .expect("the live lease still owns its append");
        live.release();
    }

    /// A kill -9 mid-mutation leaves a dead owner plus a brand-new guard
    /// it can never remove; the open path waits out the stale window.
    #[test]
    fn create_path_outlasts_a_fresh_guard_of_a_dead_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        // `pid: 0` is provably dead through the same liveness ladder the
        // reclaimer applies to real crashed holders.
        let owner = LeaseOwner {
            version: 1,
            token: "dead-holder".to_owned(),
            pid: 0,
            process_start_id: get_process_start_id(0),
            active_session_id: None,
            session_path: canonical_session_path(&path).to_string_lossy().to_string(),
            created_at: crate::util::now_iso(),
        };
        let directory = lease_directory(dir.path(), &canonical_session_path(&path));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("owner.json"),
            serde_json::to_string_pretty(&owner).unwrap() + "\n",
        )
        .unwrap();
        std::fs::create_dir(format!("{}.guard", directory.display())).unwrap();
        let started = std::time::Instant::now();
        let lease = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        // The guard was brand new (never stolen early), so success proves
        // the open waited out the stale window.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= STALE_GUARD_AFTER.saturating_sub(Duration::from_millis(250)),
            "the fresh guard was not waited out: {elapsed:?}"
        );
        // Generous ceiling: only a scheduler stall can stretch the gap.
        assert!(
            elapsed < STALE_GUARD_AFTER + THROUGH_STALE_SLACK + Duration::from_secs(10),
            "the stale reclaim overran its window: {elapsed:?}"
        );
        lease.release();
    }

    /// A stale guard of a live owner is never stolen: the open path fails
    /// at its deadline instead of breaking the live owner's append.
    #[cfg(unix)]
    #[test]
    fn stale_guard_of_a_live_owner_is_never_stolen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let lease = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        let guard = format!("{}.guard", lease.directory.display());
        std::fs::create_dir(&guard).unwrap();
        let old = std::time::SystemTime::now() - STALE_GUARD_AFTER - Duration::from_secs(1);
        std::fs::File::open(&guard)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let started = std::time::Instant::now();
        let error = acquire_runtime_session_lease(&path, dir.path()).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("Could not coordinate session lease"),
            "unexpected error: {error}"
        );
        assert!(
            elapsed >= STALE_GUARD_AFTER,
            "the wait was cut short: {elapsed:?}"
        );
        assert!(
            elapsed < STALE_GUARD_AFTER + THROUGH_STALE_SLACK + Duration::from_secs(10),
            "the wait overran its deadline: {elapsed:?}"
        );
        assert!(
            std::fs::symlink_metadata(&guard)
                .unwrap()
                .file_type()
                .is_dir(),
            "the live owner's guard was stolen"
        );
        std::fs::remove_dir(&guard).unwrap();
        lease.release();
    }

    #[cfg(unix)]
    #[test]
    fn stale_guard_reclaim_failure_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        // A dead owner (pid 0) whose guard reads stale but can never be
        // reclaimed: `remove_dir_all` on a plain file fails on every retry.
        let owner = LeaseOwner {
            version: 1,
            token: "dead-holder".to_owned(),
            pid: 0,
            process_start_id: get_process_start_id(0),
            active_session_id: None,
            session_path: canonical_session_path(&path).to_string_lossy().to_string(),
            created_at: crate::util::now_iso(),
        };
        let directory = lease_directory(dir.path(), &canonical_session_path(&path));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("owner.json"),
            serde_json::to_string_pretty(&owner).unwrap() + "\n",
        )
        .unwrap();
        let guard = format!("{}.guard", directory.display());
        std::fs::write(&guard, b"stale").unwrap();
        let old = std::time::SystemTime::now() - STALE_GUARD_AFTER - Duration::from_secs(1);
        std::fs::File::open(&guard)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let started = std::time::Instant::now();
        let error = acquire_runtime_session_lease(&path, dir.path()).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("Could not coordinate session lease"),
            "unexpected open error: {error}"
        );
        assert!(
            elapsed < STALE_GUARD_AFTER + THROUGH_STALE_SLACK + Duration::from_secs(2),
            "the failed reclaim outlived its deadline: {elapsed:?}"
        );
        std::fs::remove_file(&guard).unwrap();
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn rename_target_contention_covers_exist_and_not_empty() {
        // TS: EEXIST and ENOTEMPTY are contention on every platform.
        for kind in [
            std::io::ErrorKind::AlreadyExists,
            std::io::ErrorKind::DirectoryNotEmpty,
        ] {
            let error = std::io::Error::from(kind);
            assert!(is_rename_target_contention(
                Path::new("/tmp"),
                &error,
                false
            ));
            assert!(is_rename_target_contention(Path::new("/tmp"), &error, true));
        }
    }

    #[test]
    fn rename_target_contention_denied_only_when_win32_target_exists() {
        // TS: EPERM/EACCES count as contention on win32 when - and only
        // when - the lease directory actually exists.
        let dir = std::env::temp_dir().join(format!("pa-lease-c-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(is_rename_target_contention(&dir, &denied, true));
        assert!(!is_rename_target_contention(&dir, &denied, false));
        let missing = dir.join("missing");
        assert!(!is_rename_target_contention(&missing, &denied, true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_target_contention_ignores_unrelated_failures() {
        // TS: EBUSY and other codes are never contention (a shared-open
        // destination must surface, not read as a conflict).
        for error in [
            std::io::Error::from(std::io::ErrorKind::Other),
            // ERROR_SHARING_VIOLATION stays raw in std (libuv EBUSY).
            std::io::Error::from_raw_os_error(32),
        ] {
            assert!(!is_rename_target_contention(
                Path::new("/tmp"),
                &error,
                true
            ));
        }
    }

    #[test]
    fn reclaim_retry_is_win32_only_with_linear_backoff_until_cap() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let busy = std::io::Error::from_raw_os_error(33);
        for error in [&denied, &busy] {
            for attempt in 1..WIN32_RECLAIM_ATTEMPTS {
                assert_eq!(
                    reclaim_retry_delay_ms(true, error, attempt),
                    Some(10 * u64::from(attempt)),
                    "attempt={attempt}"
                );
            }
            assert_eq!(
                reclaim_retry_delay_ms(true, error, WIN32_RECLAIM_ATTEMPTS),
                None
            );
            assert_eq!(
                reclaim_retry_delay_ms(true, error, WIN32_RECLAIM_ATTEMPTS + 3),
                None
            );
            assert_eq!(reclaim_retry_delay_ms(false, error, 1), None);
        }
        let unrelated = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(reclaim_retry_delay_ms(true, &unrelated, 1), None);
    }

    #[test]
    fn owned_mutation_release_waits_for_the_admitted_callback() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, b"{}\n").unwrap();
        let lease = std::sync::Arc::new(acquire_runtime_session_lease(&path, dir.path()).unwrap());
        let directory = lease.directory.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let writer_lease = std::sync::Arc::clone(&lease);
            let writer_path = path.clone();
            let mutation = scope.spawn(move || {
                writer_lease.with_owned_mutation(&writer_path, || {
                    started_tx.send(()).unwrap();
                    finish_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                    let mut file = fs::OpenOptions::new().append(true).open(&writer_path)?;
                    file.write_all(b"continued\n")?;
                    file.sync_all()?;
                    Ok(())
                })
            });
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let releaser_lease = std::sync::Arc::clone(&lease);
            let release = scope.spawn(move || releaser_lease.release());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !lease.released.load(std::sync::atomic::Ordering::SeqCst)
                && Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            assert!(lease.released.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(
                lease
                    .append_in_flight
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert!(
                directory.exists(),
                "the admitted callback still owns the lease inside its drain bound"
            );
            finish_tx.send(()).unwrap();
            mutation.join().unwrap().unwrap();
            release.join().unwrap();
        });
        assert_eq!(fs::read(&path).unwrap(), b"{}\ncontinued\n");
        assert!(!directory.exists());
    }

    #[test]
    fn owned_mutation_errors_and_unwind_do_not_leak_a_flight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, b"{}\n").unwrap();
        let lease = acquire_runtime_session_lease(&path, dir.path()).unwrap();
        assert!(lease
            .with_owned_mutation::<()>(&path, || Err(anyhow!("injected callback error")))
            .is_err());
        assert_eq!(
            lease
                .append_in_flight
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = lease.with_owned_mutation::<()>(&path, || panic!("injected callback unwind"));
        }));
        assert!(panicked.is_err());
        assert_eq!(
            lease
                .append_in_flight
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        lease.release();
        assert!(!lease.directory.exists());
    }

    #[test]
    fn release_drains_an_append_still_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{}").unwrap();
        let lease = std::sync::Arc::new(acquire_runtime_session_lease(&path, dir.path()).unwrap());
        // An append past its released-check, still inside its write.
        lease
            .append_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let directory = lease.directory.clone();
        let releaser = {
            let lease = std::sync::Arc::clone(&lease);
            std::thread::spawn(move || lease.release())
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            directory.exists(),
            "release must wait for the append still in flight"
        );
        lease
            .append_in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        releaser.join().unwrap();
        assert!(
            !directory.exists(),
            "release reclaims the lease once the append lands"
        );
    }
}
