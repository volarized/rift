//! The producer of a fact, and which fact kinds a publication states it covers.
//!
//! The built-in analyzer and external producers both write facts into a
//! [`PackagePublication`](crate::index::PackagePublication). An external producer is an
//! extraction program outside Rift that reads one language's source and emits
//! relationships or occurrences; a fact it wrote names it through [`FactProducer`], and a
//! fact the built-in analyzer wrote names no producer.

use std::fmt;

use rift_error::{RiftError, errors};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::read::{
    ExactKind, Language, RelationshipDerivation, RelationshipFacet, SourceUnitId, SourceUnitSpan,
    SymbolId, TextRange,
};

/// Longest producer name, in bytes.
pub const PRODUCER_NAME_BYTES_MAX: usize = 64;

/// Longest producer revision, in bytes.
pub const PRODUCER_REVISION_BYTES_MAX: usize = 128;

/// Wire pattern of a producer name: a lowercase ASCII letter, then lowercase letters,
/// digits, `.`, `_`, or `-`.
pub const PRODUCER_NAME_PATTERN: &str = r"^[a-z][a-z0-9._-]*$";

/// Wire pattern of a producer revision: an ASCII letter or digit, then letters, digits,
/// `.`, `_`, `+`, or `-`.
pub const PRODUCER_REVISION_PATTERN: &str = r"^[A-Za-z0-9][A-Za-z0-9._+-]*$";

/// The field a refusal names for a producer name.
const NAME_FIELD: &str = "producer.name";

/// The field a refusal names for a producer revision.
const REVISION_FIELD: &str = "producer.revision";

/// Reason a producer name or revision breaks the form its schema advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerViolation {
    /// The value is empty or longer than its byte bound.
    Length,
    /// The value carries a character its pattern does not allow.
    Character,
}

impl ProducerViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Character => "character",
        }
    }
}

/// One spelling rule: a byte bound, the bytes allowed first, and the bytes allowed after.
struct Spelling {
    field: &'static str,
    bytes_max: usize,
    first: fn(u8) -> bool,
    rest: fn(u8) -> bool,
}

const NAME_SPELLING: Spelling = Spelling {
    field: NAME_FIELD,
    bytes_max: PRODUCER_NAME_BYTES_MAX,
    first: |byte| byte.is_ascii_lowercase(),
    rest: |byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte),
};

const REVISION_SPELLING: Spelling = Spelling {
    field: REVISION_FIELD,
    bytes_max: PRODUCER_REVISION_BYTES_MAX,
    first: |byte| byte.is_ascii_alphanumeric(),
    rest: |byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte),
};

impl Spelling {
    fn violation(&self, value: &str) -> Option<ProducerViolation> {
        match value.as_bytes() {
            [] => Some(ProducerViolation::Length),
            bytes if bytes.len() > self.bytes_max => Some(ProducerViolation::Length),
            [first, rest @ ..]
                if !(self.first)(*first) || !rest.iter().all(|b| (self.rest)(*b)) =>
            {
                Some(ProducerViolation::Character)
            }
            _ => None,
        }
    }

    fn accept(&self, value: &str) -> Result<String, RiftError> {
        match self.violation(value) {
            Some(violation) => Err(errors::analysis::producer_fact_invalid()
                .field(self.field)
                .violation(violation.as_str())
                .error()),
            None => Ok(value.to_owned()),
        }
    }
}

/// Name of one external producer, as `demo-indexer`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProducerName(pub(crate) String);

/// Revision of one external producer, as `0.6.1` or `0.6.1+3f2a9c1`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProducerRevision(pub(crate) String);

