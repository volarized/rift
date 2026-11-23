//! Opt-in counters for the ordinary Bun build diagnostic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::Serialize;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static REQUESTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static COUNTS: Mutex<BTreeMap<&'static str, Observation>> = Mutex::new(BTreeMap::new());

#[derive(Debug, Default, Serialize)]
pub struct Observation {
    pub observed: usize,
    pub unknown: usize,
    pub wall_ns: u128,
    pub first: Option<Witness>,
    pub first_affected: Option<Witness>,
    pub counters: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Serialize)]
pub struct Witness {
    pub reason: &'static str,
    pub path: Option<String>,
    pub detail: Option<String>,
}

/// Reports whether this build compiled the local diagnostic.
pub const fn compiled() -> bool {
    option_env!("RIFT_BUN_ELIGIBILITY").is_some()
}

#[inline]
pub fn active() -> bool {
    compiled() && ACTIVE.load(Ordering::Relaxed)
}

pub fn begin(paths: impl Iterator<Item = String>) {
    assert!(
        compiled(),
        "Bun observation requires RIFT_BUN_ELIGIBILITY at build time"
    );
    let mut counts = COUNTS.lock().expect("eligibility lock");
    counts.clear();
    *REQUESTED.lock().expect("eligibility paths") = paths.collect();
    ACTIVE.store(true, Ordering::Relaxed);
}

pub fn finish() -> BTreeMap<&'static str, Observation> {
    ACTIVE.store(false, Ordering::Relaxed);
    std::mem::take(&mut *COUNTS.lock().expect("eligibility lock"))
}

pub fn start() -> Option<Instant> {
    active().then(Instant::now)
}

pub fn requested(path: &str) -> bool {
    active() && REQUESTED.lock().expect("eligibility paths").contains(path)
}

pub fn note(
    family: &'static str,
    counters: &[(&'static str, usize)],
    observed: usize,
    unknown: usize,
    first: Option<(&'static str, Option<&str>, Option<&str>)>,
    started: Option<Instant>,
) {
    let affected = first.and_then(|(_, path, _)| path).is_some_and(requested);
    note_affected(
        family, counters, observed, unknown, first, affected, started,
    );
}

pub fn note_affected(
    family: &'static str,
    counters: &[(&'static str, usize)],
    observed: usize,
    unknown: usize,
    first: Option<(&'static str, Option<&str>, Option<&str>)>,
    affected: bool,
    started: Option<Instant>,
) {
    if !active() {
        return;
    }
    let mut counts = COUNTS.lock().expect("eligibility lock");
    let entry = counts.entry(family).or_default();
    entry.observed = entry.observed.saturating_add(observed);
    entry.unknown = entry.unknown.saturating_add(unknown);
    if let Some(started) = started {
        entry.wall_ns = entry.wall_ns.saturating_add(started.elapsed().as_nanos());
    }
    for &(name, value) in counters {
        let counter = entry.counters.entry(name).or_default();
        *counter = counter.saturating_add(value);
    }
    if let Some((reason, path, detail)) = first {
        let bounded = |value: &str| -> String { value.chars().take(320).collect() };
        let witness = || Witness {
            reason,
            path: path.map(bounded),
            detail: detail.map(bounded),
        };
        entry.first.get_or_insert_with(witness);
        if affected {
            entry.first_affected.get_or_insert_with(witness);
        }
    }
}
