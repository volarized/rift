//! Generic syntax facts one provider emits for one source file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath, is_portable_name};
use rift_protocol::read::{Documentation, Language, Signature, SymbolFacet};

use crate::markdown::MarkdownFacts;

/// The character a qualified name carries its disambiguating number after, keeping the
/// provider key of each declaration distinct within one file.
const DUPLICATE_SUFFIX_MARKER: char = '~';

mod retained_size;
pub(crate) mod rust_macros;

/// Half-open UTF-8 byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    /// First included byte.
    pub start: u64,
    /// First excluded byte.
    pub end: u64,
}

impl ByteRange {
    /// Reports whether byte position belongs to range.
    #[must_use]
    pub const fn contains(self, position: u64) -> bool {
        self.start <= position && position < self.end
    }

    /// Reports whether `other` lies within this range, its ends included.
    #[must_use]
    pub const fn encloses(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// How [`SyntaxDocument::node_at`] matches a byte range against the node table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMatch {
    /// The node spans exactly the range.
    Exact,
    /// The node's span contains the range.
    Enclosing,
}

/// One named syntax-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNode {
    /// Grammar node kind, borrowed from the provider's static grammar names.
    pub kind: &'static str,
    /// Node byte range.
    pub range: ByteRange,
    /// Parent index in the document node vector; `None` for the root.
    pub parent: Option<usize>,
    /// Whether the parser marked the node erroneous or missing.
    pub has_error: bool,
}

/// Python overload state established from the original import and decorator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonOverload {
    /// Ordinary function declaration without an overload decorator.
    Ordinary,
    /// Import or decorator association was not established.
    Unknown,
    /// A typing overload import supplies this decorator.
    Overload {
        /// Whole original import statement.
        import_statement: ByteRange,
        /// Imported typing module token.
        module: ByteRange,
        /// Imported overload token, absent for a module import.
        imported: Option<ByteRange>,
        /// Name bound by the import.
        binding: ByteRange,
        /// Original decorator expression, excluding the leading `@`.
        decorator: ByteRange,
    },
}

/// One named declaration extracted from a source file.
///
/// `Eq` is not derived: `signatures` and `documentation` carry
/// [`rift_protocol::read::Extensions`], whose `serde_json::Value` payload
/// implements only `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxSymbol {
    /// Declared short name.
    pub name: String,
    /// Container-qualified name, unique within the file's symbol space.
    pub qualified_name: String,
    /// The containing symbol's qualified name; `None` for a declaration at
    /// the top level of the file.
    pub container: Option<String>,
    /// The provider's kind word, such as `function`, carried on the wire
    /// unchanged.
    pub kind: &'static str,
    /// Grammar node kind for the exact declaration range, when one matches.
    pub node_kind: Option<&'static str>,
    /// Portable categories this declaration falls into, in the provider's
    /// declared order.
    pub facets: Vec<SymbolFacet>,
    /// Authored visibility spelling, such as `pub(crate)`; `None` when the
    /// language states no visibility.
    pub visibility: Option<String>,
    /// Complete declaration byte range, extended over attached outer
    /// attributes and outer doc comments.
    pub range: ByteRange,
    /// The item node's own byte range, excluding any attached outer
    /// attributes and doc comments. Equal to `range` when nothing attaches.
    pub item_range: ByteRange,
    /// The grammar field naming this declaration, when the provider exposes one.
    pub name_range: Option<ByteRange>,
    /// The implementation part: the grammar's body or value field; `None`
    /// for a declaration without one.
    pub body_range: Option<ByteRange>,
    /// Callable forms this declaration renders as: its header before the
    /// implementation, or its whole text when it has none. Empty for a
    /// declaration the grammar does not mark callable.
    pub signatures: Arc<[Signature]>,
    /// Doc comments the grammar attaches to this declaration, stripped of
    /// comment syntax. Empty when nothing attaches.
    pub documentation: Arc<[Documentation]>,
    /// Exact source ranges for attached documentation.
    pub documentation_ranges: Vec<ByteRange>,
    /// Captured Rust module path state. Missing state remains unknown when facts are restored.
    pub module_path: Option<RustModulePath>,
    /// Captured Python overload state; absent for older or other language facts.
    pub python_overload: Option<PythonOverload>,
}

/// Form of one captured export binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxExportKind {
    /// Named local export or named reexport.
    Named,
    /// All exports from another module.
    All,
    /// A namespace export from another module.
    Namespace,
    /// A default export.
    Default,
}

/// Exact source ranges retained for one export binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxExportBinding {
    /// Authored export form.
    pub kind: SyntaxExportKind,
    /// Complete export statement.
    pub range: ByteRange,
    /// Defining local name or imported name.
    pub local: Option<ByteRange>,
    /// Exported name, when the statement names it.
    pub exported: Option<ByteRange>,
    /// Module source token, when the statement names another module.
    pub source: Option<ByteRange>,
    /// Existing declaration container, when present.
    pub container: Option<String>,
    /// Whether the binding exports only a type.
    pub type_only: bool,
}

/// Path attributes captured for one Rust module during the original syntax walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustModulePath {
    /// The semantic attribute run contains no module path or conditional path attribute.
    Absent,
    /// An accepted literal path and its exact unescaped source content.
    Literal {
        /// Validated path below the source root.
        path: rift_core::ProjectPath,
        /// Original literal content, before the module item.
        range: ByteRange,
    },
    /// Unsupported or conflicting attributes prevent an established module path.
    Unknown,
}

/// Origin of syntax facts before or after framework context applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxOrigin {
    /// Facts returned by a shipped syntax provider, including its embedded languages.
    Provider,
    /// Facts assembled after framework ownership or configuration applies.
    Framework,
}

