//! Wire models for the Rift MCP read tools.
//!
//! Every type here is a wire contract: serde attributes define exactly what
//! the server accepts and returns, and the MCP server derives its advertised
//! request and response schemas from these definitions.

use crate::configuration::Duration;
use crate::dependencies::RequestedPackage;
use crate::schema;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Most characters a declaration lookup name carries.
pub const SYMBOL_NAME_CHARACTERS_MAX: usize = 4096;
/// Most project declarations proposed when a lookup finds no match.
pub const SYMBOL_ALTERNATIVES_MAX: usize = 3;

/// The ASCII punctuation an identity's path keeps literal: the RFC 3986 path set less its
/// alphanumerics, spelled in the order a regular-expression character class takes, with `-`
/// last. `rift_core::encode_path` escapes every other byte, and that crate's own test binds
/// its escape set to this spelling, so a pattern cannot drift from the encoder that mints the
/// values it describes.
macro_rules! identity_path_punctuation {
    () => {
        r"._~!$&'()*+,;=:@/-"
    };
}

/// One character of a `rift://` identity's path: a kept byte, or one percent-escape.
macro_rules! identity_path_character {
    () => {
        concat!(
            r"(?:[A-Za-z0-9",
            identity_path_punctuation!(),
            r"]|%[0-9A-F]{2})"
        )
    };
}

/// A character class of what one project path segment holds - anything but the `/`
/// separator, a backslash, or a control character - less the characters `$excluded` names.
macro_rules! project_path_class {
    ($($excluded:literal)?) => {
        concat!(r"[^\\\u0000-\u001F\u007F/", $($excluded,)? "]")
    };
}

/// One project path segment other than `.` and `..`: a run holding a character other than
/// `.`, or three dots or more.
macro_rules! project_path_segment {
    () => {
        concat!(
            r"(?:\.*",
            project_path_class!("."),
            project_path_class!(),
            r"*|\.{3,})"
        )
    };
}

/// The first segment of a project path: one `project_path_segment!` other than `.rift`,
/// spelled out prefix by prefix, since the regex dialects a JSON Schema pattern must
/// satisfy share no lookahead.
macro_rules! project_path_first_segment {
    () => {
        concat!(
            "(?:",
            project_path_class!("."),
            project_path_class!(),
            r"*|\.\.",
            project_path_class!(),
            r"+|\.",
            project_path_class!(".r"),
            project_path_class!(),
            r"*|\.r(?:",
            project_path_class!("i"),
            project_path_class!(),
            "*|i(?:",
            project_path_class!("f"),
            project_path_class!(),
            "*|f(?:",
            project_path_class!("t"),
            project_path_class!(),
            "*|t",
            project_path_class!(),
            "+)?)?)?)"
        )
    };
}

/// The language segment a `rift://node/` or `rift://symbol/` identity carries before its path:
/// one word, or two joined by `:`.
macro_rules! identity_language_segment {
    () => {
        r"[A-Za-z][A-Za-z0-9._-]*(?::[A-Za-z][A-Za-z0-9._-]*)?"
    };
}

/// The ASCII punctuation an identity's path keeps literal, for the tests that bind the encoder
/// and the served patterns to one alphabet.
pub const IDENTITY_PATH_PUNCTUATION: &str = identity_path_punctuation!();
/// The character class every served `rift://` identity pattern uses for its path.
pub const IDENTITY_PATH_CHARACTER: &str = identity_path_character!();

/// The spelling [`Language::identity_segment`] and [`Language::from_identity_segment`]
/// agree on: one lowercase language word, or two joined by `:`. The
/// `[languages.<identity>]` configuration table key uses the same grammar.
pub(crate) const LANGUAGE_IDENTITY_PATTERN: &str = r"^[a-z][a-z0-9._-]*(?::[a-z][a-z0-9._-]*)?$";
/// Longest accepted language name or dialect word, in bytes.
pub(crate) const LANGUAGE_WORD_BYTES_MAX: usize = 64;
/// Longest accepted language identity segment: two words joined by one colon.
pub(crate) const LANGUAGE_IDENTITY_BYTES_MAX: usize = LANGUAGE_WORD_BYTES_MAX * 2 + 1;

// Search-specific models (`SearchParams`, `PathSelector`, and their neighbors) live in
// `search` so this module stays below its size bound; re-exporting them here keeps every
// existing `rift_protocol::read::SearchParams`-style path resolving.
pub use crate::search::{
    CHANGE_BASE_FIELD, CHANGE_HEAD_FIELD, COMMIT_MESSAGE_BYTES_MAX, COMMIT_PATHS_MAX, ChangeHead,
    ChangeTree, CommitHit, GraphHop, HopDirection, MatchedField, PathPattern, PathPatternViolation,
    PathSelector, ResultOrder, SEARCH_CHANGE_HEAD_DEFAULT, SEARCH_CHANGE_PATHS_MAX,
    SEARCH_PATTERN_CHARS_MAX, SEARCH_TRAVERSAL_DEPTH_DEFAULT, SEARCH_TRAVERSAL_DEPTH_MAX,
    SEARCH_TRAVERSAL_DEPTH_MIN, SEARCH_TRAVERSAL_FACETS_MAX, SearchChange, SearchHit,
    SearchHitTarget, SearchInclude, SearchParams, SearchParamsTarget, SearchResult,
    SearchTraversal, SymbolChange, TraversalDirection,
};
// Diagnostic-family models (`Diagnostic`, its context, and their neighbors) live in
// `diagnostic` so this module stays below its size bound; re-exporting them here keeps every
// existing `rift_protocol::read::Diagnostic`-style path resolving.
pub use crate::diagnostic::{
    Diagnostic, DiagnosticContext, DiagnosticContextSource, DiagnosticContinuation,
    DiagnosticRelated, DiagnosticReliability, DiagnosticTag,
};

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
pub struct Documentation {
    /// Markup the comment text uses.
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

impl ExactKind {
    /// Whether the kind has the form the schema pattern advertises: an ASCII letter, then
    /// ASCII letters, digits, `.`, `_`, or `-`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        is_kind_word(&self.0)
    }
}

impl TryFrom<String> for ExactKind {
    type Error = ExactKindError;

    fn try_from(kind: String) -> Result<Self, Self::Error> {
        if is_kind_word(&kind) {
            Ok(Self(kind))
        } else {
            Err(ExactKindError { kind })
        }
    }
}

/// A kind outside the form [`ExactKind`] advertises.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactKindError {
    kind: String,
}

impl ExactKindError {
    /// The kind that was refused.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
}

impl std::fmt::Display for ExactKindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "kind {:?} must start with an ASCII letter followed by ASCII letters, digits, `.`, \
             `_`, or `-`, such as `function` or `type_alias`",
            self.kind
        )
    }
}

impl std::error::Error for ExactKindError {}

/// Whether one kind matches the form `ExactKind` advertises.
fn is_kind_word(kind: &str) -> bool {
    let mut bytes = kind.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

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
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Extensions(pub BTreeMap<ExtensionKey, ExtensionValue>);

impl Extensions {
    /// Whether this holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
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
        pattern = concat!(r"^rift://file/", identity_path_character!(), r"{1,1000}$")
    ))]
    pub String,
);

/// One declaration a `get_symbol` lookup found.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::get_symbol_hit_addresses_one_location)]
pub struct GetSymbolHit {
    /// The declaration that matched.
    pub symbol: Symbol,
    /// Project-relative path, present for a declaration that belongs to the project.
    /// Exactly one of `path` and `unit` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ProjectPath>,
    /// Source-catalog unit, present for a declaration that belongs to a dependency or the
    /// standard library. Exactly one of `path` and `unit` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SourceUnitId>,
    /// Byte range of the declaration within `path` or `unit`.
    pub range: TextRange,
    /// The 1-based source line where the declaration begins.
    #[schemars(range(min = 1_u64))]
    pub line: u64,
    /// The declaration node's identity, including its source range and witness.
    /// Absent unless `include` names `source`, or when source is unavailable or outside
    /// the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// The declaration source, present when `include` names `source` and the provider
    /// can read it. Absent for a source-less declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The symbol's timeline, present when `include` names `history`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SymbolHistory>,
    /// Exact documentation references, present only when `include` names `documentation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<crate::documentation::DocumentationContext>,
}

/// One optional `get_symbol` hit field the caller may opt into through `include`.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GetSymbolInclude {
    /// The hit's node identity and declaration source excerpt.
    Source,
    /// The hit's version-control timeline.
    History,
    /// Bounded documentation referring to the exact declaration.
    Documentation,
}

/// Gets declarations by name and returns them with their bodies inline, so one call replaces
/// a search followed by paging through the file.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {
        "name": "ReadService",
        "language": "rust",
        "include": ["source", "history"],
        "limit": 5,
        "page_index": 0
    },
    {
        "name": "Deserialize",
        "limit": 10,
        "page_index": 1
    },
    {
        "name": "spawn",
        "scope": "all",
        "limit": 5
    },
    {
        "name": "spawn",
        "scope": "global",
        "packages": [
            {
                "manager": "cargo",
                "name": "tokio",
                "version": "1.47.1"
            }
        ],
        "limit": 5
    }
]))]
pub struct GetSymbolParams {
    /// The declaration name to look up - a name, not a full `SymbolId` or free-text
    /// query; `search` takes free text. The server ranks exact original name or qualified-name
    /// spelling first, followed by case-insensitive exact, prefix, and qualified-name substring
    /// matches. Every tier joins the result set, so a substring match still answers after
    /// the exact and prefix matches.
    #[schemars(length(min = 1, max = 4096))]
    pub name: String,
    /// Narrows the answer to one language. Omitted searches every served language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Which declarations the lookup searches: the project tree, the dependency
    /// packages, or both. Omitted, `local`. The server refuses a scope beyond `local`
    /// together with `rev`, since package facts are served for the current tree alone.
    #[serde(default)]
    pub scope: SearchScope,
    /// Packages this lookup reads beside the ones the workspace's manifests and
    /// lockfiles name, at most 64. An entry naming a package the workspace depends on
    /// replaces that package's versions for this read, and an entry naming another
    /// package adds it. The server refuses `packages` beside the `local` scope, since a
    /// project read consults no package, and beside `rev`, since package facts are
    /// served for the current tree alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub packages: Vec<RequestedPackage>,
    /// Optional hit fields to attach: `source`, `history`. Omitted defaults to
    /// `["source"]`; an explicit empty list carries neither.
    #[serde(default = "default_get_symbol_params_include")]
    pub include: Vec<GetSymbolInclude>,
    /// Most hits to return in one page, at most 10,000; the server refuses a larger
    /// `limit` naming the field. The server's result bound caps the set itself, and an
    /// answer whose set reached it warns `results_truncated`.
    #[serde(default = "default_get_symbol_params_limit")]
    #[schemars(range(min = 1_u64, max = PAGE_LIMIT_MAX))]
    pub limit: u64,
    /// Zero-based page of the result set to serve, sized by `limit`. A `page_index` past
    /// the last page returns an empty page whose `pagination` carries the requested
    /// `page_index` and the true `total_pages`.
    #[serde(default = "default_get_symbol_params_page_index")]
    pub page_index: u64,
    /// The version-control revision to read - a branch, tag, or commit id as the
    /// workspace's version control spells it. Omitted reads the current tree. The server
    /// refuses a revision read when the workspace has no version-control repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
}

fn default_get_symbol_params_include() -> Vec<GetSymbolInclude> {
    vec![GetSymbolInclude::Source]
}

