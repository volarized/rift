//! Documentation metadata over existing symbol and file content, and the `[documentation]`
//! table of `rift.toml` that selects which documentation files the index collects.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::configuration::{ByteSize, ConfigurationViolation, first_out_of_range};
use crate::read::{
    Digest, Language, PathPattern, ProjectPath, SourceUnitId, SymbolId, SymbolOrigin, TextRange,
};
use crate::source::pattern_list_violation;

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
/// Supported selected sources one documentation collection accepts.
pub const DOCUMENTATION_SOURCES_CEILING: u32 = 5_000_000;
/// Source bytes one parser accepts.
pub const DOCUMENTATION_SOURCE_BYTES_MAX: u32 = 4 << 20;
/// Supported source bytes one documentation parser accepts.
pub const DOCUMENTATION_SOURCE_BYTES_CEILING: u32 = 64 << 20;
/// Source bytes one collection accepts together.
pub const DOCUMENTATION_TOTAL_BYTES_MAX: u64 = 512 << 20;
/// Supported source bytes one documentation collection accepts together.
pub const DOCUMENTATION_TOTAL_BYTES_CEILING: u64 = 64 << 30;
/// Blocks one collection retains. The fastapi corpus tree's translated documentation
/// alone holds 124,000.
pub const DOCUMENTATION_BLOCKS_MAX: u32 = 250_000;
/// Supported blocks one documentation collection retains.
pub const DOCUMENTATION_BLOCKS_CEILING: u32 = 50_000_000;
/// Links or reference candidates one collection retains. The fastapi corpus tree holds
/// 21,000 links and 52,000 unresolved reference spellings.
pub const DOCUMENTATION_REFERENCES_MAX: u32 = 250_000;
/// Supported links or reference candidates one documentation collection retains.
pub const DOCUMENTATION_REFERENCES_CEILING: u32 = 50_000_000;
/// Heading levels one block retains.
pub const DOCUMENTATION_HEADING_DEPTH_MAX: u32 = 512;
/// Supported heading levels one documentation block retains.
pub const DOCUMENTATION_HEADING_DEPTH_CEILING: u32 = 65_536;
/// Bytes one authored heading, destination, or reference spelling retains.
pub const DOCUMENTATION_TEXT_BYTES_MAX: u32 = 4_096;
/// Supported bytes one authored documentation spelling retains.
pub const DOCUMENTATION_TEXT_BYTES_CEILING: u32 = 1 << 20;
/// Warnings one collection retains.
pub const DOCUMENTATION_WARNINGS_MAX: u32 = 256;
/// Supported warnings one documentation collection retains.
pub const DOCUMENTATION_WARNINGS_CEILING: u32 = 65_536;
/// References one symbol read retains.
pub const DOCUMENTATION_SYMBOL_REFERENCES_MAX: u32 = 32;
/// Bytes one documentation excerpt retains.
pub const DOCUMENTATION_EXCERPT_BYTES_MAX: u32 = 16 << 10;
/// License files one selected source may name.
pub const DOCUMENTATION_LICENSE_FILES_MAX: u32 = 256;
/// Default blocks one documentation layer holds.
pub const DOCUMENTATION_LAYER_BLOCKS_MAX: u32 = 2_000_000;
/// Supported blocks one documentation layer holds.
pub const DOCUMENTATION_LAYER_BLOCKS_CEILING: u32 = 100_000_000;
/// Default ranking identity mappings one documentation layer holds.
pub const DOCUMENTATION_MAPPINGS_MAX: u32 = 6_000_000;
/// Supported ranking identity mappings one documentation layer holds.
pub const DOCUMENTATION_MAPPINGS_CEILING: u32 = 300_000_000;
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
    #[schemars(length(max = 65_536))]
    pub warnings: Vec<DocumentationWarning>,
}

/// Entries `documentation.exclude` or `documentation.force_include` may hold, at most.
pub const DOCUMENTATION_PATTERNS_MAX: usize = 512;
/// Parser progress callbacks one documentation source may consume.
pub const DOCUMENTATION_PROGRESS_CALLBACKS_MAX: u32 = 131_072;

