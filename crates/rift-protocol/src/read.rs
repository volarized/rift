//! Wire models for the Rift MCP read tools.
//!
//! Every type here is a wire contract: serde attributes define exactly what
//! the server accepts and returns, and the MCP server derives its advertised
//! request and response schemas from these definitions.

use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// Search-specific models (`SearchParams`, `PathSelector`, the filter tree, and their
// neighbors) live in `search` so this module stays below its size bound; re-exporting them
// here keeps every existing `rift_protocol::read::SearchParams`-style path resolving.
pub use crate::search::{
    ElementFilter, FieldFilter, FieldFilterOp, Filter, GraphHop, HopDirection, MatchedField,
    PathPattern, PathPatternViolation, PathSelector, RelationFilter, RelationFilterDirection,
    RelationFilterQuantifier, ResultOrder, SearchHit, SearchHitTarget, SearchInclude, SearchIntent,
    SearchParams, SearchParamsTarget, SearchResult, SearchTraversal, TraversalDirection,
};
// Diagnostic-family models (`Diagnostic`, its context, and their neighbors) live in
// `diagnostic` so this module stays below its size bound; re-exporting them here keeps every
// existing `rift_protocol::read::Diagnostic`-style path resolving.
pub use crate::diagnostic::{
    Diagnostic, DiagnosticCode, DiagnosticContext, DiagnosticContextSource, DiagnosticContinuation,
    DiagnosticRelated, DiagnosticReliability, DiagnosticTag,
};

/// Empirical coupling between two symbols: how often revisions that touched one touched the
/// other. The relation is observed from history rather than resolved from source, so it
/// reaches coupling no reference graph carries - two implementations of one concept with no
/// edge between them.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoChange {
    /// The symbol the coupling is stated for.
    pub subject: SymbolId,
    /// The symbol that historically changes with it.
    pub partner: SymbolId,
    /// Revisions inside the walked depth that touched both symbols.
    #[schemars(range(min = 1_u64, max = 9_007_199_254_740_991_u64))]
    pub together: u64,
    /// Revisions inside the walked depth that touched `subject` at all. Never below
    /// `together`.
    #[schemars(range(min = 1_u64, max = 9_007_199_254_740_991_u64))]
    pub touches: u64,
}

/// How far the claim reaches.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CoverageReach {
    /// Only what this request touched.
    Request,
    /// Every visible file of the workspace.
    Project,
    /// The workspace's resolved dependencies.
    Dependencies,
    /// The workspace, its dependencies, and the standard library together.
    All,
}

/// What a completeness statement covers - everything the request asked for, one file, or a
/// standing scope the answer holds over.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
pub enum CoverageScope {
    /// A standing scope identified by its name.
    Reach {
        /// How far the claim reaches.
        reach: CoverageReach,
    },
    /// A single unit is just a file: the claim holds for that path and says nothing about any other.
    Unit {
        /// The file the claim is about.
        unit: FileId,
    },
}

/// The first eight lowercase hex characters of a SHA-256, the same witness convention `NodeId`
/// uses. The full digest is computed and compared internally; only this short form ever
/// reaches the wire.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Digest(
    #[schemars(example = &"3f9a1c2e")]
    #[schemars(regex(pattern = r"^[0-9a-f]{8}$"))]
    pub String,
);

/// One block of documentation attached to a declaration, in the markup it was written in.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Documentation {
    /// Which markup the text is written in, since whoever displays a doc comment is the one
    /// that renders it.
    pub format: DocumentationFormat,
    /// The body of the comment, with the comment syntax stripped.
    pub text: String,
}

/// Which markup the text is written in, since whoever displays a doc comment is the one that
/// renders it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationFormat {
    /// Plain text with no markup to render.
    Plain,
    /// Markdown as authored in the source.
    Markdown,
}

/// A provider-local kind preserving the construct name used by that language implementation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ExactKind(#[schemars(regex(pattern = r"^[A-Za-z][A-Za-z0-9._-]*$"))] pub String);

/// A reverse-domain namespaced extension or extension-operation identifier.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ExtensionKey(
    #[schemars(regex(pattern = r"^[a-z0-9]+(?:[.-][a-z0-9]+)+\.[A-Za-z][A-Za-z0-9_-]*$"))]
    pub  String,
);

/// Versioned extension value. data is validated against the schema advertised for its key
/// and version.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionValue {
    /// Which version of the key's advertised schema shaped `data`. A consumer skips a value
    /// whose version it does not implement.
    #[schemars(range(min = 1_u64))]
    pub version: u64,
    /// The value itself, shaped by whatever that key and version advertise. Rift carries it
    /// and never interprets it.
    pub data: serde_json::Value,
}

/// Facts a provider carries that the model has no field for, under a reverse-domain key.
/// Keys and values use RFC 8785 canonical JSON. Consumers skip entries they do not
/// implement.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Extensions(pub BTreeMap<ExtensionKey, ExtensionValue>);

/// One file and the languages that read it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::forbid_symlink_language_facts)]
pub struct File {
    /// Project-relative source identity and the URI from which this record and its bytes
    /// are read.
    pub id: FileId,
    /// Regular-file metadata or a symbolic-link target.
    pub content: FileContent,
    /// Distinct `Language` values in `regions`, sorted by name and dialect. A file holding
    /// embedded languages advertises each of them.
    pub languages: Vec<Language>,
    /// Byte ranges parsed with each language grammar. Entries sort by start, end, language
    /// name, and dialect with null first. Regions may overlap when two grammars parse the
    /// same bytes.
    pub regions: Vec<LanguageRegion>,
    /// Whether Rift produced facts from this file. False where there is nothing to read,
    /// and where no provider claims the path.
    pub semantic: bool,
}

/// The bytes of a regular file or the target of a symbolic link.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
pub enum FileContent {
    /// A physical or generated file with bytes in it. Every node and symbol with readable
    /// source comes from this kind.
    Regular {
        /// Size in bytes.
        #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
        size: u64,
        /// Whether the file is executable.
        executable: bool,
    },
    /// A symbolic link whose target is carried as canonical base64.
    Symlink {
        /// Canonical padded base64 of the raw target bytes. Rift does not follow the
        /// target.
        #[schemars(length(max = 5464))]
        #[schemars(regex(
            pattern = r"^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$"
        ))]
        target: String,
    },
}