fn default_get_symbol_params_limit() -> u64 {
    5
}

fn default_get_symbol_params_page_index() -> u64 {
    PAGE_INDEX_DEFAULT
}

/// One page of declarations matching a name.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_get_symbol_result_empty_defaults)]
#[schemars(extend("examples" = [
    {
        "hits": [
            {
                "symbol": {
                    "id": "rift://symbol/rust/src/config.rs/load_config",
                    "language": "rust",
                    "name": "load_config",
                    "kind": "function",
                    "facets": [
                        "value",
                        "callable",
                        "public"
                    ],
                    "visibility": "pub",
                    "types": [
                        {
                            "role": "return",
                            "origin": "declared",
                            "type": {
                                "language": "rust",
                                "source": "Result<Config, ConfigError>"
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
                            "language": "rust",
                            "parameters": [
                                {
                                    "name": "path",
                                    "types": [
                                        {
                                            "role": "parameter",
                                            "origin": "declared",
                                            "type": {
                                                "language": "rust",
                                                "source": "&Path"
                                            }
                                        }
                                    ],
                                    "optional": false,
                                    "variadic": false
                                }
                            ],
                            "returns": [
                                {
                                    "role": "return",
                                    "origin": "declared",
                                    "type": {
                                        "language": "rust",
                                        "source": "Result<Config, ConfigError>"
                                    }
                                }
                            ]
                        }
                    ],
                    "documentation": [
                        {
                            "format": "markdown",
                            "text": "Loads the workspace configuration from `rift.toml`."
                        }
                    ]
                },
                "path": "src/config.rs",
                "line": 10,
                "range": {
                    "start": 162,
                    "end": 355
                },
                "node": "rift://node/rust/src/config.rs@218-355#67ecfb36",
                "source": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}",
                "history": {
                    "symbol": "rift://symbol/rust/src/config.rs/load_config",
                    "versions": [
                        {
                            "revision": "1f2080e49da12fee4431e6872630509355cd62d1",
                            "path": "src/config.rs",
                            "kind": "signature_changed",
                            "timestamp": "2026-08-21T14:03:22+00:00",
                            "summary": "Return ConfigError from load_config",
                            "author": {
                                "name": "Alice",
                                "email": "alice@example.com"
                            }
                        },
                        {
                            "revision": "8259026556ceae156a29adb53178c842ca32c4a2",
                            "path": "src/config.rs",
                            "kind": "introduced",
                            "timestamp": "2026-08-17T09:41:05+00:00",
                            "summary": "Add workspace configuration loading",
                            "author": {
                                "name": "Alice",
                                "email": "alice@example.com"
                            }
                        }
                    ],
                    "complete": true
                }
            }
        ],
        "pagination": {
            "page_index": 0,
            "total_pages": 1
        }
    }
]))]
pub struct GetSymbolResult {
    /// The declarations on this page, best match first.
    pub hits: Vec<GetSymbolHit>,
    /// Where this page sits in the full result set under the request's `limit`.
    pub pagination: Pagination,
    /// Warnings attached to this result. Absent when there is nothing to warn about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// A language name and its optional dialect. The pair is the identity facts are filed under,
/// so `sql` and `sql:postgresql` are two languages with two symbol spaces. Serializes as one
/// string: `name`, or `name:dialect` when a dialect is set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Language {
    /// The language name, such as `sql`, `json`, or `css`. Lowercase, so `TypeScript` and
    /// `typescript` cannot split one language into two identity spaces.
    pub name: String,
    /// A dialect whose syntax or semantics differ within the language, such as
    /// `postgresql`, `jsonc`, or `scss`. Lowercase, as `name` is.
    pub dialect: Option<String>,
}

impl Language {
    /// The segment a `SymbolId` or `NodeId` files this identity under.
    ///
    /// `name` alone, or `name:dialect` when a dialect is set - the two
    /// spellings the `SymbolId` and `NodeId` patterns advertise after their
    /// `rift://symbol/` and `rift://node/` prefixes.
    #[must_use]
    pub fn identity_segment(&self) -> String {
        match &self.dialect {
            Some(dialect) => format!("{}:{dialect}", self.name),
            None => self.name.clone(),
        }
    }

    /// Parses the exact segment form [`Self::identity_segment`] returns.
    ///
    /// # Errors
    ///
    /// Returns [`LanguageIdentityError`] when the segment is not one lowercase
    /// language word or two such words separated by one colon.
    pub fn from_identity_segment(segment: &str) -> Result<Self, LanguageIdentityError> {
        let (name, dialect) = match segment.split_once(':') {
            Some((name, dialect)) if !dialect.contains(':') => (name, Some(dialect)),
            Some(_) => return Err(LanguageIdentityError::new(segment)),
            None => (segment, None),
        };
        if !is_language_word(name) || dialect.is_some_and(|dialect| !is_language_word(dialect)) {
            return Err(LanguageIdentityError::new(segment));
        }
        Ok(Self {
            name: name.to_owned(),
            dialect: dialect.map(str::to_owned),
        })
    }
}

impl TryFrom<String> for Language {
    type Error = LanguageIdentityError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::from_identity_segment(&text)
    }
}

impl From<Language> for String {
    fn from(language: Language) -> Self {
        language.identity_segment()
    }
}

impl JsonSchema for Language {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Language".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": LANGUAGE_IDENTITY_PATTERN,
            "maxLength": LANGUAGE_IDENTITY_BYTES_MAX,
            "examples": ["rust"],
            "description": "A language name and its optional dialect, joined by `:`. \
                            `sql` and `sql:postgresql` are two languages with two symbol \
                            spaces."
        })
    }
}

/// A language identity segment outside the `name` or `name:dialect` form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageIdentityError {
    segment: String,
}

impl LanguageIdentityError {
    fn new(segment: &str) -> Self {
        Self {
            segment: segment.to_owned(),
        }
    }

    /// The segment that failed to parse.
    #[must_use]
    pub fn segment(&self) -> &str {
        &self.segment
    }
}

impl std::fmt::Display for LanguageIdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "language identity {:?} must use `name` or `name:dialect` with lowercase words",
            self.segment
        )
    }
}

impl std::error::Error for LanguageIdentityError {}

/// Whether one word matches the language identity form.
fn is_language_word(word: &str) -> bool {
    let mut characters = word.chars();
    let starts_lowercase = characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase());
    starts_lowercase
        && word.len() <= LANGUAGE_WORD_BYTES_MAX
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
}

/// One node of a file's concrete syntax tree. It identifies a source range and
/// provider-local syntax kind. `symbol` connects the node to semantic identity when the
/// language supplies one.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_node_empty_defaults)]
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
    /// knowing the grammar that produced them. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<NodeFacet>,
    /// The bytes it spans, as offsets into the file.
    pub range: TextRange,
    /// The node's named parts, including its function body and documentation. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<NodeRegion>,
    /// The region this one is nested inside. Absent at the top level of a unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    /// Syntax facts the model has no field for, namespaced by the provider that emitted
    /// them. Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
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
/// of the SHA-256 of the node's source bytes. The identity describes the node in the
/// revision the response names.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct NodeId(
    #[schemars(example = &"rift://node/rust/lib.rs@220-268#3f9a1c2e")]
    #[schemars(length(min = 27, max = 8192))]
    #[schemars(regex(
        pattern = concat!(
            r"^rift://node/",
            identity_language_segment!(),
            r"/",
            identity_path_character!(),
            r"{1,1000}@\d+-\d+#[0-9a-f]{8}$"
        )
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

/// Lists the syntax nodes covering one position, outermost first. Each node carries its
/// source range and kind, including expressions smaller than a declaration.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {
        "path": "src/config.rs",
        "position": 338
    }
]))]
pub struct NodesParams {
    /// Project-relative file to inspect.
    #[schemars(length(min = 1))]
    pub path: ProjectPath,
    /// UTF-8 byte offset the listed nodes must cover - one position, not a range; the nodes
    /// themselves carry the spans. A position inside a multi-byte character is valid and
    /// answers with its enclosing nodes; a position at or past the file's byte length
    /// refuses.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub position: u64,
    /// The version-control revision to read - a branch, tag, or commit id as the
    /// workspace's version control spells it. Omitted reads the current tree. The server
    /// refuses a revision read when the workspace has no version-control repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
}

/// The nodes covering one position, with identities derived from their source bytes.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_nodes_result_empty_defaults)]
#[schemars(extend("examples" = [
    {
        "nodes": [
            {
                "id": "rift://node/rust/src/config.rs@0-356#dcbef6dd",
                "unit": "rift://file/src/config.rs",
                "language": "rust",
                "kind": "source_file",
                "range": {
                    "start": 0,
                    "end": 356
                }
            },
            {
                "id": "rift://node/rust/src/config.rs@218-355#67ecfb36",
                "symbol": "rift://symbol/rust/src/config.rs/load_config",
                "unit": "rift://file/src/config.rs",
                "language": "rust",
                "kind": "function_item",
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
                "parent": "rift://node/rust/src/config.rs@0-356#dcbef6dd"
            },
            {
                "id": "rift://node/rust/src/config.rs@281-355#4e554fa8",
                "unit": "rift://file/src/config.rs",
                "language": "rust",
                "kind": "block",
                "range": {
                    "start": 281,
                    "end": 355
                },
                "parent": "rift://node/rust/src/config.rs@218-355#67ecfb36"
            },
            {
                "id": "rift://node/rust/src/config.rs@334-353#4df4426e",
                "unit": "rift://file/src/config.rs",
                "language": "rust",
                "kind": "call_expression",
                "facets": [
                    "expression"
                ],
                "range": {
                    "start": 334,
                    "end": 353
                },
                "parent": "rift://node/rust/src/config.rs@281-355#4e554fa8"
            },
            {
                "id": "rift://node/rust/src/config.rs@334-346#03f22dac",
                "unit": "rift://file/src/config.rs",
                "language": "rust",
                "kind": "identifier",
                "range": {
                    "start": 334,
                    "end": 346
                },
                "parent": "rift://node/rust/src/config.rs@334-353#4df4426e"
            }
        ],
        "source": [
            "use std::path::Path;\n\nuse crate::error::ConfigError;\n\n/// Workspace configuration read from `rift.toml`.\npub struct Config {\n    pub root: std::path::PathBuf,\n}\n\n/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}\n",
            "pub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}",
            "{\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}",
            "parse_config(&text)",
            "parse_config"
        ]
    }
]))]
pub struct NodesResult {
    /// Nodes covering the position, outermost first.
    pub nodes: Vec<Node>,
    /// One excerpt per node in `nodes`, in the same order, each spanning that node's own
    /// range. Empty when `nodes` is empty.
    pub source: Vec<String>,
    /// Warnings attached to this result. Absent when there is nothing to warn about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// One package as its package manager identifies it.
#[derive(Clone, Debug, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    /// Package manager or ecosystem name.
    #[schemars(length(max = 128))]
    pub manager: String,
    /// Canonical registry endpoint, including its path when that path identifies the registry.
    /// Credentials, query and fragment are never part of this owner.
    #[schemars(length(min = 1, max = 4096))]
    pub registry: String,
    /// Package name in that ecosystem.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// Resolved package version.
    #[schemars(length(max = 4096))]
    pub version: String,
}

/// One exact runtime or compiler release.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    /// Canonical runtime or compiler name.
    #[schemars(length(min = 1, max = 128))]
    pub runtime: String,
    /// Exact runtime or compiler version.
    #[schemars(length(min = 1, max = 4096))]
    pub version: String,
}

