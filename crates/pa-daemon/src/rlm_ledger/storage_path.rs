//! Non-migrating ledger selection under a permanent logical-scope file lock.
//! The lifecycle lock order is M -> namespace; this module never acquires M.
//! Old writers which ignore this namespace lock are unsupported. Directory
//! lookup is bounded and observational, not protection from arbitrary root swaps.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::{replay, rlm_ledger_path, RLM_LEDGER_DIR};

const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_HEADER_BYTES: u64 = 16 * 1024;

struct Selection {
    path: PathBuf,
    file: Option<File>,
    /// Mixed-history evidence that blocks authority but not display of the
    /// uniquely selected healthy history.
    authority_blocker: Option<String>,
}

/// Own files, never borrowed identity tokens: there is no self-reference and no
/// detached historical file-ID cache. The selected handle survives the operation.
pub(super) struct StorageGuard {
    lock: File,
    lock_path: PathBuf,
    selection: Selection,
    /// Dead-seed recoveries performed under this guard, for the caller to log.
    pub(super) recovered: Vec<SeedScratchRecovery>,
}

pub(super) fn same_open_file(left: &File, right: &File) -> Result<bool> {
    let left_identity = pa_core::platform::opened_file_identity(left)?;
    let right_identity = pa_core::platform::opened_file_identity(right)?;
    Ok(left_identity == right_identity)
}

/// A ledger-held namespace lock is typically held only for the duration of
/// one short, local file read/write - not across an M-fenced network wait.
/// Two DIFFERENT M-fenced targets under the SAME agent/sessions scope (e.g.
/// two concurrent session switches to different files) legitimately both
/// need this namespace lock in quick succession; without a bounded retry
/// here, the loser of a pure-timing race would fail an otherwise-valid
/// operation outright (#42: confirmed via `concurrent_replacements_never_
/// interleave_their_critical_sections` - 2 concurrent switches, each its
/// own M fence, both needing this shared namespace lock in turn).
const ACQUIRE_RETRY_ATTEMPTS: u32 = 20;
const ACQUIRE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

