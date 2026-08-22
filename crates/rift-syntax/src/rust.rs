use rift_core::ProjectPath;
use rift_core::constants::RUST_SOURCE_BYTES_MAX_DEFAULT;
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;
use tree_sitter::{
    Node, Parser, Query as TreeSitterQuery, QueryCursor, QueryError, StreamingIterator,
};

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
        let start =
            u64::try_from(node.start_byte()).map_err(|source| position_overflow(node, source))?;
        let end =
            u64::try_from(node.end_byte()).map_err(|source| position_overflow(node, source))?;
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
    /// Returns [`RustSyntaxError`] naming the first zero bound.
    pub fn new(
        source_bytes_max: usize,
        syntax_nodes_max: usize,
        syntax_depth_max: usize,
    ) -> Result<Self, RustSyntaxError> {
        let bounds = [
            (source_bytes_max, RustSyntaxBound::SourceBytesMax),
            (syntax_nodes_max, RustSyntaxBound::SyntaxNodesMax),
            (syntax_depth_max, RustSyntaxBound::SyntaxDepthMax),
        ];
        for (value, bound) in bounds {
            if value == 0 {
                return Err(Error::new(RustSyntaxFault::ZeroLimit { bound }));
            }
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
            source_bytes_max: RUST_SOURCE_BYTES_MAX_DEFAULT,
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

/// Tree-sitter grammar node kind interpreted by this crate.
///
/// Vocabulary comes from the `node-types.json` of the pinned
/// tree-sitter-rust 0.24.2 grammar. The grammar defines many more kinds;
/// only the ones this crate reads are listed, so conversion from an
/// arbitrary kind string is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustGrammarNodeKind {
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
    pub const ALL: [Self; 11] = [
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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
    type Err = RustSyntaxError;

    fn from_str(kind: &str) -> Result<Self, Self::Err> {
        Self::from_kind(kind)
            .ok_or_else(|| Error::new(RustSyntaxFault::UnknownNodeKind { kind: kind.into() }))
    }
}

/// Grammar field name this crate reads, from tree-sitter-rust 0.24.2
/// `node-types.json`.
#[derive(Debug, Clone, Copy)]
enum RustGrammarField {
    /// `name` field on declaration items.
    Name,
    /// `type` field on `impl_item`.
    Type,
}

impl RustGrammarField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Type => "type",
        }
    }
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

