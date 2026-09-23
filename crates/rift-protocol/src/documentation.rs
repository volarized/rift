//! Documentation metadata over existing symbol and file content.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{Digest, Language, ProjectPath, SourceUnitId, SymbolId, SymbolOrigin, TextRange};

/// Full lowercase SHA-256 digest for documentation content and identity.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct DocumentationDigest(
    #[schemars(length(min = 64, max = 64))]
    #[schemars(example = &"3f9a1c2e7b4d6a09876543210fedcba9876543210fedcba9876543210fedcba")]
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub String,
);

/// Selected sources one collection accepts.
pub const DOCUMENTATION_SOURCES_MAX: u32 = 100_000;
/// Source bytes one parser accepts.
pub const DOCUMENTATION_SOURCE_BYTES_MAX: u32 = 4 << 20;
/// Source bytes one collection accepts together.
pub const DOCUMENTATION_TOTAL_BYTES_MAX: u64 = 512 << 20;
/// Blocks one collection retains.
pub const DOCUMENTATION_BLOCKS_MAX: u32 = 100_000;
/// Links or reference candidates one collection retains.
pub const DOCUMENTATION_REFERENCES_MAX: u32 = 100_000;
/// Heading levels one block retains.
pub const DOCUMENTATION_HEADING_DEPTH_MAX: u32 = 512;
/// Bytes one authored heading, destination, or reference spelling retains.
pub const DOCUMENTATION_TEXT_BYTES_MAX: u32 = 4_096;
/// Warnings one collection retains.
pub const DOCUMENTATION_WARNINGS_MAX: u32 = 256;
/// References one symbol read retains.
pub const DOCUMENTATION_SYMBOL_REFERENCES_MAX: u32 = 32;
/// Bytes one documentation excerpt retains.
pub const DOCUMENTATION_EXCERPT_BYTES_MAX: u32 = 16 << 10;
/// License files one selected source may name.
pub const DOCUMENTATION_LICENSE_FILES_MAX: u32 = 256;
/// Bytes an authored notebook cell identifier may carry.
pub const NOTEBOOK_CELL_ID_BYTES_MAX: u32 = 64;
/// Physical JSON string ranges one selected cell may carry.
pub const NOTEBOOK_SOURCE_RANGES_MAX: u32 = 250_000;

/// The selected source's format.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationSourceFormat {
    /// Markdown block and inline syntax.
    Markdown,
    /// Recognized Markdown ranges inside MDX.
    Mdx,
    /// Supported reStructuredText block and reference syntax.
    RestructuredText,
    /// Plain text paragraphs.
    Text,
    /// Selected markdown and code cells from a notebook.
    Notebook,
    /// Documentation attached by a syntax provider.
    AttachedComment,
}

/// The selected source's canonical address.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentationSourceIdentity {
    /// A visible workspace file.
    Project {
        /// Path relative to the workspace root.
        path: ProjectPath,
    },
    /// A source unit supplied by an exact package adapter.
    Package {
        /// Canonical package source identity.
        unit: SourceUnitId,
    },
}

/// The authored cell identifier or its position when no identifier exists.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NotebookCellIdentity {
    /// A valid identifier stored in the notebook.
    Authored {
        /// Identifier accepted by the notebook format.
        id: String,
    },
    /// The cell's zero-based position in its notebook.
    Indexed {
        /// Position among every cell, including unselected cells.
        index: u32,
    },
}

/// The selected notebook cell's content kind.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NotebookCellKind {
    /// Markdown source decoded from a markdown cell.
    Markdown,
    /// Source decoded from a code cell, without execution.
    Code,
}

/// A selected cell addressed within its parent notebook.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotebookCell {
    /// Authored identifier or original cell position.
    pub identity: NotebookCellIdentity,
    /// Which content the cell carries.
    pub kind: NotebookCellKind,
}

/// One content owner: a regular source or decoded notebook cell.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationContentIdentity {
    /// Original workspace file or package source unit.
    pub source: DocumentationSourceIdentity,
    /// Decoded cell when the parent source is a notebook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<NotebookCell>,
}

/// Why the caller selected this source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationSelectionReason {
    /// Visible documentation in the captured workspace.
    Workspace,
    /// Source selected from one exact package's acquired bytes.
    PackageArchive,
    /// Comment supplied by its declaration's syntax provider.
    AttachedComment,
    /// Source supplied by a cloud resolver.
    CloudResolver,
}

/// License metadata supplied by the source adapter.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationLicense {
    /// Declared expression; collection does not infer a license from source text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// License files whose bytes the adapter verified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DocumentationLicenseFile>,
}

/// One license file's address and byte digest.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationLicenseFile {
    /// Path relative to the source's project or package root.
    pub path: ProjectPath,
    /// Digest of the license file's original bytes.
    pub digest: DocumentationDigest,
}