/// Identity of one file in the tree a request targets. The path after `rift://file/` is a
/// `ProjectPath` in canonical percent-encoding. The server re-validates the decoded path
/// wherever a `FileId` arrives, so the `ProjectPath` exclusions hold for every consumer,
/// whatever schema its implementation generated from.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct FileId(
    #[schemars(example = &"rift://file/src/lib.rs")]
    #[schemars(length(min = 13, max = 8192))]
    #[schemars(regex(
        pattern = r"^rift://file/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,1000}$"
    ))]
    pub String,
);

/// One declaration a `get_symbol` lookup found.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetSymbolHit {
    /// The declaration that matched.
    pub symbol: Symbol,
    /// The declaration node, whose identity `replace_symbol` can act on. Absent when
    /// source is unavailable or outside the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Node>,
    /// The declaration source when the request asked for bodies and the provider can read
    /// it. Absent for source-less declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceExcerpt>,
    /// The symbol's timeline, present when the request asked for history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SymbolHistory>,
    /// Symbols that historically change with this one, strongest coupling first, present
    /// when the request asked for history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub co_changes: Option<Vec<CoChange>>,
}

/// Gets declarations by name and returns them with their bodies inline, so one call replaces
/// a search followed by paging through the file.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {
        "name": "ReadService",
        "language": {
            "name": "rust"
        },
        "include_body": true,
        "include_history": true,
        "limit": 5,
        "page_index": 0,
        "scope": "all"
    },
    {
        "name": "Deserialize",
        "scope": "dependencies",
        "limit": 10,
        "page_index": 1
    }
]))]
#[schemars(transform = schema::forbid_get_symbol_rev_with_projection)]
pub struct GetSymbolParams {
    /// The declaration name to look up - a name, not a free-text query; `search` takes
    /// those. An exact symbol name ranks first, then prefix matches, then qualified-name
    /// substrings.
    #[schemars(length(min = 1, max = 4096))]
    pub name: String,
    /// Narrows the answer to one language. Omitted searches every served language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Whether each hit carries its declaration source.
    #[serde(default = "default_get_symbol_params_include_body")]
    pub include_body: bool,
    /// Whether each hit carries its version-control timeline and co-change coupling. Off by
    /// default: a timeline is read when the caller is deciding about the symbol, not on
    /// every lookup.
    #[serde(default = "default_get_symbol_params_include_history")]
    pub include_history: bool,
    /// Most hits to return in one page, capped by `max_page_items`.
    #[serde(default = "default_get_symbol_params_limit")]
    #[schemars(range(min = 1_u64, max = 10_000_u64))]
    pub limit: u64,
    /// Zero-based page of the result set to serve, sized by `limit`. A `page_index` past
    /// the last page returns an empty page whose `pagination` carries the requested
    /// `page_index` and the true `total_pages`.
    #[serde(default = "default_get_symbol_params_page_index")]
    pub page_index: u64,
    /// The projection to read. Omitted reads the workspace tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectionId>,
    /// The version-control revision to read - a branch, tag, or commit id as the
    /// workspace's version control spells it. Omitted reads the current tree, and `rev`
    /// never combines with `projection`. The server refuses a revision read when the
    /// workspace has no version-control repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
    /// Source locations eligible for matches. All is the default because a known name may
    /// identify a dependency or standard-library declaration.
    #[serde(default = "default_get_symbol_params_scope")]
    pub scope: SearchScope,
}

fn default_get_symbol_params_include_body() -> bool {
    true
}

fn default_get_symbol_params_include_history() -> bool {
    false
}

fn default_get_symbol_params_limit() -> u64 {
    5
}

fn default_get_symbol_params_page_index() -> u64 {
    PAGE_INDEX_DEFAULT
}

fn default_get_symbol_params_scope() -> SearchScope {
    SearchScope::All
}

/// One page of declarations matching a name.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "hits": [
            {
                "symbol": {
                    "id": "rift://symbol/rust/src/config.rs/load_config",
                    "language": {
                        "name": "rust"
                    },
                    "name": "load_config",
                    "kind": "rust.function",
                    "facets": [
                        "value",
                        "callable",
                        "public"
                    ],
                    "origin": {
                        "location": {
                            "kind": "project"
                        },
                        "source_kind": "authored",
                        "unit": "rift://source/project/src/config.rs"
                    },
                    "modifiers": [],
                    "visibility": "pub",
                    "types": [
                        {
                            "role": "return",
                            "origin": "declared",
                            "type": {
                                "language": {
                                    "name": "rust"
                                },
                                "source": "Result<Config, ConfigError>",
                                "extensions": {}
                            }
                        }
                    ],
                    "signatures": [
                        {
                            "display": "pub fn load_config(path: &Path) -> Result<Config, ConfigError>",
                            "links": [
                                {
                                    "range": {
                                        "start": 42,
                                        "end": 48
                                    },
                                    "symbol": "rift://symbol/rust/src/config.rs/Config"
                                }
                            ],
                            "language": {
                                "name": "rust"
                            },
                            "parameters": [
                                {
                                    "name": "path",
                                    "types": [
                                        {
                                            "role": "parameter",
                                            "origin": "declared",
                                            "type": {
                                                "language": {
                                                    "name": "rust"
                                                },
                                                "source": "&Path",
                                                "extensions": {}
                                            }
                                        }
                                    ],
                                    "optional": false,
                                    "variadic": false,
                                    "extensions": {}
                                }
                            ],
                            "returns": [
                                {
                                    "role": "return",
                                    "origin": "declared",
                                    "type": {
                                        "language": {
                                            "name": "rust"
                                        },
                                        "source": "Result<Config, ConfigError>",
                                        "extensions": {}
                                    }
                                }
                            ],
                            "type_parameters": [],
                            "throws": [],
                            "effects": [],
                            "extensions": {}
                        }
                    ],
                    "documentation": [
                        {
                            "format": "markdown",
                            "text": "Loads the workspace configuration from `rift.toml`."
                        }
                    ],
                    "extensions": {},
                    "document_local": false
                },
                "node": {
                    "id": "rift://node/rust/src/config.rs@218-355#67ecfb36",
                    "symbol": "rift://symbol/rust/src/config.rs/load_config",
                    "unit": "rift://file/src/config.rs",
                    "language": {
                        "name": "rust"
                    },
                    "kind": "rust.function_item",
                    "facets": [
                        "declaration",
                        "definition"
                    ],
                    "range": {
                        "start": 218,
                        "end": 355
                    },
                    "regions": [
                        {
                            "role": "name",
                            "range": {
                                "start": 225,
                                "end": 236
                            }
                        },
                        {
                            "role": "body",
                            "range": {
                                "start": 281,
                                "end": 355
                            }
                        }
                    ],
                    "parent": "rift://node/rust/src/config.rs@0-356#dcbef6dd",
                    "extensions": {}
                },
                "source": {
                    "span": {
                        "unit": "rift://source/project/src/config.rs",
                        "range": {
                            "start": 162,
                            "end": 355
                        }
                    },
                    "text": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}"
                },
                "history": {
                    "symbol": "rift://symbol/rust/src/config.rs/load_config",
                    "versions": [
                        {
                            "revision": "1f2080e49da12fee4431e6872630509355cd62d1",
                            "path": "src/config.rs",
                            "kind": "signature_changed",
                            "timestamp": "2026-08-21T14:03:22Z",
                            "summary": "Return ConfigError from load_config"
                        },
                        {
                            "revision": "8259026556ceae156a29adb53178c842ca32c4a2",
                            "path": "src/config.rs",
                            "kind": "introduced",
                            "timestamp": "2026-08-17T09:41:05Z",
                            "summary": "Add workspace configuration loading"
                        }
                    ]
                },
                "co_changes": [
                    {
                        "subject": "rift://symbol/rust/src/config.rs/load_config",
                        "partner": "rift://symbol/rust/src/error.rs/ConfigError",
                        "together": 4,
                        "touches": 9
                    }
                ]
            }
        ],
        "pagination": {
            "page_index": 0,
            "total_pages": 1
        },
        "warnings": []
    }
]))]
pub struct GetSymbolResult {
    /// The declarations on this page, best match first.
    pub hits: Vec<GetSymbolHit>,
    /// Where this page sits in the full result set under the request's `limit`.
    pub pagination: Pagination,
    /// Warnings attached to this result, empty when there is nothing to warn about.
    pub warnings: Vec<ReadWarning>,
}

