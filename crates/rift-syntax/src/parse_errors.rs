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
    // Validation reads the tree extraction read: a recovered ECMAScript document.
    let ecmascript = [
        ShippedLanguage::JavaScript,
        ShippedLanguage::TypeScript,
        ShippedLanguage::TypeScriptTsx,
    ]
    .iter()
    .any(|shipped| shipped.language() == *language);
    let tree = if ecmascript {
        crate::ecmascript::recovery::parse(&mut parser, source.text).map(|(tree, _)| tree)
    } else {
        parser.parse(source.text, None)
    }
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
    // TypeScript variance annotations (`in`, `out`) are not in the grammar. The parser
    // confines that error to the type parameter list, which names no declaration.
    let recovered_header = [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx]
        .iter()
        .any(|shipped| shipped.language() == *language)
        .then(|| {
            let id = grammar.field_id_for_name("type_parameters");
            assert!(
                id.is_some(),
                "shipped grammar must define header field: field=type_parameters"
            );
            id
        })
        .flatten();
    invalid_ranges(
        tree.root_node(),
        source,
        limits,
        &declarations,
        HeaderRules {
            terminator: header_terminator,
            recovered: recovered_header,
            value: ecmascript
                .then(|| grammar.field_id_for_name("value"))
                .flatten(),
            // Zero in JavaScript, which has no object type.
            members: grammar.id_for_node_kind("object_type", true),
            // A Rust declaration is named apart from the generic arguments of the types it
            // writes and from its where clause. Both kinds are zero in other grammars.
            unnamed: if *language == ShippedLanguage::Rust.language() {
                ["type_arguments", "where_clause"].map(|kind| {
                    let id = grammar.id_for_node_kind(kind, true);
                    assert_ne!(
                        id, 0,
                        "shipped grammar must define header element: kind={kind}"
                    );
                    id
                })
            } else {
                [0; 2]
            },
        },
    )
}

/// Grammar facts that decide which header errors keep a declaration's name.
#[derive(Clone, Copy)]
struct HeaderRules {
    /// Token that ends a header before an optional body.
    terminator: Option<u16>,
    /// Header field whose own errors leave the name and the other header parts intact.
    recovered: Option<NonZeroU16>,
    /// Field holding the value a declaration is initialized with or aliases.
    value: Option<NonZeroU16>,
    /// Kind of a braced member list that can be a declaration's whole value.
    members: u16,
    /// Kinds of elements that name no part of the declaration whose header holds them.
    unnamed: [u16; 2],
}

fn invalid_ranges(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    declarations: &BTreeSet<ByteRange>,
    rules: HeaderRules,
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
                    && header_has_error(syntax_element, body_field, rules)
                    && !opaque_macro_body(syntax_element)))
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
///
/// Neither does an error confined to the recovered header field: the name, the
/// heritage clause and every other header part are then outside the error.
///
/// A declaration without a body of its own can take one from its value: the member
/// list a type alias names, or the body of the function a variable or field is
/// initialized with. That function's parameters stay part of the header.
fn header_has_error(
    syntax_element: Node<'_>,
    body_field: Option<NonZeroU16>,
    rules: HeaderRules,
) -> bool {
    let own_body = body_field.and_then(|field| syntax_element.child_by_field_id(field.get()));
    let value = rules
        .value
        .filter(|_| own_body.is_none())
        .and_then(|field| syntax_element.child_by_field_id(field.get()));
    let value_body = value.and_then(|value| {
        if value.kind_id() == rules.members && rules.members != 0 {
            Some(value)
        } else {
            body_field.and_then(|field| value.child_by_field_id(field.get()))
        }
    });
    // The function between the declaration and its body, when the value is one.
    let function = value.filter(|value| value_body.is_some_and(|body| body != *value));
    let body = own_body.or(value_body);
    let recovered = rules
        .recovered
        .and_then(|field| syntax_element.child_by_field_id(field.get()));
    if body.is_none() && recovered.is_none() {
        return has_named_error(syntax_element, rules);
    }
    if let Some(function) = function
        && header_children_have_error(function, body, None, rules)
    {
        return true;
    }
    // The declaration's own header ends where its value function or its body starts.
    header_children_have_error(syntax_element, function.or(body), recovered, rules)
}

