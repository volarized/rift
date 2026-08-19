use std::fmt;

use rift_core::ProjectPath;
use tree_sitter::{Node, Parser, Query as TreeSitterQuery, QueryCursor, StreamingIterator};

const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1024 * 1024;
const SYNTAX_NODES_MAX_DEFAULT: usize = 250_000;
const SYNTAX_DEPTH_MAX_DEFAULT: usize = 512;

/// Half-open UTF-8 byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    /// First included byte.
    pub start: u64,
    /// First excluded byte.
    pub end: u64,
}

impl ByteRange {
    fn from_node(node: Node<'_>) -> Result<Self, RustSyntaxError> {
        let start = u64::try_from(node.start_byte()).map_err(|source| {
            RustSyntaxError::with_source(RustSyntaxViolation::PositionOverflow, source)
        })?;
        let end = u64::try_from(node.end_byte()).map_err(|source| {
            RustSyntaxError::with_source(RustSyntaxViolation::PositionOverflow, source)
        })?;
        Ok(Self { start, end })
    }

    /// Reports whether byte position belongs to range.
    #[must_use]
    pub const fn contains(self, position: u64) -> bool {
        self.start <= position && position < self.end
    }
}

/// Rust source admitted to sans-I/O syntax analysis.
#[derive(Debug, Clone, Copy)]
pub struct RustSource<'a> {
    /// Canonical project-relative path.
    pub path: &'a ProjectPath,
    /// UTF-8 source text.
    pub text: &'a str,
}

/// Bounded Rust syntax admission limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct RustSyntaxLimits {
    source_bytes_max: usize,
    syntax_nodes_max: usize,
    syntax_depth_max: usize,
}

impl RustSyntaxLimits {
    /// Constructs positive syntax bounds.
    ///
    /// # Errors
    ///
    /// Returns [`RustSyntaxError`] when any bound is zero.
    pub fn new(
        source_bytes_max: usize,
        syntax_nodes_max: usize,
        syntax_depth_max: usize,
    ) -> Result<Self, RustSyntaxError> {
        if source_bytes_max == 0 || syntax_nodes_max == 0 || syntax_depth_max == 0 {
            return Err(RustSyntaxError::new(RustSyntaxViolation::ZeroLimit));
        }
        Ok(Self {
            source_bytes_max,
            syntax_nodes_max,
            syntax_depth_max,
        })
    }
}

impl Default for RustSyntaxLimits {
    fn default() -> Self {
        Self {
            source_bytes_max: SOURCE_BYTES_MAX_DEFAULT,
            syntax_nodes_max: SYNTAX_NODES_MAX_DEFAULT,
            syntax_depth_max: SYNTAX_DEPTH_MAX_DEFAULT,
        }
    }
}

/// Rust declaration kind emitted by Tree-sitter provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RustSymbolKind {
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

/// Authored visibility of one Rust declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustVisibility {
    /// Declaration has no visibility modifier.
    Private,
    /// Declaration uses `pub`.
    Public,
    /// Declaration uses exact restricted visibility spelling such as `pub(crate)`.
    Restricted(String),
}

/// One named Rust declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustSymbol {
    /// Declared short name.
    pub name: String,
    /// Module/type-qualified name within file.
    pub qualified_name: String,
    /// Declaration category.
    pub kind: RustSymbolKind,
    /// Authored declaration visibility.
    pub visibility: RustVisibility,
    /// Complete declaration byte range.
    pub range: ByteRange,
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
pub struct RustQuery {
    inner: TreeSitterQuery,
}