/// A language name and its optional dialect. The pair is the identity facts are filed under,
/// so `sql` and `sql:postgresql` are two languages with two symbol spaces.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Language {
    /// The language name, such as `sql`, `json`, or `css`. Lowercase, so `TypeScript` and
    /// `typescript` cannot split one language into two identity spaces.
    #[schemars(length(max = 64))]
    #[schemars(regex(pattern = r"^[a-z][a-z0-9._-]*$"))]
    #[schemars(example = &"rust")]
    pub name: String,
    /// A dialect whose syntax or semantics differ within the language, such as
    /// `postgresql`, `jsonc`, or `scss`. Lowercase, as `name` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    #[schemars(regex(pattern = r"^[a-z][a-z0-9._-]*$"))]
    pub dialect: Option<String>,
}

/// A byte range of one file and the language used to parse it. The owner of `App.svelte`
/// can mark its script block as TypeScript. A generated file records ranges in its own byte
/// coordinates.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageRegion {
    /// The language grammar used for these bytes.
    pub language: Language,
    /// Offsets of the language region inside the file.
    pub range: TextRange,
}

/// One node of a file's concrete syntax tree. It identifies a source range and
/// provider-local syntax kind. `symbol` connects the node to semantic identity when the
/// language supplies one.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Unique identifier of this source region, and the URI that resolves it.
    pub id: NodeId,
    /// The symbol written at this node. Absent where a node writes no symbol -
    /// punctuation, a keyword, a comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolId>,
    /// The file the node is written in.
    pub unit: FileId,
    /// The grammar that produced this node. It belongs to the identity because two
    /// providers can produce different trees over the same file bytes.
    pub language: Language,
    /// What the node is in the provider's vocabulary, such as `fn_item`, `mapping.key`, or
    /// `selector.class`.
    pub kind: ExactKind,
    /// Portable structural classification, so a query can ask for bodies or imports without
    /// knowing the grammar that produced them.
    pub facets: Vec<NodeFacet>,
    /// The bytes it spans, as offsets into the file.
    pub range: TextRange,
    /// The node's named parts, so an operation can rewrite a function body without touching
    /// the documentation above it.
    pub regions: Vec<NodeRegion>,
    /// The region this one is nested inside. Absent at the top level of a unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    /// Syntax facts the model has no field for, namespaced by the provider that emitted
    /// them.
    pub extensions: Extensions,
}

/// Portable structural facets, so a filter can ask for bodies or imports without knowing the
/// grammar that produced them.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeFacet {
    /// Introduces a name.
    Declaration,
    /// Supplies the implementation behind a declared name.
    Definition,
    /// The implementation part of a declaration.
    Body,
    /// A delimited group of statements.
    Block,
    /// One executable step.
    Statement,
    /// Computes a value.
    Expression,
    /// Spells a type.
    TypeExpression,
    /// Brings an external name into scope.
    Import,
    /// Exposes a name outside its unit.
    Export,
    /// A declared input of a callable.
    Parameter,
    /// A value passed at a call site.
    Argument,
    /// A decorator or attribute qualifying a construct.
    Annotation,
    /// Commentary the language ignores.
    Comment,
    /// A name as written in the source.
    Identifier,
    /// A value written out directly.
    Literal,
    /// A destructuring or match pattern.
    Pattern,
    /// Produced by a tool rather than authored.
    Generated,
    /// Belongs to test code.
    Test,
}

/// Identity of one syntax-tree node. The byte range locates the node in the tree the request
/// targets; the fragment after `#` is its witness - the first eight lowercase hex characters
/// of the SHA-256 of the node's source bytes. Resolution recomputes the witness before
/// acting on the address and refuses with a failed `source_unchanged` precondition when the
/// bytes have drifted, so an address read from a stale listing cannot splice into the wrong
/// code.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct NodeId(
    #[schemars(example = &"rift://node/rust/lib.rs@220-268#3f9a1c2e")]
    #[schemars(length(min = 27, max = 8192))]
    #[schemars(regex(
        pattern = r"^rift://node/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/-]|%[0-9A-F]{2}){1,1000}@\d+-\d+#[0-9a-f]{8}$"
    ))]
    pub String,
);

