//! YAML syntax facts from the pinned tree-sitter-yaml grammar.
//!
//! Each mapping entry - the grammar's `block_mapping_pair` or `flow_pair` -
//! declares one `mapping_entry` symbol. A `document` declares a symbol of
//! its own only when the stream holds more than one document. Sequence
//! items have no keys and mint no symbols - no index is invented as a name.
//!
//! Decisions this module fixes:
//! - A symbol's `name` is the key's scalar spelling: a plain or block
//!   scalar's bytes as authored, a quoted scalar's bytes between the
//!   quotes with escapes kept. A key that is not a scalar - a flow
//!   collection - names by its own source bytes. A key spelling no
//!   characters (`''`) names nothing and emits no symbol.
//! - Qualified names join nested entry keys with ` > ` (`server > port`),
//!   the markdown spelling and for the markdown reason: a key is an
//!   arbitrary string, so the language reserves no separator - `.` and `/`
//!   both occur in real keys - and every byte of ` > ` escapes in the
//!   minted address, so the spelling cannot be mistaken for path
//!   structure.
//! - In a stream of one document, entries qualify by their key path alone.
//!   In a multi-document stream each document declares a `document` symbol
//!   named by its 1-based ordinal decimal string - the stream orders its
//!   documents, and they carry no name of their own - and its entries file
//!   under that ordinal (`2 > name`).
//! - An anchor or alias emits no symbol: an entry whose value is an alias
//!   or carries an anchor is a normal entry via its key.
//! - Nothing attaches in front of an entry, so `range` equals
//!   `item_range`. An entry's `body_range` is the value node's span,
//!   absent for a key with no value; a document's is its content node's
//!   span, absent for a bare `---`.
//! - Entries carry no visibility and no portable symbol facet:
//!   `visibility` stays `None` and `facets` stays empty.
//! - `signatures` and `documentation` stay empty for every entry and every
//!   document: YAML has no callable form and, with nothing attached in
//!   front of a declaration, no comment for one to carry either.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet};
use tree_sitter::{Node, Parser};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Grammar spelling of a `document`.
const DOCUMENT_KIND: &str = "document";
/// Grammar spelling of a `block_node`, the wrapper around block content.
const BLOCK_NODE_KIND: &str = "block_node";
/// Grammar spelling of a `flow_node`, the wrapper around flow content.
const FLOW_NODE_KIND: &str = "flow_node";
/// Grammar spelling of a `block_mapping_pair` (`key: value`).
const BLOCK_MAPPING_PAIR_KIND: &str = "block_mapping_pair";
/// Grammar spelling of a `flow_pair` (`{key: value}`).
const FLOW_PAIR_KIND: &str = "flow_pair";
/// Grammar spelling of a `plain_scalar`.
const PLAIN_SCALAR_KIND: &str = "plain_scalar";
/// Grammar spelling of a `single_quote_scalar`.
const SINGLE_QUOTE_SCALAR_KIND: &str = "single_quote_scalar";
/// Grammar spelling of a `double_quote_scalar`.
const DOUBLE_QUOTE_SCALAR_KIND: &str = "double_quote_scalar";
/// Grammar spelling of a `block_scalar` (`|` or `>` content).
const BLOCK_SCALAR_KIND: &str = "block_scalar";
/// Grammar field holding a pair's key.
const KEY_FIELD: &str = "key";
/// Grammar field holding a pair's value.
const VALUE_FIELD: &str = "value";

/// The YAML kind word behind the wire kind `yaml.mapping_entry`.
const MAPPING_ENTRY_KIND_WORD: &str = "mapping_entry";
/// The YAML kind word behind the wire kind `yaml.document`.
const DOCUMENT_KIND_WORD: &str = "document";

/// The separator YAML qualified names join nested entry keys with.
const ENTRY_QUALIFICATION_SEPARATOR: &str = " > ";