impl RustQuery {
    /// Compiles one query against pinned Rust grammar.
    ///
    /// # Errors
    ///
    /// Returns [`RustSyntaxError`] when query cannot compile.
    pub fn new(source: &str) -> Result<Self, RustSyntaxError> {
        let language = rust_language();
        let inner = TreeSitterQuery::new(&language, source).map_err(|error| {
            RustSyntaxError::with_source(RustSyntaxViolation::InvalidQuery, error)
        })?;
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
    /// Returns [`RustSyntaxError`] for zero bound, incompatible grammar,
    /// cancellation, oversized source, or capture overflow.
    pub fn captures(
        &self,
        source: &str,
        captures_max: usize,
    ) -> Result<Vec<RustQueryCapture>, RustSyntaxError> {
        if captures_max == 0 {
            return Err(RustSyntaxError::new(RustSyntaxViolation::ZeroLimit));
        }
        if source.len() > SOURCE_BYTES_MAX_DEFAULT {
            return Err(RustSyntaxError::new(RustSyntaxViolation::SourceTooLarge));
        }
        let language = rust_language();
        let mut parser = Parser::new();
        parser.set_language(&language).map_err(|error| {
            RustSyntaxError::with_source(RustSyntaxViolation::IncompatibleGrammar, error)
        })?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| RustSyntaxError::new(RustSyntaxViolation::ParseCancelled))?;
        let mut cursor = QueryCursor::new();
        let mut query_captures = cursor.captures(&self.inner, tree.root_node(), source.as_bytes());
        let mut captures = Vec::new();
        while let Some((query_match, capture_index)) = query_captures.next() {
            if captures.len() >= captures_max {
                return Err(RustSyntaxError::new(RustSyntaxViolation::TooManyCaptures));
            }
            let capture = query_match.captures[*capture_index];
            captures.push(RustQueryCapture {
                name: self.inner.capture_names()[capture.index as usize].into(),
                range: ByteRange::from_node(capture.node)?,
            });
        }
        Ok(captures)
    }
}

/// One named Tree-sitter node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustNode {
    /// Grammar node kind.
    pub kind: String,
    /// Node byte range.
    pub range: ByteRange,
    /// Parent index in document node vector.
    pub parent: Option<usize>,
    /// Whether parser marked node erroneous or missing.
    pub has_error: bool,
}

/// Immutable syntax facts for one Rust source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustSyntaxDocument {
    path: ProjectPath,
    nodes: Vec<RustNode>,
    symbols: Vec<RustSymbol>,
    has_errors: bool,
}

impl RustSyntaxDocument {
    /// Returns source path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns every named syntax node in pre-order.
    #[must_use]
    pub fn nodes(&self) -> &[RustNode] {
        &self.nodes
    }

    /// Returns extracted declarations in source order.
    #[must_use]
    pub fn symbols(&self) -> &[RustSymbol] {
        &self.symbols
    }

    /// Reports whether parser observed malformed syntax.
    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Returns nodes covering byte position, outermost first.
    #[must_use]
    pub fn nodes_at(&self, position: u64) -> Vec<&RustNode> {
        self.nodes
            .iter()
            .filter(|node| node.range.contains(position))
            .collect()
    }
}

/// Stable Rust syntax failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustSyntaxViolation {
    /// Limit was configured as zero.
    ZeroLimit,
    /// Source exceeds byte bound.
    SourceTooLarge,
    /// Syntax tree exceeds node bound.
    TooManyNodes,
    /// Syntax tree exceeds depth bound.
    TooDeep,
    /// Grammar cannot be loaded by runtime.
    IncompatibleGrammar,
    /// Parser produced no tree.
    ParseCancelled,
    /// Platform position cannot fit wire width.
    PositionOverflow,
    /// Query is invalid for pinned Rust grammar.
    InvalidQuery,
    /// Query produced more captures than admitted.
    TooManyCaptures,
}

/// Opaque Rust syntax failure.
#[derive(Debug)]
pub struct RustSyntaxError {
    violation: RustSyntaxViolation,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl RustSyntaxError {
    fn new(violation: RustSyntaxViolation) -> Self {
        Self {
            violation,
            source: None,
        }
    }

    fn with_source(
        violation: RustSyntaxViolation,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            violation,
            source: Some(Box::new(source)),
        }
    }

    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> RustSyntaxViolation {
        self.violation
    }
}

