//! The MCP request and result of one exact symbol read.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::documentation::DocumentationContext;
use crate::identity::SymbolIdentity;
use crate::read::{
    ExactKind, GetSymbolInclude, MAX_RESULTS_DEFAULT, PAGE_DEFAULT, PAGE_LIMIT_MAX, Pagination,
    ReadWarning, SourceExcerpt, SourceUnitId, Symbol, SymbolHistory, SymbolId,
};
use crate::revision::Revision;
use crate::symbol_read::{
    self, GET_SYMBOL_MAX_RESULTS, SymbolUnavailableReason, deserialize_symbol_id,
};

/// Optional sections of an exact symbol read.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolSection {
    /// Attached documentation and references to the selected symbol.
    Documentation,
    /// Exact source of the returned declarations.
    Source,
    /// Commit or release history for the selected symbol.
    History,
    /// Captured outgoing edges of the selected symbol.
    Edges,
}

/// Reads one exact logical symbol at a selected revision.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {"id": "rift://symbol/local/rust/app/parser/parse"},
    {"id": "rift://symbol/local/rust/app/parser/parse", "include": ["edges"], "exclude": ["source"]}
]))]
pub struct GetSymbolParams {
    /// Canonical identity. Ownership and language come from this address.
    #[serde(deserialize_with = "deserialize_symbol_id")]
    #[schemars(with = "SymbolIdentity")]
    pub id: SymbolId,
    /// Sections to add to the default `documentation` and `source` selections.
    #[serde(
        default,
        deserialize_with = "crate::mcp_projection::deserialize_set",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub include: BTreeSet<SymbolSection>,
    /// Sections to remove after applying the defaults and `include`.
    #[serde(
        default,
        deserialize_with = "crate::mcp_projection::deserialize_set",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub exclude: BTreeSet<SymbolSection>,
    /// Selects a commit, release, or served tree. Omitted reads the current tree or the package
    /// version pinned by the workspace, when one is pinned. A moved tree revision is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// The 1-based declaration page.
    #[serde(default = "default_page")]
    #[schemars(range(min = 1_u64))]
    pub page: u64,
    /// Declarations per page, at most 10,000.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1_u64, max = GET_SYMBOL_MAX_RESULTS))]
    pub max_results: u64,
}

fn default_page() -> u64 {
    PAGE_DEFAULT
}

fn default_max_results() -> u64 {
    MAX_RESULTS_DEFAULT
}

/// The effective selection after applying the request's exclusions.
pub type SymbolSections = BTreeSet<SymbolSection>;

impl GetSymbolParams {
    /// Applies the default sections, additions, and exclusions.
    #[must_use]
    pub fn selection(&self) -> SymbolSections {
        [SymbolSection::Documentation, SymbolSection::Source]
            .into_iter()
            .chain(self.include.iter().copied())
            .filter(|section| !self.exclude.contains(section))
            .collect()
    }

    /// Selects the existing exact-symbol read that supplies the base facts.
    #[must_use]
    pub fn into_exact(self, sections: &SymbolSections) -> symbol_read::GetSymbolParams {
        symbol_read::GetSymbolParams {
            id: self.id,
            include: GetSymbolInclude {
                documentation: sections.contains(&SymbolSection::Documentation),
                history: sections.contains(&SymbolSection::History),
            },
            revision: self.revision,
            page: self.page,
            max_results: self.max_results,
        }
    }
}

/// One returned declaration with its selected exact source.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolDeclaration {
    /// The declaration location and its associations to symbol facts.
    #[serde(flatten)]
    pub binding: symbol_read::SymbolDeclaration,
    /// Exact bytes of the declaration when selected and available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceExcerpt>,
}

