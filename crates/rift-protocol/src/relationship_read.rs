//! Complete adjacency acquired through the existing global search operation.

use crate::read::{
    PackageIdentity, Relationship, RelationshipFacet, RuntimeIdentity, SymbolId, TraversalDirection,
};
use crate::symbol_read::{CapturedView, CapturedViewId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Most canonical subjects in one raw adjacency batch.
pub const RELATIONSHIP_SEEDS_MAX: usize = 64;
/// Capability advertised for the raw relationship branch of `/v1/search`.
pub const RELATIONSHIPS_FEATURE: &str = "relationships";

/// One relationship operation the reader establishes from its existing collected facts.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipCapability {
    /// Supported direction relative to each selected subject.
    pub direction: TraversalDirection,
    /// Facets whose complete selected-domain reads the reader supports.
    #[schemars(length(min = 1, max = 16))]
    #[schemars(extend("uniqueItems" = true))]
    pub facets: Vec<RelationshipFacet>,
}

/// Opaque identity of one original record within its selected publication.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RelationshipRecordId(pub(crate) crate::identity::SourceDigest);

impl JsonSchema for RelationshipRecordId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelationshipRecordId".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = crate::identity::SourceDigest::json_schema(generator);
        schema.insert("description".to_owned(),serde_json::Value::String(
            "Opaque full identity of one original relationship record within its selected publication.".to_owned()));
        schema
    }
}

impl RelationshipRecordId {
    /// Validates the complete digest form without exposing the provider's storage key.
    ///
    /// # Errors
    /// Returns the existing full-digest violation for invalid or shortened identities.
    pub fn parse(value: &str) -> Result<Self, rift_error::RiftError> {
        crate::identity::SourceDigest::parse(value).map(Self)
    }

    /// Complete publication-bound record identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// One original relationship record, independent of equal endpoint or evidence payloads.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipRecord {
    /// Opaque full identity scoped to the selected publication.
    pub id: RelationshipRecordId,
    /// Existing domain relationship, including evidence and physical occurrence.
    pub relationship: Relationship,
}

/// One incoming or outgoing adjacency selection, without ranking or path projection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipSelection {
    /// Established subjects whose complete adjacency is required.
    #[schemars(length(min = 1, max = 64))]
    pub seeds: Vec<SymbolId>,
    /// Direction relative to each selected subject.
    pub direction: TraversalDirection,
    /// Portable relationship facets. Empty selects every available facet.
    #[schemars(length(max = 16))]
    pub facets: Vec<RelationshipFacet>,
}

/// The exclusive raw relationship request on the existing search endpoint.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("anyOf" = [{"required": ["packages"]}, {"required": ["runtimes"]}, {"required": ["view"]}]))]
pub struct RelationshipSearchRequest {
    /// Subjects, direction and facets selected for this adjacency batch.
    pub relationships: RelationshipSelection,
    /// Exact eligible package domain. Required unless `view` retains that domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(min = 1, max = 20000))]
    pub packages: Vec<PackageIdentity>,
    /// Exact eligible runtime domain, retaining compiler or runtime versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(min = 1, max = 20000))]
    pub runtimes: Vec<RuntimeIdentity>,
    /// Requested exact projections; source bytes use the same selected publication.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1))]
    pub include: Vec<crate::read::GetSymbolInclude>,
    /// Original serving context. Continuation never selects latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedViewId>,
}

/// Original relationship records for one requested subject on this page.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipGroup {
    /// Requested canonical subject, including when its adjacency is empty.
    pub seed: SymbolId,
    /// Original records, preserving parallel edges, evidence and occurrences.
    #[schemars(length(max = 1000))]
    pub relationships: Vec<RelationshipRecord>,
    /// Physical usages, including recursive and file-level locations without a caller symbol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1000))]
    pub occurrences: Vec<crate::query::QueryOccurrence>,
    /// Provider exhausted this subject's adjacency under the selected exact domain.
    pub complete: bool,
}

/// One raw relationship page from an immutable selected publication.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipSearchPage {
    /// One authoritative group for every requested subject, in request order.
    #[schemars(length(min = 1, max = 64))]
    pub groups: Vec<RelationshipGroup>,
    /// Exact symbols reached by this page. Identical captured records may repeat across pages;
    /// conflicting records are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10000))]
    pub symbols: Vec<crate::query::QuerySymbol>,
    /// Physical source catalog and requested bytes for reached declarations and occurrences.
    /// Identical captured records may repeat across pages; conflicting records are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10000))]
    #[schemars(with = "Vec<RelationshipSourceSchema>")]
    pub sources: Vec<crate::query::QuerySource>,
    /// Opaque continuation bound to subjects, direction, facets and selected domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub next_cursor: Option<String>,
    /// Existing warnings retained without treating missing facts as complete empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub warnings: Vec<crate::read::ReadWarning>,
    /// Analyzer revision shared by every page and exact read.
    #[schemars(length(min = 1, max = 128))]
    pub analyzer_revision: String,
    /// Corpus revision shared by every page and exact read.
    #[schemars(length(min = 1, max = 128))]
    pub corpus_revision: String,
    /// Original serving context and expiry, including an empty adjacency.
    pub view: CapturedView,
}

/// Complete raw page or the existing typed unavailable outcome.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RelationshipSearchResult {
    /// Complete required domain was selected; follow every required cursor before evaluation.
    Page(RelationshipSearchPage),
    /// Required relationship facts cannot be established.
    Unavailable(crate::search_read::SearchUnavailable),
}

struct RelationshipSourceSchema;

impl JsonSchema for RelationshipSourceSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelationshipSource".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let source = generator.subschema_for::<crate::query::QuerySource>();
        schemars::json_schema!({"allOf":[source,{"required":["digest"]}]})
    }
}
