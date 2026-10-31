//! Test-only completion barriers around real supervisor lifecycle boundaries.
//! They never replace worker replies, process ownership, or publication results.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase { LaunchChecked, RecoveryReplayed }

struct Barrier {
    phase: Phase,
    path: PathBuf,
    reached: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

#[derive(Default)]
pub(super) struct LifecycleTestHooks {
    pub(super) executable: Mutex<Option<PathBuf>>,
    pub(super) ready: Mutex<Option<oneshot::Sender<()>>>,
    /// Simulates a supervisor crash right after a child deletion's tombstone
    /// commits, before its stop descriptor persists. Fires once.
    pub(super) crash_after_tombstone: std::sync::atomic::AtomicBool,
    barrier: Mutex<Option<Barrier>>,
}

impl LifecycleTestHooks {
    pub(super) fn pause(&self, phase: Phase, path: &Path) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, wait) = oneshot::channel();
        let (resume, release) = oneshot::channel();
        let previous = self.barrier.lock().unwrap().replace(Barrier {
            phase, path: crate::lease::canonical_session_path(path), reached, resume: release,
        });
        assert!(previous.is_none(), "one owned lifecycle barrier per supervisor");
        (wait, resume)
    }

    pub(super) async fn checkpoint(&self, phase: Phase, path: Option<&str>) -> anyhow::Result<()> {
        let barrier = {
            let mut slot = self.barrier.lock().unwrap();
            let matches = slot.as_ref().is_some_and(|barrier| barrier.phase == phase
                && path.is_some_and(|path| crate::lease::canonical_session_path(Path::new(path)) == barrier.path));
            if matches { slot.take() } else { None }
        };
        if let Some(barrier) = barrier {
            barrier.reached.send(()).map_err(|_| anyhow::anyhow!("lifecycle observer disappeared"))?;
            tokio::time::timeout(std::time::Duration::from_secs(15), barrier.resume).await
                .map_err(|_| anyhow::anyhow!("lifecycle barrier release timed out"))?
                .map_err(|_| anyhow::anyhow!("lifecycle barrier owner dropped"))?;
        }
        Ok(())
    }
}