impl SymbolDeclaration {
    fn is_valid_for(&self, symbol: &Symbol) -> bool {
        self.binding.is_valid_for(symbol)
            && self.source.as_ref().is_none_or(|source| {
                source.span.range == self.binding.range
                    && u64::try_from(source.text.len()).is_ok_and(|bytes| {
                        self.binding.range.end - self.binding.range.start == bytes
                    })
                    && match (&self.binding.path, &self.binding.unit) {
                        (Some(path), None) => source.span.unit == SourceUnitId::project(&path.0),
                        (None, Some(unit)) => &source.span.unit == unit,
                        _ => false,
                    }
            })
    }
}

/// One known outgoing edge of an exact symbol.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolEdge {
    /// The edge kind from its provider.
    pub kind: ExactKind,
    /// Canonical identity at the target of the edge.
    pub target: SymbolId,
}

/// The exact symbol and its selected sections, even when it has no declarations.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FoundSymbol {
    /// The established logical symbol, including its canonical identity.
    pub symbol: Box<Symbol>,
    /// The revision this answer read.
    pub revision: Revision,
    /// Declaration bindings with their selected source, in deterministic order.
    #[schemars(length(max = PAGE_LIMIT_MAX))]
    pub declarations: Vec<SymbolDeclaration>,
    /// The requested page and complete declaration count.
    pub pagination: Pagination,
    /// The selected commit or release timeline, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SymbolHistory>,
    /// The selected documentation that refers to the exact symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<DocumentationContext>,
    /// Known outgoing edges when selected and available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<Vec<SymbolEdge>>,
    /// Read conditions sent to the server log.
    #[serde(skip)]
    pub warnings: Vec<ReadWarning>,
}

/// An exact symbol, an empty object for a definite miss, or a typed cause of unavailability.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
#[schemars(transform = crate::schema::require_result_object)]
#[schemars(extend("examples" = [
    {},
    {"id": "rift://symbol/local/rust/app/parser/parse", "reason": "insufficient_coverage"}
]))]
pub enum GetSymbolResult {
    /// The exact identity was established.
    Found(Box<FoundSymbol>),
    /// The exact selection could not establish presence or absence.
    Unavailable {
        /// The requested canonical identity.
        #[serde(deserialize_with = "deserialize_symbol_id")]
        #[schemars(with = "SymbolIdentity")]
        id: SymbolId,
        /// The selected revision, when established.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
        /// The cause of unavailability.
        reason: SymbolUnavailableReason,
        /// Read conditions sent to the server log.
        #[serde(skip)]
        warnings: Vec<ReadWarning>,
    },
    /// The selected revision proved the identity absent.
    Missing {
        /// Read conditions sent to the server log.
        #[serde(skip)]
        warnings: Vec<ReadWarning>,
    },
}

impl GetSymbolResult {
    /// Adapts the base exact-symbol facts to the public MCP representation.
    #[must_use]
    pub fn from_exact(result: symbol_read::GetSymbolResult, sections: &SymbolSections) -> Self {
        match result {
            symbol_read::GetSymbolResult::Found {
                mut symbol,
                revision,
                mut declarations,
                pagination,
                history,
                documentation,
                warnings,
            } => {
                if !sections.contains(&SymbolSection::Documentation) {
                    symbol.documentation.clear();
                    for declaration in &mut declarations {
                        declaration.documentation_indices.clear();
                    }
                }
                Self::Found(Box::new(FoundSymbol {
                    symbol,
                    revision,
                    declarations: declarations
                        .into_iter()
                        .map(|binding| SymbolDeclaration {
                            binding,
                            source: None,
                        })
                        .collect(),
                    pagination,
                    history: history.filter(|_| sections.contains(&SymbolSection::History)),
                    documentation: documentation
                        .filter(|_| sections.contains(&SymbolSection::Documentation)),
                    edges: None,
                    warnings,
                }))
            }
            symbol_read::GetSymbolResult::Missing { warnings, .. } => Self::Missing { warnings },
            symbol_read::GetSymbolResult::Unavailable {
                id,
                revision,
                reason,
                warnings,
            } => Self::Unavailable {
                id,
                revision,
                reason,
                warnings,
            },
        }
    }