impl ProducerName {
    /// Accepts one producer name.
    ///
    /// # Errors
    /// Returns `producer_fact_invalid` naming `producer.name` and the
    /// [`ProducerViolation`] it breaks.
    pub fn parse(value: &str) -> Result<Self, RiftError> {
        NAME_SPELLING.accept(value).map(Self)
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProducerRevision {
    /// Accepts one producer revision.
    ///
    /// # Errors
    /// Returns `producer_fact_invalid` naming `producer.revision` and the
    /// [`ProducerViolation`] it breaks.
    pub fn parse(value: &str) -> Result<Self, RiftError> {
        REVISION_SPELLING.accept(value).map(Self)
    }

    /// The revision as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! producer_spelling_wire {
    ($type:ty, $name:literal, $bound:expr, $pattern:expr, $description:literal, $example:literal) => {
        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $type {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": $bound,
                    "pattern": $pattern,
                    "description": $description,
                    "examples": [$example]
                })
            }
        }
    };
}

producer_spelling_wire!(
    ProducerName,
    "ProducerName",
    PRODUCER_NAME_BYTES_MAX,
    PRODUCER_NAME_PATTERN,
    "Name of one external producer: a lowercase ASCII letter, then lowercase letters, digits, `.`, `_`, or `-`.",
    "demo-indexer"
);

producer_spelling_wire!(
    ProducerRevision,
    "ProducerRevision",
    PRODUCER_REVISION_BYTES_MAX,
    PRODUCER_REVISION_PATTERN,
    "Revision of one external producer: an ASCII letter or digit, then letters, digits, `.`, `_`, `+`, or `-`.",
    "0.6.1"
);

/// The external program that produced a fact, and its revision. A fact without one was
/// produced by Rift's built-in analyzer.
#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(deny_unknown_fields)]
pub struct FactProducer {
    /// The producer's name, stable across its revisions.
    pub name: ProducerName,
    /// The producer revision that emitted the fact.
    pub revision: ProducerRevision,
}

/// One kind of fact a publication may carry beside its declarations.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// Edges between two symbols, in `relationships`.
    Relationships,
    /// Places a unit's source names a symbol, in `occurrences`.
    Occurrences,
}

/// One fact kind a publication covers for one language, and the producer of those facts.
///
/// A covered kind with no facts means its producer found none. A kind no entry names was
/// not produced, so its absence says nothing about the source. Two producers of one kind
/// take two entries.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FactCoverage {
    /// The fact kind covered.
    pub kind: FactKind,
    /// The external producer of the facts this entry covers. Absent for the facts the
    /// built-in analyzer produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<FactProducer>,
}

/// Facts one external producer emitted for one language of one publication.
///
/// An importer resolves each fact against the publication's source units, names the
/// producer on every fact it adds, and records the covered kinds in the language's
/// coverage. A fact may name a source range in place of the symbol it starts at; the
/// importer resolves the range to the declaration enclosing it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerFacts {
    /// The producer that emitted every fact here.
    pub producer: FactProducer,
    /// The language whose source the producer read.
    pub language: Language,
    /// The fact kinds the producer covered for `language`, including kinds it found no
    /// fact of.
    pub kinds: Vec<FactKind>,
    /// Edges the producer found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relationships: Vec<ProducerRelationship>,
    /// Places the producer found a unit's source naming a symbol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<ProducerOccurrence>,
}

/// One edge an external producer found.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerRelationship {
    /// The symbol the edge starts at. Absent when the producer named only `occurrence`:
    /// the importer then starts the edge at the declaration enclosing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SymbolId>,
    /// What the edge is in the producer's vocabulary, such as `call`.
    pub kind: ExactKind,
    /// Portable classification of the edge.
    pub facets: Vec<RelationshipFacet>,
    /// The symbol the edge points at.
    pub to: SymbolId,
    /// Where the edge was read from. Required when `from` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<SourceUnitSpan>,
    /// How the producer established the edge.
    pub derivation: RelationshipDerivation,
    /// How likely a `heuristic` edge is to hold, from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// One place an external producer found a unit's source naming a symbol.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerOccurrence {
    /// The symbol the source names.
    pub symbol: SymbolId,
    /// The unit carrying the occurrence.
    pub unit: SourceUnitId,
    /// The occurrence's byte range inside `unit`.
    pub range: TextRange,
}

#[cfg(test)]
mod tests;
