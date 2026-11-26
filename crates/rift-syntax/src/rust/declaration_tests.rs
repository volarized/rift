//! Declarations the Rust rules emit beyond items: struct, union, and
//! variant fields, unions, trait associated types, nested functions, and
//! the visibility trait items take from their trait.

use rift_core::ProjectPath;
use rift_protocol::read::SymbolKind;

use super::*;

fn analyze(text: &str) -> SyntaxDocument {
    let path = ProjectPath::new("src/lib.rs").expect("valid fixture path");
    let document = RustSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("Rust fixture must parse");
    assert!(!document.has_errors());
    document
}

/// Each declaration's qualified name, container, and kind word.
fn declared(document: &SyntaxDocument) -> Vec<(&str, Option<&str>, &str)> {
    document
        .symbols()
        .iter()
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.container.as_deref(),
                symbol.kind,
            )
        })
        .collect()
}

fn symbol<'document>(
    document: &'document SyntaxDocument,
    qualified: &str,
) -> &'document crate::SyntaxSymbol {
    document
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == qualified)
        .unwrap_or_else(|| panic!("fixture declares {qualified}: {:?}", declared(document)))
}

fn spelled(text: &str, range: ByteRange) -> &str {
    let start = usize::try_from(range.start).expect("fixture start");
    let end = usize::try_from(range.end).expect("fixture end");
    &text[start..end]
}

/// Named fields of a struct, a union, and a struct-like variant declare
/// under their owner, with their own visibility and doc comments; a tuple
/// struct's positional fields declare nothing.
#[test]
fn test_named_fields_declare_under_struct_union_and_variant() {
    let text = "/// A point.\npub struct Point {\n    /// Horizontal.\n    pub x: i32,\n    y: i32,\n}\n\
                pub union Bits { pub i: u32, f: f32 }\n\
                pub enum Color { Blue { b: u8 } }\n\
                pub struct Tagged(pub u8);\n";
    let document = analyze(text);
    assert_eq!(
        declared(&document),
        [
            ("Point", None, "struct"),
            ("Point::x", Some("Point"), "field_declaration"),
            ("Point::y", Some("Point"), "field_declaration"),
            ("Bits", None, "union_item"),
            ("Bits::i", Some("Bits"), "field_declaration"),
            ("Bits::f", Some("Bits"), "field_declaration"),
            ("Color", None, "enum"),
            ("Color::Blue", Some("Color"), "variant"),
            ("Color::Blue::b", Some("Color::Blue"), "field_declaration"),
            ("Tagged", None, "struct"),
        ]
    );
    let x = symbol(&document, "Point::x");
    assert_eq!(
        x.kinds,
        [SymbolKind::Value, SymbolKind::Member, SymbolKind::Public]
    );
    assert_eq!(x.visibility.as_deref(), Some("pub"));
    assert_eq!(
        x.documentation.first().map(|doc| doc.text.as_str()),
        Some("Horizontal.")
    );
    assert_eq!(spelled(text, x.range), "/// Horizontal.\n    pub x: i32");
    assert_eq!(x.name_range.map(|range| spelled(text, range)), Some("x"));
    let y = symbol(&document, "Point::y");
    assert_eq!(y.kinds, [SymbolKind::Value, SymbolKind::Member]);
    assert_eq!(y.visibility.as_deref(), Some("private"));
    assert_eq!(
        symbol(&document, "Bits").kinds,
        [SymbolKind::Type, SymbolKind::Public]
    );
    let blue = symbol(&document, "Color::Blue::b");
    assert_eq!(blue.visibility.as_deref(), Some("pub"));
    assert!(blue.kinds.contains(&SymbolKind::Public));
}

