//! Immutable managed kernel-venv generations, crash-safe publication, and
//! process-identity leases. Cleanup is retain-only; explicit/custom environments never enter managed retention.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::{default_rlm_extra_uv_args, kernel_venv_python, BootstrapPythonSkill, PYTHON_VERSION};
use crate::platform::{directory_identity, DirectoryIdentity};

mod explicit;
pub(in crate::kernel::bootstrap) use explicit::{
    reclaim_explicit_path, ExplicitBuild, ExplicitReclaim,
};

const GENERATION_PREFIX: &str = "generation-";
const PUBLISHED_FILE: &str = ".generation-published";
const OWNER_FILE: &str = ".generation-owner";
const LEASE_DIR: &str = ".leases";
const BUILD_RECORD_PREFIX: &str = ".generation-build-";
const RECORD_VERSION: u64 = 2;
const IDENTITY_HEX_CHARS: usize = 20;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessOwner {
    pid: u32,
    process_start_id: Option<String>,
}

impl ProcessOwner {
    fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            process_start_id: pa_types::platform::process::process_start_id(pid),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BuildRecord {
    version: u64,
    generation: String,
    identity: String,
    created_at_ms: u64,
    token: String,
    directory_identity: DirectoryIdentity,
    root_identity: DirectoryIdentity,
    owner: ProcessOwner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OwnershipRecord {
    version: u64,
    generation: String,
    identity: String,
    token: String,
    directory_identity: DirectoryIdentity,
    root_identity: DirectoryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PublishedRecord {
    version: u64,
    generation: String,
    identity: String,
    created_at_ms: u64,
    token: String,
    directory_identity: DirectoryIdentity,
    root_identity: DirectoryIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LeaseKind {
    Resolver,
    Kernel,
    Handoff,
}

impl LeaseKind {
    fn label(self) -> &'static str {
        match self {
            Self::Resolver => "resolver",
            Self::Kernel => "kernel",
            Self::Handoff => "handoff",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeaseRecord {
    version: u64,
    kind: LeaseKind,
    created_at_ms: u64,
    #[serde(default)]
    directory_identity: Option<DirectoryIdentity>,
    #[serde(default)]
    root_identity: Option<DirectoryIdentity>,
    owner: ProcessOwner,
}

enum RecordState<T> {
    Missing,
    Valid { value: T, raw: Vec<u8> },
    Unknown,
}

#[derive(Debug, Clone)]
struct ParsedGenerationName {
    identity: String,
    created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HolderState {
    Live,
    Dead,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationUse {
    Inactive,
    Protected,
}

/// One unpublished generation owned by the current bootstrap attempt.
pub(in crate::kernel::bootstrap) struct GenerationBuild {
    root: PathBuf,
    root_identity: DirectoryIdentity,
    path: PathBuf,
    sidecar: PathBuf,
    record: BuildRecord,
    sidecar_raw: Vec<u8>,
}

impl GenerationBuild {
    pub(in crate::kernel::bootstrap) fn begin(root: &Path, identity: &str) -> anyhow::Result<Self> {
        Self::begin_at(root, identity, now_millis(), uuid::Uuid::new_v4())
    }

    fn begin_at(
        root: &Path,
        identity: &str,
        created_at_ms: u64,
        nonce: uuid::Uuid,
    ) -> anyhow::Result<Self> {
        ensure_generation_root(root)?;
        let Some(root_identity) = directory_identity(root)? else {
            bail!(
                "filesystem cannot prove kernel generation root identity: {}",
                root.display()
            );
        };
        let generation = format!("{GENERATION_PREFIX}{identity}-{created_at_ms:020}-{nonce}");
        let path = root.join(&generation);
        std::fs::create_dir(&path).with_context(|| {
            format!(
                "atomically claiming kernel generation path {}",
                path.display()
            )
        })?;
        crate::platform::perms::restrict_dir(&path)?;
        if directory_identity(root)?.as_ref() != Some(&root_identity) {
            bail!("kernel generation root identity changed during candidate creation");
        }
        let Some(directory_identity) = directory_identity(&path)? else {
            let _ = std::fs::remove_dir(&path);
            bail!(
                "filesystem cannot prove kernel generation directory identity: {}",
                path.display()
            );
        };
        let sidecar = root.join(format!("{BUILD_RECORD_PREFIX}{nonce}.json"));
        let record = BuildRecord {
            version: RECORD_VERSION,
            generation,
            identity: identity.to_string(),
            created_at_ms,
            token: nonce.to_string(),
            directory_identity,
            root_identity: root_identity.clone(),
            owner: ProcessOwner::current(),
        };
        let sidecar_raw = match write_json_new(&sidecar, &record) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    "unpublished kernel generation retained after sidecar publication failure"
                );
                return Err(error);
            }
        };
        Ok(Self {
            root: root.to_path_buf(),
            root_identity,
            path,
            sidecar,
            record,
            sidecar_raw,
        })
    }

    pub(in crate::kernel::bootstrap) fn path(&self) -> &Path {
        &self.path
    }

    /// Claim the directory only after `uv venv` created it. A failure before
    /// this marker is unknowable and is retained rather than deleted.
    pub(in crate::kernel::bootstrap) fn claim_created_path(&self) -> anyhow::Result<()> {
        let RecordState::Valid { value, raw } = read_record::<BuildRecord>(&self.sidecar) else {
            bail!("kernel generation build ownership is unavailable after uv venv");
        };
        if value != self.record || raw != self.sidecar_raw {
            bail!("kernel generation build ownership changed after uv venv");
        }
        if directory_identity(&self.root)?.as_ref() != Some(&self.root_identity) {
            bail!("kernel generation root identity changed during uv venv");
        }
        require_direct_directory(&self.path)?;
        if directory_identity(&self.path)?.as_ref() != Some(&self.record.directory_identity) {
            bail!("kernel generation directory identity changed during uv venv");
        }
        write_json_new(
            &self.path.join(OWNER_FILE),
            &OwnershipRecord {
                version: RECORD_VERSION,
                generation: self.record.generation.clone(),
                identity: self.record.identity.clone(),
                token: self.record.token.clone(),
                directory_identity: self.record.directory_identity.clone(),
                root_identity: self.record.root_identity.clone(),
            },
        )?;
        Ok(())
    }

    /// Retain this attempt's unpublished target. Only a raw-equal sidecar
    /// whose target is provably absent is removed; directories are never deleted.
    pub(in crate::kernel::bootstrap) fn discard(&self) {
        let RecordState::Valid { raw, .. } = read_record::<BuildRecord>(&self.sidecar) else {
            return;
        };
        if raw != self.sidecar_raw
            || directory_identity(&self.root).ok().flatten().as_ref() != Some(&self.root_identity)
        {
            return;
        }
        if !matches!(
            read_record::<PublishedRecord>(&self.path.join(PUBLISHED_FILE)),
            RecordState::Missing
        ) {
            return;
        }
        match symlink_metadata_state(&self.path) {
            Ok(None) => remove_file_if_unchanged(&self.sidecar, &self.sidecar_raw),
            Ok(Some(metadata))
                if metadata.file_type().is_dir()
                    && !metadata.file_type().is_symlink()
                    && directory_matches_build(&self.path, &self.record) =>
            {
                tracing::warn!("unpublished kernel generation retained after bootstrap failure");
            }
            Ok(Some(_)) | Err(_) => {}
        }
    }

    /// Publish after the caller validates the interpreter and imports. The
    /// resolver lease lands first; the marker is the single selection point.
    pub(in crate::kernel::bootstrap) fn publish(&self) -> anyhow::Result<()> {
        let RecordState::Valid { value, raw } = read_record::<BuildRecord>(&self.sidecar) else {
            bail!("kernel generation build ownership is unavailable");
        };
        if value != self.record || raw != self.sidecar_raw {
            bail!("kernel generation build ownership changed before publication");
        }
        if directory_identity(&self.root)?.as_ref() != Some(&self.root_identity) {
            bail!("kernel generation root identity changed before publication");
        }
        require_direct_directory(&self.path)?;
        if !ownership_matches_build(&self.path, &self.record) {
            bail!("kernel generation directory ownership changed before publication");
        }
        write_process_lease(&self.path, LeaseKind::Resolver, ProcessOwner::current())?;
        let marker = PublishedRecord {
            version: RECORD_VERSION,
            generation: self.record.generation.clone(),
            identity: self.record.identity.clone(),
            created_at_ms: self.record.created_at_ms,
            token: self.record.token.clone(),
            directory_identity: self.record.directory_identity.clone(),
            root_identity: self.record.root_identity.clone(),
        };
        write_json_new(&self.path.join(PUBLISHED_FILE), &marker)?;
        remove_file_if_unchanged(&self.sidecar, &self.sidecar_raw);
        Ok(())
    }

    #[cfg(test)]
    fn sidecar(&self) -> &Path {
        &self.sidecar
    }
}

/// A child/handoff lease. Drop removes only the exact record this guard wrote;
/// a replaced or unreadable path is retained conservatively.
pub(crate) struct GenerationLeaseGuard {
    path: PathBuf,
    raw: Vec<u8>,
}

impl GenerationLeaseGuard {
    /// Release only after the caller observed the protected process exit or
    /// proved that no child was spawned. Dropping a cancelled watcher retains
    /// the lease conservatively.
    pub(crate) fn release(self) {
        remove_file_if_unchanged(&self.path, &self.raw);
    }
}

pub(in crate::kernel::bootstrap) fn generation_root(base_venv: &Path) -> anyhow::Result<PathBuf> {
    let name = base_venv.file_name().ok_or_else(|| {
        anyhow!(
            "kernel venv path has no final component: {}",
            base_venv.display()
        )
    })?;
    let mut generated = name.to_os_string();
    generated.push(".generations");
    Ok(base_venv.with_file_name(generated))
}

pub(in crate::kernel::bootstrap) fn generation_identity(
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<String> {
    let identity = serde_json::json!({
        "schema": super::version::BOOTSTRAP_SCHEMA,
        "python": PYTHON_VERSION,
        "runtime": runtime_identity,
        "snapshot": super::version::STATE_SNAPSHOT_REQUIREMENT,
        "extra_uv_args": default_rlm_extra_uv_args(),
        "python_skills": python_skills,
    });
    let bytes = serde_json::to_vec(&identity)?;
    let digest = format!("{:x}", sha2::Sha256::digest(bytes));
    Ok(digest[..IDENTITY_HEX_CHARS].to_string())
}

/// Marker-valid generations for this exact environment identity, newest first.
pub(in crate::kernel::bootstrap) fn published_candidates(
    root: &Path,
    identity: &str,
) -> anyhow::Result<Vec<PathBuf>> {
    ensure_generation_root(root)?;
    let Some(root_identity) = directory_identity(root)? else {
        bail!(
            "filesystem cannot prove kernel generation root identity: {}",
            root.display()
        );
    };
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(parsed) = parse_generation_name(&entry.file_name()) else {
            continue;
        };
        if parsed.identity != identity {
            continue;
        }
        let marker_path = entry.path().join(PUBLISHED_FILE);
        let RecordState::Valid { value, .. } = read_record::<PublishedRecord>(&marker_path) else {
            continue;
        };
        if valid_published_record(&value, &entry.file_name(), &parsed)
            && ownership_matches_published(&entry.path(), &value)
        {
            candidates.push((value.created_at_ms, entry.path()));
        }
    }
    if directory_identity(root)?.as_ref() != Some(&root_identity) {
        bail!("kernel generation root identity changed during discovery");
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    Ok(candidates.into_iter().map(|(_, path)| path).collect())
}

/// Put a fresh host resolver lease on a selected published generation. Must
/// run while the caller holds the generation-root bootstrap lock.
pub(in crate::kernel::bootstrap) fn lease_for_resolver(generation: &Path) -> anyhow::Result<()> {
    require_valid_published_generation(generation)?;
    require_direct_directory(&generation.join(LEASE_DIR))?;
    write_process_lease(generation, LeaseKind::Resolver, ProcessOwner::current())?;
    Ok(())
}

/// Protect the resolve-to-spawn window. A dead handoff holder remains
/// conservative/unknown: a reparented interpreter may exist without a child
/// lease if the host crashed in that window.
pub(crate) fn begin_kernel_lease_handoff(
    python: &Path,
) -> anyhow::Result<Option<GenerationLeaseGuard>> {
    lease_python_process(python, std::process::id(), LeaseKind::Handoff)
}

/// Lease the generation to the actual kernel PID. Non-managed interpreters do
/// not participate and return `None`.
pub(crate) fn register_kernel_python_lease(
    python: &Path,
    pid: u32,
) -> anyhow::Result<Option<GenerationLeaseGuard>> {
    if pid == 0 {
        bail!("cannot lease a kernel generation to pid 0");
    }
    lease_python_process(python, pid, LeaseKind::Kernel)
}

fn lease_python_process(
    python: &Path,
    pid: u32,
    kind: LeaseKind,
) -> anyhow::Result<Option<GenerationLeaseGuard>> {
    let Some(generation) = python.parent().and_then(Path::parent) else {
        return Ok(None);
    };
    if parse_generation_name(generation.file_name().unwrap_or_default()).is_none() {
        return Ok(None);
    }
    let expected = kernel_venv_python(generation);
    if expected != python {
        return Ok(None);
    }
    require_valid_published_generation(generation)?;
    require_direct_directory(&generation.join(LEASE_DIR))?;
    let owner = ProcessOwner {
        pid,
        process_start_id: pa_types::platform::process::process_start_id(pid),
    };
    let (path, raw) = write_process_lease(generation, kind, owner)?;
    Ok(Some(GenerationLeaseGuard { path, raw }))
}

/// Classify managed generations under the root lock. Inactive, abandoned,
/// malformed, unknown, and live directories are retained; no recursive GC runs.
pub(in crate::kernel::bootstrap) fn prepare_generation_root(root: &Path) -> anyhow::Result<()> {
    ensure_generation_root(root)?;
    let Some(root_identity) = directory_identity(root)? else {
        bail!(
            "filesystem cannot prove kernel generation root identity: {}",
            root.display()
        );
    };
    retain_abandoned_builds(root, &root_identity)?;
    let mut inactive_count = 0usize;
    for entry in std::fs::read_dir(root)? {
        if directory_identity(root)?.as_ref() != Some(&root_identity) {
            bail!("kernel generation root identity changed during retention scan");
        }
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(parsed) = parse_generation_name(&entry.file_name()) else {
            continue;
        };
        let marker_path = entry.path().join(PUBLISHED_FILE);
        let RecordState::Valid { value, .. } = read_record::<PublishedRecord>(&marker_path) else {
            continue;
        };
        if valid_published_record(&value, &entry.file_name(), &parsed)
            && ownership_matches_published(&entry.path(), &value)
            && generation_use(&entry.path()) == GenerationUse::Inactive
        {
            inactive_count += 1;
        }
    }
    if inactive_count > 0 {
        tracing::warn!(
            inactive_count,
            "inactive kernel generations retained; automatic recursive deletion is disabled until an identity-bound maintenance path exists"
        );
    }
    Ok(())
}

fn retain_abandoned_builds(root: &Path, root_identity: &DirectoryIdentity) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        if directory_identity(root)?.as_ref() != Some(root_identity) {
            bail!("kernel generation root identity changed during build retention");
        }
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_file()
            || !name.starts_with(BUILD_RECORD_PREFIX)
            || !name.ends_with(".json")
        {
            continue;
        }
        let RecordState::Valid { value, raw } = read_record::<BuildRecord>(&entry.path()) else {
            continue;
        };
        if value.version != RECORD_VERSION || value.root_identity != *root_identity {
            continue;
        }
        let Some(parsed) = parse_generation_name(OsStr::new(&value.generation)) else {
            continue;
        };
        if parsed.identity != value.identity || parsed.created_at_ms != value.created_at_ms {
            continue;
        }
        let generation = root.join(&value.generation);
        match read_record::<PublishedRecord>(&generation.join(PUBLISHED_FILE)) {
            RecordState::Valid { value: marker, .. }
                if valid_published_record(&marker, OsStr::new(&value.generation), &parsed)
                    && ownership_matches_build(&generation, &value) =>
            {
                remove_file_if_unchanged(&entry.path(), &raw);
            }
            RecordState::Missing if holder_state(&value.owner) == HolderState::Dead => {
                match symlink_metadata_state(&generation)? {
                    None => remove_file_if_unchanged(&entry.path(), &raw),
                    Some(metadata)
                        if metadata.file_type().is_dir()
                            && !metadata.file_type().is_symlink()
                            && directory_matches_build(&generation, &value) =>
                    {
                        tracing::warn!(
                            "abandoned kernel generation retained; automatic recursive deletion is disabled"
                        );
                    }
                    Some(_) => {}
                }
            }
            RecordState::Missing | RecordState::Valid { .. } | RecordState::Unknown => {}
        }
    }
    Ok(())
}

fn generation_use(generation: &Path) -> GenerationUse {
    let Ok(Some(current_identity)) = directory_identity(generation) else {
        return GenerationUse::Protected;
    };
    let Some(current_root_identity) = generation
        .parent()
        .and_then(|root| directory_identity(root).ok().flatten())
    else {
        return GenerationUse::Protected;
    };
    let lease_dir = generation.join(LEASE_DIR);
    let Ok(Some(metadata)) = symlink_metadata_state(&lease_dir) else {
        return GenerationUse::Protected;
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return GenerationUse::Protected;
    }
    let Ok(entries) = std::fs::read_dir(&lease_dir) else {
        return GenerationUse::Protected;
    };
    let mut leases = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return GenerationUse::Protected;
        };
        let Ok(kind) = entry.file_type() else {
            return GenerationUse::Protected;
        };
        if !kind.is_file() {
            return GenerationUse::Protected;
        }
        let RecordState::Valid { value, .. } = read_record::<LeaseRecord>(&entry.path()) else {
            return GenerationUse::Protected;
        };
        if value.version != RECORD_VERSION
            || value.directory_identity.as_ref() != Some(&current_identity)
            || value.root_identity.as_ref() != Some(&current_root_identity)
        {
            return GenerationUse::Protected;
        }
        leases.push(value);
    }
    if leases.is_empty() {
        return GenerationUse::Protected;
    }
    let child_lease_was_durable = leases.iter().any(|lease| lease.kind == LeaseKind::Kernel);
    for lease in leases {
        match holder_state(&lease.owner) {
            HolderState::Live | HolderState::Unknown => return GenerationUse::Protected,
            // A dead host in the resolve->child window is unknowable until a
            // finalized child lease proves the handoff reached durability.
            HolderState::Dead if lease.kind == LeaseKind::Handoff && !child_lease_was_durable => {
                return GenerationUse::Protected;
            }
            HolderState::Dead => {}
        }
    }
    GenerationUse::Inactive
}

fn holder_state(owner: &ProcessOwner) -> HolderState {
    match pa_types::platform::process::is_process_alive(owner.pid) {
        Ok(false) => HolderState::Dead,
        Err(_) => HolderState::Unknown,
        Ok(true) => match (
            owner.process_start_id.as_deref(),
            pa_types::platform::process::process_start_id(owner.pid).as_deref(),
        ) {
            (Some(recorded), Some(current)) if recorded == current => HolderState::Live,
            (Some(_), Some(_)) => HolderState::Dead,
            (None, _) | (_, None) => HolderState::Unknown,
        },
    }
}

fn write_process_lease(
    generation: &Path,
    kind: LeaseKind,
    owner: ProcessOwner,
) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    let lease_dir = generation.join(LEASE_DIR);
    ensure_direct_private_directory(&lease_dir)?;
    let path = lease_dir.join(format!(
        "{}-{}-{}.json",
        kind.label(),
        owner.pid,
        uuid::Uuid::new_v4()
    ));
    let Some(generation_identity) = directory_identity(generation)? else {
        bail!(
            "filesystem cannot prove kernel generation directory identity: {}",
            generation.display()
        );
    };
    let Some(root_identity) = generation
        .parent()
        .and_then(|root| directory_identity(root).ok().flatten())
    else {
        bail!("filesystem cannot prove kernel generation root identity");
    };
    let record = LeaseRecord {
        version: RECORD_VERSION,
        kind,
        created_at_ms: now_millis(),
        directory_identity: Some(generation_identity),
        root_identity: Some(root_identity),
        owner,
    };
    let raw = write_json_new(&path, &record)?;
    Ok((path, raw))
}

fn require_valid_published_generation(generation: &Path) -> anyhow::Result<PublishedRecord> {
    require_direct_directory(generation)?;
    let name = generation.file_name().unwrap_or_default();
    let parsed = parse_generation_name(name)
        .ok_or_else(|| anyhow!("not a managed kernel generation: {}", generation.display()))?;
    let marker = match read_record::<PublishedRecord>(&generation.join(PUBLISHED_FILE)) {
        RecordState::Valid { value, .. } => value,
        RecordState::Missing => bail!(
            "kernel generation is not published: {}",
            generation.display()
        ),
        RecordState::Unknown => bail!(
            "kernel generation marker is unreadable: {}",
            generation.display()
        ),
    };
    if !valid_published_record(&marker, name, &parsed)
        || !ownership_matches_published(generation, &marker)
    {
        bail!(
            "kernel generation markers do not own {}",
            generation.display()
        );
    }
    Ok(marker)
}

fn directory_matches_build(generation: &Path, build: &BuildRecord) -> bool {
    directory_identity(generation).ok().flatten().as_ref() == Some(&build.directory_identity)
}

fn ownership_matches_build(generation: &Path, build: &BuildRecord) -> bool {
    let RecordState::Valid { value, .. } =
        read_record::<OwnershipRecord>(&generation.join(OWNER_FILE))
    else {
        return false;
    };
    value.version == RECORD_VERSION
        && value.generation == build.generation
        && value.identity == build.identity
        && value.token == build.token
        && value.directory_identity == build.directory_identity
        && value.root_identity == build.root_identity
        && generation
            .parent()
            .and_then(|root| directory_identity(root).ok().flatten())
            .as_ref()
            == Some(&build.root_identity)
        && directory_matches_build(generation, build)
        && uuid::Uuid::parse_str(&value.token).is_ok()
}

fn ownership_matches_published(generation: &Path, published: &PublishedRecord) -> bool {
    let RecordState::Valid { value, .. } =
        read_record::<OwnershipRecord>(&generation.join(OWNER_FILE))
    else {
        return false;
    };
    value.version == RECORD_VERSION
        && value.generation == published.generation
        && value.identity == published.identity
        && value.token == published.token
        && value.directory_identity == published.directory_identity
        && value.root_identity == published.root_identity
        && generation
            .parent()
            .and_then(|root| directory_identity(root).ok().flatten())
            .as_ref()
            == Some(&published.root_identity)
        && directory_identity(generation).ok().flatten().as_ref()
            == Some(&published.directory_identity)
        && uuid::Uuid::parse_str(&value.token).is_ok()
}

fn valid_published_record(
    record: &PublishedRecord,
    name: &OsStr,
    parsed: &ParsedGenerationName,
) -> bool {
    record.version == RECORD_VERSION
        && OsStr::new(&record.generation) == name
        && record.identity == parsed.identity
        && record.created_at_ms == parsed.created_at_ms
        && uuid::Uuid::parse_str(&record.token).is_ok()
}

fn parse_generation_name(name: &OsStr) -> Option<ParsedGenerationName> {
    let name = name.to_str()?;
    let rest = name.strip_prefix(GENERATION_PREFIX)?;
    let (identity, rest) = rest.split_once('-')?;
    if identity.len() != IDENTITY_HEX_CHARS
        || !identity.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let (created, nonce) = rest.split_once('-')?;
    if created.len() != 20 || !created.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    uuid::Uuid::parse_str(nonce).ok()?;
    Some(ParsedGenerationName {
        identity: identity.to_ascii_lowercase(),
        created_at_ms: created.parse().ok()?,
    })
}

fn now_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn ensure_generation_root(root: &Path) -> anyhow::Result<()> {
    match symlink_metadata_state(root)? {
        Some(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Some(_) => bail!(
            "managed kernel generation root is not a direct directory: {}",
            root.display()
        ),
        None => {
            crate::platform::perms::create_dir_all_private(root)
                .with_context(|| format!("creating kernel generation root {}", root.display()))?;
            require_direct_directory(root)
        }
    }
}

fn ensure_direct_private_directory(path: &Path) -> anyhow::Result<()> {
    match symlink_metadata_state(path)? {
        Some(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Some(_) => bail!("refusing non-directory or symlink path: {}", path.display()),
        None => {
            crate::platform::perms::create_dir_all_private(path)?;
            require_direct_directory(path)
        }
    }
}

fn require_direct_directory(path: &Path) -> anyhow::Result<()> {
    match symlink_metadata_state(path)? {
        Some(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Some(_) => bail!("refusing non-directory or symlink path: {}", path.display()),
        None => bail!("directory does not exist: {}", path.display()),
    }
}

fn symlink_metadata_state(path: &Path) -> anyhow::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading metadata for {}", path.display()))
        }
    }
}

fn write_json_new<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<Vec<u8>> {
    let mut raw = serde_json::to_vec(value)?;
    raw.push(b'\n');
    write_private_new(path, &raw)?;
    Ok(raw)
}

fn write_private_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    require_direct_directory(parent)?;
    if symlink_metadata_state(path)?.is_some() {
        bail!(
            "refusing to replace existing ownership record: {}",
            path.display()
        );
    }
    let temp = parent.join(format!(
        ".{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file); // Windows cannot rename an open file.
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("publishing ownership record {}", path.display()))
}

fn read_record<T: DeserializeOwned>(path: &Path) -> RecordState<T> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RecordState::Missing;
        }
        Err(_) => return RecordState::Unknown,
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_RECORD_BYTES
    {
        return RecordState::Unknown;
    }
    let raw = match std::fs::read(path) {
        Ok(raw) if raw.len() as u64 <= MAX_RECORD_BYTES => raw,
        Ok(_) | Err(_) => return RecordState::Unknown,
    };
    match serde_json::from_slice(&raw) {
        Ok(value) => RecordState::Valid { value, raw },
        Err(_) => RecordState::Unknown,
    }
}