/// Numeric grammar ids for every kind and field this module reads, resolved
/// once so each walk decision compares integers.
#[derive(Debug)]
struct YamlKinds {
    document: u16,
    block_node: u16,
    flow_node: u16,
    block_mapping_pair: u16,
    flow_pair: u16,
    plain_scalar: u16,
    single_quote_scalar: u16,
    double_quote_scalar: u16,
    block_scalar: u16,
    key: NonZeroU16,
    value: NonZeroU16,
}

impl YamlKinds {
    /// Resolves the pinned grammar's mapping vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            document: kind_id(language, DOCUMENT_KIND),
            block_node: kind_id(language, BLOCK_NODE_KIND),
            flow_node: kind_id(language, FLOW_NODE_KIND),
            block_mapping_pair: kind_id(language, BLOCK_MAPPING_PAIR_KIND),
            flow_pair: kind_id(language, FLOW_PAIR_KIND),
            plain_scalar: kind_id(language, PLAIN_SCALAR_KIND),
            single_quote_scalar: kind_id(language, SINGLE_QUOTE_SCALAR_KIND),
            double_quote_scalar: kind_id(language, DOUBLE_QUOTE_SCALAR_KIND),
            block_scalar: kind_id(language, BLOCK_SCALAR_KIND),
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
        "pinned YAML grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one grammar field id, proving the pinned grammar defines it.
fn field_id(language: &tree_sitter::Language, field: &str) -> NonZeroU16 {
    language.field_id_for_name(field).unwrap_or_else(|| {
        panic!(
            "pinned YAML grammar must define field used by symbol \
             extraction: field={field}"
        )
    })
}

/// The pinned grammar's decisions for the shared bounded walk, carrying the
/// stream's document ordinals resolved once per analysis.
#[derive(Debug)]
struct YamlRules {
    kinds: &'static YamlKinds,
    /// Document node ids beside their 1-based ordinal, sorted by node id
    /// for lookup; empty for a stream of at most one document, where no
    /// document symbol exists.
    document_ordinals: Vec<(usize, usize)>,
}

impl YamlRules {
    /// Builds the rules for one parsed stream, resolving the stream's
    /// document ordinals once so each lookup during the walk is a binary
    /// search rather than a sibling scan.
    fn new(kinds: &'static YamlKinds, root: Node<'_>) -> Self {
        let mut document_ordinals: Vec<(usize, usize)> = (0..root.named_child_count())
            .filter_map(|index| root.named_child(index))
            .filter(|child| child.kind_id() == kinds.document)
            .enumerate()
            .map(|(index, child)| (child.id(), index + 1))
            .collect();
        if document_ordinals.len() < 2 {
            document_ordinals.clear();
        }
        document_ordinals.sort_unstable_by_key(|(id, _)| *id);
        Self {
            kinds,
            document_ordinals,
        }
    }

    /// The key's scalar spelling: the scalar's bytes as authored, quotes
    /// removed for a quoted scalar. `None` when `node` neither is nor
    /// wraps a scalar.
    fn scalar_spelling(&self, node: Node<'_>, text: &str) -> Option<String> {
        if let Some(spelling) = self.direct_scalar_spelling(node, text) {
            return Some(spelling);
        }
        (0..node.named_child_count())
            .filter_map(|index| node.named_child(index))
            .find_map(|child| self.direct_scalar_spelling(child, text))
    }

    /// The spelling when `node` itself is a scalar kind.
    fn direct_scalar_spelling(&self, node: Node<'_>, text: &str) -> Option<String> {
        let id = node.kind_id();
        let quoted = id == self.kinds.single_quote_scalar || id == self.kinds.double_quote_scalar;
        let scalar = quoted || id == self.kinds.plain_scalar || id == self.kinds.block_scalar;
        if !scalar {
            return None;
        }
        let spelling = text.get(node.byte_range())?;
        if quoted {
            return spelling
                .get(1..spelling.len().saturating_sub(1))
                .map(Into::into);
        }
        Some(spelling.to_owned())
    }

