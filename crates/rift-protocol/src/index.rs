//! The canonical package publication: the logical package index one analyzer run emits.
//!
//! A [`PackagePublication`] is what a package analyzer produces from one package's source
//! bytes and nothing else. The local package index stores or derives its searchable rows
//! from it, and a later global ingestion consumes the same shape, so physical storage may
//! differ between the two while this model does not.
//!
//! Every collection is in stable identity order and every stored record carries a digest
//! over its own canonical content, so two publications are comparable record by record.
//! The whole publication renders as RFC 8785 canonical JSON through
//! [`canonical_json`](crate::canonical::canonical_json), and carries no host-absolute
//! path: a unit is addressed by its package-relative path and its
//! [`SourceUnitId`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{
    Digest, Documentation, ExactKind, Language, PackageIdentity, ProjectPath, Signature,
    SourceUnitId, SymbolId, SymbolOrigin, TextRange,
};

/// The publication shape this revision of the analyzer emits.
///
/// The schema advertises a minimum of one, and this constant is the value every
/// publication carries.
///
/// It changes only when the publication shape or a record identity changes in a way a
/// consumer cannot read past: a consumer that meets a higher revision than it knows
/// refuses the publication rather than reading fields it cannot place.
pub const PACKAGE_PUBLICATION_FORMAT_REVISION: u32 = 1;

/// Source units one publication may carry.
pub const PACKAGE_UNITS_MAX: u32 = 100_000;
/// Symbols one publication may carry.
pub const PACKAGE_SYMBOLS_MAX: u32 = 1_000_000;
/// Search documents one publication may carry: one per symbol plus one per selected file.
pub const PACKAGE_DOCUMENTS_MAX: u32 = 1_100_000;
/// Analysis warnings one publication may carry.
pub const PACKAGE_WARNINGS_MAX: u32 = 256;
/// Bytes one record's retained source may hold.
pub const PACKAGE_SOURCE_BYTES_MAX: u32 = 1 << 20;
/// Ranking terms one search document may carry.
pub const PACKAGE_IDENTIFIER_TERMS_MAX: u32 = 1_024;

/// One package's analyzed source, as the analyzer publishes it.
///
/// `Eq` is not derived: [`PackageSymbol`] is not `Eq`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackagePublication {
    /// The publication shape this document follows. A consumer refuses a revision it does
    /// not know.
    #[schemars(range(min = 1_u32))]
    pub format_revision: u32,
    /// The analyzer that produced it: the digest of the grammar versions and extraction
    /// source the run depended on. Two publications with equal package bytes and equal
    /// `analyzer_revision` are equal.
    pub analyzer_revision: Digest,
    /// The package as its manager identifies it.
    pub package: PackageIdentity,
    /// The digest of the package source this publication was produced from, over every
    /// unit's path and content in unit order.
    pub source_digest: Digest,
    /// Every analyzed source unit, in unit identity order.
    #[schemars(length(max = 100_000))]
    pub units: Vec<PackageSourceUnit>,
    /// Every parsed declaration, in symbol identity order. Container declarations are
    /// kept whatever their visibility, so a consumer can assemble a read.
    #[schemars(length(max = 1_000_000))]
    pub symbols: Vec<PackageSymbol>,
    /// Every search document, in document identity order.
    #[schemars(length(max = 1_100_000))]
    pub documents: Vec<PackageDocument>,
    /// What the analyzer could not do, in the order it met each. Absent when the run
    /// analyzed every selected file whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub warnings: Vec<PackageAnalysisWarning>,
}

/// One analyzed file of a package.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSourceUnit {
    /// The unit this file is filed under.
    pub unit: SourceUnitId,
    /// The file's path relative to the package root.
    pub path: ProjectPath,
    /// The language whose provider parsed it.
    pub language: Language,
    /// The digest of the file's whole bytes, whatever `source` retained.
    pub content_digest: Digest,
    /// The file's source, cut at the analyzer's retained-source bound.
    #[schemars(length(max = 1_048_576))]
    pub source: String,
    /// Whether `source` holds the whole file. `false` means the bytes were cut at the
    /// bound; `content_digest` still covers the whole file.
    pub source_complete: bool,
    /// The digest of this record's canonical content, so two publications compare record
    /// by record.
    pub digest: Digest,
}