impl StorageGuard {
    pub(super) fn acquire(agent_dir: &Path, sessions_dir: &Path) -> Result<Self> {
        let directory = agent_dir.join("rlm-ledger-locks");
        fs::create_dir_all(&directory)?;
        checked_directory(&directory)?;
        let proposed = rlm_ledger_path(agent_dir, sessions_dir);
        let lock_path = directory.join(proposed.file_name().expect("ledger filename"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = match options.open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                checked_metadata(&lock_path)?;
                OpenOptions::new().read(true).write(true).open(&lock_path)?
            }
            Err(error) => return Err(error.into()),
        };
        validate_opened_metadata(&lock.metadata()?)?;
        // Capability failure is Unknown even for an otherwise empty namespace.
        let _ = pa_core::platform::opened_file_identity(&lock)?;
        let mut attempt = 0;
        loop {
            match pa_core::platform::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error) => {
                    let would_block = matches!(
                        error
                            .get_ref()
                            .and_then(|source| source.downcast_ref::<std::fs::TryLockError>()),
                        Some(std::fs::TryLockError::WouldBlock)
                    );
                    if !would_block {
                        return Err(error).context("RLM ledger scope could not be locked");
                    }
                    attempt += 1;
                    if attempt >= ACQUIRE_RETRY_ATTEMPTS {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "RLM ledger scope is busy; retry this operation (no append was acknowledged)",
                        )
                        .into());
                    }
                    std::thread::sleep(ACQUIRE_RETRY_INTERVAL);
                }
            }
        }
        // Under the held namespace lock no live seed owns a scratch: a seed
        // holds this guard from create_new through hard_link and cleanup.
        let recovered = recover_seed_scratch(agent_dir, &proposed)?;
        let guard = Self {
            lock,
            lock_path,
            selection: select(agent_dir, sessions_dir, None)?,
            recovered,
        };
        guard.verify_lock()?;
        Ok(guard)
    }

    pub(super) fn path(&self) -> &Path {
        &self.selection.path
    }
    pub(super) fn file(&self) -> Option<&File> {
        self.selection.file.as_ref()
    }
    pub(super) fn has_file(&self) -> bool {
        self.selection.file.is_some()
    }

    pub(super) fn authority_blocked(&self) -> bool {
        self.selection.authority_blocker.is_some()
    }

    pub(super) fn require_authority(&self) -> Result<()> {
        if let Some(blocker) = &self.selection.authority_blocker {
            bail!("{blocker}");
        }
        Ok(())
    }

    fn verify_lock(&self) -> Result<()> {
        let current = open_read(&self.lock_path)?;
        if !same_open_file(&self.lock, &current)? {
            bail!("RLM ledger namespace lock pathname was replaced; refusing authority");
        }
        Ok(())
    }

    pub(super) fn refresh(&mut self, agent_dir: &Path, sessions_dir: &Path) -> Result<()> {
        self.refresh_with_seed(agent_dir, sessions_dir, None)
    }

    pub(super) fn refresh_with_seed(
        &mut self,
        agent_dir: &Path,
        sessions_dir: &Path,
        allowed_seed: Option<&Path>,
    ) -> Result<()> {
        self.verify_lock()?;
        let next = select(agent_dir, sessions_dir, allowed_seed)?;
        if next.path != self.selection.path {
            bail!("RLM ledger selection changed during operation; refusing a second history");
        }
        if let Some(previous) = &self.selection.file {
            let current = next
                .file
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("selected RLM ledger disappeared"))?;
            if !same_open_file(previous, current)? {
                bail!("selected RLM ledger was replaced during operation");
            }
        }
        self.selection = next;
        Ok(())
    }

    /// Remove only this operation's temporary link after both pathnames are
    /// proven to name its retained published file. Mismatch retains evidence.
    pub(super) fn retire_published_seed(&self, published: &File, temp_path: &Path) -> Result<()> {
        self.verify_lock()?;
        let selected = self
            .file()
            .ok_or_else(|| anyhow::anyhow!("published seed is not selected"))?;
        if !same_open_file(published, selected)? {
            bail!("published seed identity differs from selected ledger");
        }
        let temp = open_read(temp_path)?;
        if !same_open_file(published, &temp)? {
            bail!("retained seed candidate changed; refusing cleanup");
        }
        let current = open_read(self.path())?;
        if !same_open_file(published, &current)? {
            bail!("selected ledger changed before seed cleanup");
        }
        fs::remove_file(temp_path).context("retire validated seed candidate link")?;
        self.verify_lock()
    }

    pub(super) fn read_bytes(&self, limit: u64) -> Result<Option<Vec<u8>>> {
        self.verify_lock()?;
        let Some(selected) = self.file() else {
            return Ok(None);
        };
        let metadata = selected.metadata()?;
        if metadata.len() > limit {
            bail!("RLM ledger exceeds {limit} bytes; refusing to read");
        }
        // This duplicates the selected handle, not a pathname. Header validation,
        // stat, and body replay therefore refer to the same retained object.
        let mut reader = selected.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        reader.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            bail!("RLM ledger grew beyond {limit} bytes; refusing to read");
        }
        let current = open_read(self.path())?;
        if !same_open_file(selected, &current)? {
            bail!("selected RLM ledger changed during replay");
        }
        self.verify_lock()?;
        Ok(Some(bytes))
    }

    pub(super) fn open_append(&self) -> Result<File> {
        self.verify_lock()?;
        if let Some(selected) = self.file() {
            checked_metadata(self.path())?;
            let file = OpenOptions::new()
                .read(true)
                .append(true)
                .open(self.path())?;
            validate_opened_metadata(&file.metadata()?)?;
            // Both original and reopened handles remain owned through the write.
            if !same_open_file(selected, &file)? {
                bail!("selected RLM ledger changed before append");
            }
            return Ok(file);
        }
        // A new ledger is never created here: it is published whole (header
        // plus first record) through the seed scratch and no-clobber link.
        bail!(
            "RLM ledger {} has no selected file to append to",
            self.path().display()
        )
    }
}

fn checked_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        bail!("RLM ledger namespace directory is not a plain directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != pa_core::platform::process::current_user_id()
            || metadata.mode() & 0o022 != 0
        {
            bail!("RLM ledger namespace directory ownership is not private");
        }
    }
    Ok(())
}

fn is_reparse(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // WinNT.h FILE_ATTRIBUTE_REPARSE_POINT. Identity does not authorize reparse traversal.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn validate_opened_metadata(metadata: &Metadata) -> Result<()> {
    if !metadata.is_file() || is_reparse(metadata) {
        bail!("RLM ledger entry is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != pa_core::platform::process::current_user_id() {
            bail!("RLM ledger entry has foreign ownership");
        }
    }
    Ok(())
}

fn checked_metadata(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    validate_opened_metadata(&metadata)?;
    Ok(metadata)
}

fn open_read(path: &Path) -> Result<File> {
    let before = checked_metadata(path)?;
    let file = File::open(path)?;
    let opened = file.metadata()?;
    validate_opened_metadata(&opened)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            bail!("RLM ledger entry changed while opening");
        }
    }
    #[cfg(not(unix))]
    let _ = before;
    let _ = pa_core::platform::opened_file_identity(&file)?;
    Ok(file)
}

fn path_within(candidate: &Path, root: &Path) -> Result<bool> {
    if !candidate.is_absolute() {
        return Ok(false);
    }
    let resolved = crate::lease::checked_session_path(candidate)?;
    Ok(resolved == root || resolved.starts_with(root))
}