/// Captured invocation, direct module items, and optional complete direct token ranges.
pub type MacroInvocation = (ByteRange, Vec<ByteRange>, Option<Vec<ByteRange>>);

/// Original Rust scope range and optional complete direct child ranges.
pub type Scope = (ByteRange, Option<Vec<ByteRange>>);

/// Immutable syntax facts and complete named nodes shared by documents at different paths.
///
/// The node table is present while the facts come straight from a parse and absent once an
/// owner that keeps facts for long released it ([`SyntaxDocument::into_facts`]) or restored
/// facts from storage.
///
/// Equality leaves the node table out: the source digest and syntax bounds the facts carry
/// decide it, so released or restored facts equal the parse they came from. `Eq` is not
/// derived: [`SyntaxSymbol`] is not `Eq`.
#[derive(Debug, Clone)]
pub struct SyntaxFacts {
    origin: SyntaxOrigin,
    language: Language,
    nodes: Option<Arc<[SyntaxNode]>>,
    symbols: Vec<SyntaxSymbol>,
    left_out_symbols: Vec<SyntaxSymbol>,
    export_bindings: Option<Vec<SyntaxExportBinding>>,
    macro_invocations: Option<Vec<MacroInvocation>>,
    scope: Option<Vec<Scope>>,
    has_errors: bool,
    left_out_declarations: usize,
    markdown_facts: Option<MarkdownFacts>,
    source_digest: Option<FileDigest>,
    syntax_limits: Option<crate::SyntaxLimits>,
}

impl PartialEq for SyntaxFacts {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            origin,
            language,
            nodes: _,
            symbols,
            left_out_symbols,
            export_bindings,
            macro_invocations,
            scope,
            has_errors,
            left_out_declarations,
            markdown_facts,
            source_digest,
            syntax_limits,
        } = self;
        *origin == other.origin
            && *language == other.language
            && *symbols == other.symbols
            && *left_out_symbols == other.left_out_symbols
            && *export_bindings == other.export_bindings
            && *macro_invocations == other.macro_invocations
            && *scope == other.scope
            && *has_errors == other.has_errors
            && *left_out_declarations == other.left_out_declarations
            && *markdown_facts == other.markdown_facts
            && *source_digest == other.source_digest
            && *syntax_limits == other.syntax_limits
    }
}

/// Immutable syntax facts for one source file and its project path.
///
/// Cloning a document or placing its facts at another path shares its syntax facts.
/// Equality compares the path, the facts, and the node tables.
#[derive(Debug, Clone)]
pub struct SyntaxDocument {
    path: ProjectPath,
    facts: Arc<SyntaxFacts>,
}

impl PartialEq for SyntaxDocument {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.facts == other.facts && self.nodes() == other.nodes()
    }
}

/// Suffixes every repeated qualified name apart, in source order.
///
/// A qualified name is the address half of a declaration's provider key, so two
/// declarations spelling one name would share one key. A name exactly one
/// declaration spells keeps its spelling. A name several declarations spell is
/// held by none of them: each takes a `~N` suffix counting from 1 in source
/// order, so no provider key silently names the first declaration. A logical
/// symbol identity never carries the suffix: namespace analysis binds every
/// declaration of one path to one identity. Only
/// `qualified_name` changes: ranges, containers, and kinds stay as the
/// provider extracted them. Whole names are compared, never parsed, so a
/// name the source already spells with a `~` is an ordinary name here.
///
/// Every provider funnels through [`SyntaxDocument::new`], so the rule holds
/// for every language without a provider restating it.
///
/// The pass needs no budget of its own: the provider's node bound already
/// fixed how many declarations reach here, and the search for an unused
/// number cannot outrun the names already taken.
fn suffix_duplicate_qualified_names(symbols: &mut [SyntaxSymbol]) {
    let occurrences = qualified_name_occurrences(symbols);
    let mut taken = kept_names(&occurrences);
    let mut counts: HashMap<String, u32> = HashMap::new();
    for symbol in symbols {
        if occurrences.get(&symbol.qualified_name) == Some(&1) {
            continue;
        }
        let count = counts.entry(symbol.qualified_name.clone()).or_default();
        let suffixed = unused_suffixed_name(&symbol.qualified_name, count, &taken);
        taken.insert(suffixed.clone());
        symbol.qualified_name = suffixed;
    }
}

/// How many declarations spell each qualified name in one document.
fn qualified_name_occurrences(symbols: &[SyntaxSymbol]) -> HashMap<String, usize> {
    let mut occurrences: HashMap<String, usize> = HashMap::with_capacity(symbols.len());
    for symbol in symbols {
        *occurrences
            .entry(symbol.qualified_name.clone())
            .or_default() += 1;
    }
    occurrences
}

