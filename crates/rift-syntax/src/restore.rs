//! Checked construction of captured syntax facts.

use rift_error::{RiftError, errors};

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rift_core::FileDigest;
use rift_protocol::read::Language;

use crate::{
    ByteRange, MarkdownBlockFact, MarkdownHeadingFact, MarkdownLinkFact,
    MarkdownReferenceCandidate, ShippedLanguage, SyntaxFacts, SyntaxLimits, SyntaxSymbol,
};

/// Every field needed to reconstruct normalized per-file facts.
///
/// Qualified names already carry the provider's duplicate suffixes. Checked construction
/// preserves them, attached documentation, parser errors, and omission counts unchanged.
#[derive(Debug, Clone)]
pub struct SyntaxFactsParts {
    /// Actual provider language and dialect.
    pub language: Language,
    /// Normalized declarations in source order.
    pub symbols: Vec<SyntaxSymbol>,
    /// Whether the provider reported parser errors.
    pub has_errors: bool,
    /// Number of extracted declarations the provider omitted.
    pub left_out_declarations: usize,
    /// Complete Markdown structure, when present.
    pub markdown_facts: Option<crate::MarkdownFacts>,
    /// Digest recorded for the original source bytes.
    pub source_digest: FileDigest,
}

/// All Markdown fields needed for checked construction.
#[derive(Debug, Clone)]
pub struct MarkdownFactsParts {
    /// Blocks in source order.
    pub blocks: Vec<MarkdownBlockFact>,
    /// Headings in source order.
    pub headings: Vec<MarkdownHeadingFact>,
    /// Links in source order.
    pub links: Vec<MarkdownLinkFact>,
    /// Inline code candidates in source order.
    pub reference_candidates: Vec<MarkdownReferenceCandidate>,
    /// Sorted parser error ranges.
    pub error_ranges: Vec<ByteRange>,
    /// Sorted ranges omitted by metadata policy.
    pub omitted_ranges: Vec<ByteRange>,
}

#[derive(Clone, Copy)]
pub(crate) struct MarkdownFactsView<'facts> {
    blocks: &'facts [MarkdownBlockFact],
    headings: &'facts [MarkdownHeadingFact],
    links: &'facts [MarkdownLinkFact],
    reference_candidates: &'facts [MarkdownReferenceCandidate],
    error_ranges: &'facts [ByteRange],
    omitted_ranges: &'facts [ByteRange],
}

impl MarkdownFactsParts {
    pub(crate) fn view(&self) -> MarkdownFactsView<'_> {
        MarkdownFactsView {
            blocks: &self.blocks,
            headings: &self.headings,
            links: &self.links,
            reference_candidates: &self.reference_candidates,
            error_ranges: &self.error_ranges,
            omitted_ranges: &self.omitted_ranges,
        }
    }
}

impl crate::MarkdownFacts {
    pub(crate) fn view(&self) -> MarkdownFactsView<'_> {
        MarkdownFactsView {
            blocks: self.blocks(),
            headings: self.headings(),
            links: self.links(),
            reference_candidates: self.reference_candidates(),
            error_ranges: self.error_ranges(),
            omitted_ranges: self.omitted_ranges(),
        }
    }
}

/// Resolves captured names through the shipped provider's own vocabulary.
///
/// Returned names retain the provider's static storage. Decoded strings are neither
/// leaked nor interned in a process-wide collection.
#[derive(Debug)]
pub struct SyntaxNames {
    shipped: ShippedLanguage,
    grammar: &'static tree_sitter::Language,
}