/// One candidate after strict duplicate-aware header validation. Its handle
/// lives only inside one operation; foreign handles are closed after use.
struct Candidate {
    path: PathBuf,
    file: File,
    scope: PathBuf,
}

/// Observed identity of a candidate in one operation: the key a cached
/// verdict must match. No file handle is retained across calls.
#[derive(Clone, PartialEq, Eq)]
struct CandidateKey {
    path: PathBuf,
    stat: replay::FileIdentity,
    #[cfg(unix)]
    device: u64,
}

fn candidate_key(path: &Path, file: &File) -> Result<CandidateKey> {
    let metadata = file.metadata()?;
    Ok(CandidateKey {
        path: path.to_path_buf(),
        stat: replay::opened_file_stat(file)?,
        #[cfg(unix)]
        device: {
            use std::os::unix::fs::MetadataExt;
            metadata.dev()
        },
    })
}

struct AuthorityRoots {
    scope: PathBuf,
    artifact_roots: Vec<PathBuf>,
}

impl AuthorityRoots {
    fn new(agent_dir: &Path, scope: &Path) -> Result<Self> {
        let mut artifact_roots = vec![crate::lease::checked_session_path(
            &agent_dir.join(crate::context_tree_children::RLM_SESSION_ARTIFACTS_DIR),
        )?];
        if let Some(parent) = scope.parent() {
            artifact_roots.push(crate::lease::checked_session_path(
                &parent.join(crate::context_tree_children::RLM_SESSION_ARTIFACTS_DIR),
            )?);
        }
        artifact_roots.sort();
        artifact_roots.dedup();
        Ok(Self {
            scope: scope.to_path_buf(),
            artifact_roots,
        })
    }