/// One named part of a node, and the bytes it spans.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRegion {
    /// Which part of the node this is.
    pub role: RegionRole,
    /// Offsets into the file, on the same scale as `Node.range`.
    pub range: TextRange,
}

/// Lists the syntax nodes covering one position, outermost first. It returns a witnessed
/// address for an edit smaller than a declaration, such as one call expression.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {
        "path": "src/config.rs",
        "position": 338
    }
]))]
#[schemars(transform = schema::forbid_nodes_rev_with_projection)]
pub struct NodesParams {
    /// Project-relative file to inspect.
    #[schemars(length(min = 1))]
    pub path: ProjectPath,
    /// UTF-8 byte offset the listed nodes must cover - one position, not a range; the nodes
    /// themselves carry the spans.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub position: u64,
    /// The projection to read. Omitted reads the workspace tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<ProjectionId>,
    /// The version-control revision to read - a branch, tag, or commit id as the
    /// workspace's version control spells it. Omitted reads the current tree, and `rev`
    /// never combines with `projection`. The server refuses a revision read when the
    /// workspace has no version-control repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
}

/// The nodes covering one position. Each identity carries its witness, so an address taken
/// from this listing refuses cleanly once the bytes drift.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "nodes": [
            {
                "id": "rift://node/rust/src/config.rs@0-356#dcbef6dd",
                "unit": "rift://file/src/config.rs",
                "language": {
                    "name": "rust"
                },
                "kind": "rust.source_file",
                "facets": [],
                "range": {
                    "start": 0,
                    "end": 356
                },
                "regions": [],
                "extensions": {}
            },
            {
                "id": "rift://node/rust/src/config.rs@218-355#67ecfb36",
                "symbol": "rift://symbol/rust/src/config.rs/load_config",
                "unit": "rift://file/src/config.rs",
                "language": {
                    "name": "rust"
                },
                "kind": "rust.function_item",
                "facets": [
                    "declaration",
                    "definition"
                ],
                "range": {
                    "start": 218,
                    "end": 355
                },
                "regions": [
                    {
                        "role": "name",
                        "range": {
                            "start": 225,
                            "end": 236
                        }
                    },
                    {
                        "role": "body",
                        "range": {
                            "start": 281,
                            "end": 355
                        }
                    }
                ],
                "parent": "rift://node/rust/src/config.rs@0-356#dcbef6dd",
                "extensions": {}
            },
            {
                "id": "rift://node/rust/src/config.rs@281-355#4e554fa8",
                "unit": "rift://file/src/config.rs",
                "language": {
                    "name": "rust"
                },
                "kind": "rust.block",
                "facets": [],
                "range": {
                    "start": 281,
                    "end": 355
                },
                "regions": [],
                "parent": "rift://node/rust/src/config.rs@218-355#67ecfb36",
                "extensions": {}
            },
            {
                "id": "rift://node/rust/src/config.rs@334-353#4df4426e",
                "unit": "rift://file/src/config.rs",
                "language": {
                    "name": "rust"
                },
                "kind": "rust.call_expression",
                "facets": [
                    "expression"
                ],
                "range": {
                    "start": 334,
                    "end": 353
                },
                "regions": [],
                "parent": "rift://node/rust/src/config.rs@281-355#4e554fa8",
                "extensions": {}
            },
            {
                "id": "rift://node/rust/src/config.rs@334-346#03f22dac",
                "unit": "rift://file/src/config.rs",
                "language": {
                    "name": "rust"
                },
                "kind": "rust.identifier",
                "facets": [],
                "range": {
                    "start": 334,
                    "end": 346
                },
                "regions": [],
                "parent": "rift://node/rust/src/config.rs@334-353#4df4426e",
                "extensions": {}
            }
        ],
        "source": [
            {
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 0,
                        "end": 356
                    }
                },
                "text": "use std::path::Path;\n\nuse crate::error::ConfigError;\n\n/// Workspace configuration read from `rift.toml`.\npub struct Config {\n    pub root: std::path::PathBuf,\n}\n\n/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}\n"
            },
            {
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 218,
                        "end": 355
                    }
                },
                "text": "pub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}"
            },
            {
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 281,
                        "end": 355
                    }
                },
                "text": "{\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}"
            },
            {
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 334,
                        "end": 353
                    }
                },
                "text": "parse_config(&text)"
            },
            {
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 334,
                        "end": 346
                    }
                },
                "text": "parse_config"
            }
        ],
        "warnings": []
    }
]))]
pub struct NodesResult {
    /// Nodes covering the position, outermost first.
    pub nodes: Vec<Node>,
    /// The source of each node, in the same order.
    pub source: Vec<SourceExcerpt>,
    /// Warnings attached to this result, empty when there is nothing to warn about.
    pub warnings: Vec<ReadWarning>,
}

/// One package as its package manager identifies it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    /// Package manager or ecosystem name.
    #[schemars(length(max = 128))]
    pub manager: String,
    /// Package name in that ecosystem.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// Resolved package version.
    #[schemars(length(max = 4096))]
    pub version: String,
}

/// Default `page_index` for a paginated request: the first page.
pub const PAGE_INDEX_DEFAULT: u64 = 0;

/// Where one page sits in the full result set the request's `limit` divides.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pagination {
    /// The zero-based page this answer serves.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub page_index: u64,
    /// The page count of the full result set under the request's `limit`, computed within
    /// the server's result bound. Zero when the result set is empty.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub total_pages: u64,
}

/// One parameter of a `Signature`: what it is called, the types bound to it, and how a call
/// may pass it. A receiver is one of these too, held in its own field because it has no
/// position in the parameter list.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    /// What the parameter is called. Absent where the language allows an unnamed one, as
    /// a positional parameter in a function type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where this parameter is written in the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// What it accepts. An array because a declared type and an inferred one are separate
    /// bindings.
    pub types: Vec<TypeBinding>,
    /// Whether a call may leave it out.
    pub optional: bool,
    /// Whether it absorbs the arguments that follow - `*args`, `...rest`.
    pub variadic: bool,
    /// The default value as written in the source. Absent where there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Parameter facts the model has no field for, namespaced by the provider that emitted
    /// them.
    pub extensions: Extensions,
}