/// The names one declaration each spells, which keep their spelling and are
/// therefore unavailable to a suffix.
fn kept_names(occurrences: &HashMap<String, usize>) -> HashSet<String> {
    occurrences
        .iter()
        .filter(|(_, occurrence_count)| **occurrence_count == 1)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The first `{name}~{number}` past `count` that no declaration holds,
/// leaving `count` at the number it took.
///
/// Counting continues past a number another declaration keeps, so a suffix
/// this pass writes can never repeat a name the source authored.
fn unused_suffixed_name(name: &str, count: &mut u32, taken: &HashSet<String>) -> String {
    loop {
        *count += 1;
        let suffixed = format!("{name}{DUPLICATE_SUFFIX_MARKER}{count}");
        if !taken.contains(&suffixed) {
            return suffixed;
        }
    }
}

/// Keeps Contribution-admitted declarations in `symbols` and retains the full omitted facts.
/// The provider's source and node bounds apply to both collections.
fn leave_out_refused_names(symbols: &mut Vec<SyntaxSymbol>) -> Vec<SyntaxSymbol> {
    let mut left_out = Vec::new();
    for symbol in std::mem::take(symbols) {
        if is_portable_name(&symbol.name) && is_portable_name(&symbol.qualified_name) {
            symbols.push(symbol);
        } else {
            left_out.push(symbol);
        }
    }
    left_out
}

impl SyntaxDocument {
    /// Creates a file holder with no declaration syntax facts.
    ///
    /// Use this for selected text whose format has no shipped syntax provider. Format
    /// extraction remains the documentation adapter's responsibility.
    #[must_use]
    pub fn empty(language: Language, path: ProjectPath) -> Self {
        Self::new(language, path, Vec::new(), Vec::new(), false)
    }

    /// Assembles one document from a provider's extracted facts, suffixing
    /// every repeated qualified name apart so each declaration addresses
    /// one identity. Declarations outside the Contribution name bounds retain
    /// their complete syntax facts in the omitted collection.
    pub(crate) fn new(
        language: Language,
        path: ProjectPath,
        nodes: Vec<SyntaxNode>,
        symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        Self::assemble(language, path, nodes, symbols, has_errors, None)
    }

    /// Checks literal Rust function header tokens within the token and source byte bounds.
    ///
    /// The tokens must come from the caller's checked original source range.
    #[must_use]
    pub fn function_header(input: &[&str], bounds: (usize, usize)) -> bool {
        let (tokens_max, source_bytes_max) = bounds;
        input.len() <= tokens_max
            && input
                .iter()
                .try_fold(0_usize, |bytes, token| bytes.checked_add(token.len()))
                .is_some_and(|bytes| bytes <= source_bytes_max)
            && rust_macros::function_header_tokens(input)
    }

    pub(crate) fn new_rust(
        language: Language,
        source: crate::SyntaxSource<'_>,
        limits: crate::SyntaxLimits,
        nodes: Vec<SyntaxNode>,
        mut symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        let mut invocations = Self::invocations(&language, &nodes);
        if let Some(invocations) = &mut invocations {
            rust_macros::capture(
                source.text,
                &nodes,
                invocations,
                Some((&language, limits, &mut symbols)),
            );
        }
        symbols.sort_by_key(|symbol| symbol.item_range.start);
        let mut document = Self::assemble(
            language,
            source.path.clone(),
            nodes,
            symbols,
            has_errors,
            invocations,
        );
        let facts = Arc::make_mut(&mut document.facts);
        facts.source_digest = Some(FileDigest::of(source.text.as_bytes()));
        facts.scope = rust_macros::capture_scope(
            source.text,
            facts.nodes.as_deref().unwrap_or_default(),
            has_errors,
        );
        facts.syntax_limits = Some(limits);
        document
    }

    fn invocations(language: &Language, nodes: &[SyntaxNode]) -> Option<Vec<MacroInvocation>> {
        (language.name == "rust" && language.dialect.is_none()).then(|| {
            nodes
                .iter()
                .filter(|node| {
                    node.kind == "macro_invocation"
                        && node
                            .parent
                            .and_then(|parent| nodes.get(parent))
                            .is_some_and(|parent| {
                                (parent.kind == "source_file" && parent.parent.is_none())
                                    || (parent.kind == "expression_statement"
                                        && parent
                                            .parent
                                            .and_then(|root| nodes.get(root))
                                            .is_some_and(|root| {
                                                root.kind == "source_file" && root.parent.is_none()
                                            }))
                            })
                })
                .map(|node| (node.range, Vec::new(), None))
                .collect()
        })
    }

    fn assemble(
        language: Language,
        path: ProjectPath,
        nodes: Vec<SyntaxNode>,
        mut symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
        captured: Option<Vec<MacroInvocation>>,
    ) -> Self {
        suffix_duplicate_qualified_names(&mut symbols);
        let declaration_nodes = declaration_node_kinds(&nodes, &symbols);
        for symbol in &mut symbols {
            if symbol.node_kind.is_none() {
                symbol.node_kind = declaration_nodes.get(&symbol.range).copied();
            }
        }
        let macro_invocations = captured.or_else(|| Self::invocations(&language, &nodes));
        let left_out_symbols = leave_out_refused_names(&mut symbols);
        let left_out_declarations = left_out_symbols.len();
        Self {
            path,
            facts: Arc::new(SyntaxFacts {
                origin: SyntaxOrigin::Provider,
                language,
                nodes: Some(Arc::from(nodes)),
                symbols,
                left_out_symbols,
                export_bindings: None,
                macro_invocations,
                scope: None,
                has_errors,
                left_out_declarations,
                markdown_facts: None,
                source_digest: None,
                syntax_limits: None,
            }),
        }
    }

    /// Places these facts at another project path.
    ///
    /// The caller must establish that source bytes, language, dialect, and syntax bounds
    /// match the facts this document holds.
    #[must_use]
    pub fn at_path(&self, path: ProjectPath) -> Self {
        Self {
            path,
            facts: Arc::clone(&self.facts),
        }
    }

    /// Attaches facts extracted from the same Markdown tree.
    pub(crate) fn with_markdown_facts(mut self, facts: MarkdownFacts) -> Self {
        let shared = Arc::make_mut(&mut self.facts);
        shared.has_errors |= !facts.error_ranges().is_empty();
        shared.markdown_facts = Some(facts);
        self
    }

    /// Attaches export bindings extracted from the same syntax tree.
    pub(crate) fn with_export_bindings(mut self, bindings: Vec<SyntaxExportBinding>) -> Self {
        Arc::make_mut(&mut self.facts).export_bindings = Some(bindings);
        self
    }

    /// Returns the language identity these facts are filed under.
    #[must_use]
    pub fn language(&self) -> &Language {
        self.facts.language()
    }

    /// Returns source path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns path-independent syntax facts.
    #[must_use]
    pub fn facts(&self) -> &SyntaxFacts {
        &self.facts
    }

    /// Returns digest for exact bytes parsed by this provider.
    #[must_use]
    pub fn source_digest(&self) -> Option<&FileDigest> {
        self.facts.source_digest()
    }

    pub(crate) fn with_source_witness(mut self, source: &str) -> Self {
        let facts = Arc::make_mut(&mut self.facts);
        facts.source_digest = Some(FileDigest::of(source.as_bytes()));
        let nodes = facts.nodes.clone().unwrap_or_default();
        if facts.language.name == "rust" && facts.language.dialect.is_none() {
            facts.scope = rust_macros::capture_scope(source, &nodes, facts.has_errors);
        }
        if let Some(invocations) = &mut facts.macro_invocations {
            rust_macros::capture(source, &nodes, invocations, None);
        }
        self
    }

    /// Returns every named syntax node in pre-order.
    #[must_use]
    pub fn nodes(&self) -> &[SyntaxNode] {
        self.facts.nodes().unwrap_or_default()
    }

    /// Returns extracted declarations in source order.
    #[must_use]
    pub fn symbols(&self) -> &[SyntaxSymbol] {
        self.facts.symbols()
    }

    /// How many extracted declarations this document leaves out: each with
    /// a `name` or `qualified_name` the Contribution contract refuses -
    /// empty, past `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes, or holding a
    /// control character.
    #[must_use]
    pub fn left_out_declaration_count(&self) -> usize {
        self.facts.left_out_declaration_count()
    }

    /// Reports whether parser observed malformed syntax.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.facts.has_errors()
    }

    /// Returns Markdown block and inline facts when the Markdown provider produced them.
    #[must_use]
    pub fn markdown_facts(&self) -> Option<&MarkdownFacts> {
        self.facts.markdown_facts()
    }

    /// Returns nodes covering byte position, outermost first.
    #[must_use]
    pub fn nodes_at(&self, position: u64) -> Vec<&SyntaxNode> {
        self.nodes()
            .iter()
            .filter(|node| node.range.contains(position))
            .collect()
    }

    /// Returns the innermost node at `range`: the one spanning it exactly, or under
    /// [`NodeMatch::Enclosing`] the one whose span contains it. `None` when no node matches.
    ///
    /// Pre-order lists every node after its ancestors, so the last match is the innermost.
    #[must_use]
    pub fn node_at(&self, range: ByteRange, matching: NodeMatch) -> Option<&SyntaxNode> {
        self.nodes().iter().rev().find(|node| match matching {
            NodeMatch::Exact => node.range == range,
            NodeMatch::Enclosing => node.range.encloses(range),
        })
    }

    /// Returns the nodes that belong to one declaration, outermost first: its item node, the
    /// node of `node_kind` spanning `item_range`, then every descendant outside the
    /// declarations nested in it. Empty when the table holds no item node.
    ///
    /// One pass over the item's subtree, with a binary search over the nested declarations'
    /// spans per node: `O(n log m)` for `n` subtree nodes and `m` nested declarations.
    #[must_use]
    pub fn declaration_nodes(&self, symbol: &SyntaxSymbol) -> Vec<&SyntaxNode> {
        let nodes = self.nodes();
        let Some(item) = nodes.iter().position(|node| {
            node.range == symbol.item_range && symbol.node_kind.is_none_or(|kind| kind == node.kind)
        }) else {
            return Vec::new();
        };
        let nested = self.nested_declaration_spans(symbol);
        let mut belongs = vec![true];
        let mut owned = vec![&nodes[item]];
        // Pre-order keeps the item's subtree contiguous: the walk ends at the first node whose
        // parent lies before the item.
        for node in &nodes[item + 1..] {
            let parent = node.parent.and_then(|parent| parent.checked_sub(item));
            let Some(parent_belongs) = parent.and_then(|parent| belongs.get(parent).copied())
            else {
                break;
            };
            let member = parent_belongs && !within_any(&nested, node.range);
            belongs.push(member);
            if member {
                owned.push(node);
            }
        }
        owned
    }

    /// The outermost spans of the declarations nested inside `symbol`'s item, sorted by start
    /// and disjoint, each the nested declaration's complete range.
    fn nested_declaration_spans(&self, symbol: &SyntaxSymbol) -> Vec<ByteRange> {
        let mut spans = self
            .symbols()
            .iter()
            .chain(self.facts.left_out_symbols())
            .filter(|other| {
                other.item_range != symbol.item_range && symbol.item_range.encloses(other.range)
            })
            .map(|other| other.range)
            .collect::<Vec<_>>();
        spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
        let mut outermost: Vec<ByteRange> = Vec::with_capacity(spans.len());
        for span in spans {
            if outermost.last().is_none_or(|last| !last.encloses(span)) {
                outermost.push(span);
            }
        }
        outermost
    }

    /// Clones the shared path-independent facts.
    #[must_use]
    pub fn shared_facts(&self) -> Arc<SyntaxFacts> {
        Arc::clone(&self.facts)
    }

    /// Returns shared path-independent facts with the complete node table released.
    ///
    /// An owner that keeps facts for long, such as an index, holds no node rows: a read that
    /// needs them parses the captured bytes again under the same bounds.
    #[must_use]
    pub fn into_facts(mut self) -> Arc<SyntaxFacts> {
        if self.facts.nodes.is_some() {
            Arc::make_mut(&mut self.facts).nodes = None;
        }
        self.facts
    }
}

