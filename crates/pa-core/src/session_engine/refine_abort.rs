//! The in-flight refinement's abort slot (TS `_refineAbortController`): every refine run
//! registers a fresh controller for its whole plan -> apply flow, and a session abort aborts
//! whichever run is registered. The registration clears itself when its run ends.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pa_agent::abort::{AbortController, AbortSignal};

#[derive(Default)]
pub(crate) struct RefineAbortSlot {
    current: Mutex<Option<(u64, AbortController)>>,
    generations: AtomicU64,
}

impl RefineAbortSlot {
    /// Register a fresh controller for one refine run; a later run replaces it (TS overwrites
    /// the single controller field the same way).
    pub(crate) fn register(&self) -> RefineAbortRegistration<'_> {
        let generation = self.generations.fetch_add(1, Ordering::SeqCst);
        let controller = AbortController::new();
        let signal = controller.signal();
        *self.current.lock().expect("refine abort slot lock") = Some((generation, controller));
        RefineAbortRegistration {
            slot: self,
            generation,
            signal,
        }
    }

    /// Abort the registered run, if any; a session with no refine in flight ignores it.
    pub(crate) fn abort(&self) {
        if let Some((_, controller)) = self
            .current
            .lock()
            .expect("refine abort slot lock")
            .as_ref()
        {
            controller.abort();
        }
    }
}

/// One run's registration: it carries the run's signal and unregisters on drop, so an abort that
/// arrives after the run ended reaches nothing.
pub(crate) struct RefineAbortRegistration<'a> {
    slot: &'a RefineAbortSlot,
    generation: u64,
    signal: AbortSignal,
}

impl RefineAbortRegistration<'_> {
    pub(crate) fn signal(&self) -> &AbortSignal {
        &self.signal
    }
}

impl Drop for RefineAbortRegistration<'_> {
    fn drop(&mut self) {
        let mut current = self.slot.current.lock().expect("refine abort slot lock");
        if current
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            *current = None;
        }
    }
}