/// One path below the workspace root, using forward slashes and UTF-8 in Unicode NFC - Rift
/// normalizes what it emits and what it accepts, and compares byte-for-byte. The empty path
/// names the root itself. Absolute paths, backslashes, control characters, empty segments,
/// and `.` or `..` segments are refused before the filesystem is touched. The limit is 1000
/// UTF-8 bytes, not characters. A workspace holding two entries whose NFC forms are equal
/// fails the read that touches them with `content_unavailable`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ProjectPath(
    #[schemars(example = &"src/lib.rs")]
    #[schemars(length(max = 1000))]
    #[schemars(regex(
        pattern = r"^(?:$|(?!\.rift(?:/|$))(?!/)(?!.*(?:^|/)\.{1,2}(?:/|$))(?!.*//)[^\\\u0000-\u001F\u007F/]+(?:/[^\\\u0000-\u001F\u007F/]+)*)$"
    ))]
    pub String,
);

/// Identity of one projection, and the URI that resolves it. The caller names the
/// projection at `projection_create` - a directory-valid name of 1 to 64 lowercase
/// letters, digits, and interior dashes, such as `my-feature-one` - and the URI carries
/// that name. The name addresses the projection while it lives; `projection_remove`
/// frees it, and a later projection reusing the name is a distinct projection. A change
/// request that omits its `projection` field applies to the workspace tree itself.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ProjectionId(
    #[schemars(example = &"rift://projection/my-feature-one")]
    #[schemars(regex(pattern = r"^rift://projection/[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$"))]
    pub String,
);

/// One warning attached to a read result. The answer stands; the warning carries evidence
/// of a condition the caller weighs before relying on it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "code", deny_unknown_fields, rename_all = "snake_case")]
pub enum ReadWarning {
    /// The answer was computed from an index that lags the tree the read captured. Facts
    /// derived from the index may miss the newest writes; the digests state which two
    /// trees disagree.
    StaleIndex {
        /// Tree revision the published index covers.
        index_tree_revision: Digest,
        /// Tree revision the read captured.
        captured_tree_revision: Digest,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
}

/// One named part of a node. A language marks these out inside a declaration, so an
/// operation can address the body of a function without addressing its documentation.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RegionRole {
    /// The stretch that best identifies the node when presented.
    Selection,
    /// The name being declared.
    Name,
    /// The declaration up to where the body starts.
    Header,
    /// The implementation, without documentation or header.
    Body,
    /// The interior of the node without its delimiters.
    Content,
    /// The doc comment attached to the declaration.
    Documentation,
    /// The full extent including what surrounds the node proper.
    Enclosing,
}

/// One directed edge between two symbols. Its evidence is the nodes it was read from, and
/// its derivation is how much the provider knew when it was read.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    /// The symbol the edge starts at.
    pub from: SymbolId,
    /// What the edge is in this provider's vocabulary, such as `import`, `use`, or
    /// `implements`.
    pub kind: ExactKind,
    /// Portable classification, so a query for `imports` finds local kinds such as `import`
    /// and `use` alike.
    #[schemars(length(min = 1))]
    pub facets: Vec<RelationshipFacet>,
    /// The symbol the edge points at. One Rift cannot read carries the `external` origin;
    /// the edge is the same either way.
    pub to: SymbolId,
    /// The nodes this edge was read from.
    pub evidence: Vec<NodeId>,
    /// How this edge was established.
    pub derivation: RelationshipDerivation,
    /// How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other
    /// derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 1))]
    pub confidence: Option<f64>,
    /// Edge facts the model has no field for, namespaced by the provider that emitted them.
    pub extensions: Extensions,
}

/// How this edge was established. Every edge reaches Rift from a provider; this field
/// records how much the provider knew. A consumer may act on `resolution` directly. Lower
/// levels require another check before rewriting.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipDerivation {
    /// The provider resolved the edge semantically, so a consumer may act on it directly.
    Resolution,
    /// The edge was read from syntax alone, without semantic resolution.
    Syntax,
    /// The edge is a guess, qualified by `confidence`.
    Heuristic,
}

/// One portable category an edge falls into. The local kinds `import` and `use` can share
/// the `imports` facet, which lets one query cross languages.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipFacet {
    /// The source contains the target within its scope.
    Contains,
    /// The source declares the target.
    Declares,
    /// The source adds to a declaration made elsewhere.
    Augments,
    /// The source mentions the target.
    References,
    /// The source invokes the target.
    Calls,
    /// The source creates an instance of the target.
    Constructs,
    /// The source reads the target's value.
    Reads,
    /// The source assigns to the target.
    Writes,
    /// The source brings the target into scope.
    Imports,
    /// The source exposes the target outside its unit.
    Exports,
    /// The source inherits from the target.
    Extends,
    /// The source fulfils the target's interface.
    Implements,
    /// The source is typed by the target.
    HasType,
    /// The source replaces the target inherited from a supertype.
    Overrides,
    /// The source is another name for the target.
    Aliases,
    /// The source produces the target as generated code.
    Generates,
    /// The source requires the target to build or run.
    DependsOn,
    /// The source carries the target as an annotation.
    AnnotatedBy,
    /// The source can raise the target.
    Throws,
    /// The source handles the target when raised.
    Catches,
    /// The source's type parameter is constrained by the target.
    BoundedBy,
    /// The source applies concrete arguments to the generic target.
    Instantiates,
    /// The source is a specialization of the generic target.
    Specializes,
    /// The source and target are separately dispatched forms of one name.
    Overloads,
    /// The source incorporates the target as a mixin.
    MixesIn,
    /// The source embeds the target within its own definition.
    Embeds,
    /// The source exercises the target as a test.
    Tests,
    /// The source supplies configuration for the target.
    Configures,
    /// The source binds a name or value to the target.
    Binds,
}

/// Longest revision spelling the wire accepts, in bytes; the accepted charset is ASCII, so
/// the schema's `{1,128}` repetition counts the same units.
pub const REVISION_ID_BYTES_MAX: usize = 128;

/// Identity of one revision in the workspace's version-control history, spelled the way the
/// version-control system spells it. Rift carries it opaquely and never orders two revisions
/// by comparing their identifiers.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct RevisionId(
    #[schemars(example = &"main")]
    #[schemars(regex(pattern = r"^[A-Za-z0-9._/-]{1,128}$"))]
    pub String,
);

