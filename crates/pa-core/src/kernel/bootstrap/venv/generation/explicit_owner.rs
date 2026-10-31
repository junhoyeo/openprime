//! Persisted enrolment of an explicit environment created by this product.
//! Version files prove compatibility, not ownership. Only a completed build
//! writes this sidecar; existing environments are never silently enrolled.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::{
    directory_identity, generation_root, read_record, require_direct_directory,
    symlink_metadata_state, write_json_new, DirectoryIdentity, RecordState,
};

const OWNER_SUFFIX: &str = ".prime-agent-managed.json";
const ROOT_OWNER_FILE: &str = ".prime-agent-root.json";
const VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerRecord {
    version: u64,
    token: String,
    path: PathBuf,
    directory_identity: DirectoryIdentity,
    parent_identity: DirectoryIdentity,
    bootstrap_hash: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RootRecord {
    version: u64,
    owner: OwnerRecord,
    directory_identity: DirectoryIdentity,
}

/// An identity-bound published explicit base. This is not permission to
/// modify the base; it only authorizes a separately owned sibling namespace.
pub(in crate::kernel::bootstrap) struct ExplicitOwnership {
    record: OwnerRecord,
    raw: Vec<u8>,
}

fn owner_path(venv: &Path) -> PathBuf {
    venv.with_file_name(format!(
        "{}{OWNER_SUFFIX}",
        venv.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// Canonicalize the parent, not the leaf: leaf symlinks must never enrol a
/// different directory, while /tmp and /private/tmp share one lock/record.
pub(in crate::kernel::bootstrap) fn canonical_explicit_path(venv: &Path) -> anyhow::Result<PathBuf> {
    let parent = venv.parent().ok_or_else(|| anyhow!("explicit kernel path has no parent"))?;
    let name = venv.file_name().ok_or_else(|| anyhow!("explicit kernel path has no final component"))?;
    Ok(parent.canonicalize()?.join(name))
}

fn require_private_record(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink()
        || !crate::platform::perms::is_private_owner_metadata(&metadata)
    {
        bail!("explicit kernel ownership record is not private at {}; retained without repair", path.display());
    }
    Ok(())
}

fn bootstrap_hash(venv: &Path) -> anyhow::Result<String> {
    let path = venv.join(".bootstrap-version");
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > super::MAX_RECORD_BYTES {
        bail!("explicit kernel bootstrap marker is not a bounded regular file");
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() as u64 > super::MAX_RECORD_BYTES {
        bail!("explicit kernel bootstrap marker grew during inspection");
    }
    Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
}

impl ExplicitOwnership {
    pub(in crate::kernel::bootstrap) fn read(venv: &Path) -> anyhow::Result<Option<Self>> {
        let (record, raw) = match read_record::<OwnerRecord>(&owner_path(venv)) {
            RecordState::Missing => return Ok(None),
            RecordState::Valid { value, raw } => (value, raw),
            RecordState::Unknown => bail!("explicit kernel ownership record is unreadable; retained without repair"),
        };
        let ownership = Self { record, raw };
        ownership.verify(venv)?;
        Ok(Some(ownership))
    }

    pub(super) fn publish(venv: &Path, token: &str) -> anyhow::Result<()> {
        require_direct_directory(venv)?;
        let parent = venv.parent().ok_or_else(|| anyhow!("explicit kernel path has no parent"))?;
        let record = OwnerRecord {
            version: VERSION,
            token: token.to_string(),
            path: canonical_explicit_path(venv)?,
            directory_identity: directory_identity(venv)?.ok_or_else(|| anyhow!("unknown explicit directory identity"))?,
            parent_identity: directory_identity(parent)?.ok_or_else(|| anyhow!("unknown explicit parent identity"))?,
            bootstrap_hash: bootstrap_hash(venv)?,
        };
        let raw = write_json_new(&owner_path(venv), &record)?;
        Self { record, raw }.verify(venv)
    }

    pub(in crate::kernel::bootstrap) fn verify(&self, venv: &Path) -> anyhow::Result<()> {
        require_direct_directory(venv)?;
        let parent = venv.parent().ok_or_else(|| anyhow!("explicit kernel path has no parent"))?;
        require_private_record(&owner_path(venv))?;
        let unchanged = matches!(read_record::<OwnerRecord>(&owner_path(venv)), RecordState::Valid { value, raw } if value == self.record && raw == self.raw);
        if !unchanged
            || self.record.version != VERSION
            || uuid::Uuid::parse_str(&self.record.token).is_err()
            || canonical_explicit_path(venv)? != self.record.path
            || directory_identity(venv)?.as_ref() != Some(&self.record.directory_identity)
            || directory_identity(parent)?.as_ref() != Some(&self.record.parent_identity)
            || bootstrap_hash(venv)? != self.record.bootstrap_hash
        {
            bail!("explicit kernel ownership changed at {}; retained without repair", venv.display());
        }
        Ok(())
    }

    pub(in crate::kernel::bootstrap) fn lease_for_resolver(&self, venv: &Path) -> anyhow::Result<()> {
        self.verify(venv)?;
        super::write_process_lease(venv, super::LeaseKind::Resolver, super::ProcessOwner::current())?;
        self.verify(venv)
    }

    /// Called under the explicit-base bootstrap lock. A preexisting sibling
    /// root needs its own identity-bound record; a foreign directory is not adopted.
    pub(in crate::kernel::bootstrap) fn prepare_generations(&self, venv: &Path) -> anyhow::Result<()> {
        self.verify(venv)?;
        let root = generation_root(venv)?;
        match symlink_metadata_state(&root)? {
            None => {
                std::fs::create_dir(&root)?;
                crate::platform::perms::restrict_dir(&root)?;
                let directory_identity = directory_identity(&root)?.ok_or_else(|| anyhow!("unknown explicit generation root identity"))?;
                write_json_new(&root.join(ROOT_OWNER_FILE), &RootRecord {
                    version: VERSION,
                    owner: self.record.clone(),
                    directory_identity,
                })?;
            }
            Some(_) => require_direct_directory(&root)?,
        }
        require_private_record(&root.join(ROOT_OWNER_FILE))?;
        let RecordState::Valid { value, .. } = read_record::<RootRecord>(&root.join(ROOT_OWNER_FILE)) else {
            bail!("explicit generation root ownership is unavailable at {}; retained without repair", root.display());
        };
        if value.version != VERSION || value.owner != self.record
            || directory_identity(&root)?.as_ref() != Some(&value.directory_identity)
        {
            bail!("explicit generation root ownership changed at {}; retained without repair", root.display());
        }
        self.verify(venv)
    }
}
