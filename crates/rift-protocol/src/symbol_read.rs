//! Exact logical symbol reads and their declaration projections.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    documentation::DocumentationContext,
    read::{
        GetSymbolInclude, NodeId, PAGE_LIMIT_MAX, ProjectPath, ReadWarning, RevisionId,
        SourceUnitId, Symbol, SymbolHistory, SymbolId, SymbolOrigin, TextRange,
    },
};

/// A service-issued key for one immutable view in one serving context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct CapturedViewId(String);

impl JsonSchema for CapturedViewId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CapturedViewId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 3,
            "maxLength": 4096,
            "pattern": "^(?:[0-9a-f]{64}|[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+)$",
            "description": "A service-issued key for one immutable view in one serving context."
        })
    }
}

impl CapturedViewId {
    /// Accepts a complete local digest or an opaque service-issued view key.
    ///
    /// # Errors
    /// Returns an error for a malformed or oversized view key.
    pub fn parse(value: &str) -> Result<Self, String> {
        let bounded = value.len() <= 4096;
        let digest = bounded
            && value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let opaque = bounded
            && value.split_once('.').is_some_and(|(claims, signature)| {
                [claims, signature].iter().all(|component| {
                    !component.is_empty()
                        && component
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
            });
        if value.len() > 4096 || (!digest && !opaque) {
            return Err(
                "invalid captured view; supply the complete view key returned by a read".to_owned(),
            );
        }
        Ok(Self(value.to_owned()))
    }

    /// The complete context-bound view key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CapturedViewId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<CapturedViewId> for String {
    fn from(value: CapturedViewId) -> Self {
        value.0
    }
}

/// The immutable serving view and the expiry promised by its owner.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedView {
    /// Selects the same workspace, owner, applicability and accepted index revision.
    pub id: CapturedViewId,
    /// RFC 3339 expiry. The service retains the view's required facts until this time.
    #[schemars(extend("format" = "date-time"))]
    pub expires_at: String,
}

/// Reads one exact logical symbol obtained from discovery or another read.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetSymbolParams {
    /// Canonical identity. Ownership and language come from this address.
    #[serde(deserialize_with = "deserialize_symbol_id")]
    #[schemars(with = "crate::identity::SymbolIdentity")]
    pub id: SymbolId,
    /// Requested projections. Omitted defaults to source; an empty list requests none.
    #[serde(default = "default_include")]
    pub include: Vec<GetSymbolInclude>,
    /// A local Git revision. Refused for a package or runtime owner and beside `view`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
    /// A view returned by a preceding read. Expiry never selects latest implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedViewId>,
    /// Most declarations returned in this read. Omitted uses the existing five-item bound.
    #[serde(default = "default_declaration_limit")]
    #[schemars(range(min = 1_u64, max = PAGE_LIMIT_MAX))]
    pub declaration_limit: u64,
    /// Continuation bound to this exact symbol, projections and serving view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub declaration_cursor: Option<String>,
}

pub(crate) fn deserialize_symbol_id<'de, D>(deserializer: D) -> Result<SymbolId, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    SymbolId::parse(&value).map_err(|violation| {
        serde::de::Error::custom(format!(
            "invalid symbol identity: {violation:?}; supply a canonical rift://symbol/ address"
        ))
    })
}

fn default_include() -> Vec<GetSymbolInclude> {
    vec![GetSymbolInclude::Source]
}

fn default_declaration_limit() -> u64 {
    5
}

/// One source binding for a logical object. Source-less objects have no bindings.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolDeclaration {
    /// Contextual owner and source kind of this declaration's source.
    pub origin: SymbolOrigin,
    /// Repository-relative path. Exactly one of `path` and `unit` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ProjectPath>,
    /// Released source unit. Exactly one of `path` and `unit` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SourceUnitId>,
    /// The declaration's complete byte range in its source.
    pub range: TextRange,
    /// One-based line where this declaration begins.
    #[schemars(range(min = 1_u64))]
    pub line: u64,
    /// Source node when available and requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// Source excerpt when requested and available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 67_108_864))]
    pub source: Option<String>,
    /// Whether the supplied excerpt contains this declaration's complete source.
    pub source_complete: bool,
    /// Entries in the result symbol's signature array supplied by this declaration.
    pub signature_indices: Vec<u64>,
    /// Entries in the result symbol's type array supplied by this declaration.
    pub type_indices: Vec<u64>,
    /// Entries in the result symbol's documentation array supplied by this declaration.
    pub documentation_indices: Vec<u64>,
}

impl SymbolDeclaration {
    /// Checks location and projection references against their one owning symbol.
    #[must_use]
    pub fn is_valid_for(&self, symbol: &Symbol) -> bool {
        self.path.is_some() != self.unit.is_some()
            && self.range.start <= self.range.end
            && self.line > 0
            && (!self.source_complete || self.source.is_some())
            && indices_fit(&self.signature_indices, symbol.signatures.len())
            && indices_fit(&self.type_indices, symbol.types.len())
            && indices_fit(&self.documentation_indices, symbol.documentation.len())
    }
}

