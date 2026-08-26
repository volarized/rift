//! TOML syntax facts from the pinned tree-sitter-toml grammar.
//!
//! A `pair` - one `key = value` line - declares one `member` symbol. A
//! `table` (`[server]`) or `table_array_element` (`[[hooks]]`) declares one
//! `table` symbol from its header key, so both are addressable and
//! `replace_symbol` can rewrite a whole table. A pair nested inside a table,
//! an inline table, or an array of inline tables still declares, qualified
//! through whichever pair or table holds it - the same rule an array of
//! objects follows in JSON.
//!
//! Decisions this module fixes:
//! - A `bare_key`'s name is its bytes. A `quoted_key`'s name is the bytes
//!   between its quotes, escapes kept as authored (`a\"b` stays `a\"b`); the
//!   quote can be `"` or `'`, and either delimits by a single byte. A
//!   `dotted_key` (`a.b.c`) joins its segments with the qualification
//!   separator, so `a.b.c = 1` names `a > b > c` before any container
//!   qualifies it further - TOML's own `.` cannot serve as that separator,
//!   because a quoted key may contain one. A key whose only segment is an
//!   empty quoted string (`'' = 1`) names nothing and emits no symbol; a
//!   dotted key with an empty segment still emits, because the joined
//!   spelling carries the other segments.
//! - Qualified names join with ` > ` (`server > port`), the JSON and YAML
//!   spelling.
//! - Every `[[hooks]]` element repeats its header key, so both the table
//!   symbol and every identically-named pair beneath each element collide on
//!   qualified name. `SyntaxDocument` already appends the `~N` suffix
//!   `SymbolId` reserves, so this module does nothing further for it.
//! - **This grammar declares no fields anywhere** - `Language::field_count`
//!   is `0` on the pinned grammar, not only for `pair`, `table`, and
//!   `table_array_element`. A pair's key is its first named child and its
//!   value its second; both are found by kind, and there is no `field_id`
//!   call to make.
//! - A `#` comment sharing a pair's source line (`key = 1 # note`) parses as
//!   a trailing named child of that `pair`, after the value - tree-sitter's
//!   `comment` extra rule can land inside a rule it borders, not only
//!   beside it. Locating a pair's key and value by kind, filtering out a
//!   `comment` child, reads past this without special-casing the trailing
//!   position; a leading comment on its own line stays a sibling of the
//!   pair or table it precedes, outside both, and so stays with the file.
//! - Nothing else attaches in front of a pair, table, or table array
//!   element, so `range` equals `item_range`.
//! - A table or table array element has no wrapping node for the pairs it
//!   holds, so its `body_range` is always absent; a pair's `body_range` is
//!   its value node's span.
//! - Members carry no visibility and no portable symbol facet: `visibility`
//!   stays `None` and `facets` stays empty.

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

/// Grammar spelling of a `pair`, one `key = value` line.
const PAIR_KIND: &str = "pair";
/// Grammar spelling of a `table` (`[server]`).
const TABLE_KIND: &str = "table";
/// Grammar spelling of a `table_array_element` (`[[hooks]]`).
const TABLE_ARRAY_ELEMENT_KIND: &str = "table_array_element";
/// Grammar spelling of a `bare_key`, an unquoted key segment.
const BARE_KEY_KIND: &str = "bare_key";
/// Grammar spelling of a `quoted_key`, a `"..."` or `'...'` key segment.
const QUOTED_KEY_KIND: &str = "quoted_key";
/// Grammar spelling of a `dotted_key`, a `.`-joined chain of key segments.
const DOTTED_KEY_KIND: &str = "dotted_key";
/// Grammar spelling of a `comment`.
const COMMENT_KIND: &str = "comment";
/// Grammar spelling of a `string` value.
const STRING_KIND: &str = "string";
/// Grammar spelling of an `integer` value.
const INTEGER_KIND: &str = "integer";
/// Grammar spelling of a `float` value.
const FLOAT_KIND: &str = "float";
/// Grammar spelling of a `boolean` value.
const BOOLEAN_KIND: &str = "boolean";
/// Grammar spelling of a bare RFC 3339 date, `local_date`.
const LOCAL_DATE_KIND: &str = "local_date";
/// Grammar spelling of a bare RFC 3339 time, `local_time`.
const LOCAL_TIME_KIND: &str = "local_time";
/// Grammar spelling of an offset-less RFC 3339 timestamp, `local_date_time`.
const LOCAL_DATE_TIME_KIND: &str = "local_date_time";
/// Grammar spelling of an offset RFC 3339 timestamp, `offset_date_time`.
const OFFSET_DATE_TIME_KIND: &str = "offset_date_time";

