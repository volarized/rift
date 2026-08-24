//! JSON syntax facts from the pinned tree-sitter-json grammar.
//!
//! Each object member - the grammar's `pair` node - declares one `member`
//! symbol. Values declare nothing on their own: a document holding a bare
//! scalar or array emits no symbols, and its nodes still serve.
//!
//! Decisions this module fixes:
//! - A symbol's `name` is the key's bytes between its quotes, escapes kept
//!   as authored (`a\"b` stays `a\"b`). A pair whose key spells no
//!   characters (`""`) or is missing a quote names nothing and emits no
//!   symbol.
//! - Qualified names join nested member keys with ` > ` (`server > port`),
//!   the markdown spelling and for the markdown reason: a key is an
//!   arbitrary string, so the language reserves no separator - `.` and `/`
//!   both occur in real keys (`@types/node` in a package manifest) - and
//!   every byte of ` > ` escapes in the minted address, so the spelling
//!   cannot be mistaken for path structure.
//! - Array elements have no keys and mint no symbols - no index is
//!   invented as a name. Pairs inside an object element still emit,
//!   qualified through the holding member's key path: both `host` members
//!   of `{"servers": [{"host": "a"}, {"host": "b"}]}` file as
//!   `servers > host`, sharing one qualified name with distinct spans, the
//!   same policy duplicate keys get.
//! - Nothing attaches in front of a pair, so `range` equals `item_range`.
//!   `body_range` is the value node's span, absent when the tree is
//!   missing the value.
//! - Members carry no visibility and no portable symbol facet:
//!   `visibility` stays `None` and `facets` stays empty.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet};
use tree_sitter::{Node, Parser};

use crate::document::SyntaxDocument;
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Grammar spelling of a `pair`, one object member.
const PAIR_KIND: &str = "pair";
/// Grammar spelling of a `string`, the only key shape the grammar accepts.
const STRING_KIND: &str = "string";
/// The quote delimiting a key string; the name is the bytes between one
/// pair of them.
const KEY_QUOTE: char = '"';
/// Grammar field holding a pair's key.
const KEY_FIELD: &str = "key";
/// Grammar field holding a pair's value.
const VALUE_FIELD: &str = "value";

/// The one JSON kind word behind the wire kind `json.member`.
const MEMBER_KIND_WORD: &str = "member";

/// The separator JSON qualified names join nested member keys with.
const MEMBER_QUALIFICATION_SEPARATOR: &str = " > ";

/// Numeric grammar ids for every kind and field this module reads, resolved
/// once so each walk decision compares integers.
#[derive(Debug)]
struct JsonKinds {
    pair: u16,
    string: u16,
    key: NonZeroU16,
    value: NonZeroU16,
}

impl JsonKinds {
    /// Resolves the pinned grammar's member vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            pair: kind_id(language, PAIR_KIND),
            string: kind_id(language, STRING_KIND),
            key: field_id(language, KEY_FIELD),
            value: field_id(language, VALUE_FIELD),
        }
    }
}

/// Resolves one node kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned JSON grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one grammar field id, proving the pinned grammar defines it.
fn field_id(language: &tree_sitter::Language, field: &str) -> NonZeroU16 {
    language.field_id_for_name(field).unwrap_or_else(|| {
        panic!(
            "pinned JSON grammar must define field used by symbol \
             extraction: field={field}"
        )
    })
}

/// The pinned grammar's decisions for the shared bounded walk.
#[derive(Debug)]
struct JsonRules {
    kinds: &'static JsonKinds,
}

impl JsonRules {
    /// The pair's key spelling: the bytes between the key string's quotes,
    /// escapes kept as authored. `None` for a pair whose key is missing,
    /// not a string, missing a quote, or spelling no characters.
    fn member_name(&self, pair: Node<'_>, text: &str) -> Option<String> {
        let key = pair.child_by_field_id(self.kinds.key.get())?;
        if key.kind_id() != self.kinds.string {
            return None;
        }
        let spelling = text.get(key.byte_range())?;
        let name = spelling.strip_prefix(KEY_QUOTE)?.strip_suffix(KEY_QUOTE)?;
        (!name.is_empty()).then(|| name.to_owned())
    }
}