    /// The entry's name from its key; `None` for a keyless pair or a key
    /// spelling no characters.
    fn entry_name(&self, pair: Node<'_>, text: &str) -> Option<String> {
        let key = pair.child_by_field_id(self.kinds.key.get())?;
        let name = self
            .scalar_spelling(key, text)
            .or_else(|| text.get(key.byte_range()).map(Into::into))?;
        (!name.is_empty()).then_some(name)
    }

    /// The declaration of one mapping entry, with the value span as the
    /// body.
    fn entry_declaration(
        &self,
        pair: Node<'_>,
        text: &str,
    ) -> Result<Option<Declaration>, SyntaxError> {
        let Some(name) = self.entry_name(pair, text) else {
            return Ok(None);
        };
        let body_range = match pair.child_by_field_id(self.kinds.value.get()) {
            Some(value) => Some(extract::byte_range(value)?),
            None => None,
        };
        Ok(Some(Declaration {
            name,
            kind: MAPPING_ENTRY_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range,
            documentation: Vec::new(),
        }))
    }

    /// The document's 1-based ordinal; `None` outside a multi-document
    /// stream.
    fn document_ordinal(&self, document: Node<'_>) -> Option<usize> {
        self.document_ordinals
            .binary_search_by_key(&document.id(), |(id, _)| *id)
            .ok()
            .map(|index| self.document_ordinals[index].1)
    }

    /// The declaration of one document in a multi-document stream, named by
    /// its ordinal, with the content node's span as the body.
    fn document_declaration(&self, document: Node<'_>) -> Result<Option<Declaration>, SyntaxError> {
        let Some(ordinal) = self.document_ordinal(document) else {
            return Ok(None);
        };
        Ok(Some(Declaration {
            name: ordinal.to_string(),
            kind: DOCUMENT_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range: self.document_body_range(document)?,
            documentation: Vec::new(),
        }))
    }

    /// The document's content span; `None` for a bare `---`.
    fn document_body_range(&self, document: Node<'_>) -> Result<Option<ByteRange>, SyntaxError> {
        let content = (0..document.named_child_count())
            .filter_map(|index| document.named_child(index))
            .find(|child| {
                child.kind_id() == self.kinds.block_node || child.kind_id() == self.kinds.flow_node
            });
        match content {
            Some(node) => extract::byte_range(node).map(Some),
            None => Ok(None),
        }
    }
}

impl GrammarRules for YamlRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        let id = node.kind_id();
        if id == self.kinds.block_mapping_pair || id == self.kinds.flow_pair {
            return self.entry_declaration(node, text);
        }
        if id == self.kinds.document {
            return self.document_declaration(node);
        }
        Ok(None)
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        let id = node.kind_id();
        if id == self.kinds.block_mapping_pair || id == self.kinds.flow_pair {
            return self.entry_name(node, text);
        }
        if id == self.kinds.document {
            return self
                .document_ordinal(node)
                .map(|ordinal| ordinal.to_string());
        }
        None
    }

    /// A declaration starts at its own node: nothing attaches in front.
    fn declaration_start(&self, node: Node<'_>, _text: &str) -> usize {
        node.start_byte()
    }

    fn qualification_separator(&self) -> &'static str {
        ENTRY_QUALIFICATION_SEPARATOR
    }
}

/// Bounded Tree-sitter YAML fact provider.
#[derive(Debug, Clone)]
pub struct YamlSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl YamlSyntaxProvider {
    /// File extensions this provider parses, without their leading dot.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["yaml", "yml"];

