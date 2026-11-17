use std::collections::BTreeSet;

use rift_error::{RiftError, errors};
use rift_protocol::read::Language;
use tree_sitter::{Node, Parser};

use crate::{ByteRange, SyntaxLimits, SyntaxNames, SyntaxSource, extract};

pub(crate) fn invalid_declarations(
    language: &Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    declarations: &[ByteRange],
) -> Result<BTreeSet<ByteRange>, RiftError> {
    limits.admit_source(source)?;
    if declarations.len() > limits.syntax_nodes_max() {
        return errors::syntax::too_many_nodes()
            .path(source.path)
            .syntax_nodes_max(limits.syntax_nodes_max())
            .fail();
    }
    let names =
        SyntaxNames::new(language).ok_or_else(|| errors::syntax::facts_kind_invalid().error())?;
    let grammar = names.grammar();
    let mut parser = Parser::new();
    parser.set_language(grammar).map_err(|_| {
        errors::syntax::incompatible_grammar()
            .grammar_abi_version(grammar.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error()
    })?;
    let tree = parser
        .parse(source.text, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let declarations = declarations.iter().copied().collect::<BTreeSet<_>>();
    invalid_ranges(tree.root_node(), source, limits, &declarations)
}

fn invalid_ranges(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    declarations: &BTreeSet<ByteRange>,
) -> Result<BTreeSet<ByteRange>, RiftError> {
    let mut invalid = BTreeSet::new();
    let mut pending = vec![(root, 0_usize, false)];
    let mut visited = 0_usize;
    let mut cursor = root.walk();
    while let Some((node, depth, in_error)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        if visited + pending.len() + node.named_child_count() > limits.syntax_nodes_max() {
            return errors::syntax::too_many_nodes()
                .path(source.path)
                .syntax_nodes_max(limits.syntax_nodes_max())
                .fail();
        }
        let range = extract::byte_range(node)?;
        if declarations.contains(&range) && (in_error || node.has_error()) {
            invalid.insert(range);
        }
        let in_error = in_error || node.is_error() || node.is_missing();
        extract::push_named_children(&mut pending, &mut cursor, node, |child| {
            (child, depth + 1, in_error)
        });
    }
    Ok(invalid)
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_error::errors;

    use crate::{PythonSyntaxProvider, SyntaxLimits, SyntaxProvider, SyntaxSource};

    #[test]
    fn missing_tokens_refuse_only_declarations_with_errors() {
        for (extension, text, expected) in [
            (
                "py",
                "def ok():\n    pass\ndef broken(: pass\nclass Fine:\n    pass\n",
                &["broken"][..],
            ),
            ("ts", "class Container { method() {}", &[][..]),
        ] {
            let path = ProjectPath::new(format!("main.{extension}")).expect("source path");
            let source = SyntaxSource { path: &path, text };
            let provider = crate::registry::provider_for_extension(extension).expect("provider");
            let document = provider
                .analyze(source, SyntaxLimits::DEFAULT)
                .expect("source facts");
            let ranges = document
                .symbols()
                .iter()
                .map(|symbol| symbol.item_range)
                .collect::<Vec<_>>();
            let invalid = provider
                .invalid_declarations(source, SyntaxLimits::DEFAULT, &ranges)
                .expect("declaration validation");
            let names = document
                .symbols()
                .iter()
                .filter(|symbol| invalid.contains(&symbol.item_range))
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, expected);
        }
    }

    #[test]
    fn declaration_validation_preserves_source_node_and_depth_bounds() {
        let path = ProjectPath::new("main.py").expect("source path");
        let source = SyntaxSource {
            path: &path,
            text: "def beacon():\n    pass\n",
        };
        let provider = PythonSyntaxProvider::default();
        for (limits, slug) in [
            (
                SyntaxLimits::new(1, 100, 100),
                errors::syntax::source_too_large::SLUG,
            ),
            (
                SyntaxLimits::new(100, 1, 100),
                errors::syntax::too_many_nodes::SLUG,
            ),
            (
                SyntaxLimits::new(100, 100, 1),
                errors::syntax::too_deep::SLUG,
            ),
        ] {
            let error = provider
                .invalid_declarations(source, limits.expect("positive bounds"), &[])
                .expect_err("source refused");
            assert_eq!(error.slug(), slug);
        }
        assert!(
            provider
                .invalid_declarations(source, SyntaxLimits::DEFAULT, &[])
                .expect("valid source")
                .is_empty()
        );
    }

    #[test]
    fn declaration_validation_refuses_unknown_grammars_and_excess_declarations() {
        let path = ProjectPath::new("main.py").expect("source path");
        let source = SyntaxSource {
            path: &path,
            text: "",
        };
        let language =
            rift_protocol::read::Language::from_identity_segment("unknown").expect("language");
        let error = super::invalid_declarations(&language, source, SyntaxLimits::DEFAULT, &[])
            .expect_err("unknown grammar");
        assert_eq!(error.slug(), errors::syntax::facts_kind_invalid::SLUG);
        let limits = SyntaxLimits::new(1, 1, 1).expect("positive bounds");
        let declarations = [crate::ByteRange { start: 0, end: 0 }; 2];
        let error = PythonSyntaxProvider::default()
            .invalid_declarations(source, limits, &declarations)
            .expect_err("declaration bound");
        assert_eq!(error.slug(), errors::syntax::too_many_nodes::SLUG);
    }
}
