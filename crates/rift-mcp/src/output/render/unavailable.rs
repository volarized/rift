//! The opening line of an answer that cannot establish presence or absence.

use rift_protocol::read::ReadWarning;
use rift_protocol::symbol_read::SymbolUnavailableReason;

/// The first words of every unavailable answer.
const UNAVAILABLE: &str = "unavailable";
/// What a caller reads while the selected index is preparing.
const INDEXING: &str = "indexing is in progress";

/// The opening line of an unavailable answer.
///
/// A preparing index reports how far preparation has come, from the progress warnings the
/// answer kept. Every other reason names its variant.
pub(super) fn line(reason: SymbolUnavailableReason, warnings: &[ReadWarning]) -> String {
    if reason != SymbolUnavailableReason::IndexPreparing {
        return format!("{UNAVAILABLE} · {reason:?}");
    }
    let counts: Vec<String> = warnings.iter().filter_map(progress).collect();
    if counts.is_empty() {
        return format!("{UNAVAILABLE} · {INDEXING}");
    }
    format!("{UNAVAILABLE} · {INDEXING} ({})", counts.join(", "))
}

/// Whether an unavailable answer keeps `warning` for its opening line.
///
/// Only a preparing index reports progress, so every other reason keeps nothing.
pub(super) fn reports(reason: SymbolUnavailableReason, warning: &ReadWarning) -> bool {
    reason == SymbolUnavailableReason::IndexPreparing && progress(warning).is_some()
}

/// The count one preparation warning carries, in the unit that warning counts.
fn progress(warning: &ReadWarning) -> Option<String> {
    match warning {
        ReadWarning::LocalIndexPreparing {
            prepared,
            total: Some(total),
            ..
        } => Some(format!("{prepared} of {total} files")),
        ReadWarning::LocalIndexPreparing {
            prepared,
            total: None,
            ..
        } => Some(format!("{prepared} files so far")),
        ReadWarning::PatternIndexPreparing {
            prepared, total, ..
        } => Some(format!("{prepared} of {total} rows of file text")),
        _ => None,
    }
}