    fn in_artifacts(&self, path: &Path, work: &mut u64) -> Result<bool> {
        for root in &self.artifact_roots {
            if charged_within(path, root, work)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[derive(Default)]
struct Topology {
    parent_by_child: std::collections::HashMap<PathBuf, PathBuf>,
    /// Rename/delete records naming children without a spawn in this history.
    unlinked_children: Vec<PathBuf>,
}

/// Strictly parse one retained history into canonical topology. Every
/// canonicalization is charged to `work`; exhaustion is Unknown.
fn read_topology(file: &File, work: &mut u64) -> Result<Topology> {
    let metadata = file.metadata()?;
    charge(work, metadata.len())?;
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    reader
        .take(super::RLM_LEDGER_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > super::RLM_LEDGER_MAX_BYTES {
        bail!("RLM ledger topology scan exceeds byte limit");
    }
    let terminated = bytes.is_empty() || bytes.ends_with(b"\n");
    let mut topology = Topology::default();
    let mut spawned = std::collections::HashSet::new();
    let mut later = Vec::new();
    let mut segments = bytes.split(|byte| *byte == b'\n').enumerate().peekable();
    let mut header_seen = false;
    // The same per-line rule as authority reads: skip only the bad line and
    // keep scanning, so a later well-formed edge still proves a relation.
    while let Some((index, raw)) = segments.next() {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if index >= super::RLM_LEDGER_MAX_RECORDS {
            bail!("RLM ledger topology exceeds record limit");
        }
        let torn = !terminated && segments.peek().is_none();
        match replay::classify_authority_line(raw, index, torn)? {
            replay::AuthorityLine::Record(replay::LedgerRecord::Spawn {
                parent, child, ..
            }) => {
                charge(work, 2)?;
                let parent = absolute_topology_path(&parent)?;
                let child = absolute_topology_path(&child)?;
                if let Some(previous) = topology
                    .parent_by_child
                    .insert(child.clone(), parent.clone())
                {
                    if previous != parent {
                        bail!("RLM ledger topology has conflicting parents for one child");
                    }
                }
                spawned.insert(child);
            }
            replay::AuthorityLine::Record(
                replay::LedgerRecord::Rename { child, .. }
                | replay::LedgerRecord::Delete { child, .. },
            ) => {
                charge(work, 1)?;
                later.push(absolute_topology_path(&child)?);
            }
            // Only the first line may be the already-validated meta header.
            replay::AuthorityLine::Meta if !header_seen => {}
            replay::AuthorityLine::Meta => {
                bail!("RLM ledger topology contains a second meta header")
            }
            replay::AuthorityLine::Skip(_) => charge(work, 1)?,
        }
        header_seen = true;
    }
    topology.unlinked_children = later
        .into_iter()
        .filter(|child| !spawned.contains(child))
        .collect();
    Ok(topology)
}

fn absolute_topology_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("RLM ledger topology path is relative; refusing cwd-dependent authority");
    }
    Ok(crate::lease::checked_session_path(path)?)
}

/// Each history and each foreign relation has its own documented budget; one
/// large valid ledger cannot exhaust the work allowance of another root.
#[cfg(not(test))]
const RELATION_WORK_LIMIT: u64 =
    super::RLM_LEDGER_MAX_BYTES + super::RLM_LEDGER_MAX_RECORDS as u64 * 4;
/// Unit tests pin the cap-crossing behavior with smaller limits; the
/// production limits above are the documented values.
#[cfg(test)]
const RELATION_WORK_LIMIT: u64 = 1024 * 1024;

fn charge(work: &mut u64, amount: u64) -> Result<()> {
    *work = work
        .checked_add(amount)
        .ok_or_else(|| anyhow::anyhow!("RLM ledger relation work overflow"))?;
    if *work > RELATION_WORK_LIMIT {
        bail!("RLM ledger relation check exceeds its per-history work limit");
    }
    Ok(())
}

const SHARED_RELATION_WORK_LIMIT: u64 = RELATION_WORK_LIMIT * 4;

fn charge_shared(shared: &mut u64, amount: u64) -> Result<()> {
    *shared = shared
        .checked_add(amount)
        .ok_or_else(|| anyhow::anyhow!("RLM ledger shared relation work overflow"))?;
    if *shared > SHARED_RELATION_WORK_LIMIT {
        bail!("RLM ledger mixed-history checks exceed the shared namespace work limit");
    }
    Ok(())
}

fn charged_within(candidate: &Path, root: &Path, work: &mut u64) -> Result<bool> {
    charge(work, 1)?;
    path_within(candidate, root)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Current,
    Foreign,
}

/// Follow parent links through BOTH the selected and foreign histories until a
/// configured root is reached. Unresolved, cyclic, or over-budget chains are
/// Unknown, which callers treat as related (refusal), never as unrelated.
fn owner_of(
    path: &Path,
    roots: &AuthorityRoots,
    foreign_scope: &Path,
    graphs: [&Topology; 2],
    work: &mut u64,
) -> Result<Option<Owner>> {
    let mut cursor = path.to_path_buf();
    let mut seen = std::collections::HashSet::new();
    loop {
        charge(work, 1)?;
        if charged_within(&cursor, &roots.scope, work)? {
            return Ok(Some(Owner::Current));
        }
        if charged_within(&cursor, foreign_scope, work)? {
            return Ok(Some(Owner::Foreign));
        }
        if !seen.insert(cursor.clone()) {
            return Ok(None);
        }
        let Some(parent) = graphs
            .iter()
            .find_map(|graph| graph.parent_by_child.get(&cursor))
        else {
            return Ok(None);
        };
        cursor = parent.clone();
    }
}

/// A foreign-header history is unrelated only when every path it mentions
/// is proven to belong to its own declared root. Anything reaching this root,
/// or unresolved inside an artifact tree, refuses. Histories are never merged.
fn foreign_history_is_related(
    foreign: &Topology,
    foreign_scope: &Path,
    selected: &Topology,
    roots: &AuthorityRoots,
    work: &mut u64,
) -> Result<bool> {
    for child in &foreign.unlinked_children {
        if charged_within(child, &roots.scope, work)? || roots.in_artifacts(child, work)? {
            return Ok(true);
        }
    }
    for (child, parent) in &foreign.parent_by_child {
        for path in [child, parent] {
            let artifact = roots.in_artifacts(path, work)?;
            if !artifact && !charged_within(path, &roots.scope, work)? {
                continue;
            }
            match owner_of(path, roots, foreign_scope, [foreign, selected], work)? {
                Some(Owner::Foreign) => {}
                Some(Owner::Current) | None => return Ok(true),
            }
        }
    }
    Ok(false)
}

struct RelationVerdict {
    agent_dir: PathBuf,
    scope: PathBuf,
    keys: Vec<CandidateKey>,
}

const MAX_CACHED_VERDICTS: usize = 64;

fn relation_cache() -> &'static std::sync::Mutex<Vec<RelationVerdict>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Vec<RelationVerdict>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Must run under the namespace lock with keys taken from handles opened in
/// THIS operation. Only "unrelated" verdicts are cached. Without a stable
/// device+inode identity (non-Unix), nothing is cached and checks rerun.
fn relation_verdict_is_cached(
    agent_dir: &Path,
    scope: &Path,
    keys: &[CandidateKey],
) -> Result<bool> {
    if !cfg!(unix) {
        return Ok(false);
    }
    let cache = relation_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("RLM ledger relation cache poisoned"))?;
    Ok(cache.iter().any(|cached| {
        cached.agent_dir == agent_dir && cached.scope == scope && cached.keys == keys
    }))
}

fn remember_relation_verdict(
    agent_dir: &Path,
    scope: &Path,
    keys: Vec<CandidateKey>,
) -> Result<()> {
    if !cfg!(unix) {
        return Ok(());
    }
    let mut cache = relation_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("RLM ledger relation cache poisoned"))?;
    cache.retain(|cached| cached.agent_dir != agent_dir || cached.scope != scope);
    if cache.len() >= MAX_CACHED_VERDICTS {
        cache.remove(0);
    }
    cache.push(RelationVerdict {
        agent_dir: agent_dir.to_path_buf(),
        scope: scope.to_path_buf(),
        keys,
    });
    Ok(())
}