/// The `[documentation]` table: which documentation files the index collects beside the
/// source. By default every Markdown, MDX, reStructuredText, plain text, and notebook file is
/// collected, except change logs, licenses, codes of conduct, and archived, deprecated, or
/// translated copies.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_documentation_ranges)]
pub struct DocumentationConfiguration {
    /// Whether documentation files are collected at all. `false` collects none; the
    /// documentation comments attached to declarations stay.
    pub enabled: bool,
    /// Exact file paths or globs whose documentation is left out beside the default
    /// exclusions, in [`PathPattern`] syntax.
    #[schemars(length(max = 512))]
    pub exclude: Vec<PathPattern>,
    /// Exact file paths or globs collected although a default exclusion leaves them out, in
    /// [`PathPattern`] syntax. A match `exclude` also names is still left out.
    #[schemars(length(max = 512))]
    pub force_include: Vec<PathPattern>,
    /// Selected documentation sources one collection accepts.
    #[schemars(range(min = 1, max = 5_000_000))]
    pub max_sources: u32,
    /// Source bytes one documentation parser accepts.
    pub max_file: ByteSize,
    /// Source bytes one documentation collection accepts together.
    pub max_total: ByteSize,
    /// Documentation blocks one collection retains.
    #[schemars(range(min = 1, max = 50_000_000))]
    pub max_blocks: u32,
    /// Links, fragments, or reference candidates one collection retains.
    #[schemars(range(min = 1, max = 50_000_000))]
    pub max_references: u32,
    /// Bytes one heading, language, destination, or reference spelling retains.
    pub max_text: ByteSize,
    /// Heading levels one documentation block retains.
    #[schemars(range(min = 1, max = 65_536))]
    pub max_heading_depth: u32,
    /// Warnings one documentation collection retains.
    #[schemars(range(min = 1, max = 65_536))]
    pub max_warnings: u32,
    /// Syntax nodes one documentation parser accepts.
    #[schemars(range(min = 1, max = 100_000_000))]
    pub max_nodes: u32,
    /// Nesting one documentation parser accepts.
    #[schemars(range(min = 1, max = 65_536))]
    pub max_depth: u32,
    /// Progress callbacks one reStructuredText parser accepts.
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub max_progress: u32,
    /// Blocks one documentation layer holds across its collections.
    #[schemars(range(min = 1, max = 100_000_000))]
    pub max_layer_blocks: u32,
    /// Ranking identity mappings one documentation layer holds.
    #[schemars(range(min = 1, max = 300_000_000))]
    pub max_mappings: u32,
}

impl Default for DocumentationConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            exclude: Vec::new(),
            force_include: Vec::new(),
            max_sources: DOCUMENTATION_SOURCES_MAX,
            max_file: ByteSize::from_bytes(u64::from(DOCUMENTATION_SOURCE_BYTES_MAX)),
            max_total: ByteSize::from_bytes(DOCUMENTATION_TOTAL_BYTES_MAX),
            max_blocks: DOCUMENTATION_BLOCKS_MAX,
            max_references: DOCUMENTATION_REFERENCES_MAX,
            max_text: ByteSize::from_bytes(u64::from(DOCUMENTATION_TEXT_BYTES_MAX)),
            max_heading_depth: DOCUMENTATION_HEADING_DEPTH_MAX,
            max_warnings: DOCUMENTATION_WARNINGS_MAX,
            max_nodes: u32::try_from(crate::configuration::SYNTAX_NODES_DEFAULT)
                .expect("syntax node default fits u32"),
            max_depth: u32::try_from(crate::configuration::SYNTAX_DEPTH_DEFAULT)
                .expect("syntax depth default fits u32"),
            max_progress: DOCUMENTATION_PROGRESS_CALLBACKS_MAX,
            max_layer_blocks: DOCUMENTATION_LAYER_BLOCKS_MAX,
            max_mappings: DOCUMENTATION_MAPPINGS_MAX,
        }
    }
}

impl DocumentationConfiguration {
    /// Validates documentation selection and collection bounds.
    ///
    /// # Errors
    ///
    /// Returns the first key outside its supported range or carrying an invalid pattern.
    pub fn validate(&self) -> Result<(), ConfigurationViolation> {
        self.violation().map_or(Ok(()), Err)
    }

