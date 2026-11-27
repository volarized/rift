//! Generic syntax facts one provider emits for one source file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath, is_portable_name};
use rift_protocol::read::{
    Documentation, Language, Signature, SymbolKind, symbol_name_is_retained,
};

use crate::markdown::MarkdownFacts;

mod retained_size;
mod syntax_element_places;

pub use syntax_element_places::{SyntaxHolder, SyntaxPlace};
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

/// How [`SyntaxDocument::syntax_element_at`] matches a byte range against the syntax element table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxMatch {
    /// The syntax element spans exactly the range.
    Exact,
    /// The syntax element's span contains the range.
    Enclosing,
}

/// One named syntax-tree syntax element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxElement {
    /// Grammar syntax element kind, borrowed from the provider's static grammar names.
    pub kind: &'static str,
    /// Syntax element byte range.
    pub range: ByteRange,
    /// Parent index in the document syntax element vector; `None` for the root.
    pub parent: Option<usize>,
    /// Whether the parser marked the syntax element erroneous or missing.
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
    /// Container-qualified name. Every declaration of one symbol in the file spells it,
    /// so several declarations can share it.
    pub qualified_name: String,
    /// Position among the file's declarations spelling `qualified_name`, counted from 0
    /// in source order. With `qualified_name` it keys the declaration within its file.
    pub declaration_ordinal: usize,
    /// The containing symbol's qualified name; `None` for a declaration at
    /// the top level of the file.
    pub container: Option<String>,
    /// The provider's kind word, such as `function`, carried on the wire
    /// unchanged.
    pub kind: &'static str,
    /// Grammar syntax element kind for the exact declaration range, when one matches.
    pub syntax_element_kind: Option<&'static str>,
    /// Portable categories this declaration falls into, in the provider's
    /// declared order.
    pub kinds: Vec<SymbolKind>,
    /// Authored visibility spelling, such as `pub(crate)`; `None` when the
    /// language states no visibility.
    pub visibility: Option<String>,
    /// Complete declaration byte range, extended over attached outer
    /// attributes and outer doc comments.
    pub range: ByteRange,
    /// The item syntax element's own byte range, excluding any attached outer
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

impl SyntaxSymbol {
    /// Whether this is the physical name of a Python parameter.
    #[must_use]
    pub fn is_python_parameter(&self) -> bool {
        self.kind == crate::python::PARAMETER_KIND_WORD
            || self.is_python_variadic_parameter()
            || self.is_python_overload_parameter()
    }

    /// Whether this parameter captures an argument list or a keyword dictionary.
    #[must_use]
    pub fn is_python_variadic_parameter(&self) -> bool {
        self.kind == crate::python::VARIADIC_PARAMETER_KIND_WORD
    }

    /// Whether this parameter belongs to one declaration of an overload family.
    #[must_use]
    pub fn is_python_overload_parameter(&self) -> bool {
        self.kind == crate::python::OVERLOAD_PARAMETER_KIND_WORD
    }

    /// Whether this is the physical name of an authored Python loop target.
    #[must_use]
    pub fn is_python_loop_binding(&self) -> bool {
        self.kind == crate::python::LOOP_BINDING_KIND_WORD
    }

    /// Whether this is the physical name of a Python comprehension target.
    #[must_use]
    pub fn is_python_comprehension_binding(&self) -> bool {
        self.kind == crate::python::COMPREHENSION_BINDING_KIND
    }

    /// Whether this is an authored ECMAScript value parameter.
    #[must_use]
    pub fn is_ecmascript_parameter(&self) -> bool {
        self.kind == crate::ecmascript::PARAMETER_KIND
            || self.is_ecmascript_callback_parameter()
            || self.is_javascript_callback_pattern_parameter()
            || self.is_ecmascript_overload_parameter()
            || self.is_javascript_method_parameter()
            || self.is_typescript_signature_parameter()
            || self.is_javascript_scoped_function_parameter()
            || self.is_javascript_shorthand_parameter()
            || self.is_typescript_callable_type_parameter()
            || self.is_typescript_index_signature_parameter()
            || self.is_javascript_wrapped_parameter()
            || self.is_javascript_destructured_parameter()
            || self.is_typescript_destructured_parameter()
    }

    /// Whether this is the physical parameter of a JavaScript function value.
    #[must_use]
    pub fn is_ecmascript_callback_parameter(&self) -> bool {
        self.kind == crate::ecmascript::CALLBACK_PARAMETER_KIND
    }

    /// Whether this is a written binding in a JavaScript function value's parameter pattern.
    #[must_use]
    pub fn is_javascript_callback_pattern_parameter(&self) -> bool {
        self.kind == crate::ecmascript::CALLBACK_PATTERN_PARAMETER_KIND
    }