fn blocked_by(path: &Path) -> String {
    format!(
        "RLM ledger relation check is blocked by {}; repair that ledger or retire it only after verifying it is unrelated (no automatic merge or deletion is performed)",
        path.display()
    )
}

/// Read and strictly validate one candidate's ownership header from the
/// retained handle. Duplicate keys are ambiguity, never last-value-wins.
fn read_candidate(path: PathBuf) -> Result<Candidate> {
    let file = open_read(&path)?;
    let mut header_reader = file.try_clone()?;
    header_reader.seek(SeekFrom::Start(0))?;
    read_candidate_header(path, file, header_reader)?
}

/// Header bytes read from a safely opened entry.
enum HeaderLine {
    Line(String),
    /// `read_line` reported `InvalidData`: the bytes are not UTF-8.
    InvalidUtf8,
}

/// The single header read. Every I/O error is returned as fatal, except
/// `InvalidData` (bad UTF-8), which is content. `read_line` retries
/// `Interrupted` itself; any other error after a retry is fatal here.
fn read_header_line(reader: impl Read) -> std::io::Result<HeaderLine> {
    let mut line = String::new();
    match BufReader::new(reader.take(MAX_HEADER_BYTES + 1)).read_line(&mut line) {
        Ok(_) => Ok(HeaderLine::Line(line)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            Ok(HeaderLine::InvalidUtf8)
        }
        Err(error) => Err(error),
    }
}

/// Header validation for an entry whose physical identity is already safe.
/// Outer `Err`: fatal I/O while reading the header (display refuses).
/// Inner `Err`: content the code decided is not a valid ownership header
/// (bad UTF-8, over-length, no line ending, malformed or mismatched header);
/// that may become an authority blocker. Unsafe identity never reaches here.
fn read_candidate_header(
    path: PathBuf,
    file: File,
    header_reader: impl Read,
) -> Result<std::result::Result<Candidate, anyhow::Error>> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        || name.len() != 16
        || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(Err(anyhow::anyhow!(
            "RLM ledger cannot establish entry ownership: {}",
            path.display()
        )));
    }
    let line = match read_header_line(header_reader)
        .with_context(|| format!("read RLM ledger header {}", path.display()))?
    {
        HeaderLine::Line(line) => line,
        HeaderLine::InvalidUtf8 => {
            return Ok(Err(anyhow::anyhow!(
                "RLM ledger ownership header is not UTF-8"
            )))
        }
    };
    Ok(validate_candidate_header(path, file, &name, &line))
}

fn validate_candidate_header(
    path: PathBuf,
    file: File,
    name: &str,
    line: &str,
) -> Result<Candidate> {
    // Over-length and missing line ending are decided here, from the bytes,
    // not from an I/O error kind: they are content.
    if line.len() as u64 > MAX_HEADER_BYTES || !line.ends_with('\n') {
        bail!("RLM ledger ownership header is incomplete or oversized");
    }
    replay::validate_unique_json(line)?;
    let header: Value = serde_json::from_str(line)
        .map_err(|_| anyhow::anyhow!("RLM ledger ownership header is malformed"))?;
    let declared = header
        .get("sessionsDir")
        .and_then(Value::as_str)
        .filter(|dir| Path::new(dir).is_absolute());
    if header.get("v").and_then(Value::as_u64) != Some(1)
        || header.get("op").and_then(Value::as_str) != Some("meta")
        || header.get("at").and_then(Value::as_str).is_none()
        || declared.is_none()
    {
        bail!("RLM ledger ownership header is malformed");
    }
    let declared = declared.expect("validated directory");
    if crate::paths::hash_key(declared, 16) != name {
        bail!("RLM ledger filename disagrees with its ownership header");
    }
    let scope = replay::checked_scope(Path::new(declared))?;
    Ok(Candidate { path, file, scope })
}

pub(super) enum SeedScratchRecovery {
    /// No ledger existed: the dead attempt's scratch moved aside, bytes kept.
    Retained { from: PathBuf, to: PathBuf },
    /// The publish completed and only cleanup was missed: the extra NAME of
    /// the selected ledger inode was unlinked (link count 2 -> 1).
    RetiredPublishedName { name: PathBuf },
}