impl RevisionId {
    /// Classifies this spelling against the charset and length its schema advertises.
    /// `schemars` regexes are declarative only - nothing enforces them at
    /// deserialization - so every acceptance point calls this before the spelling
    /// reaches revision resolution.
    #[must_use]
    pub fn violation(&self) -> Option<RevisionIdViolation> {
        revision_id_violation(&self.0)
    }
}

/// Reason a revision spelling breaks the contract [`RevisionId`]'s schema advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionIdViolation {
    /// The spelling is empty.
    Empty,
    /// The spelling is longer than [`REVISION_ID_BYTES_MAX`] bytes.
    TooLong,
    /// The spelling carries a byte outside `A-Z a-z 0-9 . _ / -`.
    CharsetForbidden,
}

impl RevisionIdViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::CharsetForbidden => "charset_forbidden",
        }
    }
}

/// Classifies one revision spelling against the rules [`RevisionId`]'s schema advertises.
/// Arms are ordered by precedence: the first matching rule names the violation.
fn revision_id_violation(value: &str) -> Option<RevisionIdViolation> {
    let accepted =
        |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-');
    match value.as_bytes() {
        [] => Some(RevisionIdViolation::Empty),
        bytes if bytes.len() > REVISION_ID_BYTES_MAX => Some(RevisionIdViolation::TooLong),
        bytes if !bytes.iter().all(accepted) => Some(RevisionIdViolation::CharsetForbidden),
        _ => None,
    }
}

/// Which source locations a symbol lookup or search may return.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// Only source the workspace owns.
    Project,
    /// Only source of resolved dependencies.
    Dependencies,
    /// Project, dependency, and standard-library source alike.
    All,
}

/// How much a `Diagnostic` matters, in the provider's own judgement. Providers map their
/// toolchain's own levels onto these four, so a caller can drop everything below `warning`
/// without knowing which language produced it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The provider judges the code wrong.
    Error,
    /// Suspect but not necessarily wrong.
    Warning,
    /// Informational, with nothing to fix.
    Info,
    /// A gentle suggestion a consumer may hide.
    Hint,
}

/// One callable form of a symbol: the text it renders as, the symbols that text points at,
/// and its structure. Overloads are separate entries.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    /// The signature as a reader sees it, in the language's own syntax.
    pub display: String,
    /// Symbols named inside `display`, each with the byte range of `display` that names it,
    /// so a renderer can turn the rendered text into links.
    pub links: Vec<SignatureLink>,
    /// The language whose syntax `display` is written in.
    pub language: Language,
    /// The implicit first parameter - `self`, `this`. Absent for a free function, and for
    /// languages that have no such thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<Parameter>,
    /// Declared parameters, in source order.
    pub parameters: Vec<Parameter>,
    /// What the call yields. An array because a language may return several values, and
    /// because a declared and an inferred return are separate bindings.
    pub returns: Vec<TypeBinding>,
    /// The generic parameters this form declares, each as the symbol that declares it.
    pub type_parameters: Vec<SymbolId>,
    /// Types this form declares it can raise.
    pub throws: Vec<TypeExpression>,
    /// Effect keywords the declaration carries, in the language's own words: `async`,
    /// `unsafe`, `pure`. The spelling is preserved and never mapped onto a portable
    /// meaning.
    pub effects: Vec<String>,
    /// Signature facts the model has no field for, namespaced by the provider that emitted
    /// them.
    pub extensions: Extensions,
}

/// One symbol named inside a rendered signature, with the byte range of that rendering which
/// names it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureLink {
    /// Offsets into the rendered string in `Signature.display`.
    pub range: TextRange,
    /// The symbol that stretch of text names.
    pub symbol: SymbolId,
}

/// A copy of source from the catalog. The unit may belong to the project, a dependency, or
/// the standard library; the excerpt preserves bytes as they were when the answer was
/// produced.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceExcerpt {
    /// The source unit and byte range the text was taken from.
    pub span: SourceUnitSpan,
    /// The source bytes returned by the request.
    pub text: String,
}

/// How source or a declaration came to exist.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A person wrote it.
    Authored,
    /// A tool produced it from other source.
    Generated,
    /// The provider minted it without any source text.
    Synthetic,
}

/// Where source belongs. Package ownership is separate from whether source was authored or
/// generated.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
pub enum SourceLocation {
    /// Source owned by the current workspace.
    Project {
        /// Local package that owns the source, or absent when no package manifest assigns
        /// one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        package: Option<PackageIdentity>,
    },
    /// Source owned by one resolved dependency.
    Dependency {
        /// Resolved dependency that owns the source.
        package: PackageIdentity,
    },
    /// Source installed with the language toolchain.
    Stdlib {},
    /// Source outside the project, dependency graph, and standard library.
    External {},
}

/// A byte range of one file.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpan {
    /// Which file the offsets are into.
    pub unit: FileId,
    /// The bytes, as offsets into that file.
    pub range: TextRange,
}

/// Stable identity of one source unit in the source catalog: a resolver identity, then that
/// resolver's canonical unit key in canonical percent-encoding - for the project resolver, the
/// project-relative path, as `rift://source/project/src/lib.rs`. An identity derives from its
/// resolver's canonical human-readable key; digests appear on the wire only as short witnesses
/// where byte-identity is required.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct SourceUnitId(
    #[schemars(length(min = 17, max = 8192))]
    #[schemars(regex(
        pattern = r"^rift://source/[a-z][a-z0-9_.-]{0,127}/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,8192}$"
    ))]
    pub String,
);

/// One byte range in a source-catalog unit.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceUnitSpan {
    /// Source unit containing the bytes.
    pub unit: SourceUnitId,
    /// Half-open UTF-8 byte range in that unit.
    pub range: TextRange,
}

