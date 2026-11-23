//! Proves when the TypeScript parser's errors are solely authored variance modifiers.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::{Node, Parser};

use crate::SyntaxLimits;

type Span = (usize, usize);

/// Adjacent modifier tokens without the parameter name they precede.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Modifiers(Span, Option<Span>);

impl Modifiers {
    fn blank(self, source: &mut [u8]) {
        let Self(first, second) = self;
        for (start, end) in std::iter::once(first).chain(second) {
            source[start..end].fill(b' ');
        }
    }
}

/// A single incomplete parse cannot claim file identity from a partial recovery proof.
const VARIANCE_ERROR_SPANS_MAX: usize = 1024;

/// Checks the error spine of an already parsed file and reparses only when every
/// error can be matched to one exact parameter prefix. The comparison parse
/// blanks the authored modifiers, keeps all other bytes, and contributes no facts.
pub(super) fn accounts_for_parse_errors(
    parser: &mut Parser,
    root: Node<'_>,
    source: &str,
    limits: SyntaxLimits,
) -> bool {
    let mut candidates = BTreeMap::new();
    let mut matched = BTreeSet::new();
    let mut pending = vec![(root, 0)];
    let mut visited = 0;
    let mut cursor = root.walk();
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        let Ok(children) = usize::try_from(node.child_count()) else {
            return false;
        };
        if node.is_missing()
            || depth > limits.syntax_depth_max()
            || visited + pending.len() + children > limits.syntax_elements_max()
        {
            return false;
        }
        if node.kind() == "type_parameters"
            && is_declaration_parameter_list(node)
            && !parameter_errors(node, source, &mut candidates)
        {
            return false;
        }
        if node.is_error() {
            let Some(modifiers) = candidates
                .get(&coordinates(node))
                .filter(|_| is_parameter_prefix_error(node))
            else {
                return false;
            };
            matched.insert(*modifiers);
        }
        pending.extend(
            node.children(&mut cursor)
                .filter(|child| child.has_error() || child.is_missing())
                .map(|child| (child, depth + 1)),
        );
    }
    if matched.is_empty() {
        return false;
    }
    let mut checked = source.as_bytes().to_vec();
    for modifiers in matched {
        modifiers.blank(&mut checked);
    }
    parser
        .parse(&checked, None)
        .is_some_and(|parsed| !parsed.root_node().has_error())
}

fn coordinates(node: Node<'_>) -> Span {
    (node.start_byte(), node.end_byte())
}

fn is_declaration_parameter_list(list: Node<'_>) -> bool {
    list.parent().is_some_and(|declaration| {
        matches!(
            declaration.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
        ) && declaration.child_by_field_name("type_parameters") == Some(list)
    })
}

fn is_parameter_prefix_error(error: Node<'_>) -> bool {
    let Some(parent) = error.parent() else {
        return false;
    };
    let list = if parent.kind() == "type_parameter" {
        parent.parent()
    } else {
        Some(parent)
    };
    list.is_some_and(|list| list.kind() == "type_parameters" && is_declaration_parameter_list(list))
}

fn parameter_errors(
    list: Node<'_>,
    source: &str,
    candidates: &mut BTreeMap<Span, Modifiers>,
) -> bool {
    let mut cursor = list.walk();
    for boundary in list
        .children(&mut cursor)
        .filter(|child| matches!(child.kind(), "<" | ",") && !child.is_missing())
    {
        let Some((modifiers, name)) = prefix(source, boundary.end_byte(), list.end_byte()) else {
            continue;
        };
        let Modifiers(first, second) = modifiers;
        let mut ranges = [Some(name), Some(first), second, None];
        if let Some((start, _)) = second {
            ranges[3] = Some((start, name.1));
        }
        if ranges
            .into_iter()
            .flatten()
            .any(|range| candidates.insert(range, modifiers).is_some())
            || candidates.len() > VARIANCE_ERROR_SPANS_MAX
        {
            return false;
        }
    }
    true
}

fn prefix(source: &str, start: usize, end: usize) -> Option<(Modifiers, Span)> {
    let bytes = source.as_bytes().get(..end)?;
    let first = identifier(bytes, start)?;
    let keyword = source.get(first.0..first.1)?;
    if !matches!(keyword, "in" | "out") {
        return None;
    }
    let next = identifier(bytes, first.1)?;
    let second = (keyword == "in" && source.get(next.0..next.1) == Some("out")).then_some(next);
    let name = if second.is_some() {
        identifier(bytes, next.1)?
    } else {
        next
    };
    (!matches!(source.get(name.0..name.1)?, "in" | "out" | "extends"))
        .then_some((Modifiers(first, second), name))
}

fn identifier(bytes: &[u8], start: usize) -> Option<Span> {
    let mut rest = bytes.get(start..)?.iter();
    let spaces = rest
        .by_ref()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    let start = start + spaces;
    let first = *bytes.get(start)?;
    if !(first.is_ascii_alphabetic() || matches!(first, b'_' | b'$')) {
        return None;
    }
    let width = bytes[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
        .count();
    Some((start, start + width))
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use crate::{
        SyntaxLimits, SyntaxProvider, SyntaxSource, TypeScriptDialect, TypeScriptSyntaxProvider,
    };

    #[test]
    fn comparison_parse_blanks_only_the_matched_words() {
        let original = "interface Hold<in out T extends Box<U> = Base> { item: T }";
        let boundary = original.find('<').expect("parameter opening") + 1;
        let (modifiers, (start, end)) =
            super::prefix(original, boundary, original.len()).expect("exact variance prefix");
        assert_eq!(&original[start..end], "T");
        let mut checked = original.as_bytes().to_vec();
        modifiers.blank(&mut checked);
        assert_eq!(
            &checked,
            b"interface Hold<       T extends Box<U> = Base> { item: T }"
        );
        for parameter in [
            "<in in T>",
            "<out in T>",
            "<out>",
            "<out/**/T>",
            "<outer T>",
        ] {
            assert!(super::prefix(parameter, 1, parameter.len()).is_none());
        }
    }

    #[test]
    fn exact_variance_prefixes_explain_only_matched_syntax_errors() {
        let provider = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript);
        let path = ProjectPath::new("probe.ts").expect("fixture path");
        for (text, raw_errors, complete) in [
            (
                "interface Mid<out T = unknown, in U = string> { value: T; run(input: U): T }\nclass Box<in out T> { item!: T }\ninterface Held<out T = unknown> { held: T }",
                true,
                true,
            ),
            ("interface Bad<out T extends > { value: T }", true, false),
            ("interface 1Bad<out T> { value: T }", true, false),
            ("interface Bad<out T> extends { value: T }", true, false),
            ("type Bad<out T> = ;", true, false),
            ("interface Bad<out T> { value: }", true, false),
            ("interface Kept<out> { value: out }", false, true),
        ] {
            let document = provider
                .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
                .expect("fixture capture");
            assert_eq!(document.has_errors(), raw_errors, "{text}");
            assert_eq!(
                document.facts().declaration_syntax_complete(),
                complete,
                "{text}"
            );
        }
    }
}