impl RustVisibility {
    /// Classifies authored `visibility_modifier` text from tree-sitter-rust grammar.
    ///
    /// Bare `pub` is [`RustVisibility::Public`]; any other authored spelling
    /// such as `pub(crate)` or `pub(in path)` stays verbatim in
    /// [`RustVisibility::Restricted`]. Declarations without a modifier never
    /// reach this conversion and stay [`RustVisibility::Private`].
    #[must_use]
    pub fn from_authored(text: &str) -> Self {
        match text {
            "pub" => Self::Public,
            restricted => Self::Restricted(restricted.into()),
        }
    }
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
#[derive(Debug)]
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
    /// Returns [`RustSyntaxError`] for zero bound, incompatible grammar,
    /// cancellation, oversized source, or capture overflow.
    pub fn captures(
        &self,
        source: &str,
        captures_max: usize,
    ) -> Result<Vec<RustQueryCapture>, RustSyntaxError> {
        if captures_max == 0 {
            return Err(Error::new(RustSyntaxFault::ZeroLimit {
                bound: RustSyntaxBound::CapturesMax,
            }));
        }
        if source.len() > RUST_SOURCE_BYTES_MAX_DEFAULT {
            return Err(Error::new(RustSyntaxFault::SourceTooLarge {
                path: None,
                source_bytes: source.len(),
                source_bytes_max: RUST_SOURCE_BYTES_MAX_DEFAULT,
            }));
        }
        let mut parser = rust_parser()?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| Error::new(RustSyntaxFault::ParseCancelled { path: None }))?;
        let mut cursor = QueryCursor::new();
        let mut query_captures = cursor.captures(&self.inner, tree.root_node(), source.as_bytes());
        let mut captures = Vec::new();
        while let Some((query_match, capture_index)) = query_captures.next() {
            if captures.len() >= captures_max {
                return Err(Error::new(RustSyntaxFault::TooManyCaptures {
                    captures_max,
                }));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
    /// Node kind is outside interpreted grammar vocabulary.
    UnknownNodeKind,
}

/// Configurable Rust syntax bound named in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RustSyntaxBound {
    /// Admitted source bytes.
    SourceBytesMax,
    /// Admitted syntax nodes.
    SyntaxNodesMax,
    /// Admitted syntax depth.
    SyntaxDepthMax,
    /// Admitted query captures.
    CapturesMax,
}

/// Failure kind behind one [`RustSyntaxError`].
///
/// `path` is `None` when the failing source reached [`RustQuery::captures`],
/// which admits raw text without a project path.
#[derive(Debug, PartialEq, Eq)]
pub enum RustSyntaxFault {
    /// Limit was configured as zero.
    ZeroLimit {
        /// Bound configured as zero.
        bound: RustSyntaxBound,
    },
    /// Source exceeds byte bound.
    SourceTooLarge {
        /// Failing source path.
        path: Option<ProjectPath>,
        /// Observed source bytes.
        source_bytes: usize,
        /// Configured byte bound.
        source_bytes_max: usize,
    },
    /// Syntax tree exceeds node bound.
    TooManyNodes {
        /// Failing source path.
        path: ProjectPath,
        /// Configured node bound.
        syntax_nodes_max: usize,
    },
    /// Syntax tree exceeds depth bound.
    TooDeep {
        /// Failing source path.
        path: ProjectPath,
        /// Configured depth bound.
        syntax_depth_max: usize,
    },
    /// Grammar cannot be loaded by runtime.
    IncompatibleGrammar {
        /// ABI version compiled into grammar.
        grammar_abi_version: usize,
        /// Oldest ABI version runtime accepts.
        runtime_abi_min: usize,
        /// Newest ABI version runtime accepts.
        runtime_abi_max: usize,
    },
    /// Parser produced no tree.
    ParseCancelled {
        /// Failing source path.
        path: Option<ProjectPath>,
    },
    /// Platform position cannot fit wire width.
    PositionOverflow {
        /// Grammar kind of overflowing node.
        node_kind: &'static str,
        /// Node start byte.
        start_byte: usize,
        /// Node end byte.
        end_byte: usize,
        /// Failed integer conversion.
        source: std::num::TryFromIntError,
    },
    /// Query is invalid for pinned Rust grammar.
    InvalidQuery {
        /// One-based failing line number.
        line_number: usize,
        /// Failing query line text.
        line_text: String,
        /// Underlying Tree-sitter rejection.
        source: QueryError,
    },
    /// Query produced more captures than admitted.
    TooManyCaptures {
        /// Configured capture bound.
        captures_max: usize,
    },
    /// Node kind is outside interpreted grammar vocabulary.
    UnknownNodeKind {
        /// Unrecognized grammar kind string.
        kind: String,
    },
}

impl RustSyntaxFault {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> RustSyntaxViolation {
        match self {
            Self::ZeroLimit { .. } => RustSyntaxViolation::ZeroLimit,
            Self::SourceTooLarge { .. } => RustSyntaxViolation::SourceTooLarge,
            Self::TooManyNodes { .. } => RustSyntaxViolation::TooManyNodes,
            Self::TooDeep { .. } => RustSyntaxViolation::TooDeep,
            Self::IncompatibleGrammar { .. } => RustSyntaxViolation::IncompatibleGrammar,
            Self::ParseCancelled { .. } => RustSyntaxViolation::ParseCancelled,
            Self::PositionOverflow { .. } => RustSyntaxViolation::PositionOverflow,
            Self::InvalidQuery { .. } => RustSyntaxViolation::InvalidQuery,
            Self::TooManyCaptures { .. } => RustSyntaxViolation::TooManyCaptures,
            Self::UnknownNodeKind { .. } => RustSyntaxViolation::UnknownNodeKind,
        }
    }
}

impl Fault for RustSyntaxFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::ZeroLimit { .. } => ErrorName::Wire(ErrorCode::ConfigurationInvalid),
            Self::SourceTooLarge { .. }
            | Self::TooManyNodes { .. }
            | Self::TooDeep { .. }
            | Self::TooManyCaptures { .. } => ErrorName::Wire(ErrorCode::LimitExceeded),
            Self::ParseCancelled { .. } => ErrorName::Wire(ErrorCode::Cancelled),
            Self::IncompatibleGrammar { .. }
            | Self::PositionOverflow { .. }
            | Self::InvalidQuery { .. }
            | Self::UnknownNodeKind { .. } => ErrorName::Wire(ErrorCode::InternalError),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::ZeroLimit { bound } => {
                vec![ErrorContext::new("bound", fault_label(bound))]
            }
            Self::SourceTooLarge {
                path,
                source_bytes,
                source_bytes_max,
            } => vec![
                ErrorContext::new(
                    "path",
                    path.as_ref()
                        .map_or_else(|| "<raw text>".to_string(), ToString::to_string),
                ),
                ErrorContext::new("source_bytes", source_bytes.to_string()),
                ErrorContext::new("source_bytes_max", source_bytes_max.to_string()),
            ],
            Self::TooManyNodes {
                path,
                syntax_nodes_max,
            } => vec![
                ErrorContext::new("path", path.to_string()),
                ErrorContext::new("syntax_nodes_max", syntax_nodes_max.to_string()),
            ],
            Self::TooDeep {
                path,
                syntax_depth_max,
            } => vec![
                ErrorContext::new("path", path.to_string()),
                ErrorContext::new("syntax_depth_max", syntax_depth_max.to_string()),
            ],
            Self::IncompatibleGrammar {
                grammar_abi_version,
                runtime_abi_min,
                runtime_abi_max,
            } => vec![
                ErrorContext::new("grammar_abi_version", grammar_abi_version.to_string()),
                ErrorContext::new("runtime_abi_min", runtime_abi_min.to_string()),
                ErrorContext::new("runtime_abi_max", runtime_abi_max.to_string()),
            ],
            Self::ParseCancelled { path } => path.as_ref().map_or_else(Vec::new, |path| {
                vec![ErrorContext::new("path", path.to_string())]
            }),
            Self::PositionOverflow {
                node_kind,
                start_byte,
                end_byte,
                source: _,
            } => vec![
                ErrorContext::new("node_kind", *node_kind),
                ErrorContext::new("start_byte", start_byte.to_string()),
                ErrorContext::new("end_byte", end_byte.to_string()),
            ],
            Self::InvalidQuery {
                line_number,
                line_text,
                source: _,
            } => vec![
                ErrorContext::new("line_number", line_number.to_string()),
                ErrorContext::new("line_text", line_text.clone()),
            ],
            Self::TooManyCaptures { captures_max } => {
                vec![ErrorContext::new("captures_max", captures_max.to_string())]
            }
            Self::UnknownNodeKind { kind } => {
                vec![ErrorContext::new("node_kind", kind.clone())]
            }
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PositionOverflow { source, .. } => Some(source),
            Self::InvalidQuery { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Opaque Rust syntax failure.
pub type RustSyntaxError = Error<RustSyntaxFault>;

fn incompatible_grammar(language: &tree_sitter::Language) -> RustSyntaxError {
    Error::new(RustSyntaxFault::IncompatibleGrammar {
        grammar_abi_version: language.abi_version(),
        runtime_abi_min: tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
        runtime_abi_max: tree_sitter::LANGUAGE_VERSION,
    })
}

fn position_overflow(node: Node<'_>, source: std::num::TryFromIntError) -> RustSyntaxError {
    Error::new(RustSyntaxFault::PositionOverflow {
        node_kind: node.kind(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        source,
    })
}

fn invalid_query(query_source: &str, source: QueryError) -> RustSyntaxError {
    Error::new(RustSyntaxFault::InvalidQuery {
        line_number: source.row + 1,
        line_text: query_source.lines().nth(source.row).unwrap_or("").into(),
        source,
    })
}

/// Bounded Tree-sitter Rust fact provider.
#[derive(Debug, Clone, Copy)]
pub struct RustSyntaxProvider {
    limits: RustSyntaxLimits,
}

impl RustSyntaxProvider {
    /// File extensions this provider parses, without their leading dot. The workspace walk
    /// admits a file as source only when some shipped provider declares its extension.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["rs"];

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
            return Err(Error::new(RustSyntaxFault::SourceTooLarge {
                path: Some(source.path.clone()),
                source_bytes: source.text.len(),
                source_bytes_max: self.limits.source_bytes_max,
            }));
        }
        let mut parser = rust_parser()?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(RustSyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let (nodes, symbols) = self.extract(tree.root_node(), source)?;
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
        source: RustSource<'_>,
    ) -> Result<(Vec<RustNode>, Vec<RustSymbol>), RustSyntaxError> {
        let text = source.text;
        let mut nodes = Vec::new();
        let mut symbols = Vec::new();
        let mut pending = vec![(root, None, String::new(), 0_usize)];
        while let Some((node, parent, qualification, depth)) = pending.pop() {
            if depth > self.limits.syntax_depth_max {
                return Err(Error::new(RustSyntaxFault::TooDeep {
                    path: source.path.clone(),
                    syntax_depth_max: self.limits.syntax_depth_max,
                }));
            }
            assert!(
                nodes.len() < self.limits.syntax_nodes_max,
                "the enqueue guard must keep the walker below the node bound: \
                 nodes={}, syntax_nodes_max={}",
                nodes.len(),
                self.limits.syntax_nodes_max,
            );

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
                    return Err(self.too_many_nodes(source));
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

    fn too_many_nodes(&self, source: RustSource<'_>) -> RustSyntaxError {
        Error::new(RustSyntaxFault::TooManyNodes {
            path: source.path.clone(),
            syntax_nodes_max: self.limits.syntax_nodes_max,
        })
    }
}

fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn rust_parser() -> Result<Parser, RustSyntaxError> {
    let language = rust_language();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| incompatible_grammar(&language))?;
    Ok(parser)
}

impl Default for RustSyntaxProvider {
    fn default() -> Self {
        Self::new(RustSyntaxLimits::default())
    }
}

fn declaration_name(node: Node<'_>, text: &str) -> Option<(String, RustSymbolKind)> {
    let kind = RustGrammarNodeKind::from_kind(node.kind())?.symbol_kind()?;
    let name = node.child_by_field_name(RustGrammarField::Name.as_str())?;
    text.get(name.byte_range())
        .map(|value| (value.into(), kind))
}

fn declaration_visibility(node: Node<'_>, text: &str) -> RustVisibility {
    (0..node.named_child_count())
        .filter_map(|index| node.named_child(index))
        .find(|child| child.kind() == RustGrammarNodeKind::VisibilityModifier.as_str())
        .and_then(|child| text.get(child.byte_range()))
        .map_or(RustVisibility::Private, RustVisibility::from_authored)
}

fn container_name(node: Node<'_>, text: &str) -> Option<String> {
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

fn qualify(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}::{name}")
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

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
                .fault()
                .violation(),
            RustSyntaxViolation::TooManyCaptures
        );
        assert_eq!(
            RustQuery::new("(missing_node) @rift.name")
                .expect_err("invalid node kind must fail")
                .fault()
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
                .fault()
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
            source_error.fault().violation(),
            RustSyntaxViolation::SourceTooLarge
        );

        let node_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(RustSource {
                    path: &path(),
                    text: "fn x() {}",
                })
                .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            RustSyntaxViolation::TooManyNodes
        );

        let depth_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 20, 1).expect("positive limits"))
                .analyze(RustSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                })
                .expect_err("depth bound");
        assert_eq!(
            depth_error.fault().violation(),
            RustSyntaxViolation::TooDeep
        );
    }

    #[test]
    fn test_provider_classifies_every_declaration_kind() {
        let text = "enum E {}\ntrait T {}\ntype A = u8;\nconst C: u8 = 0;\nstatic S: u8 = 0;\nmacro_rules! m { () => {}; }";
        let document = RustSyntaxProvider::default()
            .analyze(RustSource {
                path: &path(),
                text,
            })
            .expect("Rust fixture must parse");
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| symbol.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                RustSymbolKind::Enum,
                RustSymbolKind::Trait,
                RustSymbolKind::TypeAlias,
                RustSymbolKind::Constant,
                RustSymbolKind::Static,
                RustSymbolKind::Macro,
            ]
        );
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
        assert_eq!(
            error.fault().violation(),
            RustSyntaxViolation::UnknownNodeKind
        );
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
            (RustSyntaxLimits::new(0, 1, 1), "source_bytes_max"),
            (RustSyntaxLimits::new(1, 0, 1), "syntax_nodes_max"),
            (RustSyntaxLimits::new(1, 1, 0), "syntax_depth_max"),
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
        assert_eq!(error.fault().violation(), RustSyntaxViolation::ZeroLimit);
        assert_eq!(
            error.to_string(),
            "the workspace configuration failed validation: bound captures_max; \
             correct the reported configuration field, then retry"
        );
    }

    #[test]
    fn test_source_too_large_error_reports_sizes_path_and_limit_origin() {
        let error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(3, 10, 10).expect("positive limits"))
                .analyze(RustSource {
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
        let oversized = "a".repeat(RUST_SOURCE_BYTES_MAX_DEFAULT + 1);
        let error = query.captures(&oversized, 1).expect_err("oversized source");
        assert_eq!(
            error.fault().violation(),
            RustSyntaxViolation::SourceTooLarge
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the request exceeded a declared resource limit: \
                 path <raw text>, source_bytes {bytes}, source_bytes_max {max}; \
                 resize the request below the named limit, or raise that limit \
                 in the workspace configuration",
                bytes = RUST_SOURCE_BYTES_MAX_DEFAULT + 1,
                max = RUST_SOURCE_BYTES_MAX_DEFAULT,
            )
        );
    }

    #[test]
    fn test_tree_bound_errors_report_path_and_configured_limit() {
        let node_error =
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 1, 10).expect("positive limits"))
                .analyze(RustSource {
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
            RustSyntaxProvider::new(RustSyntaxLimits::new(100, 20, 1).expect("positive limits"))
                .analyze(RustSource {
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
            assert_eq!(error.fault().violation(), RustSyntaxViolation::InvalidQuery);
            match error.fault() {
                RustSyntaxFault::InvalidQuery { source, .. } => {
                    assert_eq!(source.kind, kind, "classifies {query}");
                }
                other => panic!("expected InvalidQuery, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_runtime_only_errors_render_full_context() {
        let grammar = incompatible_grammar(&rust_language());
        assert_eq!(
            grammar.fault().violation(),
            RustSyntaxViolation::IncompatibleGrammar
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
                abi = rust_language().abi_version(),
                min = tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
                max = tree_sitter::LANGUAGE_VERSION,
            )
        );

        let cancelled =
            RustSyntaxError::new(RustSyntaxFault::ParseCancelled { path: Some(path()) });
        assert_eq!(
            cancelled.fault().violation(),
            RustSyntaxViolation::ParseCancelled
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
        let cancelled_query = RustSyntaxError::new(RustSyntaxFault::ParseCancelled { path: None });
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
            RustSyntaxViolation::PositionOverflow
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
            RustSyntaxViolation::UnknownNodeKind
        );
        assert_eq!(unknown.descriptor().code(), "internal_error");
        assert_eq!(
            unknown.context(),
            vec![ErrorContext::new("node_kind", "bogus")]
        );

        let zero = RustSyntaxLimits::new(0, 1, 1).expect_err("zero source bound");
        assert_eq!(zero.fault().violation(), RustSyntaxViolation::ZeroLimit);
        assert_eq!(zero.descriptor().code(), "configuration_invalid");
        assert_eq!(
            zero.context(),
            vec![ErrorContext::new("bound", "source_bytes_max")]
        );
    }
}
