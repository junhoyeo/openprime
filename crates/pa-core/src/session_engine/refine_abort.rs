//! The refine abort slot (TS `_refineAbortController`): every refine run registers a controller
//! for its whole plan -> apply flow, and a session abort aborts whichever controller the slot
//! holds. The daemon's engine owns the slot (shared lock-free with every session it builds), so an
//! abort never waits on the session slot's lock. A delivery that will run a refine arms a pending
//! controller first: an abort that lands before the refine registers (model resolve, first-use
//! session build) aborts that pending controller, and the refine adopts it and refuses to start.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pa_agent::abort::{AbortController, AbortSignal};

struct SlotEntry {
    generation: u64,
    controller: AbortController,
    /// Armed by a delivery and not yet adopted by a refine registration.
    pending: bool,
}

/// The shared refine abort slot; the lock is held only for the swap, never across an await.
#[derive(Default)]
pub struct RefineAbortSlot {
    current: Mutex<Option<SlotEntry>>,
    generations: AtomicU64,
}

impl RefineAbortSlot {
    fn fresh(&self, pending: bool) -> SlotEntry {
        SlotEntry {
            generation: self.generations.fetch_add(1, Ordering::SeqCst),
            controller: AbortController::new(),
            pending,
        }
    }

    /// Arm a pending controller for the delivery about to run a refine; the next registration
    /// adopts it, aborted or not.
    ///
    /// # Panics
    ///
    /// Panics when the slot's mutex is poisoned.
    pub fn arm(&self) {
        let entry = self.fresh(true);
        *self.current.lock().expect("refine abort slot lock") = Some(entry);
    }

    /// Drop the delivery's pending controller when no refine adopted it.
    ///
    /// # Panics
    ///
    /// Panics when the slot's mutex is poisoned.
    pub fn disarm(&self) {
        let mut current = self.current.lock().expect("refine abort slot lock");
        if current.as_ref().is_some_and(|entry| entry.pending) {
            *current = None;
        }
    }

    /// Abort the registered run or the armed delivery, if any; an empty slot ignores it.
    ///
    /// # Panics
    ///
    /// Panics when the slot's mutex is poisoned.
    pub fn abort(&self) {
        if let Some(entry) = self
            .current
            .lock()
            .expect("refine abort slot lock")
            .as_ref()
        {
            entry.controller.abort();
        }
    }

    /// Register one refine run: adopt the delivery's pending controller (already aborted when the
    /// abort beat the run), else install a fresh one. A later run replaces it (TS overwrites the
    /// single controller field the same way).
    pub(crate) fn register(self: &Arc<Self>) -> RefineAbortRegistration {
        let mut current = self.current.lock().expect("refine abort slot lock");
        if !current.as_ref().is_some_and(|entry| entry.pending) {
            *current = Some(self.fresh(false));
        }
        let entry = current.as_mut().expect("the slot holds the run's entry");
        entry.pending = false;
        RefineAbortRegistration {
            slot: Arc::clone(self),
            generation: entry.generation,
            signal: entry.controller.signal(),
        }
    }
}

/// One run's registration: it carries the run's signal and unregisters on drop, so an abort that
/// arrives after the run ended reaches nothing.
pub(crate) struct RefineAbortRegistration {
    slot: Arc<RefineAbortSlot>,
    generation: u64,
    signal: AbortSignal,
}

impl RefineAbortRegistration {
    pub(crate) fn signal(&self) -> &AbortSignal {
        &self.signal
    }
}

impl Drop for RefineAbortRegistration {
    fn drop(&mut self) {
        let mut current = self.slot.current.lock().expect("refine abort slot lock");
        if current
            .as_ref()
            .is_some_and(|entry| entry.generation == self.generation)
        {
            *current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_abort_on_the_armed_delivery_reaches_the_refine_that_registers_later() {
        let slot = Arc::new(RefineAbortSlot::default());
        slot.arm();
        slot.abort();
        let registration = slot.register();
        assert!(registration.signal().is_aborted());
    }

    #[test]
    fn a_registered_run_adopts_the_armed_controller_and_unregisters_on_drop() {
        let slot = Arc::new(RefineAbortSlot::default());
        slot.arm();
        let registration = slot.register();
        assert!(!registration.signal().is_aborted());
        slot.abort();
        assert!(registration.signal().is_aborted());
        drop(registration);
        // The delivery's end finds nothing left to disarm; the next run starts clean.
        slot.disarm();
        assert!(!slot.register().signal().is_aborted());
    }

    #[test]
    fn a_disarmed_delivery_leaves_no_pending_abort() {
        let slot = Arc::new(RefineAbortSlot::default());
        slot.arm();
        slot.disarm();
        slot.abort();
        assert!(!slot.register().signal().is_aborted());
    }
}