/// One declaration a package's source carries.
///
/// `Eq` is not derived: [`Signature`] carries a confidence value, so it is not `Eq`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSymbol {
    /// The declaration's identity.
    pub symbol: SymbolId,
    /// Where the declaration came from: the package, and whether it was authored.
    pub origin: SymbolOrigin,
    /// The unit the declaration was read from.
    pub unit: SourceUnitId,
    /// The declared short name, as the grammar spells it.
    pub name: String,
    /// The container-qualified name, unique within the unit's symbol space.
    pub qualified_name: String,
    /// The provider's own kind word, composed as `{language}.{kind}`.
    pub kind: ExactKind,
    /// The declaration's byte range inside its unit.
    pub range: TextRange,
    /// The one-based line the declaration starts on.
    pub line: u64,
    /// The rendered signature, when the declaration has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    /// The documentation attached to the declaration, when the grammar attaches any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<Documentation>,
    /// The declaration's own source, cut at [`PACKAGE_SOURCE_BYTES_MAX`].
    #[schemars(length(max = 1_048_576))]
    pub source: String,
    /// Whether `source` holds the whole declaration.
    pub source_complete: bool,
    /// Whether the declaration is public under its language's own rules. A consumer
    /// answering a package read serves public declarations alone.
    pub public: bool,
    /// The digest of this record's canonical content.
    pub digest: Digest,
}

/// What one search document ranks.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PackageDocumentKind {
    /// One public declaration.
    Symbol,
    /// One selected file, ranked by its content.
    File,
}

/// One searchable record, carrying the fields a ranking reads.
///
/// A field a provider does not supply is absent rather than filled from a neighbor: an
/// absent signature is an absent signature, never the declaration source repeated.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDocument {
    /// The document's stable key: the symbol identity for a declaration, the unit
    /// identity for a file.
    pub identity: String,
    /// What the document ranks.
    pub kind: PackageDocumentKind,
    /// The unit the document was built from.
    pub unit: SourceUnitId,
    /// The language whose provider parsed the unit.
    pub language: Language,
    /// The package the document belongs to.
    pub package: PackageIdentity,
    /// The digest of the content this document was built from.
    pub content_digest: Digest,
    /// The declaration name, or the file name including its extension.
    pub name: String,
    /// The container-qualified name. Absent on a file document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    /// The words `name` and `qualified_name` split into on case and separator boundaries,
    /// lowercased, deduplicated, in first-seen order, at most
    /// [`PACKAGE_IDENTIFIER_TERMS_MAX`] of them. Kept apart from the names so a ranking
    /// counts each term once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1_024))]
    pub identifier_terms: Vec<String>,
    /// The rendered signature, when the declaration has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The attached documentation body, when the grammar attaches any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// The declaration's own source. Absent on a file document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1_048_576))]
    pub declaration_source: Option<String>,
    /// The file's content. Absent on a symbol document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1_048_576))]
    pub file_content: Option<String>,
    /// The digest of this record's canonical content.
    pub digest: Digest,
}

