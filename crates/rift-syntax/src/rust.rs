//! Rust syntax facts from the pinned tree-sitter-rust grammar.

mod attachment;
mod binding;
#[cfg(test)]
mod fixture;
mod layout;

pub use layout::RustCrateLayout;

use std::sync::OnceLock;

use rift_binding::ModuleLayout;
use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet, SymbolFacet};
use tree_sitter::{Node, Parser, Query as TreeSitterQuery, QueryCursor, StreamingIterator};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxBound, SyntaxError, SyntaxFault, incompatible_grammar, invalid_query};
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Rust declaration kind emitted by the Tree-sitter provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RustSymbolKind {
    /// Function or method.
    Function,
    /// Structure.
    Struct,
    /// Enumeration.
    Enum,
    /// Trait.
    Trait,
    /// Type alias.
    TypeAlias,
    /// Constant.
    Constant,
    /// Static item.
    Static,
    /// Module.
    Module,
    /// Declarative macro.
    Macro,
}

impl RustSymbolKind {
    /// The provider kind word behind the wire kind `rust.{word}`.
    const fn word(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::TypeAlias => "type_alias",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::Module => "module",
            Self::Macro => "macro",
        }
    }

    /// Portable facets for this kind, before visibility adds `Public`.
    fn facets(self) -> Vec<SymbolFacet> {
        match self {
            Self::Function => vec![SymbolFacet::Value, SymbolFacet::Callable],
            Self::Struct | Self::Enum | Self::Trait => vec![SymbolFacet::Type],
            Self::TypeAlias => vec![SymbolFacet::Type, SymbolFacet::Alias],
            Self::Module => vec![SymbolFacet::Namespace, SymbolFacet::Module],
            Self::Macro => vec![SymbolFacet::Macro],
            Self::Constant | Self::Static => vec![SymbolFacet::Value],
        }
    }

    /// The grammar field spanning this kind's implementation part; `None`
    /// for a kind whose grammar declares no body or value field.
    const fn body_field(self) -> Option<RustGrammarField> {
        match self {
            Self::Function | Self::Struct | Self::Enum | Self::Trait | Self::Module => {
                Some(RustGrammarField::Body)
            }
            Self::Constant | Self::Static => Some(RustGrammarField::Value),
            Self::TypeAlias | Self::Macro => None,
        }
    }
}

/// Tree-sitter grammar node kind interpreted by this module.
///
/// Vocabulary comes from the `node-types.json` of the pinned
/// tree-sitter-rust 0.24.2 grammar. The grammar defines many more kinds;
/// only the ones this module reads are listed, so conversion from an
/// arbitrary kind string is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustGrammarNodeKind {
    /// `const_item` declaration.
    ConstItem,
    /// `enum_item` declaration.
    EnumItem,
    /// `function_item` declaration.
    FunctionItem,
    /// `impl_item` block.
    ImplItem,
    /// `macro_definition` declaration.
    MacroDefinition,
    /// `mod_item` declaration.
    ModItem,
    /// `static_item` declaration.
    StaticItem,
    /// `struct_item` declaration.
    StructItem,
    /// `trait_item` declaration.
    TraitItem,
    /// `type_item` declaration.
    TypeItem,
    /// `visibility_modifier` marker on one declaration.
    VisibilityModifier,
}

impl RustGrammarNodeKind {
    /// Every interpreted kind, ordered by grammar spelling.
    const ALL: [Self; 11] = [
        Self::ConstItem,
        Self::EnumItem,
        Self::FunctionItem,
        Self::ImplItem,
        Self::MacroDefinition,
        Self::ModItem,
        Self::StaticItem,
        Self::StructItem,
        Self::TraitItem,
        Self::TypeItem,
        Self::VisibilityModifier,
    ];

