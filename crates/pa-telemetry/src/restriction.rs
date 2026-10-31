//! Monotone admission-scoped restrictions on telemetry delivery.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared identity for one admission's telemetry restriction.
///
/// Clones refer to the same restriction. There is deliberately no reset: new
/// unrestricted work must use a new token. This is an additional restriction,
/// not consent and not an override of the client's global telemetry policy.
#[derive(Clone, Debug)]
pub struct RestrictionToken(Arc<AtomicBool>);

impl RestrictionToken {
    /// A distinct, initially unrestricted admission.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// A distinct admission whose events must never be delivered.
    #[must_use]
    pub fn restricted() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    /// Permanently restrict this admission and all its clones. Does not wait
    /// for delivery, acquire a mutex, or recall already in-flight events.
    pub fn restrict(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether this admission has been restricted.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