    /// Whether this parameter belongs to one physical TypeScript overload.
    #[must_use]
    pub fn is_ecmascript_overload_parameter(&self) -> bool {
        self.kind == crate::ecmascript::OVERLOAD_PARAMETER_KIND
    }

    /// Whether this is a selected Cargo library's physical source root.
    #[must_use]
    pub fn is_rust_library_root(&self) -> bool {
        self.kind == "module"
            && self.qualified_name == "."
            && self.syntax_element_kind == Some("source_file")
    }

    /// Whether this is the checked source file of an out-of-line Rust module.
    #[must_use]
    pub fn is_rust_module_source(&self) -> bool {
        self.kind == "module"
            && self.qualified_name == ".."
            && self.syntax_element_kind == Some("source_file")
    }

    /// Whether this parameter belongs to an authored JavaScript method.
    #[must_use]
    pub fn is_javascript_method_parameter(&self) -> bool {
        self.kind == crate::ecmascript::METHOD_PARAMETER_KIND
    }

    /// Whether this is the physical parameter of a TypeScript function signature.
    #[must_use]
    pub fn is_typescript_signature_parameter(&self) -> bool {
        self.kind == crate::ecmascript::SIGNATURE_PARAMETER_KIND
    }

    /// Whether this parameter belongs to a separately anchored JavaScript function.
    #[must_use]
    pub fn is_javascript_scoped_function_parameter(&self) -> bool {
        self.kind == crate::ecmascript::SCOPED_FUNCTION_PARAMETER_KIND
    }

    /// Whether this JavaScript parameter shares its name with a shorthand property.
    #[must_use]
    pub fn is_javascript_shorthand_parameter(&self) -> bool {
        self.kind == crate::ecmascript::SHORTHAND_PARAMETER_KIND
    }

    /// Whether this value parameter is written in a TypeScript callable type.
    #[must_use]
    pub fn is_typescript_callable_type_parameter(&self) -> bool {
        self.kind == crate::ecmascript::CALLABLE_TYPE_PARAMETER_KIND
    }

    /// Whether this value parameter is written in a TypeScript index signature.
    #[must_use]
    pub fn is_typescript_index_signature_parameter(&self) -> bool {
        self.kind == crate::ecmascript::INDEX_SIGNATURE_PARAMETER_KIND
    }

    /// Whether this is a named JavaScript defaulted or rest parameter.
    #[must_use]
    pub fn is_javascript_wrapped_parameter(&self) -> bool {
        self.kind == crate::ecmascript::WRAPPED_PARAMETER_KIND
    }

    /// Whether this is a named JavaScript object or array parameter binding.
    #[must_use]
    pub fn is_javascript_destructured_parameter(&self) -> bool {
        self.kind == crate::ecmascript::DESTRUCTURED_PARAMETER_KIND
    }

    /// Whether this is a binding written in a TypeScript parameter pattern.
    #[must_use]
    pub fn is_typescript_destructured_parameter(&self) -> bool {
        self.kind == crate::ecmascript::TYPED_DESTRUCTURED_PARAMETER_KIND
    }

    /// Whether this is the written type parameter of a TypeScript declaration.
    #[must_use]
    pub fn is_typescript_type_parameter(&self) -> bool {
        self.kind == crate::ecmascript::TYPE_PARAMETER_KIND
    }

    /// Whether this ECMAScript binding uses a package declaration reservation.
    #[must_use]
    pub fn is_ecmascript_package_binding(&self) -> bool {
        self.is_ecmascript_parameter()
            || self.is_typescript_type_parameter()
            || self.is_typescript_method_type_parameter()
            || self.is_typescript_function_type_parameter()
            || self.is_typescript_signature_type_parameter()
    }

    /// Whether this is the written type parameter of a TypeScript method.
    #[must_use]
    pub fn is_typescript_method_type_parameter(&self) -> bool {
        self.kind == crate::ecmascript::METHOD_TYPE_PARAMETER_KIND
    }

    /// Whether this is the written type parameter of a TypeScript function.
    #[must_use]
    pub fn is_typescript_function_type_parameter(&self) -> bool {
        self.kind == crate::ecmascript::FUNCTION_TYPE_PARAMETER_KIND
    }

    /// Whether this is the written type parameter of an anonymous TypeScript signature.
    #[must_use]
    pub fn is_typescript_signature_type_parameter(&self) -> bool {
        self.kind == crate::ecmascript::SIGNATURE_TYPE_PARAMETER_KIND
    }

    /// Whether a checked local macro emits a method at this written name.
    #[must_use]
    pub fn is_rust_macro_method(&self) -> bool {
        self.kind == "function"
            && self.syntax_element_kind == Some("identifier")
            && self.container.is_some()
            && self.name_range == Some(self.item_range)
    }