    /// Returns grammar spelling from tree-sitter-rust `node-types.json`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConstItem => "const_item",
            Self::EnumItem => "enum_item",
            Self::FunctionItem => "function_item",
            Self::ImplItem => "impl_item",
            Self::MacroDefinition => "macro_definition",
            Self::ModItem => "mod_item",
            Self::StaticItem => "static_item",
            Self::StructItem => "struct_item",
            Self::TraitItem => "trait_item",
            Self::TypeItem => "type_item",
            Self::VisibilityModifier => "visibility_modifier",
        }
    }

    /// Classifies grammar kinds the tree walk does not interpret as `None`.
    fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == kind)
    }

    const fn symbol_kind(self) -> Option<RustSymbolKind> {
        match self {
            Self::FunctionItem => Some(RustSymbolKind::Function),
            Self::StructItem => Some(RustSymbolKind::Struct),
            Self::EnumItem => Some(RustSymbolKind::Enum),
            Self::TraitItem => Some(RustSymbolKind::Trait),
            Self::TypeItem => Some(RustSymbolKind::TypeAlias),
            Self::ConstItem => Some(RustSymbolKind::Constant),
            Self::StaticItem => Some(RustSymbolKind::Static),
            Self::ModItem => Some(RustSymbolKind::Module),
            Self::MacroDefinition => Some(RustSymbolKind::Macro),
            Self::ImplItem | Self::VisibilityModifier => None,
        }
    }
}

impl std::str::FromStr for RustGrammarNodeKind {
    type Err = SyntaxError;

    fn from_str(kind: &str) -> Result<Self, Self::Err> {
        Self::from_kind(kind)
            .ok_or_else(|| Error::new(SyntaxFault::UnknownNodeKind { kind: kind.into() }))
    }
}

/// Grammar field name this module reads, from tree-sitter-rust 0.24.2
/// `node-types.json`.
#[derive(Debug, Clone, Copy)]
enum RustGrammarField {
    /// `name` field on declaration items.
    Name,
    /// `type` field on `impl_item`.
    Type,
    /// `body` field on block-bodied declaration items.
    Body,
    /// `value` field on `const_item` and `static_item`.
    Value,
}

impl RustGrammarField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Type => "type",
            Self::Body => "body",
            Self::Value => "value",
        }
    }
}

/// Authored visibility of one Rust declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RustVisibility {
    /// Declaration has no visibility modifier.
    Private,
    /// Declaration uses `pub`.
    Public,
    /// Declaration uses exact restricted visibility spelling such as `pub(crate)`.
    Restricted(String),
}

impl RustVisibility {
    /// Classifies authored `visibility_modifier` text from tree-sitter-rust grammar.
    ///
    /// Bare `pub` is [`RustVisibility::Public`]; any other authored spelling
    /// such as `pub(crate)` or `pub(in path)` stays verbatim in
    /// [`RustVisibility::Restricted`]. Declarations without a modifier never
    /// reach this conversion and stay [`RustVisibility::Private`].
    fn from_authored(text: &str) -> Self {
        match text {
            "pub" => Self::Public,
            restricted => Self::Restricted(restricted.into()),
        }
    }

    /// The authored spelling served on the wire: `private`, `pub`, or the
    /// restricted form verbatim.
    fn authored(&self) -> String {
        match self {
            Self::Private => "private".into(),
            Self::Public => "pub".into(),
            Self::Restricted(authored) => authored.clone(),
        }
    }
}

/// One bounded match produced by a Rift-owned Rust query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustQueryCapture {
    /// Query capture name.
    pub name: String,
    /// Captured syntax range.
    pub range: ByteRange,
}

/// Rift-owned adapter around Tree-sitter Rust queries.
#[derive(Debug)]
pub struct RustQuery {
    inner: TreeSitterQuery,
}

impl RustQuery {
    /// Compiles one query against pinned Rust grammar.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] when query cannot compile.
    pub fn new(source: &str) -> Result<Self, SyntaxError> {
        let language = rust_grammar();
        let inner = TreeSitterQuery::new(&language, source)
            .map_err(|error| invalid_query(source, error))?;
        Ok(Self { inner })
    }

    /// Returns query capture vocabulary.
    #[must_use]
    pub fn capture_names(&self) -> &[&str] {
        self.inner.capture_names()
    }

    /// Reports whether query declares capture name.
    #[must_use]
    pub fn has_capture(&self, name: &str) -> bool {
        self.capture_names().contains(&name)
    }