/// Whether a child before `end` holds an error, other than the `recovered` child.
fn header_children_have_error(
    syntax_element: Node<'_>,
    end: Option<Node<'_>>,
    recovered: Option<Node<'_>>,
    rules: HeaderRules,
) -> bool {
    let mut cursor = syntax_element.walk();
    syntax_element
        .children(&mut cursor)
        .take_while(|child| {
            // Python can place a body error before an empty block. A missing
            // colon still belongs to the header and must be validated.
            let header_ended = rules.terminator == Some(child.kind_id()) && !child.is_missing();
            Some(*child) != end && !header_ended
        })
        .any(|child| Some(child) != recovered && has_named_error(child, rules))
}

/// Whether `element` holds an error outside the elements that name no declaration.
///
/// Reads each element below `element` at most once, and only those that hold an error.
fn has_named_error(element: Node<'_>, rules: HeaderRules) -> bool {
    if !element.has_error() || rules.unnamed == [0; 2] {
        return element.has_error();
    }
    let mut pending = vec![element];
    let mut cursor = element.walk();
    while let Some(element) = pending.pop() {
        if element.is_error() || element.is_missing() {
            return true;
        }
        if !rules.unnamed.contains(&element.kind_id()) {
            pending.extend(element.children(&mut cursor).filter(Node::has_error));
        }
    }
    false
}