/// Retains each shipped grammar once so its names outlive parsed trees.
fn grammar(shipped: ShippedLanguage) -> &'static tree_sitter::Language {
    static RUST: OnceLock<tree_sitter::Language> = OnceLock::new();
    static JAVASCRIPT: OnceLock<tree_sitter::Language> = OnceLock::new();
    static TYPESCRIPT: OnceLock<tree_sitter::Language> = OnceLock::new();
    static TSX: OnceLock<tree_sitter::Language> = OnceLock::new();
    static MARKDOWN: OnceLock<tree_sitter::Language> = OnceLock::new();
    static JSON: OnceLock<tree_sitter::Language> = OnceLock::new();
    static YAML: OnceLock<tree_sitter::Language> = OnceLock::new();
    static TOML: OnceLock<tree_sitter::Language> = OnceLock::new();
    static HTML: OnceLock<tree_sitter::Language> = OnceLock::new();
    static ANGULAR: OnceLock<tree_sitter::Language> = OnceLock::new();
    static CSS: OnceLock<tree_sitter::Language> = OnceLock::new();
    static VUE: OnceLock<tree_sitter::Language> = OnceLock::new();
    static SVELTE: OnceLock<tree_sitter::Language> = OnceLock::new();
    static C: OnceLock<tree_sitter::Language> = OnceLock::new();
    static CPP: OnceLock<tree_sitter::Language> = OnceLock::new();
    static CYTHON: OnceLock<tree_sitter::Language> = OnceLock::new();
    static PYTHON: OnceLock<tree_sitter::Language> = OnceLock::new();

    match shipped {
        ShippedLanguage::Rust => RUST.get_or_init(crate::rust::rust_grammar),
        ShippedLanguage::JavaScript => {
            JAVASCRIPT.get_or_init(crate::javascript::javascript_grammar)
        }
        ShippedLanguage::TypeScript => {
            TYPESCRIPT.get_or_init(|| crate::TypeScriptDialect::TypeScript.grammar())
        }
        ShippedLanguage::TypeScriptTsx => {
            TSX.get_or_init(|| crate::TypeScriptDialect::Tsx.grammar())
        }
        ShippedLanguage::Markdown => MARKDOWN.get_or_init(crate::markdown::markdown_grammar),
        ShippedLanguage::Json | ShippedLanguage::Jsonc => {
            JSON.get_or_init(crate::json::json_grammar)
        }
        ShippedLanguage::Html => HTML.get_or_init(crate::html::html_grammar),
        ShippedLanguage::HtmlAngular => ANGULAR.get_or_init(crate::web_component::angular_grammar),
        ShippedLanguage::Css => CSS.get_or_init(crate::css::css_grammar),
        ShippedLanguage::Vue => VUE.get_or_init(crate::web_component::vue_grammar),
        ShippedLanguage::Svelte => SVELTE.get_or_init(crate::web_component::svelte_grammar),
        ShippedLanguage::C => C.get_or_init(|| crate::native::grammar(shipped)),
        ShippedLanguage::Cpp => CPP.get_or_init(|| crate::native::grammar(shipped)),
        ShippedLanguage::Cython => CYTHON.get_or_init(|| crate::native::grammar(shipped)),
        ShippedLanguage::Yaml => YAML.get_or_init(crate::yaml::yaml_grammar),
        ShippedLanguage::Toml => TOML.get_or_init(crate::toml::toml_grammar),
        ShippedLanguage::Python => PYTHON.get_or_init(|| tree_sitter_python::LANGUAGE.into()),
    }
}

impl SyntaxNames {
    /// Selects the shipped grammar for an exact language and dialect.
    #[must_use]
    pub fn new(language: &Language) -> Option<Self> {
        let shipped = crate::registry::shipped_languages()
            .find(|(_, provider)| provider.language() == language)
            .map(|(definition, _)| definition.shipped())?;
        let grammar = grammar(shipped);
        Some(Self { shipped, grammar })
    }