    /// Executes query with explicit source and capture bounds.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] for zero bound, incompatible grammar,
    /// cancellation, oversized source, or capture overflow.
    pub fn captures(
        &self,
        source: &str,
        captures_max: usize,
    ) -> Result<Vec<RustQueryCapture>, SyntaxError> {
        if captures_max == 0 {
            return Err(Error::new(SyntaxFault::ZeroLimit {
                bound: SyntaxBound::CapturesMax,
            }));
        }
        if source.len() > RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT {
            return Err(Error::new(SyntaxFault::SourceTooLarge {
                path: None,
                source_bytes: source.len(),
                source_bytes_max: RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
            }));
        }
        let mut parser = rust_parser()?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| Error::new(SyntaxFault::ParseCancelled { path: None }))?;
        let mut cursor = QueryCursor::new();
        let mut query_captures = cursor.captures(&self.inner, tree.root_node(), source.as_bytes());
        let mut captures = Vec::new();
        while let Some((query_match, capture_index)) = query_captures.next() {
            if captures.len() >= captures_max {
                return Err(Error::new(SyntaxFault::TooManyCaptures { captures_max }));
            }
            let capture = query_match.captures[*capture_index];
            captures.push(RustQueryCapture {
                name: self.inner.capture_names()[capture.index as usize].into(),
                range: extract::byte_range(capture.node)?,
            });
        }
        Ok(captures)
    }
}

/// Bounded Tree-sitter Rust fact provider.
#[derive(Debug, Clone)]
pub struct RustSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl RustSyntaxProvider {
    /// File extensions this provider parses, without their leading dot. The workspace walk
    /// includes a file as source only when some shipped provider declares its extension.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["rs"];

    /// Default maximum bytes this provider accepts from one Rust source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The Rust provider's declared default bounds, proven positive at compile
/// time.
const RUST_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for RustSyntaxProvider {
    fn default() -> Self {
        Self::new(RUST_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for RustSyntaxProvider {
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
        let mut parser = rust_parser()?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let (nodes, symbols) = extract::extract(
            tree.root_node(),
            source,
            self.limits,
            &self.language,
            &RustGrammarRules,
        )?;
        let document = SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            nodes,
            symbols,
            tree.root_node().has_error(),
        );
        Ok(
            match binding::unit_binding_facts(tree.root_node(), source, self.limits) {
                Some(facts) => document.with_binding(facts),
                None => document,
            },
        )
    }

    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        let mut facets = Vec::new();
        if kind.ends_with("_item") || kind.ends_with("_declaration") {
            facets.extend([NodeFacet::Declaration, NodeFacet::Definition]);
        }
        if kind.ends_with("_expression") {
            facets.push(NodeFacet::Expression);
        }
        if kind.ends_with("_statement") {
            facets.push(NodeFacet::Statement);
        }
        if kind.contains("comment") {
            facets.push(NodeFacet::Comment);
        }
        facets
    }

    fn binding_layout(&self, paths: &[&str]) -> Option<Box<dyn ModuleLayout + Send + Sync>> {
        Some(Box::new(RustCrateLayout::new(paths)))
    }
}

/// tree-sitter-rust's decisions for the shared bounded walk.
#[derive(Debug)]
struct RustGrammarRules;

impl GrammarRules for RustGrammarRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        let Some(kind) =
            RustGrammarNodeKind::from_kind(node.kind()).and_then(RustGrammarNodeKind::symbol_kind)
        else {
            return Ok(None);
        };
        let Some(name) = declaration_name(node, text) else {
            return Ok(None);
        };
        let visibility = declaration_visibility(node, text);
        let mut facets = declaration_facets(kind, &visibility);
        if is_entrypoint(node, kind, &name) {
            facets.push(SymbolFacet::Entrypoint);
        }
        Ok(Some(Declaration {
            name,
            kind: kind.word(),
            facets,
            visibility: Some(visibility.authored()),
            body_range: body_range(node, kind)?,
            documentation: attachment::attached_documentation(node, text),
        }))
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        match RustGrammarNodeKind::from_kind(node.kind())? {
            RustGrammarNodeKind::ModItem
            | RustGrammarNodeKind::StructItem
            | RustGrammarNodeKind::EnumItem
            | RustGrammarNodeKind::TraitItem => {
                let name = node.child_by_field_name(RustGrammarField::Name.as_str())?;
                text.get(name.byte_range()).map(Into::into)
            }
            RustGrammarNodeKind::ImplItem => {
                let item = node.child_by_field_name(RustGrammarField::Type.as_str())?;
                text.get(item.byte_range())
                    .map(|value| value.split_whitespace().collect::<String>())
            }
            _ => None,
        }
    }

    /// A declaration's start, extended over its attached outer attributes
    /// and outer doc comments so the whole declaration - not just the item
    /// node - is what `replace_symbol` and `insert_symbol` act on.
    fn declaration_start(&self, node: Node<'_>, text: &str) -> usize {
        attachment::declaration_start(node, text)
    }

    fn qualification_separator(&self) -> &'static str {
        "::"
    }
}