/// Provider-resolved semantic identity. Source structure lives in Node and is connected
/// through Relationship.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Symbol {
    /// Unique identifier of this symbol across the whole workspace, and the URI that change
    /// requests, filters, and relationship records accept unchanged.
    pub id: SymbolId,
    /// The language this symbol belongs to.
    pub language: Language,
    /// The human-readable name, as written in the source: `parseConfig`. Rendered
    /// signatures live in `signatures`.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// What this symbol is in the provider's vocabulary, such as `trait`, `function`, or
    /// `table`.
    pub kind: ExactKind,
    /// Portable classification for cross-language queries. The local kinds `trait` and
    /// `interface` can both carry the `type` facet.
    pub facets: Vec<SymbolFacet>,
    /// Where the declaration belongs, how it was produced, and its source unit when
    /// readable.
    pub origin: SymbolOrigin,
    /// The symbol this one belongs to - the class that owns a method, the module that owns
    /// a function. Ownership is not lexical: a Go method is written beside its type and a
    /// Rust method inside an `impl` block, and both name the type here. Absent at the top
    /// level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<SymbolId>,
    /// Language keywords qualifying the declaration: `export`, `async`, `const`.
    pub modifiers: Vec<String>,
    /// How widely the symbol is visible, in the language's own terms - `public`, `private`,
    /// `pub(crate)`. Absent where the language has no such concept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// The types this symbol carries, each tagged with the role it plays: a return type, a
    /// field type, a bound.
    pub types: Vec<TypeBinding>,
    /// One entry per callable form. Where the language dispatches overloads separately they
    /// are separate symbols joined by the `overloads` edge; several entries here are
    /// alternative forms of one dispatch target, as `typing.overload` writes them.
    pub signatures: Vec<Signature>,
    /// Doc comments attached to the declaration, with the markup format they were written
    /// in.
    pub documentation: Vec<Documentation>,
    /// Language-specific facts with no portable equivalent, namespaced by the provider that
    /// emitted them.
    pub extensions: Extensions,
    /// Whether language semantics confine this symbol to the document that declares it. The
    /// provider classifies locality from its language model.
    pub document_local: bool,
}

/// One portable category a symbol falls into. Kinds are language-specific; facets are
/// shared, so a filter written once applies to every served language.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolFacet {
    /// A named scope that groups declarations.
    Namespace,
    /// A compilation or import unit.
    Module,
    /// Names a type.
    Type,
    /// Holds a runtime value.
    Value,
    /// Can be invoked.
    Callable,
    /// Belongs to a containing type.
    Member,
    /// Owns members of its own.
    MemberContainer,
    /// A declared input of a callable.
    Parameter,
    /// A generic parameter a declaration abstracts over.
    TypeParameter,
    /// Instances of it can be created.
    Constructible,
    /// Other types can inherit from it.
    Extensible,
    /// Other types can fulfil it.
    Implementable,
    /// Expands at compile time.
    Macro,
    /// Exercises other code as a test.
    Test,
    /// Decorates other declarations.
    Annotation,
    /// Adds members to a type declared elsewhere.
    Extension,
    /// One case of an enumeration.
    Variant,
    /// A closed set of variants.
    Enumeration,
    /// Another name for an existing symbol.
    Alias,
    /// A member accessed like a field but backed by code.
    Property,
    /// Declared without a complete implementation.
    Abstract,
    /// Creates instances of its container.
    Constructor,
    /// Belongs to the type rather than an instance.
    Static,
    /// Its value can change after initialization.
    Mutable,
    /// Visible outside its declaring scope.
    Public,
    /// Marked as discouraged for new use.
    Deprecated,
    /// Where execution starts.
    Entrypoint,
    /// Invoked through operator syntax.
    Operator,
    /// Runs asynchronously.
    Async,
    /// Yields a sequence of values over time.
    Generator,
}

/// One symbol's timeline across the workspace's version-control history, newest revision
/// first. The walk is bounded by the configured history depth.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolHistory {
    /// The symbol the timeline is for.
    pub symbol: SymbolId,
    /// Revisions that touched the symbol, newest first.
    pub versions: Vec<SymbolVersion>,
}

/// Identity of one symbol. The name after the language is the provider's stable qualified
/// name for the declaration; where the language derives module identity from the file path,
/// as TypeScript does, that path is part of the name. A `~N` suffix separates declarations
/// the qualified name alone cannot, such as overloads that dispatch separately. A move can
/// change the identity when the language includes module path in that qualified name;
/// history correlation records the declaration across that change.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct SymbolId(
    #[schemars(example = &"rift://symbol/rust/crates/rift-server/src/read.rs/ReadService")]
    #[schemars(length(min = 17, max = 8192))]
    #[schemars(regex(
        pattern = r"^rift://symbol/[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?/(?:[A-Za-z0-9._~!$&'()*+,;=:/@-]|%[0-9A-F]{2}){1,1000}$"
    ))]
    pub String,
);

/// Where a symbol belongs and how its declaration came to exist. Source location and
/// generation are separate: generated code can belong to the project or to a dependency.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolOrigin {
    /// Source ownership. Absent exactly when `source_kind` is `synthetic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
    /// Whether the declaration is authored, generated, or synthetic.
    pub source_kind: SourceKind,
    /// Source-catalog unit containing the declaration. Absent when source is unavailable
    /// or the declaration is synthetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SourceUnitId>,
}

/// One revision that touched a symbol. The history provider derives it by parsing the
/// declaration at each revision that changed its file, so a version exists only where the
/// parse found the symbol.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolVersion {
    /// The revision that touched the symbol.
    pub revision: RevisionId,
    /// Where the declaration lived at that revision.
    pub path: ProjectPath,
    /// What the revision did to the symbol.
    pub kind: SymbolVersionKind,
    /// When the revision was recorded, as RFC 3339 date-time.
    #[schemars(length(max = 64))]
    pub timestamp: String,
    /// The revision's own first summary line, where the version control records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub summary: Option<String>,
}

/// What the revision did to the symbol.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolVersionKind {
    /// The revision brought the symbol into existence.
    Introduced,
    /// The revision changed the implementation without touching the signature.
    BodyChanged,
    /// The revision changed the declared interface.
    SignatureChanged,
    /// The revision relocated the declaration to another path.
    Moved,
    /// The revision deleted the declaration.
    Removed,
    /// The revision changed the annotations on the declaration.
    DecoratorsChanged,
}