/// The one TOML kind word behind the wire kind `toml.member`.
const MEMBER_KIND_WORD: &str = "member";
/// The one TOML kind word behind the wire kind `toml.table`.
const TABLE_KIND_WORD: &str = "table";

/// The separator TOML qualified names join key segments and containers
/// with, and the same separator a dotted key's own segments join with.
const QUALIFICATION_SEPARATOR: &str = " > ";

/// The single byte a quoted key's delimiter occupies on each side, whether
/// it is `"` or `'`.
const QUOTE_BYTE_WIDTH: usize = 1;

/// Numeric grammar ids for every kind the extraction walk compares, resolved
/// once so each decision compares integers. The pinned grammar declares no
/// fields, so this table holds no field ids. `node_facets` classifies the
/// value-literal kinds (`string`, `integer`, and the rest) by their kind
/// name directly, so this table holds none of those - the same split the
/// JSON provider draws between `pair`/`string`, which extraction consults,
/// and `"number"`/`"true"`/`"false"`/`"null"`, which `node_facets` matches
/// by name alone.
#[derive(Debug)]
struct TomlKinds {
    pair: u16,
    table: u16,
    table_array_element: u16,
    bare_key: u16,
    quoted_key: u16,
    dotted_key: u16,
    comment: u16,
}

impl TomlKinds {
    /// Resolves the pinned grammar's declaration and key vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind this module
    /// depends on - a grammar-version error, not a reachable operating
    /// state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            pair: kind_id(language, PAIR_KIND),
            table: kind_id(language, TABLE_KIND),
            table_array_element: kind_id(language, TABLE_ARRAY_ELEMENT_KIND),
            bare_key: kind_id(language, BARE_KEY_KIND),
            quoted_key: kind_id(language, QUOTED_KEY_KIND),
            dotted_key: kind_id(language, DOTTED_KEY_KIND),
            comment: kind_id(language, COMMENT_KIND),
        }
    }
}

/// Resolves one node kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned TOML grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// The pinned grammar's decisions for the shared bounded walk.
#[derive(Debug)]
struct TomlRules {
    kinds: &'static TomlKinds,
}

impl TomlRules {
    /// Whether `kind_id` is one of the three key-segment kinds a header or a
    /// pair's key can be built from.
    fn is_key_kind(&self, kind_id: u16) -> bool {
        kind_id == self.kinds.bare_key
            || kind_id == self.kinds.quoted_key
            || kind_id == self.kinds.dotted_key
    }

    /// One key segment's spelling: a `bare_key`'s bytes, or a `quoted_key`'s
    /// bytes between its quotes, escapes kept as authored. `None` for a
    /// node that is neither.
    fn key_segment_spelling(&self, node: Node<'_>, text: &str) -> Option<String> {
        let id = node.kind_id();
        if id == self.kinds.bare_key {
            return text.get(node.byte_range()).map(str::to_owned);
        }
        if id == self.kinds.quoted_key {
            let spelling = text.get(node.byte_range())?;
            let inner_start = QUOTE_BYTE_WIDTH;
            let inner_end = spelling.len().saturating_sub(QUOTE_BYTE_WIDTH);
            return spelling.get(inner_start..inner_end).map(str::to_owned);
        }
        None
    }