impl PackageIdentity {
    /// Validated defining registry owner of this exact package release.
    ///
    /// # Errors
    /// Returns the violated owner or length bound.
    pub fn owner(
        &self,
    ) -> Result<crate::identity::SymbolOwner, crate::identity::SymbolIdentityViolation> {
        if self.manager.len() > 128
            || self.registry.len() > 4096
            || self.name.len() > 4096
            || self.version.len() > 4096
        {
            return Err(crate::identity::SymbolIdentityViolation::Length);
        }
        let owner = crate::identity::SymbolOwner::Package {
            manager: self.manager.clone(),
            registry: self.registry.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
        };
        owner.validate()?;
        Ok(owner)
    }
}

impl<'de> Deserialize<'de> for PackageIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            manager: String,
            registry: String,
            name: String,
            version: String,
        }
        let fields = Fields::deserialize(deserializer)?;
        let package = Self {
            manager: fields.manager,
            registry: fields.registry,
            name: fields.name,
            version: fields.version,
        };
        package
            .owner()
            .map_err(|_| serde::de::Error::custom("package owner is invalid"))?;
        Ok(package)
    }
}

impl RuntimeIdentity {
    /// Validated defining runtime or compiler owner of this exact release.
    ///
    /// # Errors
    /// Returns the violated owner or length bound.
    pub fn owner(
        &self,
    ) -> Result<crate::identity::SymbolOwner, crate::identity::SymbolIdentityViolation> {
        if self.runtime.len() > 128 || self.version.len() > 4096 {
            return Err(crate::identity::SymbolIdentityViolation::Length);
        }
        let owner = crate::identity::SymbolOwner::Runtime {
            runtime: self.runtime.clone(),
            version: self.version.clone(),
        };
        owner.validate()?;
        Ok(owner)
    }
}

/// Default `page_index` for a paginated request: the first page.
pub const PAGE_INDEX_DEFAULT: u64 = 0;

/// Largest `limit` a paginated request may name; the server refuses a larger one.
pub const PAGE_LIMIT_MAX: u64 = 10_000;

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
#[schemars(transform = schema::declare_parameter_empty_defaults)]
pub struct Parameter {
    /// What the parameter is called. Absent where the language allows an unnamed one, as
    /// a positional parameter in a function type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where this parameter is written in the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// What it accepts, absent when empty. An array because a declared type and an
    /// inferred one are separate bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeBinding>,
    /// Whether a call may leave it out.
    pub optional: bool,
    /// Whether it absorbs the arguments that follow - `*args`, `...rest`.
    pub variadic: bool,
    /// The default value as written in the source. Absent where there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Parameter facts the model has no field for, namespaced by the provider that emitted
    /// them. Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// One path below the workspace root, using forward slashes and UTF-8 in Unicode NFC - Rift
/// normalizes what it emits and what it accepts, and compares byte-for-byte. The empty path
/// names the root itself. Absolute paths, backslashes, control characters, empty segments,
/// and `.` or `..` segments are refused before the filesystem is touched. The limit is 1000
/// UTF-8 bytes, not characters. A workspace holding two entries whose NFC forms are equal
/// fails the read that touches them with `content_unavailable`.
#[derive(
    Clone, Debug, Default, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ProjectPath(
    #[schemars(example = &"src/lib.rs")]
    #[schemars(length(max = 1000))]
    #[schemars(regex(
        pattern = concat!(
            "^(?:",
            project_path_first_segment!(),
            "(?:/",
            project_path_segment!(),
            ")*)?$"
        )
    ))]
    pub String,
);

/// Most package and dependency-context warnings one answer carries together: degraded
/// resolvers, then entries no public registry serves, then packages absent from the global
/// publication, then entries a release other than the requested one answers.
pub const DEPENDENCY_WARNINGS_MAX: usize = 8;

/// Most characters the `detail` of one global API warning carries: the longest
/// `requirement_unsatisfied` detail the global API's package fields admit,
/// `<manager>/<name> <requirement> answered by <version>`, at 128 characters of manager and
/// 4,096 characters each of name, requirement, and version.
pub const GLOBAL_WARNING_DETAIL_CHARS_MAX: usize = 12_431;

/// Most `source_unavailable` warnings one answer carries for the files the index left out,
/// in project-path order; when more files are left out, one more warning follows them and
/// counts the rest.
pub const SOURCE_WARNINGS_MAX: usize = 8;

/// Frameworks whose syntax depends on package or component context.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxFramework {
    /// Angular templates owned by an imported `Component` decorator.
    Angular,
    /// Tailwind directives and classes under a known major version.
    Tailwind,
}