    /// The table's list-length bounds, then each pattern's forward-slash-only contract, in
    /// key then list order.
    pub(crate) fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "documentation.max_sources",
                u64::from(self.max_sources),
                1,
                u64::from(DOCUMENTATION_SOURCES_CEILING),
            ),
            (
                "documentation.max_file",
                self.max_file.bytes(),
                1,
                u64::from(DOCUMENTATION_SOURCE_BYTES_CEILING),
            ),
            (
                "documentation.max_total",
                self.max_total.bytes(),
                1,
                DOCUMENTATION_TOTAL_BYTES_CEILING,
            ),
            (
                "documentation.max_blocks",
                u64::from(self.max_blocks),
                1,
                u64::from(DOCUMENTATION_BLOCKS_CEILING),
            ),
            (
                "documentation.max_references",
                u64::from(self.max_references),
                1,
                u64::from(DOCUMENTATION_REFERENCES_CEILING),
            ),
            (
                "documentation.max_text",
                self.max_text.bytes(),
                1,
                u64::from(DOCUMENTATION_TEXT_BYTES_CEILING),
            ),
            (
                "documentation.max_heading_depth",
                u64::from(self.max_heading_depth),
                1,
                u64::from(DOCUMENTATION_HEADING_DEPTH_CEILING),
            ),
            (
                "documentation.max_warnings",
                u64::from(self.max_warnings),
                1,
                u64::from(DOCUMENTATION_WARNINGS_CEILING),
            ),
            (
                "documentation.max_nodes",
                u64::from(self.max_nodes),
                1,
                crate::configuration::SYNTAX_NODES_MAX,
            ),
            (
                "documentation.max_depth",
                u64::from(self.max_depth),
                1,
                crate::configuration::SYNTAX_DEPTH_MAX,
            ),
            (
                "documentation.max_progress",
                u64::from(self.max_progress),
                1,
                u64::from(u32::MAX),
            ),
            (
                "documentation.max_layer_blocks",
                u64::from(self.max_layer_blocks),
                1,
                u64::from(DOCUMENTATION_LAYER_BLOCKS_CEILING),
            ),
            (
                "documentation.max_mappings",
                u64::from(self.max_mappings),
                1,
                u64::from(DOCUMENTATION_MAPPINGS_CEILING),
            ),
            (
                "documentation.exclude",
                self.exclude.len() as u64,
                0,
                DOCUMENTATION_PATTERNS_MAX as u64,
            ),
            (
                "documentation.force_include",
                self.force_include.len() as u64,
                0,
                DOCUMENTATION_PATTERNS_MAX as u64,
            ),
        ])
        .or_else(|| pattern_list_violation("documentation.exclude", &self.exclude))
        .or_else(|| pattern_list_violation("documentation.force_include", &self.force_include))
    }
}

#[cfg(test)]
mod tests {
    use super::{DOCUMENTATION_PATTERNS_MAX, DocumentationDigest};
    use crate::configuration::{ConfigurationViolation, WorkspaceConfiguration};
    use crate::read::PathPattern;

    #[test]
    fn documentation_digest_schema_requires_full_sha256() {
        let schema = serde_json::to_value(schemars::schema_for!(DocumentationDigest))
            .expect("schema serializes");
        assert_eq!(schema["type"], "string");
        assert_eq!(schema["minLength"], 64);
        assert_eq!(schema["maxLength"], 64);
        assert_eq!(schema["pattern"], "^[0-9a-f]{64}$");
    }

    #[test]
    fn test_documentation_defaults_collect_every_file_the_defaults_leave() {
        let configuration: WorkspaceConfiguration =
            serde_json::from_value(serde_json::json!({})).expect("an empty document");
        assert!(configuration.documentation.enabled);
        assert!(configuration.documentation.exclude.is_empty());
        assert!(configuration.documentation.force_include.is_empty());
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_documentation_table_parses_its_keys_and_refuses_an_unknown_one() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(serde_json::json!({
            "documentation": {
                "enabled": false,
                "exclude": ["docs/internal/**"],
                "force_include": ["CHANGELOG.md"],
            }
        }))
        .expect("a documentation table");
        assert!(!configuration.documentation.enabled);
        assert_eq!(
            configuration.documentation.exclude,
            [PathPattern("docs/internal/**".to_owned())]
        );
        assert_eq!(
            configuration.documentation.force_include,
            [PathPattern("CHANGELOG.md".to_owned())]
        );
        let unknown = serde_json::from_value::<WorkspaceConfiguration>(serde_json::json!({
            "documentation": { "include": ["docs/**"] }
        }));
        assert!(unknown.is_err(), "the table denies unknown keys");
    }

    #[test]
    fn test_documentation_pattern_lists_accept_the_cap_and_refuse_above_it() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.documentation.exclude =
            vec![PathPattern("docs/**".to_owned()); DOCUMENTATION_PATTERNS_MAX];
        assert_eq!(configuration.validate(), Ok(()));
        configuration
            .documentation
            .force_include
            .push(PathPattern("../outside.md".to_owned()));
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::PathPatternInvalid {
                field: "documentation.force_include",
                pattern: "../outside.md".to_owned(),
            })
        );

        let mut configuration = WorkspaceConfiguration::default();
        configuration.documentation.force_include =
            vec![PathPattern("x.md".to_owned()); DOCUMENTATION_PATTERNS_MAX + 1];
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "documentation.force_include",
                ..
            })
        ));

        let mut configuration = WorkspaceConfiguration::default();
        configuration.documentation.exclude = vec![PathPattern("docs\\old".to_owned())];
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::PathPatternInvalid {
                field: "documentation.exclude",
                ..
            })
        ));
    }
}