impl fmt::Display for RustSyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.violation {
            RustSyntaxViolation::ZeroLimit => "Rust syntax limits must be positive",
            RustSyntaxViolation::SourceTooLarge => "Rust source exceeds syntax byte limit",
            RustSyntaxViolation::TooManyNodes => "Rust syntax tree exceeds node limit",
            RustSyntaxViolation::TooDeep => "Rust syntax tree exceeds depth limit",
            RustSyntaxViolation::IncompatibleGrammar => "Rust grammar is incompatible",
            RustSyntaxViolation::ParseCancelled => "Rust parse was cancelled",
            RustSyntaxViolation::PositionOverflow => "Rust syntax position exceeds wire width",
            RustSyntaxViolation::InvalidQuery => "Rust syntax query is invalid",
            RustSyntaxViolation::TooManyCaptures => "Rust syntax query exceeds capture limit",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RustSyntaxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Bounded Tree-sitter Rust fact provider.
#[derive(Debug, Clone, Copy)]
pub struct RustSyntaxProvider {
    limits: RustSyntaxLimits,
}

impl RustSyntaxProvider {
    /// Constructs provider with explicit bounds.
    #[must_use]
    pub const fn new(limits: RustSyntaxLimits) -> Self {
        Self { limits }
    }

    /// Parses source and extracts named nodes and declarations.
    ///
    /// # Errors
    ///
    /// Returns [`RustSyntaxError`] for incompatible grammar, cancellation, or exceeded bound.
    pub fn analyze(&self, source: RustSource<'_>) -> Result<RustSyntaxDocument, RustSyntaxError> {
        if source.text.len() > self.limits.source_bytes_max {
            return Err(RustSyntaxError::new(RustSyntaxViolation::SourceTooLarge));
        }
        let language = rust_language();
        let mut parser = Parser::new();
        parser.set_language(&language).map_err(|error| {
            RustSyntaxError::with_source(RustSyntaxViolation::IncompatibleGrammar, error)
        })?;
        let tree = parser
            .parse(source.text, None)
            .ok_or_else(|| RustSyntaxError::new(RustSyntaxViolation::ParseCancelled))?;
        let (nodes, symbols) = self.extract(tree.root_node(), source.text)?;
        Ok(RustSyntaxDocument {
            path: source.path.clone(),
            nodes,
            symbols,
            has_errors: tree.root_node().has_error(),
        })
    }