    /// Default maximum bytes this provider accepts from one YAML source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "yaml".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The YAML provider's declared default bounds, proven positive at compile
/// time.
const YAML_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    YamlSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for YamlSyntaxProvider {
    fn default() -> Self {
        Self::new(YAML_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for YamlSyntaxProvider {
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
        let grammar = yaml_grammar();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|_| incompatible_grammar(&grammar))?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let rules = YamlRules::new(yaml_kinds(), tree.root_node());
        let (nodes, symbols) = extract::extract(
            tree.root_node(),
            source,
            self.limits,
            &self.language,
            &rules,
        )?;
        Ok(SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            nodes,
            symbols,
            tree.root_node().has_error(),
        ))
    }

    /// Portable structural facets for one YAML grammar node kind. The
    /// structural kinds - `stream`, node wrappers, mappings, sequences -
    /// carry none.
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            // The pair declares its key's name; a document declares its
            // ordinal in a multi-document stream; an anchor introduces a
            // label other content refers to.
            BLOCK_MAPPING_PAIR_KIND | FLOW_PAIR_KIND | DOCUMENT_KIND | "anchor" => {
                vec![NodeFacet::Declaration]
            }
            // A value written out directly.
            PLAIN_SCALAR_KIND
            | SINGLE_QUOTE_SCALAR_KIND
            | DOUBLE_QUOTE_SCALAR_KIND
            | BLOCK_SCALAR_KIND
            | "string_scalar"
            | "integer_scalar"
            | "float_scalar"
            | "boolean_scalar"
            | "null_scalar" => vec![NodeFacet::Literal],
            // An alias is a written name referring to an anchor.
            "alias" => vec![NodeFacet::Identifier],
            // A tag or directive qualifies the node or document it rides.
            "tag" | "yaml_directive" | "tag_directive" | "reserved_directive" => {
                vec![NodeFacet::Annotation]
            }
            "comment" => vec![NodeFacet::Comment],
            _ => Vec::new(),
        }
    }
}

fn yaml_grammar() -> tree_sitter::Language {
    tree_sitter_yaml::LANGUAGE.into()
}