    /// Whether this binding uses a package-only declaration reservation.
    #[must_use]
    pub fn is_package_binding(&self) -> bool {
        self.is_rust_local_binding()
            || self.is_rust_library_root()
            || self.is_rust_module_source()
            || self.is_rust_macro_method()
            || self.is_python_parameter()
            || self.is_python_loop_binding()
            || self.is_python_comprehension_binding()
            || self.is_ecmascript_package_binding()
    }
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
    /// Exact outer path attributes whose Cargo predicates decide the selected path.
    Conditional {
        /// Authored attribute ranges in source order.
        attributes: Vec<ByteRange>,
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

/// Immutable syntax facts and complete named syntax elements shared by documents at different paths.
///
/// The syntax element table is present while the facts come straight from a parse and absent once an
/// owner that keeps facts for long released it ([`SyntaxDocument::into_facts`]) or restored
/// facts from storage.
///
/// Equality leaves the syntax element table out: the source digest and syntax bounds the facts carry
/// decide it, so released or restored facts equal the parse they came from. `Eq` is not
/// derived: [`SyntaxSymbol`] is not `Eq`.
#[derive(Debug, Clone)]
pub struct SyntaxFacts {
    origin: SyntaxOrigin,
    language: Language,
    syntax_elements: Option<Arc<[SyntaxElement]>>,
    symbols: Vec<SyntaxSymbol>,
    left_out_symbols: Vec<SyntaxSymbol>,
    export_bindings: Option<Vec<SyntaxExportBinding>>,
    macro_invocations: Option<Vec<MacroInvocation>>,
    scope: Option<Vec<Scope>>,
    static_facts: Option<crate::StaticFacts>,
    has_errors: bool,
    variance_errors_only: bool,
    left_out_declarations: usize,
    markdown_facts: Option<MarkdownFacts>,
    source_digest: Option<FileDigest>,
    syntax_limits: Option<crate::SyntaxLimits>,
    invalid_declarations: InvalidDeclarations,
}

/// Parse-error validation of one facts value's declarations, answered once and kept.
///
/// A clone starts empty: a caller that copies facts through [`Arc::make_mut`] may change the
/// declarations the copy holds, so the copy validates its own.
#[derive(Debug, Default)]
struct InvalidDeclarations(
    std::sync::OnceLock<(
        crate::SyntaxLimits,
        Option<std::collections::BTreeSet<ByteRange>>,
    )>,
);

impl Clone for InvalidDeclarations {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl InvalidDeclarations {
    fn retained_bytes(&self) -> usize {
        self.0
            .get()
            .and_then(|(_, invalid)| invalid.as_ref())
            .map_or(0, |invalid| {
                invalid
                    .len()
                    .saturating_mul(std::mem::size_of::<ByteRange>())
            })
    }
}

impl PartialEq for SyntaxFacts {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            origin,
            language,
            syntax_elements: _,
            symbols,
            left_out_symbols,
            export_bindings,
            macro_invocations,
            scope,
            static_facts,
            has_errors,
            variance_errors_only,
            left_out_declarations,
            markdown_facts,
            source_digest,
            syntax_limits,
            invalid_declarations: _,
        } = self;
        *origin == other.origin
            && *language == other.language
            && *symbols == other.symbols
            && *left_out_symbols == other.left_out_symbols
            && *export_bindings == other.export_bindings
            && *macro_invocations == other.macro_invocations
            && *scope == other.scope
            && *static_facts == other.static_facts
            && *has_errors == other.has_errors
            && *variance_errors_only == other.variance_errors_only
            && *left_out_declarations == other.left_out_declarations
            && *markdown_facts == other.markdown_facts
            && *source_digest == other.source_digest
            && *syntax_limits == other.syntax_limits
    }
}

/// Immutable syntax facts for one source file and its project path.
///
/// Cloning a document or placing its facts at another path shares its syntax facts.
/// Equality compares the path, the facts, and the syntax element tables.
#[derive(Debug, Clone)]
pub struct SyntaxDocument {
    path: ProjectPath,
    facts: Arc<SyntaxFacts>,
}

impl PartialEq for SyntaxDocument {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.facts == other.facts
            && self.syntax_elements() == other.syntax_elements()
    }
}

/// Numbers the declarations of every qualified name from 0, in source order.
///
/// Several declarations spelling one qualified name are one logical symbol; the ordinal
/// tells them apart without changing the name. Every provider funnels through
/// [`SyntaxDocument::new`], so the rule holds for every language, and a document
/// assembled again from numbered declarations numbers them the same way.
///
/// One pass with one map lookup per declaration: the provider's syntax element bound already
/// fixed how many declarations reach here, and no count passes the slice's length.
fn number_declarations(symbols: &mut [SyntaxSymbol]) {
    let mut counts: HashMap<&str, usize> = HashMap::with_capacity(symbols.len());
    let mut ordinals = Vec::with_capacity(symbols.len());
    for symbol in symbols.iter() {
        let count = counts.entry(symbol.qualified_name.as_str()).or_default();
        ordinals.push(*count);
        *count += 1;
    }
    for (symbol, ordinal) in symbols.iter_mut().zip(ordinals) {
        symbol.declaration_ordinal = ordinal;
    }
}