    fn extract(
        &self,
        root: Node<'_>,
        text: &str,
    ) -> Result<(Vec<RustNode>, Vec<RustSymbol>), RustSyntaxError> {
        let mut nodes = Vec::new();
        let mut symbols = Vec::new();
        let mut pending = vec![(root, None, String::new(), 0_usize)];
        while let Some((node, parent, qualification, depth)) = pending.pop() {
            if depth > self.limits.syntax_depth_max {
                return Err(RustSyntaxError::new(RustSyntaxViolation::TooDeep));
            }
            if nodes.len() >= self.limits.syntax_nodes_max {
                return Err(RustSyntaxError::new(RustSyntaxViolation::TooManyNodes));
            }

            let node_index = nodes.len();
            let range = ByteRange::from_node(node)?;
            nodes.push(RustNode {
                kind: node.kind().into(),
                range,
                parent,
                has_error: node.is_error() || node.is_missing(),
            });

            let name = declaration_name(node, text);
            if let Some((name, kind)) = name.as_ref() {
                symbols.push(RustSymbol {
                    name: name.clone(),
                    qualified_name: qualify(&qualification, name),
                    kind: *kind,
                    visibility: declaration_visibility(node, text),
                    range,
                });
            }
            let child_qualification = container_name(node, text).map_or_else(
                || qualification.clone(),
                |name| qualify(&qualification, &name),
            );

            for child_index in (0..node.child_count()).rev() {
                let Some(child) = node.child(child_index) else {
                    continue;
                };
                if !child.is_named() {
                    continue;
                }
                if pending.len() + nodes.len() >= self.limits.syntax_nodes_max {
                    return Err(RustSyntaxError::new(RustSyntaxViolation::TooManyNodes));
                }
                pending.push((
                    child,
                    Some(node_index),
                    child_qualification.clone(),
                    depth + 1,
                ));
            }
        }
        Ok((nodes, symbols))
    }
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

impl Default for RustSyntaxProvider {
    fn default() -> Self {
        Self::new(RustSyntaxLimits::default())
    }
}

fn declaration_name(node: Node<'_>, text: &str) -> Option<(String, RustSymbolKind)> {
    let kind = match node.kind() {
        "function_item" => RustSymbolKind::Function,
        "struct_item" => RustSymbolKind::Struct,
        "enum_item" => RustSymbolKind::Enum,
        "trait_item" => RustSymbolKind::Trait,
        "type_item" => RustSymbolKind::TypeAlias,
        "const_item" => RustSymbolKind::Constant,
        "static_item" => RustSymbolKind::Static,
        "mod_item" => RustSymbolKind::Module,
        "macro_definition" => RustSymbolKind::Macro,
        _ => return None,
    };
    let name = node.child_by_field_name("name")?;
    text.get(name.byte_range())
        .map(|value| (value.into(), kind))
}

fn declaration_visibility(node: Node<'_>, text: &str) -> RustVisibility {
    let visibility = (0..node.named_child_count())
        .filter_map(|index| node.named_child(index))
        .find(|child| child.kind() == "visibility_modifier")
        .and_then(|child| text.get(child.byte_range()));
    match visibility {
        None => RustVisibility::Private,
        Some("pub") => RustVisibility::Public,
        Some(authored) => RustVisibility::Restricted(authored.into()),
    }
}

fn container_name(node: Node<'_>, text: &str) -> Option<String> {
    match node.kind() {
        "mod_item" | "struct_item" | "enum_item" | "trait_item" => {
            let name = node.child_by_field_name("name")?;
            text.get(name.byte_range()).map(Into::into)
        }
        "impl_item" => {
            let item = node.child_by_field_name("type")?;
            text.get(item.byte_range())
                .map(|value| value.split_whitespace().collect::<String>())
        }
        _ => None,
    }
}

fn qualify(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}::{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> ProjectPath {
        ProjectPath::new("src/lib.rs").expect("valid fixture path")
    }

    #[test]
    fn test_provider_extracts_nested_rust_declarations_and_byte_nodes() {
        let text = "pub mod café {\r\npub(crate) struct Item;\r\nimpl Item { fn load() {} }\r\n}";
        let document = RustSyntaxProvider::default()
            .analyze(RustSource {
                path: &path(),
                text,
            })
            .expect("Rust fixture must parse");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["café", "café::Item", "café::Item::load"]);
        assert_eq!(document.symbols()[0].visibility, RustVisibility::Public);
        assert_eq!(
            document.symbols()[1].visibility,
            RustVisibility::Restricted("pub(crate)".into())
        );
        assert_eq!(document.symbols()[2].visibility, RustVisibility::Private);
        assert_eq!(document.path(), &path());
        let load = text.find("load").expect("fixture contains method") as u64;
        assert!(document.nodes_at(load).len() >= 3);
        assert!(!document.has_errors());
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
            .expect("capture bound admits both functions");
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].name, "rift.name");
        assert_eq!(
            query
                .captures("fn first() {} fn second() {}", 1)
                .expect_err("capture overflow must fail")
                .violation(),
            RustSyntaxViolation::TooManyCaptures
        );
        assert_eq!(
            RustQuery::new("(missing_node) @rift.name")
                .err()
                .expect("invalid node kind must fail")
                .violation(),
            RustSyntaxViolation::InvalidQuery
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = RustSyntaxProvider::default()
            .analyze(RustSource {
                path: &path(),
                text: "fn broken( {",
            })
            .expect("Tree-sitter returns recoverable malformed tree");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_node_depth_and_positive_limits() {
        assert_eq!(
            RustSyntaxLimits::new(0, 1, 1)
                .expect_err("zero limit")
                .violation(),
            RustSyntaxViolation::ZeroLimit,
        );
        let source_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(3, 10, 10).expect("positive limits"))
                .analyze(RustSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("source bound");
        assert_eq!(
            source_error.violation(),
            RustSyntaxViolation::SourceTooLarge
        );

        let node_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(RustSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("node bound");
        assert_eq!(node_error.violation(), RustSyntaxViolation::TooManyNodes);

        let depth_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 20, 1).expect("positive limits"))
                .analyze(RustSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                })
                .expect_err("depth bound");
        assert_eq!(depth_error.violation(), RustSyntaxViolation::TooDeep);
    }
}