/// `<ledger stem>.seed-<digits>-<digits>`: this namespace's own scratch only.
fn own_seed_scratch(proposed: &Path, path: &Path) -> bool {
    let (Some(stem), Some(name)) = (
        proposed.file_stem().and_then(|stem| stem.to_str()),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return false;
    };
    let Some(suffix) = name
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix(".seed-"))
    else {
        return false;
    };
    let mut parts = suffix.split('-');
    matches!((parts.next(), parts.next(), parts.next()), (Some(pid), Some(ms), None)
        if !pid.is_empty() && !ms.is_empty() && pid.bytes().chain(ms.bytes()).all(|byte| byte.is_ascii_digit()))
}

fn recover_seed_scratch(agent_dir: &Path, proposed: &Path) -> Result<Vec<SeedScratchRecovery>> {
    let directory = agent_dir.join(RLM_LEDGER_DIR);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let ledger_exists = match fs::symlink_metadata(proposed) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let mut recovered = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            bail!("RLM ledger directory exceeds {MAX_DIRECTORY_ENTRIES} entries; refusing incomplete ownership scan");
        }
        let path = entry?.path();
        // Symlink, non-regular, foreign-owned or unknown names stay; select refuses them.
        if !own_seed_scratch(proposed, &path) || checked_metadata(&path).is_err() {
            continue;
        }
        if ledger_exists {
            // (a) Post-link crash: retire only an extra name of the SAME inode.
            let scratch = open_read(&path)?;
            let ledger = open_read(proposed)?;
            if !same_open_file(&scratch, &ledger)? {
                continue;
            }
            // Recheck both pathnames immediately before the unlink.
            let (scratch_now, ledger_now) = (open_read(&path)?, open_read(proposed)?);
            if !same_open_file(&scratch, &scratch_now)?
                || !same_open_file(&ledger, &ledger_now)?
                || !same_open_file(&scratch_now, &ledger_now)?
            {
                continue;
            }
            fs::remove_file(&path).context("retire published seed name")?;
            recovered.push(SeedScratchRecovery::RetiredPublishedName { name: path });
            continue;
        }
        let target = retain_aside(agent_dir, &path)?;
        recovered.push(SeedScratchRecovery::Retained {
            from: path,
            to: target,
        });
    }
    Ok(recovered)
}

/// Move one dead scratch into the private retained directory, never deleting
/// its bytes. The directory is no-follow, 0700 and ours; an existing symlink,
/// reparse point, foreign owner or group/other-writable directory refuses.
/// The private retained directory: created 0700, never followed, owned by
/// this user, not group/other-writable; anything else refuses.
fn retained_directory(agent_dir: &Path) -> Result<PathBuf> {
    let retained = agent_dir.join("rlm-ledger-retained");
    match fs::symlink_metadata(&retained) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&retained) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("create retained seed directory"),
            }
        }
        Err(error) => return Err(error.into()),
    }
    // symlink_metadata-based: never follows, refuses symlink/reparse/foreign/0o022.
    checked_directory(&retained).context("retained seed directory is not private")?;
    Ok(retained)
}

/// No-clobber retained names: `<base>`, then `<base>.1` ... `<base>.63`.
fn retained_targets<'a>(retained: &'a Path, base: &'a str) -> impl Iterator<Item = PathBuf> + 'a {
    let base = base.to_string();
    (0..64u32).map(move |attempt| {
        if attempt == 0 {
            retained.join(&base)
        } else {
            retained.join(format!("{base}.{attempt}"))
        }
    })
}