    /// Every leaf key segment `key` spells, left to right. A `dotted_key`
    /// nests one level per `.`, so this walks the chain with an explicit
    /// worklist rather than recursing: an adversarial dot count cannot grow
    /// the call stack, only this heap-allocated list.
    fn key_segments(&self, key: Node<'_>, text: &str) -> Vec<String> {
        let mut segments = Vec::new();
        let mut pending = vec![key];
        while let Some(node) = pending.pop() {
            if node.kind_id() == self.kinds.dotted_key {
                for index in (0..node.named_child_count()).rev() {
                    if let Some(child) = node.named_child(index) {
                        pending.push(child);
                    }
                }
                continue;
            }
            if let Some(segment) = self.key_segment_spelling(node, text) {
                segments.push(segment);
            }
        }
        segments
    }

    /// A key's declared name: its segments joined by the qualification
    /// separator. `None` for a single segment spelling no characters - a
    /// dotted key keeps every segment, even an empty one, because the
    /// joined spelling still carries the others.
    fn key_name(&self, key: Node<'_>, text: &str) -> Option<String> {
        let segments = self.key_segments(key, text);
        if let [only] = segments.as_slice()
            && only.is_empty()
        {
            return None;
        }
        (!segments.is_empty()).then(|| segments.join(QUALIFICATION_SEPARATOR))
    }

    /// A pair's key and value, found by kind rather than position: a
    /// trailing same-line comment is a third named child, filtered out
    /// before the two structural children are read in order.
    fn pair_children<'tree>(
        &self,
        pair: Node<'tree>,
    ) -> (Option<Node<'tree>>, Option<Node<'tree>>) {
        let mut structural = (0..pair.named_child_count())
            .filter_map(|index| pair.named_child(index))
            .filter(|child| child.kind_id() != self.kinds.comment);
        (structural.next(), structural.next())
    }

    /// A table or table array element's header key: the first named child
    /// of a key kind, filtered the same way a pair's children are.
    fn header_key<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        (0..node.named_child_count())
            .filter_map(|index| node.named_child(index))
            .find(|child| self.is_key_kind(child.kind_id()))
    }

    /// The declaration of one pair, with the value span as the body.
    fn pair_declaration(
        &self,
        pair: Node<'_>,
        text: &str,
    ) -> Result<Option<Declaration>, SyntaxError> {
        let (key, value) = self.pair_children(pair);
        let Some(name) = key.and_then(|key| self.key_name(key, text)) else {
            return Ok(None);
        };
        let body_range = value.map(extract::byte_range).transpose()?;
        Ok(Some(Declaration {
            name,
            kind: MEMBER_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range,
        }))
    }

    /// The declaration of one table or table array element, named from its
    /// header key. Neither grammar kind wraps its member pairs in a body
    /// node, so the declaration carries no `body_range`.
    fn table_declaration(&self, node: Node<'_>, text: &str) -> Option<Declaration> {
        let name = self
            .header_key(node)
            .and_then(|key| self.key_name(key, text))?;
        Some(Declaration {
            name,
            kind: TABLE_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range: None,
        })
    }
}

impl GrammarRules for TomlRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        let id = node.kind_id();
        if id == self.kinds.pair {
            return self.pair_declaration(node, text);
        }
        if id == self.kinds.table || id == self.kinds.table_array_element {
            return Ok(self.table_declaration(node, text));
        }
        Ok(None)
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        let id = node.kind_id();
        if id == self.kinds.pair {
            let (key, _value) = self.pair_children(node);
            return key.and_then(|key| self.key_name(key, text));
        }
        if id == self.kinds.table || id == self.kinds.table_array_element {
            return self
                .header_key(node)
                .and_then(|key| self.key_name(key, text));
        }
        None
    }

    /// A declaration starts at its own node: nothing attaches in front.
    fn declaration_start(&self, node: Node<'_>, _text: &str) -> usize {
        node.start_byte()
    }

    fn qualification_separator(&self) -> &'static str {
        QUALIFICATION_SEPARATOR
    }
}

/// Bounded Tree-sitter TOML fact provider.
#[derive(Debug, Clone)]
pub struct TomlSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl TomlSyntaxProvider {
    /// File extensions this provider parses, without their leading dot.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["toml"];