/// Source facts shared by every block belonging to one content owner.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationSource {
    /// Canonical address of the bytes block ranges use.
    pub identity: DocumentationContentIdentity,
    /// Source revision supplied by the caller.
    pub revision: DocumentationDigest,
    /// Digest of the content owner's exact bytes.
    pub content_digest: DocumentationDigest,
    /// Ownership and generation facts supplied by the caller.
    pub origin: SymbolOrigin,
    /// Parser selected for these bytes.
    pub format: DocumentationSourceFormat,
    /// Media type supplied by the source adapter.
    pub media_type: String,
    /// Why the source adapter selected these bytes.
    pub selection: DocumentationSelectionReason,
    /// Exact content length in UTF-8 bytes.
    pub byte_length: u64,
    /// Declared source language, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Original JSON string-token ranges for a decoded notebook cell, in source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub physical_ranges: Vec<TextRange>,
    /// License facts recorded by the source adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<DocumentationLicense>,
}

/// One heading in a block's ordered section path.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationHeading {
    /// Heading depth established by the format parser.
    pub level: u32,
    /// Heading identity, including the parser's duplicate-heading disambiguator.
    pub name: String,
}

/// The content a documentation block addresses.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationBlockKind {
    /// Authored prose and its markup.
    Prose,
    /// Authored code without execution or inferred name resolution.
    Code,
}

/// A baseline search document intersecting one block.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationChunk {
    /// Existing symbol or text document identity.
    pub identity: String,
    /// Exact content range held by this document.
    pub range: TextRange,
}

/// Documentation metadata addressing bytes held by an existing content owner.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationBlock {
    /// Digest of canonical source identity and structural position.
    pub identity: DocumentationDigest,
    /// Source record containing revision, format, origin, and license facts.
    pub source: DocumentationContentIdentity,
    /// Digest of the block's exact bytes.
    pub content_digest: DocumentationDigest,
    /// Headings owning the block, in increasing depth order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<DocumentationHeading>,
    /// Exact range in the regular source or decoded notebook cell.
    pub range: TextRange,
    /// One-based line where the block starts.
    pub line: u64,
    /// Whether the range carries prose or code.
    pub kind: DocumentationBlockKind,
    /// Authored code language, when supplied by the format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Existing ranked documents intersecting this block, in source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<DocumentationChunk>,
    /// Owning declaration for an attached comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolId>,
}

/// An authored link's resolved destination.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentationTarget {
    /// A collected documentation block.
    Block {
        /// Stable block identity.
        identity: DocumentationDigest,
    },
    /// A location inside one content owner.
    Source {
        /// Target content owner.
        source: DocumentationContentIdentity,
        /// Target range in its original content.
        range: TextRange,
    },
    /// One exact declaration.
    Symbol {
        /// Target declaration identity.
        symbol: SymbolId,
    },
}

/// Why an authored link or inline code candidate remains unresolved.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationUnresolvedReason {
    /// No selected source or declaration names this target.
    Missing,
    /// More than one declaration satisfies the exact spelling.
    Ambiguous,
    /// Destination uses an external scheme; collection performs no fetch.
    External,
    /// Fragment has no explicit authored target.
    Fragment,
    /// Target is outside the accepted path or identity form.
    Invalid,
}

/// The result of resolving one authored destination.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentationLinkResolution {
    /// An exact target exists in the accepted source set.
    Resolved {
        /// The identified destination.
        target: DocumentationTarget,
    },
    /// The destination does not establish an exact target.
    Unresolved {
        /// Bounded classification of the unresolved destination.
        reason: DocumentationUnresolvedReason,
    },
}

/// One authored link and its exact location inside a block.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationLink {
    /// Block containing the link.
    pub block: DocumentationDigest,
    /// Authored destination text.
    pub authored: String,
    /// Destination range in the block's content owner.
    pub range: TextRange,
    /// Exact target or reason the link remains unresolved.
    pub resolution: DocumentationLinkResolution,
}

/// Evidence establishing a documentation reference, ordered strongest first.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationReferenceEvidence {
    /// A typed contribution naming an exact declaration.
    Provider,
    /// An authored link resolving to one declaration location.
    AuthoredLink,
    /// Inline code equal to one declaration's qualified name.
    QualifiedName,
    /// Inline code equal to one unique declaration name.
    UniqueName,
}

/// An exact declaration reference authored inside standalone documentation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationReference {
    /// Digest of block, target, evidence, spelling, and occurrence ordinal.
    pub identity: DocumentationDigest,
    /// Block containing this reference.
    pub block: DocumentationDigest,
    /// Exact declaration this reference describes.
    pub target: SymbolId,
    /// Reference range in its content owner.
    pub range: TextRange,
    /// Exact authored spelling.
    pub authored: String,
    /// Rule that established the target.
    pub evidence: DocumentationReferenceEvidence,
}