/// Move one dead scratch into the private retained directory, never deleting
/// its bytes. The directory is no-follow, 0700 and ours; an existing symlink,
/// reparse point, foreign owner or group/other-writable directory refuses.
fn retain_aside(agent_dir: &Path, path: &Path) -> Result<PathBuf> {
    let retained = retained_directory(agent_dir)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("checked scratch name");
    let base = format!("{name}.{}", crate::util::now_ms());
    for target in retained_targets(&retained, &base) {
        // hard_link never overwrites: AlreadyExists tries the next suffix.
        match fs::hard_link(path, &target) {
            Ok(()) => {
                fs::remove_file(path).context("retire dead seed scratch name")?;
                return Ok(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("retain dead seed scratch"),
        }
    }
    bail!(
        "could not choose a unique retained seed name for {}",
        path.display()
    )
}

/// Copy a torn ledger tail (the bytes after the last newline) into the
/// retained directory with a no-clobber name, and sync it, before the caller
/// truncates the ledger. Any failure refuses the append; nothing is truncated.
pub(super) fn retain_torn_tail(agent_dir: &Path, ledger: &Path, tail: &[u8]) -> Result<PathBuf> {
    let retained = retained_directory(agent_dir)?;
    let name = ledger
        .file_name()
        .and_then(|name| name.to_str())
        .expect("ledger name");
    let base = format!("{name}.torn.{}", crate::util::now_ms());
    for target in retained_targets(&retained, &base) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&target) {
            Ok(mut file) => {
                file.write_all(tail).context("retain torn ledger tail")?;
                file.sync_all().context("sync retained torn ledger tail")?;
                return Ok(target);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("retain torn ledger tail"),
        }
    }
    bail!(
        "could not choose a unique retained name for the torn tail of {}",
        ledger.display()
    )
}

/// Called only with the logical scope locked. Nothing is moved, linked, copied,
/// merged, or selected by newest timestamp. Candidate handles stay open through
/// validation; the unique selected handle is returned alive.
fn select(agent_dir: &Path, sessions_dir: &Path, allowed_seed: Option<&Path>) -> Result<Selection> {
    let proposed = rlm_ledger_path(agent_dir, sessions_dir);
    let directory = agent_dir.join(RLM_LEDGER_DIR);
    match fs::symlink_metadata(&directory) {
        Ok(_) => checked_directory(&directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::lease::checked_session_path(&directory)?;
            return Ok(Selection {
                path: proposed,
                file: None,
                authority_blocker: None,
            });
        }
        Err(error) => return Err(error.into()),
    }
    let scope = replay::checked_scope(sessions_dir)?;
    let roots = AuthorityRoots::new(agent_dir, &scope)?;
    // Pass 1: validate every header; keep only the selected handle. Foreign
    // candidates keep path/scope/key; their handles close immediately.
    let mut selected: Option<Candidate> = None;
    let mut foreign: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut keys = Vec::new();
    let mut authority_blocker = None;
    for (index, entry) in fs::read_dir(&directory)?.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            bail!("RLM ledger directory exceeds {MAX_DIRECTORY_ENTRIES} entries; refusing incomplete ownership scan");
        }
        let path = entry?.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.starts_with("seed-"))
        {
            if allowed_seed.is_some_and(|allowed| allowed == path) {
                continue;
            }
            // Another namespace's scratch is that namespace's evidence; its own guard recovers it.
            if path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_none_or(|stem| Some(stem) != proposed.file_stem().and_then(|own| own.to_str()))
            {
                continue;
            }
            bail!("RLM ledger has a retained seed candidate; explicit owner recovery is required");
        }
        // Unsafe physical identity refuses display too, as before v12.
        let file = open_read(&path).with_context(|| blocked_by(&path))?;
        let key = candidate_key(&path, &file)?;
        let mut header_reader = file.try_clone().with_context(|| blocked_by(&path))?;
        header_reader
            .seek(SeekFrom::Start(0))
            .with_context(|| blocked_by(&path))?;
        // Header-read I/O failure is fatal in both branches; only content
        // classification can become a blocker or own-name lenient display.
        let candidate = match read_candidate_header(path.clone(), file, header_reader)
            .with_context(|| blocked_by(&path))?
        {
            Ok(candidate) => candidate,
            // Our own proposed filename with a missing, invalid, or relative
            // header stays the selected history for lenient display. Authority
            // reads, admission, and append refuse; nothing rewrites its header.
            Err(error) if path == proposed => {
                let file = open_read(&path).with_context(|| blocked_by(&path))?;
                if candidate_key(&path, &file)? != key {
                    bail!(
                        "RLM ledger entry changed during selection: {}",
                        blocked_by(&path)
                    );
                }
                keys.push(key);
                authority_blocker
                    .get_or_insert_with(|| format!("{}: {error:#}", blocked_by(&path)));
                if selected.is_some() {
                    bail!(
                        "RLM ledger has multiple matching filenames; refusing ambiguous authority"
                    );
                }
                selected = Some(Candidate {
                    path,
                    file,
                    scope: scope.clone(),
                });
                continue;
            }
            Err(error) => {
                authority_blocker
                    .get_or_insert_with(|| format!("{}: {error:#}", blocked_by(&path)));
                continue;
            }
        };
        keys.push(key);
        if candidate.scope == scope {
            if selected.is_some() {
                bail!("RLM ledger has multiple matching filenames; refusing ambiguous authority");
            }
            selected = Some(candidate);
        } else {
            foreign.push((candidate.path, candidate.scope));
        }
    }
    keys.sort_by(|left, right| left.path.cmp(&right.path));
    // Selected-history topology is only foreign-relation evidence. Do not parse
    // it unless a foreign candidate exists; display keeps its degraded policy.
    if authority_blocker.is_none()
        && !foreign.is_empty()
        && !relation_verdict_is_cached(agent_dir, &scope, &keys)?
    {
        let mut shared = 0;
        let verdict = (|| -> Result<Option<String>> {
            let selected_topology = match &selected {
                Some(candidate) => {
                    let mut work = 0;
                    let topology = read_topology(&candidate.file, &mut work)
                        .with_context(|| blocked_by(&candidate.path))?;
                    charge_shared(&mut shared, work)
                        .with_context(|| blocked_by(&candidate.path))?;
                    topology
                }
                None => Topology::default(),
            };
            // Pass 2: one foreign handle at a time. Reopen, revalidate its
            // header/scope and key, read, then close before the next one.
            for (path, scope_of_path) in &foreign {
                let candidate = read_candidate(path.clone()).with_context(|| blocked_by(path))?;
                if &candidate.scope != scope_of_path
                    || !keys.contains(&candidate_key(&candidate.path, &candidate.file)?)
                {
                    bail!(
                        "RLM ledger candidate changed during relation check: {}",
                        blocked_by(path)
                    );
                }
                let mut work = 0;
                let topology =
                    read_topology(&candidate.file, &mut work).with_context(|| blocked_by(path))?;
                let related = foreign_history_is_related(
                    &topology,
                    &candidate.scope,
                    &selected_topology,
                    &roots,
                    &mut work,
                )
                .with_context(|| blocked_by(path))?;
                charge_shared(&mut shared, work).with_context(|| blocked_by(path))?;
                if related {
                    return Ok(Some(format!(
                        "RLM ledger has unresolved legacy authority from another lexical root: {}",
                        blocked_by(path)
                    )));
                }
            }
            Ok(None)
        })();
        match verdict {
            Ok(None) => remember_relation_verdict(agent_dir, &scope, keys)?,
            Ok(Some(blocker)) => authority_blocker = Some(blocker),
            Err(error) => authority_blocker = Some(format!("{error:#}")),
        }
    }
    Ok(match selected {
        Some(candidate) => Selection {
            path: candidate.path,
            file: Some(candidate.file),
            authority_blocker,
        },
        None => Selection {
            path: proposed,
            file: None,
            authority_blocker,
        },
    })
}