fn rust_grammar() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn rust_parser() -> Result<Parser, SyntaxError> {
    let language = rust_grammar();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| incompatible_grammar(&language))?;
    Ok(parser)
}

fn declaration_name(node: Node<'_>, text: &str) -> Option<String> {
    let name = node.child_by_field_name(RustGrammarField::Name.as_str())?;
    text.get(name.byte_range()).map(Into::into)
}

fn declaration_visibility(node: Node<'_>, text: &str) -> RustVisibility {
    (0..node.named_child_count())
        .filter_map(|index| node.named_child(index))
        .find(|child| child.kind() == RustGrammarNodeKind::VisibilityModifier.as_str())
        .and_then(|child| text.get(child.byte_range()))
        .map_or(RustVisibility::Private, RustVisibility::from_authored)
}

fn declaration_facets(kind: RustSymbolKind, visibility: &RustVisibility) -> Vec<SymbolFacet> {
    let mut facets = kind.facets();
    if visibility == &RustVisibility::Public {
        facets.push(SymbolFacet::Public);
    }
    facets
}

/// Numeric grammar ids for the ancestor kinds [`is_file_scope`] checks,
/// resolved once from the pinned Tree-sitter Rust language so each
/// classification compares integers instead of node-kind strings.
struct RustEnclosureKinds {
    module: u16,
    implementation: u16,
    function: u16,
}

impl RustEnclosureKinds {
    /// Resolves every kind id this module reads from `language`.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar has no node kind by one of the named
    /// spellings: `id_for_node_kind` returning `0` means the grammar the
    /// runtime loaded no longer defines a kind this module depends on, a
    /// programmer/grammar-version error rather than a reachable runtime
    /// state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        let mod_item = language.id_for_node_kind(RustGrammarNodeKind::ModItem.as_str(), true);
        let impl_item = language.id_for_node_kind(RustGrammarNodeKind::ImplItem.as_str(), true);
        let function_item =
            language.id_for_node_kind(RustGrammarNodeKind::FunctionItem.as_str(), true);
        assert!(
            mod_item != 0,
            "pinned Rust grammar must define node kind: kind=mod_item"
        );
        assert!(
            impl_item != 0,
            "pinned Rust grammar must define node kind: kind=impl_item"
        );
        assert!(
            function_item != 0,
            "pinned Rust grammar must define node kind: kind=function_item"
        );
        Self {
            module: mod_item,
            implementation: impl_item,
            function: function_item,
        }
    }
}

/// Returns the process-wide resolved [`RustEnclosureKinds`], computing it once.
fn rust_enclosure_kinds() -> &'static RustEnclosureKinds {
    static KINDS: OnceLock<RustEnclosureKinds> = OnceLock::new();
    KINDS.get_or_init(|| RustEnclosureKinds::resolve(&rust_grammar()))
}

/// Reports whether `node` is a direct child of the source file's root scope:
/// no ancestor is a `mod_item`, `impl_item`, or `function_item`. Bounded by
/// the parsed tree's depth, which [`SyntaxLimits`] caps during extraction.
fn is_file_scope(node: Node<'_>) -> bool {
    let kinds = rust_enclosure_kinds();
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        let kind_id = current.kind_id();
        if kind_id == kinds.module || kind_id == kinds.implementation || kind_id == kinds.function {
            return false;
        }
        ancestor = current.parent();
    }
    true
}

/// Reports whether `node` is a file-scope `fn main` - the binary
/// entrypoint - so [`SymbolFacet::Entrypoint`] applies. A `main` nested in
/// a `mod`, an `impl`, or another `fn` never qualifies.
fn is_entrypoint(node: Node<'_>, kind: RustSymbolKind, name: &str) -> bool {
    kind == RustSymbolKind::Function && name == "main" && is_file_scope(node)
}