fn remove_file_if_unchanged(path: &Path, expected: &[u8]) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return;
    }
    if std::fs::read(path).is_ok_and(|raw| raw == expected) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(byte: char) -> String {
        std::iter::repeat_n(byte, IDENTITY_HEX_CHARS).collect()
    }

    fn fixture_generation(
        root: &Path,
        identity: &str,
        created_at_ms: u64,
        leases: &[LeaseRecord],
    ) -> PathBuf {
        ensure_generation_root(root).unwrap();
        let root_identity = directory_identity(root).unwrap().unwrap();
        let name = format!(
            "{GENERATION_PREFIX}{identity}-{created_at_ms:020}-{}",
            uuid::Uuid::new_v4()
        );
        let path = root.join(&name);
        let token = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir(&path).unwrap();
        let directory_identity = directory_identity(&path).unwrap().unwrap();
        write_json_new(
            &path.join(OWNER_FILE),
            &OwnershipRecord {
                version: RECORD_VERSION,
                generation: name.clone(),
                identity: identity.to_string(),
                token: token.clone(),
                directory_identity: directory_identity.clone(),
                root_identity: root_identity.clone(),
            },
        )
        .unwrap();
        let lease_dir = path.join(LEASE_DIR);
        std::fs::create_dir(&lease_dir).unwrap();
        for (index, lease) in leases.iter().enumerate() {
            let mut lease = lease.clone();
            lease.directory_identity = Some(directory_identity.clone());
            lease.root_identity = Some(root_identity.clone());
            write_json_new(&lease_dir.join(format!("lease-{index}.json")), &lease).unwrap();
        }
        write_json_new(
            &path.join(PUBLISHED_FILE),
            &PublishedRecord {
                version: RECORD_VERSION,
                generation: name,
                identity: identity.to_string(),
                created_at_ms,
                token,
                directory_identity,
                root_identity,
            },
        )
        .unwrap();
        path
    }

    fn lease(kind: LeaseKind, owner: ProcessOwner) -> LeaseRecord {
        LeaseRecord {
            version: RECORD_VERSION,
            kind,
            created_at_ms: now_millis(),
            directory_identity: None,
            root_identity: None,
            owner,
        }
    }

    fn dead_owner() -> ProcessOwner {
        ProcessOwner {
            pid: u32::MAX,
            process_start_id: Some("dead".to_string()),
        }
    }

    #[test]
    fn start_identity_mismatch_is_dead_but_unknown_identity_is_protected() {
        let pid = std::process::id();
        let current = pa_types::platform::process::process_start_id(pid);
        if let Some(current) = current {
            assert_eq!(
                holder_state(&ProcessOwner {
                    pid,
                    process_start_id: Some(current),
                }),
                HolderState::Live
            );
            assert_eq!(
                holder_state(&ProcessOwner {
                    pid,
                    process_start_id: Some("definitely-not-current".to_string()),
                }),
                HolderState::Dead
            );
        }
        assert_eq!(
            holder_state(&ProcessOwner {
                pid,
                process_start_id: None,
            }),
            HolderState::Unknown
        );
        assert_eq!(holder_state(&dead_owner()), HolderState::Dead);
    }

    #[test]
    fn inactive_generations_are_never_automatically_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        // Identity hash order intentionally opposes chronology: retention
        // must keep the newest timestamp, never the lexicographically largest hash.
        let old = fixture_generation(
            &root,
            &identity('f'),
            1,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        let newest = fixture_generation(
            &root,
            &identity('a'),
            2,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        let live = fixture_generation(
            &root,
            &identity('b'),
            3,
            &[lease(LeaseKind::Kernel, ProcessOwner::current())],
        );
        let malformed = fixture_generation(
            &root,
            &identity('c'),
            4,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        std::fs::write(malformed.join(LEASE_DIR).join("bad.json"), "not-json").unwrap();
        let changed_owner = fixture_generation(
            &root,
            &identity('d'),
            5,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        let changed_name = changed_owner
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let changed_directory_identity = directory_identity(&changed_owner).unwrap().unwrap();
        let changed_root_identity = directory_identity(&root).unwrap().unwrap();
        write_json_new(
            &changed_owner.join(OWNER_FILE),
            &OwnershipRecord {
                version: RECORD_VERSION,
                generation: changed_name,
                identity: identity('d'),
                token: uuid::Uuid::new_v4().to_string(),
                directory_identity: changed_directory_identity.clone(),
                root_identity: changed_root_identity.clone(),
            },
        )
        .unwrap_err(); // atomic ownership records are never overwritten
        std::fs::write(
            changed_owner.join(OWNER_FILE),
            serde_json::to_vec(&OwnershipRecord {
                version: RECORD_VERSION,
                generation: changed_owner
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                identity: identity('d'),
                token: uuid::Uuid::new_v4().to_string(),
                directory_identity: changed_directory_identity,
                root_identity: changed_root_identity,
            })
            .unwrap(),
        )
        .unwrap();

        prepare_generation_root(&root).unwrap();
        for inactive in [&old, &newest] {
            assert!(inactive.exists(), "inactive generation is retained on disk");
        }
        assert!(live.exists(), "live generation is retained");
        assert!(malformed.exists(), "unknown lease ownership is retained");
        assert!(
            changed_owner.exists(),
            "changed directory ownership is retained"
        );
    }

    #[test]
    fn dead_handoff_lease_is_conservatively_retained() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let generation = fixture_generation(
            &root,
            &identity('d'),
            1,
            &[lease(LeaseKind::Handoff, dead_owner())],
        );
        assert_eq!(generation_use(&generation), GenerationUse::Protected);
        prepare_generation_root(&root).unwrap();
        assert!(generation.exists());

        let completed = fixture_generation(
            &root,
            &identity('e'),
            2,
            &[
                lease(LeaseKind::Handoff, dead_owner()),
                lease(LeaseKind::Kernel, dead_owner()),
            ],
        );
        assert_eq!(generation_use(&completed), GenerationUse::Inactive);
    }

    #[test]
    fn strictly_named_directory_without_a_build_record_is_retained_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        ensure_generation_root(&root).unwrap();
        let unknown = root.join(format!(
            "{GENERATION_PREFIX}{}-{:020}-{}",
            identity('a'),
            1,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("foreign"), "keep").unwrap();
        prepare_generation_root(&root).unwrap();
        assert_eq!(
            std::fs::read_to_string(unknown.join("foreign")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn generation_begin_refuses_a_preexisting_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        ensure_generation_root(&root).unwrap();
        let nonce = uuid::Uuid::new_v4();
        let name = format!("{GENERATION_PREFIX}{}-{:020}-{nonce}", identity('a'), 7);
        let target = root.join(name);
        std::fs::create_dir(&target).unwrap();
        let sentinel = target.join("foreign");
        std::fs::write(&sentinel, "keep").unwrap();
        assert!(GenerationBuild::begin_at(&root, &identity('a'), 7, nonce).is_err());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[test]
    fn generation_root_replacement_aborts_claim_and_publication() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let build = GenerationBuild::begin(&root, &identity('a')).unwrap();
        let displaced = temp.path().join("displaced-root");
        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(build.claim_created_path().is_err());
        assert!(build.publish().is_err());
        assert!(displaced.join(&build.record.generation).exists());
    }

    #[test]
    fn identity_mismatch_after_uv_never_claims_or_publishes_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let build = GenerationBuild::begin(&root, &identity('b')).unwrap();
        let original = root.join("original-owned-directory");
        std::fs::rename(build.path(), &original).unwrap();
        std::fs::create_dir(build.path()).unwrap();
        let foreign = build.path().join("foreign");
        std::fs::write(&foreign, "keep").unwrap();
        assert!(build.claim_created_path().is_err());
        assert!(build.publish().is_err());
        build.discard();
        assert_eq!(std::fs::read_to_string(foreign).unwrap(), "keep");
        assert!(!build.path().join(PUBLISHED_FILE).exists());
    }

    #[test]
    fn retention_never_deletes_a_swapped_or_recreated_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let owned = fixture_generation(
            &root,
            &identity('a'),
            1,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        let displaced = root.join("displaced-owned");
        std::fs::rename(&owned, &displaced).unwrap();
        std::fs::create_dir(&owned).unwrap();
        let replacement = owned.join("replacement");
        std::fs::write(&replacement, "keep").unwrap();

        prepare_generation_root(&root).unwrap();
        assert_eq!(std::fs::read_to_string(replacement).unwrap(), "keep");
        assert!(
            displaced.exists(),
            "the recorded owned directory is retained"
        );
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".generation-trash-")),
            "retain-only cleanup never moves a candidate into a raceable trash namespace"
        );
    }

    #[test]
    fn crashed_build_retention_requires_valid_dead_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let no_target = GenerationBuild::begin(&root, &identity('b')).unwrap();
        std::fs::remove_dir(no_target.path()).unwrap();
        no_target.discard();
        assert!(
            !no_target.sidecar().exists(),
            "a failed uv command that created no target drops its exact sidecar"
        );
        let build = GenerationBuild::begin(&root, &identity('e')).unwrap();
        build.claim_created_path().unwrap();
        std::fs::write(
            build.path().join(".bootstrap-version"),
            "validated-but-private",
        )
        .unwrap();
        assert!(published_candidates(&root, &identity('e'))
            .unwrap()
            .is_empty());

        prepare_generation_root(&root).unwrap();
        assert!(build.path().exists(), "live builder remains");

        let mut record = build.record.clone();
        record.owner = dead_owner();
        std::fs::write(
            build.sidecar(),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        prepare_generation_root(&root).unwrap();
        assert!(
            build.path().exists(),
            "dead app-owned private build is retained"
        );
        assert!(build.sidecar().exists());

        let unknown = GenerationBuild::begin(&root, &identity('f')).unwrap();
        unknown.claim_created_path().unwrap();
        std::fs::write(unknown.sidecar(), "malformed").unwrap();
        prepare_generation_root(&root).unwrap();
        assert!(unknown.path().exists(), "malformed ownership is retained");

        let unclaimed = GenerationBuild::begin(&root, &identity('a')).unwrap();
        let mut record = unclaimed.record.clone();
        record.owner = dead_owner();
        std::fs::write(
            unclaimed.sidecar(),
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        prepare_generation_root(&root).unwrap();
        assert!(
            unclaimed.path().exists(),
            "pre-uv owned directory is retained safely"
        );
        assert!(unclaimed.sidecar().exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_generation_root_symlink_is_refused_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let root = temp.path().join("kernel-venv.generations");
        symlink(&outside, &root).unwrap();
        let error = prepare_generation_root(&root).unwrap_err();
        assert!(
            error.to_string().contains("not a direct directory"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn pruning_never_follows_a_generation_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        ensure_generation_root(&root).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let name = format!(
            "{GENERATION_PREFIX}{}-{:020}-{}",
            identity('a'),
            1,
            uuid::Uuid::new_v4()
        );
        symlink(&outside, root.join(name)).unwrap();
        prepare_generation_root(&root).unwrap();
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[test]
    fn resolver_handoff_stays_protected_until_the_child_lease_is_durable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let generation = fixture_generation(
            &root,
            &identity('a'),
            1,
            &[lease(LeaseKind::Resolver, dead_owner())],
        );
        let python = kernel_venv_python(&generation);
        if let Some(parent) = python.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&python, "").unwrap();

        let handoff = begin_kernel_lease_handoff(&python).unwrap().unwrap();
        assert_eq!(generation_use(&generation), GenerationUse::Protected);
        let child = register_kernel_python_lease(&python, std::process::id())
            .unwrap()
            .unwrap();
        handoff.release();
        assert_eq!(generation_use(&generation), GenerationUse::Protected);
        child.release();
        assert_eq!(generation_use(&generation), GenerationUse::Inactive);
    }

    #[test]
    fn child_lease_guard_removes_only_its_unchanged_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let generation = fixture_generation(
            &root,
            &identity('a'),
            1,
            &[lease(LeaseKind::Resolver, ProcessOwner::current())],
        );
        let python = kernel_venv_python(&generation);
        if let Some(parent) = python.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&python, "").unwrap();
        let guard = register_kernel_python_lease(&python, std::process::id())
            .unwrap()
            .unwrap();
        let lease_path = guard.path.clone();
        assert!(lease_path.exists());
        guard.release();
        assert!(!lease_path.exists());
    }

    #[test]
    fn dropping_a_cancelled_watcher_guard_retains_the_child_lease() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kernel-venv.generations");
        let generation = fixture_generation(
            &root,
            &identity('a'),
            1,
            &[lease(LeaseKind::Resolver, ProcessOwner::current())],
        );
        let python = kernel_venv_python(&generation);
        if let Some(parent) = python.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&python, "").unwrap();
        let guard = register_kernel_python_lease(&python, std::process::id())
            .unwrap()
            .unwrap();
        let lease_path = guard.path.clone();
        drop(guard);
        assert!(
            lease_path.exists(),
            "task cancellation cannot assert child death"
        );
    }
}