/// What an analyzer run could not do.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "code", deny_unknown_fields, rename_all = "snake_case")]
pub enum PackageAnalysisWarning {
    /// One selected file was left out: no shipped provider parses its extension, its
    /// bytes are not valid UTF-8, or its parse was refused.
    UnitUnavailable {
        /// The file the analyzer left out, relative to the package root.
        path: ProjectPath,
        /// Why it was left out - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// A record's retained source was cut at the analyzer's bound. The record's own
    /// digest still covers the whole content it was built from.
    SourceTruncated {
        /// The file whose source was cut.
        path: ProjectPath,
        /// Bytes the record did not retain.
        dropped: u64,
    },
    /// The run reached a collection bound and stopped there, so records past it are
    /// absent from this publication.
    PublicationTruncated {
        /// The collection that reached its bound: `units`, `symbols`, or `documents`.
        #[schemars(length(max = 64))]
        collection: String,
        /// The bound the collection stopped at.
        bound: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        PACKAGE_DOCUMENTS_MAX, PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_SOURCE_BYTES_MAX,
        PACKAGE_SYMBOLS_MAX, PACKAGE_UNITS_MAX, PACKAGE_WARNINGS_MAX, PackageAnalysisWarning,
        PackageDocumentKind, PackagePublication,
    };
    use crate::read::ProjectPath;
    use serde_json::json;

    /// `#[schemars(length(max = ...))]` takes only literals, so this pins every literal
    /// the publication advertises back to the constant that states what it means.
    #[test]
    fn test_publication_schema_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(PackagePublication)).expect("schema");
        let properties = &schema["properties"];
        let cases = [
            ("units", &properties["units"]["maxItems"], PACKAGE_UNITS_MAX),
            (
                "symbols",
                &properties["symbols"]["maxItems"],
                PACKAGE_SYMBOLS_MAX,
            ),
            (
                "documents",
                &properties["documents"]["maxItems"],
                PACKAGE_DOCUMENTS_MAX,
            ),
            (
                "warnings",
                &properties["warnings"]["maxItems"],
                PACKAGE_WARNINGS_MAX,
            ),
        ];
        for (field, found, expected) in cases {
            assert_eq!(*found, json!(expected), "{field}");
        }
        assert_eq!(properties["format_revision"]["minimum"], json!(1));
        let unit = &schema["$defs"]["PackageSourceUnit"]["properties"];
        assert_eq!(
            unit["source"]["maxLength"],
            json!(PACKAGE_SOURCE_BYTES_MAX),
            "a retained source carries the publication's own byte bound"
        );
        let document = &schema["$defs"]["PackageDocument"]["properties"];
        assert_eq!(
            document["identifier_terms"]["maxItems"],
            json!(PACKAGE_IDENTIFIER_TERMS_MAX),
            "the ranking terms carry the bound the analyzer cuts them at"
        );
    }

    /// Every bound the publication advertises is one the analyzer holds to. A field
    /// carrying whatever a grammar produced advertises none, so a publication Rift
    /// produces validates against the schema Rift serves.
    #[test]
    fn test_no_record_field_advertises_an_unenforced_length() {
        let schema =
            serde_json::to_value(schemars::schema_for!(PackagePublication)).expect("schema");
        let cases = [
            ("PackageSymbol", ["name", "qualified_name"].as_slice()),
            (
                "PackageDocument",
                [
                    "identity",
                    "name",
                    "qualified_name",
                    "signature",
                    "documentation",
                ]
                .as_slice(),
            ),
        ];
        for (model, fields) in cases {
            let properties = &schema["$defs"][model]["properties"];
            for field in fields {
                assert!(
                    properties[field]["maxLength"].is_null(),
                    "{model}.{field} advertises a length nothing enforces: {properties:#}"
                );
            }
        }
    }

    #[test]
    fn test_document_kind_spells_symbol_and_file() {
        for (kind, spelling) in [
            (PackageDocumentKind::Symbol, "symbol"),
            (PackageDocumentKind::File, "file"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                json!(spelling)
            );
        }
    }

    #[test]
    fn test_analysis_warning_round_trips_under_its_code_tag() {
        let warning = PackageAnalysisWarning::SourceTruncated {
            path: ProjectPath("src/lib.rs".to_owned()),
            dropped: 12,
        };
        let wire = json!({
            "code": "source_truncated",
            "path": "src/lib.rs",
            "dropped": 12,
        });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: PackageAnalysisWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }
}
