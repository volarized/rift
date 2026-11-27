//! Exact conditional type positions for retained property ranges.

use std::collections::BTreeSet;

use rift_error::errors;
use tree_sitter::Node;

use crate::{ByteRange, StaticBodyMember, SyntaxLimits, SyntaxSource, failure::RiftError};

use super::{complete_region, extract::byte_range, parser_for_ranges};

pub(crate) fn check(
    grammar: &tree_sitter::Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    candidates: &[(ByteRange, StaticBodyMember)],
) -> Result<BTreeSet<ByteRange>, RiftError> {
    limits.admit_source(source)?;
    if candidates.len() > limits.syntax_elements_max() {
        return errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
            .fail();
    }
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let tree = parser_for_ranges(grammar, &[])?
        .parse(source.text, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let root = tree.root_node();
    if root.kind() != "program" || root.has_error() {
        return Ok(BTreeSet::new());
    }
    Ok(candidates
        .iter()
        .filter_map(|&(alias, candidate)| {
            checked_member(root, alias, candidate, limits.syntax_depth_max())
        })
        .collect())
}

fn checked_member(
    root: Node<'_>,
    alias: ByteRange,
    candidate: StaticBodyMember,
    depth_max: usize,
) -> Option<ByteRange> {
    let member = complete_region(root, candidate.member)?;
    let mut current = member.parent()?;
    if member.kind() != "property_signature"
        || current.kind() != "object_type"
        || byte_range(current).ok()? != candidate.body
    {
        return None;
    }
    let mut first_conditional = true;
    for _ in 0..depth_max {
        let parent = current.parent()?;
        match parent.kind() {
            "parenthesized_type" => (),
            "conditional_type" => {
                let result = ["consequence", "alternative"]
                    .iter()
                    .any(|field| parent.child_by_field_name(field) == Some(current));
                let constraint = parent.child_by_field_name("right") == Some(current);
                if first_conditional {
                    if !(result || constraint) || byte_range(parent).ok()? != candidate.holder {
                        return None;
                    }
                    first_conditional = false;
                } else if !result {
                    return None;
                }
            }
            "type_alias_declaration" => {
                return (!first_conditional
                    && parent.child_by_field_name("value") == Some(current)
                    && byte_range(parent).ok()? == alias)
                    .then_some(candidate.member);
            }
            _ => return None,
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_members_check_source_ranges_and_bounds() {
        const TEXT: &str = "type Choice<T> = T extends { item: string } ? number : never;";
        let path = rift_core::ProjectPath::new("index.d.ts".to_owned()).expect("source path");
        let source = SyntaxSource {
            path: &path,
            text: TEXT,
        };
        let range = |written: &str| {
            let start = TEXT.find(written).expect("written source");
            ByteRange {
                start: u64::try_from(start).expect("start"),
                end: u64::try_from(start + written.len()).expect("end"),
            }
        };
        let member = StaticBodyMember {
            member: range("item: string"),
            body: range("{ item: string }"),
            holder: range("T extends { item: string } ? number : never"),
        };
        let alias = range(TEXT);
        let provider = crate::TypeScriptDialect::TypeScript.provider();
        let checked = |limits, candidates: &[(ByteRange, StaticBodyMember)]| {
            provider.checked_conditional_type_members(source, limits, candidates)
        };
        assert_eq!(
            checked(SyntaxLimits::default(), &[(alias, member)]).expect("complete parse"),
            BTreeSet::from([member.member])
        );
        let different = StaticBodyMember {
            holder: member.body,
            ..member
        };
        assert!(
            checked(SyntaxLimits::default(), &[(alias, different)])
                .expect("complete parse")
                .is_empty()
        );
        let bound = |bytes, candidates, depth| {
            SyntaxLimits::new(bytes, candidates, depth).expect("positive bounds")
        };
        assert!(
            checked(bound(TEXT.len(), 1, 1), &[(alias, member)])
                .expect("bounded ancestry")
                .is_empty()
        );
        assert!(
            checked(
                bound(TEXT.len(), 1, 16),
                &[(alias, member), (alias, member)]
            )
            .is_err()
        );
        assert!(checked(bound(TEXT.len() - 1, 1, 16), &[(alias, member)]).is_err());
    }
}
