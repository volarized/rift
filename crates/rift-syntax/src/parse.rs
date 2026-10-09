//! Shared bounded parsing for shipped source grammars.

use crate::extract::{self, GrammarRules};
use crate::{SyntaxDocument, SyntaxLimits, SyntaxSource};
use rift_error::{RiftError, errors};
use rift_protocol::read::Language;
use tree_sitter::{Language as Grammar, Parser, Range};

pub(crate) fn document(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
    grammar: &Grammar,
    rules: &dyn GrammarRules,
    included: &[Range],
) -> Result<SyntaxDocument, RiftError> {
    limits.admit_source(source)?;
    let mut parser = Parser::new();
    parser.set_language(grammar).map_err(|_| {
        errors::syntax::incompatible_grammar()
            .grammar_abi_version(grammar.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error()
    })?;
    parser
        .set_included_ranges(included)
        .map_err(|_| errors::syntax::facts_range_invalid().error())?;
    let tree = parser
        .parse(source.text, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let (nodes, symbols) = extract::extract(tree.root_node(), source, limits, language, rules)?;
    Ok(SyntaxDocument::new(
        language.clone(),
        source.path.clone(),
        nodes,
        symbols,
        tree.root_node().has_error(),
    )
    .with_source_witness(source.text)
    .with_syntax_limits(limits))
}

pub(crate) fn kind(grammar: &Grammar, name: &str) -> u16 {
    let id = grammar.id_for_node_kind(name, true);
    assert!(
        id != 0,
        "shipped grammar must define extraction kind: name={name}"
    );
    id
}