impl GrammarRules for JsonRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        if node.kind_id() != self.kinds.pair {
            return Ok(None);
        }
        let Some(name) = self.member_name(node, text) else {
            return Ok(None);
        };
        let body_range = match node.child_by_field_id(self.kinds.value.get()) {
            Some(value) => Some(extract::byte_range(value)?),
            None => None,
        };
        Ok(Some(Declaration {
            name,
            kind: MEMBER_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range,
        }))
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        if node.kind_id() != self.kinds.pair {
            return None;
        }
        self.member_name(node, text)
    }

    /// A declaration starts at its own node: nothing attaches in front.
    fn declaration_start(&self, node: Node<'_>, _text: &str) -> usize {
        node.start_byte()
    }

    fn qualification_separator(&self) -> &'static str {
        MEMBER_QUALIFICATION_SEPARATOR
    }
}

/// Bounded Tree-sitter JSON fact provider.
#[derive(Debug, Clone)]
pub struct JsonSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl JsonSyntaxProvider {
    /// File extensions this provider parses, without their leading dot.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["json"];

    /// Default maximum bytes this provider accepts from one JSON source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "json".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The JSON provider's declared default bounds, proven positive at compile
/// time.
const JSON_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    JsonSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for JsonSyntaxProvider {
    fn default() -> Self {
        Self::new(JSON_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for JsonSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn extensions(&self) -> &'static [&'static str] {
        Self::SOURCE_EXTENSIONS
    }

    fn source_bytes_max(&self) -> usize {
        self.limits.source_bytes_max()
    }

    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
        if source.text.len() > self.limits.source_bytes_max() {
            return Err(Error::new(SyntaxFault::SourceTooLarge {
                path: Some(source.path.clone()),
                source_bytes: source.text.len(),
                source_bytes_max: self.limits.source_bytes_max(),
            }));
        }
        let grammar = json_grammar();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|_| incompatible_grammar(&grammar))?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let rules = JsonRules {
            kinds: json_kinds(),
        };
        let (nodes, symbols) = extract::extract(tree.root_node(), source, self.limits, &rules)?;
        Ok(SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            nodes,
            symbols,
            tree.root_node().has_error(),
        ))
    }

    /// Portable structural facets for one JSON grammar node kind. The
    /// structural kinds - `document`, `object`, `array` - carry none.
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            // The pair declares its key's name.
            PAIR_KIND => vec![NodeFacet::Declaration],
            // A value written out directly.
            STRING_KIND | "number" | "true" | "false" | "null" => vec![NodeFacet::Literal],
            // The grammar accepts comments so an agent-read JSONC file still
            // parses; strict JSON never carries one.
            "comment" => vec![NodeFacet::Comment],
            _ => Vec::new(),
        }
    }
}

fn json_grammar() -> tree_sitter::Language {
    tree_sitter_json::LANGUAGE.into()
}

