//! Captured search and pattern selection failures.

use crate::{
    read::ReadWarning,
    symbol_read::{CapturedView, SymbolUnavailableReason},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The captured selection cannot serve this search or pattern request.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchUnavailable {
    /// Presence or absence cannot be established in the requested selection.
    Unavailable {
        /// The existing typed cause of selection unavailability.
        reason: SymbolUnavailableReason,
        /// Selected view, when one could be captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<CapturedView>,
        /// Details and partial-read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
}

#[cfg(test)]
mod tests;