impl SyntaxFacts {
    /// Returns the complete named tree in pre-order, or `None` when these facts hold none.
    #[must_use]
    pub fn nodes(&self) -> Option<&[SyntaxNode]> {
        self.nodes.as_deref()
    }

    /// Returns recorded export bindings, or `None` for unrecorded facts.
    #[must_use]
    pub fn export_bindings(&self) -> Option<&[SyntaxExportBinding]> {
        self.export_bindings.as_deref()
    }

    /// Returns original Rust scopes, or `None` for unrecorded scope facts.
    #[must_use]
    pub fn scope(&self) -> Option<&[Scope]> {
        self.scope.as_deref()
    }

    /// Returns captured top-level Rust macro invocations, or `None` for unrecorded facts.
    #[must_use]
    pub fn macro_invocations(&self) -> Option<&[MacroInvocation]> {
        self.macro_invocations.as_deref()
    }
    /// Returns whether these facts came from a provider or framework context.
    #[must_use]
    pub const fn origin(&self) -> SyntaxOrigin {
        self.origin
    }

    /// Returns the language identity these facts are filed under.
    #[must_use]
    pub fn language(&self) -> &Language {
        &self.language
    }

    /// Returns digest for exact bytes parsed by this provider.
    #[must_use]
    pub const fn source_digest(&self) -> Option<&FileDigest> {
        self.source_digest.as_ref()
    }

