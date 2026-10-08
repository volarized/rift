//! Entry counts for the client's retained capabilities, resolution, and failure.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Position of the retained capabilities count.
pub(super) const CAPABILITIES: usize = 0;
/// Position of the retained resolution count.
pub(super) const RESOLUTION: usize = 1;
/// Position of the retained failure count.
pub(super) const FAILURE: usize = 2;
/// One retained entry of each kind per client owner.
pub(super) const COUNTS: usize = 3;

const SCOPE: rift_tracing::InstrumentScope =
    rift_tracing::InstrumentScope::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

static ENTRY_COUNT: rift_tracing::ObservableUpDownCounter<1> =
    rift_tracing::ObservableUpDownCounter::declare(
        SCOPE,
        "cache.entry.count",
        "{entry}",
        &["cache.name"],
    );

/// Observes retained entries until the client owner drops its guard.
pub(super) fn observe(counts: &Arc<[AtomicU64; COUNTS]>) -> Option<rift_tracing::ObservationGuard> {
    let counts = Arc::downgrade(counts);
    ENTRY_COUNT.observe(move |observation| {
        if let Some(counts) = counts.upgrade() {
            observation.observe(
                ["CachedCapabilities"],
                counts[CAPABILITIES].load(Ordering::Relaxed),
            );
            observation.observe(
                ["CachedResolution"],
                counts[RESOLUTION].load(Ordering::Relaxed),
            );
            observation.observe(["CachedFailure"], counts[FAILURE].load(Ordering::Relaxed));
        }
    })
}
