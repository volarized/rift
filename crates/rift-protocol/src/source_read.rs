//! Exact source reads and declaration positions in an immutable selection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{
    ExactKind, ReadWarning, RevisionId, SourceExcerpt, SourceUnitId, SymbolId, TextRange,
};
use crate::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

/// Most positions submitted to one declaration-position read.
pub const SOURCE_POSITIONS_MAX: usize = 1000;
/// Largest line or character offset accepted by the declaration-position surface.
pub const SOURCE_POSITION_MAX: u64 = 2_147_483_647;

/// Reads a source unit or a byte range without requiring a semantic symbol.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetSourceParams {
    /// Canonical source unit, retaining its physical package or runtime owner.
    pub unit: SourceUnitId,
    /// Requested half-open UTF-8 byte range. Omitted requests the whole unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextRange>,
    /// A preceding immutable selection. Expiry never selects latest implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedViewId>,
    /// A local Git revision. Refused for released sources and beside `view`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
}

/// Source bytes, definite absence, or explicit selection unavailability.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum GetSourceResult {
    /// The selected unit exists, including when its requested range is empty.
    Found {
        /// Returned bytes and their exact source unit and range.
        source: SourceExcerpt,
        /// Whether the complete requested range or unit was returned.
        source_complete: bool,
        /// Immutable selection supplying these bytes.
        view: CapturedView,
        /// Projection cuts and other typed read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
    /// The unit is absent from completely searched captured coverage.
    Missing {
        /// Requested canonical source unit.
        unit: SourceUnitId,
        /// Complete immutable selection searched.
        view: CapturedView,
        /// Other typed read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
    /// The exact selection cannot establish presence or absence.
    Unavailable {
        /// Requested canonical source unit.
        unit: SourceUnitId,
        /// Selected view, when one could be captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<CapturedView>,
        /// Typed cause of unavailability.
        reason: SymbolUnavailableReason,
        /// Details and partial-read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
}

/// How a language engine counts characters within a source line.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum PositionEncoding {
    /// UTF-8 bytes.
    #[serde(rename = "utf-8")]
    Utf8,
    /// UTF-16 code units.
    #[serde(rename = "utf-16")]
    Utf16,
}

/// One exact position in a project, package or runtime source unit.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePosition {
    /// Canonical physical source unit, independent of the logical symbol owner.
    pub unit: SourceUnitId,
    /// Zero-based source line.
    #[schemars(range(min = 0_u64, max = SOURCE_POSITION_MAX))]
    pub line: u64,
    /// Zero-based offset counted using the request's position encoding.
    #[schemars(range(min = 0_u64, max = SOURCE_POSITION_MAX))]
    pub character: u64,
}

/// Finds declarations holding positions in one immutable selection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FindDeclarationsParams {
    /// Character units negotiated by the engine that supplied the positions.
    pub position_encoding: PositionEncoding,
    /// Positions to answer once, retaining request order.
    #[schemars(length(min = 1, max = SOURCE_POSITIONS_MAX))]
    #[schemars(extend("uniqueItems" = true))]
    pub positions: Vec<SourcePosition>,
    /// A preceding immutable selection shared by the whole batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedViewId>,
    /// A local Git revision. Refused for released sources and beside `view`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
}

/// One submitted source position and its exact declaration outcome.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeclarationPositionResult {
    /// An established declaration holds this exact position.
    Found {
        /// Submitted position, unchanged.
        position: SourcePosition,
        /// Canonical logical object, including proved runtime or stub mappings.
        #[serde(deserialize_with = "crate::symbol_read::deserialize_symbol_id")]
        #[schemars(with = "crate::identity::SymbolIdentity")]
        id: SymbolId,
        /// Declaration kind in the provider's vocabulary.
        kind: ExactKind,
        /// Immutable selection supplying this declaration.
        view: CapturedView,
    },
    /// Complete captured coverage contains no declaration at this position.
    Missing {
        /// Submitted position, unchanged.
        position: SourcePosition,
        /// Complete immutable selection searched.
        view: CapturedView,
    },
    /// Presence or absence cannot be established at this exact position.
    Unavailable {
        /// Submitted position, unchanged.
        position: SourcePosition,
        /// Selected view, when one could be captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<CapturedView>,
        /// Typed cause of unavailability.
        reason: SymbolUnavailableReason,
    },
}

/// One answer per submitted position, retaining one shared captured selection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FindDeclarationsResult {
    /// Exact outcomes in request order.
    #[schemars(length(max = SOURCE_POSITIONS_MAX))]
    pub results: Vec<DeclarationPositionResult>,
    /// Typed read warnings, including explicit partial coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