    /// Default maximum bytes this provider accepts from one TOML source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "toml".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The TOML provider's declared default bounds, proven positive at compile
/// time.
const TOML_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    TomlSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for TomlSyntaxProvider {
    fn default() -> Self {
        Self::new(TOML_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for TomlSyntaxProvider {
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
        let grammar = toml_grammar();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|_| incompatible_grammar(&grammar))?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let rules = TomlRules {
            kinds: toml_kinds(),
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

    /// Portable structural facets for one TOML grammar node kind. The
    /// structural kinds - `document`, `array`, `inline_table`, key segments
    /// - carry none.
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            // A pair declares its key's name; a table or table array
            // element declares its header key's name.
            PAIR_KIND | TABLE_KIND | TABLE_ARRAY_ELEMENT_KIND => vec![NodeFacet::Declaration],
            // A value written out directly.
            STRING_KIND
            | INTEGER_KIND
            | FLOAT_KIND
            | BOOLEAN_KIND
            | LOCAL_DATE_KIND
            | LOCAL_TIME_KIND
            | LOCAL_DATE_TIME_KIND
            | OFFSET_DATE_TIME_KIND => {
                vec![NodeFacet::Literal]
            }
            COMMENT_KIND => vec![NodeFacet::Comment],
            _ => Vec::new(),
        }
    }
}

fn toml_grammar() -> tree_sitter::Language {
    tree_sitter_toml_ng::LANGUAGE.into()
}

/// Returns the process-wide resolved TOML kind table, computing it once.
fn toml_kinds() -> &'static TomlKinds {
    static KINDS: OnceLock<TomlKinds> = OnceLock::new();
    KINDS.get_or_init(|| TomlKinds::resolve(&toml_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("rift.toml").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        TomlSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("TOML fixture must parse")
    }

    #[test]
    fn key_segment_spelling_answers_for_a_key_and_refuses_for_anything_else() {
        let text = "bare = 1\n\"quoted\" = 2\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_toml_ng::LANGUAGE.into())
            .expect("the pinned grammar must load");
        let tree = parser.parse(text, None).expect("the fixture must parse");
        let rules = TomlRules {
            kinds: toml_kinds(),
        };
        let mut spellings = Vec::new();
        let mut values = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();
        for pair in root.named_children(&mut cursor) {
            let mut inner = pair.walk();
            let children: Vec<_> = pair.named_children(&mut inner).collect();
            spellings.push(rules.key_segment_spelling(children[0], text));
            values.push(rules.key_segment_spelling(children[1], text));
        }
        assert_eq!(
            spellings,
            vec![Some("bare".to_owned()), Some("quoted".to_owned())],
            "a bare key is its bytes; a quoted key is the bytes between its quotes"
        );
        assert_eq!(
            values,
            vec![None, None],
            "a value node is neither a bare key nor a quoted key, so it spells no segment"
        );
    }

    fn qualified_names(document: &SyntaxDocument) -> Vec<&str> {
        document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect()
    }

    /// Resolution asserts every kind id is non-zero, so resolving the pinned
    /// grammar's table is the proof the vocabulary exists. The pinned
    /// grammar declares no fields at all.
    #[test]
    fn test_kind_table_resolves_on_the_pinned_grammar() {
        let kinds = TomlKinds::resolve(&toml_grammar());
        assert_ne!(kinds.pair, 0);
        assert_ne!(kinds.table, 0);
        assert_ne!(kinds.table_array_element, 0);
        assert_eq!(toml_grammar().field_count(), 0);
    }

    #[test]
    #[should_panic(expected = "must define node kind used by symbol extraction: \
                               kind=no_such_kind")]
    fn test_kind_resolution_refuses_a_kind_the_grammar_lacks() {
        let _ = kind_id(&toml_grammar(), "no_such_kind");
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = TomlSyntaxProvider::default();
        assert_eq!(provider.language().name, "toml");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["toml"]);
        assert_eq!(
            provider.source_bytes_max(),
            TomlSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    /// A table's pairs qualify through its header key, and a bare pair
    /// before any table stays at the top level. Spans: the whole pair or
    /// table for `range` and `item_range`, the value node for a pair's
    /// `body_range`, and no `body_range` for the table.
    #[test]
    fn test_table_members_qualify_through_the_header_key_with_exact_spans() {
        let text = "name = \"rift\"\n\n[server]\nport = 8080\n";
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
                ("name", None, (0, 13), Some((7, 13))),
                ("server", None, (15, 36), None),
                ("server > port", Some("server"), (24, 35), Some((31, 35))),
            ]
        );
        assert_eq!(&text[7..13], "\"rift\"");
        assert_eq!(&text[31..35], "8080");
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.range == symbol.item_range
                    && symbol.facets.is_empty()
                    && symbol.visibility.is_none()),
            "every declaration spans its own node and carries no facets and \
             no visibility"
        );
        assert_eq!(document.symbols()[0].kind, "member");
        assert_eq!(document.symbols()[1].kind, "table");
        assert_eq!(document.symbols()[2].kind, "member");
        assert!(!document.has_errors());
    }

    /// Repeated `[[hooks]]` headers collide on qualified name, and so does
    /// every identically named pair beneath each element; the document's
    /// shared suffix pass numbers each group apart independently.
    #[test]
    fn test_repeated_table_array_elements_and_their_pairs_take_distinct_suffixes() {
        let text = "[[hooks]]\nid = \"a\"\n[[hooks]]\nid = \"b\"\n";
        let document = analyze(text);
        assert_eq!(
            qualified_names(&document),
            ["hooks~1", "hooks > id~1", "hooks~2", "hooks > id~2"]
        );
        assert_eq!(document.symbols()[0].kind, "table");
        assert_eq!(document.symbols()[1].kind, "member");
    }

    /// A dotted key joins its segments with the qualification separator
    /// before any container qualifies it further, and a dotted table header
    /// does the same for the pairs it holds.
    #[test]
    fn test_dotted_keys_join_their_segments_with_the_qualification_separator() {
        let document = analyze("a.b.c = 1\n\n[server.tls]\ncert = \"a.pem\"\n");
        assert_eq!(
            qualified_names(&document),
            ["a > b > c", "server > tls", "server > tls > cert"]
        );
    }

    /// A quoted key's name is the bytes between its quotes, escapes kept as
    /// authored; either quote character delimits by one byte, and an empty
    /// quoted key names nothing.
    #[test]
    fn test_quoted_key_names_keep_the_authored_spelling_and_skip_empty_keys() {
        let text = "\"a\\\"b\" = 1\n'x > y' = 2\n'@types/node' = 3\n'' = 4\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a\\\"b", "x > y", "@types/node"]);
        assert!(!document.has_errors());
    }

    /// A `#` comment sharing a pair's source line lands as the pair's own
    /// trailing named child; the key and value are still found by kind, so
    /// the comment is never mistaken for the value and the pair's own span
    /// still covers it.
    #[test]
    fn test_a_trailing_same_line_comment_is_not_mistaken_for_the_value() {
        let text = "port = 8080 # the bound port\nhost = \"a\"\n";
        let document = analyze(text);
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.body_range.map(|body| {
                        let start = usize::try_from(body.start).expect("fixture span fits usize");
                        let end = usize::try_from(body.end).expect("fixture span fits usize");
                        &text[start..end]
                    }),
                    (symbol.range.start, symbol.range.end),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            facts,
            [
                ("port", Some("8080"), (0, 28)),
                ("host", Some("\"a\""), (29, 39)),
            ]
        );
        assert!(!document.has_errors());
    }

    /// A standalone leading comment stays a sibling of the pair or table it
    /// precedes: it widens neither node's own span.
    #[test]
    fn test_a_leading_comment_on_its_own_line_stays_with_the_file() {
        let text = "# leading comment\nport = 8080\n";
        let document = analyze(text);
        assert_eq!(
            document.symbols()[0].range,
            crate::document::ByteRange { start: 18, end: 29 }
        );
        assert!(!document.has_errors());
    }

    /// Array elements mint no symbols - no index is invented as a name -
    /// while pairs inside an inline table element still emit, qualified
    /// through the holding pair's key, the same rule an array of objects
    /// follows in JSON.
    #[test]
    fn test_array_elements_emit_only_their_nested_inline_table_pairs() {
        let text = "servers = [{ host = \"a\" }, { host = \"b\" }, 3]\n";
        let document = analyze(text);
        assert_eq!(
            qualified_names(&document),
            ["servers", "servers > host~1", "servers > host~2"]
        );
        assert_ne!(
            document.symbols()[1].range,
            document.symbols()[2].range,
            "the two elements' members keep their own spans"
        );
    }

    /// An inline table's own pairs qualify through the holding pair's key
    /// exactly as a nested table's pairs do.
    #[test]
    fn test_inline_table_pairs_qualify_through_the_holding_pair() {
        let document = analyze("server = { host = \"a\", port = 1 }\n");
        assert_eq!(
            qualified_names(&document),
            ["server", "server > host", "server > port"]
        );
    }

    /// A pair with no value at all - unlike JSON's `{"broken": }`, which
    /// still yields a `pair` node - parses as an `ERROR` node wrapping the
    /// bare key and `=`, not a `pair`: no `member` symbol names it. A
    /// well-formed pair after it still emits, so the malformed line drops
    /// only its own name, not the rest of the file's facts.
    #[test]
    fn test_a_pair_with_no_value_emits_no_symbol_but_keeps_later_facts() {
        let document = analyze("port = \nhost = \"a\"\n");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["host"]);
    }

    /// CRLF sources keep byte-exact spans. The pinned grammar's line-ending
    /// scanner folds a lone trailing `\r` into the preceding pair's own
    /// span while leaving the `\n` outside it, so a pair's range on a CRLF
    /// line runs one byte past its LF-only counterpart; the value node's
    /// own span still excludes both.
    #[test]
    fn test_crlf_source_keeps_byte_exact_ranges() {
        let text = "[server]\r\nport = 8080\r\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), (symbol.range.start, symbol.range.end)))
            .collect::<Vec<_>>();
        assert_eq!(spans, [("server", (0, 23)), ("port", (10, 22))]);
        assert_eq!(&text[17..21], "8080");
        assert!(!document.has_errors());
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
        let bounded = |limits: SyntaxLimits, text: &str| {
            TomlSyntaxProvider::new(limits).analyze(SyntaxSource {
                path: &path(),
                text,
            })
        };
        let source_error = bounded(
            SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            "a = 1\n",
        )
        .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = bounded(
            SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            "a = 1\n",
        )
        .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error = bounded(
            SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            "a = { b = 1 }\n",
        )
        .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// The declaring kinds carry the `Declaration` facet, value kinds read
    /// as `Literal`, a comment as `Comment`, and structure carries none.
    #[test]
    fn test_node_facets_classify_the_interpreted_kinds() {
        let provider = TomlSyntaxProvider::default();
        for kind in [PAIR_KIND, TABLE_KIND, TABLE_ARRAY_ELEMENT_KIND] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Declaration],
                "kind {kind} must classify as a declaration"
            );
        }
        for kind in [
            STRING_KIND,
            INTEGER_KIND,
            FLOAT_KIND,
            BOOLEAN_KIND,
            LOCAL_DATE_KIND,
            LOCAL_TIME_KIND,
            LOCAL_DATE_TIME_KIND,
            OFFSET_DATE_TIME_KIND,
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Literal],
                "kind {kind} must classify as a literal value"
            );
        }
        assert_eq!(provider.node_facets(COMMENT_KIND), [NodeFacet::Comment]);
        for kind in [
            "document",
            "array",
            "inline_table",
            BARE_KEY_KIND,
            DOTTED_KEY_KIND,
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [],
                "structural kind {kind} carries no portable facet"
            );
        }
    }
}
