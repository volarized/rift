//! Complete adjacency acquired through the existing global search operation.

use crate::read::{
    Edge, EdgeKind, PackageIdentity, RuntimeIdentity, SourceUnitId, SourceUnitSpan, Symbol,
    SymbolId, TraversalDirection,
};
use crate::symbol_read::{CapturedView, CapturedViewId, SymbolDeclaration};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default canonical subjects in one raw adjacency batch, before accepted policy applies.
pub const EDGE_SEEDS_MAX: usize = 64;
/// Capability advertised for the raw edge branch of `/v1/search`.
pub const EDGES_FEATURE: &str = "edges";

/// Positive count bounds for one raw edge operation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeBounds {
    /// Most subjects in one request.
    #[schemars(range(min = 1))]
    pub max_seeds: u64,
    /// Most selected kinds in one request.
    #[schemars(range(min = 1))]
    pub max_kinds: u64,
    /// Most edge projections in one group.
    #[schemars(range(min = 1))]
    pub max_edges: u64,
    /// Most occurrence projections in one group.
    #[schemars(range(min = 1))]
    pub max_occurrences: u64,
    /// Most original symbol records in one page.
    #[schemars(range(min = 1))]
    pub max_symbols: u64,
    /// Most original source units in one page.
    #[schemars(range(min = 1))]
    pub max_sources: u64,
}

/// One edge operation the reader establishes from its existing collected facts.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeCapability {
    /// Supported direction relative to each selected subject.
    pub direction: TraversalDirection,
    /// Kinds whose complete selected-domain reads the reader supports.
    #[schemars(length(min = 1))]
    #[schemars(extend("uniqueItems" = true))]
    pub kinds: Vec<EdgeKind>,
    /// Accepted count bounds for this operation. Absence does not establish raw read support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<EdgeBounds>,
}

/// Opaque identity of one merged edge or physical usage within its selected publication.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EdgeRecordId(pub(crate) crate::identity::SourceDigest);

impl JsonSchema for EdgeRecordId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EdgeRecordId".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = crate::identity::SourceDigest::json_schema(generator);
        schema.insert(
            "description".to_owned(),
            serde_json::Value::String(
                "Opaque full identity of one merged edge or physical usage within its selected publication."
                    .to_owned(),
            ),
        );
        schema
    }
}

impl EdgeRecordId {
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

/// One edge merged by source, exact kind, target and physical occurrence.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeRecord {
    /// Opaque full identity scoped to the selected publication.
    pub id: EdgeRecordId,
    /// Existing domain edge, including evidence and physical occurrence.
    pub edge: Edge,
}

/// One physical usage projected from an original selected-publication record.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeOccurrence {
    /// Complete original record identity. Projections of the same record share this value.
    pub id: EdgeRecordId,
    /// Related original edge, only when the stored record establishes that link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeRecordId>,
    /// Physical usage. Equal locations do not establish equal original record identities.
    pub occurrence: QueryOccurrence,
}

/// One incoming or outgoing adjacency selection, without ranking or path projection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSelection {
    /// Established subjects whose complete adjacency is required.
    #[schemars(length(min = 1))]
    pub seeds: Vec<SymbolId>,
    /// Direction relative to each selected subject.
    pub direction: TraversalDirection,
    /// Portable edge kinds. Empty selects every available kind.
    #[schemars(extend("uniqueItems" = true))]
    pub kinds: Vec<EdgeKind>,
}

/// The optional projections of read records. An omitted member is `false`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(example = serde_json::json!({"source": true}))]
pub struct EdgeInclude {
    /// Attaches the sorted analyses that produced each merged edge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub providers: bool,
    /// Attaches source text to the selected records.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source: bool,
}

impl EdgeInclude {
    /// Whether the caller requested no projection.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.source && !self.providers
    }
}

/// One physical reference or call supplied by a selected provider.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryOccurrence {
    /// The canonical symbol referenced at this location.
    pub target: SymbolId,
    /// The enclosing declaration, when the provider can establish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<SymbolId>,
    /// The exact source unit and UTF-8 byte range of the occurrence.
    pub source: SourceUnitSpan,
    /// The edge established by this occurrence.
    pub kind: EdgeKind,
}

/// A symbol together with the physical declarations selected for this read.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySymbol {
    /// The logical symbol's current selected facts.
    pub symbol: Symbol,
    /// Physical declarations, preserving their association indices.
    pub declarations: Vec<SymbolDeclaration>,
}

/// Selected source bytes, when their projection was requested.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySource {
    /// The physical source owner and path.
    pub unit: SourceUnitId,
    /// Full digest of the selected physical bytes, including when text is unrequested.
    pub digest: crate::identity::SourceDigest,
    /// Exact UTF-8 text of the selected source unit, absent when unavailable or unrequested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The exclusive raw edge request on the existing search endpoint.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("anyOf" = [{"required": ["packages"]}, {"required": ["runtimes"]}, {"required": ["view"]}]))]
pub struct EdgeSearchRequest {
    /// Subjects, direction and kinds selected for this adjacency batch.
    pub edges: EdgeSelection,
    /// Exact eligible package domain. Required unless `view` retains that domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(min = 1, max = 20000))]
    pub packages: Vec<PackageIdentity>,
    /// Exact eligible runtime domain, retaining compiler or runtime versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(min = 1, max = 20000))]
    pub runtimes: Vec<RuntimeIdentity>,
    /// Requested exact projections; source bytes use the same selected publication.
    #[serde(default, skip_serializing_if = "EdgeInclude::is_empty")]
    pub include: EdgeInclude,
    /// Original serving context. Continuation never selects latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedViewId>,
}

/// Merged edge records for one requested subject on this page.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeGroup {
    /// Requested canonical subject, including when its adjacency is empty.
    pub seed: SymbolId,
    /// Merged records, preserving distinct kinds and physical occurrences. Producer rows of the same edge consume one slot.
    pub edges: Vec<EdgeRecord>,
    /// Physical usages, including recursive and file-level locations without a caller symbol.
    /// Page limits count distinct merged edges and physical usages across their record identities.
    /// Multiple projections of one original record consume one slot; independent records
    /// consume separate slots even when their payloads or locations are equal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<EdgeOccurrence>,
    /// Provider exhausted this subject's adjacency and established complete coverage.
    /// A page with `edge_coverage_missing` can carry retained witnesses with `complete: false`.
    pub complete: bool,
}

/// One raw edge page from an immutable selected publication.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSearchPage {
    /// One authoritative group for every requested subject, in request order.
    #[schemars(length(min = 1))]
    pub groups: Vec<EdgeGroup>,
    /// Exact symbols reached by this page. Identical captured records may repeat across pages;
    /// conflicting records are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<QuerySymbol>,
    /// Physical source catalog and requested bytes for reached declarations and occurrences.
    /// Identical captured records may repeat across pages; conflicting records are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<QuerySource>,
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

/// A raw page or the existing typed unavailable outcome.
/// An unsupported operation uses the declared `503 application/problem+json` response
/// with the registered `rift.server.read_unsupported` value in `code`. Missing required
/// facts use the typed unavailable outcome, including `insufficient_coverage`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EdgeSearchResult {
    /// Bounded records from the selected domain. Incomplete coverage is explicit in the
    /// affected groups and requires a coverage warning and at least one witnessed record.
    Page(EdgeSearchPage),
    /// Required edge facts cannot be established.
    Unavailable(crate::search_read::SearchUnavailable),
}