impl GetSourceParams {
    /// Checks source spelling, byte bounds, and mutually exclusive selectors.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        selectors_valid(self.view.as_ref(), self.rev.as_ref())
            && source_valid(&self.unit, self.rev.as_ref())
            && self.range.as_ref().is_none_or(range_valid)
    }
}

impl SourcePosition {
    /// Checks canonical source spelling and negotiated coordinate bounds.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        source_valid(&self.unit, None)
            && self.line <= SOURCE_POSITION_MAX
            && self.character <= SOURCE_POSITION_MAX
    }
}

impl FindDeclarationsParams {
    /// Checks batch bounds and selectors before any source is read.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        !self.positions.is_empty()
            && self.positions.len() <= SOURCE_POSITIONS_MAX
            && selectors_valid(self.view.as_ref(), self.rev.as_ref())
            && self.positions.iter().all(|position| {
                position.is_valid()
                    && source_valid(&position.unit, self.rev.as_ref())
                    && seen.insert((position.unit.as_str(), position.line, position.character))
            })
    }
}

impl GetSourceResult {
    /// Checks response identity, captured view, and returned byte ranges.
    /// Complete coverage and view expiry are established by the serving boundary.
    #[must_use]
    pub fn is_valid_for(&self, request: &GetSourceParams) -> bool {
        if !request.is_valid() {
            return false;
        }
        match self {
            Self::Found {
                source,
                source_complete,
                view,
                ..
            } => {
                let range = &source.span.range;
                source.span.unit == request.unit
                    && view_matches(view, request.view.as_ref())
                    && range_valid(range)
                    && u64::try_from(source.text.len()).ok() == range.end.checked_sub(range.start)
                    && request.range.as_ref().map_or_else(
                        || range.start == 0,
                        |requested| {
                            range.start == requested.start
                                && range.end <= requested.end
                                && (!source_complete || range.end == requested.end)
                        },
                    )
            }
            Self::Missing { unit, view, .. } => {
                *unit == request.unit && view_matches(view, request.view.as_ref())
            }
            Self::Unavailable { unit, view, .. } => {
                *unit == request.unit
                    && view
                        .as_ref()
                        .is_none_or(|view| view_matches(view, request.view.as_ref()))
            }
        }
    }
}

impl DeclarationPositionResult {
    /// Submitted position associated with this exact outcome.
    #[must_use]
    pub const fn position(&self) -> &SourcePosition {
        match self {
            Self::Found { position, .. }
            | Self::Missing { position, .. }
            | Self::Unavailable { position, .. } => position,
        }
    }

    /// Captured view, absent only when a selection could not be admitted.
    #[must_use]
    pub const fn view(&self) -> Option<&CapturedView> {
        match self {
            Self::Found { view, .. } | Self::Missing { view, .. } => Some(view),
            Self::Unavailable { view, .. } => view.as_ref(),
        }
    }
}

impl FindDeclarationsResult {
    /// Checks one ordered outcome per position and a single shared captured view.
    #[must_use]
    pub fn is_valid_for(&self, request: &FindDeclarationsParams) -> bool {
        if !request.is_valid() || self.results.len() != request.positions.len() {
            return false;
        }
        let selected = self
            .results
            .iter()
            .find_map(DeclarationPositionResult::view);
        self.results
            .iter()
            .zip(&request.positions)
            .all(|(result, position)| {
                result.position() == position
                    && result.view().is_none_or(|view| {
                        Some(view) == selected && view_matches(view, request.view.as_ref())
                    })
                    && match result {
                        DeclarationPositionResult::Found { id, .. } => {
                            SymbolId::parse(id.as_str()).is_ok()
                        }
                        _ => true,
                    }
            })
    }
}

fn selectors_valid(view: Option<&CapturedViewId>, rev: Option<&RevisionId>) -> bool {
    !(view.is_some() && rev.is_some()) && rev.is_none_or(|rev| rev.violation().is_none())
}

fn source_valid(unit: &SourceUnitId, rev: Option<&RevisionId>) -> bool {
    SourceUnitId::parse(unit.as_str()).is_ok()
        && (rev.is_none() || unit.as_str().starts_with("rift://source/project/"))
}

fn range_valid(range: &TextRange) -> bool {
    range.start <= range.end && range.end <= 9_007_199_254_740_991
}

fn view_matches(view: &CapturedView, requested: Option<&CapturedViewId>) -> bool {
    requested.is_none_or(|requested| view.id == *requested)
}

#[cfg(test)]
mod tests;