/// An inline code candidate retained for later declaration changes.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationReferenceCandidate {
    /// Block containing the candidate.
    pub block: DocumentationDigest,
    /// Exact source range of the inline code content.
    pub range: TextRange,
    /// Authored declaration spelling.
    pub authored: String,
    /// Declared language limiting candidate declarations, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Why no exact declaration was selected.
    pub reason: DocumentationUnresolvedReason,
}

/// Counts recording what one collection parsed or omitted.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationCoverage {
    /// Accepted source records before extraction.
    pub selected: u32,
    /// Sources whose supported structure was parsed.
    pub parsed: u32,
    /// Sources omitted because their format or content was refused.
    pub omitted: u32,
    /// Sources whose output reached a collection bound.
    pub truncated: u32,
}

/// The collection step reporting incomplete documentation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationStage {
    /// Source selection and byte validation.
    Source,
    /// Format parsing and block extraction.
    Extract,
    /// Authored link and declaration reference resolution.
    Resolve,
    /// Mapping metadata to existing ranked documents.
    Index,
}

/// Why one documentation source is incomplete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationWarningKind {
    /// Source bytes or their format are unavailable.
    SourceUnavailable,
    /// Retained source ends before an addressed range.
    SourceTruncated,
    /// Format and extension do not select a supported parser.
    UnsupportedFormat,
    /// Parser refused malformed source.
    MalformedSource,
    /// Recognized syntax was excluded from documentation metadata.
    OmittedRange,
    /// A configured or format bound stopped output.
    LimitExceeded,
}

/// One bounded warning with repeated conditions counted per source.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationWarning {
    /// Source whose documentation is incomplete.
    pub source: DocumentationContentIdentity,
    /// Collection step reporting the condition.
    pub stage: DocumentationStage,
    /// Typed condition the caller can inspect.
    pub kind: DocumentationWarningKind,
    /// Number of items omitted or truncated for this condition.
    pub count: u64,
}

/// Documentation metadata published alongside an existing search corpus.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationIndex {
    /// Revision of extraction, identity, linking, and chunk rules.
    pub documentation_revision: Digest,
    /// Digest of sorted source identities, content digests, formats, and origins.
    pub selection_digest: DocumentationDigest,
    /// Source facts in content identity order.
    pub sources: Vec<DocumentationSource>,
    /// Blocks in source, range, kind, and identity order.
    pub blocks: Vec<DocumentationBlock>,
    /// Authored links and their resolution.
    pub links: Vec<DocumentationLink>,
    /// Exact references used by symbol reads.
    pub references: Vec<DocumentationReference>,
    /// Unresolved spellings used for incremental invalidation.
    pub unresolved_references: Vec<DocumentationReferenceCandidate>,
    /// Counts describing the accepted collection.
    pub coverage: DocumentationCoverage,
    /// Source-specific incomplete conditions.
    pub warnings: Vec<DocumentationWarning>,
}

/// One documentation block projected from its existing content owner.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationHit {
    /// Exact block metadata, including source identity and range.
    pub block: DocumentationBlock,
    /// Origin, revision, format, and license of the content owner.
    pub source: DocumentationSource,
    /// Documentation revision under which this block was published.
    pub documentation_revision: Digest,
}

/// One exact declaration reference and the documentation containing it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationReferenceHit {
    /// Exact reference evidence, never a repeated name lookup.
    pub reference: DocumentationReference,
    /// Source block and its origin facts.
    pub documentation: DocumentationHit,
    /// Bytes read from the existing content owner within the response bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16_384))]
    pub excerpt: Option<String>,
}

/// Bounded documentation context requested for one exact declaration.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationContext {
    /// Documentation revision used for the reverse reference lookup.
    pub documentation_revision: Digest,
    /// References ordered by evidence, source, block range, and identity.
    #[schemars(length(max = 32))]
    pub references: Vec<DocumentationReferenceHit>,
    /// Whether the reference count or total excerpt bytes reached its bound.
    pub truncated: bool,
    /// Incomplete source conditions for requested excerpts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub warnings: Vec<DocumentationWarning>,
}

#[cfg(test)]
mod tests {
    use super::DocumentationDigest;

    #[test]
    fn documentation_digest_schema_requires_full_sha256() {
        let schema = serde_json::to_value(schemars::schema_for!(DocumentationDigest))
            .expect("schema serializes");
        assert_eq!(schema["type"], "string");
        assert_eq!(schema["minLength"], 64);
        assert_eq!(schema["maxLength"], 64);
        assert_eq!(schema["pattern"], "^[0-9a-f]{64}$");
    }
}
