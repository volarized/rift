//! Declarations returned by the facets tool and global API.

use crate::read::{ExactKind, FacetRegion, SourceExcerpt, SyntaxElement, TextRange};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One kind: a declaration with its location, source, and syntax structure.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FacetDeclaration {
    /// Declaration ordinal in the symbol, from zero.
    pub declaration: u64,
    /// Kind of the declaration in the provider's vocabulary.
    pub kind: ExactKind,
    /// Full byte range in the source unit.
    pub range: TextRange,
    /// The declaration's location and exact source.
    pub source: SourceExcerpt,
    /// Named regions supplied by the syntax provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<FacetRegion>,
    /// The declaration's syntax structure.
    pub syntax: Vec<SyntaxElement>,
}
