//! Rust syntax facts from the pinned tree-sitter-rust grammar.

mod attachment;
mod blanking;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rift_error::errors;
use rift_protocol::read::{Language, SymbolKind, SyntaxKind};
use strum::VariantArray;
use tree_sitter::{Node, Parser, Query as TreeSitterQuery, QueryCursor, StreamingIterator};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::failure::{RiftError, invalid_query};
use crate::provider::{SOURCE_BYTES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource};

mod generics;
pub(crate) mod local_bindings;

/// Rust declaration kind emitted by the Tree-sitter provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::VariantArray)]
enum RustSymbolKind {
    /// Function or method.
    Function,
    /// Structure.
    Struct,
    /// Enumeration.
    Enum,
    /// Trait.
    Trait,
    /// Type alias.
    TypeAlias,
    /// Opaque type declared in a foreign block.
    ExternType,
    /// Constant.
    Constant,
    /// Static item.
    Static,
    /// Module.
    Module,
    /// Declarative macro.
    Macro,
    /// Enum variant.
    Variant,
    /// Named field of a struct, a union, or a struct-like variant.
    Field,
    /// Union.
    Union,
    /// Associated type a trait declares.
    AssociatedType,
    /// Implementation of a named type.
    Impl,
    /// A local binding whose value is a closure.
    Closure,
    /// A value bound by an ordinary let statement.
    LocalBinding,
    /// A declaration at an authored shorthand field pattern.
    ShorthandBinding,
    /// A value parameter named in a function's signature.
    ParameterBinding,
    /// A declared generic type.
    TypeParameter,
    /// A declared generic lifetime.
    LifetimeParameter,
    /// A declared generic constant.
    ConstParameter,
}

impl RustSymbolKind {
    /// The kind word this declaration kind carries on the wire.
    const fn word(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::TypeAlias => "type_alias",
            Self::ExternType => "extern_type",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Variant => "variant",
            Self::Field => "field_declaration",
            Self::Union => "union_item",
            Self::AssociatedType => "associated_type",
            Self::Impl => "impl_item",
            Self::Closure => "let_declaration",
            Self::LocalBinding => local_bindings::KIND,
            Self::ShorthandBinding => local_bindings::SHORTHAND_KIND,
            Self::ParameterBinding => local_bindings::PARAMETER_KIND,
            Self::TypeParameter => "type_parameter",
            Self::LifetimeParameter => "lifetime_parameter",
            Self::ConstParameter => "const_parameter",
        }
    }

    /// Portable kinds for this kind, before visibility adds `Public`.
    fn kinds(self) -> Vec<SymbolKind> {
        match self {
            Self::Function | Self::Closure => vec![SymbolKind::Value, SymbolKind::Callable],
            Self::Struct | Self::Enum | Self::Union | Self::ExternType => vec![SymbolKind::Type],
            Self::Trait => vec![
                SymbolKind::Type,
                SymbolKind::Implementable,
                SymbolKind::MemberContainer,
            ],
            Self::Field => vec![SymbolKind::Value, SymbolKind::Member],
            Self::Impl => vec![SymbolKind::MemberContainer],
            Self::AssociatedType => vec![SymbolKind::Type, SymbolKind::Member],
            Self::TypeAlias => vec![SymbolKind::Type, SymbolKind::Alias],
            Self::Module => vec![SymbolKind::Namespace, SymbolKind::Module],
            Self::Macro => vec![SymbolKind::Macro],
            Self::Constant | Self::Static | Self::LocalBinding | Self::ShorthandBinding => {
                vec![SymbolKind::Value]
            }
            Self::ParameterBinding | Self::ConstParameter => {
                vec![SymbolKind::Value, SymbolKind::Parameter]
            }
            Self::TypeParameter => vec![SymbolKind::Type, SymbolKind::TypeParameter],
            Self::LifetimeParameter => vec![SymbolKind::TypeParameter],
            Self::Variant => vec![SymbolKind::Variant],
        }
    }

    /// The grammar field spanning this kind's implementation part; `None`
    /// for a kind whose grammar declares no body or value field.
    const fn body_field(self) -> Option<RustGrammarField> {
        match self {
            Self::Function
            | Self::Struct
            | Self::Enum
            | Self::Trait
            | Self::Module
            | Self::Variant
            | Self::Union
            | Self::Impl => Some(RustGrammarField::Body),
            Self::Constant | Self::Static => Some(RustGrammarField::Value),
            Self::TypeAlias
            | Self::ExternType
            | Self::Macro
            | Self::Field
            | Self::AssociatedType
            | Self::Closure
            | Self::LocalBinding
            | Self::ShorthandBinding
            | Self::ParameterBinding
            | Self::TypeParameter
            | Self::LifetimeParameter
            | Self::ConstParameter => None,
        }
    }
}

/// Tree-sitter grammar syntax element kind interpreted by this module.
///
/// Vocabulary comes from the `node-types.json` of the pinned
/// tree-sitter-rust 0.24.2 grammar. The grammar defines many more kinds;
/// only the ones this module reads are listed, so conversion from an
/// arbitrary kind string is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray)]
enum RustGrammarSyntaxKind {
    /// `let_declaration` binding.
    LetDeclaration,
    /// `parameter` in a function's parameter list.
    Parameter,
    /// Receiver parameter, including reference and lifetime forms.
    SelfParameter,
    /// Name the author wrote in a shorthand field of a struct pattern.
    ShorthandFieldIdentifier,
    /// Generic type parameter.
    TypeParameter,
    /// Generic lifetime parameter.
    LifetimeParameter,
    /// Generic const parameter.
    ConstParameter,
    /// `closure_expression` value.
    ClosureExpression,
    /// `const_item` declaration.
    ConstItem,
    /// `enum_item` declaration.
    EnumItem,
    /// `enum_variant` inside an enum's variant list.
    EnumVariant,
    /// `function_item` declaration.
    FunctionItem,
    /// `function_signature_item`: a function without a body, in a trait or
    /// an `extern` block.
    FunctionSignatureItem,
    /// `impl_item` block.
    ImplItem,
    /// `macro_definition` declaration.
    MacroDefinition,
    /// `mod_item` declaration.
    ModItem,
    /// `static_item` declaration.
    StaticItem,
    /// `struct_item` declaration.
    StructItem,
    /// `trait_item` declaration.
    TraitItem,
    /// `type_item` declaration.
    TypeItem,
    /// Opaque type under an extern block.
    ExternTypeItem,
    /// `visibility_modifier` marker on one declaration.
    VisibilityModifier,
    /// `field_declaration`: one named field.
    FieldDeclaration,
    /// `union_item` declaration.
    UnionItem,
    /// `associated_type`: `type Unit;` inside a trait.
    AssociatedType,
    /// `declaration_list`: the item list of a trait, an impl, or a module.
    DeclarationList,
    /// `field_declaration_list`: the named fields of a struct, union, or
    /// variant.
    FieldDeclarationList,
    /// `enum_variant_list`: the variants of an enum.
    EnumVariantList,
}

impl RustGrammarSyntaxKind {
    /// Returns grammar spelling from tree-sitter-rust `node-types.json`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::LetDeclaration => "let_declaration",
            Self::Parameter => "parameter",
            Self::SelfParameter => "self_parameter",
            Self::ShorthandFieldIdentifier => "shorthand_field_identifier",
            Self::TypeParameter => "type_parameter",
            Self::LifetimeParameter => "lifetime_parameter",
            Self::ConstParameter => "const_parameter",
            Self::ClosureExpression => "closure_expression",
            Self::ConstItem => "const_item",
            Self::EnumItem => "enum_item",
            Self::EnumVariant => "enum_variant",
            Self::FunctionItem => "function_item",
            Self::FunctionSignatureItem => "function_signature_item",
            Self::ImplItem => "impl_item",
            Self::MacroDefinition => "macro_definition",
            Self::ModItem => "mod_item",
            Self::StaticItem => "static_item",
            Self::StructItem => "struct_item",
            Self::TraitItem => "trait_item",
            Self::TypeItem => "type_item",
            Self::ExternTypeItem => "extern_type_item",
            Self::VisibilityModifier => "visibility_modifier",
            Self::FieldDeclaration => "field_declaration",
            Self::UnionItem => "union_item",
            Self::AssociatedType => "associated_type",
            Self::DeclarationList => "declaration_list",
            Self::FieldDeclarationList => "field_declaration_list",
            Self::EnumVariantList => "enum_variant_list",
        }
    }

    /// Classifies grammar kinds the tree walk does not interpret as `None`.
    fn from_kind(kind: &str) -> Option<Self> {
        Self::VARIANTS
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == kind)
    }

    const fn symbol_kind(self) -> Option<RustSymbolKind> {
        match self {
            Self::LetDeclaration => Some(RustSymbolKind::Closure),
            Self::ShorthandFieldIdentifier => Some(RustSymbolKind::ShorthandBinding),
            Self::Parameter | Self::SelfParameter => Some(RustSymbolKind::ParameterBinding),
            Self::TypeParameter => Some(RustSymbolKind::TypeParameter),
            Self::LifetimeParameter => Some(RustSymbolKind::LifetimeParameter),
            Self::ConstParameter => Some(RustSymbolKind::ConstParameter),
            Self::ImplItem => Some(RustSymbolKind::Impl),
            Self::FunctionItem | Self::FunctionSignatureItem => Some(RustSymbolKind::Function),
            Self::StructItem => Some(RustSymbolKind::Struct),
            Self::EnumItem => Some(RustSymbolKind::Enum),
            Self::EnumVariant => Some(RustSymbolKind::Variant),
            Self::TraitItem => Some(RustSymbolKind::Trait),
            Self::TypeItem => Some(RustSymbolKind::TypeAlias),
            Self::ExternTypeItem => Some(RustSymbolKind::ExternType),
            Self::ConstItem => Some(RustSymbolKind::Constant),
            Self::StaticItem => Some(RustSymbolKind::Static),
            Self::ModItem => Some(RustSymbolKind::Module),
            Self::MacroDefinition => Some(RustSymbolKind::Macro),
            Self::FieldDeclaration => Some(RustSymbolKind::Field),
            Self::UnionItem => Some(RustSymbolKind::Union),
            Self::AssociatedType => Some(RustSymbolKind::AssociatedType),
            Self::ClosureExpression
            | Self::VisibilityModifier
            | Self::DeclarationList
            | Self::FieldDeclarationList
            | Self::EnumVariantList => None,
        }
    }
}