/// A trait's associated type and constant declare under the trait, beside
/// the impl's definitions under the implementing type, and a trait carries
/// the categories of a type others implement.
#[test]
fn test_trait_associated_items_declare_under_the_trait() {
    let text = "pub trait Shape {\n    type Unit;\n    const SIDES: u32;\n    fn area(&self) -> f64;\n}\n\
                pub struct Point;\n\
                impl Shape for Point {\n    type Unit = i32;\n    const SIDES: u32 = 0;\n    \
                fn area(&self) -> f64 { 1.0 }\n}\n";
    let document = analyze(text);
    assert_eq!(
        declared(&document),
        [
            ("Shape", None, "trait"),
            ("Shape::Unit", Some("Shape"), "associated_type"),
            ("Shape::SIDES", Some("Shape"), "constant"),
            ("Shape::area", Some("Shape"), "function"),
            (
                "Shape::area::self#parameter0",
                Some("Shape::area"),
                "parameter_binding"
            ),
            ("Point", None, "struct"),
            ("Point", None, "impl_item"),
            ("Point::Unit", Some("Point"), "type_alias"),
            ("Point::SIDES", Some("Point"), "constant"),
            ("Point::area", Some("Point"), "function"),
            (
                "Point::area::self#parameter0",
                Some("Point::area"),
                "parameter_binding"
            ),
        ]
    );
    assert_eq!(
        symbol(&document, "Shape").kinds,
        [
            SymbolKind::Type,
            SymbolKind::Implementable,
            SymbolKind::MemberContainer,
            SymbolKind::Public,
        ]
    );
    assert_eq!(
        symbol(&document, "Shape::Unit").kinds,
        [SymbolKind::Type, SymbolKind::Member, SymbolKind::Public]
    );
}

/// An item inside a trait, or inside an impl of a trait the file declares,
/// takes the trait's visibility, since the language spells none on it. An
/// impl of a trait declared elsewhere states no visibility.
#[test]
fn test_trait_items_take_the_visibility_of_their_trait() {
    let text = "pub trait Shape { fn area(&self) -> f64; }\n\
                pub(crate) trait Inner { fn depth(&self); }\n\
                pub struct Point;\n\
                impl Shape for Point { fn area(&self) -> f64 { 1.0 } }\n\
                impl Inner for Point { fn depth(&self) {} }\n\
                impl std::fmt::Display for Point {\n    \
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n}\n\
                impl Point { fn own(&self) {} }\n";
    let document = analyze(text);
    let visibility = |qualified: &str| symbol(&document, qualified).visibility.as_deref();
    assert_eq!(visibility("Shape::area"), Some("pub"));
    assert_eq!(visibility("Inner::depth"), Some("pub(crate)"));
    assert_eq!(visibility("Point::area"), Some("pub"));
    assert!(
        symbol(&document, "Point::area")
            .kinds
            .contains(&SymbolKind::Public)
    );
    assert_eq!(visibility("Point::depth"), Some("pub(crate)"));
    assert_eq!(visibility("Point::fmt"), None);
    assert_eq!(visibility("Point::own"), Some("private"));
}

/// A function nested in another qualifies under it, so two nested
/// functions sharing a name in different parents are distinct symbols.
#[test]
fn test_nested_functions_qualify_under_their_enclosing_function() {
    let document = analyze(
        "pub fn a() -> i32 { fn inner() -> i32 { 1 } inner() }\n\
         pub fn b() -> i32 { fn inner() -> i32 { 2 } inner() }\n",
    );
    assert_eq!(
        declared(&document),
        [
            ("a", None, "function"),
            ("a::inner", Some("a"), "function"),
            ("b", None, "function"),
            ("b::inner", Some("b"), "function"),
        ]
    );
}

#[test]
fn test_a_qualified_trait_does_not_take_an_unrelated_local_traits_visibility() {
    let document = analyze(
        "pub trait Shape { fn area(&self); }\nstruct Point;\nimpl external::Shape for Point { fn area(&self) {} }\n",
    );
    assert_eq!(symbol(&document, "Point::area").visibility, None);
    assert!(
        !symbol(&document, "Point::area")
            .kinds
            .contains(&SymbolKind::Public)
    );
}