/// One warning attached to a read result. The answer stands; the warning carries evidence
/// of a condition the caller weighs before relying on it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "code", deny_unknown_fields, rename_all = "snake_case")]
pub enum ReadWarning {
    /// Framework context could not be resolved for one source. Ordinary syntax stands;
    /// framework-specific facts whose context is missing are left out.
    FrameworkContextUnresolved {
        /// The source whose framework context is unresolved.
        unit: FileId,
        /// The framework whose context could not be established.
        framework: SyntaxFramework,
        /// The missing package version, configuration, or template source.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The lookup found no declaration under its name, language, and scope.
    /// `alternatives` names up to three project declarations, ordered by the smallest
    /// case-insensitive Unicode Levenshtein distance to their short or qualified names.
    /// Equal distances use qualified name, then project path order. Revision reads
    /// use that revision's declarations. A `global` lookup carries no alternatives;
    /// an `all` lookup proposes project declarations alone. If complete closest ranking
    /// exceeds `search.symbol_alternatives_work`, alternatives are empty and `detail`
    /// carries the registered failure's message, recovery action, and observed work.
    SymbolNotFound {
        /// The declaration name the caller requested.
        #[schemars(length(min = 1, max = 4096))]
        name: String,
        /// Project declaration identities the caller can use to select another name.
        #[schemars(length(max = 3))]
        alternatives: Vec<SymbolId>,
        /// Why closest declarations could not be selected within the work bound.
        /// Absent when every selected declaration was compared or scope is `global`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = 4096))]
        detail: Option<String>,
    },
    /// Documentation collection or excerpt output omitted part of a selected source.
    Documentation {
        /// Bounded source identity, stage, failure label, and omitted count.
        warning: crate::documentation::DocumentationWarning,
    },
    /// The documentation this read projects onto crossed a bound, so no documentation
    /// block joins the answer: declarations and files answer as they would without it.
    /// The condition holds until the documentation it names changes.
    DocumentationUnavailable {
        /// Which documentation was left out and the bound it crossed - prose for a
        /// reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The answer was computed from an index that lags the tree the read captured. Facts
    /// derived from the index may miss the newest writes; the digests state which two
    /// trees disagree. When the two digests are equal, the tree moved in recorded files
    /// outside the syntax-indexed ones or in the configuration file, and `detail` says
    /// which.
    StaleIndex {
        /// Tree revision the published index covers.
        index_tree_revision: Digest,
        /// Tree revision the read captured.
        captured_tree_revision: Digest,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The vector ranking holds no vectors for the tree this read captured, so the answer
    /// was ranked lexically alone. The pass over that tree is still running, or the tree
    /// was published after the corpus the ranking holds was described. `prepared` and
    /// `total` state how far the ranking has got for the captured tree, and `ready_in` is
    /// derived from workspace size and embedding progress.
    VectorIndexPreparing {
        /// Declarations of the captured tree that already carry a vector.
        prepared: u64,
        /// Declarations the set being embedded holds.
        total: u64,
        /// Estimated wait before the vector ranking joins an answer, not a measurement
        /// of this machine. A caller may report it and must not schedule against it.
        ready_in: Duration,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The local index is still preparing selected workspace files. The answer covers only
    /// files whose capture and analysis have finished; `total` is absent until discovery ends.
    LocalIndexPreparing {
        /// Selected workspace files whose capture and analysis have finished.
        #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
        prepared: u64,
        /// Selected workspace files, once discovery has finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
        total: Option<u64>,
        /// Estimated wait before local preparation finishes. No estimate is available yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready_in: Option<Duration>,
        /// Why the local index is preparing - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The vector ranking will not answer for the life of this server, so every answer
    /// is ranked lexically alone. No retry is coming: fix the `[search.vector]`
    /// configuration and start the server again.
    VectorRankingUnavailable {
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The full-text tier did not rank this answer, so it came from identifier matching
    /// alone: a query phrased as prose finds nothing. The tier is still committing the
    /// captured tree, missed a commit, holds no indexed tree, failed to load, got no
    /// database connection within `[search] busy_timeout`, or still answers for a newer
    /// publication after the request's capture attempts; `detail` names which, and what
    /// clears it.
    LexicalRankingUnavailable {
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The query carried more terms and quoted phrases than `terms_max`, so the server
    /// dropped the shortest unquoted terms and ranked the answer by the rest. Every
    /// quoted phrase is kept. Shorten `query`, or quote the terms that must be matched.
    QueryNarrowed {
        /// Terms and quoted phrases the query was cut to: the server's bound on one
        /// parsed query.
        terms_max: u64,
    },
    /// The lexical ranking stopped at `matches_max` units; hits past it never reached the
    /// page, whatever `paths` selects. Narrow `query`.
    LexicalRankingTruncated {
        /// Units the ranking stopped at: the server's bound on one lexical ranking.
        matches_max: u64,
    },
    /// The server reached `results_max`, its result bound, before ordering and paging:
    /// what the bound cut never reaches any page, and `total_pages` counts only what fit.
    /// The bound cuts the ranked candidates and the hit set alike. The warning means the
    /// bound was reached; a set of exactly `results_max` hits carries it too. Narrow
    /// `query` or `paths`.
    ResultsTruncated {
        /// The bound the read stopped at: the server's limit on both the candidates one
        /// read ranks and the hits it returns.
        results_max: u64,
    },
    /// A file held more matches of `pattern` than `matches_per_file`, so its later matches
    /// are missing from this answer while every other file answers in full. `files` names
    /// the cut files in project-path order, at most `SOURCE_WARNINGS_MAX` of them. Narrow
    /// `pattern` or `paths`, or raise the `[search]` key `pattern_matches_per_file`.
    PatternMatchesTruncated {
        /// Matches one file contributes at most: the server's bound on one file's matches.
        matches_per_file: u64,
        /// The files cut at the bound.
        #[schemars(length(max = 8))]
        files: Vec<FileId>,
    },
    /// The trigram index `pattern` selects its files through lacks rows of stored file
    /// text, and verifying those rows beside the selected ones would pass the `[search]`
    /// key `pattern_candidate_rows` or `pattern_verified_size`, so the answer covers the
    /// rows the index holds and a match in the other rows is missing from it. The server
    /// indexes those rows in the background after each write; `prepared` and `total` state
    /// how far it has got. Resend the request once the index has caught up.
    PatternIndexPreparing {
        /// Rows of stored file text the trigram index holds: one per file, or one per
        /// chunk of a file split under `[search.text]`.
        prepared: u64,
        /// Rows of stored file text the store holds.
        total: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// A file answers nothing from its bytes. Either the index left the claimed file out -
    /// its bytes are not valid UTF-8, or it crosses a per-file bound - so it answers no
    /// search or lookup, and addressing it directly still refuses `content_unavailable`;
    /// or a comparison against the working tree read the changed file in no working form -
    /// its attributes name a `filter` driver or a UTF-16 `working-tree-encoding` - so it
    /// answers changed with no declarations. Every other file in the workspace stays
    /// available. At most `SOURCE_WARNINGS_MAX` of this warning name a file, in
    /// project-path order; when more files are named, one more carries no `unit` and
    /// counts the rest. `rift server logs` names each file the index left out, and `paths`
    /// narrows a comparison onto the others.
    SourceUnavailable {
        /// The file the warning names. Absent on the one warning that counts the files
        /// past `SOURCE_WARNINGS_MAX`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<FileId>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// Text files past `[search.text] max_chunk` are left out of the text index under the
    /// key `large_files = "skip"`, so no hit answers from their text; `skipped` counts the
    /// ones the request's `paths` reach. A declaration such a file holds still answers.
    /// Setting `large_files` to `split` indexes them in chunks.
    LargeFileSkipped {
        /// The files past `max_chunk` the request's `paths` reach.
        skipped: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// Files past `[providers.syntax] max_file` are held as text the syntax provider does
    /// not parse: their text answers `search`, and none of their declarations were
    /// extracted, so no symbol hit and no `get_symbol` answer comes from them. `files`
    /// names them in project-path order, at most `SOURCE_WARNINGS_MAX` of them. Raising
    /// `max_file` parses them.
    LargeFileUnparsed {
        /// The files held as text alone.
        #[schemars(length(max = 8))]
        files: Vec<FileId>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// A lockfile the request's `paths.include` selects is left out of search, so no hit
    /// answers from it; `rift://map` still carries the versions it pins. Naming the file
    /// in `paths.force_include` searches it for one request, and removing its name from
    /// the `[search.text]` key `excluded_lockfiles` indexes it. One warning names every
    /// such file, in project-path order, at most `SOURCE_WARNINGS_MAX` of them.
    LockfileExcluded {
        /// The selected lockfiles search leaves out.
        #[schemars(length(max = 8))]
        files: Vec<FileId>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// Contributions selected for one symbol's presentation disagree on at least one
    /// field. The answer carries what normalization selected.
    SymbolDisagreement {
        /// The symbol whose presentation facts disagree.
        symbol: SymbolId,
        /// Providers whose Contribution carries a differing value for at least one
        /// presentation field, sorted and deduplicated.
        #[schemars(length(min = 1))]
        providers: Vec<String>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// A traversal walk stopped at its node bound before exhausting the reachable graph,
    /// so hits reachable beyond the bound are missing from this answer. Narrow the walk -
    /// a tighter `facets` list, a smaller `depth`, or a less-connected seed - to fit it
    /// under the bound.
    TraversalTruncated {
        /// Symbols the walk visited before it stopped, equal to the bound it hit.
        visited: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// No lane populates part of the relationship coverage the traversal asked for, so the
    /// walk had nothing to follow there whatever the graph holds. The warning states that a
    /// lane is absent; it never states that the seed has no such neighbor. An empty answer
    /// carrying it means the walk could not run; an empty answer without it means the walk
    /// ran and the seed has no neighbor under the request.
    RelationshipCoverageMissing {
        /// The requested facets no lane populates, in the request's own order,
        /// deduplicated. Absent when every requested facet has a lane.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        facets: Vec<RelationshipFacet>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// A language engine serves the seed's language, and this read carries none of its
    /// edges: the engine answered about bytes the served revision does not carry, answered
    /// more than the walk's node bound holds, or was still analyzing when
    /// `[server] readiness_timeout` was spent. The indexed relationships stand; whatever
    /// the engine resolves on top of them is missing. The warning states that an engine's
    /// analysis is absent from this answer; it never states that the seed has no such
    /// neighbor. An engine still analyzing keeps loading, and a later read, served from a
    /// revision the engine has caught up with or sent once it reads ready, carries the
    /// engine's edges again.
    EngineAnalysisUnavailable {
        /// The seed declaration's language, whose engine's answer was dropped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<Language>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// An outgoing walk dropped its edges to callees it named no declaration for. The
    /// language engine's call hierarchy named each in a file outside the project and every
    /// installed package the workspace's dependencies name, or in a package or standard
    /// library file where the global index answered no declaration at the callee's
    /// position. Every package callee drops when the global API is off or did not answer,
    /// and the answer then carries the global warning naming why. Every other edge stands.
    /// The warning states that edges are missing from this answer; it never states that
    /// the seed calls nothing more.
    CalleesDropped {
        /// Edges the walk dropped over every depth, one per call the engine named.
        callees: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The walk took a language engine's answer without progress evidence that the engine
    /// had settled: it announced no work since it started or was last told of a changed
    /// file, and stayed quiet past its `settle_delay`. The answer stands. An engine that
    /// recomputes without announcing work may have answered before it read that change, and
    /// a later read meets it settled.
    EngineReadinessUnconfirmed {
        /// The engines the walk took unconfirmed, by accepted process key, sorted. Inline
        /// processes use the exact language identity segment.
        #[schemars(length(min = 1))]
        processes: Vec<String>,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The history store the server fills in the background has not analyzed every commit
    /// `[providers.history]` selects, so a commit search answers from the `analyzed` ones
    /// alone and a commit the store has not reached answers nothing. The fill runs newest
    /// first and continues without a request; a later search answers the rest.
    HistoryStoreFilling {
        /// Commits the history store holds of the ones its latest fill selects.
        analyzed: u64,
        /// Commits the history store's latest fill selects.
        total: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// The comparison reached `paths_max` changed paths and stopped there, so declarations
    /// in the changed paths past it are missing from this answer. Narrow the comparison
    /// with `paths`, or name two sides that differ in fewer files.
    ChangeTruncated {
        /// Changed paths the comparison stopped at, equal to the bound it reached.
        paths_max: u64,
        /// Why the warning was raised - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        detail: String,
    },
    /// Global access is disabled under `[global] enabled = false`, so the answer carries no
    /// package facts.
    GlobalAccessDisabled,
    /// The global API could not answer, so the answer carries no package facts.
    GlobalApiUnavailable {
        /// Bounded class of the global API failure.
        failure_class: GlobalFailureClass,
    },
    /// The global API advertised a publication the client cannot read, so the answer
    /// carries no package facts.
    GlobalPublicationIncompatible {
        /// Bounded class of the incompatible publication.
        failure_class: GlobalFailureClass,
    },
    /// The global API returned an invalid or truncated response, so the answer carries no
    /// package facts.
    GlobalResponseInvalid {
        /// Bounded class of the invalid response.
        failure_class: GlobalFailureClass,
    },
    /// The global package page carried a bounded condition while its items remained valid.
    /// `warning_code` identifies the condition and `detail` carries its bounded explanation.
    /// `capability_unavailable` also names a feature the global API's capabilities do not
    /// advertise, such as `patterns` for a `pattern` search, and the answer then carries the
    /// project hits alone.
    GlobalPageWarning {
        /// Stable condition code returned by the global package service, or
        /// `capability_unavailable` for a feature its capabilities do not advertise.
        warning_code: GlobalPageWarningCode,
        /// Optional bounded explanation returned by the global package service, or the
        /// feature its capabilities do not advertise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(max = 12_431))]
        detail: Option<String>,
    },
    /// The global publication holds no release of an exact package the dependency
    /// context names, so nothing answers for it. At most `DEPENDENCY_WARNINGS_MAX` package
    /// and dependency-context warnings ride one answer.
    PackageAbsent {
        /// The exact package absent from the global publication.
        package: PackageIdentity,
    },
    /// The global publication holds no release a declared requirement admits, so nothing
    /// answers for it. At most `DEPENDENCY_WARNINGS_MAX` package and dependency-context
    /// warnings ride one answer.
    PackageRequirementAbsent {
        /// The declared requirement absent from the global publication.
        entry: crate::dependencies::PackageContextEntry,
    },
    /// The global publication holds no release at the exact version the dependency context
    /// names, or none inside the range a declared requirement states, so the collected
    /// release nearest it answers in its place: the package facts for `entry` come from
    /// `package`. At most `DEPENDENCY_WARNINGS_MAX` package and dependency-context warnings
    /// ride one answer.
    PackageSubstituted {
        /// The dependency context entry, as the workspace's manifests and lockfiles state it.
        entry: crate::dependencies::PackageContextEntry,
        /// The collected release that answers for `entry`.
        package: PackageIdentity,
    },
    /// The dependency context names a package no public registry serves: a path outside
    /// the workspace, a git repository, a private registry, or a URL. No global package
    /// index answers for it, and `reason` names the capability Rift does not have yet. At
    /// most `DEPENDENCY_WARNINGS_MAX` package and dependency-context warnings ride one
    /// answer.
    PackageUnavailable {
        /// The dependency context entry, as the workspace's manifests and lockfiles state
        /// it; its `availability` names the kind.
        entry: crate::dependencies::PackageContextEntry,
        /// Why no global package index answers for the entry, for a reader.
        #[schemars(length(max = 4096))]
        reason: String,
    },
    /// One resolver or standard library probe read less than the workspace states, or the
    /// context passed the bound on the entries one read carries, the smaller of the
    /// server's own and the one the global API advertises, and its entries sorted last left:
    /// every held entry before any the request's `packages` names. The dependency context
    /// may then miss packages or name a standard library by its static reading. Rides only an answer whose `scope`
    /// reaches packages; at most `DEPENDENCY_WARNINGS_MAX` package and dependency-context
    /// warnings ride one answer, this one first, in resolver order.
    PackageContextDegraded {
        /// What degraded: a resolver by its manager name, `cargo`, `uv`, `npm`, or `bun`, a
        /// standard library entry, `stdlib/rust`, `stdlib/node`, or `stdlib/python`, or the
        /// package manager whose held or requested entries left past the entry bound, such
        /// as `pypi`.
        #[schemars(length(max = 128))]
        resolver: String,
        /// What the resolver could not do - prose for a reader; nothing keys on it.
        #[schemars(length(max = 4096))]
        reason: String,
    },
}

/// Bounded classes a global API failure can carry in a read warning.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GlobalFailureClass {
    /// The endpoint could not accept a connection.
    Connection,
    /// The operation exceeded its time bound.
    Timeout,
    /// Bounded retry attempts ended without a valid response.
    RetryExhausted,
    /// The endpoint rejected credentials or access.
    Authentication,
    /// Credential configuration prevented a request.
    CredentialConfiguration,
    /// The endpoint returned a non-success status.
    NonSuccessResponse,
    /// The response did not satisfy its wire fields.
    InvalidResponse,
    /// The response exceeded its byte or item bound.
    ResponseTruncated,
    /// The publication format was not supported.
    PublicationFormat,
    /// The response named an unsupported corpus revision.
    CorpusRevision,
    /// The response omitted a required field set.
    RequiredFieldSet,
}

/// Stable conditions a global package page can report while returning valid items.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GlobalPageWarningCode {
    /// The service narrowed a query before ranking.
    QueryNarrowed,
    /// The service omitted source content at its source bound.
    SourceTruncated,
    /// The publication changed while pages were assembled.
    PublicationChanged,
    /// A requested capability was unavailable.
    CapabilityUnavailable,
    /// The service stopped before its result bound.
    ResultTruncated,
    /// The service returned a code this client does not know.
    Unknown,
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
#[schemars(transform = schema::declare_relationship_empty_defaults)]
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
    /// The nodes this edge was read from. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<NodeId>,
    /// How this edge was established.
    pub derivation: RelationshipDerivation,
    /// How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other
    /// derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 1))]
    pub confidence: Option<f64>,
    /// Edge facts the model has no field for, namespaced by the provider that emitted them.
    /// Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// How this edge was established. Every edge reaches Rift from a provider, and this field
/// records whether the provider resolved the edge semantically, read it from syntax, or
/// inferred it.
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
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    strum::AsRefStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
/// the schema's `maxLength` counts the same units.
pub const REVISION_ID_BYTES_MAX: usize = 128;

/// Identity of one revision in the workspace's version-control history, spelled the way the
/// version-control system spells it: a branch, tag, or commit id, optionally followed by
/// ancestry suffixes, such as `HEAD~2` for the second first-parent ancestor or `main^2` for
/// the second parent. Rift carries it opaquely and never orders two revisions by comparing
/// their identifiers.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct RevisionId(
    #[schemars(example = &"main")]
    #[schemars(length(min = 1, max = 128))]
    #[schemars(regex(pattern = r"^[A-Za-z0-9._/-]+(?:[~^][0-9]*)*$"))]
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
    /// The spelling carries a byte outside `A-Z a-z 0-9 . _ / - ~ ^`.
    CharsetForbidden,
    /// An ancestry suffix follows no name, or a `~` or `^` is followed by something other
    /// than digits or another suffix, as in `~1` or `HEAD~1/src`.
    AncestryInvalid,
}

impl RevisionIdViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::CharsetForbidden => "charset_forbidden",
            Self::AncestryInvalid => "ancestry_invalid",
        }
    }
}

/// Classifies one revision spelling against the rules [`RevisionId`]'s schema advertises.
/// Arms are ordered by precedence: the first matching rule names the violation.
fn revision_id_violation(value: &str) -> Option<RevisionIdViolation> {
    let name_byte =
        |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-');
    let accepted = |byte: &u8| name_byte(byte) || ANCESTRY_MARKERS.contains(byte);
    let name_end = value
        .bytes()
        .position(|byte| ANCESTRY_MARKERS.contains(&byte))
        .unwrap_or(value.len());
    let (name, ancestry) = value.as_bytes().split_at(name_end);
    let ancestry_accepted = ancestry
        .iter()
        .all(|byte| byte.is_ascii_digit() || ANCESTRY_MARKERS.contains(byte));
    match value.as_bytes() {
        [] => Some(RevisionIdViolation::Empty),
        bytes if bytes.len() > REVISION_ID_BYTES_MAX => Some(RevisionIdViolation::TooLong),
        bytes if !bytes.iter().all(accepted) => Some(RevisionIdViolation::CharsetForbidden),
        _ if name.is_empty() || !ancestry_accepted => Some(RevisionIdViolation::AncestryInvalid),
        _ => None,
    }
}

/// The bytes that open an ancestry suffix on a revision name: `~` walks first parents and
/// `^` picks a parent, each optionally followed by a count.
const ANCESTRY_MARKERS: [u8; 2] = *b"~^";

/// Which corpus a read searches. The names identify logical corpora, not storage
/// locations: `global` reaches dependency package facts wherever the server holds them.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// The project tree the server serves.
    #[default]
    Local,
    /// The dependency packages, answered from their public declarations alone.
    Global,
    /// Both: hits merge by rank; at equal rank a project hit orders before a package hit.
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
#[schemars(transform = schema::declare_signature_empty_defaults)]
pub struct Signature {
    /// The signature as a reader sees it, in the language's own syntax.
    pub display: String,
    /// Symbols named inside `display`, each with the byte range of `display` that names it,
    /// so a renderer can turn the rendered text into links. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<SignatureLink>,
    /// The language whose syntax `display` is written in.
    pub language: Language,
    /// The implicit first parameter - `self`, `this`. Absent for a free function, and for
    /// languages that have no such thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<Parameter>,
    /// Declared parameters, in source order. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// What the call yields, absent when empty. An array because a language may return
    /// several values, and because a declared and an inferred return are separate
    /// bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns: Vec<TypeBinding>,
    /// The generic parameters this form declares, each as the symbol that declares it.
    /// Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_parameters: Vec<SymbolId>,
    /// Types this form declares it can raise. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub throws: Vec<TypeExpression>,
    /// Effect keywords the declaration carries, in the language's own words: `async`,
    /// `unsafe`, `pure`. The spelling is preserved and never mapped onto a portable
    /// meaning; absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
    /// Signature facts the model has no field for, namespaced by the provider that emitted
    /// them. Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// One symbol named inside a rendered signature, with the byte range of that rendering which
/// names it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
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
/// generated. `rift-core`'s `ContributionOrigin` carries this exact type as its own
/// working representation; no served tool schema reaches it, so it carries no wire
/// examples of its own - [`SourceLocationKind`] is what a caller reads on `SymbolOrigin`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
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
    Stdlib {
        /// Exact owning runtime or compiler, absent when accepted evidence does not name a release.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<RuntimeIdentity>,
    },
    /// Source outside the project, dependency graph, and standard library.
    External {},
}

/// Which of the four places a declaration's source belongs, on `SymbolOrigin`. Package
/// ownership is the separate `package` field: a `project` declaration can carry one too,
/// and `dependency` always does.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceLocationKind {
    /// Owned by the current workspace.
    Project,
    /// Owned by one resolved dependency. The ECMAScript built-ins, such as `Array`, belong
    /// to the npm `typescript` package, whose `lib.*.d.ts` files declare them.
    Dependency,
    /// Installed with the language toolchain: `stdlib/rust`, `stdlib/node`, or
    /// `stdlib/python`.
    Stdlib,
    /// Outside the project, dependency graph, and standard library.
    External,
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

/// Identity of one physical source unit: its defining package or runtime owner and original
/// root-relative path, or its source resolver and canonical unit key. Project sources use
/// `rift://source/project/src/lib.rs`. The codec validates ownership, UTF-8, relative paths
/// and canonical percent-encoding before lookup. Content digests remain separate from source
/// addresses.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceUnitId(pub String);

impl JsonSchema for SourceUnitId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SourceUnitId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 17,
            "maxLength": crate::identity::SYMBOL_ID_BYTES_MAX,
            "pattern": crate::identity::SOURCE_UNIT_ID_PATTERN,
            "if": {"pattern": "^rift://source/(?:cargo|npm|pypi|stdlib)/"},
            "then": {"pattern": crate::identity::RELEASED_SOURCE_UNIT_ID_PATTERN},
            "else": {"pattern": crate::identity::GENERIC_SOURCE_UNIT_ID_PATTERN},
            "not": {"anyOf": [
                {"pattern": "/\\.{1,2}(?:/|$)"},
                {"pattern": "^rift://source/[^/]+/[A-Za-z]:"},
                {"pattern": "^rift://source/(?:(?:cargo|npm|pypi)/[^/]+/(?:@[^/]+/)?[^/]+@[^/]+/)[A-Za-z]:"},
                {"pattern": "^rift://source/stdlib/[^/]+@[^/]+/[A-Za-z]:"}
            ]},
            "description": "Physical source identity with a defining package or runtime owner, or a source resolver and canonical unit key. The codec validates decoded paths, UTF-8 and canonical percent-encoding before lookup.",
            "examples": [
                "rift://source/project/src/lib.rs",
                "rift://source/npm/npmjs.org/@types/node@26.6.4/fs.d.ts",
                "rift://source/stdlib/cpython@3.12.9/Lib/sys.py"
            ]
        })
    }
}

impl SourceUnitId {
    /// Accepts one canonical physical source identity.
    ///
    /// # Errors
    /// Returns a violation for malformed ownership, paths, UTF-8 or encoding.
    pub fn parse(value: &str) -> Result<Self, crate::identity::SymbolIdentityViolation> {
        crate::identity::parse_source_unit_identity(value)?;
        Ok(Self(value.to_owned()))
    }

    /// Returns the canonical physical source address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SourceUnitId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        crate::identity::parse_source_unit_identity(&value).map_err(|violation| {
            serde::de::Error::custom(format!("invalid source-unit identity: {violation:?}"))
        })?;
        Ok(Self(value))
    }
}

/// One byte range in a source-catalog unit.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceUnitSpan {
    /// Source unit containing the bytes.
    pub unit: SourceUnitId,
    /// Half-open UTF-8 byte range in that unit.
    pub range: TextRange,
}

/// Readable Symbol assembled from normalized Contributions. Source structure lives in Node
/// and is connected through Relationship.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[schemars(transform = schema::declare_symbol_empty_defaults)]
pub struct Symbol {
    /// Unique identifier of this Symbol across the whole workspace. Absent for an
    /// unestablished symbol: no accepted evidence, or more than one, established its
    /// identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<SymbolId>,
    /// The language this symbol belongs to.
    pub language: Language,
    /// The human-readable name, as written in the source: `parseConfig`. Rendered
    /// signatures live in `signatures`.
    #[schemars(length(max = 4096))]
    pub name: String,
    /// What this symbol is in the provider's vocabulary, such as `trait`, `function`, or
    /// `table`.
    pub kind: ExactKind,
    /// Portable classification for cross-language queries, absent when empty. The local
    /// kinds `trait` and `interface` can both carry the `type` facet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<SymbolFacet>,
    /// Where the declaration belongs and how it was produced. Absent when it says a
    /// project declaration, authored, with no package - the common case.
    #[serde(
        default = "default_symbol_origin",
        skip_serializing_if = "SymbolOrigin::is_common_default"
    )]
    pub origin: SymbolOrigin,
    /// The symbol this one belongs to - the class that owns a method, the module that owns
    /// a function. Ownership is not lexical: a Go method sits beside its type and a Rust
    /// method inside an `impl` block, both naming the type here; absent at the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<SymbolId>,
    /// Language keywords qualifying the declaration: `export`, `async`, `const`. Absent
    /// when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    /// How widely the symbol is visible, in the language's own terms - `public`, `private`,
    /// `pub(crate)`. Absent where the language has no such concept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// The types this symbol carries, each tagged with the role it plays: a return type, a
    /// field type, a bound. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeBinding>,
    /// One entry per callable form, absent when empty. Where the language dispatches
    /// overloads separately they are separate symbols joined by the `overloads` edge;
    /// several entries here are alternative forms of one dispatch target, as
    /// `typing.overload` writes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<Signature>,
    /// Doc comments attached to the declaration, with the markup format they were written
    /// in. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documentation: Vec<Documentation>,
    /// Language-specific facts with no portable equivalent, namespaced by the provider that
    /// emitted them. Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
    /// Whether language semantics confine this symbol to the document that declares it. The
    /// provider classifies locality from its language model; absent when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
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
/// first. The timeline follows first parents from the served revision, or the selected
/// releases in version order, through the revisions the history store holds, and follows
/// the declaration's file across a rename that kept its bytes. It is bounded by
/// `[providers.history] max_revisions` and by a shallow clone's boundary.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolHistory {
    /// The symbol the timeline is for.
    pub symbol: SymbolId,
    /// Revisions that touched the symbol, newest first.
    pub versions: Vec<SymbolVersion>,
    /// Whether the timeline reached the repository's first commit. `false` when the
    /// `max_revisions` bound, a shallow clone's boundary, or the oldest selected release
    /// ended it first, or while the history store has not yet analyzed the served
    /// revision, so revisions older than the listed ones may have touched the symbol.
    pub complete: bool,
}

/// Identity of one symbol: the language, the path of the declaring file, and the provider's
/// stable qualified name for the declaration. No shipped provider puts the file path into a
/// qualified name, so a declaration moved to another file keeps its qualified name while its
/// identity names the new path. A `~N` suffix separates declarations the qualified name
/// alone cannot, such as overloads that dispatch separately.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct SymbolId(
    #[schemars(example = &"rift://symbol/rust/crates/rift-server/src/read.rs/ReadService")]
    #[schemars(length(min = 17, max = 8192))]
    #[schemars(regex(
        pattern = concat!(
            r"^rift://symbol/",
            identity_language_segment!(),
            r"/",
            identity_path_character!(),
            r"{1,1000}$"
        )
    ))]
    pub String,
);