impl std::str::FromStr for RustGrammarSyntaxKind {
    type Err = RiftError;

    fn from_str(kind: &str) -> Result<Self, Self::Err> {
        Self::from_kind(kind).ok_or_else(|| {
            errors::syntax::unknown_syntax_element_kind()
                .syntax_element_kind(kind)
                .error()
        })
    }
}

/// Grammar field name this module reads, from tree-sitter-rust 0.24.2
/// `node-types.json`.
#[derive(Debug, Clone, Copy)]
enum RustGrammarField {
    /// `name` field on declaration items.
    Name,
    /// `type` field on `impl_item`, and on the `generic_type` an `impl_item`
    /// carries when it implements a generic type.
    Type,
    /// `body` field on block-bodied declaration items.
    Body,
    /// `value` field on `const_item` and `static_item`.
    Value,
    /// `trait` field on an `impl_item` that implements a trait.
    Trait,
    /// `pattern` field on a local binding.
    Pattern,
}

impl RustGrammarField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Type => "type",
            Self::Body => "body",
            Self::Value => "value",
            Self::Trait => "trait",
            Self::Pattern => "pattern",
        }
    }
}

/// Authored visibility of one Rust declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RustVisibility {
    /// Declaration has no visibility modifier.
    Private,
    /// Declaration uses `pub`.
    Public,
    /// Declaration uses exact restricted visibility spelling such as `pub(crate)`.
    Restricted(String),
}

impl RustVisibility {
    /// Classifies authored `visibility_modifier` text from tree-sitter-rust grammar.
    ///
    /// Bare `pub` is [`RustVisibility::Public`]; any other authored spelling
    /// such as `pub(crate)` or `pub(in path)` stays verbatim in
    /// [`RustVisibility::Restricted`]. Declarations without a modifier never
    /// reach this conversion and stay [`RustVisibility::Private`].
    fn from_authored(text: &str) -> Self {
        match text {
            "pub" => Self::Public,
            restricted => Self::Restricted(restricted.into()),
        }
    }

    /// The authored spelling served on the wire: `private`, `pub`, or the
    /// restricted form verbatim.
    fn authored(&self) -> String {
        match self {
            Self::Private => "private".into(),
            Self::Public => "pub".into(),
            Self::Restricted(authored) => authored.clone(),
        }
    }
}

/// One bounded match produced by a Rift-owned Rust query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustQueryCapture {
    /// Query capture name.
    pub name: String,
    /// Captured syntax range.
    pub range: ByteRange,
}

/// Rift-owned adapter around Tree-sitter Rust queries.
#[derive(Debug)]
pub struct RustQuery {
    inner: TreeSitterQuery,
}

impl RustQuery {
    /// Compiles one query against pinned Rust grammar.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when query cannot compile.
    pub fn new(source: &str) -> Result<Self, RiftError> {
        let language = rust_grammar();
        let inner = TreeSitterQuery::new(&language, source)
            .map_err(|error| invalid_query(source, error))?;
        Ok(Self { inner })
    }

    /// Returns query capture vocabulary.
    #[must_use]
    pub fn capture_names(&self) -> &[&str] {
        self.inner.capture_names()
    }

    /// Reports whether query declares capture name.
    #[must_use]
    pub fn has_capture(&self, name: &str) -> bool {
        self.capture_names().contains(&name)
    }

    /// Executes query with explicit source and capture bounds.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for zero bound, incompatible grammar,
    /// cancellation, oversized source, or capture overflow.
    pub fn captures(
        &self,
        source: &str,
        captures_max: usize,
    ) -> Result<Vec<RustQueryCapture>, RiftError> {
        if captures_max == 0 {
            return errors::syntax::zero_limit().bound("captures_max").fail();
        }
        if source.len() > SOURCE_BYTES_MAX_DEFAULT {
            return errors::syntax::source_too_large()
                .source_bytes(source.len())
                .source_bytes_max(SOURCE_BYTES_MAX_DEFAULT)
                .fail();
        }
        let mut parser = rust_parser()?;
        let tree = parse_tree(&mut parser, source, SyntaxLimits::DEFAULT)
            .ok_or_else(|| errors::syntax::parse_cancelled().error())?;
        let mut cursor = QueryCursor::new();
        let mut query_captures = cursor.captures(&self.inner, tree.root_node(), source.as_bytes());
        let mut captures = Vec::new();
        while let Some((query_match, capture_index)) = query_captures.next() {
            if captures.len() >= captures_max {
                return errors::syntax::too_many_captures()
                    .captures_max(captures_max)
                    .fail();
            }
            let capture = query_match.captures()[*capture_index];
            captures.push(RustQueryCapture {
                name: self.inner.capture_names()[capture.index as usize].into(),
                range: extract::byte_range(capture.node)?,
            });
        }
        Ok(captures)
    }
}

/// Bounded Tree-sitter Rust fact provider.
#[derive(Debug, Clone)]
pub struct RustSyntaxProvider {
    language: Language,
}

impl Default for RustSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "rust".to_owned(),
                dialect: None,
            },
        }
    }
}

impl SyntaxProvider for RustSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        limits.admit_source(source)?;
        let mut parser = rust_parser()?;
        let tree = parse_tree(&mut parser, source.text, limits)
            .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
        let mut capture = crate::static_analysis::Capture::for_rust(source.text, limits);
        let rules = RustGrammarRules {
            traits: file_traits(tree.root_node(), source.text),
        };
        let (syntax_elements, mut symbols, _) = extract::extract_with_capture(
            tree.root_node(),
            source,
            limits,
            &self.language,
            &rules,
            Some(&mut capture),
        )?;
        let mut facts = capture.finish();
        local_bindings::prepare(source.text, &syntax_elements, &mut symbols, &mut facts);
        let generic_references =
            generics::references(source.text, &syntax_elements, &symbols, &facts);
        facts.set_rust_generic_references(generic_references);
        Ok(SyntaxDocument::new_rust(
            self.language.clone(),
            source,
            limits,
            syntax_elements,
            symbols,
            tree.root_node().has_error(),
        )
        .with_static_facts(Some(facts)))
    }

    fn syntax_element_kinds(&self, kind: &str) -> Vec<SyntaxKind> {
        let mut kinds = Vec::new();
        if kind == attachment::ATTRIBUTE_ITEM_KIND || kind == INNER_ATTRIBUTE_ITEM_KIND {
            return vec![SyntaxKind::Annotation];
        }
        if kind.ends_with("_item") || kind.ends_with("_declaration") {
            kinds.extend([SyntaxKind::Declaration, SyntaxKind::Definition]);
        }
        if kind.ends_with("_expression") {
            kinds.push(SyntaxKind::Expression);
        }
        if kind.ends_with("_statement") {
            kinds.push(SyntaxKind::Statement);
        }
        if kind.contains("comment") {
            kinds.push(SyntaxKind::Comment);
        }
        kinds
    }
}

/// tree-sitter-rust's decisions for the shared bounded walk.
#[derive(Debug)]
struct RustGrammarRules {
    /// The visibility of every trait the file declares, by name; `None`
    /// for a name two traits share with different visibilities.
    traits: BTreeMap<String, Option<RustVisibility>>,
}

/// Where a declaration's visibility comes from.
#[derive(Debug, PartialEq, Eq)]
enum VisibilityOwner {
    /// The declaration's own `visibility_modifier`, `private` without one.
    Own,
    /// The trait, trait impl, or enum holding it, since the language
    /// spells no visibility on its items.
    Inherited(RustVisibility),
    /// An item of an impl whose trait the file does not declare: the
    /// language spells no visibility, and the trait's is not in the file.
    Unknown,
}

/// The visibility of each trait declared at module scope in the file -
/// the source root and the bodies of inline `mod` items, never function
/// bodies - by trait name. Each syntax element is read once, so the work is linear in
/// the parsed tree.
fn file_traits(root: Node<'_>, text: &str) -> BTreeMap<String, Option<RustVisibility>> {
    let mut traits: BTreeMap<String, Option<RustVisibility>> = BTreeMap::new();
    let mut pending = vec![root];
    let mut cursor = root.walk();
    while let Some(scope) = pending.pop() {
        for item in scope.named_children(&mut cursor) {
            match RustGrammarSyntaxKind::from_kind(item.kind()) {
                Some(RustGrammarSyntaxKind::TraitItem) => {
                    let Some(name) = declaration_name(item, text) else {
                        continue;
                    };
                    let visibility = declaration_visibility(item, text);
                    traits
                        .entry(name)
                        .and_modify(|known| {
                            if known.as_ref() != Some(&visibility) {
                                *known = None;
                            }
                        })
                        .or_insert_with(|| Some(visibility.clone()));
                }
                Some(RustGrammarSyntaxKind::ModItem) => {
                    pending.extend(item.child_by_field_name(RustGrammarField::Body.as_str()));
                }
                _ => {}
            }
        }
    }
    traits
}