#[cfg(test)]
mod header_tests {
    use super::*;

    /// Receiver-side injection: optionally one `Interrupted` (which std
    /// retries), then a real I/O error from the reader the parser consumes.
    struct FailingReader {
        interrupt_first: bool,
        kind: std::io::ErrorKind,
    }

    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.interrupt_first) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            Err(std::io::Error::from(self.kind))
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, File) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("0123456789abcdef.jsonl");
        fs::write(&path, b"{}\n").unwrap();
        let file = File::open(&path).unwrap();
        (temp, path, file)
    }

    #[test]
    fn header_read_io_errors_are_fatal_not_blockers() {
        for kind in [
            std::io::ErrorKind::Other,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            for interrupt_first in [false, true] {
                let (_temp, path, file) = fixture();
                let outcome = read_candidate_header(
                    path,
                    file,
                    FailingReader {
                        interrupt_first,
                        kind,
                    },
                );
                assert!(
                    outcome.is_err(),
                    "{kind:?} (interrupted first: {interrupt_first}) must be fatal, not content"
                );
            }
        }
    }

    #[test]
    fn invalid_utf8_is_content_not_fatal() {
        let (_temp, path, file) = fixture();
        let outcome = read_candidate_header(path, file, &b"\xff\xfe\n"[..]).unwrap();
        assert!(outcome.is_err());
    }

    #[test]
    fn missing_line_ending_and_over_length_are_content_decided_by_code() {
        let (_temp, path, file) = fixture();
        assert!(read_candidate_header(path, file, &b"{\"v\":1}"[..])
            .unwrap()
            .is_err());
        let (_temp, path, file) = fixture();
        let long = vec![b'x'; MAX_HEADER_BYTES as usize + 8];
        assert!(read_candidate_header(path, file, &long[..])
            .unwrap()
            .is_err());
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn two_recoveries_in_one_millisecond_do_not_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path();
        let ledger_dir = agent.join(RLM_LEDGER_DIR);
        fs::create_dir_all(&ledger_dir).unwrap();
        let name = "0123456789abcdef.seed-1-1";
        let scratch = ledger_dir.join(name);
        fs::write(&scratch, b"first").unwrap();
        // Occupy every base name the call can choose within a bounded window,
        // so the no-clobber link must fall through to a suffix.
        let retained = agent.join("rlm-ledger-retained");
        fs::DirBuilder::new().create(&retained).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&retained, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let start = crate::util::now_ms();
        for ms in start..start + 5_000 {
            fs::write(retained.join(format!("{name}.{ms}")), b"").unwrap();
        }
        let target = retain_aside(agent, &scratch).unwrap();
        assert!(
            target
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".1"),
            "{}",
            target.display()
        );
        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert!(!scratch.exists());
        // Earlier retained files are untouched.
        let base = target.with_extension("");
        assert_eq!(fs::read(&base).unwrap(), b"");
    }
}