impl SymbolId {
    /// Parses one canonical logical symbol identity without lookup or I/O.
    ///
    /// # Errors
    /// Returns the codec violation when ownership, hierarchy or encoding is invalid.
    pub fn parse(value: &str) -> Result<Self, crate::identity::SymbolIdentityViolation> {
        crate::identity::SymbolIdentity::parse(value).map(|identity| Self(identity.wire_identity()))
    }

    /// Builds a wire identifier from an already validated logical identity.
    #[must_use]
    pub fn from_identity(identity: &crate::identity::SymbolIdentity) -> Self {
        Self(identity.wire_identity())
    }

    /// The canonical wire spelling of this identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Takes the canonical wire spelling of this identifier.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Where a symbol belongs and how its declaration came to exist. Source location and
/// generation are separate: generated code can belong to the project or to a dependency.
/// Absent from `Symbol` entirely when it says a project declaration, authored, with no
/// package.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SymbolOrigin {
    /// Which of the four places the declaration belongs. Absent exactly when
    /// `source_kind` is `synthetic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocationKind>,
    /// The package that owns the declaration: present for `dependency`, and optionally
    /// for `project`. Absent for `stdlib`, `external`, and a synthetic declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageIdentity>,
    /// Exact runtime or compiler release that owns a standard-library declaration.
    /// Mutually exclusive with `package`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeIdentity>,
    /// Whether the declaration is authored, generated, or synthetic.
    pub source_kind: SourceKind,
}