/// The trait path an impl's `trait` field spells, without generic arguments.
/// Qualified paths stay qualified, so an external trait cannot take the
/// visibility of a local trait sharing its last name.
fn implemented_trait_name<'text>(implementation: Node<'_>, text: &'text str) -> Option<&'text str> {
    let mut name = implementation.child_by_field_name(RustGrammarField::Trait.as_str())?;
    if name.kind() == GENERIC_TYPE_KIND {
        name = name.child_by_field_name(RustGrammarField::Type.as_str())?;
    }
    text.get(name.byte_range())
}

impl RustGrammarRules {
    /// Where the visited declaration's visibility comes from: a trait item
    /// takes its trait's, an item of a trait impl takes the visibility of the
    /// trait the file declares under that name, and a variant and its fields
    /// take their enum's.
    fn visibility_owner(&self, visited: Visited<'_, '_>, text: &str) -> VisibilityOwner {
        let kind = |visited: Option<Visited<'_, '_>>| {
            visited.and_then(|visited| {
                RustGrammarSyntaxKind::from_kind(visited.syntax_element().kind())
            })
        };
        let parent = visited.parent();
        let holder = parent.and_then(Visited::parent);
        match (kind(parent), kind(holder)) {
            (
                Some(RustGrammarSyntaxKind::DeclarationList),
                Some(RustGrammarSyntaxKind::TraitItem),
            ) => {
                let trait_item = holder.map(Visited::syntax_element);
                VisibilityOwner::Inherited(
                    trait_item.map_or(RustVisibility::Private, |syntax_element| {
                        declaration_visibility(syntax_element, text)
                    }),
                )
            }
            (
                Some(RustGrammarSyntaxKind::DeclarationList),
                Some(RustGrammarSyntaxKind::ImplItem),
            ) => {
                let Some(implementation) = holder.map(Visited::syntax_element) else {
                    return VisibilityOwner::Own;
                };
                if implementation
                    .child_by_field_name(RustGrammarField::Trait.as_str())
                    .is_none()
                {
                    return VisibilityOwner::Own;
                }
                implemented_trait_name(implementation, text)
                    .and_then(|name| self.traits.get(name).cloned().flatten())
                    .map_or(VisibilityOwner::Unknown, VisibilityOwner::Inherited)
            }
            (
                Some(RustGrammarSyntaxKind::EnumVariantList),
                Some(RustGrammarSyntaxKind::EnumItem),
            ) => {
                VisibilityOwner::Inherited(holder.map_or(RustVisibility::Private, |enumeration| {
                    declaration_visibility(enumeration.syntax_element(), text)
                }))
            }
            (
                Some(RustGrammarSyntaxKind::FieldDeclarationList),
                Some(RustGrammarSyntaxKind::EnumVariant),
            ) => holder.map_or(VisibilityOwner::Own, |variant| {
                self.visibility_owner(variant, text)
            }),
            _ => VisibilityOwner::Own,
        }
    }
}

impl GrammarRules for RustGrammarRules {
    fn name_range(&self, syntax_element: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        syntax_element
            .child_by_field_name(RustGrammarField::Name.as_str())
            .or_else(|| local_bindings::receiver(syntax_element))
            .or_else(|| {
                (syntax_element.kind() == "shorthand_field_identifier").then_some(syntax_element)
            })
            .or_else(|| {
                matches!(
                    RustGrammarSyntaxKind::from_kind(syntax_element.kind()),
                    Some(RustGrammarSyntaxKind::LetDeclaration | RustGrammarSyntaxKind::Parameter)
                )
                .then(|| syntax_element.child_by_field_name(RustGrammarField::Pattern.as_str()))
                .flatten()
            })
            .or_else(|| {
                (RustGrammarSyntaxKind::from_kind(syntax_element.kind())
                    == Some(RustGrammarSyntaxKind::ImplItem))
                .then(|| syntax_element.child_by_field_name(RustGrammarField::Type.as_str()))
                .flatten()
                .map(implemented_type)
            })
            .map(extract::byte_range)
            .transpose()
    }

    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(mut kind) = RustGrammarSyntaxKind::from_kind(syntax_element.kind())
            .and_then(RustGrammarSyntaxKind::symbol_kind)
        else {
            return Ok(None);
        };
        let name = if kind == RustSymbolKind::ParameterBinding {
            local_bindings::parameter_name(visited, text).map(str::to_owned)
        } else if kind == RustSymbolKind::ShorthandBinding {
            local_bindings::shorthand_name(visited, text).map(str::to_owned)
        } else if matches!(
            kind,
            RustSymbolKind::TypeParameter
                | RustSymbolKind::LifetimeParameter
                | RustSymbolKind::ConstParameter
        ) {
            local_bindings::generic_name(visited, text).map(str::to_owned)
        } else {
            declaration_name(syntax_element, text).or_else(|| {
                if kind != RustSymbolKind::Closure {
                    return None;
                }
                let name = local_bindings::name(visited, text)?;
                kind = RustSymbolKind::LocalBinding;
                Some(name.to_owned())
            })
        };
        let Some(name) = name else {
            return Ok(None);
        };
        let visibility = match self.visibility_owner(visited, text) {
            VisibilityOwner::Own => Some(declared_visibility(visited, kind, text)),
            VisibilityOwner::Inherited(visibility) => Some(visibility),
            VisibilityOwner::Unknown => None,
        };
        let mut kinds = kind.kinds();
        if visibility == Some(RustVisibility::Public) {
            kinds.push(SymbolKind::Public);
        }
        if is_entrypoint(visited, kind, &name) {
            kinds.push(SymbolKind::Entrypoint);
        }
        let (documentation, documentation_ranges) =
            attachment::attached_documentation(visited, text)?;
        Ok(Some(Declaration {
            name,
            kind: if kind == RustSymbolKind::LocalBinding
                && local_bindings::explicit_mut(syntax_element)
            {
                local_bindings::EXPLICIT_LOCAL_KIND
            } else {
                kind.word()
            },
            kinds,
            visibility: visibility.as_ref().map(RustVisibility::authored),
            body_range: body_range(syntax_element, kind)?,
            documentation,
            documentation_ranges,
        }))
    }

    fn container_name(&self, syntax_element: Node<'_>, text: &str) -> Option<String> {
        match RustGrammarSyntaxKind::from_kind(syntax_element.kind())? {
            RustGrammarSyntaxKind::ModItem
            | RustGrammarSyntaxKind::StructItem
            | RustGrammarSyntaxKind::EnumItem
            | RustGrammarSyntaxKind::TraitItem
            | RustGrammarSyntaxKind::TypeItem
            | RustGrammarSyntaxKind::AssociatedType
            | RustGrammarSyntaxKind::UnionItem
            | RustGrammarSyntaxKind::EnumVariant
            | RustGrammarSyntaxKind::FunctionItem
            | RustGrammarSyntaxKind::FunctionSignatureItem => {
                let name = syntax_element.child_by_field_name(RustGrammarField::Name.as_str())?;
                text.get(name.byte_range()).map(Into::into)
            }
            RustGrammarSyntaxKind::LetDeclaration => declaration_name(syntax_element, text),
            RustGrammarSyntaxKind::ImplItem => {
                let item = syntax_element.child_by_field_name(RustGrammarField::Type.as_str())?;
                implemented_type_name(item, text)
            }
            _ => None,
        }
    }

    /// A declaration's start, extended over its attached outer attributes
    /// and outer doc comments so the whole declaration - not just the item
    /// syntax element - is what the symbol read returns.
    fn declaration_start(&self, visited: Visited<'_, '_>, text: &str) -> usize {
        local_bindings::pattern(visited.syntax_element()).map_or_else(
            || attachment::declaration_start(visited, text),
            |name| name.start_byte(),
        )
    }

    fn declaration_syntax_element<'tree>(&self, visited: Visited<'_, 'tree>) -> Node<'tree> {
        let item = visited.syntax_element();
        local_bindings::pattern(item).unwrap_or(item)
    }

    fn qualification_separator(&self) -> &'static str {
        "::"
    }

    fn module_path(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<crate::RustModulePath>, RiftError> {
        if visited.syntax_element().kind() != RustGrammarSyntaxKind::ModItem.as_str() {
            return Ok(None);
        }
        attachment::module_path(visited, text).map(Some)
    }
}

pub(crate) fn rust_grammar() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

pub(crate) fn parse_tree(
    parser: &mut Parser,
    text: &str,
    limits: SyntaxLimits,
) -> Option<tree_sitter::Tree> {
    let prepared = blanking::blank_const_trait_keywords(text);
    let tree = parser.parse(prepared.as_ref(), None)?;
    let Some(repaired) = blanking::blank_empty_trait_bounds(
        prepared.as_ref(),
        tree.root_node(),
        limits.syntax_elements_max(),
    ) else {
        return Some(tree);
    };
    parser.parse(repaired, None).or(Some(tree))
}

fn rust_parser() -> Result<Parser, RiftError> {
    let language = rust_grammar();
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|_| {
        errors::syntax::incompatible_grammar()
            .grammar_abi_version(language.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error()
    })?;
    Ok(parser)
}