// The Rust grammar parses macro matchers as token trees. Their contents do not
// determine whether the macro's name and delimiters form a valid declaration.
fn opaque_macro_body(element: Node<'_>) -> bool {
    if element.kind() != "macro_definition" {
        return false;
    }
    let Some(name) = element
        .child_by_field_name("name")
        .filter(|name| !name.has_error())
    else {
        return false;
    };
    let mut cursor = element.walk();
    let mut children = element
        .children(&mut cursor)
        .filter(|child| !child.is_extra());
    let Some(keyword) = children.next() else {
        return false;
    };
    if keyword.kind() != "macro_rules!" || children.next() != Some(name) {
        return false;
    }
    let Some(open) = children.next() else {
        return false;
    };
    let close = match open.kind() {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => return false,
    };
    children
        .filter(|child| child.kind() != ";")
        .last()
        .is_some_and(|last| last.kind() == close && !last.is_missing())
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
    fn type_parameter_errors_keep_declarations_named_outside_them() {
        for (extension, text, expected) in [
            (
                "ts",
                "interface Base<T> { base: T }\ninterface Task<out T, in U = string> extends Base<T> { value: T }\n",
                &[][..],
            ),
            ("ts", "type Alias<out T> = Array<T>;\n", &[][..]),
            (
                "ts",
                "class Other<T> {}\nclass Box<in out T> extends Other<T> { item!: T }\n",
                &[][..],
            ),
            (
                "tsx",
                "interface Task<out T> { value: T }\nconst view = <div />;\n",
                &[][..],
            ),
            // An error on the heritage clause still refuses the declaration.
            (
                "ts",
                "interface Broken<out T> extends { value: T }\n",
                &["Broken"][..],
            ),
            // So does an error in a header part of a declaration without a body.
            ("ts", "type Broken<out T> = ;\n", &["Broken"][..]),
            // Other grammars keep refusing a broken type parameter list.
            ("py", "class Broken[???]:\n    pass\n", &["Broken"][..]),
        ] {
            let path = ProjectPath::new(format!("main.{extension}")).expect("source path");
            let source = SyntaxSource { path: &path, text };
            let provider = crate::registry::provider_for_extension(extension).expect("provider");
            let document = provider
                .analyze(source, SyntaxLimits::DEFAULT)
                .expect("source facts");
            assert!(document.has_errors(), "{extension}: {text:?}");
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
    fn rust_generic_argument_and_where_clause_errors_keep_the_declaration() {
        // #1099: neither part names the declaration whose header holds it.
        for (text, expected) in [
            ("pub type Hours = Ranged<-25, 25>;", &[][..]),
            ("pub struct Item { hours: Ranged<-25, 25> }", &[][..]),
            (
                "fn hours(value: Ranged<-25, 25>) -> Ranged<-1, 1> {}",
                &[][..],
            ),
            ("impl Read for Ranged<-25, 25> {}", &[][..]),
            ("fn empty() where (): Copy {}", &[][..]),
            // An error in another part of the header still refuses the declaration.
            ("fn broken(x: ) {}", &["broken"][..]),
            ("struct Broken where Item: Sized;", &["Broken"][..]),
            ("pub type Broken<-1> = Item;", &["Broken"][..]),
        ] {
            let path = ProjectPath::new("main.rs").expect("source path");
            let source = SyntaxSource { path: &path, text };
            let provider = crate::registry::provider_for_extension("rs").expect("provider");
            let document = provider
                .analyze(source, SyntaxLimits::DEFAULT)
                .expect("source facts");
            assert!(document.has_errors(), "{text:?}");
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
            assert_eq!(names, expected, "{text:?}");
        }
    }

    #[test]
    fn value_body_errors_keep_the_declaration_they_initialize() {
        for (extension, text, expected) in [
            // The missing member of the optional chain is inside the function body.
            (
                "ts",
                "export const inspect = () => {\n  const global = globalThis as any\n  return global?.\n}\nexport function run() { return inspect() }\n",
                &[][..],
            ),
            (
                "mjs",
                "export const inspect = () => { return ) }\n",
                &[][..],
            ),
            ("ts", "class Item { run = () => { return ) } }\n", &[][..]),
            // A type alias takes its member list as its body, even if a return tuple is unclosed.
            (
                "ts",
                "type Shape = {\n  <T>(value: T): [T, Box<T>]\n  <T = undefined>(): [T | undefined, Box<T>\n}\nexport const after = 1\n",
                &[][..],
            ),
            // The function's parameters and a value that is no body still refuse.
            (
                "ts",
                "export const bad = (value: ) => { return 1 }\n",
                &["bad"][..],
            ),
            (
                "ts",
                "export const broken = { value: ) }\n",
                &["broken"][..],
            ),
            (
                "ts",
                "type Broken = { value: string } & ;\n",
                &["Broken"][..],
            ),
        ] {
            let path = ProjectPath::new(format!("main.{extension}")).expect("source path");
            let source = SyntaxSource { path: &path, text };
            let provider = crate::registry::provider_for_extension(extension).expect("provider");
            let document = provider
                .analyze(source, SyntaxLimits::DEFAULT)
                .expect("source facts");
            assert!(document.has_errors(), "{extension}: {text:?}");
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
                .filter(|symbol| symbol.container.is_none())
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, expected, "{extension}: {text:?}");
        }
    }

    #[test]
    fn facts_keep_the_validation_answer_and_copies_validate_again() {
        let path = ProjectPath::new("main.py").expect("source path");
        let text = "def ok():\n    pass\ndef broken(: pass\n";
        let source = SyntaxSource { path: &path, text };
        let provider = PythonSyntaxProvider::default();
        let facts = provider
            .analyze(source, SyntaxLimits::DEFAULT)
            .expect("source facts")
            .into_facts();
        let ranges = facts
            .symbols()
            .iter()
            .map(|symbol| symbol.item_range)
            .collect::<Vec<_>>();
        let expected = provider
            .invalid_declarations(source, SyntaxLimits::DEFAULT, &ranges)
            .expect("declaration validation");
        assert!(!expected.is_empty());
        assert_eq!(
            facts.invalid_declarations(source, SyntaxLimits::DEFAULT),
            Some(expected.clone())
        );
        // The kept answer serves the next call without the source it was computed from.
        let other = SyntaxSource {
            path: &path,
            text: "",
        };
        assert_eq!(
            facts.invalid_declarations(other, SyntaxLimits::DEFAULT),
            Some(expected)
        );
        // Other bounds are validated again rather than answered from the kept result.
        let limits = SyntaxLimits::new(1, 100, 100).expect("positive bounds");
        assert_eq!(facts.invalid_declarations(source, limits), None);
        let copy = facts.as_ref().clone();
        assert_eq!(
            copy.invalid_declarations(other, SyntaxLimits::DEFAULT),
            Some(std::collections::BTreeSet::new())
        );
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

    #[test]
    fn a_root_error_keeps_declarations_outside_the_body_that_holds_it() {
        // A generic return type missing its closing angle bracket makes the document
        // root an error. Leaving out the interface body's interior parses the rest whole.
        let signatures = "  <T extends Before, U extends number>(\n    data: T,\n    status?: U\n  ): Wrapped<T, U, 'body'\n  <T extends Before, U extends number>(\n    data: T,\n    init?: Init<U>\n  ): Wrapped<T, U, 'body'>\n  <T extends null, U extends number>(\n    data: T,\n    status?: U\n  ): Wrapped<null, U, 'body'>\n";
        let tail = "export class Item {\n  value = 1\n  read() { return this.value }\n}\nexport function later(item: Item) { return item.read() }\n";
        let recovered = format!(
            "export type Before = string\nexport interface Respond {{\n{signatures}}}\n{tail}"
        );
        // The body never closes, so no interior can be left out.
        let unclosed = format!("export interface Respond {{\n{signatures}{tail}");
        // An error outside every braced body gives recovery nothing to leave out.
        let outside = format!(") ) )\n{recovered}");
        // A second body with its own error is left out as well.
        let second = format!("{recovered}export function tail() {{ return ) ) ) }}\n");
        let all = &["Before", "Respond", "Item", "value", "read", "later"][..];
        for (extension, text, kept, refused) in [
            ("ts", recovered.as_str(), all, &[][..]),
            ("tsx", recovered.as_str(), all, &[][..]),
            (
                "ts",
                second.as_str(),
                &["Before", "Item", "later", "tail"][..],
                &[][..],
            ),
            ("ts", unclosed.as_str(), &[][..], &["Item", "later"][..]),
            (
                "ts",
                outside.as_str(),
                &[][..],
                &["Before", "Item", "later"][..],
            ),
        ] {
            let path = ProjectPath::new(format!("main.{extension}")).expect("source path");
            let source = SyntaxSource { path: &path, text };
            let provider = crate::registry::provider_for_extension(extension).expect("provider");
            let document = provider
                .analyze(source, SyntaxLimits::DEFAULT)
                .expect("source facts");
            assert!(document.has_errors(), "{extension}: {text:?}");
            let ranges = document
                .symbols()
                .iter()
                .map(|symbol| symbol.item_range)
                .collect::<Vec<_>>();
            let invalid = provider
                .invalid_declarations(source, SyntaxLimits::DEFAULT, &ranges)
                .expect("declaration validation");
            let valid = document
                .symbols()
                .iter()
                .filter(|symbol| !invalid.contains(&symbol.item_range))
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>();
            for name in kept {
                assert!(
                    valid.contains(name),
                    "{extension}: {name} kept in {valid:?}"
                );
            }
            for name in refused {
                assert!(
                    !valid.contains(name),
                    "{extension}: {name} refused in {valid:?}"
                );
            }
            // Offsets are the source's own: every kept name is read at its written place.
            for symbol in document.symbols() {
                let range = symbol.name_range.expect("declaration name");
                let start = usize::try_from(range.start).expect("offset");
                let end = usize::try_from(range.end).expect("offset");
                assert_eq!(&text[start..end], symbol.name, "{extension}");
            }
        }
    }
}