/// Returns the process-wide resolved YAML kind table, computing it once.
fn yaml_kinds() -> &'static YamlKinds {
    static KINDS: OnceLock<YamlKinds> = OnceLock::new();
    KINDS.get_or_init(|| YamlKinds::resolve(&yaml_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("deploy/pipeline.yaml").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        YamlSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("YAML fixture must parse")
    }

    fn qualified_names(document: &SyntaxDocument) -> Vec<(&str, Option<&str>)> {
        document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.container.as_deref()))
            .collect()
    }

    /// Resolution asserts every kind and field id is non-zero, so resolving
    /// the pinned grammar's table is the proof the vocabulary exists.
    #[test]
    fn test_kind_table_resolves_on_the_pinned_grammar() {
        let kinds = YamlKinds::resolve(&yaml_grammar());
        assert_ne!(kinds.document, 0);
        assert_ne!(kinds.block_mapping_pair, 0);
        assert_ne!(kinds.flow_pair, 0);
    }

    #[test]
    #[should_panic(expected = "must define node kind used by symbol extraction: \
                               kind=no_such_kind")]
    fn test_kind_resolution_refuses_a_kind_the_grammar_lacks() {
        let _ = kind_id(&yaml_grammar(), "no_such_kind");
    }

    #[test]
    #[should_panic(expected = "must define field used by symbol extraction: \
                               field=no_such_field")]
    fn test_field_resolution_refuses_a_field_the_grammar_lacks() {
        let _ = field_id(&yaml_grammar(), "no_such_field");
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = YamlSyntaxProvider::default();
        assert_eq!(provider.language().name, "yaml");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["yaml", "yml"]);
        assert_eq!(
            provider.source_bytes_max(),
            YamlSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    /// Block and flow entries qualify through their key path with
    /// byte-exact spans: the whole pair for `range` and `item_range`, the
    /// value node for `body_range`.
    #[test]
    fn test_block_and_flow_entries_qualify_through_the_key_path() {
        let text = "server:\n  port: 8080\nflow: {a: 1, b: [2, 3]}\n";
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
                ("server", None, (0, 20), Some((10, 20))),
                ("server > port", Some("server"), (10, 20), Some((16, 20))),
                ("flow", None, (21, 44), Some((27, 44))),
                ("flow > a", Some("flow"), (28, 32), Some((31, 32))),
                ("flow > b", Some("flow"), (34, 43), Some((37, 43))),
            ]
        );
        assert_eq!(&text[10..20], "port: 8080");
        assert_eq!(&text[16..20], "8080");
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.kind == "mapping_entry"
                    && symbol.range == symbol.item_range
                    && symbol.facets.is_empty()
                    && symbol.visibility.is_none()),
            "every entry files under the one YAML mapping kind, spans its \
             own pair, and carries no facets and no visibility"
        );
        assert!(!document.has_errors());
    }

    /// A stream of one document declares no `document` symbol: entries
    /// qualify by their key path alone.
    #[test]
    fn test_single_document_stream_emits_no_document_symbol() {
        let document = analyze("---\nname: rift\n");
        assert_eq!(qualified_names(&document), [("name", None)]);
        assert_eq!(document.symbols()[0].kind, "mapping_entry");
    }

    /// Each document in a multi-document stream declares a `document`
    /// symbol named by its ordinal, and entries file under that ordinal.
    #[test]
    fn test_multi_document_stream_names_documents_by_ordinal() {
        let text = "---\nname: one\n---\nname: two\n";
        let document = analyze(text);
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.kind,
                    symbol.container.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            facts,
            [
                ("1", "document", None),
                ("1 > name", "mapping_entry", Some("1")),
                ("2", "document", None),
                ("2 > name", "mapping_entry", Some("2")),
            ]
        );
        assert!(!document.has_errors());
    }

    /// A bare `---` document still counts in the ordinals and declares
    /// with no body; a document holding content spans it as the body.
    #[test]
    fn test_empty_document_in_a_stream_declares_without_a_body() {
        let document = analyze("---\n---\nname: two\n");
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(facts, [("1", false), ("2", true), ("2 > name", true)]);
    }

    /// Sequence items mint no symbols - no index is invented as a name -
    /// while entries inside a sequence item's mapping still emit, qualified
    /// through the holding entry's key path.
    #[test]
    fn test_sequence_items_emit_only_their_nested_entries() {
        let text = "servers:\n  - host: a\n  - host: b\nplain:\n  - 1\n  - 2\n";
        let document = analyze(text);
        assert_eq!(
            qualified_names(&document),
            [
                ("servers", None),
                ("servers > host~1", Some("servers")),
                ("servers > host~2", Some("servers")),
                ("plain", None),
            ]
        );
        assert_ne!(
            document.symbols()[1].range,
            document.symbols()[2].range,
            "the two items' entries keep their own spans"
        );
    }

    /// An anchor and an alias emit no symbols of their own: the entry
    /// carrying them stays a normal entry via its key, and an aliased
    /// value is the entry's body.
    #[test]
    fn test_anchors_and_aliases_emit_no_symbols() {
        let text = "base: &defaults\n  a: 1\nalias: *defaults\n";
        let document = analyze(text);
        assert_eq!(
            qualified_names(&document),
            [("base", None), ("base > a", Some("base")), ("alias", None)]
        );
        let alias = &document.symbols()[2];
        let body = alias.body_range.expect("the aliased value is the body");
        let start = usize::try_from(body.start).expect("fixture span fits usize");
        let end = usize::try_from(body.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "*defaults");
    }

    /// Quoted keys name by the bytes between the quotes; a key spelling no
    /// characters names nothing, and a key with no value has no body.
    #[test]
    fn test_key_spellings_strip_quotes_and_skip_empty_keys() {
        let text = "\"quoted key\": v\n'single': w\n'': x\nbare:\n";
        let document = analyze(text);
        let facts = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            facts,
            [("quoted key", true), ("single", true), ("bare", false)]
        );
    }

    /// Repeated keys take distinct qualified names with distinct spans:
    /// neither keeps the bare key path, each takes a `~N` suffix, the same
    /// policy every provider applies.
    #[test]
    fn test_repeated_keys_take_distinct_qualified_names_with_distinct_spans() {
        let document = analyze("port: 1\nport: 2\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["port~1", "port~2"]);
        assert_ne!(
            document.symbols()[0].range,
            document.symbols()[1].range,
            "the two entries keep their own spans"
        );
    }

    /// A sequence of mappings - the shape `.github/dependabot.yml` writes
    /// its `updates` under - gives each item's repeated key its own
    /// qualified name, so the file mints one identity per declaration.
    #[test]
    fn test_a_sequence_of_mappings_names_each_repeated_key_apart() {
        let text = "updates:\n  - package-ecosystem: bun\n  \
                    - package-ecosystem: cargo\n  \
                    - package-ecosystem: github-actions\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "updates",
                "updates > package-ecosystem~1",
                "updates > package-ecosystem~2",
                "updates > package-ecosystem~3"
            ]
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("key: [unclosed\n");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
    }

    /// CRLF sources keep byte-exact spans: names exclude `\r`, and every
    /// range lands on the CRLF file's own byte offsets.
    #[test]
    fn test_crlf_source_keeps_byte_exact_ranges() {
        let text = "server:\r\n  port: 8080\r\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.map(|body| (body.start, body.end)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("server", (0, 23), Some((11, 23))),
                ("port", (11, 21), Some((17, 21))),
            ]
        );
        assert_eq!(&text[17..21], "8080");
        assert!(!document.has_errors());
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
        let bounded = |limits: SyntaxLimits, text: &str| {
            YamlSyntaxProvider::new(limits).analyze(SyntaxSource {
                path: &path(),
                text,
            })
        };
        let source_error = bounded(
            SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            "a: 1\n",
        )
        .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = bounded(
            SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            "a: 1\n",
        )
        .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error = bounded(
            SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            "a:\n  b: 1\n",
        )
        .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    /// Deep nesting stays well inside the default depth budget.
    #[test]
    fn test_deep_nesting_fits_the_default_depth_budget() {
        let mut text = String::new();
        for level in 0..64 {
            text.push_str(&"  ".repeat(level));
            text.push_str("level:\n");
        }
        text.push_str(&"  ".repeat(64));
        text.push_str("leaf: 1\n");
        let document = analyze(&text);
        assert_eq!(document.symbols().len(), 65);
        assert!(!document.has_errors());
    }

    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// The declaring kinds carry the `Declaration` facet, scalars read as
    /// `Literal`, an alias as `Identifier`, tags and directives as
    /// `Annotation`, and structure carries none.
    #[test]
    fn test_node_facets_classify_the_interpreted_kinds() {
        let provider = YamlSyntaxProvider::default();
        for kind in [
            BLOCK_MAPPING_PAIR_KIND,
            FLOW_PAIR_KIND,
            DOCUMENT_KIND,
            "anchor",
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Declaration],
                "kind {kind} must classify as a declaration"
            );
        }
        for kind in [
            PLAIN_SCALAR_KIND,
            SINGLE_QUOTE_SCALAR_KIND,
            DOUBLE_QUOTE_SCALAR_KIND,
            BLOCK_SCALAR_KIND,
            "string_scalar",
            "integer_scalar",
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Literal],
                "kind {kind} must classify as a literal value"
            );
        }
        assert_eq!(provider.node_facets("alias"), [NodeFacet::Identifier]);
        for kind in [
            "tag",
            "yaml_directive",
            "tag_directive",
            "reserved_directive",
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Annotation],
                "kind {kind} must classify as an annotation"
            );
        }
        assert_eq!(provider.node_facets("comment"), [NodeFacet::Comment]);
        for kind in [
            "stream",
            BLOCK_NODE_KIND,
            FLOW_NODE_KIND,
            "block_mapping",
            "flow_mapping",
            "block_sequence",
            "block_sequence_item",
            "flow_sequence",
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [],
                "structural kind {kind} carries no portable facet"
            );
        }
    }
}