/// Half-open UTF-8 byte offsets over authoritative UTF-8 source. Every provider converts
/// from whatever its toolchain counts in at its own boundary, so two toolchains' column
/// numbers arrive here on the same scale. No JSON Schema keyword can tie one field to
/// another, so that `end` is never below `start` is asserted by the surface
/// validation tests instead.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextRange {
    /// First byte of the range, counted from the start of the file.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub start: u64,
    /// One past the last byte. Equal to `start` for an empty range, which is how a position
    /// between two bytes is spelled.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub end: u64,
}

/// One type a symbol carries, together with the role it plays for that symbol.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeBinding {
    /// What this type is to the symbol that carries it.
    pub role: TypeBindingRole,
    /// Where the type fact came from.
    pub origin: TypeBindingOrigin,
    /// The type itself.
    pub r#type: TypeExpression,
}

/// Where the type fact came from. A declared type and an inferred one can both be present
/// and disagree, which is the interesting case.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TypeBindingOrigin {
    /// Written in the source by the author.
    Declared,
    /// Worked out by the provider from usage.
    Inferred,
    /// Required by the surrounding context.
    Expected,
}

/// What this type is to the symbol that carries it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TypeBindingRole {
    /// The type of the implicit first parameter.
    Receiver,
    /// The type a parameter accepts.
    Parameter,
    /// The type a call yields.
    Return,
    /// The type a field holds.
    Field,
    /// A constraint on a type parameter.
    Bound,
    /// The type of a collection's entries.
    Element,
    /// The type a map is indexed by.
    Key,
    /// The type of the failure a fallible result carries.
    Error,
    /// The type an alias or wrapper stands for.
    Underlying,
    /// The type a generator produces per step.
    Yielded,
    /// The type awaiting the value resolves to.
    Awaited,
    /// The type that tags which variant a value holds.
    Discriminant,
}

/// How a type is written in the source, plus the symbol that declares it when one does. A
/// type with a declaration resolves to that symbol; a structural type - `string | null`,
/// `{ a: string }` - has the spelling and nothing to resolve to.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeExpression {
    /// The language the spelling is in, and so which provider produced it.
    pub language: Language,
    /// The type as it is written: `Optional[Config]`, `&mut [u8]`, `string | null`.
    pub source: String,
    /// The symbol that declares this type, where one does. Absent for a structural type,
    /// which has a spelling and nothing to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<SymbolId>,
    /// Type facts the model has no field for, namespaced by the provider that emitted them.
    pub extensions: Extensions,
}

#[cfg(test)]
mod tests {
    use super::{
        Digest, GetSymbolParams, PAGE_INDEX_DEFAULT, REVISION_ID_BYTES_MAX, RevisionId,
        RevisionIdViolation, SourceUnitId,
    };
    use schemars::schema_for;
    use serde_json::json;

    /// Attribute arguments and `#[serde(default = ...)]` functions are both compiled apart
    /// from the schema; this pins the advertised default to the constant the field's
    /// default function returns.
    #[test]
    fn get_symbol_params_schema_page_index_default_equals_the_enforced_constant() {
        let schema = serde_json::to_value(schema_for!(GetSymbolParams)).expect("schema");
        assert_eq!(
            schema["properties"]["page_index"]["default"],
            json!(PAGE_INDEX_DEFAULT)
        );
    }

    #[test]
    fn revision_id_schema_pattern_states_the_enforced_length_bound() {
        let schema = serde_json::to_value(schema_for!(RevisionId)).expect("revision id schema");
        assert_eq!(
            schema["pattern"],
            json!(format!("^[A-Za-z0-9._/-]{{1,{REVISION_ID_BYTES_MAX}}}$"))
        );
    }

    #[test]
    fn revision_id_violation_classifies_what_the_schema_pattern_rejects() {
        let cases = [
            ("", Some(RevisionIdViolation::Empty)),
            (
                "a".repeat(REVISION_ID_BYTES_MAX + 1).leak() as &str,
                Some(RevisionIdViolation::TooLong),
            ),
            ("HEAD~1", Some(RevisionIdViolation::CharsetForbidden)),
            (
                "rev with space",
                Some(RevisionIdViolation::CharsetForbidden),
            ),
            ("main", None),
            ("feature/rev-reads", None),
            ("v0.0.6", None),
            ("dd0a482", None),
        ];
        for (spelling, expected) in cases {
            assert_eq!(
                RevisionId(spelling.to_owned()).violation(),
                expected,
                "spelling {spelling:?}"
            );
        }
    }

    #[test]
    fn revision_id_violations_spell_their_serialized_labels() {
        for (violation, label) in [
            (RevisionIdViolation::Empty, "empty"),
            (RevisionIdViolation::TooLong, "too_long"),
            (RevisionIdViolation::CharsetForbidden, "charset_forbidden"),
        ] {
            assert_eq!(violation.as_str(), label);
            assert_eq!(
                serde_json::to_value(violation).expect("serialize"),
                json!(label)
            );
        }
    }

    #[test]
    fn digest_schema_pattern_is_eight_lowercase_hex_characters() {
        let schema = serde_json::to_value(schema_for!(Digest)).expect("digest schema");
        assert_eq!(schema["pattern"], json!(r"^[0-9a-f]{8}$"));
    }

    #[test]
    fn digest_round_trips_an_eight_character_wire_value() {
        let digest = Digest("0123abcd".to_owned());
        let value = serde_json::to_value(&digest).expect("serialize");
        assert_eq!(value, json!("0123abcd"));
        let parsed: Digest = serde_json::from_value(value).expect("deserialize");
        assert_eq!(parsed, digest);
    }

    #[test]
    fn source_unit_id_schema_pattern_is_resolver_then_project_path_charset() {
        let schema = serde_json::to_value(schema_for!(SourceUnitId)).expect("source unit schema");
        assert_eq!(
            schema["pattern"],
            json!(
                r"^rift://source/[a-z][a-z0-9_.-]{0,127}/(?:[A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-F]{2}){1,8192}$"
            )
        );
    }

    #[test]
    fn source_unit_id_round_trips_a_project_file_and_a_nested_path() {
        for id in [
            "rift://source/project/lib.rs",
            "rift://source/project/crates/rift-server/src/read.rs",
        ] {
            let parsed: SourceUnitId = serde_json::from_value(json!(id)).expect("deserialize");
            assert_eq!(serde_json::to_value(&parsed).expect("serialize"), json!(id));
        }
    }
}
