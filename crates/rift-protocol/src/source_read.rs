//! Declaration positions in an immutable selection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{ExactKind, ReadWarning, SourceUnitId, SymbolId};
use crate::revision::Revision;
use crate::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

/// Most positions submitted to one declaration-position read.
pub const SOURCE_POSITIONS_MAX: usize = 1000;
/// Largest line or character offset accepted by the declaration-position surface.
pub const SOURCE_POSITION_MAX: u64 = 2_147_483_647;

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

/// One exact position in a source unit, at the release of a package or runtime unit.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePosition {
    /// Canonical physical source unit, independent of the logical symbol owner.
    pub unit: SourceUnitId,
    /// The release of the unit's package or runtime owner whose bytes hold the position, as
    /// `rift://revision/cargo/crates.io/demo@1.0.0`. Present exactly when the unit has such an
    /// owner: a custom resolver's unit names no release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
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
        /// The release of the declaration's owner that `id` answers in. It differs from
        /// the position's release when a proved stub mapping names a runtime declaration.
        revision: Revision,
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

impl SourcePosition {
    /// Checks canonical source spelling, a release exactly for a unit with a package or runtime
    /// owner, and negotiated coordinate bounds.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let released = crate::identity::parse_source_unit_identity(self.unit.as_str()).is_ok_and(
            |(owner, _)| match (owner, &self.revision) {
                (Some(owner), Some(revision)) => revision.answers_for(&owner),
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            },
        );
        released && self.line <= SOURCE_POSITION_MAX && self.character <= SOURCE_POSITION_MAX
    }
}

impl FindDeclarationsParams {
    /// Checks batch bounds and selectors before any source is read.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        !self.positions.is_empty()
            && self.positions.len() <= SOURCE_POSITIONS_MAX
            && self.positions.iter().all(|position| {
                position.is_valid()
                    && seen.insert((position.unit.as_str(), position.line, position.character))
            })
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
                        DeclarationPositionResult::Found {
                            id, kind, revision, ..
                        } => {
                            crate::identity::SymbolIdentity::parse(id.as_str())
                                .is_ok_and(|identity| revision.answers_for(identity.owner()))
                                && ExactKind::try_from(kind.0.clone()).is_ok()
                        }
                        _ => true,
                    }
            })
    }
}

fn view_matches(view: &CapturedView, requested: Option<&CapturedViewId>) -> bool {
    requested.is_none_or(|requested| view.id == *requested)
}

#[cfg(test)]
mod tests;
