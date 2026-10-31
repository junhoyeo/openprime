//! The exact `PRIME_AGENT_KERNEL_VENV` build record: a sidecar beside the
//! caller's path (uv refuses a non-empty target, so nothing may land inside
//! it first) that proves an unpublished directory is this product's own
//! build. Abandoned builds move aside and are retained, never deleted.

use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::{Deserialize, Serialize};

use super::{
    directory_identity, holder_state, read_record, remove_file_if_unchanged,
    symlink_metadata_state, write_json_new, DirectoryIdentity, HolderState, ProcessOwner,
    RecordState,
};

const BUILD_RECORD_SUFFIX: &str = ".prime-agent-build.json";
const ABANDONED_INFIX: &str = ".abandoned-";
const EXPLICIT_BUILD_VERSION: u64 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExplicitBuildRecord {
    version: u64,
    token: String,
    directory_identity: DirectoryIdentity,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    parent_identity: Option<DirectoryIdentity>,
    owner: ProcessOwner,
}

fn sibling(venv: &Path, suffix: &str) -> PathBuf {
    venv.with_file_name(format!(
        "{}{suffix}",
        venv.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn record_path(venv: &Path) -> PathBuf {
    sibling(venv, BUILD_RECORD_SUFFIX)
}

fn trusted_build_record(venv: &Path) -> RecordState<ExplicitBuildRecord> {
    let path = record_path(venv);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return RecordState::Missing,
        Err(_) => return RecordState::Unknown,
    };
    if !metadata.is_file() || metadata.file_type().is_symlink()
        || !crate::platform::perms::is_private_owner_metadata(&metadata)
    {
        return RecordState::Unknown;
    }
    match read_record::<ExplicitBuildRecord>(&path) {
        RecordState::Valid { value, raw }
            if value.version == EXPLICIT_BUILD_VERSION
                && uuid::Uuid::parse_str(&value.token).is_ok()
                && super::canonical_explicit_path(venv).ok().as_ref() == value.path.as_ref()
                && value.path.is_some()
                && venv.parent().and_then(|parent| directory_identity(parent).ok().flatten()) == value.parent_identity
                && value.parent_identity.is_some()
                && holds_recorded_directory(venv, &value) => RecordState::Valid { value, raw },
        RecordState::Missing | RecordState::Unknown | RecordState::Valid { .. } => RecordState::Unknown,
    }
}

/// One attempt's claim on the exact path it just created. Dropped before
/// [`ExplicitBuild::publish`] (failure, or the runtime cancelling the
/// prewarm), it moves its own directory aside: one identity-checked rename.
pub(in crate::kernel::bootstrap) struct ExplicitBuild {
    venv: PathBuf,
    record: ExplicitBuildRecord,
    raw: Vec<u8>,
    published: bool,
}

impl ExplicitBuild {
    /// Record ownership of the directory this attempt just created, before
    /// anything else is written for it.
    pub(in crate::kernel::bootstrap) fn claim(venv: &Path) -> anyhow::Result<Self> {
        crate::platform::perms::require_private_owner_metadata_support()?;
        let parent = venv.parent().ok_or_else(|| anyhow::anyhow!("explicit path has no parent"))?;
        let Some(parent_identity) = directory_identity(parent)? else {
            bail!("filesystem cannot prove explicit build parent identity");
        };
        let Some(directory_identity) = directory_identity(venv)? else {
            bail!(
                "filesystem cannot prove PRIME_AGENT_KERNEL_VENV directory identity at {}",
                venv.display()
            );
        };
        let record = ExplicitBuildRecord {
            version: EXPLICIT_BUILD_VERSION,
            token: uuid::Uuid::new_v4().to_string(),
            directory_identity,
            path: Some(super::canonical_explicit_path(venv)?),
            parent_identity: Some(parent_identity),
            owner: ProcessOwner::current(),
        };
        let raw = write_json_new(&record_path(venv), &record)?;
        Ok(Self {
            venv: venv.to_path_buf(),
            record,
            raw,
            published: false,
        })
    }

    /// A pending build record prevents the legacy-current fast path. Even a
    /// valid version file may belong to a still-live unpublished builder.
    pub(in crate::kernel::bootstrap) fn pending(venv: &Path) -> bool {
        !matches!(read_record::<ExplicitBuildRecord>(&record_path(venv)), RecordState::Missing)
    }

    /// Finish a dead builder's publication only after the caller validates
    /// the actual interpreter and exact requested version under the lock.
    pub(in crate::kernel::bootstrap) fn finish_interrupted_publication(venv: &Path) -> anyhow::Result<()> {
        let (record, raw) = match trusted_build_record(venv) {
            RecordState::Missing => return Ok(()),
            RecordState::Valid { value, raw }
                if holder_state(&value.owner) == HolderState::Dead
                    && holds_recorded_directory(venv, &value) => (value, raw),
            RecordState::Valid { .. } | RecordState::Unknown => {
                bail!("explicit kernel build ownership is live or unknown; retained without publication");
            }
        };
        Self { venv: venv.to_path_buf(), record, raw, published: false }.publish()
    }

    /// Whether the exact path still is the directory this attempt created.
    pub(in crate::kernel::bootstrap) fn owns_path(&self) -> bool {
        holds_recorded_directory(&self.venv, &self.record)
            && matches!(trusted_build_record(&self.venv),
                RecordState::Valid { value, raw } if value == self.record && raw == self.raw)
    }

    /// The build validated: the record is no longer needed.
    pub(in crate::kernel::bootstrap) fn publish(mut self) -> anyhow::Result<()> {
        if !self.owns_path() {
            bail!("explicit kernel ownership changed before publication");
        }
        super::write_process_lease(&self.venv, super::LeaseKind::Resolver, ProcessOwner::current())?;
        super::explicit_owner::ExplicitOwnership::publish(&self.venv, &self.record.token)?;
        self.published = true;
        remove_file_if_unchanged(&record_path(&self.venv), &self.raw);
        Ok(())
    }
}

impl Drop for ExplicitBuild {
    fn drop(&mut self) {
        if self.published || !self.owns_path() {
            return;
        }
        // A failed rename (Windows: a killed uv may still hold handles)
        // keeps the record, so a later run reclaims the directory once this
        // owner is dead.
        if set_aside(&self.venv, &self.record).is_none()
            && holds_recorded_directory(&self.venv, &self.record)
        {
            return;
        }
        remove_file_if_unchanged(&record_path(&self.venv), &self.raw);
    }
}

fn holds_recorded_directory(venv: &Path, record: &ExplicitBuildRecord) -> bool {
    matches!(
        symlink_metadata_state(venv),
        Ok(Some(metadata)) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
    ) && directory_identity(venv).ok().flatten().as_ref() == Some(&record.directory_identity)
}

/// Rename the recorded directory to `<venv>.abandoned-<token>`; anything
/// else at the path stays untouched.
fn set_aside(venv: &Path, record: &ExplicitBuildRecord) -> Option<PathBuf> {
    if !holds_recorded_directory(venv, record) {
        return None;
    }
    let aside = sibling(venv, &format!("{ABANDONED_INFIX}{}", record.token));
    if !matches!(symlink_metadata_state(&aside), Ok(None)) {
        return None;
    }
    std::fs::rename(venv, &aside).ok()?;
    tracing::warn!(
        path = %aside.display(),
        "abandoned kernel build retained; automatic recursive deletion is disabled"
    );
    Some(aside)
}

/// What [`reclaim_explicit_path`] did to make the exact path buildable.
pub(in crate::kernel::bootstrap) enum ExplicitReclaim {
    Untouched,
    RemovedEmpty,
    SetAside(PathBuf),
}

/// Under the bootstrap lock, for a non-current exact path: an empty real
/// directory is removed with a non-recursive rmdir; this product's own
/// unpublished build whose owner is dead moves aside, retained. Anything
/// else (no or unreadable record, live or unknowable owner, another
/// directory, a symlink, a path it cannot inspect) stays untouched, so the
/// caller's refusal reports it.
pub(in crate::kernel::bootstrap) fn reclaim_explicit_path(venv: &Path) -> ExplicitReclaim {
    let record = match trusted_build_record(venv) {
        RecordState::Valid { value, raw }
            if holder_state(&value.owner) == HolderState::Dead =>
        {
            Some((value, raw))
        }
        _ => None,
    };
    if record.is_none() && ExplicitBuild::pending(venv) {
        return ExplicitReclaim::Untouched;
    }
    let outcome = match symlink_metadata_state(venv) {
        Ok(Some(metadata))
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
        {
            let Ok(mut entries) = std::fs::read_dir(venv) else {
                return ExplicitReclaim::Untouched;
            };
            if entries.next().is_none() {
                // Fails, and leaves the path alone, if anything appeared.
                if std::fs::remove_dir(venv).is_err() {
                    return ExplicitReclaim::Untouched;
                }
                ExplicitReclaim::RemovedEmpty
            } else {
                // A written version file marks a build that got as far as
                // publication: a stale environment, never an abandoned one.
                let unpublished = super::super::read_bootstrap_version(venv).is_none();
                match record.as_ref().filter(|_| unpublished) {
                    Some((value, _)) => match set_aside(venv, value) {
                        Some(aside) => ExplicitReclaim::SetAside(aside),
                        None => return ExplicitReclaim::Untouched,
                    },
                    None => return ExplicitReclaim::Untouched,
                }
            }
        }
        Ok(None) => ExplicitReclaim::Untouched,
        Ok(Some(_)) | Err(_) => return ExplicitReclaim::Untouched,
    };
    if let Some((_, raw)) = &record {
        remove_file_if_unchanged(&record_path(venv), raw);
    }
    outcome
}
