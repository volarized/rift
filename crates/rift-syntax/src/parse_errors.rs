use std::collections::BTreeSet;
use std::num::NonZeroU16;

use rift_error::{RiftError, errors};
use rift_protocol::read::Language;
use tree_sitter::{Node, Parser};

use crate::{ByteRange, ShippedLanguage, SyntaxLimits, SyntaxNames, SyntaxSource, extract};

pub(crate) fn invalid_declarations(
    language: &Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    declarations: &[ByteRange],
) -> Result<BTreeSet<ByteRange>, RiftError> {
    limits.admit_source(source)?;
    if declarations.len() > limits.syntax_elements_max() {
        return errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
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
    let header_terminator = [ShippedLanguage::Python, ShippedLanguage::Cython]
        .iter()
        .any(|shipped| shipped.language() == *language)
        .then(|| {
            let id = grammar.id_for_node_kind(":", false);
            assert_ne!(
                id, 0,
                "shipped grammar must define header terminator: kind=:"
            );
            id
        });
    invalid_ranges(
        tree.root_node(),
        source,
        limits,
        &declarations,
        header_terminator,
    )
}

fn invalid_ranges(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    declarations: &BTreeSet<ByteRange>,
    header_terminator: Option<u16>,
) -> Result<BTreeSet<ByteRange>, RiftError> {
    let mut invalid = BTreeSet::new();
    let body_field = root.language().field_id_for_name("body");
    let mut pending = vec![(root, 0_usize, false)];
    let mut visited = 0_usize;
    let mut cursor = root.walk();
    while let Some((syntax_element, depth, in_error)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        if visited + pending.len() + syntax_element.named_child_count()
            > limits.syntax_elements_max()
        {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        let range = extract::byte_range(syntax_element)?;
        let in_error = in_error || syntax_element.is_error() || syntax_element.is_missing();
        if declarations.contains(&range)
            && (in_error
                || (syntax_element != root
                    && header_has_error(syntax_element, body_field, header_terminator)))
        {
            invalid.insert(range);
        }
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| {
            (child, depth + 1, in_error)
        });
    }
    Ok(invalid)
}

/// A declaration's body does not decide whether its header can name a symbol.
fn header_has_error(
    syntax_element: Node<'_>,
    body_field: Option<NonZeroU16>,
    header_terminator: Option<u16>,
) -> bool {
    let Some(body) = body_field.and_then(|field| syntax_element.child_by_field_id(field.get()))
    else {
        return syntax_element.has_error();
    };
    let mut cursor = syntax_element.walk();
    syntax_element
        .children(&mut cursor)
        .take_while(|child| {
            // Python can place a body error before an empty block. A missing
            // colon still belongs to the header and must be validated.
            let header_ended = header_terminator == Some(child.kind_id()) && !child.is_missing();
            *child != body && !header_ended
        })
        .any(|child| child.has_error())
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
            (
                "py",
                "class Container:\n    def ok(self):\n        pass\n    def broken(self,: pass\n    def fine(self):\n        pass\n",
                &["broken"][..],
            ),
            (
                "py",
                "else:\n    def broken():\n        pass\n",
                &["broken"][..],
            ),
            ("py", "class Broken[???]:\n    pass\n", &["Broken"][..]),
            ("py", "def container():\n    return ???\n", &[][..]),
            ("py", "class Container:\n    ???\n", &[][..]),
            ("pyx", "def container():\n    return ???\n", &[][..]),
            ("pyx", "class Broken[???]:\n    pass\n", &["Broken"][..]),
            (
                "rs",
                "mod container { pub fn ok() {} pub fn broken(x: ) {} }",
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
            assert_eq!(names, expected, "{extension}: {text:?}");
        }
    }

    #[test]
    fn declaration_validation_preserves_syntax_bounds() {
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
                errors::syntax::too_many_syntax_elements::SLUG,
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
        assert_eq!(error.slug(), errors::syntax::too_many_syntax_elements::SLUG);
    }
}
