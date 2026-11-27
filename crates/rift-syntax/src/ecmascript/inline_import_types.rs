//! Checks written relative import types against a byte-preserving TypeScript parse.

use rift_core::constants::PROJECT_PATH_BYTES_MAX;
use rift_error::errors;
use tree_sitter::{Node, Parser, Tree};

mod checked_type_aliases;
pub(super) use checked_type_aliases::recover as recover_type_aliases;

use crate::{ByteRange, SyntaxLimits, SyntaxSource, failure::RiftError};

use super::parser_for_ranges;

/// The written parts of a checked inline TypeScript type import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedInlineTypeImport {
    /// The relative module name inside its quotes.
    pub module: ByteRange,
    /// The written type identifier after the specifier.
    pub qualifier: ByteRange,
    /// The written name of the declaration whose type uses the import.
    pub owner_name: ByteRange,
    /// The parser kind of the owning declaration.
    pub owner_kind: &'static str,
}

#[derive(Debug)]
struct Candidate {
    prefix: std::ops::Range<usize>,
    module: std::ops::Range<usize>,
    qualifier: usize,
}

pub(crate) fn check(
    grammar: &tree_sitter::Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
) -> Result<Vec<CheckedInlineTypeImport>, RiftError> {
    limits.admit_source(source)?;
    let candidates = bounded_candidates(source, limits)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut parser = parser_for_ranges(grammar, &[])?;
    let original = parser
        .parse(source.text, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let Some((tree, candidates)) =
        comparison_tree(&mut parser, original.root_node(), source, candidates)?
    else {
        return Ok(Vec::new());
    };
    let root = tree.root_node();
    let mut checked = Vec::new();
    for candidate in &candidates {
        if let Some(import) = checked_candidate(root, candidate, limits.syntax_depth_max()) {
            checked.push(import);
        }
    }
    Ok(checked)
}

fn bounded_candidates(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
) -> Result<Vec<Candidate>, RiftError> {
    candidates(source.text, limits.syntax_elements_max()).ok_or_else(|| {
        errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
            .error()
    })
}

fn comparison_tree(
    parser: &mut Parser,
    root: Node<'_>,
    source: SyntaxSource<'_>,
    mut candidates: Vec<Candidate>,
) -> Result<Option<(Tree, Vec<Candidate>)>, RiftError> {
    if root.kind() != "program" || root.is_error() {
        return Ok(None);
    }
    candidates.retain(|candidate| original_tokens(root, candidate));
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut comparison = source.text.as_bytes().to_vec();
    for candidate in &candidates {
        // Keep newlines and byte offsets; the comparison tree contributes no other facts.
        for byte in &mut comparison[candidate.prefix.clone()] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    let tree = parser
        .parse(&comparison, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let root = tree.root_node();
    Ok((root.kind() == "program" && !root.is_error()).then_some((tree, candidates)))
}

fn original_tokens(root: Node<'_>, candidate: &Candidate) -> bool {
    let start = candidate.prefix.start;
    let dot = candidate.prefix.end - 1;
    let quoted = candidate.module.start - 1..candidate.module.end + 1;
    root.descendant_for_byte_range(start, start + "import".len())
        .is_some_and(|keyword| {
            keyword.byte_range() == (start..start + "import".len())
                && matches!(keyword.kind(), "import" | "type_identifier" | "identifier")
        })
        && root
            .named_descendant_for_byte_range(quoted.start, quoted.end)
            .is_some_and(|literal| {
                literal.kind() == "string" && literal.byte_range() == quoted && !literal.has_error()
            })
        && root
            .descendant_for_byte_range(dot, dot + 1)
            .is_some_and(|separator| {
                separator.kind() == "." && separator.byte_range() == (dot..dot + 1)
            })
}

fn candidates(text: &str, maximum: usize) -> Option<Vec<Candidate>> {
    let mut results = Vec::new();
    let mut consumed = 0;
    for (position, _) in text.match_indices("import") {
        if position < consumed
            || position
                .checked_sub(1)
                .and_then(|previous| text.as_bytes().get(previous))
                .is_some_and(|byte| {
                    byte.is_ascii_alphanumeric()
                        || *byte >= 128
                        || matches!(*byte, b'_' | b'$' | b'.')
                })
        {
            continue;
        }
        if let Some(candidate) = candidate_at(text.as_bytes(), position) {
            if results.len() == maximum {
                return None;
            }
            consumed = candidate.prefix.end;
            results.push(candidate);
        }
    }
    Some(results)
}

fn candidate_at(bytes: &[u8], start: usize) -> Option<Candidate> {
    let open = after_space(bytes, start.checked_add("import".len())?);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let quote_index = after_space(bytes, open + 1);
    let quote = *bytes.get(quote_index)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let module_start = quote_index + 1;
    let rest = bytes.get(module_start..)?;
    if !(rest.starts_with(b"./") || rest.starts_with(b"../")) {
        return None;
    }
    let module_len = rest
        .iter()
        .take(PROJECT_PATH_BYTES_MAX + 1)
        .position(|byte| *byte == quote || matches!(*byte, b'\\' | b'\r' | b'\n'))?;
    let module_end = module_start + module_len;
    if bytes.get(module_end) != Some(&quote) {
        return None;
    }
    let close = after_space(bytes, module_end + 1);
    if bytes.get(close) != Some(&b')') {
        return None;
    }
    let dot = after_space(bytes, close + 1);
    if bytes.get(dot) != Some(&b'.') {
        return None;
    }
    let qualifier = after_space(bytes, dot + 1);
    bytes
        .get(qualifier)
        .filter(|byte| !byte.is_ascii_whitespace())?;
    Some(Candidate {
        prefix: start..dot + 1,
        module: module_start..module_end,
        qualifier,
    })
}

fn after_space(bytes: &[u8], mut offset: usize) -> usize {
    while bytes.get(offset).is_some_and(u8::is_ascii_whitespace) {
        offset += 1;
    }
    offset
}

fn checked_candidate(
    root: Node<'_>,
    candidate: &Candidate,
    depth_max: usize,
) -> Option<CheckedInlineTypeImport> {
    let qualifier =
        root.named_descendant_for_byte_range(candidate.qualifier, candidate.qualifier + 1)?;
    if qualifier.start_byte() != candidate.qualifier
        || qualifier.kind() != "type_identifier"
        || qualifier.has_error()
    {
        return None;
    }
    let (owner, owner_kind) = checked_owner(qualifier, depth_max)?;
    let name = owner.child_by_field_name("name")?;
    if name.has_error() || owner.has_error() {
        return None;
    }
    Some(CheckedInlineTypeImport {
        module: ByteRange {
            start: u64::try_from(candidate.module.start).ok()?,
            end: u64::try_from(candidate.module.end).ok()?,
        },
        qualifier: crate::extract::byte_range(qualifier).ok()?,
        owner_name: crate::extract::byte_range(name).ok()?,
        owner_kind,
    })
}

fn checked_owner(qualifier: Node<'_>, depth_max: usize) -> Option<(Node<'_>, &'static str)> {
    let mut inner = qualifier;
    for _ in 0..depth_max {
        let parent = inner.parent()?;
        match parent.kind() {
            "union_type" | "intersection_type" | "parenthesized_type" | "array_type"
            | "generic_type" | "type_arguments" => inner = parent,
            "type_alias_declaration" if parent.child_by_field_name("value") == Some(inner) => {
                return Some((parent, "type_alias_declaration"));
            }
            "type_annotation" => {
                let parameter = parent.parent()?;
                if !matches!(
                    parameter.kind(),
                    "required_parameter" | "optional_parameter"
                ) || parameter.child_by_field_name("type") != Some(parent)
                {
                    return None;
                }
                let parameters = parameter.parent()?;
                let owner = parameters.parent()?;
                return (parameters.kind() == "formal_parameters"
                    && owner.kind() == "function_signature"
                    && owner.child_by_field_name("parameters") == Some(parameters))
                .then_some((owner, "function_signature"));
            }
            _ => return None,
        }
    }
    None
}