/// `SymbolOrigin`'s wire default: a project declaration, authored, with no package.
/// `Symbol.origin` omits itself from the wire when it equals this value.
fn default_symbol_origin() -> SymbolOrigin {
    SymbolOrigin {
        location: Some(SourceLocationKind::Project),
        package: None,
        runtime: None,
        source_kind: SourceKind::Authored,
    }
}

impl SymbolOrigin {
    /// Whether this is the common case a caller may assume when `Symbol.origin` is
    /// absent: a project declaration, authored, with no package.
    fn is_common_default(&self) -> bool {
        self == &default_symbol_origin()
    }
}

/// One revision that touched a symbol. The history provider parses the declaration at each
/// first-parent revision that changed its file and classifies adjacent states; a revision
/// whose source cannot be parsed contributes no version.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolVersion {
    /// The revision that touched the symbol.
    pub revision: RevisionId,
    /// Where the declaration lived at that revision.
    pub path: ProjectPath,
    /// How the revision changed the symbol.
    pub kind: SymbolVersionKind,
    /// When the revision was committed, as RFC 3339 date-time carrying the recorded offset:
    /// the committer time, not the author time.
    #[schemars(length(max = 64))]
    pub timestamp: String,
    /// The revision's own first summary line, where the version control records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub summary: Option<String>,
    /// Who authored the revision, as the version control records it.
    pub author: CommitAuthor,
}

/// The author one revision records, as committed: no `.mailmap` rewrites the name or the
/// address.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAuthor {
    /// The author's name.
    pub name: String,
    /// The author's email address.
    pub email: String,
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
pub struct TypeBinding {
    /// The role this type plays for the symbol.
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
#[schemars(transform = schema::declare_type_expression_empty_defaults)]
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
    /// Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

#[cfg(test)]
mod tests {
    use crate::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};

    use super::{
        Digest, Duration, FileId, GLOBAL_WARNING_DETAIL_CHARS_MAX, GetSymbolParams,
        GlobalFailureClass, GlobalPageWarningCode, IDENTITY_PATH_CHARACTER,
        LANGUAGE_IDENTITY_PATTERN, Language, NodeId, PAGE_INDEX_DEFAULT, PAGE_LIMIT_MAX,
        PackageIdentity, REVISION_ID_BYTES_MAX, ReadWarning, RelationshipFacet, RevisionId,
        RevisionIdViolation, SOURCE_WARNINGS_MAX, SearchScope, SourceUnitId, Symbol, SymbolId,
    };
    use schemars::schema_for;
    use serde_json::json;
    use strum::VariantArray;

    /// Each relationship facet label matches the spelling serde emits.
    #[test]
    fn relationship_facet_labels_match_serde() {
        for facet in RelationshipFacet::VARIANTS {
            assert_eq!(
                serde_json::to_value(facet).expect("serializes"),
                json!(facet.as_ref())
            );
        }
    }

    /// The kinds the advertised schema pattern accepts are the kinds `ExactKind` accepts,
    /// over both sides of every rule the pattern states.
    #[test]
    fn exact_kind_schema_pattern_equals_the_kind_rule() {
        let schema = serde_json::to_value(schema_for!(super::ExactKind)).expect("kind schema");
        let validator = jsonschema::validator_for(&schema).expect("the kind schema compiles");
        let samples = [
            ("function", true),
            ("type_alias", true),
            ("rust.struct", true),
            ("enum-member", true),
            ("F9", true),
            ("", false),
            ("9struct", false),
            ("_private", false),
            (".hidden", false),
            ("two words", false),
            ("kind:dialect", false),
            ("naïve", false),
        ];
        for (kind, accepted) in samples {
            assert_eq!(
                validator.is_valid(&json!(kind)),
                accepted,
                "schema: {kind:?}"
            );
            let parsed = super::ExactKind::try_from(kind.to_owned());
            assert_eq!(parsed.is_ok(), accepted, "rule: {kind:?}");
            assert_eq!(
                super::ExactKind(kind.to_owned()).is_valid(),
                accepted,
                "held: {kind:?}"
            );
            if let Err(error) = parsed {
                assert_eq!(error.kind(), kind);
                assert!(error.to_string().contains("such as `function`"), "{error}");
            }
        }
    }

    /// A refused identity segment renders with the segment it read and the two
    /// forms it accepts, so an operator fixing `rift.toml` sees both.
    #[test]
    fn a_refused_language_identity_names_the_segment_and_the_accepted_forms() {
        let error =
            Language::from_identity_segment("Rust").expect_err("an uppercase word is refused");
        assert_eq!(error.segment(), "Rust");
        let rendered = error.to_string();
        assert!(rendered.contains("\"Rust\""), "{rendered}");
        assert!(rendered.contains("name:dialect"), "{rendered}");
        assert!(
            std::error::Error::source(&error).is_none(),
            "the refusal carries no source"
        );
    }

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

    /// A `global_page_warning` carries the global API's `detail` as it arrived, so its
    /// advertised `maxLength` is the bound the client accepts a detail under.
    #[test]
    fn global_page_warning_schema_detail_length_equals_the_global_bound() {
        let schema = serde_json::to_value(schema_for!(ReadWarning)).expect("warning schema");
        let arm = schema["oneOf"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|arm| arm["properties"]["code"]["const"] == "global_page_warning")
            .expect("the warning union holds a global_page_warning arm");
        assert_eq!(
            arm["properties"]["detail"]["maxLength"],
            json!(GLOBAL_WARNING_DETAIL_CHARS_MAX)
        );
    }

    /// The schema's `maximum` on `limit` and `accepted_limit`'s refusal both read
    /// `PAGE_LIMIT_MAX`; this pins the advertised maximum to that one constant.
    #[test]
    fn get_symbol_params_schema_limit_maximum_equals_the_enforced_constant() {
        let schema = serde_json::to_value(schema_for!(GetSymbolParams)).expect("schema");
        assert_eq!(
            schema["properties"]["limit"]["maximum"],
            json!(PAGE_LIMIT_MAX)
        );
    }

    /// The advertised schema states the same default the field's default function
    /// returns: an omitted `include` implies `["source"]`.
    #[test]
    fn get_symbol_params_schema_include_default_is_source_only() {
        let schema = serde_json::to_value(schema_for!(GetSymbolParams)).expect("schema");
        assert_eq!(
            schema["properties"]["include"]["default"],
            json!(["source"])
        );
    }

    /// `scope` takes serde's `default`, which reads the enum's own `Default`; this pins
    /// the advertised default to the `project` member that impl selects.
    #[test]
    fn get_symbol_params_schema_scope_default_is_local() {
        let schema = serde_json::to_value(schema_for!(GetSymbolParams)).expect("schema");
        assert_eq!(schema["properties"]["scope"]["default"], json!("local"));
        assert_eq!(
            serde_json::to_value(SearchScope::default()).expect("serialize"),
            json!("local")
        );
    }

    /// `include: ["body"]` names no `GetSymbolInclude` member; a request naming it is
    /// refused at deserialization, and the refusal names the accepted values.
    #[test]
    fn get_symbol_include_rejects_an_unknown_entry_and_names_the_accepted_values() {
        let error = serde_json::from_value::<GetSymbolParams>(
            json!({"name": "Beacon", "include": ["body"]}),
        )
        .expect_err("an unknown include entry must fail deserialization");
        let message = error.to_string();
        assert!(
            message.contains("source") && message.contains("history"),
            "{message}"
        );
    }

    #[test]
    fn revision_id_schema_states_the_enforced_length_bound() {
        let schema = serde_json::to_value(schema_for!(RevisionId)).expect("revision id schema");
        assert_eq!(schema["maxLength"], json!(REVISION_ID_BYTES_MAX));
        assert_eq!(schema["minLength"], json!(1));
    }

    #[test]
    fn revision_id_violation_classifies_what_the_schema_pattern_rejects() {
        let cases = [
            ("", Some(RevisionIdViolation::Empty)),
            (
                "a".repeat(REVISION_ID_BYTES_MAX + 1).leak() as &str,
                Some(RevisionIdViolation::TooLong),
            ),
            (
                "rev with space",
                Some(RevisionIdViolation::CharsetForbidden),
            ),
            ("HEAD@{1}", Some(RevisionIdViolation::CharsetForbidden)),
            ("~1", Some(RevisionIdViolation::AncestryInvalid)),
            ("^", Some(RevisionIdViolation::AncestryInvalid)),
            ("HEAD~1/src", Some(RevisionIdViolation::AncestryInvalid)),
            ("HEAD~x", Some(RevisionIdViolation::AncestryInvalid)),
            ("HEAD~1", None),
            ("HEAD~", None),
            ("HEAD^", None),
            ("main^2~3", None),
            ("main", None),
            ("feature/rev-reads", None),
            ("v0.0.6", None),
            ("dd0a482", None),
        ];
        let schema = serde_json::to_value(schema_for!(RevisionId)).expect("revision schema");
        let advertised = jsonschema::validator_for(&schema).expect("the schema compiles");
        for (spelling, expected) in cases {
            assert_eq!(
                RevisionId(spelling.to_owned()).violation(),
                expected,
                "spelling {spelling:?}"
            );
            assert_eq!(
                advertised.is_valid(&json!(spelling)),
                expected.is_none(),
                "the advertised schema and the classifier agree on {spelling:?}"
            );
        }
    }

    #[test]
    fn revision_id_violations_spell_their_serialized_labels() {
        for (violation, label) in [
            (RevisionIdViolation::Empty, "empty"),
            (RevisionIdViolation::TooLong, "too_long"),
            (RevisionIdViolation::CharsetForbidden, "charset_forbidden"),
            (RevisionIdViolation::AncestryInvalid, "ancestry_invalid"),
        ] {
            assert_eq!(violation.as_str(), label);
            assert_eq!(
                serde_json::to_value(violation).expect("serialize"),
                json!(label)
            );
        }
    }