    /// Returns the provider's kind word when the provider defines it.
    #[must_use]
    pub fn symbol_kind(&self, name: &str) -> Option<&'static str> {
        let base = match self.shipped {
            ShippedLanguage::Rust => crate::rust::restored_symbol_kind(name),
            ShippedLanguage::JavaScript => {
                crate::ecmascript::restored_symbol_kind(crate::javascript::javascript_kinds(), name)
            }
            ShippedLanguage::TypeScript => crate::ecmascript::restored_symbol_kind(
                crate::TypeScriptDialect::TypeScript.kinds(),
                name,
            ),
            ShippedLanguage::TypeScriptTsx => {
                crate::ecmascript::restored_symbol_kind(crate::TypeScriptDialect::Tsx.kinds(), name)
            }
            ShippedLanguage::Markdown => crate::markdown::restored_symbol_kind(name),
            ShippedLanguage::Json | ShippedLanguage::Jsonc => {
                crate::json::restored_symbol_kind(name)
            }
            ShippedLanguage::Html => {
                crate::html::restored_symbol_kind(name).or_else(|| embedded_symbol_kind(name))
            }
            ShippedLanguage::Css => crate::css::restored_symbol_kind(name),
            ShippedLanguage::HtmlAngular | ShippedLanguage::Vue | ShippedLanguage::Svelte => {
                crate::web_component::restored_symbol_kind(name)
                    .or_else(|| embedded_symbol_kind(name))
            }
            ShippedLanguage::C | ShippedLanguage::Cpp | ShippedLanguage::Cython => {
                crate::native::restored_symbol_kind(name)
            }
            ShippedLanguage::Yaml => crate::yaml::restored_symbol_kind(name),
            ShippedLanguage::Toml => crate::toml::restored_symbol_kind(name),
            ShippedLanguage::Python => crate::python::restored_symbol_kind(name),
        };
        base.or_else(|| match self.shipped {
            ShippedLanguage::Html
            | ShippedLanguage::HtmlAngular
            | ShippedLanguage::Css
            | ShippedLanguage::Vue
            | ShippedLanguage::Svelte
            | ShippedLanguage::JavaScript
            | ShippedLanguage::TypeScript
            | ShippedLanguage::TypeScriptTsx => crate::tailwind::restored_symbol_kind(name),
            _ => None,
        })
    }

    /// Returns a named grammar node kind, or `None` for an unknown spelling.
    #[must_use]
    pub fn node_kind(&self, name: &str) -> Option<&'static str> {
        let native = grammar_node_kind(self.grammar, name);
        if native.is_some() {
            return native;
        }
        self.embedded_languages()
            .iter()
            .find_map(|shipped| grammar_node_kind(grammar(*shipped), name))
    }

    fn embedded_languages(&self) -> &'static [ShippedLanguage] {
        match self.shipped {
            ShippedLanguage::Html
            | ShippedLanguage::HtmlAngular
            | ShippedLanguage::Vue
            | ShippedLanguage::Svelte => &[
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::Css,
            ],
            ShippedLanguage::TypeScript | ShippedLanguage::TypeScriptTsx => {
                &[ShippedLanguage::HtmlAngular]
            }
            _ => &[],
        }
    }

    fn accepts_embedded_language(&self, language: &Language) -> bool {
        let Some(other) = Self::new(language) else {
            return false;
        };
        self.embedded_languages().contains(&other.shipped)
    }

    pub(crate) fn grammar(&self) -> &'static tree_sitter::Language {
        self.grammar
    }
}

pub(crate) fn range(source: &str, range: ByteRange) -> Result<(), RiftError> {
    let start =
        usize::try_from(range.start).map_err(|_| errors::syntax::facts_range_invalid().error())?;
    let end =
        usize::try_from(range.end).map_err(|_| errors::syntax::facts_range_invalid().error())?;
    if start > end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return Err(errors::syntax::facts_range_invalid().error());
    }
    Ok(())
}