/// Keeps Contribution-admitted declarations in `symbols` and returns the other declarations
/// whose names a record may retain, with the count of every declaration left out.
///
/// A declaration whose name no record may retain, such as a quoted member name with a control
/// character, is counted alone. The provider's source and syntax element bounds apply to both
/// collections.
fn leave_out_refused_names(symbols: &mut Vec<SyntaxSymbol>) -> (Vec<SyntaxSymbol>, usize) {
    let retained = |name: &str| symbol_name_is_retained(name, usize::MAX);
    let mut left_out = Vec::new();
    let mut count = 0;
    for symbol in std::mem::take(symbols) {
        if is_portable_name(&symbol.name) && is_portable_name(&symbol.qualified_name) {
            symbols.push(symbol);
            continue;
        }
        count += 1;
        if retained(&symbol.name) && retained(&symbol.qualified_name) {
            left_out.push(symbol);
        }
    }
    (left_out, count)
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

    /// Assembles one document from a provider's extracted facts, numbering the
    /// declarations of every qualified name in source order. Declarations outside the
    /// Contribution name bounds retain their complete syntax facts in the omitted
    /// collection.
    pub(crate) fn new(
        language: Language,
        path: ProjectPath,
        syntax_elements: Vec<SyntaxElement>,
        symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        Self::assemble(language, path, syntax_elements, symbols, has_errors, None)
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
        syntax_elements: Vec<SyntaxElement>,
        mut symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        let mut invocations = Self::invocations(&language, &syntax_elements);
        if let Some(invocations) = &mut invocations {
            rust_macros::capture(
                source.text,
                &syntax_elements,
                invocations,
                Some((&language, limits, &mut symbols)),
            );
        }
        symbols.sort_by_key(|symbol| symbol.item_range.start);
        let mut document = Self::assemble(
            language,
            source.path.clone(),
            syntax_elements,
            symbols,
            has_errors,
            invocations,
        );
        let facts = Arc::make_mut(&mut document.facts);
        facts.source_digest = Some(FileDigest::of(source.text.as_bytes()));
        facts.scope = rust_macros::capture_scope(
            source.text,
            facts.syntax_elements.as_deref().unwrap_or_default(),
            has_errors,
        );
        facts.syntax_limits = Some(limits);
        document
    }

    fn invocations(
        language: &Language,
        syntax_elements: &[SyntaxElement],
    ) -> Option<Vec<MacroInvocation>> {
        (language.name == "rust" && language.dialect.is_none()).then(|| {
            syntax_elements
                .iter()
                .filter(|syntax_element| {
                    syntax_element.kind == "macro_invocation"
                        && syntax_element
                            .parent
                            .and_then(|parent| syntax_elements.get(parent))
                            .is_some_and(|parent| {
                                (parent.kind == "source_file" && parent.parent.is_none())
                                    || (parent.kind == "expression_statement"
                                        && parent
                                            .parent
                                            .and_then(|root| syntax_elements.get(root))
                                            .is_some_and(|root| {
                                                root.kind == "source_file" && root.parent.is_none()
                                            }))
                            })
                })
                .map(|syntax_element| (syntax_element.range, Vec::new(), None))
                .collect()
        })
    }

    fn assemble(
        language: Language,
        path: ProjectPath,
        syntax_elements: Vec<SyntaxElement>,
        mut symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
        captured: Option<Vec<MacroInvocation>>,
    ) -> Self {
        number_declarations(&mut symbols);
        let declaration_syntax_elements =
            declaration_syntax_element_kinds(&syntax_elements, &symbols);
        for symbol in &mut symbols {
            if symbol.syntax_element_kind.is_none() {
                symbol.syntax_element_kind =
                    declaration_syntax_elements.get(&symbol.range).copied();
            }
        }
        let macro_invocations = captured.or_else(|| Self::invocations(&language, &syntax_elements));
        let (left_out_symbols, left_out_declarations) = leave_out_refused_names(&mut symbols);
        Self {
            path,
            facts: Arc::new(SyntaxFacts {
                origin: SyntaxOrigin::Provider,
                language,
                syntax_elements: Some(Arc::from(syntax_elements)),
                symbols,
                left_out_symbols,
                export_bindings: None,
                macro_invocations,
                scope: None,
                static_facts: None,
                has_errors,
                variance_errors_only: false,
                left_out_declarations,
                markdown_facts: None,
                source_digest: None,
                syntax_limits: None,
                invalid_declarations: InvalidDeclarations::default(),
            }),
        }
    }

    /// Keeps project declarations and the complete syntax tree without package-only bindings.
    #[must_use]
    pub fn without_package_bindings(mut self) -> Self {
        if self
            .facts
            .symbols
            .iter()
            .any(SyntaxSymbol::is_package_binding)
        {
            Arc::make_mut(&mut self.facts)
                .symbols
                .retain(|symbol| !symbol.is_package_binding());
        }
        self
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

    /// Keeps the provider's checked variance coverage separate from raw parser errors.
    pub(crate) fn with_variance_errors_only(mut self, proven: bool) -> Self {
        if proven {
            Arc::make_mut(&mut self.facts).variance_errors_only = true;
        }
        self
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

    pub(crate) fn with_static_facts(mut self, facts: Option<crate::StaticFacts>) -> Self {
        Arc::make_mut(&mut self.facts).static_facts = facts;
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
        let syntax_elements = facts.syntax_elements.clone().unwrap_or_default();
        if facts.language.name == "rust" && facts.language.dialect.is_none() {
            facts.scope = rust_macros::capture_scope(source, &syntax_elements, facts.has_errors);
        }
        if let Some(invocations) = &mut facts.macro_invocations {
            rust_macros::capture(source, &syntax_elements, invocations, None);
        }
        self
    }

    /// Returns every named syntax element in pre-order.
    #[must_use]
    pub fn syntax_elements(&self) -> &[SyntaxElement] {
        self.facts.syntax_elements().unwrap_or_default()
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

    /// Returns syntax elements covering byte position, outermost first.
    #[must_use]
    pub fn syntax_elements_at(&self, position: u64) -> Vec<&SyntaxElement> {
        self.syntax_elements()
            .iter()
            .filter(|syntax_element| syntax_element.range.contains(position))
            .collect()
    }

    /// Returns the innermost syntax element at `range`: the one spanning it exactly, or under
    /// [`SyntaxMatch::Enclosing`] the one whose span contains it. `None` when no syntax element matches.
    ///
    /// Pre-order lists every syntax element after its ancestors, so the last match is the innermost.
    #[must_use]
    pub fn syntax_element_at(
        &self,
        range: ByteRange,
        matching: SyntaxMatch,
    ) -> Option<&SyntaxElement> {
        self.syntax_elements()
            .iter()
            .rev()
            .find(|syntax_element| match matching {
                SyntaxMatch::Exact => syntax_element.range == range,
                SyntaxMatch::Enclosing => syntax_element.range.encloses(range),
            })
    }

    /// Clones the shared path-independent facts.
    #[must_use]
    pub fn shared_facts(&self) -> Arc<SyntaxFacts> {
        Arc::clone(&self.facts)
    }

    /// Returns shared path-independent facts with the complete syntax element table released.
    ///
    /// An owner that keeps facts for long, such as an index, holds no syntax element rows: a read that
    /// needs them parses the captured bytes again under the same bounds.
    #[must_use]
    pub fn into_facts(mut self) -> Arc<SyntaxFacts> {
        if self.facts.syntax_elements.is_some() {
            Arc::make_mut(&mut self.facts).syntax_elements = None;
        }
        self.facts
    }
}

impl SyntaxFacts {
    /// Adds the source-file declaration of a library established by its Cargo manifest.
    ///
    /// The caller supplies the selected crate name. The retained Rust facts must match
    /// the complete source bytes and hold no parse error or omitted declaration.
    #[must_use]
    pub fn with_rust_library_root(mut self, name: &str, source: &str) -> Option<Self> {
        let root = self.rust_source_module(name, source, ".")?;
        self.symbols.insert(0, root);
        Some(self)
    }

    /// Adds checked authored Rust macro method names after other syntax declarations.
    ///
    /// The caller establishes the selected macro arm and the admitted inherent owner.
    #[must_use]
    pub fn with_rust_macro_methods(mut self, methods: Vec<SyntaxSymbol>) -> Self {
        self.symbols.extend(methods);
        self.symbols.sort_by_key(|symbol| symbol.item_range.start);
        self
    }

    /// Captures a separate whole-file declaration for an out-of-line Rust module.
    /// The caller establishes the authored module path and package owner.
    #[must_use]
    pub fn rust_module_source(
        &self,
        name: &str,
        path: &ProjectPath,
        source: &str,
    ) -> Option<Arc<Self>> {
        let symbol = self.rust_source_module(name, source, "..")?;
        let mut facts = SyntaxDocument::empty(self.language.clone(), path.clone()).into_facts();
        Arc::get_mut(&mut facts)?.symbols.push(symbol);
        Some(facts)
    }

    fn rust_source_module(
        &self,
        name: &str,
        source: &str,
        qualified: &str,
    ) -> Option<SyntaxSymbol> {
        if self.language.name != "rust"
            || self.has_errors
            || self.left_out_declaration_count() != 0
            || self.source_digest != Some(FileDigest::of(source.as_bytes()))
            || !is_portable_name(name)
        {
            return None;
        }
        let range = ByteRange {
            start: 0,
            end: u64::try_from(source.len()).ok()?,
        };
        Some(SyntaxSymbol {
            declaration_ordinal: 0,
            qualified_name: qualified.to_owned(),
            container: None,
            name: name.to_owned(),
            kind: "module",
            syntax_element_kind: Some("source_file"),
            kinds: vec![SymbolKind::Module],
            visibility: None,
            range,
            item_range: range,
            name_range: None,
            body_range: Some(range),
            signatures: Arc::from([]),
            documentation: Arc::from([]),
            documentation_ranges: Vec::new(),
            module_path: None,
            python_overload: None,
        })
    }

    /// Returns captured source-based name-resolution evidence, when recorded.
    #[must_use]
    pub const fn static_facts(&self) -> Option<&crate::StaticFacts> {
        self.static_facts.as_ref()
    }

    /// Returns the names with a captured Python method receiver binding.
    #[must_use]
    pub fn python_receiver_names(&self) -> BTreeSet<ByteRange> {
        if self.language().name != "python" {
            return BTreeSet::new();
        }
        self.static_facts()
            .into_iter()
            .flat_map(crate::StaticFacts::bindings)
            .filter_map(|binding| {
                matches!(
                    binding.kind,
                    crate::StaticBindingKind::PythonReceiver { .. }
                )
                .then_some(binding.name)
            })
            .collect()
    }

    /// Whether the Rust binding was recovered in a function disjoint from macro effects.
    ///
    /// Those bindings use their ordinary identities and have a later package admission
    /// reservation, so they cannot consume slots for previously retained declarations.
    #[must_use]
    pub fn is_disjoint_rust_binding(&self, symbol: &SyntaxSymbol) -> bool {
        symbol.is_rust_local_binding()
            && !symbol.is_rust_receiver_binding()
            && !symbol.is_rust_generic_parameter()
            && !symbol.is_rust_explicit_local_binding()
            && self.static_facts().is_some_and(|facts| {
                !facts.rust_textual_macro_calls().is_empty()
                    || !facts.rust_macro_barriers().is_empty()
            })
    }

    /// Returns the complete named tree in pre-order, or `None` when these facts hold none.
    #[must_use]
    pub fn syntax_elements(&self) -> Option<&[SyntaxElement]> {
        self.syntax_elements.as_deref()
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

    /// Qualified names of TypeScript methods that can own a value parameter.
    #[must_use]
    pub fn typescript_method_names(&self) -> BTreeSet<&str> {
        if self.language.name != "typescript"
            || !self
                .symbols
                .iter()
                .any(SyntaxSymbol::is_ecmascript_parameter)
        {
            return BTreeSet::new();
        }
        self.symbols
            .iter()
            .filter(|symbol| symbol.kind == "method")
            .map(|symbol| symbol.qualified_name.as_str())
            .collect()
    }

    /// Number of established declaration candidates before optional package bindings.
    #[must_use]
    pub fn primary_symbols_len(&self) -> usize {
        if matches!(
            self.language.name.as_str(),
            "rust" | "python" | "javascript" | "typescript"
        ) {
            self.symbols
                .iter()
                .filter(|symbol| !symbol.is_package_binding())
                .count()
        } else {
            self.symbols.len()
        }
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

    /// Item ranges of these facts' declarations that contain a parser error or sit inside an
    /// error subtree, as [`crate::SyntaxProvider::invalid_declarations`] answers them.
    ///
    /// `source` must be the bytes these facts were parsed from. The first call under `limits`
    /// parses `source` again; later calls under the same limits reuse that answer, so a pass
    /// that runs on every index build validates a held file once. `None` is a refusal: no
    /// provider for the language or a crossed syntax bound.
    #[must_use]
    pub fn invalid_declarations(
        &self,
        source: crate::SyntaxSource<'_>,
        limits: crate::SyntaxLimits,
    ) -> Option<std::collections::BTreeSet<ByteRange>> {
        let validate = || {
            let ranges = self
                .symbols
                .iter()
                .map(|symbol| symbol.item_range)
                .collect::<Vec<_>>();
            crate::registry::provider_for_language(&self.language)?
                .invalid_declarations(source, limits, &ranges)
                .ok()
        };
        let (validated, invalid) = self
            .invalid_declarations
            .0
            .get_or_init(|| (limits, validate()));
        if *validated == limits {
            invalid.clone()
        } else {
            validate()
        }
    }

    /// Whether the provider explained every parser error by a checked TypeScript variance
    /// annotation.
    #[must_use]
    pub const fn variance_errors_only(&self) -> bool {
        self.variance_errors_only
    }

    /// Whether the provider parsed the declaration inventory or explained each parser error
    /// by a checked TypeScript variance annotation.
    #[must_use]
    pub const fn declaration_syntax_complete(&self) -> bool {
        !self.has_errors || self.variance_errors_only
    }

    /// Returns Markdown block and inline facts when the Markdown provider produced them.
    #[must_use]
    pub const fn markdown_facts(&self) -> Option<&MarkdownFacts> {
        self.markdown_facts.as_ref()
    }
}

fn declaration_syntax_element_kinds(
    syntax_elements: &[SyntaxElement],
    symbols: &[SyntaxSymbol],
) -> BTreeMap<ByteRange, &'static str> {
    let mut ranges = symbols
        .iter()
        .map(|symbol| symbol.range)
        .collect::<HashSet<_>>();
    let mut declaration_syntax_elements = BTreeMap::new();
    for syntax_element in syntax_elements {
        if ranges.remove(&syntax_element.range) {
            declaration_syntax_elements.insert(syntax_element.range, syntax_element.kind);
        }
    }
    declaration_syntax_elements
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
            syntax_elements: None,
            symbols: parts.symbols,
            left_out_symbols: parts.left_out_symbols,
            export_bindings: parts.export_bindings,
            macro_invocations: parts.macro_invocations,
            scope: parts.scope,
            static_facts: parts.static_facts,
            has_errors: parts.has_errors,
            variance_errors_only: parts.variance_errors_only,
            left_out_declarations: parts.left_out_declarations,
            markdown_facts: parts.markdown_facts,
            source_digest: Some(parts.source_digest),
            syntax_limits: Some(limits),
            invalid_declarations: InvalidDeclarations::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_core::{PROVIDER_SYMBOL_ID_BYTES_MAX, symbol_identity};

    use super::*;
    use crate::contribution::DocumentPlacement;
    use crate::provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};

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
            declaration_ordinal: 0,
            container: None,
            kind: "mapping_entry",
            syntax_element_kind: None,
            kinds: Vec::new(),
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
        assert!(document.syntax_elements().is_empty());
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
        assert_eq!(second.syntax_elements(), first.syntax_elements());
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
        assert_eq!(placed.syntax_elements(), tsx.syntax_elements());
        assert_eq!(placed.symbols(), tsx.symbols());
    }

    #[test]
    fn indexed_facts_release_syntax_element_table_and_keep_first_declaration_match() {
        let range = ByteRange { start: 7, end: 8 };
        let document = SyntaxDocument::new(
            language(),
            path(),
            vec![
                SyntaxElement {
                    kind: "first_kind",
                    range,
                    parent: None,
                    has_error: false,
                },
                SyntaxElement {
                    kind: "second_kind",
                    range,
                    parent: Some(0),
                    has_error: true,
                },
            ],
            vec![symbol("entry", range.start)],
            false,
        );
        let syntax_element_table = Arc::downgrade(
            document
                .facts
                .syntax_elements
                .as_ref()
                .expect("a parsed document holds its syntax element table"),
        );
        assert_eq!(
            document
                .syntax_elements()
                .iter()
                .find(|syntax_element| syntax_element.range == range)
                .map(|syntax_element| syntax_element.kind),
            Some("first_kind")
        );

        let parsed = document.facts().clone();
        let facts = document.into_facts();
        assert_eq!(
            *facts, parsed,
            "releasing the syntax element table keeps the facts equal"
        );
        assert!(
            parsed
                .syntax_elements()
                .is_some_and(|syntax_elements| syntax_elements.len() == 2)
        );
        assert_eq!(facts.syntax_elements(), None);
        let heap = |facts: &SyntaxFacts| {
            get_size2::GetSize::get_heap_size_with_tracker(
                facts,
                get_size2::StandardTracker::default(),
            )
            .0
        };
        let mut without_syntax_elements = parsed.clone();
        without_syntax_elements.syntax_elements = None;
        assert_eq!(
            heap(&parsed),
            heap(&without_syntax_elements) + 2 * std::mem::size_of::<SyntaxElement>(),
            "a held syntax element table counts toward the retained bytes"
        );
        drop(parsed);
        assert!(syntax_element_table.upgrade().is_none());
        assert_eq!(facts.symbols()[0].syntax_element_kind, Some("first_kind"));
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
    fn documents_are_equal_when_path_facts_and_syntax_element_tables_agree() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);

        let moved = document.at_path(ProjectPath::new("src/moved.rs").expect("valid path"));
        let released = SyntaxDocument {
            path: path.clone(),
            facts: document.clone().into_facts(),
        };

        assert_eq!(document, rust_document(&path, NESTED_SOURCE));
        assert_ne!(document, moved, "the path differs");
        assert_ne!(document, released, "the syntax element table differs");
        assert_eq!(*document.facts(), *released.facts());
    }

    #[test]
    fn syntax_element_at_answers_the_innermost_exact_or_enclosing_syntax_element() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);
        let literal = range_of("1\n");
        let literal = ByteRange {
            end: literal.start + 1,
            ..literal
        };

        let exact = document
            .syntax_element_at(literal, SyntaxMatch::Exact)
            .expect("the literal syntax element");
        assert_eq!(exact.range, literal);
        assert_eq!(exact.kind, "integer_literal");
        let inside = ByteRange {
            start: literal.start,
            end: literal.start,
        };
        assert_eq!(document.syntax_element_at(inside, SyntaxMatch::Exact), None);
        assert_eq!(
            document
                .syntax_element_at(inside, SyntaxMatch::Enclosing)
                .map(|syntax_element| syntax_element.kind),
            Some("integer_literal")
        );
        let function = declared(&document, "light").item_range;
        let across = ByteRange {
            start: function.start,
            end: literal.end,
        };
        assert_eq!(
            document
                .syntax_element_at(across, SyntaxMatch::Enclosing)
                .map(|syntax_element| syntax_element.range),
            Some(function)
        );
        let past = ByteRange {
            start: 0,
            end: NESTED_SOURCE.len() as u64 + 1,
        };
        assert_eq!(
            document.syntax_element_at(past, SyntaxMatch::Enclosing),
            None
        );
    }

    #[test]
    fn a_released_syntax_element_table_answers_no_syntax_element() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let document = rust_document(&path, NESTED_SOURCE);
        let released = SyntaxDocument {
            path: path.clone(),
            facts: document.into_facts(),
        };
        assert!(released.syntax_elements().is_empty());
        assert!(released.syntax_element_places(|_, _| true).is_empty());
        assert_eq!(
            released.syntax_element_at(ByteRange { start: 0, end: 1 }, SyntaxMatch::Enclosing),
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

    /// The keys `(qualified_name, declaration_ordinal)` every declaration of `document`
    /// carries, in source order.
    fn keys(document: &SyntaxDocument) -> Vec<(&str, usize)> {
        document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.declaration_ordinal))
            .collect()
    }

    /// Two declarations of one name both keep the name and take ordinals 0 and 1.
    #[test]
    fn test_two_declarations_under_one_name_keep_it_and_number_apart() {
        let document = document(&["port", "port"]);
        assert_eq!(keys(&document), [("port", 0), ("port", 1)]);
    }

    /// Two repeated names interleaved in one document number apart from each other, and a
    /// name one declaration spells keeps ordinal 0.
    #[test]
    fn test_interleaved_repeated_names_number_independently() {
        let document = document(&["version", "port", "host", "port", "host", "port"]);
        assert_eq!(
            keys(&document),
            [
                ("version", 0),
                ("port", 0),
                ("host", 0),
                ("port", 1),
                ("host", 1),
                ("port", 2)
            ]
        );
    }

    /// Numbering a numbered document again assigns the same ordinals.
    #[test]
    fn test_assembling_numbered_declarations_again_keeps_their_ordinals() {
        let first = document(&["port", "port", "host"]);
        let again = SyntaxDocument::new(
            language(),
            path(),
            Vec::new(),
            first.symbols().to_vec(),
            false,
        );
        assert_eq!(keys(&again), keys(&first));
    }

    /// The provider address keeps the first declaration on its name's address and keeps
    /// every later one apart by its ordinal.
    #[test]
    fn test_repeated_declarations_take_distinct_provider_addresses() {
        let document = document(&["updates > package-ecosystem", "updates > package-ecosystem"]);
        let addresses = document
            .symbols()
            .iter()
            .map(|symbol| symbol.provider_address("yaml", path().as_str()))
            .collect::<Vec<_>>();
        let first = symbol_identity("yaml", path().as_str(), "updates > package-ecosystem");
        assert_eq!(addresses, [first.clone(), format!("{first}#1")]);
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

    /// Declarations of one name at the bound stay, numbered apart, beside a name past it.
    #[test]
    fn test_repeated_names_at_the_bound_stay_numbered() {
        let at_bound = "n".repeat(PROVIDER_SYMBOL_ID_BYTES_MAX);
        let past_bound = "n".repeat(PROVIDER_SYMBOL_ID_BYTES_MAX + 1);
        let document = document(&[at_bound.as_str(), at_bound.as_str(), past_bound.as_str()]);
        assert_eq!(
            keys(&document),
            [(at_bound.as_str(), 0), (at_bound.as_str(), 1)]
        );
        assert_eq!(document.left_out_declaration_count(), 1);
    }
}