/// Returns the process-wide resolved JSON kind table, computing it once.
fn json_kinds() -> &'static JsonKinds {
    static KINDS: OnceLock<JsonKinds> = OnceLock::new();
    KINDS.get_or_init(|| JsonKinds::resolve(&json_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("config/settings.json").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        JsonSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("JSON fixture must parse")
    }

    /// Resolution asserts every kind and field id is non-zero, so resolving
    /// the pinned grammar's table is the proof the vocabulary exists.
    #[test]
    fn test_kind_table_resolves_on_the_pinned_grammar() {
        let kinds = JsonKinds::resolve(&json_grammar());
        assert_ne!(kinds.pair, 0);
        assert_ne!(kinds.string, 0);
    }

    #[test]
    #[should_panic(expected = "must define node kind used by symbol extraction: \
                               kind=no_such_kind")]
    fn test_kind_resolution_refuses_a_kind_the_grammar_lacks() {
        let _ = kind_id(&json_grammar(), "no_such_kind");
    }

    #[test]
    #[should_panic(expected = "must define field used by symbol extraction: \
                               field=no_such_field")]
    fn test_field_resolution_refuses_a_field_the_grammar_lacks() {
        let _ = field_id(&json_grammar(), "no_such_field");
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = JsonSyntaxProvider::default();
        assert_eq!(provider.language().name, "json");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["json"]);
        assert_eq!(
            provider.source_bytes_max(),
            JsonSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    /// Nested members qualify through their key path with byte-exact spans:
    /// the whole pair for `range` and `item_range`, the value node for
    /// `body_range`.
    #[test]
    fn test_nested_members_qualify_through_the_key_path_with_exact_spans() {
        let text =
            "{\"server\": {\"port\": 8080, \"tls\": {\"cert\": \"a.pem\"}}, \"name\": \"rift\"}";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.container.as_deref(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.map(|body| (body.start, body.end)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("server", None, (1, 51), Some((11, 51))),
                ("server > port", Some("server"), (12, 24), Some((20, 24))),
                ("server > tls", Some("server"), (26, 50), Some((33, 50))),
                (
                    "server > tls > cert",
                    Some("server > tls"),
                    (34, 49),
                    Some((42, 49)),
                ),
                ("name", None, (53, 67), Some((61, 67))),
            ]
        );
        assert_eq!(
            &text[1..51],
            "\"server\": {\"port\": 8080, \"tls\": {\"cert\": \"a.pem\"}}"
        );
        assert_eq!(&text[20..24], "8080");
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.kind == "member"
                    && symbol.range == symbol.item_range
                    && symbol.facets.is_empty()
                    && symbol.visibility.is_none()),
            "every member files under the one JSON kind, spans its own pair, \
             and carries no facets and no visibility"
        );
        assert!(!document.has_errors());
    }

    /// Array elements mint no symbols - no index is invented as a name -
    /// while pairs inside an object element still emit, qualified through
    /// the holding member's key path.
    #[test]
    fn test_object_elements_in_an_array_emit_their_pairs_without_indices() {
        let text = "{\"servers\": [{\"host\": \"a\"}, {\"host\": \"b\"}, 3]}";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.container.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("servers", None),
                ("servers > host", Some("servers")),
                ("servers > host", Some("servers")),
            ]
        );
        assert_ne!(
            document.symbols()[1].range,
            document.symbols()[2].range,
            "the two elements' members keep their own spans"
        );
    }

    /// A document holding a bare array or scalar emits no symbols; its
    /// nodes still serve.
    #[test]
    fn test_bare_array_and_scalar_documents_emit_no_symbols() {
        for text in ["[1, 2, 3]", "42", "\"prose\"", "true", "null"] {
            let document = analyze(text);
            assert!(
                document.symbols().is_empty(),
                "a keyless document must emit no symbols: {text}"
            );
            assert!(!document.has_errors());
            assert!(!document.nodes().is_empty());
        }
    }

    /// Duplicate keys both emit under one shared qualified name with
    /// distinct spans, the same policy every provider applies.
    #[test]
    fn test_duplicate_keys_share_one_qualified_name_with_distinct_spans() {
        let document = analyze("{\"port\": 1, \"port\": 2}");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["port", "port"]);
        assert_ne!(
            document.symbols()[0].range,
            document.symbols()[1].range,
            "the two members keep their own spans"
        );
    }

    /// The name is the key's source spelling between the quotes: escapes
    /// stay as authored, ` > `, `.`, and `/` stay literal, and an empty key
    /// names nothing.
    #[test]
    fn test_member_names_keep_the_authored_key_spelling() {
        let text = "{\"a\\\"b\": 1, \"x > y\": 2, \"@types/node\": 3, \"a.b\": 4, \"\": 5}";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a\\\"b", "x > y", "@types/node", "a.b"]);
        assert!(!document.has_errors());
    }

    /// The grammar marks a pair missing its value; the pair still emits
    /// from its key, with no body.
    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("{\"broken\": }");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
        assert_eq!(document.symbols()[0].name, "broken");
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
        let bounded = |limits: SyntaxLimits, text: &str| {
            JsonSyntaxProvider::new(limits).analyze(SyntaxSource {
                path: &path(),
                text,
            })
        };
        let source_error = bounded(
            SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            "{\"a\": 1}",
        )
        .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = bounded(
            SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            "{\"a\": 1}",
        )
        .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error = bounded(
            SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            "{\"a\": {\"b\": 1}}",
        )
        .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    /// Deep nesting stays well inside the default depth budget.
    #[test]
    fn test_deep_nesting_fits_the_default_depth_budget() {
        let mut text = String::new();
        for _ in 0..64 {
            text.push_str("{\"level\": ");
        }
        text.push('1');
        for _ in 0..64 {
            text.push('}');
        }
        let document = analyze(&text);
        assert_eq!(document.symbols().len(), 64);
        assert!(!document.has_errors());
    }

    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// The pair carries the `Declaration` facet, scalar values read as
    /// `Literal`, a comment as `Comment`, and structure carries none.
    #[test]
    fn test_node_facets_classify_the_interpreted_kinds() {
        let provider = JsonSyntaxProvider::default();
        assert_eq!(provider.node_facets(PAIR_KIND), [NodeFacet::Declaration]);
        for kind in [STRING_KIND, "number", "true", "false", "null"] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Literal],
                "kind {kind} must classify as a literal value"
            );
        }
        assert_eq!(provider.node_facets("comment"), [NodeFacet::Comment]);
        for kind in ["document", "object", "array", "string_content"] {
            assert_eq!(
                provider.node_facets(kind),
                [],
                "structural kind {kind} carries no portable facet"
            );
        }
    }
}