    /// Checks the identity, revision, and projected declarations of a backend answer.
    #[must_use]
    pub fn is_valid_for(&self, requested: &SymbolId) -> bool {
        let Ok(identity) = SymbolIdentity::parse(requested.as_str()) else {
            return false;
        };
        match self {
            Self::Found(found) => {
                found.symbol.id.as_ref() == Some(requested)
                    && identity.language() == &found.symbol.language
                    && found.revision.answers_for(identity.owner())
                    && found
                        .history
                        .as_ref()
                        .is_none_or(|history| history.is_valid_for(requested))
                    && u64::try_from(found.declarations.len())
                        .is_ok_and(|length| length <= PAGE_LIMIT_MAX)
                    && found
                        .declarations
                        .iter()
                        .all(|declaration| declaration.is_valid_for(&found.symbol))
                    && found.edges.as_ref().is_none_or(|edges| {
                        edges.iter().all(|edge| edge.kind.is_valid())
                            && edges.windows(2).all(|pair| pair[0] < pair[1])
                    })
            }
            Self::Unavailable { id, revision, .. } => {
                id == requested
                    && revision
                        .as_ref()
                        .is_none_or(|revision| revision.answers_for(identity.owner()))
            }
            Self::Missing { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "rift://symbol/local/rust/app/parser/parse";
    const REVISION: &str =
        "rift://revision/tree/4e1b6c1ab5d6ddbdd1d7e3a7e38fc1bf1d2a35c6a1b0a7c8a4e0bd0c7a9b6f11";

    #[test]
    fn section_sets_apply_defaults_then_exclusions() {
        let defaults: GetSymbolParams = serde_json::from_value(json!({"id":ID})).expect("defaults");
        assert_eq!(
            defaults.selection(),
            SymbolSections::from([SymbolSection::Documentation, SymbolSection::Source])
        );
        let scoped: GetSymbolParams = serde_json::from_value(json!({
            "id": ID, "include": ["source", "history", "edges"], "exclude": ["source", "history"]
        }))
        .expect("typed sets");
        assert_eq!(
            scoped.selection(),
            SymbolSections::from([SymbolSection::Documentation, SymbolSection::Edges])
        );
        let schema = serde_json::to_value(schemars::schema_for!(GetSymbolParams)).expect("schema");
        let validator = jsonschema::validator_for(&schema).expect("validator");
        for rejected in [
            json!({"id":ID, "include":{"history":true}}),
            json!({"id":ID, "include":["history","history"]}),
            json!({"id":ID, "exclude":["other"]}),
            json!({"id":ID, "exclude":["source","source"]}),
        ] {
            assert!(!validator.is_valid(&rejected), "{rejected}");
            assert!(serde_json::from_value::<GetSymbolParams>(rejected).is_err());
        }
    }

    #[test]
    fn only_definite_miss_is_empty() -> Result<(), Box<dyn std::error::Error>> {
        let schema = serde_json::to_value(schemars::schema_for!(GetSymbolResult))?;
        let validator = jsonschema::validator_for(&schema)?;
        let requested = SymbolId::parse(ID)?;
        for value in [
            json!({}),
            json!({"id": ID, "reason":"index_preparing"}),
            json!({
                "symbol":{"id":ID,"language":"rust","name":"parse","kind":"function"},
                "revision":REVISION, "declarations":[],
                "pagination":{"page":1,"pages":0,"max_results":10,"total":0}
            }),
        ] {
            assert!(validator.is_valid(&value), "{value}");
            let answer: GetSymbolResult = serde_json::from_value(value.clone())?;
            assert!(answer.is_valid_for(&requested), "{answer:?}");
            assert_eq!(
                matches!(&answer, GetSymbolResult::Missing { .. }),
                value == json!({})
            );
            assert_eq!(serde_json::to_value(answer)?, value);
        }
        for invalid in [json!({"outcome":"missing"}), json!({"id":ID})] {
            assert!(!validator.is_valid(&invalid), "{invalid}");
            assert!(serde_json::from_value::<GetSymbolResult>(invalid).is_err());
        }
        Ok(())
    }
}