    /// Returns extracted declarations in source order.
    #[must_use]
    pub fn symbols(&self) -> &[SyntaxSymbol] {
        &self.symbols
    }

    /// Full declarations omitted from the portable Contribution collection.
    #[must_use]
    pub fn left_out_symbols(&self) -> &[SyntaxSymbol] {
        &self.left_out_symbols
    }

    /// How many extracted declarations this document leaves out.
    #[must_use]
    pub const fn left_out_declaration_count(&self) -> usize {
        self.left_out_declarations
    }

    /// Reports whether parser observed malformed syntax.
    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Returns Markdown block and inline facts when the Markdown provider produced them.
    #[must_use]
    pub const fn markdown_facts(&self) -> Option<&MarkdownFacts> {
        self.markdown_facts.as_ref()
    }
}

/// Whether `range` lies within one of `spans`, which are sorted by start and disjoint.
fn within_any(spans: &[ByteRange], range: ByteRange) -> bool {
    let after = spans.partition_point(|span| span.start <= range.start);
    after
        .checked_sub(1)
        .and_then(|index| spans.get(index))
        .is_some_and(|span| span.encloses(range))
}

fn declaration_node_kinds(
    nodes: &[SyntaxNode],
    symbols: &[SyntaxSymbol],
) -> BTreeMap<ByteRange, &'static str> {
    let mut ranges = symbols
        .iter()
        .map(|symbol| symbol.range)
        .collect::<HashSet<_>>();
    let mut declaration_nodes = BTreeMap::new();
    for node in nodes {
        if ranges.remove(&node.range) {
            declaration_nodes.insert(node.range, node.kind);
        }
    }
    declaration_nodes
}

impl SyntaxDocument {
    pub(crate) fn with_framework_context(mut self) -> Self {
        Arc::make_mut(&mut self.facts).origin = SyntaxOrigin::Framework;
        self
    }

    /// Adds declarations omitted by documents merged into this document.
    pub(crate) fn with_left_out_declarations(mut self, count: usize) -> Self {
        let facts = Arc::make_mut(&mut self.facts);
        facts.left_out_declarations = facts
            .left_out_declarations
            .checked_add(count)
            .expect("merged omitted declaration count must fit usize");
        self
    }

    pub(crate) fn with_syntax_limits(mut self, limits: crate::SyntaxLimits) -> Self {
        Arc::make_mut(&mut self.facts).syntax_limits = Some(limits);
        self
    }
}

impl SyntaxFacts {
    /// Returns the effective syntax bounds recorded by the provider.
    #[must_use]
    pub const fn syntax_limits(&self) -> Option<crate::SyntaxLimits> {
        self.syntax_limits
    }