fn contains(outer: ByteRange, inner: ByteRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn ordered(ranges: impl IntoIterator<Item = ByteRange>) -> Result<(), RiftError> {
    let mut previous = None;
    for range in ranges {
        if previous.is_some_and(|start| start > range.start) {
            return Err(errors::syntax::facts_order_invalid().error());
        }
        previous = Some(range.start);
    }
    Ok(())
}

fn count(count: usize, limits: SyntaxLimits) -> Result<(), RiftError> {
    if count > limits.syntax_nodes_max() {
        return Err(errors::syntax::facts_count_exceeded().error());
    }
    Ok(())
}

pub(crate) fn validate_symbols(
    source: &str,
    limits: SyntaxLimits,
    parts: &SyntaxFactsParts,
) -> Result<(), RiftError> {
    if source.len() > limits.source_bytes_max() {
        return Err(errors::syntax::facts_source_too_large().error());
    }
    if parts.source_digest != FileDigest::of(source.as_bytes()) {
        return Err(errors::syntax::facts_source_mismatch().error());
    }
    count(
        parts
            .symbols
            .len()
            .checked_add(parts.left_out_declarations)
            .ok_or_else(|| errors::syntax::facts_count_exceeded().error())?,
        limits,
    )?;
    let names = SyntaxNames::new(&parts.language)
        .ok_or_else(|| errors::syntax::facts_kind_invalid().error())?;
    ordered(parts.symbols.iter().map(|symbol| symbol.range))?;
    let mut positions = BTreeMap::new();
    for (position, symbol) in parts.symbols.iter().enumerate() {
        if !rift_core::is_portable_name(&symbol.name)
            || !rift_core::is_portable_name(&symbol.qualified_name)
            || positions
                .insert(symbol.qualified_name.as_str(), position)
                .is_some()
        {
            return Err(errors::syntax::facts_name_invalid().error());
        }
        validate_symbol(source, limits, &names, &parts.language, symbol)?;
    }
    validate_symbol_parents(&parts.symbols, limits, &positions)
}

fn text(value: &str, limits: SyntaxLimits) -> Result<(), RiftError> {
    if value.len() > limits.source_bytes_max() {
        return Err(errors::syntax::facts_text_exceeded().error());
    }
    Ok(())
}

fn validate_signature(
    signature: &rift_protocol::read::Signature,
    names: &SyntaxNames,
    language: &Language,
    limits: SyntaxLimits,
) -> Result<(), RiftError> {
    text(&signature.display, limits)?;
    // The shared syntax walk emits source headers; semantic fields are derived later.
    let language_valid =
        signature.language == *language || names.accepts_embedded_language(&signature.language);
    if !language_valid
        || !signature.links.is_empty()
        || signature.receiver.is_some()
        || !signature.parameters.is_empty()
        || !signature.returns.is_empty()
        || !signature.type_parameters.is_empty()
        || !signature.throws.is_empty()
        || !signature.effects.is_empty()
        || !signature.extensions.is_empty()
    {
        return Err(errors::syntax::facts_structure_invalid().error());
    }
    Ok(())
}

fn validate_symbol(
    source: &str,
    limits: SyntaxLimits,
    names: &SyntaxNames,
    language: &Language,
    symbol: &SyntaxSymbol,
) -> Result<(), RiftError> {
    if names.symbol_kind(symbol.kind).is_none()
        || symbol
            .node_kind
            .is_some_and(|kind| names.node_kind(kind).is_none())
    {
        return Err(errors::syntax::facts_kind_invalid().error());
    }
    range(source, symbol.range)?;
    range(source, symbol.item_range)?;
    if !contains(symbol.range, symbol.item_range) {
        return Err(errors::syntax::facts_range_invalid().error());
    }
    for inner in symbol
        .name_range
        .into_iter()
        .chain(symbol.body_range)
        .chain(symbol.documentation_ranges.iter().copied())
    {
        range(source, inner)?;
        if !contains(symbol.range, inner) {
            return Err(errors::syntax::facts_range_invalid().error());
        }
    }
    count(symbol.facets.len(), limits)?;
    count(symbol.documentation_ranges.len(), limits)?;
    count(symbol.signatures.len(), limits)?;
    count(symbol.documentation.len(), limits)?;
    if let Some(visibility) = &symbol.visibility {
        text(visibility, limits)?;
    }
    for documentation in symbol.documentation.iter() {
        text(&documentation.text, limits)?;
    }
    for signature in symbol.signatures.iter() {
        validate_signature(signature, names, language, limits)?;
    }
    ordered(symbol.documentation_ranges.iter().copied())?;
    Ok(())
}

fn validate_symbol_parents(
    symbols: &[SyntaxSymbol],
    limits: SyntaxLimits,
    positions: &BTreeMap<&str, usize>,
) -> Result<(), RiftError> {
    let mut depths = vec![0_usize; symbols.len()];
    for (position, symbol) in symbols.iter().enumerate() {
        let depth = match symbol.container.as_deref() {
            None => 1,
            Some(container) => {
                if !rift_core::is_portable_name(container) {
                    return Err(errors::syntax::facts_name_invalid().error());
                }
                // Duplicate suffixing changes qualified names but retains authored containers.
                match positions.get(container).copied() {
                    None => 1,
                    Some(parent) => {
                        if parent >= position || !contains(symbols[parent].range, symbol.range) {
                            return Err(errors::syntax::facts_reference_invalid().error());
                        }
                        depths[parent]
                            .checked_add(1)
                            .ok_or_else(|| errors::syntax::facts_depth_exceeded().error())?
                    }
                }
            }
        };
        if depth > limits.syntax_depth_max() {
            return Err(errors::syntax::facts_depth_exceeded().error());
        }
        depths[position] = depth;
    }
    Ok(())
}

pub(crate) fn validate_markdown(
    source: &str,
    symbols: &[SyntaxSymbol],
    limits: SyntaxLimits,
    parts: MarkdownFactsView<'_>,
) -> Result<(), RiftError> {
    if source.len() > limits.source_bytes_max() {
        return Err(errors::syntax::facts_source_too_large().error());
    }
    for length in [
        parts.blocks.len(),
        parts.headings.len(),
        parts.links.len(),
        parts.reference_candidates.len(),
        parts.error_ranges.len(),
        parts.omitted_ranges.len(),
    ] {
        count(length, limits)?;
    }
    ordered(parts.blocks.iter().map(|block| block.range))?;
    ordered(parts.headings.iter().map(|heading| heading.range))?;
    ordered(parts.links.iter().map(|link| link.range))?;
    ordered(
        parts
            .reference_candidates
            .iter()
            .map(|candidate| candidate.range),
    )?;
    ordered(parts.error_ranges.iter().copied())?;
    ordered(parts.omitted_ranges.iter().copied())?;
    let lines = rift_core::line::line_starts(source);
    let blocks: std::collections::BTreeSet<_> =
        parts.blocks.iter().map(|block| block.range).collect();
    validate_headings(source, symbols, limits, parts.headings)?;
    validate_blocks(source, parts.blocks, parts.headings, &lines, limits)?;
    validate_links(source, parts.links, &blocks)?;
    for candidate in parts.reference_candidates {
        range(source, candidate.range)?;
        if !blocks.contains(&candidate.block_range)
            || !contains(candidate.block_range, candidate.range)
        {
            return Err(errors::syntax::facts_reference_invalid().error());
        }
    }
    for error in parts.error_ranges.iter().chain(parts.omitted_ranges) {
        range(source, *error)?;
    }
    Ok(())
}

fn validate_headings(
    source: &str,
    symbols: &[SyntaxSymbol],
    limits: SyntaxLimits,
    headings: &[MarkdownHeadingFact],
) -> Result<(), RiftError> {
    let mut depths = vec![0_usize; headings.len()];
    for (position, heading) in headings.iter().enumerate() {
        range(source, heading.range)?;
        let symbol = symbols
            .get(heading.symbol_index)
            .ok_or_else(|| errors::syntax::facts_reference_invalid().error())?;
        if symbol.kind != crate::markdown::HEADING_KIND_WORD
            || !contains(symbol.range, heading.range)
            || !(1..=6).contains(&heading.level)
        {
            return Err(errors::syntax::facts_structure_invalid().error());
        }
        let depth = match heading.parent {
            None => 1,
            Some(parent) => {
                if parent >= position || headings[parent].level >= heading.level {
                    return Err(errors::syntax::facts_reference_invalid().error());
                }
                depths[parent]
                    .checked_add(1)
                    .ok_or_else(|| errors::syntax::facts_depth_exceeded().error())?
            }
        };
        if depth > limits.syntax_depth_max() {
            return Err(errors::syntax::facts_depth_exceeded().error());
        }
        depths[position] = depth;
    }
    Ok(())
}

fn validate_blocks(
    source: &str,
    blocks: &[MarkdownBlockFact],
    headings: &[MarkdownHeadingFact],
    lines: &[usize],
    limits: SyntaxLimits,
) -> Result<(), RiftError> {
    for block in blocks {
        if let Some(language) = &block.code_language {
            text(language, limits)?;
        }
        range(source, block.range)?;
        if block.line != rift_core::line::line_of(lines, block.range.start) {
            return Err(errors::syntax::facts_structure_invalid().error());
        }
        if let Some(index) = block.heading {
            let heading = headings
                .get(index)
                .ok_or_else(|| errors::syntax::facts_reference_invalid().error())?;
            if heading.range.start > block.range.start {
                return Err(errors::syntax::facts_reference_invalid().error());
            }
        }
    }
    Ok(())
}

fn validate_links(
    source: &str,
    links: &[MarkdownLinkFact],
    blocks: &std::collections::BTreeSet<ByteRange>,
) -> Result<(), RiftError> {
    for link in links {
        range(source, link.block_range)?;
        range(source, link.range)?;
        if !blocks.contains(&link.block_range) || !contains(link.block_range, link.range) {
            return Err(errors::syntax::facts_reference_invalid().error());
        }
        for inner in link
            .destination_range
            .into_iter()
            .chain(link.fragment_range)
            .chain(link.label_range)
        {
            range(source, inner)?;
            if !contains(link.range, inner) {
                return Err(errors::syntax::facts_range_invalid().error());
            }
        }
    }
    Ok(())
}

impl SyntaxFacts {
    /// Validates and restores normalized facts for exact source bytes and syntax bounds.
    ///
    /// Each collection is bounded by `limits.syntax_nodes_max()`. Validation checks UTF-8
    /// ranges, source order, provider names, unique declaration names, and parent references.
    /// Decoded text fits the source byte bound. Signatures retain the source header and
    /// language emitted by the syntax walk; semantic fields must remain empty.
    /// Resolved parents must precede their children, which rejects cycles in one bounded pass.
    /// Authored containers absent after duplicate suffixing remain accepted.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for incompatible source, invalid facts, or exceeded bounds.
    pub fn from_parts(
        source: &str,
        limits: SyntaxLimits,
        parts: SyntaxFactsParts,
    ) -> Result<Self, RiftError> {
        validate_symbols(source, limits, &parts)?;
        if let Some(markdown) = &parts.markdown_facts {
            if parts.language != ShippedLanguage::Markdown.language() {
                return Err(errors::syntax::facts_structure_invalid().error());
            }
            validate_markdown(source, &parts.symbols, limits, markdown.view())?;
        } else if parts.language == ShippedLanguage::Markdown.language() {
            return Err(errors::syntax::facts_structure_invalid().error());
        }
        Ok(Self::restored(parts, limits))
    }
}

fn embedded_symbol_kind(name: &str) -> Option<&'static str> {
    crate::ecmascript::restored_symbol_kind(crate::javascript::javascript_kinds(), name)
        .or_else(|| {
            crate::ecmascript::restored_symbol_kind(
                crate::TypeScriptDialect::TypeScript.kinds(),
                name,
            )
        })
        .or_else(|| crate::css::restored_symbol_kind(name))
}

fn grammar_node_kind(grammar: &'static tree_sitter::Language, name: &str) -> Option<&'static str> {
    let id = grammar.id_for_node_kind(name, true);
    grammar.node_kind_for_id(id).filter(|kind| *kind == name)
}
