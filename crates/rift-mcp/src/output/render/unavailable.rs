//! The opening line of an answer that cannot establish presence or absence.

use rift_protocol::symbol_read::SymbolUnavailableReason;

/// The opening line of an unavailable answer.
pub(super) fn line(reason: SymbolUnavailableReason) -> String {
    match reason {
        SymbolUnavailableReason::IndexPreparing => {
            "unavailable | indexing is in progress".to_owned()
        }
        _ => format!("unavailable | {reason:?}"),
    }
}