    pub(crate) fn restored(parts: crate::SyntaxFactsParts, limits: crate::SyntaxLimits) -> Self {
        Self {
            origin: parts.origin,
            language: parts.language,
            nodes: None,
            symbols: parts.symbols,
            left_out_symbols: parts.left_out_symbols,
            export_bindings: parts.export_bindings,
            macro_invocations: parts.macro_invocations,
            scope: parts.scope,
            has_errors: parts.has_errors,
            left_out_declarations: parts.left_out_declarations,
            markdown_facts: parts.markdown_facts,
            source_digest: Some(parts.source_digest),
            syntax_limits: Some(limits),
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_core::{PROVIDER_SYMBOL_ID_BYTES_MAX, encode_path, symbol_identity};

    use super::*;
    use crate::contribution::DocumentPlacement;
    use crate::provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};

    /// The literal characters `SymbolId`'s advertised pattern accepts in the
    /// segments after the language, transcribed from `rift-protocol`, with
    /// `%` for the escape form it also allows.
    const SYMBOL_ID_TAIL_CHARACTERS: &str = "._~!$&'()*+,;=:/@-%";

    fn language() -> Language {
        Language {
            name: "yaml".to_owned(),
            dialect: None,
        }
    }

    fn path() -> ProjectPath {
        ProjectPath::new(".github/dependabot.yml").expect("valid fixture path")
    }

    fn rust_document(path: &ProjectPath, source: &str) -> SyntaxDocument {
        crate::registry::provider_for_extension("rs")
            .expect("the Rust provider claims rs")
            .analyze(SyntaxSource { path, text: source }, SyntaxLimits::default())
            .expect("the Rust fixture parses")
    }

    /// One declaration spelling `qualified_name`, spanning one byte at
    /// `start` so source order stays readable in an assertion.
    fn symbol(qualified_name: &str, start: u64) -> SyntaxSymbol {
        let range = ByteRange {
            start,
            end: start + 1,
        };
        SyntaxSymbol {
            name: qualified_name.to_owned(),
            qualified_name: qualified_name.to_owned(),
            container: None,
            kind: "mapping_entry",
            node_kind: None,
            facets: Vec::new(),
            visibility: None,
            range,
            item_range: range,
            name_range: None,
            body_range: None,
            signatures: Arc::from([]),
            documentation: Arc::from([]),
            documentation_ranges: Vec::new(),
            module_path: None,
            python_overload: None,
        }
    }

    /// A document whose declarations spell `names` in source order.
    fn document(names: &[&str]) -> SyntaxDocument {
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let start = u64::try_from(index).expect("fixture index fits a byte offset");
                symbol(name, start)
            })
            .collect();
        SyntaxDocument::new(language(), path(), Vec::new(), symbols, false)
    }

    #[test]
    fn empty_file_holder_keeps_language_and_path_without_declarations() {
        let language = language();
        let path = path();

        let document = SyntaxDocument::empty(language.clone(), path.clone());

        assert_eq!(document.language(), &language);
        assert_eq!(document.path(), &path);
        assert!(document.nodes().is_empty());
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    #[test]
    fn placing_same_facts_at_another_path_keeps_old_document_and_releases_on_drop() {
        let first_path = ProjectPath::new("src/first.rs").expect("valid first path");
        let second_path = ProjectPath::new("src/second.rs").expect("valid second path");
        let source = "pub fn beacon() {}\n";
        let first = rust_document(&first_path, source);
        let facts = Arc::downgrade(&first.facts);

        let second = first.at_path(second_path.clone());
        assert_eq!(first.path(), &first_path);
        assert_eq!(second.path(), &second_path);
        assert!(Arc::ptr_eq(&first.facts, &second.facts));
        assert_eq!(second.language(), first.language());
        assert_eq!(second.source_digest(), first.source_digest());
        assert_eq!(second.nodes(), first.nodes());
        assert_eq!(second.symbols(), first.symbols());
        assert_eq!(second.has_errors(), first.has_errors());
        assert_eq!(
            DocumentPlacement::project(&first)
                .expect("first project placement")
                .identity_path(),
            first_path.as_str()
        );
        assert_eq!(
            DocumentPlacement::project(&second)
                .expect("second project placement")
                .identity_path(),
            second_path.as_str()
        );

        let changed = rust_document(&second_path, "pub fn lantern() {}\n");
        assert_ne!(changed.symbols(), second.symbols());
        assert_eq!(second.symbols()[0].name, "beacon");

        drop(first);
        drop(second);
        assert!(facts.upgrade().is_none());
    }

    #[test]
    fn placing_facts_keeps_the_language_and_dialect_that_produced_them() {
        use crate::typescript::TypeScriptDialect;

        let path = ProjectPath::new("src/view.tsx").expect("valid TypeScript path");
        let source = "const App = () => <section />;\n";
        let tsx = TypeScriptDialect::Tsx
            .provider()
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("the TSX fixture parses");
        let typescript = TypeScriptDialect::TypeScript
            .provider()
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("the TypeScript grammar records its parse result");

        let placed = tsx.at_path(ProjectPath::new("src/copied.ts").expect("valid copied path"));
        assert_eq!(placed.language().name, "typescript");
        assert_eq!(placed.language().dialect.as_deref(), Some("tsx"));
        assert_eq!(typescript.language().name, "typescript");
        assert_eq!(typescript.language().dialect, None);
        assert_ne!(placed.language(), typescript.language());
        assert_eq!(placed.nodes(), tsx.nodes());
        assert_eq!(placed.symbols(), tsx.symbols());
    }

    #[test]
    fn indexed_facts_release_node_table_and_keep_first_declaration_match() {
        let range = ByteRange { start: 7, end: 8 };
        let document = SyntaxDocument::new(
            language(),
            path(),
            vec![
                SyntaxNode {
                    kind: "first_kind",
                    range,
                    parent: None,
                    has_error: false,
                },
                SyntaxNode {
                    kind: "second_kind",
                    range,
                    parent: Some(0),
                    has_error: true,
                },
            ],
            vec![symbol("entry", range.start)],
            false,
        );
        let node_table = Arc::downgrade(
            document
                .facts
                .nodes
                .as_ref()
                .expect("a parsed document holds its node table"),
        );
        assert_eq!(
            document
                .nodes()
                .iter()
                .find(|node| node.range == range)
                .map(|node| node.kind),
            Some("first_kind")
        );

        let parsed = document.facts().clone();
        let facts = document.into_facts();
        assert_eq!(
            *facts, parsed,
            "releasing the node table keeps the facts equal"
        );
        assert!(parsed.nodes().is_some_and(|nodes| nodes.len() == 2));
        assert_eq!(facts.nodes(), None);
        let heap = |facts: &SyntaxFacts| {
            get_size2::GetSize::get_heap_size_with_tracker(
                facts,
                get_size2::StandardTracker::default(),
            )
            .0
        };
        let mut without_nodes = parsed.clone();
        without_nodes.nodes = None;
        assert_eq!(
            heap(&parsed),
            heap(&without_nodes) + 2 * std::mem::size_of::<SyntaxNode>(),
            "a held node table counts toward the retained bytes"
        );
        drop(parsed);
        assert!(node_table.upgrade().is_none());
        assert_eq!(facts.symbols()[0].node_kind, Some("first_kind"));
    }

    /// A Rust document with a module nesting a function, beside a free function.
    const NESTED_SOURCE: &str = "/// Lamp.\npub mod lamp {\n    /// Light.\n    pub fn light() -> u8 {\n        1\n    }\n}\n\npub fn dim() {}\n";

    fn range_of(text: &str) -> ByteRange {
        let start = NESTED_SOURCE.find(text).expect("fixture text");
        ByteRange {
            start: start as u64,
            end: (start + text.len()) as u64,
        }
    }

    fn declared<'a>(document: &'a SyntaxDocument, name: &str) -> &'a SyntaxSymbol {
        document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == name)
            .expect("fixture declaration")
    }

    #[test]
    fn documents_are_equal_when_path_facts_and_node_tables_agree() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);

        let moved = document.at_path(ProjectPath::new("src/moved.rs").expect("valid path"));
        let released = SyntaxDocument {
            path: path.clone(),
            facts: document.clone().into_facts(),
        };

        assert_eq!(document, rust_document(&path, NESTED_SOURCE));
        assert_ne!(document, moved, "the path differs");
        assert_ne!(document, released, "the node table differs");
        assert_eq!(*document.facts(), *released.facts());
    }

    #[test]
    fn node_at_answers_the_innermost_exact_or_enclosing_node() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);
        let literal = range_of("1\n");
        let literal = ByteRange {
            end: literal.start + 1,
            ..literal
        };

        let exact = document
            .node_at(literal, NodeMatch::Exact)
            .expect("the literal node");
        assert_eq!(exact.range, literal);
        assert_eq!(exact.kind, "integer_literal");
        let inside = ByteRange {
            start: literal.start,
            end: literal.start,
        };
        assert_eq!(document.node_at(inside, NodeMatch::Exact), None);
        assert_eq!(
            document
                .node_at(inside, NodeMatch::Enclosing)
                .map(|node| node.kind),
            Some("integer_literal")
        );
        let function = declared(&document, "light").item_range;
        let across = ByteRange {
            start: function.start,
            end: literal.end,
        };
        assert_eq!(
            document
                .node_at(across, NodeMatch::Enclosing)
                .map(|node| node.range),
            Some(function)
        );
        let past = ByteRange {
            start: 0,
            end: NESTED_SOURCE.len() as u64 + 1,
        };
        assert_eq!(document.node_at(past, NodeMatch::Enclosing), None);
    }

    #[test]
    fn declaration_nodes_hold_the_item_subtree_without_nested_declarations() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);
        let lamp = declared(&document, "lamp");
        let light = declared(&document, "light");

        let module = document.declaration_nodes(lamp);
        let function = document.declaration_nodes(light);

        assert_eq!(module.first().map(|node| node.range), Some(lamp.item_range));
        assert!(
            module.iter().all(|node| !light.range.encloses(node.range)),
            "the nested function and its doc comment belong to the function: {module:?}"
        );
        assert!(module.iter().any(|node| node.kind == "declaration_list"));
        assert_eq!(
            function.first().map(|node| node.range),
            Some(light.item_range)
        );
        assert!(function.iter().any(|node| node.kind == "integer_literal"));
        assert!(
            function
                .iter()
                .all(|node| light.item_range.encloses(node.range))
        );
        let dim = document.declaration_nodes(declared(&document, "dim"));
        assert!(
            dim.iter()
                .all(|node| node.range.start >= range_of("pub fn dim").start)
        );
    }

    #[test]
    fn declaration_nodes_are_empty_without_an_item_node_or_a_node_table() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);
        let mut moved = declared(&document, "light").clone();
        moved.item_range = ByteRange { start: 0, end: 1 };
        assert!(document.declaration_nodes(&moved).is_empty());

        let released = SyntaxDocument {
            path: path.clone(),
            facts: document.clone().into_facts(),
        };
        assert!(released.nodes().is_empty());
        assert!(
            released
                .declaration_nodes(declared(&document, "light"))
                .is_empty()
        );
        assert_eq!(
            released.node_at(ByteRange { start: 0, end: 1 }, NodeMatch::Enclosing),
            None
        );
    }

    fn qualified_names(document: &SyntaxDocument) -> Vec<&str> {
        document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect()
    }

    #[test]
    fn test_a_document_without_repeated_names_keeps_every_qualified_name() {
        let document = document(&["version", "updates", "updates > directory"]);
        assert_eq!(
            qualified_names(&document),
            ["version", "updates", "updates > directory"]
        );
    }

    #[test]
    fn test_an_empty_symbol_list_stays_empty() {
        let document = document(&[]);
        assert!(document.symbols().is_empty());
    }

    #[test]
    fn test_a_single_declaration_keeps_its_qualified_name() {
        let document = document(&["updates"]);
        assert_eq!(qualified_names(&document), ["updates"]);
    }

    /// No member of a repeated name holds the bare name: both take a
    /// suffix, so an address carrying the bare name resolves to neither.
    #[test]
    fn test_two_declarations_under_one_name_both_take_suffixes() {
        let document = document(&["port", "port"]);
        assert_eq!(qualified_names(&document), ["port~1", "port~2"]);
    }

    #[test]
    fn test_four_declarations_under_one_name_number_in_source_order() {
        let document = document(&["port", "port", "port", "port"]);
        assert_eq!(
            qualified_names(&document),
            ["port~1", "port~2", "port~3", "port~4"]
        );
    }

    /// A name one declaration spells stays byte-identical beside a repeated
    /// name that every member of suffixes.
    #[test]
    fn test_a_name_one_declaration_spells_stays_bare_beside_a_repeated_name() {
        let document = document(&["version", "port", "port"]);
        assert_eq!(qualified_names(&document), ["version", "port~1", "port~2"]);
    }

    /// Two repeated names interleaved in one document count apart from each
    /// other: each name's own declarations number it.
    #[test]
    fn test_interleaved_repeated_names_number_independently() {
        let document = document(&["port", "host", "port", "host", "port"]);
        assert_eq!(
            qualified_names(&document),
            ["port~1", "host~1", "port~2", "host~2", "port~3"]
        );
    }

    /// The pass compares whole qualified names and appends, so a name the
    /// source already spells with the suffix character is an ordinary name.
    #[test]
    fn test_a_name_already_carrying_the_suffix_character_is_not_parsed() {
        let document = document(&["a~b", "a~b", "a~b"]);
        assert_eq!(qualified_names(&document), ["a~b~1", "a~b~2", "a~b~3"]);
    }

    /// A name one declaration spells as `foo~1` keeps that spelling, so the
    /// repeated `foo` counts past it. The result does not depend on where
    /// the kept name sits in source order.
    #[test]
    fn test_a_repeated_name_counts_past_a_suffix_another_declaration_keeps() {
        let kept_first = document(&["foo~1", "foo", "foo"]);
        assert_eq!(qualified_names(&kept_first), ["foo~1", "foo~2", "foo~3"]);
        let kept_last = document(&["foo", "foo", "foo~1"]);
        assert_eq!(qualified_names(&kept_last), ["foo~2", "foo~3", "foo~1"]);
    }

    /// Source order, not iteration order, decides which declaration takes
    /// which suffix: the suffixes ascend with the declaration spans.
    #[test]
    fn test_source_order_decides_which_declaration_takes_which_suffix() {
        let document = document(&["port", "port", "port"]);
        let numbered = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.range.start))
            .collect::<Vec<_>>();
        assert_eq!(numbered, [("port~1", 0), ("port~2", 1), ("port~3", 2)]);
    }

    /// Only the qualified name moves: the container, kind, and every span
    /// stay as the provider extracted them.
    #[test]
    fn test_a_suffixed_declaration_keeps_every_other_fact() {
        let document = document(&["port", "port"]);
        let suffixed = &document.symbols()[1];
        assert_eq!(suffixed.qualified_name, "port~2");
        assert_eq!(suffixed.name, "port");
        assert_eq!(suffixed.container, None);
        assert_eq!(suffixed.kind, "mapping_entry");
        assert_eq!(suffixed.range, ByteRange { start: 1, end: 2 });
        assert_eq!(suffixed.item_range, ByteRange { start: 1, end: 2 });
    }

    /// The suffix marker reaches the wire identity literally: `encode_path`
    /// keeps `~` out of its escape set, and the minted identity carries
    /// only characters `SymbolId`'s pattern accepts.
    #[test]
    fn test_a_suffixed_name_reaches_the_wire_identity_unescaped() {
        let document = document(&["updates > package-ecosystem", "updates > package-ecosystem"]);
        let suffixed = document.symbols()[0].qualified_name.as_str();
        assert_eq!(suffixed, "updates > package-ecosystem~1");
        assert_eq!(
            encode_path(suffixed),
            "updates%20%3E%20package-ecosystem~1",
            "encode_path keeps the suffix marker literal"
        );
        let identity = symbol_identity("yaml", path().as_str(), suffixed);
        assert_eq!(
            identity,
            "rift://symbol/yaml/.github/dependabot.yml/updates%20%3E%20package-ecosystem~1"
        );
        let tail = identity
            .strip_prefix("rift://symbol/yaml/")
            .expect("the identity files under its language segment");
        assert!(
            tail.chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || SYMBOL_ID_TAIL_CHARACTERS.contains(character)),
            "every character must be one SymbolId's pattern accepts: identity={identity}"
        );
    }

    /// A declaration whose name is empty, holds a control character, or
    /// passes `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes is left out; a name at
    /// the bound stays. The count names how many left.
    #[test]
    fn test_names_the_contract_refuses_are_left_out_and_counted() {
        let at_bound = "n".repeat(PROVIDER_SYMBOL_ID_BYTES_MAX);
        let past_bound = "n".repeat(PROVIDER_SYMBOL_ID_BYTES_MAX + 1);
        let document = document(&[
            "",
            "first\nline",
            at_bound.as_str(),
            past_bound.as_str(),
            "kept",
        ]);
        assert_eq!(qualified_names(&document), [at_bound.as_str(), "kept"]);
        assert_eq!(document.left_out_declaration_count(), 2 + 1);
    }

    #[test]
    fn test_a_document_whose_names_the_contract_accepts_leaves_none_out() {
        let document = document(&["version", "updates"]);
        assert_eq!(document.left_out_declaration_count(), 0);
    }

    /// A declaration nested under a refused one carries the refused bytes in
    /// its own qualified name, so the same rule leaves it out.
    #[test]
    fn test_a_declaration_nested_under_a_refused_name_is_left_out_with_it() {
        let document = document(&["first\nline", "first\nline > child", "kept"]);
        assert_eq!(qualified_names(&document), ["kept"]);
        assert_eq!(document.left_out_declaration_count(), 2);
    }

    /// The suffix pass runs first, so the bound holds for the qualified name
    /// the Contribution carries: two declarations spelling one name at the
    /// bound each take a suffix that passes it, and both are left out.
    #[test]
    fn test_the_bound_holds_for_the_suffixed_qualified_name() {
        let at_bound = "n".repeat(PROVIDER_SYMBOL_ID_BYTES_MAX);
        let document = document(&[at_bound.as_str(), at_bound.as_str(), "kept"]);
        assert_eq!(qualified_names(&document), ["kept"]);
        assert_eq!(document.left_out_declaration_count(), 2);
    }
}