/// The implementation part of one declaration: its grammar `body` or `value`
/// field's span. `None` when the kind declares no such field, or when this
/// node omits it - a unit struct, a `mod name;`, a valueless trait constant.
fn body_range(node: Node<'_>, kind: RustSymbolKind) -> Result<Option<ByteRange>, SyntaxError> {
    let Some(field) = kind.body_field() else {
        return Ok(None);
    };
    let Some(body) = node.child_by_field_name(field.as_str()) else {
        return Ok(None);
    };
    extract::byte_range(body).map(Some)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rift_core::{ErrorCode, ErrorContext, ErrorName, ProjectPath};

    use super::*;
    use crate::failure::{SyntaxViolation, position_overflow};

    fn path() -> ProjectPath {
        ProjectPath::new("src/lib.rs").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        RustSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("Rust fixture must parse")
    }

    #[test]
    fn test_provider_extracts_nested_rust_declarations_and_byte_nodes() {
        let text = "pub mod café {\r\npub(crate) struct Item;\r\nimpl Item { fn load() {} }\r\n}";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["café", "café::Item", "café::Item::load"]);
        assert_eq!(document.symbols()[0].visibility.as_deref(), Some("pub"));
        assert_eq!(
            document.symbols()[1].visibility.as_deref(),
            Some("pub(crate)")
        );
        assert_eq!(document.symbols()[2].visibility.as_deref(), Some("private"));
        assert_eq!(document.path(), &path());
        assert_eq!(document.language().name, "rust");
        let load = text.find("load").expect("fixture contains method") as u64;
        assert!(document.nodes_at(load).len() >= 3);
        assert!(!document.has_errors());
    }

    #[test]
    fn test_document_container_names_the_containing_scope_or_none_at_top_level() {
        let text = "pub mod café {\npub struct Item;\nimpl Item { fn load() {} }\n}";
        let document = analyze(text);
        let containers = document
            .symbols()
            .iter()
            .map(|symbol| symbol.container.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(containers, [None, Some("café"), Some("café::Item")]);
    }

    #[test]
    fn test_document_kind_words_cover_every_declaration_kind() {
        let text = "fn f() {}\nstruct S;\nenum E {}\ntrait T {}\ntype A = u8;\n\
                    const C: u8 = 0;\nstatic G: u8 = 0;\nmod m {}\nmacro_rules! q { () => {}; }";
        let document = analyze(text);
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| symbol.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "function",
                "struct",
                "enum",
                "trait",
                "type_alias",
                "constant",
                "static",
                "module",
                "macro",
            ]
        );
    }

    /// A callable declaration with a body renders one signature whose `display` is the
    /// header text - the source from the item's own start to where the body begins, trimmed
    /// of trailing whitespace - and carries the document's language.
    #[test]
    fn test_a_function_with_a_body_renders_one_signature_from_its_header() {
        let document = analyze("pub fn compute(x: i32) -> i32 {\n    x\n}\n");
        let function = &document.symbols()[0];
        assert_eq!(function.signatures.len(), 1);
        assert_eq!(
            function.signatures[0].display,
            "pub fn compute(x: i32) -> i32"
        );
        assert_eq!(function.signatures[0].language.name, "rust");
    }

    /// A kind the grammar declares no `Callable` facet for never renders a signature, even
    /// when it carries a `body_range` - a struct's braces are not a callable implementation.
    #[test]
    fn test_a_non_callable_declaration_with_a_body_range_renders_no_signature() {
        let document = analyze("pub struct Beacon { field: u8 }\n");
        assert!(document.symbols()[0].signatures.is_empty());
    }

    /// Every function in a file renders its own signature; nothing bleeds between them.
    #[test]
    fn test_every_function_in_a_document_renders_its_own_signature() {
        let document = analyze("pub fn one() {}\npub fn two(x: u8) -> u8 { x }\n");
        let signatures: Vec<&str> = document
            .symbols()
            .iter()
            .map(|symbol| symbol.signatures[0].display.as_str())
            .collect();
        assert_eq!(signatures, ["pub fn one()", "pub fn two(x: u8) -> u8"]);
    }

    #[test]
    fn test_document_facets_render_kind_categories_and_public_visibility() {
        let document =
            analyze("pub fn f() {}\ntype A = u8;\nmod m {}\nmacro_rules! q { () => {}; }");
        let facets = document
            .symbols()
            .iter()
            .map(|symbol| symbol.facets.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            facets,
            [
                vec![
                    SymbolFacet::Value,
                    SymbolFacet::Callable,
                    SymbolFacet::Public
                ],
                vec![SymbolFacet::Type, SymbolFacet::Alias],
                vec![SymbolFacet::Namespace, SymbolFacet::Module],
                vec![SymbolFacet::Macro],
            ]
        );
    }

    /// Every declaration kind whose grammar declares a body or value field
    /// carries `body_range`; a kind or node without one carries `None`.
    #[test]
    fn test_document_body_range_present_exactly_where_the_grammar_declares_one() {
        let text = "fn f() { 1; }\nstruct S { x: u8 }\nstruct Unit;\nenum E { A }\n\
                    trait T { fn t(&self); }\nmod m { fn inner() {} }\nmod declared;\n\
                    const C: u8 = 7;\nstatic G: u8 = 9;\ntype A = u8;\n\
                    macro_rules! q { () => {}; }";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("f", true),
                ("S", true),
                ("Unit", false),
                ("E", true),
                ("T", true),
                ("m", true),
                ("inner", true),
                ("declared", false),
                ("C", true),
                ("G", true),
                ("A", false),
                ("q", false),
            ]
        );
        let function = &document.symbols()[0];
        let body = function.body_range.expect("fn f owns a body");
        let start = usize::try_from(body.start).expect("fixture span fits usize");
        let end = usize::try_from(body.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "{ 1; }");
    }

    #[test]
    fn test_provider_node_facets_classify_declaration_expression_statement_and_comment() {
        let provider = RustSyntaxProvider::default();
        assert_eq!(
            provider.node_facets("function_item"),
            [NodeFacet::Declaration, NodeFacet::Definition]
        );
        assert_eq!(
            provider.node_facets("binary_expression"),
            [NodeFacet::Expression]
        );
        assert_eq!(
            provider.node_facets("expression_statement"),
            [NodeFacet::Statement]
        );
        assert_eq!(provider.node_facets("line_comment"), [NodeFacet::Comment]);
        assert_eq!(provider.node_facets("identifier"), []);
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = RustSyntaxProvider::default();
        assert_eq!(provider.language().name, "rust");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["rs"]);
        assert_eq!(
            provider.source_bytes_max(),
            RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    #[test]
    fn test_query_adapter_owns_capture_vocabulary_and_bounds() {
        let query = RustQuery::new("(function_item name: (identifier) @rift.name)")
            .expect("valid Rust query");
        assert_eq!(query.capture_names(), &["rift.name"]);
        assert!(query.has_capture("rift.name"));
        assert!(!query.has_capture("rift.missing"));
        let captures = query
            .captures("fn first() {} fn second() {}", 2)
            .expect("capture bound accepts both functions");
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].name, "rift.name");
        assert_eq!(
            query
                .captures("fn first() {} fn second() {}", 1)
                .expect_err("capture overflow must fail")
                .fault()
                .violation(),
            SyntaxViolation::TooManyCaptures
        );
        assert_eq!(
            RustQuery::new("(missing_node) @rift.name")
                .expect_err("invalid node kind must fail")
                .fault()
                .violation(),
            SyntaxViolation::InvalidQuery
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("fn broken( {");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_node_depth_and_positive_limits() {
        assert_eq!(
            SyntaxLimits::new(0, 1, 1)
                .expect_err("zero limit")
                .fault()
                .violation(),
            SyntaxViolation::ZeroLimit,
        );
        let source_error =
            RustSyntaxProvider::new(SyntaxLimits::new(3, 10, 10).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error =
            RustSyntaxProvider::new(SyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error =
            RustSyntaxProvider::new(SyntaxLimits::new(100, 20, 1).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                })
                .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    #[test]
    fn test_grammar_node_kind_round_trips_and_rejects_unknown() {
        for kind in RustGrammarNodeKind::ALL {
            assert_eq!(
                kind.as_str()
                    .parse::<RustGrammarNodeKind>()
                    .expect("known kind"),
                kind
            );
        }
        let error = "flumph_item"
            .parse::<RustGrammarNodeKind>()
            .expect_err("unknown kind");
        assert_eq!(error.fault().violation(), SyntaxViolation::UnknownNodeKind);
        assert_eq!(
            error.descriptor().name(),
            ErrorName::Wire(ErrorCode::InternalError)
        );
        assert!(error.source().is_none());
        assert_eq!(
            error.to_string(),
            "the server failed in a way it did not classify: \
             node_kind flumph_item; \
             retry once, and report the full message if the failure repeats"
        );
    }

    #[test]
    fn test_visibility_from_authored_distinguishes_public_and_restricted() {
        assert_eq!(RustVisibility::from_authored("pub"), RustVisibility::Public);
        assert_eq!(
            RustVisibility::from_authored("pub(super)"),
            RustVisibility::Restricted("pub(super)".into())
        );
    }

    #[test]
    fn test_zero_limit_error_names_offending_bound_and_configuration_site() {
        let cases = [
            (SyntaxLimits::new(0, 1, 1), "source_bytes_max"),
            (SyntaxLimits::new(1, 0, 1), "syntax_nodes_max"),
            (SyntaxLimits::new(1, 1, 0), "syntax_depth_max"),
        ];
        for (result, bound_name) in cases {
            let error = result.expect_err("zero bound");
            assert_eq!(
                error.descriptor().name(),
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "the workspace configuration failed validation: bound {bound_name}; \
                     correct the reported configuration field, then retry"
                )
            );
        }
        let query = RustQuery::new("(function_item) @rift.item").expect("valid Rust query");
        let error = query
            .captures("fn a() {}", 0)
            .expect_err("zero captures_max");
        assert_eq!(error.fault().violation(), SyntaxViolation::ZeroLimit);
        assert_eq!(
            error.to_string(),
            "the workspace configuration failed validation: bound captures_max; \
             correct the reported configuration field, then retry"
        );
    }

    #[test]
    fn test_source_too_large_error_reports_sizes_path_and_limit_origin() {
        let error = RustSyntaxProvider::new(SyntaxLimits::new(3, 10, 10).expect("positive limits"))
            .analyze(SyntaxSource {
                path: &path(),
                text: "fn x() {}",
            })
            .expect_err("source bound");
        assert_eq!(
            error.descriptor().name(),
            ErrorName::Wire(ErrorCode::LimitExceeded)
        );
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: \
             path src/lib.rs, source_bytes 9, source_bytes_max 3; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );

        let query = RustQuery::new("(function_item) @rift.item").expect("valid Rust query");
        let oversized = "a".repeat(RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT + 1);
        let error = query.captures(&oversized, 1).expect_err("oversized source");
        assert_eq!(error.fault().violation(), SyntaxViolation::SourceTooLarge);
        assert_eq!(
            error.to_string(),
            format!(
                "the request exceeded a declared resource limit: \
                 path <raw text>, source_bytes {bytes}, source_bytes_max {max}; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
                bytes = RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT + 1,
                max = RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
            )
        );
    }

    #[test]
    fn test_tree_bound_errors_report_path_and_configured_limit() {
        let node_error =
            RustSyntaxProvider::new(SyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("node bound");
        assert_eq!(
            node_error.to_string(),
            "the request exceeded a declared resource limit: \
             path src/lib.rs, syntax_nodes_max 1; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );

        let depth_error =
            RustSyntaxProvider::new(SyntaxLimits::new(100, 20, 1).expect("positive limits"))
                .analyze(SyntaxSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                })
                .expect_err("depth bound");
        assert_eq!(
            depth_error.to_string(),
            "the request exceeded a declared resource limit: \
             path src/lib.rs, syntax_depth_max 1; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
    }

    #[test]
    fn test_query_errors_report_line_reason_and_capture_limit() {
        let error = RustQuery::new("(missing_node) @rift.name").expect_err("invalid node kind");
        assert!(error.source().is_some(), "keeps tree-sitter source");
        assert_eq!(
            error.descriptor().name(),
            ErrorName::Wire(ErrorCode::InternalError)
        );
        assert_eq!(
            error.to_string(),
            "the server failed in a way it did not classify: \
             line_number 1, line_text (missing_node) @rift.name; \
             retry once, and report the full message if the failure repeats"
        );

        let query = RustQuery::new("(function_item name: (identifier) @rift.name)")
            .expect("valid Rust query");
        let error = query
            .captures("fn first() {} fn second() {}", 1)
            .expect_err("capture overflow");
        assert_eq!(
            error.descriptor().name(),
            ErrorName::Wire(ErrorCode::LimitExceeded)
        );
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: \
             captures_max 1; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
    }

    #[test]
    fn test_invalid_query_reason_covers_every_tree_sitter_error_kind() {
        use tree_sitter::QueryErrorKind;

        let cases = [
            ("(((", QueryErrorKind::Syntax),
            (
                "(function_item flumph: (identifier) @x)",
                QueryErrorKind::Field,
            ),
            ("((function_item) @x (#eq? @y @y))", QueryErrorKind::Capture),
            ("((function_item) @x (#eq?))", QueryErrorKind::Predicate),
            ("(identifier (identifier))", QueryErrorKind::Structure),
        ];
        for (query, kind) in cases {
            let error = RustQuery::new(query).expect_err("query must be rejected");
            assert_eq!(error.fault().violation(), SyntaxViolation::InvalidQuery);
            match error.fault() {
                SyntaxFault::InvalidQuery { source, .. } => {
                    assert_eq!(source.kind, kind, "classifies {query}");
                }
                other => panic!("expected InvalidQuery, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_runtime_only_errors_render_full_context() {
        let grammar = incompatible_grammar(&rust_grammar());
        assert_eq!(
            grammar.fault().violation(),
            SyntaxViolation::IncompatibleGrammar
        );
        assert_eq!(
            grammar.descriptor().name(),
            ErrorName::Wire(ErrorCode::InternalError)
        );
        assert_eq!(
            grammar.to_string(),
            format!(
                "the server failed in a way it did not classify: \
                 grammar_abi_version {abi}, runtime_abi_min {min}, runtime_abi_max {max}; \
                 retry once, and report the full message if the failure repeats",
                abi = rust_grammar().abi_version(),
                min = tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
                max = tree_sitter::LANGUAGE_VERSION,
            )
        );

        let cancelled = SyntaxError::new(SyntaxFault::ParseCancelled { path: Some(path()) });
        assert_eq!(
            cancelled.fault().violation(),
            SyntaxViolation::ParseCancelled
        );
        assert_eq!(
            cancelled.descriptor().name(),
            ErrorName::Wire(ErrorCode::Cancelled)
        );
        assert!(cancelled.source().is_none());
        assert_eq!(
            cancelled.to_string(),
            "the request was cancelled before it completed: path src/lib.rs; \
             resend the request if the result is still needed"
        );
        let cancelled_query = SyntaxError::new(SyntaxFault::ParseCancelled { path: None });
        assert_eq!(
            cancelled_query.to_string(),
            "the request was cancelled before it completed; \
             resend the request if the result is still needed"
        );

        let mut parser = rust_parser().expect("pinned grammar loads");
        let tree = parser.parse("fn x() {}", None).expect("fixture parses");
        let overflow = position_overflow(
            tree.root_node(),
            u32::try_from(u64::MAX).expect_err("u64::MAX exceeds u32"),
        );
        assert_eq!(
            overflow.fault().violation(),
            SyntaxViolation::PositionOverflow
        );
        assert_eq!(
            overflow.descriptor().name(),
            ErrorName::Wire(ErrorCode::InternalError)
        );
        assert!(overflow.source().is_some(), "keeps conversion source");
        assert_eq!(
            overflow.to_string(),
            "the server failed in a way it did not classify: \
             node_kind source_file, start_byte 0, end_byte 9; \
             retry once, and report the full message if the failure repeats"
        );
    }

    #[test]
    fn test_unknown_node_kind_and_zero_limit_report_code_and_context() {
        let unknown = "bogus"
            .parse::<RustGrammarNodeKind>()
            .expect_err("unregistered kind");
        assert_eq!(
            unknown.fault().violation(),
            SyntaxViolation::UnknownNodeKind
        );
        assert_eq!(unknown.descriptor().code(), "internal_error");
        assert_eq!(
            unknown.context(),
            vec![ErrorContext::new("node_kind", "bogus")]
        );

        let zero = SyntaxLimits::new(0, 1, 1).expect_err("zero source bound");
        assert_eq!(zero.fault().violation(), SyntaxViolation::ZeroLimit);
        assert_eq!(zero.descriptor().code(), "configuration_invalid");
        assert_eq!(
            zero.context(),
            vec![ErrorContext::new("bound", "source_bytes_max")]
        );
    }

    #[test]
    fn test_file_scope_main_carries_the_entrypoint_facet() {
        let document = analyze("fn main() {}\n");
        let main = &document.symbols()[0];
        assert_eq!(main.name, "main");
        assert!(main.facets.contains(&SymbolFacet::Entrypoint));
    }

    #[test]
    fn test_main_nested_in_a_module_does_not_carry_the_entrypoint_facet() {
        let document = analyze("mod tests {\n    fn main() {}\n}\n");
        let main = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "main");
        let main = main.expect("fixture declares a nested main");
        assert!(!main.facets.contains(&SymbolFacet::Entrypoint));
    }

    #[test]
    fn test_a_method_named_main_does_not_carry_the_entrypoint_facet() {
        let document = analyze("struct S;\nimpl S {\n    fn main(&self) {}\n}\n");
        let main = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "main");
        let main = main.expect("fixture declares a method named main");
        assert!(!main.facets.contains(&SymbolFacet::Entrypoint));
    }
}