    #[test]
    fn identity_segment_is_the_name_when_no_dialect_is_set() {
        let language = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        assert_eq!(language.identity_segment(), "rust");
    }

    #[test]
    fn identity_segment_joins_name_and_dialect_with_a_colon() {
        let language = Language {
            name: "typescript".to_owned(),
            dialect: Some("tsx".to_owned()),
        };
        assert_eq!(language.identity_segment(), "typescript:tsx");
    }

    #[test]
    fn language_identity_segment_parser_is_exact_inverse() {
        for segment in ["rust", "typescript:tsx", "objective-c"] {
            let language = Language::from_identity_segment(segment).expect("valid identity");
            assert_eq!(language.identity_segment(), segment);
        }
        for segment in ["", "Rust", "rust:", ":rs", "rust:macro:item", "rust lang"] {
            let error = Language::from_identity_segment(segment).expect_err("invalid identity");
            assert_eq!(error.segment(), segment);
        }
        let oversized = format!("a{}", "b".repeat(64));
        assert!(Language::from_identity_segment(&oversized).is_err());
    }

    /// `Language` serializes as the same string [`Language::identity_segment`] renders,
    /// on both a bare name and a `name:dialect` pair, and rejects the object shape the
    /// type used to carry.
    #[test]
    fn language_round_trips_through_its_identity_segment_string() {
        let rust = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        assert_eq!(
            serde_json::to_value(&rust).expect("serialize"),
            json!("rust")
        );
        assert_eq!(
            serde_json::from_value::<Language>(json!("rust")).expect("deserialize"),
            rust
        );
        let dialect = Language {
            name: "typescript".to_owned(),
            dialect: Some("tsx".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(&dialect).expect("serialize"),
            json!("typescript:tsx")
        );
        assert_eq!(
            serde_json::from_value::<Language>(json!("typescript:tsx")).expect("deserialize"),
            dialect
        );
        assert!(serde_json::from_value::<Language>(json!({"name": "rust"})).is_err());
    }

    /// The advertised schema pattern and length are the exact grammar
    /// [`Language::from_identity_segment`] enforces.
    #[test]
    fn language_schema_pattern_equals_the_identity_segment_grammar() {
        let schema = serde_json::to_value(schema_for!(Language)).expect("language schema");
        assert_eq!(schema["type"], json!("string"));
        assert_eq!(schema["pattern"], json!(LANGUAGE_IDENTITY_PATTERN));
        assert_eq!(
            schema["maxLength"],
            json!(super::LANGUAGE_IDENTITY_BYTES_MAX)
        );
        assert!(!super::is_language_word(
            &"a".repeat(super::LANGUAGE_WORD_BYTES_MAX + 1)
        ));
        assert!(super::is_language_word(
            &"a".repeat(super::LANGUAGE_WORD_BYTES_MAX)
        ));
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
    fn unestablished_symbol_round_trips_with_a_dependency_origin() {
        let value = json!({
            "language": "rust",
            "name": "Beacon",
            "kind": "struct",
            "facets": ["type"],
            "origin": {
                "location": "dependency",
                "package": { "manager": "cargo", "registry": "crates.io", "name": "beacon-core", "version": "0.1.0" },
                "source_kind": "authored"
            }
        });
        let symbol: Symbol =
            serde_json::from_value(value.clone()).expect("unestablished Symbol decodes");
        assert_eq!(
            symbol.id, None,
            "no accepted evidence established an identity"
        );
        assert_eq!(symbol.modifiers, Vec::<String>::new());
        assert!(!symbol.document_local);
        assert_eq!(
            serde_json::to_value(symbol).expect("unestablished Symbol encodes"),
            value,
            "every empty collection, document_local's false, and a non-default origin stay stable"
        );
    }

    /// `Symbol.origin` omits itself from the wire in the common case: a project
    /// declaration, authored, with no package. Any other case - a dependency, a
    /// package, a differing `source_kind` - keeps `origin` on the wire.
    #[test]
    fn symbol_origin_omits_itself_only_for_the_common_project_authored_case() {
        let common = json!({
            "language": "rust",
            "name": "Beacon",
            "kind": "struct",
            "facets": ["type"]
        });
        let symbol: Symbol =
            serde_json::from_value(common.clone()).expect("Symbol without origin decodes");
        assert_eq!(
            serde_json::to_value(&symbol).expect("serialize"),
            common,
            "an absent origin round-trips to the common default and stays absent"
        );
        assert_eq!(symbol.origin.source_kind, super::SourceKind::Authored);
        assert_eq!(
            symbol.origin.location,
            Some(super::SourceLocationKind::Project)
        );
    }

    /// The `ProjectPath` pattern states the rules `rift_core::ProjectPath` enforces with no
    /// lookahead, which neither the JSON Schema dialect nor the `regex` crate the global
    /// API client generator reads it with share: the root, no leading, trailing, or doubled
    /// separator, no `.` or `..` segment, no backslash or control character, and no `.rift`
    /// first segment.
    #[test]
    fn project_path_pattern_refuses_what_the_path_rules_refuse() {
        let schema =
            serde_json::to_value(schema_for!(super::ProjectPath)).expect("project path schema");
        let pattern = schema["pattern"].as_str().expect("an advertised pattern");
        assert!(!pattern.contains("(?!"), "no lookahead: {pattern}");
        let validator = jsonschema::validator_for(&json!({ "type": "string", "pattern": pattern }))
            .expect("the advertised pattern compiles");
        let accepted = [
            "",
            "src/lib.rs",
            ".github/workflows/ci.yml",
            ".r",
            ".ri",
            ".rif",
            ".rifts",
            ".rift.toml",
            "src/.rift",
            "...",
            "..a/b",
            "a./.b",
        ];
        for path in accepted {
            assert!(
                validator.is_valid(&json!(path)),
                "{pattern} must accept {path:?}"
            );
        }
        let refused = [
            ".rift",
            ".rift/index",
            "/src",
            "src/",
            "src//lib.rs",
            ".",
            "..",
            "./src",
            "src/./lib.rs",
            "src/..",
            "src\\lib.rs",
            "src/\u{1}.rs",
            "src/\u{7f}.rs",
        ];
        for path in refused {
            assert!(
                !validator.is_valid(&json!(path)),
                "{pattern} must refuse {path:?}"
            );
        }
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

    /// Every served identity spells its path with the one shared character class, and each
    /// accepts an `@` inside that path. `rift_core::encode_path` keeps `@` literal because RFC
    /// 3986 lists it in the path set, so a pattern that left it out refused an identity the
    /// server had itself minted: every npm scoped package directory reached it.
    #[test]
    fn every_identity_pattern_accepts_the_path_bytes_the_encoder_keeps() {
        let cases = [
            (
                serde_json::to_value(schema_for!(FileId)).expect("file schema"),
                "rift://file/packages/@scope/name/package.json",
            ),
            (
                serde_json::to_value(schema_for!(NodeId)).expect("node schema"),
                "rift://node/json/packages/@scope/name/package.json@920-1004#b7fb41b1",
            ),
            (
                serde_json::to_value(schema_for!(SourceUnitId)).expect("source unit schema"),
                "rift://source/project/packages/@scope/name/package.json",
            ),
            (
                serde_json::to_value(schema_for!(SymbolId)).expect("symbol schema"),
                "rift://symbol/json/packages/@scope/name/package.json/name",
            ),
        ];
        for (schema, identity) in cases {
            let pattern = schema["pattern"].as_str().expect("an advertised pattern");
            assert!(
                pattern.contains(IDENTITY_PATH_CHARACTER),
                "an identity pattern spells its path with the shared class: {pattern}"
            );
            let validator =
                jsonschema::validator_for(&json!({ "type": "string", "pattern": pattern }))
                    .expect("the advertised pattern compiles");
            assert!(
                validator.is_valid(&json!(identity)),
                "{pattern} must accept {identity}"
            );
        }
    }

    /// The tier warnings carry the evidence a caller weighs, so each one is pinned to the
    /// `code` its consumers match on and to the members that carry that evidence.
    #[test]
    fn every_tier_warning_round_trips_under_its_code_tag() {
        let cases = [
            (
                ReadWarning::VectorIndexPreparing {
                    prepared: 1_200,
                    total: 4_800,
                    ready_in: Duration::from_millis(45_000),
                    detail: "Vector search is being prepared".to_owned(),
                },
                json!({
                    "code": "vector_index_preparing",
                    "prepared": 1_200,
                    "total": 4_800,
                    "ready_in": "45s",
                    "detail": "Vector search is being prepared",
                }),
            ),
            (
                ReadWarning::HistoryStoreFilling {
                    analyzed: 40,
                    total: 100,
                    detail: "the history store has analyzed 40 of 100 commits".to_owned(),
                },
                json!({
                    "code": "history_store_filling",
                    "analyzed": 40,
                    "total": 100,
                    "detail": "the history store has analyzed 40 of 100 commits",
                }),
            ),
            (
                ReadWarning::VectorRankingUnavailable {
                    detail: "the model weights could not be acquired".to_owned(),
                },
                json!({
                    "code": "vector_ranking_unavailable",
                    "detail": "the model weights could not be acquired",
                }),
            ),
            (
                ReadWarning::LexicalRankingUnavailable {
                    detail: "the full-text index did not open".to_owned(),
                },
                json!({
                    "code": "lexical_ranking_unavailable",
                    "detail": "the full-text index did not open",
                }),
            ),
            (
                ReadWarning::SourceUnavailable {
                    unit: Some(FileId("rift://file/src%2Finvalid.rs".to_owned())),
                    detail: "src/invalid.rs is not UTF-8 and is absent from the index".to_owned(),
                },
                json!({
                    "code": "source_unavailable",
                    "unit": "rift://file/src%2Finvalid.rs",
                    "detail": "src/invalid.rs is not UTF-8 and is absent from the index",
                }),
            ),
            (
                ReadWarning::LockfileExcluded {
                    files: vec![FileId("rift://file/Cargo.lock".to_owned())],
                    detail: "Cargo.lock is left out of search".to_owned(),
                },
                json!({
                    "code": "lockfile_excluded",
                    "files": ["rift://file/Cargo.lock"],
                    "detail": "Cargo.lock is left out of search",
                }),
            ),
            (
                ReadWarning::SymbolDisagreement {
                    symbol: SymbolId("rift://symbol/rust/src/lib.rs/Beacon".to_owned()),
                    providers: vec!["history".to_owned(), "syntax".to_owned()],
                    detail: "normalization selected one presentation for this symbol; \
                             history, syntax disagree on at least one field"
                        .to_owned(),
                },
                json!({
                    "code": "symbol_disagreement",
                    "symbol": "rift://symbol/rust/src/lib.rs/Beacon",
                    "providers": ["history", "syntax"],
                    "detail": "normalization selected one presentation for this symbol; \
                               history, syntax disagree on at least one field",
                }),
            ),
        ];
        assert_round_trips(cases);
    }

    /// The package warnings a `global` or `all` answer rides with, pinned the same way.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table pins every package warning wire shape"
    )]
    fn every_package_warning_round_trips_under_its_code_tag() {
        let cases = [
            (
                ReadWarning::GlobalAccessDisabled,
                json!({ "code": "global_access_disabled" }),
            ),
            (
                ReadWarning::GlobalApiUnavailable {
                    failure_class: GlobalFailureClass::RetryExhausted,
                },
                json!({
                    "code": "global_api_unavailable",
                    "failure_class": "retry_exhausted",
                }),
            ),
            (
                ReadWarning::GlobalPublicationIncompatible {
                    failure_class: GlobalFailureClass::PublicationFormat,
                },
                json!({
                    "code": "global_publication_incompatible",
                    "failure_class": "publication_format",
                }),
            ),
            (
                ReadWarning::GlobalResponseInvalid {
                    failure_class: GlobalFailureClass::ResponseTruncated,
                },
                json!({
                    "code": "global_response_invalid",
                    "failure_class": "response_truncated",
                }),
            ),
            (
                ReadWarning::GlobalPageWarning {
                    warning_code: GlobalPageWarningCode::SourceTruncated,
                    detail: Some("source exceeded the active bound".to_owned()),
                },
                json!({
                    "code": "global_page_warning",
                    "warning_code": "source_truncated",
                    "detail": "source exceeded the active bound",
                }),
            ),
            (
                ReadWarning::PackageAbsent {
                    package: PackageIdentity {
                        manager: "cargo".to_owned(),
                        registry: "crates.io".to_owned(),
                        name: "missing-helper".to_owned(),
                        version: "0.1.0".to_owned(),
                    },
                },
                json!({
                    "code": "package_absent",
                    "package": {
                        "manager": "cargo",
                        "registry": "crates.io",
                        "name": "missing-helper",
                        "version": "0.1.0",
                    },
                }),
            ),
            (
                ReadWarning::PackageRequirementAbsent {
                    entry: PackageContextEntry::new(
                        "cargo",
                        "missing-helper",
                        PackageSelector::Requirement("^0.1".to_owned()),
                        PackageAvailability::Canonical,
                    ),
                },
                json!({
                    "code": "package_requirement_absent",
                    "entry": {
                        "manager": "cargo",
                        "name": "missing-helper",
                        "requirement": "^0.1",
                        "availability": "canonical",
                    },
                }),
            ),
            (
                ReadWarning::PackageSubstituted {
                    entry: PackageContextEntry::new(
                        "npm",
                        "typescript",
                        PackageSelector::Requirement("~5.7.2".to_owned()),
                        PackageAvailability::Canonical,
                    ),
                    package: PackageIdentity {
                        manager: "npm".to_owned(),
                        registry: "registry.npmjs.org".to_owned(),
                        name: "typescript".to_owned(),
                        version: "5.9.3".to_owned(),
                    },
                },
                json!({
                    "code": "package_substituted",
                    "entry": {
                        "manager": "npm",
                        "name": "typescript",
                        "requirement": "~5.7.2",
                        "availability": "canonical",
                    },
                    "package": {
                        "manager": "npm",
                        "registry": "registry.npmjs.org",
                        "name": "typescript",
                        "version": "5.9.3",
                    },
                }),
            ),
            (
                ReadWarning::PackageUnavailable {
                    entry: PackageContextEntry::new(
                        "cargo",
                        "helper",
                        PackageSelector::Requirement("^0.1".to_owned()),
                        PackageAvailability::Git,
                    ),
                    reason: crate::dependencies::GIT_UNAVAILABLE_REASON.to_owned(),
                },
                json!({
                    "code": "package_unavailable",
                    "entry": {
                        "manager": "cargo",
                        "name": "helper",
                        "requirement": "^0.1",
                        "availability": "git",
                    },
                    "reason": crate::dependencies::GIT_UNAVAILABLE_REASON,
                }),
            ),
            (
                ReadWarning::PackageContextDegraded {
                    resolver: "cargo".to_owned(),
                    reason: "cargo is not on PATH; the lockfile alone was read".to_owned(),
                },
                json!({
                    "code": "package_context_degraded",
                    "resolver": "cargo",
                    "reason": "cargo is not on PATH; the lockfile alone was read",
                }),
            ),
        ];
        assert_round_trips(cases);
    }

    /// Each warning serializes to the wire value beside it, and reads back equal.
    fn assert_round_trips<const COUNT: usize>(cases: [(ReadWarning, serde_json::Value); COUNT]) {
        for (warning, wire) in cases {
            assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
            let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
            assert_eq!(parsed, warning);
        }
    }

    #[test]
    fn the_query_narrowing_warning_round_trips_under_its_code_tag() {
        let warning = ReadWarning::QueryNarrowed { terms_max: 32 };
        let wire = json!({ "code": "query_narrowed", "terms_max": 32 });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }

    #[test]
    fn the_lexical_truncation_warning_round_trips_under_its_code_tag() {
        let warning = ReadWarning::LexicalRankingTruncated { matches_max: 1_000 };
        let wire = json!({ "code": "lexical_ranking_truncated", "matches_max": 1_000 });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }

    /// An empty facet list is omitted, so absence is the signal a caller reads.
    #[test]
    fn the_relationship_coverage_warning_omits_an_empty_facet_list() {
        let cases = [
            (
                ReadWarning::RelationshipCoverageMissing {
                    facets: vec![RelationshipFacet::Implements],
                    detail: "no lane populates the requested facet implements".to_owned(),
                },
                json!({
                    "code": "relationship_coverage_missing",
                    "facets": ["implements"],
                    "detail": "no lane populates the requested facet implements",
                }),
            ),
            (
                ReadWarning::RelationshipCoverageMissing {
                    facets: Vec::new(),
                    detail: "no lane populates the requested coverage".to_owned(),
                },
                json!({
                    "code": "relationship_coverage_missing",
                    "detail": "no lane populates the requested coverage",
                }),
            ),
        ];
        for (warning, wire) in cases {
            assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
            let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
            assert_eq!(parsed, warning);
        }
    }

    #[test]
    fn the_pattern_matches_truncation_warning_round_trips_and_bounds_its_files() {
        let warning = ReadWarning::PatternMatchesTruncated {
            matches_per_file: 1_000,
            files: vec![FileId("rift://file/src%2Fmany.rs".to_owned())],
        };
        let wire = json!({
            "code": "pattern_matches_truncated",
            "matches_per_file": 1_000,
            "files": ["rift://file/src%2Fmany.rs"],
        });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
        let schema = serde_json::to_value(schema_for!(ReadWarning)).expect("warning schema");
        let arm = schema["oneOf"]
            .as_array()
            .and_then(|arms| {
                arms.iter().find(|arm| {
                    arm["properties"]["code"]["const"] == json!("pattern_matches_truncated")
                })
            })
            .expect("the schema advertises the match-bound warning");
        assert_eq!(
            arm["properties"]["files"]["maxItems"],
            json!(SOURCE_WARNINGS_MAX),
            "the advertised bound is the one the server cuts at"
        );
    }

    #[test]
    fn the_pattern_index_preparation_warning_round_trips_under_its_code_tag() {
        let warning = ReadWarning::PatternIndexPreparing {
            prepared: 12_000,
            total: 17_500,
            detail: "12000 of 17500 rows of file text are in the trigram index".to_owned(),
        };
        let wire = json!({
            "code": "pattern_index_preparing",
            "prepared": 12_000,
            "total": 17_500,
            "detail": "12000 of 17500 rows of file text are in the trigram index",
        });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }

    #[test]
    fn the_local_index_preparation_warning_omits_unknown_progress() {
        let warning = ReadWarning::LocalIndexPreparing {
            prepared: 0,
            total: None,
            ready_in: None,
            detail: "selected local files are still being prepared".to_owned(),
        };
        let wire = json!({
            "code": "local_index_preparing",
            "prepared": 0,
            "detail": "selected local files are still being prepared",
        });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }

    #[test]
    fn the_large_file_warnings_round_trip_under_their_code_tags() {
        let cases = [
            (
                ReadWarning::LargeFileSkipped {
                    skipped: 2,
                    detail: "2 selected files are past max_chunk".to_owned(),
                },
                json!({
                    "code": "large_file_skipped",
                    "skipped": 2,
                    "detail": "2 selected files are past max_chunk",
                }),
            ),
            (
                ReadWarning::LargeFileUnparsed {
                    files: vec![FileId("rift://file/big.js".to_owned())],
                    detail: "big.js is past max_file".to_owned(),
                },
                json!({
                    "code": "large_file_unparsed",
                    "files": ["rift://file/big.js"],
                    "detail": "big.js is past max_file",
                }),
            ),
        ];
        assert_round_trips(cases);
    }

    #[test]
    fn the_results_truncation_warning_round_trips_under_its_code_tag() {
        let warning = ReadWarning::ResultsTruncated { results_max: 1_000 };
        let wire = json!({ "code": "results_truncated", "results_max": 1_000 });
        assert_eq!(serde_json::to_value(&warning).expect("serialize"), wire);
        let parsed: ReadWarning = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(parsed, warning);
    }

    #[test]
    fn the_read_warning_schema_advertises_every_tier_warning() {
        let schema = serde_json::to_value(schema_for!(ReadWarning)).expect("warning schema");
        let arms = schema["oneOf"].as_array().cloned().unwrap_or_default();
        let codes: Vec<serde_json::Value> = arms
            .iter()
            .map(|arm| arm["properties"]["code"].clone())
            .collect();
        for code in [
            "stale_index",
            "vector_index_preparing",
            "vector_ranking_unavailable",
            "lexical_ranking_unavailable",
            "query_narrowed",
            "lexical_ranking_truncated",
            "results_truncated",
            "pattern_matches_truncated",
            "pattern_index_preparing",
            "source_unavailable",
            "large_file_skipped",
            "large_file_unparsed",
            "symbol_disagreement",
            "global_access_disabled",
            "global_api_unavailable",
            "global_publication_incompatible",
            "global_response_invalid",
            "global_page_warning",
            "package_absent",
            "package_requirement_absent",
            "package_unavailable",
            "package_context_degraded",
            "relationship_coverage_missing",
            "history_store_filling",
        ] {
            assert!(
                codes.contains(&json!({ "const": code, "type": "string" })),
                "the schema must advertise {code}: {codes:?}"
            );
        }
        for removed in ["global_index_unavailable", "package_skipped"] {
            assert!(
                !codes.contains(&json!({ "const": removed, "type": "string" })),
                "the global route emits no {removed}: {codes:?}"
            );
        }
    }

    #[test]
    fn global_warning_schema_bounds_failure_class_and_carries_no_counts() {
        let warning_schema = serde_json::to_value(schema_for!(ReadWarning)).expect("schema");
        let arms = warning_schema["oneOf"]
            .as_array()
            .expect("tagged warning schema");
        let arm = arms
            .iter()
            .find(|arm| {
                arm["properties"]["code"]
                    == json!({
                        "const": "global_api_unavailable",
                        "type": "string",
                    })
            })
            .expect("global API warning schema");
        // A set: the member order follows whether a workspace build enables serde_json's
        // `preserve_order`, which the schema never promises.
        let properties: std::collections::BTreeSet<&str> = arm["properties"]
            .as_object()
            .expect("an arm names its properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            properties,
            std::collections::BTreeSet::from(["code", "failure_class"])
        );

        let failure_schema =
            serde_json::to_value(schema_for!(GlobalFailureClass)).expect("failure class schema");
        assert_eq!(
            failure_schema["oneOf"]
                .as_array()
                .expect("failure class schema")
                .iter()
                .map(|arm| arm["const"].clone())
                .collect::<Vec<_>>(),
            vec![
                json!("connection"),
                json!("timeout"),
                json!("retry_exhausted"),
                json!("authentication"),
                json!("credential_configuration"),
                json!("non_success_response"),
                json!("invalid_response"),
                json!("response_truncated"),
                json!("publication_format"),
                json!("corpus_revision"),
                json!("required_field_set"),
            ]
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
