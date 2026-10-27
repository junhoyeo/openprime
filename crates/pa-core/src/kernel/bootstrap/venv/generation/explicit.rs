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
    RecordState, RECORD_VERSION,
};

const BUILD_RECORD_SUFFIX: &str = ".prime-agent-build.json";
const ABANDONED_INFIX: &str = ".abandoned-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExplicitBuildRecord {
    version: u64,
    token: String,
    directory_identity: DirectoryIdentity,
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
        let Some(directory_identity) = directory_identity(venv)? else {
            bail!(
                "filesystem cannot prove PRIME_AGENT_KERNEL_VENV directory identity at {}",
                venv.display()
            );
        };
        let record = ExplicitBuildRecord {
            version: RECORD_VERSION,
            token: uuid::Uuid::new_v4().to_string(),
            directory_identity,
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

    /// Whether the exact path still is the directory this attempt created.
    pub(in crate::kernel::bootstrap) fn owns_path(&self) -> bool {
        holds_recorded_directory(&self.venv, &self.record)
    }

    /// The build validated: the record is no longer needed.
    pub(in crate::kernel::bootstrap) fn publish(mut self) {
        self.published = true;
        remove_file_if_unchanged(&record_path(&self.venv), &self.raw);
    }
}

impl Drop for ExplicitBuild {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        set_aside(&self.venv, &self.record);
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
/// directory, a symlink) stays untouched.
pub(in crate::kernel::bootstrap) fn reclaim_explicit_path(
    venv: &Path,
) -> anyhow::Result<ExplicitReclaim> {
    let record = match read_record::<ExplicitBuildRecord>(&record_path(venv)) {
        RecordState::Valid { value, raw }
            if value.version == RECORD_VERSION
                && holder_state(&value.owner) == HolderState::Dead =>
        {
            Some((value, raw))
        }
        _ => None,
    };
    let outcome = match symlink_metadata_state(venv)? {
        Some(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            if std::fs::read_dir(venv)?.next().is_none() {
                // Fails, and leaves the path alone, if anything appeared.
                if std::fs::remove_dir(venv).is_err() {
                    return Ok(ExplicitReclaim::Untouched);
                }
                ExplicitReclaim::RemovedEmpty
            } else {
                // A written version file marks a build that got as far as
                // publication: a stale environment, never an abandoned one.
                let unpublished = super::super::read_bootstrap_version(venv).is_none();
                match record.as_ref().filter(|_| unpublished) {
                    Some((value, _)) => match set_aside(venv, value) {
                        Some(aside) => ExplicitReclaim::SetAside(aside),
                        None => return Ok(ExplicitReclaim::Untouched),
                    },
                    None => return Ok(ExplicitReclaim::Untouched),
                }
            }
        }
        Some(_) => return Ok(ExplicitReclaim::Untouched),
        None => ExplicitReclaim::Untouched,
    };
    if let Some((_, raw)) = &record {
        remove_file_if_unchanged(&record_path(venv), raw);
    }
    Ok(outcome)
}