fn indices_fit(indices: &[u64], length: usize) -> bool {
    indices.windows(2).all(|pair| pair[0] < pair[1])
        && indices
            .iter()
            .all(|index| usize::try_from(*index).is_ok_and(|index| index < length))
}

/// The structural parts compared when an alternative was selected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolMatchContext {
    /// Defining package, runtime or local scope.
    Owner,
    /// Language and dialect.
    Language,
    /// Semantic module path.
    Module,
    /// Owning container path.
    Container,
    /// Callable dispatch form.
    Form,
    /// Terminal declared name.
    Name,
    /// Exact release of the same defining owner.
    Version,
}

/// An actual object proposed after a valid exact miss.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolAlternative {
    /// The candidate's actual canonical identity, including its exact owner and version.
    #[serde(deserialize_with = "deserialize_symbol_id")]
    #[schemars(with = "crate::identity::SymbolIdentity")]
    pub id: SymbolId,
    /// The candidate's displayed name.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// Structural comparisons that explain this candidate.
    pub match_context: Vec<SymbolMatchContext>,
}

/// Definite absence after a complete exact selection, with bounded alternatives.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolNotFound {
    /// The valid exact identity requested by the caller.
    #[serde(deserialize_with = "deserialize_symbol_id")]
    #[schemars(with = "crate::identity::SymbolIdentity")]
    pub id: SymbolId,
    /// Actual structural candidates, never substitutions for the requested object.
    #[schemars(length(max = 3))]
    pub alternatives: Vec<SymbolAlternative>,
    /// Unfinished candidate work, when the complete nearest set could not be proved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub detail: Option<String>,
}

/// Why an exact selection cannot establish presence or definite absence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolUnavailableReason {
    /// The encoded exact release has no accepted publication.
    ExactRelease,
    /// The selected index is preparing.
    IndexPreparing,
    /// The serving read failed.
    ReadFailed,
    /// The publication failed validation.
    CorruptPublication,
    /// Accepted facts do not cover this exact selection.
    InsufficientCoverage,
    /// The requested captured view expired or was explicitly removed.
    ViewExpired,
    /// The captured view belongs to another workspace or scope mapping.
    ViewContext,
}

/// Exactly one logical object, definite absence, or explicit selection unavailability.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum GetSymbolResult {
    /// The exact identity was established, even when it has no source declarations.
    Found {
        /// One logical symbol. Its established `id` is required by read validation.
        symbol: Box<Symbol>,
        /// The immutable context used for this result and its continuations.
        view: CapturedView,
        /// Declaration bindings in deterministic source order.
        #[schemars(length(max = PAGE_LIMIT_MAX))]
        declarations: Vec<SymbolDeclaration>,
        /// Continuation for remaining declarations in this exact view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1, max = 4096))]
        declaration_cursor: Option<String>,
        /// Requested timeline for the logical symbol.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history: Option<SymbolHistory>,
        /// Requested bounded documentation context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        documentation: Option<DocumentationContext>,
        /// Projection cuts and other typed read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
    /// The valid identity is absent from the completely searched exact selection.
    Missing {
        /// Definite exact miss and its actual structural candidates.
        symbol_not_found: SymbolNotFound,
        /// The exact immutable selection searched.
        view: CapturedView,
        /// Warnings, including incomplete candidate work.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
    /// The exact selection cannot establish presence or absence.
    Unavailable {
        /// The requested exact canonical identity.
        #[serde(deserialize_with = "deserialize_symbol_id")]
        #[schemars(with = "crate::identity::SymbolIdentity")]
        id: SymbolId,
        /// The selected view, when one could be captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<CapturedView>,
        /// The typed cause of unavailability.
        reason: SymbolUnavailableReason,
        /// Details and available partial-read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
}

impl GetSymbolResult {
    /// Checks exact identity and declaration associations at a serving boundary.
    #[must_use]
    pub fn is_valid_for(&self, requested: &SymbolId) -> bool {
        match self {
            Self::Found {
                symbol,
                declarations,
                ..
            } => {
                symbol.id.as_ref() == Some(requested)
                    && crate::identity::SymbolIdentity::parse(requested.as_str())
                        .is_ok_and(|identity| identity.language() == &symbol.language)
                    && u64::try_from(declarations.len())
                        .is_ok_and(|length| length <= PAGE_LIMIT_MAX)
                    && declarations
                        .iter()
                        .all(|declaration| declaration.is_valid_for(symbol))
            }
            Self::Missing {
                symbol_not_found, ..
            } => {
                symbol_not_found.id == *requested
                    && SymbolId::parse(requested.as_str()).is_ok()
                    && symbol_not_found.alternatives.len() <= 3
                    && symbol_not_found
                        .alternatives
                        .iter()
                        .all(|candidate| SymbolId::parse(candidate.id.as_str()).is_ok())
            }
            Self::Unavailable { id, .. } => id == requested && SymbolId::parse(id.as_str()).is_ok(),
        }
    }
}

#[cfg(test)]
mod tests;
