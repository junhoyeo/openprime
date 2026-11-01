//! The one permanent metadata fence for a canonical runtime-session lease.
//! No network/await/destructor that acquires a lease may run in a held closure.

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use pa_core::platform::lock_dir::NativeFileFence;

use super::{GuardWait, RuntimeLeaseOwnership, RuntimeSessionAuthority, SessionLease};

thread_local! {
    static HELD: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

struct ThreadClaim(PathBuf);
impl Drop for ThreadClaim {
    fn drop(&mut self) {
        HELD.with(|held| {
            held.borrow_mut().remove(&self.0);
        });
    }
}

fn mutex_path(directory: &Path) -> Result<PathBuf> {
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".lock"))
        .context("unverifiable runtime lease key")?;
    anyhow::ensure!(
        name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unverifiable runtime lease key"
    );
    Ok(directory.with_extension("fence"))
}

/// The existing common mutation entry: the native file is permanent and is
/// never removed by release/reclaim. A legacy bare guard has no verifiable
/// holder; retain/refuse it rather than using age as an ownership certificate.
pub(super) fn coordinate<T>(
    directory: &Path,
    wait: GuardWait,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let path = mutex_path(directory)?;
    let root = directory
        .parent()
        .context("runtime lease has no coordination root")?;
    let metadata = std::fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && super::checked_session_path(root)? == root,
        "unverifiable runtime coordination directory; retained"
    );
    let admitted = HELD.with(|held| held.borrow_mut().insert(path.clone()));
    anyhow::ensure!(admitted, "runtime session fence is non-reentrant");
    let claim = ThreadClaim(path.clone());
    let mut carrier = NativeFileFence::open(&path)?;
    let budget = match wait {
        GuardWait::Fast => Duration::from_millis(1_500),
        GuardWait::ThroughStale => Duration::from_millis(5_500),
    };
    let deadline = Instant::now() + budget;
    while !carrier.try_lock()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "runtime session metadata fence is busy"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let legacy = PathBuf::from(format!("{}.guard", directory.display()));
    match std::fs::symlink_metadata(&legacy) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err(anyhow!(
                "legacy session guard ownership is unverifiable; retained at {}",
                legacy.display()
            ))
        }
    }
    carrier.verify()?;
    let result = action();
    let verified = carrier.verify();
    // A returned SessionLease (or an error's cleanup) must not drop while M
    // is held. Close the OS handle and thread claim before propagating either.
    drop(carrier);
    drop(claim);
    verified?;
    result
}

#[derive(Debug)]
pub(super) struct RuntimeSessionTarget {
    pub(super) agent_dir: PathBuf,
    pub(super) canonical: PathBuf,
    pub(super) directory: PathBuf,
    agent_spelling: PathBuf,
    target_spelling: PathBuf,
}

impl RuntimeSessionTarget {
    pub(super) fn resolve(agent_dir: &Path, target: &Path) -> Result<Self> {
        anyhow::ensure!(
            !agent_dir.as_os_str().is_empty() && !target.as_os_str().is_empty(),
            "fileless state cannot be used as a runtime lease key"
        );
        let agent_spelling = std::path::absolute(crate::paths::expand_tilde(
            agent_dir
                .to_str()
                .context("agent root is not a verifiable UTF-8 identity")?,
        )?)?;
        let target_spelling = std::path::absolute(crate::paths::expand_tilde(
            target
                .to_str()
                .context("session path is not a verifiable UTF-8 identity")?,
        )?)?;
        let agent_dir = super::checked_session_path(&agent_spelling)?;
        let canonical = super::checked_session_path(&target_spelling)?;
        anyhow::ensure!(
            canonical.to_str().is_some(),
            "session path is not a verifiable UTF-8 identity"
        );
        let directory = super::lease_directory_for_canonical(&agent_dir, &canonical);
        Ok(Self {
            agent_dir,
            canonical,
            directory,
            agent_spelling,
            target_spelling,
        })
    }

    pub(super) fn opened_path(&self) -> &Path {
        &self.target_spelling
    }

    pub(super) fn revalidate(&self) -> Result<()> {
        anyhow::ensure!(
            super::checked_session_path(&self.agent_spelling)? == self.agent_dir
                && super::checked_session_path(&self.target_spelling)? == self.canonical,
            "runtime session path identity changed; ownership retained"
        );
        Ok(())
    }
}

/// Thread-confined proof that M for this exact canonical target is held.
/// It cannot be cloned, returned, moved to another thread, or held over await.
pub(crate) struct HeldRuntimeSessionFence<'f> {
    target: &'f RuntimeSessionTarget,
    thread_confined: PhantomData<Rc<()>>,
}

impl HeldRuntimeSessionFence<'_> {
    pub(crate) fn canonical_path(&self) -> &Path {
        &self.target.canonical
    }
    pub(crate) fn inspect_ownership(&self) -> Result<RuntimeLeaseOwnership> {
        self.target.revalidate()?;
        super::inspect_runtime_ownership_at(self.target)
    }

    pub(crate) fn acquire_runtime_lease(
        &mut self,
        authority: &RuntimeSessionAuthority,
    ) -> Result<SessionLease> {
        self.target.revalidate()?;
        anyhow::ensure!(
            super::checked_session_path(authority.agent_dir())? == self.target.agent_dir,
            "runtime lease authority belongs to another agent root"
        );
        super::publish_runtime_owner(self.target, authority)
    }

    pub(crate) fn revalidate_lease(&self, lease: &SessionLease) -> Result<()> {
        self.target.revalidate()?;
        super::revalidate_owned_lease(self.target, lease)
    }
}

/// Run a short synchronous ownership transaction. Resolve ledger objects and do
/// network work before entering; use fixed-order try-locks for publication data.
/// Never call a self-locking lease method or drop a `SessionLease` in this closure.
pub(crate) fn with_runtime_session_fence<T>(
    agent_dir: &Path,
    target: &Path,
    action: impl FnOnce(&mut HeldRuntimeSessionFence<'_>) -> Result<T>,
) -> Result<T> {
    let target = RuntimeSessionTarget::resolve(agent_dir, target)?;
    let root = target
        .directory
        .parent()
        .context("runtime lease has no coordination root")?;
    pa_core::platform::perms::create_dir_all_private(root)?;
    target.revalidate()?;
    coordinate(&target.directory, GuardWait::ThroughStale, || {
        target.revalidate()?;
        let mut held = HeldRuntimeSessionFence {
            target: &target,
            thread_confined: PhantomData,
        };
        action(&mut held)
    })
}