fn declaration_name(syntax_element: Node<'_>, text: &str) -> Option<String> {
    if syntax_element.kind() == RustGrammarSyntaxKind::ImplItem.as_str() {
        return implemented_type_name(
            syntax_element.child_by_field_name(RustGrammarField::Type.as_str())?,
            text,
        );
    }
    if RustGrammarSyntaxKind::from_kind(syntax_element.kind())
        == Some(RustGrammarSyntaxKind::LetDeclaration)
    {
        let value = syntax_element.child_by_field_name(RustGrammarField::Value.as_str())?;
        if RustGrammarSyntaxKind::from_kind(value.kind())
            != Some(RustGrammarSyntaxKind::ClosureExpression)
        {
            return None;
        }
        let name = syntax_element.child_by_field_name(RustGrammarField::Pattern.as_str())?;
        return text
            .get(name.byte_range())
            .filter(|name| *name != "_" && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
            .map(str::to_owned);
    }
    let name = syntax_element
        .child_by_field_name(RustGrammarField::Name.as_str())
        .or_else(|| {
            (RustGrammarSyntaxKind::from_kind(syntax_element.kind())
                == Some(RustGrammarSyntaxKind::ImplItem))
            .then(|| syntax_element.child_by_field_name(RustGrammarField::Type.as_str()))
            .flatten()
            .map(implemented_type)
        })?;
    text.get(name.byte_range()).map(Into::into)
}

/// Grammar spelling of a type applying generic arguments, from
/// tree-sitter-rust 0.24.2 `node-types.json`.
const GENERIC_TYPE_KIND: &str = "generic_type";
/// Grammar spelling of an inner attribute (`#![..]`), from tree-sitter-rust's
/// `node-types.json`.
const INNER_ATTRIBUTE_ITEM_KIND: &str = "inner_attribute_item";

/// The type an `impl` block implements, without the generic arguments the
/// block applies to it: methods of `impl<T> Holder<T>` qualify through
/// `Holder`, the name the `struct` itself declares. Grouping parentheses keep
/// the wrapped type's owner, while a comma keeps a tuple distinct.
pub(crate) fn implemented_type(mut element: Node<'_>) -> Node<'_> {
    // Each step removes one type wrapper from the admitted syntax tree.
    loop {
        if element.kind() == GENERIC_TYPE_KIND {
            let Some(inner) = element.child_by_field_name(RustGrammarField::Type.as_str()) else {
                return element;
            };
            element = inner;
        } else if element.kind() == "tuple_type" {
            let mut cursor = element.walk();
            if element
                .children(&mut cursor)
                .any(|child| child.kind() == ",")
            {
                return element;
            }
            let mut children = element
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let Some(inner) = children.next() else {
                return element;
            };
            if children.next().is_some() {
                return element;
            }
            element = inner;
        } else {
            return element;
        }
    }
}

/// Structural owners retain reference mutability, array lengths and tuple element order.
/// Lifetimes and generic arguments do not change the declaration's logical owner name.
fn implemented_type_name(element: Node<'_>, text: &str) -> Option<String> {
    enum Part<'tree> {
        Type(Node<'tree>),
        ArrayEnd(Option<Node<'tree>>),
        Token(&'static str),
    }
    static KINDS: OnceLock<([u16; 5], [u16; 3])> = OnceLock::new();
    let (kinds, fields) = KINDS.get_or_init(|| {
        let grammar = rust_grammar();
        (
            [
                "generic_type",
                "reference_type",
                "mutable_specifier",
                "array_type",
                "tuple_type",
            ]
            .map(|kind| grammar.id_for_node_kind(kind, true)),
            ["type", "length", "element"].map(|field| {
                grammar
                    .field_id_for_name(field)
                    .expect("pinned type field")
                    .get()
            }),
        )
    });
    let mut pending = vec![Part::Type(element)];
    let mut name = String::new();
    // Each part visits one type child or closes it, bounded by syntax depth.
    while let Some(part) = pending.pop() {
        match part {
            Part::Token(token) => name.push_str(token),
            Part::ArrayEnd(length) => {
                if let Some(length) = length {
                    name.push(';');
                    name.extend(text.get(length.byte_range())?.split_whitespace());
                }
                name.push(']');
            }
            Part::Type(element) => {
                let element = implemented_type(element);
                match element.kind_id() {
                    kind if kind == kinds[0] => {
                        pending.push(Part::Type(element.child_by_field_id(fields[0])?));
                    }
                    kind if kind == kinds[1] => {
                        name.push('&');
                        let mut cursor = element.walk();
                        if element
                            .named_children(&mut cursor)
                            .any(|child| child.kind_id() == kinds[2])
                        {
                            name.push_str("mut ");
                        }
                        pending.push(Part::Type(element.child_by_field_id(fields[0])?));
                    }
                    kind if kind == kinds[3] => {
                        name.push('[');
                        pending.push(Part::ArrayEnd(element.child_by_field_id(fields[1])));
                        pending.push(Part::Type(element.child_by_field_id(fields[2])?));
                    }
                    kind if kind == kinds[4] => {
                        let mut cursor = element.walk();
                        let elements = element
                            .named_children(&mut cursor)
                            .filter(|child| !child.is_extra())
                            .collect::<Vec<_>>();
                        name.push('(');
                        pending.push(Part::Token(")"));
                        if elements.len() == 1 {
                            pending.push(Part::Token(","));
                        }
                        for index in (0..elements.len()).rev() {
                            pending.push(Part::Type(elements[index]));
                            if index != 0 {
                                pending.push(Part::Token(","));
                            }
                        }
                    }
                    _ => name.extend(text.get(element.byte_range())?.split_whitespace()),
                }
            }
        }
    }
    Some(name)
}

/// A declaration's visibility: its `visibility_modifier`, or, for a `macro_rules!`
/// macro that carries no modifier, `pub` when `#[macro_export]` is attached.
fn declared_visibility(
    visited: Visited<'_, '_>,
    kind: RustSymbolKind,
    text: &str,
) -> RustVisibility {
    let authored = declaration_visibility(visited.syntax_element(), text);
    if kind == RustSymbolKind::Macro
        && authored == RustVisibility::Private
        && attachment::exports_macro(visited, text)
    {
        return RustVisibility::Public;
    }
    authored
}

fn declaration_visibility(syntax_element: Node<'_>, text: &str) -> RustVisibility {
    let mut cursor = syntax_element.walk();
    syntax_element
        .named_children(&mut cursor)
        .find(|child| child.kind() == RustGrammarSyntaxKind::VisibilityModifier.as_str())
        .and_then(|child| text.get(child.byte_range()))
        .map_or(RustVisibility::Private, RustVisibility::from_authored)
}

fn declaration_kinds(kind: RustSymbolKind, visibility: &RustVisibility) -> Vec<SymbolKind> {
    let mut kinds = kind.kinds();
    if visibility == &RustVisibility::Public {
        kinds.push(SymbolKind::Public);
    }
    kinds
}

/// Numeric grammar ids for the ancestor kinds [`is_file_scope`] checks,
/// resolved once from the pinned Tree-sitter Rust language so each
/// classification compares integers instead of syntax element-kind strings.
struct RustEnclosureKinds {
    module: u16,
    implementation: u16,
    trait_definition: u16,
    function: u16,
}

impl RustEnclosureKinds {
    /// Resolves every kind id this module reads from `language`.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar has no syntax element kind by one of the named
    /// spellings: `id_for_node_kind` returning `0` means the grammar the
    /// runtime loaded no longer defines a kind this module depends on, a
    /// programmer/grammar-version error rather than a reachable runtime
    /// state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        let mod_item = language.id_for_node_kind(RustGrammarSyntaxKind::ModItem.as_str(), true);
        let impl_item = language.id_for_node_kind(RustGrammarSyntaxKind::ImplItem.as_str(), true);
        let trait_item = language.id_for_node_kind(RustGrammarSyntaxKind::TraitItem.as_str(), true);
        let function_item =
            language.id_for_node_kind(RustGrammarSyntaxKind::FunctionItem.as_str(), true);
        assert!(
            mod_item != 0,
            "pinned Rust grammar must define syntax element kind: kind=mod_item"
        );
        assert!(
            impl_item != 0,
            "pinned Rust grammar must define syntax element kind: kind=impl_item"
        );
        assert!(
            trait_item != 0,
            "pinned Rust grammar must define syntax element kind: kind=trait_item"
        );
        assert!(
            function_item != 0,
            "pinned Rust grammar must define syntax element kind: kind=function_item"
        );
        Self {
            module: mod_item,
            implementation: impl_item,
            trait_definition: trait_item,
            function: function_item,
        }
    }
}

/// Returns the process-wide resolved [`RustEnclosureKinds`], computing it once.
fn rust_enclosure_kinds() -> &'static RustEnclosureKinds {
    static KINDS: OnceLock<RustEnclosureKinds> = OnceLock::new();
    KINDS.get_or_init(|| RustEnclosureKinds::resolve(&rust_grammar()))
}

/// Reports whether `syntax element` is a direct child of the source file's root scope:
/// no ancestor is a `mod_item`, `impl_item`, `trait_item`, or
/// `function_item`. Bounded by the parsed tree's depth, which
/// [`SyntaxLimits`] caps during extraction.
fn is_file_scope(visited: Visited<'_, '_>) -> bool {
    let kinds = rust_enclosure_kinds();
    let mut ancestor = visited.parent();
    while let Some(current) = ancestor {
        let kind_id = current.syntax_element().kind_id();
        if kind_id == kinds.module
            || kind_id == kinds.implementation
            || kind_id == kinds.trait_definition
            || kind_id == kinds.function
        {
            return false;
        }
        ancestor = current.parent();
    }
    true
}

/// Reports whether `syntax element` is a file-scope `fn main` - the binary
/// entrypoint - so [`SymbolKind::Entrypoint`] applies. A `main` nested in
/// a `mod`, an `impl`, a `trait`, or another `fn` never qualifies.
fn is_entrypoint(visited: Visited<'_, '_>, kind: RustSymbolKind, name: &str) -> bool {
    kind == RustSymbolKind::Function && name == "main" && is_file_scope(visited)
}

/// The implementation part of one declaration: its grammar `body` or `value`
/// field's span. `None` when the kind declares no such field, or when this
/// syntax element omits it - a unit struct, a `mod name;`, a valueless trait constant.
fn body_range(
    syntax_element: Node<'_>,
    kind: RustSymbolKind,
) -> Result<Option<ByteRange>, RiftError> {
    if kind == RustSymbolKind::Closure {
        return syntax_element
            .child_by_field_name(RustGrammarField::Value.as_str())
            .and_then(|value| value.child_by_field_name(RustGrammarField::Body.as_str()))
            .map(extract::byte_range)
            .transpose();
    }
    let Some(field) = kind.body_field() else {
        return Ok(None);
    };
    let Some(body) = syntax_element.child_by_field_name(field.as_str()) else {
        return Ok(None);
    };
    extract::byte_range(body).map(Some)
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    <RustSymbolKind as strum::VariantArray>::VARIANTS
        .iter()
        .map(|kind| kind.word())
        .find(|word| *word == name)
}

pub(crate) fn literal_function(
    source: &str,
    language: &Language,
    name_range: ByteRange,
    item_range: ByteRange,
    body_range: ByteRange,
    public: bool,
) -> Option<crate::SyntaxSymbol> {
    let name = source
        .get(usize::try_from(name_range.start).ok()?..usize::try_from(name_range.end).ok()?)?
        .to_owned();
    let visibility = if public {
        RustVisibility::Public
    } else {
        RustVisibility::Private
    };
    let declaration = extract::Declaration {
        name: name.clone(),
        kind: RustSymbolKind::Function.word(),
        kinds: declaration_kinds(RustSymbolKind::Function, &visibility),
        visibility: Some(visibility.authored()),
        body_range: Some(body_range),
        documentation: Vec::new(),
        documentation_ranges: Vec::new(),
    };
    let signatures = extract::callable_signature(&declaration, item_range, source, language)?;
    Some(crate::SyntaxSymbol {
        declaration_ordinal: 0,
        name: name.clone(),
        qualified_name: name,
        container: None,
        kind: declaration.kind,
        syntax_element_kind: None,
        kinds: declaration.kinds,
        visibility: declaration.visibility,
        range: item_range,
        item_range,
        name_range: None,
        body_range: Some(body_range),
        signatures: std::sync::Arc::from([signatures]),
        documentation: std::sync::Arc::from([]),
        documentation_ranges: Vec::new(),
        module_path: None,
        python_overload: None,
    })
}

#[cfg(test)]
mod declaration_tests;

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_error::errors;

    use super::*;
    use crate::failure::position_overflow;

    fn text_at(text: &str, range: ByteRange) -> &str {
        let start = usize::try_from(range.start).expect("range starts within source");
        let end = usize::try_from(range.end).expect("range ends within source");
        &text[start..end]
    }

    fn path() -> ProjectPath {
        ProjectPath::new("src/lib.rs").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text,
                },
                SyntaxLimits::default(),
            )
            .expect("Rust fixture must parse")
    }

    #[test]
    fn empty_supertrait_bound_preserves_original_rust_source() {
        let path = rift_core::ProjectPath::new("src/payload.rs").expect("Rust source path");
        let source = "pub trait Café: where for<'a> &'a Self: IntoIterator { fn open(&self); }";
        let provider = RustSyntaxProvider::default();
        let parsed = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::DEFAULT,
            )
            .expect("complete authored trait");
        assert!(!parsed.has_errors());
        assert_eq!(
            parsed.facts().source_digest(),
            Some(&rift_core::FileDigest::of(source.as_bytes()))
        );
        let trait_symbol = parsed
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "Café")
            .expect("authored trait");
        assert_eq!(trait_symbol.item_range.start, 0);
        assert_eq!(
            trait_symbol.item_range.end,
            u64::try_from(source.len()).expect("source bytes")
        );
        assert!(
            parsed
                .symbols()
                .iter()
                .any(|symbol| symbol.qualified_name == "Café::open")
        );
        let query =
            RustQuery::new("(trait_item name: (type_identifier) @name)").expect("trait query");
        let captures = query.captures(source, 2).expect("one trait name capture");
        assert_eq!(captures.len(), 1);
        let name = source.find("Café").expect("written name");
        let from = u64::try_from(name).expect("name offset");
        let to = u64::try_from(name + "Café".len()).expect("name end");
        assert_eq!(
            captures[0].range,
            crate::ByteRange {
                start: from,
                end: to
            }
        );

        let malformed = "pub trait Broken: where for<'a> &'a Self: { fn open(&self); }";
        let parsed = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: malformed,
                },
                SyntaxLimits::DEFAULT,
            )
            .expect("retains a typed parse error");
        assert!(parsed.has_errors());
    }

    #[test]
    fn test_provider_extracts_nested_rust_declarations_and_byte_syntax_elements() {
        let text = "pub mod café {\r\npub(crate) struct Item;\r\nimpl Item { fn load() {} }\r\n}";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["café", "café::Item", "café::Item", "café::Item::load"]
        );
        assert_eq!(document.symbols()[0].visibility.as_deref(), Some("pub"));
        assert_eq!(
            document.symbols()[1].visibility.as_deref(),
            Some("pub(crate)")
        );
        assert_eq!(document.symbols()[3].visibility.as_deref(), Some("private"));
        assert_eq!(document.path(), &path());
        assert_eq!(document.language().name, "rust");
        let load = text.find("load").expect("fixture contains method") as u64;
        assert!(document.syntax_elements_at(load).len() >= 3);
        assert!(!document.has_errors());
    }

    #[test]
    fn test_legacy_try_macros_and_foreign_types_keep_authored_ranges() {
        let text = "unsafe extern \"C\" { pub type Opaque; fn f(); }\nfn run() { try!(f()); try { leaf(); } }";
        let document = analyze(text);
        assert!(!document.has_errors());
        let opaque = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "Opaque")
            .expect("opaque type");
        let function = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "f")
            .expect("foreign function");
        assert_eq!(opaque.kind, "extern_type");
        assert_eq!(opaque.visibility.as_deref(), Some("pub"));
        assert_eq!(function.visibility.as_deref(), Some("private"));
        assert_eq!(
            text_at(text, function.name_range.expect("function range")),
            "f"
        );
        let static_facts = document.facts().static_facts().expect("Rust static facts");
        assert_eq!(
            static_facts
                .unsupported_bindings()
                .iter()
                .map(|range| text_at(text, *range))
                .collect::<Vec<_>>(),
            ["try!(f())"]
        );
        let query =
            RustQuery::new("(try_block) @block (macro_invocation macro: (identifier) @macro)")
                .expect("Rust query");
        assert_eq!(
            query
                .captures(text, 8)
                .expect("grammar captures")
                .iter()
                .map(|capture| (capture.name.as_str(), text_at(text, capture.range)))
                .collect::<Vec<_>>(),
            [("macro", "try"), ("block", "try { leaf(); }")]
        );
        assert!(analyze("fn broken() { try!(leaf() }").has_errors());
    }

    #[test]
    fn test_module_path_absence_is_recorded_only_for_module_declarations() {
        let document = analyze("mod ordinary; pub fn run() {} mod inline {} ");
        assert_eq!(
            document.symbols()[0].module_path,
            Some(crate::RustModulePath::Absent)
        );
        assert_eq!(document.symbols()[1].module_path, None);
        assert_eq!(
            document.symbols()[2].module_path,
            Some(crate::RustModulePath::Absent)
        );
    }

    #[test]
    fn test_module_path_literal_keeps_original_content_across_blank_lines() {
        for attribute in ["#[path = \"mapped.rs\"]", "#[path = r#\"mapped.rs\"#]"] {
            let text = format!("{attribute}\n\n// ordinary comment\npub mod child;");
            let document = analyze(&text);
            let symbol = &document.symbols()[0];
            let Some(crate::RustModulePath::Literal { path, range }) = &symbol.module_path else {
                panic!("actual module path literal must be captured");
            };
            assert_eq!(path.as_str(), "mapped.rs");
            assert_eq!(text_at(&text, *range), "mapped.rs");
            assert!(range.end <= symbol.item_range.start);
            assert!(range.end <= symbol.range.start);
            assert_eq!(text_at(&text, symbol.item_range), "pub mod child;");
            assert_eq!(document.path(), &self::path());
        }
    }

    #[test]
    fn test_module_path_unsupported_or_conflicting_attributes_stay_unknown() {
        for attribute in [
            "#[path = \"../shared.rs\"]",
            "#[path = \"\"]",
            "#[path = \"/shared.rs\"]",
            "#[path = \"mapped\\u{2e}rs\"]",
            "#[path = \"one.rs\"]\n#[path = \"two.rs\"]",
            "#[path = \"same.rs\"]\n#[path = \"same.rs\"]",
            "#[custom_module]",
        ] {
            let text = format!("{attribute}\n\nmod child;");
            let document = analyze(&text);
            assert!(!document.has_errors(), "{attribute}");
            let symbol = &document.symbols()[0];
            assert_eq!(
                symbol.module_path,
                Some(crate::RustModulePath::Unknown),
                "{attribute}"
            );
            assert_eq!(text_at(&text, symbol.item_range), "mod child;");
            assert_eq!(document.path(), &self::path());
        }
    }

    #[test]
    fn test_module_path_known_builtin_attributes_do_not_create_a_path() {
        let document = analyze("#[cfg(feature = \"present\")]\n\n#[allow(dead_code)]\nmod child;");
        assert_eq!(
            document.symbols()[0].module_path,
            Some(crate::RustModulePath::Absent)
        );
    }

    #[test]
    fn test_module_path_inner_attributes_preserve_unknown_path_state() {
        for text in [
            "mod outer { #![path = \"custom\"] mod child; }",
            "mod outer { #![cfg_attr(feature = \"other\", path = \"custom\")] mod child; }",
            "mod outer { #![custom] mod child; }",
        ] {
            let document = analyze(text);
            assert!(!document.has_errors(), "{text}");
            let outer = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == "outer")
                .expect("retained inline module");
            assert_eq!(outer.module_path, Some(crate::RustModulePath::Unknown));
            assert!(outer.body_range.is_some());
            let start = usize::try_from(outer.item_range.start).expect("fixture start");
            let end = usize::try_from(outer.item_range.end).expect("fixture end");
            assert_eq!(&text[start..end], text);
            assert!(
                document
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.qualified_name == "outer::child")
            );
        }
        let document = analyze("mod outer { #![allow(dead_code)] mod child; }");
        assert_eq!(
            document.symbols()[0].module_path,
            Some(crate::RustModulePath::Absent)
        );
    }

    #[test]
    fn test_module_path_literal_refuses_owning_path_byte_bound() {
        let path = "a".repeat(rift_core::constants::PROJECT_PATH_BYTES_MAX + 1);
        let document = analyze(&format!("#[path = \"{path}\"]\nmod child;"));
        assert_eq!(
            document.symbols()[0].module_path,
            Some(crate::RustModulePath::Unknown)
        );
    }

    #[test]
    fn test_document_container_names_the_containing_scope_or_none_at_top_level() {
        let text = "pub mod café {\npub struct Item;\nimpl Item { fn load() {} }\n}";
        let document = analyze(text);
        let containers = document
            .symbols()
            .iter()
            .map(|symbol| symbol.container.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            containers,
            [None, Some("café"), Some("café"), Some("café::Item")]
        );
    }

    /// The qualified name the index holds for a method of a generic `impl`
    /// block, beside the one it holds for a method of a plain block: both
    /// qualify through the implemented type's own name, so the generic
    /// arguments the block applies never reach a method's address, and each
    /// method's container names the `struct` that declares the type.
    #[test]
    fn test_a_generic_impl_qualifies_its_methods_through_the_implemented_type() {
        let text = "pub struct Holder<Store> {\n    store: Store,\n}\n\n\
                    pub struct Plain;\n\n\
                    impl<Store: Clone> Holder<Store> {\n    pub fn run(&self) {}\n}\n\n\
                    impl Plain {\n    pub fn run(&self) {}\n}\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Holder",
                "Holder::Store#generic0",
                "Holder::store",
                "Plain",
                "Holder",
                "Holder::Store#generic1",
                "Holder::run",
                "Holder::run::self#parameter0",
                "Plain",
                "Plain::run",
                "Plain::run::self#parameter0"
            ]
        );
        let containers = document
            .symbols()
            .iter()
            .map(|symbol| symbol.container.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            containers,
            [
                None,
                Some("Holder"),
                Some("Holder"),
                None,
                None,
                Some("Holder"),
                Some("Holder"),
                Some("Holder::run"),
                None,
                Some("Plain"),
                Some("Plain::run")
            ]
        );
    }

    #[test]
    fn test_document_kind_words_cover_every_declaration_kind() {
        let text = "fn f() {}\nstruct S;\nenum E { V }\ntrait T {}\ntype A = u8;\n\
                    const C: u8 = 0;\nstatic G: u8 = 0;\nmod m {}\nmacro_rules! q { () => {}; }";
        let document = analyze(text);
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| symbol.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "function",
                "struct",
                "enum",
                "variant",
                "trait",
                "type_alias",
                "constant",
                "static",
                "module",
                "macro",
            ]
        );
    }

    /// A callable declaration with a body renders one signature whose `display` is the
    /// header text - the source from the item's own start to where the body begins, trimmed
    /// of trailing whitespace - and carries the document's language.
    #[test]
    fn test_a_function_with_a_body_renders_one_signature_from_its_header() {
        let document = analyze("pub fn compute(x: i32) -> i32 {\n    x\n}\n");
        let function = &document.symbols()[0];
        assert_eq!(function.signatures.len(), 1);
        assert_eq!(
            function.signatures[0].display,
            "pub fn compute(x: i32) -> i32"
        );
        assert_eq!(function.signatures[0].language.name, "rust");
    }

    /// A kind the grammar declares no `Callable` kind for never renders a signature, even
    /// when it carries a `body_range` - a struct's braces are not a callable implementation.
    #[test]
    fn test_a_non_callable_declaration_with_a_body_range_renders_no_signature() {
        let document = analyze("pub struct Beacon { field: u8 }\n");
        assert!(document.symbols()[0].signatures.is_empty());
    }

    /// Every function in a file renders its own signature; nothing bleeds between them.
    #[test]
    fn test_every_function_in_a_document_renders_its_own_signature() {
        let document = analyze("pub fn one() {}\npub fn two(x: u8) -> u8 { x }\n");
        let signatures: Vec<&str> = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.kinds.contains(&SymbolKind::Callable))
            .map(|symbol| symbol.signatures[0].display.as_str())
            .collect();
        assert_eq!(signatures, ["pub fn one()", "pub fn two(x: u8) -> u8"]);
    }

    /// A trait method without a body and a function in an `extern` block are
    /// declarations: each qualifies through its trait or the file, and renders
    /// its signature from its own text without the closing `;`.
    #[test]
    fn test_a_bodyless_function_declares_and_renders_its_own_text_as_signature() {
        let document = analyze(
            "pub trait Iterator {\n    type Item;\n    \
             fn next(&mut self) -> Option<Self::Item>;\n    \
             fn count(self) -> usize { 0 }\n}\n\
             unsafe extern \"C\" {\n    fn abs(x: i32) -> i32;\n}\n",
        );
        let functions = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == "function")
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.body_range.is_some(),
                    symbol.signatures[0].display.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            functions,
            [
                (
                    "Iterator::next",
                    false,
                    "fn next(&mut self) -> Option<Self::Item>"
                ),
                ("Iterator::count", true, "fn count(self) -> usize"),
                ("abs", false, "fn abs(x: i32) -> i32"),
            ]
        );
    }

    /// Each enum variant declares a `variant` qualified by its enum, with the
    /// documentation attached to it and no signature.
    #[test]
    fn test_an_enum_variant_qualifies_through_its_enum() {
        let document = analyze(
            "pub enum Option<T> {\n    /// No value.\n    None,\n    \
             #[stable]\n    Some(T),\n    Pair { left: T, right: T },\n}\n",
        );
        let variants = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == "variant")
            .collect::<Vec<_>>();
        let names = variants
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Option::None", "Option::Some", "Option::Pair"]);
        assert_eq!(variants[0].documentation[0].text, "No value.");
        assert!(variants.iter().all(|symbol| symbol.signatures.is_empty()));
        assert_eq!(variants[0].kinds, [SymbolKind::Variant, SymbolKind::Public]);
        assert_eq!(variants[0].visibility.as_deref(), Some("pub"));
    }

    /// The const-trait forms the pinned grammar lacks parse without an error
    /// once blanked, the methods inside them keep their container, and a
    /// signature still shows the authored `[const]`.
    #[test]
    fn test_const_trait_forms_parse_and_keep_their_containers() {
        let text = "pub struct Plain;\n\
                    const impl Default for Plain {\n    fn default() -> Self { Plain }\n}\n\
                    impl const Clone for Plain {\n    fn clone(&self) -> Self { Plain }\n}\n\
                    const unsafe impl Send for Plain {}\n\
                    pub const trait Measure {\n    fn size(&self) -> usize;\n}\n\
                    pub fn call<F: [const] FnOnce() -> u8>(f: F) -> u8 { f() }\n\
                    pub fn old<F: ~const FnOnce() -> u8>(f: F) -> u8 { f() }\n\
                    pub fn closure() -> u8 { let f = const || 1; f() }\n";
        let unblanked = rust_parser()
            .expect("grammar loads")
            .parse(text, None)
            .expect("a tree");
        assert!(unblanked.root_node().has_error());
        let document = analyze(text);
        assert!(!document.has_errors());
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Plain",
                "Plain",
                "Plain::default",
                "Plain",
                "Plain::clone",
                "Plain::clone::self#parameter0",
                "Plain",
                "Measure",
                "Measure::size",
                "Measure::size::self#parameter0",
                "call",
                "call::F#generic0",
                "call::f#parameter0",
                "old",
                "old::F#generic0",
                "old::f#parameter0",
                "closure",
                "closure::f",
            ]
        );
        let call = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "call")
            .expect("call declaration");
        assert_eq!(
            call.signatures[0].display,
            "pub fn call<F: [const] FnOnce() -> u8>(f: F) -> u8"
        );
    }

    #[test]
    fn attached_documentation_ranges_name_original_comment_bytes() {
        let text = "/// First line.\n/// Second line.\n#[inline]\npub fn serve() {}\n";
        let document = analyze(text);
        let ranges = &document.symbols()[0].documentation_ranges;
        assert_eq!(
            ranges
                .iter()
                .map(|range| text_at(text, *range))
                .collect::<Vec<_>>(),
            ["/// First line.\n", "/// Second line.\n"]
        );
        assert!(ranges[1].end < document.symbols()[0].item_range.start);
    }

    #[test]
    fn test_document_exported_macro_is_public_and_a_bare_macro_is_private() {
        let document = analyze(
            "#[macro_export]\nmacro_rules! shown { () => {}; }\n\
             #[macro_export(local_inner_macros)]\nmacro_rules! local { () => {}; }\n\
             #[macro_export (/* comment */ local_inner_macros, )]\n\n\
             // comment\nmacro_rules! across_comment { () => {}; }\n\
             #[cfg_attr(any(), macro_export(local_inner_macros))]\n\
             macro_rules! conditional { () => {}; }\n\
             macro_rules! hidden { () => {}; }\n",
        );
        let visibilities = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.visibility.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            visibilities,
            [
                ("shown", Some("pub")),
                ("local", Some("pub")),
                ("across_comment", Some("pub")),
                ("conditional", Some("private")),
                ("hidden", Some("private")),
            ]
        );
        assert!(
            document.symbols()[..3]
                .iter()
                .all(|symbol| symbol.kinds.contains(&SymbolKind::Public))
        );
        assert!(
            document.symbols()[3..]
                .iter()
                .all(|symbol| !symbol.kinds.contains(&SymbolKind::Public))
        );
    }

    #[test]
    fn test_document_kinds_render_kind_categories_and_public_visibility() {
        let document =
            analyze("pub fn f() {}\ntype A = u8;\nmod m {}\nmacro_rules! q { () => {}; }");
        let kinds = document
            .symbols()
            .iter()
            .map(|symbol| symbol.kinds.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                vec![SymbolKind::Value, SymbolKind::Callable, SymbolKind::Public],
                vec![SymbolKind::Type, SymbolKind::Alias],
                vec![SymbolKind::Namespace, SymbolKind::Module],
                vec![SymbolKind::Macro],
            ]
        );
    }

    /// Every declaration kind whose grammar declares a body or value field
    /// carries `body_range`; a kind or syntax element without one carries `None`.
    #[test]
    fn test_document_body_range_present_exactly_where_the_grammar_declares_one() {
        let text = "fn f() { 1; }\nstruct S { x: u8 }\nstruct Unit;\nenum E { A }\n\
                    trait T { fn t(&self); }\nmod m { fn inner() {} }\nmod declared;\n\
                    const C: u8 = 7;\nstatic G: u8 = 9;\ntype A = u8;\n\
                    macro_rules! q { () => {}; }";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.body_range.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("f", true),
                ("S", true),
                ("x", false),
                ("Unit", false),
                ("E", true),
                ("A", false),
                ("T", true),
                ("t", false),
                ("self", false),
                ("m", true),
                ("inner", true),
                ("declared", false),
                ("C", true),
                ("G", true),
                ("A", false),
                ("q", false),
            ]
        );
        let function = &document.symbols()[0];
        let body = function.body_range.expect("fn f owns a body");
        let start = usize::try_from(body.start).expect("fixture span fits usize");
        let end = usize::try_from(body.end).expect("fixture span fits usize");
        assert_eq!(&text[start..end], "{ 1; }");
    }

    #[test]
    fn test_provider_syntax_element_kinds_classify_declaration_expression_statement_and_comment() {
        let provider = RustSyntaxProvider::default();
        assert_eq!(
            provider.syntax_element_kinds("function_item"),
            [SyntaxKind::Declaration, SyntaxKind::Definition]
        );
        assert_eq!(
            provider.syntax_element_kinds("binary_expression"),
            [SyntaxKind::Expression]
        );
        assert_eq!(
            provider.syntax_element_kinds("expression_statement"),
            [SyntaxKind::Statement]
        );
        assert_eq!(
            provider.syntax_element_kinds("line_comment"),
            [SyntaxKind::Comment]
        );
        assert_eq!(
            provider.syntax_element_kinds("attribute_item"),
            [SyntaxKind::Annotation]
        );
        assert_eq!(
            provider.syntax_element_kinds("inner_attribute_item"),
            [SyntaxKind::Annotation]
        );
        assert_eq!(provider.syntax_element_kinds("identifier"), []);
    }

    #[test]
    fn test_provider_declares_language() {
        let provider = RustSyntaxProvider::default();
        assert_eq!(provider.language().name, "rust");
        assert_eq!(provider.language().dialect, None);
    }

    #[test]
    fn test_query_adapter_owns_capture_vocabulary_and_bounds() {
        let query = RustQuery::new("(function_item name: (identifier) @rift.name)")
            .expect("valid Rust query");
        assert_eq!(query.capture_names(), &["rift.name"]);
        assert!(query.has_capture("rift.name"));
        assert!(!query.has_capture("rift.missing"));
        let captures = query
            .captures("fn first() {} fn second() {}", 2)
            .expect("capture bound accepts both functions");
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].name, "rift.name");
        assert_eq!(
            query
                .captures("fn first() {} fn second() {}", 1)
                .expect_err("capture overflow must fail")
                .slug(),
            rift_error::errors::syntax::too_many_captures::SLUG
        );
        assert_eq!(
            RustQuery::new("(missing_syntax_element) @rift.name")
                .expect_err("invalid syntax element kind must fail")
                .slug(),
            rift_error::errors::syntax::invalid_query::SLUG
        );
    }

    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("fn broken( {");
        assert!(document.has_errors());
        assert!(!document.syntax_elements().is_empty());
    }

    #[test]
    fn test_provider_enforces_source_syntax_element_depth_and_positive_limits() {
        assert_eq!(
            SyntaxLimits::new(0, 1, 1).expect_err("zero limit").slug(),
            rift_error::errors::syntax::zero_limit::SLUG,
        );
        let source_error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                },
                SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            )
            .expect_err("source bound");
        assert_eq!(
            source_error.slug(),
            rift_error::errors::syntax::source_too_large::SLUG
        );

        let syntax_element_error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                },
                SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            )
            .expect_err("syntax element bound");
        assert_eq!(
            syntax_element_error.slug(),
            rift_error::errors::syntax::too_many_syntax_elements::SLUG
        );

        let depth_error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                },
                SyntaxLimits::new(100, 20, 1).expect("positive limits"),
            )
            .expect_err("depth bound");
        assert_eq!(
            depth_error.slug(),
            rift_error::errors::syntax::too_deep::SLUG
        );
    }

    #[test]
    fn test_grammar_syntax_element_kind_round_trips_and_rejects_unknown() {
        for kind in RustGrammarSyntaxKind::VARIANTS.iter().copied() {
            assert_eq!(
                kind.as_str()
                    .parse::<RustGrammarSyntaxKind>()
                    .expect("known kind"),
                kind
            );
        }
        let error = "flumph_item"
            .parse::<RustGrammarSyntaxKind>()
            .expect_err("unknown kind");
        assert_eq!(
            error.slug(),
            errors::syntax::unknown_syntax_element_kind::SLUG
        );
        assert_eq!(
            error.context().collect::<Vec<_>>(),
            vec![("syntax_element_kind", "flumph_item".to_owned())]
        );
    }

    #[test]
    fn test_visibility_from_authored_distinguishes_public_and_restricted() {
        assert_eq!(RustVisibility::from_authored("pub"), RustVisibility::Public);
        assert_eq!(
            RustVisibility::from_authored("pub(super)"),
            RustVisibility::Restricted("pub(super)".into())
        );
    }

    #[test]
    fn test_zero_limit_error_names_offending_bound_and_configuration_site() {
        let cases = [
            (SyntaxLimits::new(0, 1, 1), "source_bytes_max"),
            (SyntaxLimits::new(1, 0, 1), "syntax_elements_max"),
            (SyntaxLimits::new(1, 1, 0), "syntax_depth_max"),
        ];
        for (result, bound) in cases {
            let error = result.expect_err("zero bound");
            assert_eq!(error.slug(), errors::syntax::zero_limit::SLUG);
            assert_eq!(
                error.context().collect::<Vec<_>>(),
                vec![("bound", bound.to_owned())]
            );
        }
        let query = RustQuery::new("(function_item) @rift.item").expect("valid Rust query");
        let error = query
            .captures("fn a() {}", 0)
            .expect_err("zero captures_max");
        assert_eq!(error.slug(), errors::syntax::zero_limit::SLUG);
        assert_eq!(
            error.context().collect::<Vec<_>>(),
            vec![("bound", "captures_max".to_owned())]
        );
    }

    #[test]
    fn test_source_too_large_error_reports_sizes_path_and_limit_origin() {
        let error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                },
                SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            )
            .expect_err("source bound");
        assert_eq!(error.slug(), errors::syntax::source_too_large::SLUG);
        assert_eq!(
            error.context().collect::<Vec<_>>(),
            vec![
                ("path", "src/lib.rs".to_owned()),
                ("source_bytes", "9".to_owned()),
                ("source_bytes_max", "3".to_owned()),
            ]
        );
        let query = RustQuery::new("(function_item) @rift.item").expect("valid Rust query");
        let oversized = "a".repeat(SOURCE_BYTES_MAX_DEFAULT + 1);
        let error = query.captures(&oversized, 1).expect_err("oversized source");
        assert_eq!(error.slug(), errors::syntax::source_too_large::SLUG);
        assert_eq!(
            error.context().collect::<Vec<_>>(),
            vec![
                ("source_bytes", (SOURCE_BYTES_MAX_DEFAULT + 1).to_string()),
                ("source_bytes_max", SOURCE_BYTES_MAX_DEFAULT.to_string()),
            ]
        );
    }

    #[test]
    fn test_tree_bound_errors_report_path_and_configured_limit() {
        let syntax_element_error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() {}",
                },
                SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            )
            .expect_err("syntax element bound");
        assert_eq!(
            syntax_element_error.slug(),
            errors::syntax::too_many_syntax_elements::SLUG
        );
        assert!(
            syntax_element_error
                .context()
                .any(|(key, value)| key == "path" && value == "src/lib.rs")
        );
        assert!(
            syntax_element_error
                .context()
                .any(|(key, value)| key == "syntax_elements_max" && value == "1")
        );
        assert!(syntax_element_error.to_string().contains("src/lib.rs"));

        let depth_error = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path(),
                    text: "fn x() { { 1 } }",
                },
                SyntaxLimits::new(100, 20, 1).expect("positive limits"),
            )
            .expect_err("depth bound");
        assert_eq!(depth_error.slug(), errors::syntax::too_deep::SLUG);
        assert!(
            depth_error
                .context()
                .any(|(key, value)| key == "path" && value == "src/lib.rs")
        );
        assert!(
            depth_error
                .context()
                .any(|(key, value)| key == "syntax_depth_max" && value == "1")
        );
        assert!(depth_error.to_string().contains("src/lib.rs"));
    }

    #[test]
    fn test_query_errors_report_line_reason_and_capture_limit() {
        let error = RustQuery::new("(missing_syntax_element) @rift.name")
            .expect_err("invalid syntax element kind");
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(error.slug(), errors::syntax::invalid_query::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "line_number" && value == "1")
        );
        let query = RustQuery::new("(function_item name: (identifier) @rift.name)")
            .expect("valid Rust query");
        let error = query
            .captures("fn first() {} fn second() {}", 1)
            .expect_err("capture overflow");
        assert_eq!(error.slug(), errors::syntax::too_many_captures::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "captures_max" && value == "1")
        );
    }

    #[test]
    fn test_invalid_query_reason_covers_every_tree_sitter_error_kind() {
        use tree_sitter::QueryErrorKind;
        let cases = [
            ("(((", QueryErrorKind::Syntax),
            (
                "(function_item flumph: (identifier) @x)",
                QueryErrorKind::Field,
            ),
            ("((function_item) @x (#eq? @y @y))", QueryErrorKind::Capture),
            ("((function_item) @x (#eq?))", QueryErrorKind::Predicate),
            ("(identifier (identifier))", QueryErrorKind::Structure),
        ];
        for (query, kind) in cases {
            let error = RustQuery::new(query).expect_err("query must be rejected");
            assert_eq!(error.slug(), errors::syntax::invalid_query::SLUG);
            assert_eq!(
                std::error::Error::source(&error)
                    .expect("invalid query carries tree-sitter source")
                    .downcast_ref::<tree_sitter::QueryError>()
                    .expect("query source is tree-sitter QueryError")
                    .kind,
                kind,
                "classifies {query}"
            );
        }
    }

    #[test]
    fn test_runtime_only_errors_render_full_context() {
        let language = rust_grammar();
        let grammar = errors::syntax::incompatible_grammar()
            .grammar_abi_version(language.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error();
        assert_eq!(grammar.slug(), errors::syntax::incompatible_grammar::SLUG);
        assert!(
            grammar
                .context()
                .any(|(key, _)| key == "grammar_abi_version")
        );
        let cancelled = errors::syntax::parse_cancelled().path(path()).error();
        assert_eq!(cancelled.slug(), errors::syntax::parse_cancelled::SLUG);
        assert_eq!(
            cancelled.context().collect::<Vec<_>>(),
            vec![("path", "src/lib.rs".to_owned())]
        );
        let cancelled_query = errors::syntax::parse_cancelled().error();
        assert_eq!(
            cancelled_query.slug(),
            errors::syntax::parse_cancelled::SLUG
        );
        assert_eq!(cancelled_query.context().count(), 0);
        let mut parser = rust_parser().expect("pinned grammar loads");
        let tree = parser.parse("fn x() {}", None).expect("fixture parses");
        let overflow = position_overflow(
            tree.root_node(),
            u32::try_from(u64::MAX).expect_err("u64::MAX exceeds u32"),
        );
        assert_eq!(overflow.slug(), errors::syntax::position_overflow::SLUG);
        assert!(std::error::Error::source(&overflow).is_some());
        assert!(
            overflow
                .context()
                .any(|(key, _)| key == "syntax_element_kind")
        );
    }

    #[test]
    fn test_unknown_syntax_element_kind_and_zero_limit_report_code_and_context() {
        let unknown = "bogus"
            .parse::<RustGrammarSyntaxKind>()
            .expect_err("unregistered kind");
        assert_eq!(
            unknown.slug(),
            errors::syntax::unknown_syntax_element_kind::SLUG
        );
        assert_eq!(
            unknown.context().collect::<Vec<_>>(),
            vec![("syntax_element_kind", "bogus".to_owned())]
        );
        let zero = SyntaxLimits::new(0, 1, 1).expect_err("zero source bound");
        assert_eq!(zero.slug(), errors::syntax::zero_limit::SLUG);
        assert_eq!(
            zero.context().collect::<Vec<_>>(),
            vec![("bound", "source_bytes_max".to_owned())]
        );
    }

    #[test]
    fn test_file_scope_main_carries_the_entrypoint_kind() {
        let document = analyze("fn main() {}\n");
        let main = &document.symbols()[0];
        assert_eq!(main.name, "main");
        assert!(main.kinds.contains(&SymbolKind::Entrypoint));
    }

    #[test]
    fn test_main_nested_in_a_module_does_not_carry_the_entrypoint_kind() {
        let document = analyze("mod tests {\n    fn main() {}\n}\n");
        let main = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "main");
        let main = main.expect("fixture declares a nested main");
        assert!(!main.kinds.contains(&SymbolKind::Entrypoint));
    }

    #[test]
    fn test_a_method_named_main_does_not_carry_the_entrypoint_kind() {
        let document = analyze("struct S;\nimpl S {\n    fn main(&self) {}\n}\n");
        let main = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "main");
        let main = main.expect("fixture declares a method named main");
        assert!(!main.kinds.contains(&SymbolKind::Entrypoint));
    }

    #[test]
    fn test_a_trait_default_main_does_not_carry_the_entrypoint_kind() {
        let document = analyze("trait Runs {\n    fn main() {}\n}\n");
        let main = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "main");
        let main = main.expect("fixture declares a trait default method named main");
        assert!(!main.kinds.contains(&SymbolKind::Entrypoint));
    }

    #[test]
    fn test_documentation_cfg_attributes_keep_original_module_path_facts() {
        for (prefix, body) in [
            (
                "#[cfg_attr(docsrs, doc(cfg(feature = \"rt\")))]\n",
                "pub mod task;",
            ),
            ("#[cfg_attr(docsrs, doc(hidden))]\r\n", "mod child;"),
            (
                "",
                "mod inline { #![cfg_attr(docsrs, doc(hidden))] mod child; }",
            ),
        ] {
            let text = format!("{prefix}{body}");
            let document = analyze(&text);
            assert!(!document.has_errors());
            let symbol = &document.symbols()[0];
            assert_eq!(symbol.module_path, Some(crate::RustModulePath::Absent));
            assert_eq!(text_at(&text, symbol.item_range), body);
        }
        let text = "#[path = \"mapped.rs\"]\n#[cfg_attr(docsrs, doc(hidden))]\nmod child;";
        let document = analyze(text);
        let Some(crate::RustModulePath::Literal { path, range }) =
            &document.symbols()[0].module_path
        else {
            panic!("original literal path survives documentation attribute");
        };
        assert_eq!(path.as_str(), "mapped.rs");
        assert_eq!(text_at(text, *range), "mapped.rs");
    }

    #[test]
    fn test_documentation_cfg_attributes_keep_conditional_path_evidence_separate() {
        for attribute in [
            "cfg_attr(docsrs, doc(hidden), path = \"other.rs\")",
            "cfg_attr(docsrs, doc(hidden), macro_use)",
            "cfg_attr(docsrs, doc(hidden), custom)",
            "cfg_attr(docsrs, path = \"other.rs\")",
            "cfg_attr(feature = \"rt\", doc(hidden))",
            "cfg_attr(docsrs, doc(include_str!(\"other.rs\")))",
            "cfg_attr(docsrs, doc[hidden])",
            "cfg_attr(docsrs, doc(hidden);)",
            "cfg_attr(docsrs, doc(hidden),)",
        ] {
            for text in [
                format!("#[{attribute}]\nmod child;"),
                format!("mod inline {{ #![{attribute}] mod child; }}"),
            ] {
                let document = analyze(&text);
                let state = &document.symbols()[0].module_path;
                if text.starts_with("#[") && attribute.contains("path =") {
                    let Some(crate::RustModulePath::Conditional { attributes }) = state else {
                        panic!("expected conditional path metadata: {text}");
                    };
                    assert_eq!(attributes.len(), 1, "{text}");
                    assert_eq!(text_at(&text, attributes[0]), format!("#[{attribute}]"));
                } else {
                    assert_eq!(state, &Some(crate::RustModulePath::Unknown), "{text}");
                }
            }
        }
        let attribute = format!(
            "#[cfg_attr(docsrs, doc({}))]",
            "hidden,".repeat(rift_core::constants::PROJECT_PATH_BYTES_MAX)
        );
        let document = analyze(&format!("{attribute}\nmod child;"));
        assert_eq!(
            document.symbols()[0].module_path,
            Some(crate::RustModulePath::Unknown)
        );
    }
}
