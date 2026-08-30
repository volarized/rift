//! Rust name-binding facts extracted from one parsed tree.
//!
//! One walk over the tree the syntax pass already parsed emits scopes, definitions,
//! references, imports, member links, and module declarations into
//! [`UnitBindingFacts`]. A malformed subtree contributes nothing and the rest of the
//! unit keeps its facts; an exceeded bound drops the whole unit's facts, so the
//! document keeps no binding and `analyze` still succeeds.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rift_binding::{
    BindingLimits, DefinitionOrder, IMPORT_EXPLICIT_RANK, IMPORT_WILDCARD_RANK,
    NAME_PATH_SEGMENTS_MAX, Name, NamePath, PathAnchor, Rank, ScopeKind, UnitBindingFacts,
    UnitBindingFactsBuilder, UnitDefinition, UnitDefinitionIndex, UnitImport, UnitMemberLink,
    UnitModuleDeclaration, UnitReference, UnitScopeIndex, VisibilitySpelling,
};
use rift_core::{ExactKind, LoopBudget, ReferenceRole, SourceRange};
use rift_protocol::read::SymbolFacet;
use tree_sitter::Node;

use super::{RustSymbolKind, RustVisibility, declaration_facets, declaration_visibility};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Wire prefix every Rust binding kind composes with its kind word.
const RUST_KIND_PREFIX: &str = "rust";
/// Kind word for a `union_item` definition.
const UNION_KIND_WORD: &str = "union";
/// Kind word for an `enum_variant` definition.
const VARIANT_KIND_WORD: &str = "variant";
/// Kind word for a function or closure parameter binding.
const PARAMETER_KIND_WORD: &str = "parameter";
/// Kind word for a `let`, loop, arm, or condition pattern binding.
const LOCAL_KIND_WORD: &str = "local";
/// Kind word for a generic parameter definition.
const TYPE_PARAMETER_KIND_WORD: &str = "type_parameter";
/// Facets a pattern binding carries.
const LOCAL_FACETS: &[SymbolFacet] = &[SymbolFacet::Value];
/// Facets a parameter binding carries.
const PARAMETER_FACETS: &[SymbolFacet] = &[SymbolFacet::Value, SymbolFacet::Parameter];
/// File names whose `mod` declarations resolve beside the file itself.
const DIRECTORY_OWNING_FILE_NAMES: [&str; 3] = ["lib.rs", "main.rs", "mod.rs"];
/// Extension a module candidate file carries.
const RUST_FILE_SUFFIX: &str = ".rs";
/// Most chain nodes one path extraction walks; a longer chain cannot form a valid path.
const PATH_NODES_MAX: usize = NAME_PATH_SEGMENTS_MAX * 2;

/// Signal that the unit's facts are dropped: a bound tripped or the fact store refused.
struct UnitDropped;

/// What the walk does with one grammar node kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRole {
    /// An item declaration the syntax provider also extracts.
    Item(RustSymbolKind),
    /// A `union_item` declaration.
    Union,
    /// A body-less trait method signature.
    FunctionSignature,
    /// An associated type inside a trait body.
    AssociatedType,
    /// An `enum_variant` inside an enum body.
    EnumVariant,
    /// An `impl_item` block.
    Impl,
    /// A `block` opening a block scope.
    Block,
    /// A `closure_expression` opening a block scope over its parameters.
    Closure,
    /// A `match_expression` whose arms open block scopes.
    Match,
    /// A `for_expression` binding its pattern in a block scope.
    For,
    /// A `while_expression` binding a `let` condition in a block scope.
    While,
    /// An `if_expression`, scoped only when its condition binds a pattern.
    If,
    /// A bare `let_condition` outside `if` and `while`.
    LetCondition,
    /// A bare `let_chain` outside `if` and `while`.
    LetChain,
    /// A `let_declaration` binding its pattern sequentially.
    LetDeclaration,
    /// A `use_declaration` flattened into import links.
    Use,
    /// An `identifier` read in expression position.
    Read,
    /// A `type_identifier` naming a type.
    TypeName,
    /// A `scoped_identifier` path in expression position.
    ScopedPath,
    /// A `scoped_type_identifier` path naming a type.
    ScopedTypePath,
    /// A `generic_type` whose base names a type.
    GenericType,
    /// A `generic_function` such as a turbofish callee.
    GenericFunction,
    /// A `call_expression` marking its callee a call.
    Call,
    /// A `macro_invocation` marking its macro name a call.
    MacroInvocation,
    /// An `assignment_expression` marking a plain left identifier a write.
    Assignment,
    /// A `struct_expression` marking its name a type.
    StructExpression,
    /// A kind the walk never descends into.
    Skip,
}

/// Grammar node-kind ids this module reads, resolved once from the pinned grammar.
#[derive(Debug, Clone, Copy)]
struct GrammarKinds {
    function_item: u16,
    struct_item: u16,
    enum_item: u16,
    union_item: u16,
    trait_item: u16,
    type_item: u16,
    const_item: u16,
    static_item: u16,
    mod_item: u16,
    macro_definition: u16,
    function_signature_item: u16,
    associated_type: u16,
    enum_variant: u16,
    impl_item: u16,
    block: u16,
    closure_expression: u16,
    match_expression: u16,
    match_arm: u16,
    for_expression: u16,
    while_expression: u16,
    if_expression: u16,
    let_condition: u16,
    let_chain: u16,
    let_declaration: u16,
    use_declaration: u16,
    use_as_clause: u16,
    use_list: u16,
    scoped_use_list: u16,
    use_wildcard: u16,
    identifier: u16,
    type_identifier: u16,
    scoped_identifier: u16,
    scoped_type_identifier: u16,
    generic_type: u16,
    generic_function: u16,
    call_expression: u16,
    macro_invocation: u16,
    assignment_expression: u16,
    struct_expression: u16,
    visibility_modifier: u16,
    crate_keyword: u16,
    self_keyword: u16,
    super_keyword: u16,
    tuple_struct_pattern: u16,
    struct_pattern: u16,
    token_tree: u16,
    attribute_item: u16,
    inner_attribute_item: u16,
    type_parameter: u16,
    const_parameter: u16,
    lifetime_parameter: u16,
    lifetime: u16,
    parameter: u16,
    self_parameter: u16,
    variadic_parameter: u16,
    metavariable: u16,
}

impl GrammarKinds {
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            function_item: kind_id(language, "function_item"),
            struct_item: kind_id(language, "struct_item"),
            enum_item: kind_id(language, "enum_item"),
            union_item: kind_id(language, "union_item"),
            trait_item: kind_id(language, "trait_item"),
            type_item: kind_id(language, "type_item"),
            const_item: kind_id(language, "const_item"),
            static_item: kind_id(language, "static_item"),
            mod_item: kind_id(language, "mod_item"),
            macro_definition: kind_id(language, "macro_definition"),
            function_signature_item: kind_id(language, "function_signature_item"),
            associated_type: kind_id(language, "associated_type"),
            enum_variant: kind_id(language, "enum_variant"),
            impl_item: kind_id(language, "impl_item"),
            block: kind_id(language, "block"),
            closure_expression: kind_id(language, "closure_expression"),
            match_expression: kind_id(language, "match_expression"),
            match_arm: kind_id(language, "match_arm"),
            for_expression: kind_id(language, "for_expression"),
            while_expression: kind_id(language, "while_expression"),
            if_expression: kind_id(language, "if_expression"),
            let_condition: kind_id(language, "let_condition"),
            let_chain: kind_id(language, "let_chain"),
            let_declaration: kind_id(language, "let_declaration"),
            use_declaration: kind_id(language, "use_declaration"),
            use_as_clause: kind_id(language, "use_as_clause"),
            use_list: kind_id(language, "use_list"),
            scoped_use_list: kind_id(language, "scoped_use_list"),
            use_wildcard: kind_id(language, "use_wildcard"),
            identifier: kind_id(language, "identifier"),
            type_identifier: kind_id(language, "type_identifier"),
            scoped_identifier: kind_id(language, "scoped_identifier"),
            scoped_type_identifier: kind_id(language, "scoped_type_identifier"),
            generic_type: kind_id(language, "generic_type"),
            generic_function: kind_id(language, "generic_function"),
            call_expression: kind_id(language, "call_expression"),
            macro_invocation: kind_id(language, "macro_invocation"),
            assignment_expression: kind_id(language, "assignment_expression"),
            struct_expression: kind_id(language, "struct_expression"),
            visibility_modifier: kind_id(language, "visibility_modifier"),
            crate_keyword: kind_id(language, "crate"),
            self_keyword: kind_id(language, "self"),
            super_keyword: kind_id(language, "super"),
            tuple_struct_pattern: kind_id(language, "tuple_struct_pattern"),
            struct_pattern: kind_id(language, "struct_pattern"),
            token_tree: kind_id(language, "token_tree"),
            attribute_item: kind_id(language, "attribute_item"),
            inner_attribute_item: kind_id(language, "inner_attribute_item"),
            type_parameter: kind_id(language, "type_parameter"),
            const_parameter: kind_id(language, "const_parameter"),
            lifetime_parameter: kind_id(language, "lifetime_parameter"),
            lifetime: kind_id(language, "lifetime"),
            parameter: kind_id(language, "parameter"),
            self_parameter: kind_id(language, "self_parameter"),
            variadic_parameter: kind_id(language, "variadic_parameter"),
            metavariable: kind_id(language, "metavariable"),
        }
    }

    /// The walk's decision for every dispatched kind id.
    fn roles(&self) -> BTreeMap<u16, NodeRole> {
        BTreeMap::from([
            (self.function_item, NodeRole::Item(RustSymbolKind::Function)),
            (self.struct_item, NodeRole::Item(RustSymbolKind::Struct)),
            (self.enum_item, NodeRole::Item(RustSymbolKind::Enum)),
            (self.trait_item, NodeRole::Item(RustSymbolKind::Trait)),
            (self.type_item, NodeRole::Item(RustSymbolKind::TypeAlias)),
            (self.const_item, NodeRole::Item(RustSymbolKind::Constant)),
            (self.static_item, NodeRole::Item(RustSymbolKind::Static)),
            (self.mod_item, NodeRole::Item(RustSymbolKind::Module)),
            (self.macro_definition, NodeRole::Item(RustSymbolKind::Macro)),
            (self.union_item, NodeRole::Union),
            (self.function_signature_item, NodeRole::FunctionSignature),
            (self.associated_type, NodeRole::AssociatedType),
            (self.enum_variant, NodeRole::EnumVariant),
            (self.impl_item, NodeRole::Impl),
            (self.block, NodeRole::Block),
            (self.closure_expression, NodeRole::Closure),
            (self.match_expression, NodeRole::Match),
            (self.for_expression, NodeRole::For),
            (self.while_expression, NodeRole::While),
            (self.if_expression, NodeRole::If),
            (self.let_condition, NodeRole::LetCondition),
            (self.let_chain, NodeRole::LetChain),
            (self.let_declaration, NodeRole::LetDeclaration),
            (self.use_declaration, NodeRole::Use),
            (self.identifier, NodeRole::Read),
            (self.type_identifier, NodeRole::TypeName),
            (self.scoped_identifier, NodeRole::ScopedPath),
            (self.scoped_type_identifier, NodeRole::ScopedTypePath),
            (self.generic_type, NodeRole::GenericType),
            (self.generic_function, NodeRole::GenericFunction),
            (self.call_expression, NodeRole::Call),
            (self.macro_invocation, NodeRole::MacroInvocation),
            (self.assignment_expression, NodeRole::Assignment),
            (self.struct_expression, NodeRole::StructExpression),
            (self.visibility_modifier, NodeRole::Skip),
            (self.crate_keyword, NodeRole::Skip),
            (self.self_keyword, NodeRole::Skip),
            (self.super_keyword, NodeRole::Skip),
            (self.tuple_struct_pattern, NodeRole::Skip),
            (self.struct_pattern, NodeRole::Skip),
            (self.token_tree, NodeRole::Skip),
            (self.attribute_item, NodeRole::Skip),
            (self.inner_attribute_item, NodeRole::Skip),
            (self.lifetime, NodeRole::Skip),
            (self.self_parameter, NodeRole::Skip),
            (self.variadic_parameter, NodeRole::Skip),
            (self.metavariable, NodeRole::Skip),
        ])
    }
}

/// Grammar field ids this module reads, resolved once from the pinned grammar.
#[derive(Debug, Clone, Copy)]
struct GrammarFields {
    name: u16,
    body: u16,
    parameters: u16,
    type_parameters: u16,
    pattern: u16,
    value: u16,
    type_field: u16,
    condition: u16,
    consequence: u16,
    alternative: u16,
    path: u16,
    alias: u16,
    list: u16,
    argument: u16,
    function: u16,
    macro_field: u16,
    left: u16,
    trait_field: u16,
}

impl GrammarFields {
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            name: field_id(language, "name"),
            body: field_id(language, "body"),
            parameters: field_id(language, "parameters"),
            type_parameters: field_id(language, "type_parameters"),
            pattern: field_id(language, "pattern"),
            value: field_id(language, "value"),
            type_field: field_id(language, "type"),
            condition: field_id(language, "condition"),
            consequence: field_id(language, "consequence"),
            alternative: field_id(language, "alternative"),
            path: field_id(language, "path"),
            alias: field_id(language, "alias"),
            list: field_id(language, "list"),
            argument: field_id(language, "argument"),
            function: field_id(language, "function"),
            macro_field: field_id(language, "macro"),
            left: field_id(language, "left"),
            trait_field: field_id(language, "trait"),
        }
    }
}

/// Grammar ids this module reads, with the walk's per-kind decisions.
struct BindingGrammar {
    kinds: GrammarKinds,
    fields: GrammarFields,
    roles: BTreeMap<u16, NodeRole>,
}

impl BindingGrammar {
    fn resolve() -> Self {
        let language = super::rust_grammar();
        let kinds = GrammarKinds::resolve(&language);
        let fields = GrammarFields::resolve(&language);
        let roles = kinds.roles();
        Self {
            kinds,
            fields,
            roles,
        }
    }

    fn role(&self, kind_id: u16) -> Option<NodeRole> {
        self.roles.get(&kind_id).copied()
    }
}

/// Returns the process-wide resolved [`BindingGrammar`], computing it once.
fn binding_grammar() -> &'static BindingGrammar {
    static GRAMMAR: OnceLock<BindingGrammar> = OnceLock::new();
    GRAMMAR.get_or_init(BindingGrammar::resolve)
}

fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned Rust grammar must define node kind: kind={kind}"
    );
    id
}

fn field_id(language: &tree_sitter::Language, field: &str) -> u16 {
    let id = language.field_id_for_name(field);
    assert!(
        id.is_some(),
        "pinned Rust grammar must define field: field={field}"
    );
    id.map_or(0, std::num::NonZeroU16::get)
}

/// Extracts one unit's binding facts from an already-parsed tree.
///
/// A malformed subtree contributes nothing and the rest of the unit keeps its facts. An
/// exceeded bound - the walk's node budget, mirroring `syntax_nodes_max`, or any
/// [`BindingLimits`] unit bound - drops the whole unit's facts and answers `None`, so
/// `analyze` never fails on binding extraction. An empty source holds no scope and also
/// answers `None`.
pub(super) fn unit_binding_facts<'a>(
    root: Node<'a>,
    source: SyntaxSource<'a>,
    limits: SyntaxLimits,
) -> Option<UnitBindingFacts> {
    let grammar = binding_grammar();
    let mut builder = UnitBindingFacts::builder(BindingLimits::default());
    let range = node_range(root)?;
    let unit_scope = builder.scope(ScopeKind::Module, range, None).ok()?;
    let mut walker = Walker {
        text: source.text,
        unit_path: source.path.as_str(),
        grammar,
        builder,
        budget: LoopBudget::new(limits.syntax_nodes_max()),
        pending: Vec::new(),
    };
    walker.push_children(root, unit_scope);
    walker.drain().ok()?;
    Some(walker.builder.build())
}

/// One extracted path: where its first segment is looked up, and its segments.
struct ExtractedPath {
    anchor: PathAnchor,
    path: NamePath,
}

/// Walks the tree with an explicit stack, emitting facts into the unit builder.
struct Walker<'a> {
    text: &'a str,
    unit_path: &'a str,
    grammar: &'static BindingGrammar,
    builder: UnitBindingFactsBuilder,
    budget: LoopBudget,
    pending: Vec<(Node<'a>, UnitScopeIndex)>,
}

impl<'a> Walker<'a> {
    /// Drains the stack; every visited or sub-walked node is charged to the budget.
    fn drain(&mut self) -> Result<(), UnitDropped> {
        while let Some((node, scope)) = self.pending.pop() {
            self.charge()?;
            self.visit(node, scope)?;
        }
        Ok(())
    }

    fn charge(&mut self) -> Result<(), UnitDropped> {
        self.budget.consume().map_err(|_| UnitDropped)
    }

    fn visit(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let Some(role) = self.grammar.role(node.kind_id()) else {
            self.push_children(node, scope);
            return Ok(());
        };
        match role {
            NodeRole::Item(kind) => self.item(node, scope, kind),
            NodeRole::Union => self.member_bodied(node, scope, None),
            NodeRole::FunctionSignature => self.function(node, scope, false),
            NodeRole::AssociatedType => {
                self.plain_item(node, scope, RustSymbolKind::TypeAlias, false)
            }
            NodeRole::EnumVariant => self.variant(node, scope),
            NodeRole::Impl => self.implementation(node, scope),
            NodeRole::Block => self.block(node, scope),
            NodeRole::Closure => self.closure(node, scope),
            NodeRole::Match => self.match_body(node, scope),
            NodeRole::For => self.for_loop(node, scope),
            NodeRole::While => self.while_loop(node, scope),
            NodeRole::If => self.if_branch(node, scope),
            NodeRole::LetCondition => self.let_condition(node, scope, scope),
            NodeRole::LetChain => self.condition(node, scope, scope),
            NodeRole::LetDeclaration => self.local_declaration(node, scope),
            NodeRole::Use => self.use_tree(node, scope),
            NodeRole::Read | NodeRole::ScopedPath => {
                self.reference(node, scope, ReferenceRole::Read)
            }
            NodeRole::TypeName | NodeRole::ScopedTypePath => {
                self.reference(node, scope, ReferenceRole::Type)
            }
            NodeRole::GenericType => self.generic_type(node, scope),
            NodeRole::GenericFunction => self.generic_function(node, scope, ReferenceRole::Read),
            NodeRole::Call => self.call(node, scope),
            NodeRole::MacroInvocation => self.macro_call(node, scope),
            NodeRole::Assignment => self.assignment(node, scope),
            NodeRole::StructExpression => self.struct_literal(node, scope),
            NodeRole::Skip => Ok(()),
        }
    }

    fn item(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        kind: RustSymbolKind,
    ) -> Result<(), UnitDropped> {
        match kind {
            RustSymbolKind::Function => self.function(node, scope, true),
            RustSymbolKind::Struct | RustSymbolKind::Enum | RustSymbolKind::Trait => {
                self.member_bodied(node, scope, Some(kind))
            }
            RustSymbolKind::TypeAlias | RustSymbolKind::Constant | RustSymbolKind::Static => {
                self.plain_item(node, scope, kind, true)
            }
            RustSymbolKind::Module => self.module(node, scope),
            RustSymbolKind::Macro => self.macro_item(node, scope),
        }
    }

    /// One callable declaration: a definition, then a scope for its facts.
    ///
    /// The scope holds the callable's generics, parameters, and body.
    fn function(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        is_item: bool,
    ) -> Result<(), UnitDropped> {
        let definition = self.item_definition(
            node,
            scope,
            RustSymbolKind::Function,
            DefinitionOrder::Item,
            is_item,
        );
        if let Some(definition) = definition {
            self.emit_definition(definition)?;
        }
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let function_scope = self.scope(ScopeKind::Block, range, scope)?;
        self.type_parameter_list(node, function_scope)?;
        self.parameter_list(node, function_scope)?;
        let fields = self.grammar.fields;
        let handled = [fields.name, fields.parameters, fields.type_parameters];
        self.push_unhandled_children(node, function_scope, &handled);
        Ok(())
    }

    /// A struct, enum, trait, or union: definition plus a member scope over its body.
    fn member_bodied(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        kind: Option<RustSymbolKind>,
    ) -> Result<(), UnitDropped> {
        let fields = self.grammar.fields;
        let body = node.child_by_field_id(fields.body);
        let member = match body.and_then(node_range) {
            Some(range) => Some(self.scope(ScopeKind::Member, range, scope)?),
            None => None,
        };
        let definition = match kind {
            Some(kind) => self.item_definition(node, scope, kind, DefinitionOrder::Item, true),
            None => self.union_definition(node, scope),
        };
        if let Some(mut definition) = definition {
            if let Some(member) = member {
                definition = definition.declaring(member);
            }
            self.emit_definition(definition)?;
        }
        if let (Some(body), Some(member)) = (body, member) {
            self.pending.push((body, member));
        }
        let handled = [fields.name, fields.body, fields.type_parameters];
        self.push_unhandled_children(node, scope, &handled);
        Ok(())
    }

    /// A type alias, constant, static, or associated type: definition plus descent.
    fn plain_item(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        kind: RustSymbolKind,
        is_item: bool,
    ) -> Result<(), UnitDropped> {
        let definition = self.item_definition(node, scope, kind, DefinitionOrder::Item, is_item);
        if let Some(definition) = definition {
            self.emit_definition(definition)?;
        }
        self.push_unhandled_children(node, scope, &[self.grammar.fields.name]);
        Ok(())
    }

    fn module(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        match node.child_by_field_id(self.grammar.fields.body) {
            Some(body) => self.inline_module(node, body, scope),
            None => self.declared_module(node, scope),
        }
    }

    /// An inline `mod name { .. }`: the definition declares its body's module scope.
    fn inline_module(
        &mut self,
        node: Node<'a>,
        body: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let Some(range) = node_range(body) else {
            return Ok(());
        };
        let module_scope = self.scope(ScopeKind::Module, range, scope)?;
        let definition = self.item_definition(
            node,
            scope,
            RustSymbolKind::Module,
            DefinitionOrder::Item,
            true,
        );
        if let Some(definition) = definition {
            self.emit_definition(definition.declaring(module_scope))?;
        }
        self.pending.push((body, module_scope));
        Ok(())
    }

    /// A body-less `mod name;`: the definition gains candidate unit paths.
    fn declared_module(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let definition = self.item_definition(
            node,
            scope,
            RustSymbolKind::Module,
            DefinitionOrder::Item,
            true,
        );
        let Some(definition) = definition else {
            return Ok(());
        };
        let module_name = definition.name().as_str().to_owned();
        let index = self.emit_definition(definition)?;
        let candidates = module_candidates(self.unit_path, &module_name);
        self.builder
            .module_declaration(UnitModuleDeclaration::new(index, candidates))
            .map_err(|_| UnitDropped)
    }

    /// A `macro_rules!` definition: sequential, visible only after its own end.
    fn macro_item(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let order = DefinitionOrder::Sequential(range.end());
        let definition = self.item_definition(node, scope, RustSymbolKind::Macro, order, true);
        if let Some(definition) = definition {
            self.emit_definition(definition)?;
        }
        Ok(())
    }

    /// An enum variant: public, since reaching it already passed the enum's visibility.
    fn variant(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let definition = self.field_named(node).and_then(|name| {
            let range = node_range(node)?;
            let definition = UnitDefinition::new(
                scope,
                name,
                range,
                exact_kind(VARIANT_KIND_WORD),
                DefinitionOrder::Item,
                VisibilitySpelling::Public,
            );
            Some(definition.with_facets(LOCAL_FACETS.to_vec()))
        });
        if let Some(definition) = definition {
            self.emit_definition(definition)?;
        }
        self.push_unhandled_children(node, scope, &[self.grammar.fields.name]);
        Ok(())
    }

    /// An `impl` block: type references, a member scope, and its owner link.
    fn implementation(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let fields = self.grammar.fields;
        if let Some(trait_node) = node.child_by_field_id(fields.trait_field) {
            self.reference(trait_node, scope, ReferenceRole::Type)?;
        }
        let type_node = node.child_by_field_id(fields.type_field);
        if let Some(type_node) = type_node {
            self.reference(type_node, scope, ReferenceRole::Type)?;
        }
        let body = node.child_by_field_id(fields.body);
        if let Some(body) = body {
            self.implementation_body(node, body, scope, type_node)?;
        }
        let handled = [
            fields.trait_field,
            fields.type_field,
            fields.body,
            fields.type_parameters,
        ];
        self.push_unhandled_children(node, scope, &handled);
        Ok(())
    }

    fn implementation_body(
        &mut self,
        node: Node<'a>,
        body: Node<'a>,
        scope: UnitScopeIndex,
        type_node: Option<Node<'a>>,
    ) -> Result<(), UnitDropped> {
        let Some(range) = node_range(body) else {
            return Ok(());
        };
        let member = self.scope(ScopeKind::Member, range, scope)?;
        if let Some(owner) = type_node.and_then(|type_node| self.extracted_path(type_node)) {
            self.builder
                .member_link(UnitMemberLink::new(scope, owner.anchor, owner.path, member))
                .map_err(|_| UnitDropped)?;
        }
        self.type_parameter_list(node, member)?;
        self.pending.push((body, member));
        Ok(())
    }

    fn block(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let block_scope = self.scope(ScopeKind::Block, range, scope)?;
        self.push_children(node, block_scope);
        Ok(())
    }

    fn closure(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let closure_scope = self.scope(ScopeKind::Block, range, scope)?;
        let fields = self.grammar.fields;
        if let Some(parameters) = node.child_by_field_id(fields.parameters) {
            self.closure_parameters(parameters, closure_scope)?;
        }
        self.push_unhandled_children(node, closure_scope, &[fields.parameters]);
        Ok(())
    }

    fn closure_parameters(
        &mut self,
        list: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        for index in 0..list.named_child_count() {
            let Some(child) = list.named_child(index) else {
                continue;
            };
            self.charge()?;
            if child.kind_id() == grammar.kinds.parameter {
                self.typed_parameter(child, scope)?;
            } else {
                self.pattern_bindings(
                    child,
                    scope,
                    DefinitionOrder::Item,
                    PARAMETER_KIND_WORD,
                    PARAMETER_FACETS,
                )?;
            }
        }
        Ok(())
    }

    /// A function's parameter list: patterns bind, types read, `self` binds nothing.
    fn parameter_list(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(list) = node.child_by_field_id(grammar.fields.parameters) else {
            return Ok(());
        };
        for index in 0..list.named_child_count() {
            let Some(child) = list.named_child(index) else {
                continue;
            };
            self.charge()?;
            if child.kind_id() == grammar.kinds.parameter {
                self.typed_parameter(child, scope)?;
            } else {
                self.pending.push((child, scope));
            }
        }
        Ok(())
    }

    fn typed_parameter(
        &mut self,
        parameter: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let fields = self.grammar.fields;
        if let Some(pattern) = parameter.child_by_field_id(fields.pattern) {
            self.pattern_bindings(
                pattern,
                scope,
                DefinitionOrder::Item,
                PARAMETER_KIND_WORD,
                PARAMETER_FACETS,
            )?;
        }
        if let Some(type_node) = parameter.child_by_field_id(fields.type_field) {
            self.pending.push((type_node, scope));
        }
        Ok(())
    }

    /// The generic parameters a function or impl declares, defined in `scope`.
    fn type_parameter_list(
        &mut self,
        owner: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(list) = owner.child_by_field_id(grammar.fields.type_parameters) else {
            return Ok(());
        };
        for index in 0..list.named_child_count() {
            let Some(child) = list.named_child(index) else {
                continue;
            };
            self.charge()?;
            let kind_id = child.kind_id();
            let parameter = kind_id == grammar.kinds.type_parameter
                || kind_id == grammar.kinds.const_parameter
                || kind_id == grammar.kinds.lifetime_parameter;
            if !parameter {
                continue;
            }
            if let Some(name_node) = child.child_by_field_id(grammar.fields.name) {
                self.generic_parameter_definition(name_node, scope)?;
            }
            self.push_unhandled_children(child, scope, &[grammar.fields.name]);
        }
        Ok(())
    }

    fn generic_parameter_definition(
        &mut self,
        name_node: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let Some(text) = self.node_text(name_node) else {
            return Ok(());
        };
        let Ok(name) = Name::new(text) else {
            return Ok(());
        };
        let Some(range) = node_range(name_node) else {
            return Ok(());
        };
        let definition = UnitDefinition::new(
            scope,
            name,
            range,
            exact_kind(TYPE_PARAMETER_KIND_WORD),
            DefinitionOrder::Item,
            VisibilitySpelling::Private,
        )
        .with_facets(vec![SymbolFacet::TypeParameter]);
        self.emit_definition(definition).map(|_| ())
    }

    fn match_body(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        if let Some(value) = node.child_by_field_id(grammar.fields.value) {
            self.pending.push((value, scope));
        }
        let Some(body) = node.child_by_field_id(grammar.fields.body) else {
            return Ok(());
        };
        for index in 0..body.named_child_count() {
            let Some(arm) = body.named_child(index) else {
                continue;
            };
            self.charge()?;
            if arm.kind_id() == grammar.kinds.match_arm {
                self.match_arm(arm, scope)?;
            }
        }
        Ok(())
    }

    /// One match arm: its pattern binds in an arm scope the guard and value share.
    fn match_arm(&mut self, arm: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(range) = node_range(arm) else {
            return Ok(());
        };
        let arm_scope = self.scope(ScopeKind::Block, range, scope)?;
        if let Some(pattern) = arm.child_by_field_id(grammar.fields.pattern) {
            self.arm_pattern(pattern, arm_scope)?;
        }
        if let Some(value) = arm.child_by_field_id(grammar.fields.value) {
            self.pending.push((value, arm_scope));
        }
        Ok(())
    }

    fn arm_pattern(&mut self, pattern: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let guard = pattern.child_by_field_id(self.grammar.fields.condition);
        for index in 0..pattern.named_child_count() {
            let Some(child) = pattern.named_child(index) else {
                continue;
            };
            self.charge()?;
            if guard.map(|node| node.id()) == Some(child.id()) {
                self.pending.push((child, scope));
            } else {
                self.pattern_bindings(
                    child,
                    scope,
                    DefinitionOrder::Item,
                    LOCAL_KIND_WORD,
                    LOCAL_FACETS,
                )?;
            }
        }
        Ok(())
    }

    /// A `for` loop: the pattern binds in the loop scope, the iterated value outside it.
    fn for_loop(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let loop_scope = self.scope(ScopeKind::Block, range, scope)?;
        if let Some(pattern) = node.child_by_field_id(grammar.fields.pattern) {
            self.pattern_bindings(
                pattern,
                loop_scope,
                DefinitionOrder::Item,
                LOCAL_KIND_WORD,
                LOCAL_FACETS,
            )?;
        }
        if let Some(value) = node.child_by_field_id(grammar.fields.value) {
            self.pending.push((value, scope));
        }
        if let Some(body) = node.child_by_field_id(grammar.fields.body) {
            self.pending.push((body, loop_scope));
        }
        Ok(())
    }

    fn while_loop(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let loop_scope = self.scope(ScopeKind::Block, range, scope)?;
        if let Some(condition) = node.child_by_field_id(grammar.fields.condition) {
            self.condition(condition, loop_scope, scope)?;
        }
        if let Some(body) = node.child_by_field_id(grammar.fields.body) {
            self.pending.push((body, loop_scope));
        }
        Ok(())
    }

    /// An `if`: only a `let`-bearing condition opens a scope over the consequence.
    fn if_branch(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let condition = node.child_by_field_id(grammar.fields.condition);
        let bound = condition.is_some_and(|condition| {
            condition.kind_id() == grammar.kinds.let_condition
                || condition.kind_id() == grammar.kinds.let_chain
        });
        if !bound {
            self.push_children(node, scope);
            return Ok(());
        }
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let branch_scope = self.scope(ScopeKind::Block, range, scope)?;
        if let Some(condition) = condition {
            self.condition(condition, branch_scope, scope)?;
        }
        if let Some(consequence) = node.child_by_field_id(grammar.fields.consequence) {
            self.pending.push((consequence, branch_scope));
        }
        if let Some(alternative) = node.child_by_field_id(grammar.fields.alternative) {
            self.pending.push((alternative, scope));
        }
        Ok(())
    }

    /// Routes one condition: patterns bind in `inner`, evaluated values read in `outer`.
    fn condition(
        &mut self,
        node: Node<'a>,
        inner: UnitScopeIndex,
        outer: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let kind_id = node.kind_id();
        if kind_id == grammar.kinds.let_condition {
            return self.let_condition(node, inner, outer);
        }
        if kind_id != grammar.kinds.let_chain {
            self.pending.push((node, outer));
            return Ok(());
        }
        for index in 0..node.named_child_count() {
            let Some(child) = node.named_child(index) else {
                continue;
            };
            self.charge()?;
            if child.kind_id() == grammar.kinds.let_condition {
                self.let_condition(child, inner, outer)?;
            } else {
                self.pending.push((child, outer));
            }
        }
        Ok(())
    }

    fn let_condition(
        &mut self,
        node: Node<'a>,
        inner: UnitScopeIndex,
        outer: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        if let Some(pattern) = node.child_by_field_id(grammar.fields.pattern) {
            self.pattern_bindings(
                pattern,
                inner,
                DefinitionOrder::Item,
                LOCAL_KIND_WORD,
                LOCAL_FACETS,
            )?;
        }
        if let Some(value) = node.child_by_field_id(grammar.fields.value) {
            self.pending.push((value, outer));
        }
        Ok(())
    }

    /// A `let` statement: bindings become visible at the statement's end byte.
    fn local_declaration(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        if let Some(pattern) = node.child_by_field_id(grammar.fields.pattern) {
            self.pattern_bindings(
                pattern,
                scope,
                DefinitionOrder::Sequential(range.end()),
                LOCAL_KIND_WORD,
                LOCAL_FACETS,
            )?;
        }
        self.push_unhandled_children(node, scope, &[grammar.fields.pattern]);
        Ok(())
    }

    /// Emits one definition per bound `identifier` inside a pattern.
    ///
    /// The type field of a tuple-struct or struct pattern and every scoped path are
    /// skipped: they name existing items rather than bind new ones.
    fn pattern_bindings(
        &mut self,
        pattern: Node<'a>,
        scope: UnitScopeIndex,
        order: DefinitionOrder,
        kind_word: &'static str,
        facets: &[SymbolFacet],
    ) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let mut pending = vec![pattern];
        while let Some(node) = pending.pop() {
            self.charge()?;
            let kind_id = node.kind_id();
            if kind_id == grammar.kinds.identifier {
                self.binding_definition(node, scope, order, kind_word, facets)?;
                continue;
            }
            if kind_id == grammar.kinds.scoped_identifier {
                continue;
            }
            let typed = kind_id == grammar.kinds.tuple_struct_pattern
                || kind_id == grammar.kinds.struct_pattern;
            let skipped = typed
                .then(|| node.child_by_field_id(grammar.fields.type_field))
                .flatten()
                .map(|type_node| type_node.id());
            for index in 0..node.named_child_count() {
                let Some(child) = node.named_child(index) else {
                    continue;
                };
                if Some(child.id()) != skipped {
                    pending.push(child);
                }
            }
        }
        Ok(())
    }

    fn binding_definition(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        order: DefinitionOrder,
        kind_word: &'static str,
        facets: &[SymbolFacet],
    ) -> Result<(), UnitDropped> {
        let Some(text) = self.node_text(node) else {
            return Ok(());
        };
        let Ok(name) = Name::new(text) else {
            return Ok(());
        };
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let definition = UnitDefinition::new(
            scope,
            name,
            range,
            exact_kind(kind_word),
            order,
            VisibilitySpelling::Private,
        )
        .with_facets(facets.to_vec());
        self.emit_definition(definition).map(|_| ())
    }

    /// Flattens one `use` declaration into import links at `scope`.
    fn use_tree(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let visibility = visibility_spelling(&declaration_visibility(node, self.text));
        let Some(argument) = node.child_by_field_id(grammar.fields.argument) else {
            return Ok(());
        };
        let mut entries: Vec<(Node<'a>, Vec<Name>, PathAnchor)> =
            vec![(argument, Vec::new(), PathAnchor::SelfModule)];
        while let Some((entry, prefix, anchor)) = entries.pop() {
            self.charge()?;
            self.use_entry(entry, &prefix, anchor, visibility, scope, &mut entries)?;
        }
        Ok(())
    }

    fn use_entry(
        &mut self,
        entry: Node<'a>,
        prefix: &[Name],
        anchor: PathAnchor,
        visibility: VisibilitySpelling,
        scope: UnitScopeIndex,
        entries: &mut Vec<(Node<'a>, Vec<Name>, PathAnchor)>,
    ) -> Result<(), UnitDropped> {
        let kinds = self.grammar.kinds;
        let kind_id = entry.kind_id();
        if kind_id == kinds.identifier || kind_id == kinds.scoped_identifier {
            let Some((anchor, segments)) = self.merged_use_path(entry, prefix, anchor) else {
                return Ok(());
            };
            let bound = segments.last().cloned();
            return self.emit_import(
                scope,
                bound,
                anchor,
                segments,
                IMPORT_EXPLICIT_RANK,
                visibility,
            );
        }
        if kind_id == kinds.use_as_clause {
            return self.aliased_entry(entry, prefix, anchor, visibility, scope);
        }
        if kind_id == kinds.use_list {
            push_list_entries(entry, prefix, anchor, entries);
            return Ok(());
        }
        if kind_id == kinds.scoped_use_list {
            self.scoped_list_entry(entry, prefix, anchor, entries);
            return Ok(());
        }
        if kind_id == kinds.use_wildcard {
            return self.wildcard_entry(entry, prefix, anchor, visibility, scope);
        }
        if kind_id == kinds.self_keyword {
            let Some(bound) = prefix.last().cloned() else {
                return Ok(());
            };
            let segments = prefix.to_vec();
            return self.emit_import(
                scope,
                Some(bound),
                anchor,
                segments,
                IMPORT_EXPLICIT_RANK,
                visibility,
            );
        }
        Ok(())
    }

    fn aliased_entry(
        &mut self,
        entry: Node<'a>,
        prefix: &[Name],
        anchor: PathAnchor,
        visibility: VisibilitySpelling,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let fields = self.grammar.fields;
        let Some(path_node) = entry.child_by_field_id(fields.path) else {
            return Ok(());
        };
        let Some((anchor, segments)) = self.merged_use_path(path_node, prefix, anchor) else {
            return Ok(());
        };
        let alias = entry
            .child_by_field_id(fields.alias)
            .and_then(|alias| self.node_text(alias))
            .and_then(|text| Name::new(text).ok());
        let Some(alias) = alias else {
            return Ok(());
        };
        self.emit_import(
            scope,
            Some(alias),
            anchor,
            segments,
            IMPORT_EXPLICIT_RANK,
            visibility,
        )
    }

    fn scoped_list_entry(
        &mut self,
        entry: Node<'a>,
        prefix: &[Name],
        anchor: PathAnchor,
        entries: &mut Vec<(Node<'a>, Vec<Name>, PathAnchor)>,
    ) {
        let fields = self.grammar.fields;
        let merged = match entry.child_by_field_id(fields.path) {
            Some(path_node) => self.merged_use_path(path_node, prefix, anchor),
            None => Some((anchor, prefix.to_vec())),
        };
        let Some((anchor, prefix)) = merged else {
            return;
        };
        if let Some(list) = entry.child_by_field_id(fields.list) {
            push_list_entries(list, &prefix, anchor, entries);
        }
    }

    fn wildcard_entry(
        &mut self,
        entry: Node<'a>,
        prefix: &[Name],
        anchor: PathAnchor,
        visibility: VisibilitySpelling,
        scope: UnitScopeIndex,
    ) -> Result<(), UnitDropped> {
        let merged = match entry.named_child(0) {
            Some(path_node) => self.merged_use_path(path_node, prefix, anchor),
            None => Some((anchor, prefix.to_vec())),
        };
        let Some((anchor, segments)) = merged else {
            return Ok(());
        };
        self.emit_import(
            scope,
            None,
            anchor,
            segments,
            IMPORT_WILDCARD_RANK,
            visibility,
        )
    }

    /// Appends a use-path node's segments to `prefix`.
    ///
    /// The node's own anchor may stand only at the front of the whole path.
    fn merged_use_path(
        &self,
        node: Node<'a>,
        prefix: &[Name],
        anchor: PathAnchor,
    ) -> Option<(PathAnchor, Vec<Name>)> {
        let (extracted, segments) = self.extracted_segments(node)?;
        let anchor = match (extracted, prefix.is_empty()) {
            (PathAnchor::Lexical, _) => anchor,
            (explicit, true) => explicit,
            _ => return None,
        };
        let mut merged = prefix.to_vec();
        merged.extend(segments);
        Some((anchor, merged))
    }

    fn emit_import(
        &mut self,
        scope: UnitScopeIndex,
        name: Option<Name>,
        anchor: PathAnchor,
        segments: Vec<Name>,
        rank: Rank,
        visibility: VisibilitySpelling,
    ) -> Result<(), UnitDropped> {
        let Ok(path) = NamePath::new(segments) else {
            return Ok(());
        };
        self.builder
            .import(UnitImport::new(scope, name, anchor, path, visibility, rank))
            .map_err(|_| UnitDropped)
    }

    /// A `generic_type` reference, then descent into its type arguments.
    fn generic_type(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        self.reference(node, scope, ReferenceRole::Type)?;
        self.push_unhandled_children(node, scope, &[self.grammar.fields.type_field]);
        Ok(())
    }

    fn generic_function(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        role: ReferenceRole,
    ) -> Result<(), UnitDropped> {
        let fields = self.grammar.fields;
        if let Some(function) = node.child_by_field_id(fields.function) {
            if self.reference_kind(function) {
                self.reference(function, scope, role)?;
            } else {
                self.pending.push((function, scope));
            }
        }
        self.push_unhandled_children(node, scope, &[fields.function]);
        Ok(())
    }

    /// A call: a plain or scoped callee becomes a `Call` reference.
    fn call(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let Some(callee) = node.child_by_field_id(grammar.fields.function) else {
            self.push_children(node, scope);
            return Ok(());
        };
        let kind_id = callee.kind_id();
        if kind_id == grammar.kinds.identifier || kind_id == grammar.kinds.scoped_identifier {
            self.reference(callee, scope, ReferenceRole::Call)?;
        } else if kind_id == grammar.kinds.generic_function {
            self.generic_function(callee, scope, ReferenceRole::Call)?;
        } else {
            self.pending.push((callee, scope));
        }
        self.push_unhandled_children(node, scope, &[grammar.fields.function]);
        Ok(())
    }

    /// A macro invocation: the macro name is a call; its token tree stays unread.
    fn macro_call(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        if let Some(name) = node.child_by_field_id(self.grammar.fields.macro_field) {
            self.reference(name, scope, ReferenceRole::Call)?;
        }
        Ok(())
    }

    fn assignment(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        let grammar = self.grammar;
        let left = node.child_by_field_id(grammar.fields.left);
        match left {
            Some(target) if target.kind_id() == grammar.kinds.identifier => {
                self.reference(target, scope, ReferenceRole::Write)?;
                self.push_unhandled_children(node, scope, &[grammar.fields.left]);
            }
            _ => self.push_children(node, scope),
        }
        Ok(())
    }

    fn struct_literal(&mut self, node: Node<'a>, scope: UnitScopeIndex) -> Result<(), UnitDropped> {
        if let Some(name) = node.child_by_field_id(self.grammar.fields.name) {
            self.reference(name, scope, ReferenceRole::Type)?;
        }
        self.push_unhandled_children(node, scope, &[self.grammar.fields.name]);
        Ok(())
    }

    /// Emits one reference for a name or path node; unextractable shapes are skipped.
    fn reference(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        role: ReferenceRole,
    ) -> Result<(), UnitDropped> {
        let kinds = self.grammar.kinds;
        let kind_id = node.kind_id();
        let single = kind_id == kinds.identifier || kind_id == kinds.type_identifier;
        let extracted = if single {
            self.single_segment(node)
        } else if self.reference_kind(node) {
            self.extracted_path(node)
        } else {
            None
        };
        let Some(extracted) = extracted else {
            return Ok(());
        };
        let Some(range) = node_range(node) else {
            return Ok(());
        };
        let reference = UnitReference::new(scope, range, extracted.anchor, extracted.path, role);
        self.builder.reference(reference).map_err(|_| UnitDropped)
    }

    /// Whether `node`'s kind can form a reference path.
    fn reference_kind(&self, node: Node<'a>) -> bool {
        let kinds = self.grammar.kinds;
        let kind_id = node.kind_id();
        kind_id == kinds.identifier
            || kind_id == kinds.type_identifier
            || kind_id == kinds.scoped_identifier
            || kind_id == kinds.scoped_type_identifier
            || kind_id == kinds.generic_type
    }

    fn single_segment(&self, node: Node<'a>) -> Option<ExtractedPath> {
        let name = Name::new(self.node_text(node)?).ok()?;
        Some(ExtractedPath {
            anchor: PathAnchor::Lexical,
            path: NamePath::single(name),
        })
    }

    fn extracted_path(&self, node: Node<'a>) -> Option<ExtractedPath> {
        let (anchor, segments) = self.extracted_segments(node)?;
        let path = NamePath::new(segments).ok()?;
        Some(ExtractedPath { anchor, path })
    }

    fn extracted_segments(&self, node: Node<'a>) -> Option<(PathAnchor, Vec<Name>)> {
        let nodes = self.raw_path_nodes(node)?;
        self.anchored(&nodes)
    }

    /// The path chain's nodes leading-first, generics stripped.
    ///
    /// `None` for a shape this module does not read, such as a bracketed
    /// qualified type.
    fn raw_path_nodes(&self, node: Node<'a>) -> Option<Vec<Node<'a>>> {
        let grammar = self.grammar;
        let mut reversed = Vec::new();
        let mut current = Some(node);
        for _ in 0..PATH_NODES_MAX {
            let Some(step) = current else {
                reversed.reverse();
                return Some(reversed);
            };
            let kind_id = step.kind_id();
            if kind_id == grammar.kinds.scoped_identifier
                || kind_id == grammar.kinds.scoped_type_identifier
            {
                reversed.push(step.child_by_field_id(grammar.fields.name)?);
                current = step.child_by_field_id(grammar.fields.path);
            } else if kind_id == grammar.kinds.generic_type {
                current = step.child_by_field_id(grammar.fields.type_field);
            } else if kind_id == grammar.kinds.identifier
                || kind_id == grammar.kinds.type_identifier
                || kind_id == grammar.kinds.crate_keyword
                || kind_id == grammar.kinds.self_keyword
                || kind_id == grammar.kinds.super_keyword
            {
                reversed.push(step);
                current = None;
            } else {
                return None;
            }
        }
        None
    }

    /// Splits leading `crate`, `self`, or `super` keywords off as the path's anchor.
    fn anchored(&self, nodes: &[Node<'a>]) -> Option<(PathAnchor, Vec<Name>)> {
        let kinds = self.grammar.kinds;
        let (anchor, first_segment) = match nodes.first().map(Node::kind_id) {
            Some(kind_id) if kind_id == kinds.crate_keyword => (PathAnchor::Crate, 1),
            Some(kind_id) if kind_id == kinds.self_keyword => (PathAnchor::SelfModule, 1),
            Some(kind_id) if kind_id == kinds.super_keyword => {
                let count = nodes
                    .iter()
                    .take_while(|node| node.kind_id() == kinds.super_keyword)
                    .count();
                (PathAnchor::Super(u8::try_from(count).ok()?), count)
            }
            _ => (PathAnchor::Lexical, 0),
        };
        let mut names = Vec::new();
        for node in &nodes[first_segment..] {
            let kind_id = node.kind_id();
            if kind_id != kinds.identifier && kind_id != kinds.type_identifier {
                return None;
            }
            names.push(Name::new(self.node_text(*node)?).ok()?);
        }
        Some((anchor, names))
    }

    /// The definition an item declares, spanning the item node itself.
    ///
    /// `extract::extract` records the same span as the syntax symbol's `item_range`
    /// (`let range = byte_range(node)?;` feeding `qualified_symbol`), so an `is_item`
    /// definition's range equals its syntax declaration's `item_range` and equivalence
    /// evidence can bind the two.
    fn item_definition(
        &self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        kind: RustSymbolKind,
        order: DefinitionOrder,
        is_item: bool,
    ) -> Option<UnitDefinition> {
        let name = self.field_named(node)?;
        let range = node_range(node)?;
        let visibility = declaration_visibility(node, self.text);
        let facets = declaration_facets(kind, &visibility);
        let definition = UnitDefinition::new(
            scope,
            name,
            range,
            exact_kind(kind.word()),
            order,
            visibility_spelling(&visibility),
        )
        .with_facets(facets);
        Some(if is_item {
            definition.item()
        } else {
            definition
        })
    }

    /// A `union_item` definition; the syntax provider extracts no unions, so no syntax
    /// declaration shares it.
    fn union_definition(&self, node: Node<'a>, scope: UnitScopeIndex) -> Option<UnitDefinition> {
        let name = self.field_named(node)?;
        let range = node_range(node)?;
        let visibility = declaration_visibility(node, self.text);
        let definition = UnitDefinition::new(
            scope,
            name,
            range,
            exact_kind(UNION_KIND_WORD),
            DefinitionOrder::Item,
            visibility_spelling(&visibility),
        );
        Some(definition.with_facets(vec![SymbolFacet::Type]))
    }

    fn emit_definition(
        &mut self,
        definition: UnitDefinition,
    ) -> Result<UnitDefinitionIndex, UnitDropped> {
        self.builder.definition(definition).map_err(|_| UnitDropped)
    }

    fn scope(
        &mut self,
        kind: ScopeKind,
        range: SourceRange,
        parent: UnitScopeIndex,
    ) -> Result<UnitScopeIndex, UnitDropped> {
        self.builder
            .scope(kind, range, Some(parent))
            .map_err(|_| UnitDropped)
    }

    fn field_named(&self, node: Node<'a>) -> Option<Name> {
        let name = node.child_by_field_id(self.grammar.fields.name)?;
        Name::new(self.node_text(name)?).ok()
    }

    fn node_text(&self, node: Node<'_>) -> Option<&'a str> {
        self.text.get(node.byte_range())
    }

    fn push_children(&mut self, node: Node<'a>, scope: UnitScopeIndex) {
        for index in 0..node.named_child_count() {
            if let Some(child) = node.named_child(index) {
                self.pending.push((child, scope));
            }
        }
    }

    /// Pushes every named child not already consumed through one of `skip_fields`.
    fn push_unhandled_children(
        &mut self,
        node: Node<'a>,
        scope: UnitScopeIndex,
        skip_fields: &[u16],
    ) {
        let skipped: Vec<usize> = skip_fields
            .iter()
            .filter_map(|field| node.child_by_field_id(*field))
            .map(|child| child.id())
            .collect();
        for index in 0..node.named_child_count() {
            let Some(child) = node.named_child(index) else {
                continue;
            };
            if !skipped.contains(&child.id()) {
                self.pending.push((child, scope));
            }
        }
    }
}

fn push_list_entries<'tree>(
    list: Node<'tree>,
    prefix: &[Name],
    anchor: PathAnchor,
    entries: &mut Vec<(Node<'tree>, Vec<Name>, PathAnchor)>,
) {
    for index in 0..list.named_child_count() {
        if let Some(child) = list.named_child(index) {
            entries.push((child, prefix.to_vec(), anchor));
        }
    }
}

/// The node's span at wire width; `None` for a zero-width node.
fn node_range(node: Node<'_>) -> Option<SourceRange> {
    let start = u64::try_from(node.start_byte()).ok()?;
    let end = u64::try_from(node.end_byte()).ok()?;
    SourceRange::new(start, end).ok()
}

fn exact_kind(word: &str) -> ExactKind {
    ExactKind(format!("{RUST_KIND_PREFIX}.{word}"))
}

/// Maps an authored visibility to the resolver's spelling; `pub(in path)` and
/// `pub(self)` narrow to private.
fn visibility_spelling(visibility: &RustVisibility) -> VisibilitySpelling {
    match visibility {
        RustVisibility::Private => VisibilitySpelling::Private,
        RustVisibility::Public => VisibilitySpelling::Public,
        RustVisibility::Restricted(text) => restricted_spelling(text),
    }
}

fn restricted_spelling(text: &str) -> VisibilitySpelling {
    let compact: String = text.split_whitespace().collect();
    match compact.as_str() {
        "pub(crate)" => VisibilitySpelling::Crate,
        "pub(super)" => VisibilitySpelling::Super,
        _ => VisibilitySpelling::Private,
    }
}

/// The two project paths that could hold a declared module's body, strongest first.
fn module_candidates(unit_path: &str, module: &str) -> Vec<String> {
    let (directory, file) = match unit_path.rsplit_once('/') {
        Some((directory, file)) => (directory, file),
        None => ("", unit_path),
    };
    let base = if DIRECTORY_OWNING_FILE_NAMES.contains(&file) {
        directory.to_owned()
    } else {
        let stem = file.strip_suffix(RUST_FILE_SUFFIX).unwrap_or(file);
        joined(directory, stem)
    };
    vec![
        joined(&base, &format!("{module}{RUST_FILE_SUFFIX}")),
        joined(&base, &format!("{module}/mod{RUST_FILE_SUFFIX}")),
    ]
}

fn joined(directory: &str, tail: &str) -> String {
    if directory.is_empty() {
        tail.to_owned()
    } else {
        format!("{directory}/{tail}")
    }
}

#[cfg(test)]
mod tests {
    use rift_binding::{
        BindingGraph, BindingLimits, LinkedGraph, NeverCancelled, ResolutionSet, assemble,
        resolve_all,
    };
    use rift_core::{
        ContributionOrigin, ProjectPath, ReferenceRole, SourceKind, SourceLocation, SourceUnitId,
        encode_path,
    };

    use super::{binding_grammar, module_candidates};
    use crate::{RustSyntaxProvider, SyntaxDocument, SyntaxProvider, SyntaxSource};

    fn analyze(path: &str, text: &str) -> SyntaxDocument {
        let path = ProjectPath::new(path).expect("fixture path");
        RustSyntaxProvider::default()
            .analyze(SyntaxSource { path: &path, text })
            .expect("fixture parses")
    }

    fn source_unit(path: &str) -> SourceUnitId {
        SourceUnitId::parse(&format!("rift://source/project/{}", encode_path(path)))
            .expect("fixture unit identity")
    }

    fn origin() -> ContributionOrigin {
        let location = SourceLocation::Project { package: None };
        ContributionOrigin::new(Some(location), SourceKind::Authored).expect("authored origin")
    }

    /// Assembles, links, and resolves the documents' extracted binding facts.
    fn resolved(documents: &[(&str, &SyntaxDocument)]) -> (BindingGraph, ResolutionSet) {
        let units: Vec<_> = documents
            .iter()
            .map(|(path, document)| {
                let facts = document.binding().expect("binding facts extracted");
                (source_unit(path), origin(), facts)
            })
            .collect();
        let limits = BindingLimits::default();
        let graph = assemble(&units, &limits).expect("facts assemble");
        let set = {
            let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
            resolve_all(&linked, &limits, &NeverCancelled).expect("resolution completes")
        };
        (graph, set)
    }

    fn offset(text: &str, needle: &str) -> u64 {
        u64::try_from(text.find(needle).expect("needle in fixture")).expect("offset fits")
    }

    fn last_offset(text: &str, needle: &str) -> u64 {
        u64::try_from(text.rfind(needle).expect("needle in fixture")).expect("offset fits")
    }

    /// Targets of the reference starting at `at` in `unit_path`: `(name, unit key, start)`.
    fn targets_at(
        graph: &BindingGraph,
        set: &ResolutionSet,
        unit_path: &str,
        at: u64,
    ) -> Vec<(String, String, u64)> {
        let reference = graph
            .reference_ids()
            .find(|id| {
                let reference = graph.reference(*id);
                let unit = graph.unit(graph.scope(reference.scope()).unit());
                unit.source().key().as_str() == unit_path && reference.range().start() == at
            })
            .expect("reference at offset");
        set.resolution(reference)
            .targets()
            .iter()
            .map(|id| {
                let definition = graph.definition(*id);
                let unit = graph.unit(graph.scope(definition.scope()).unit());
                (
                    definition.name().as_str().to_owned(),
                    unit.source().key().as_str().to_owned(),
                    definition.range().start(),
                )
            })
            .collect()
    }

    fn lib_targets(text: &str, at: u64) -> Vec<(String, String, u64)> {
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        targets_at(&graph, &set, "src/lib.rs", at)
    }

    fn target(name: &str, start: u64) -> (String, String, u64) {
        (name.to_owned(), "src/lib.rs".to_owned(), start)
    }

    #[test]
    fn test_binding_nearest_lexical_definition_wins() {
        let text = "fn f() {}\nfn h() {\n    fn f() {}\n    { fn f() {} f(); }\n}\n";
        let targets = lib_targets(text, offset(text, "f();"));
        assert_eq!(targets, [target("f", last_offset(text, "fn f() {}"))]);
    }

    #[test]
    fn test_binding_let_shadowing_resolves_by_position() {
        let text = "fn h() {\n    first(x);\n    let x = 1;\n    second(x);\n    let x = 2;\n    third(x);\n}\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let before = targets_at(&graph, &set, "src/lib.rs", offset(text, "x);"));
        assert!(
            before.is_empty(),
            "no binding is visible before the first let"
        );
        let between = targets_at(&graph, &set, "src/lib.rs", offset(text, "second(x") + 7);
        assert_eq!(between, [target("x", offset(text, "x = 1"))]);
        let after = targets_at(&graph, &set, "src/lib.rs", offset(text, "third(x") + 6);
        assert_eq!(after, [target("x", offset(text, "x = 2"))]);
    }

    #[test]
    fn test_binding_item_reference_before_definition_resolves() {
        let text = "fn h() { later(); }\nfn later() {}\n";
        let targets = lib_targets(text, offset(text, "later()"));
        assert_eq!(targets, [target("later", offset(text, "fn later()"))]);
    }

    #[test]
    fn test_binding_inline_module_does_not_inherit_parent_names() {
        let text = "fn f() {}\nmod inner {\n    fn g() { f(); }\n}\nmod outer {\n    use super::f;\n    fn g() { f(); }\n}\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let unimported = targets_at(&graph, &set, "src/lib.rs", offset(text, "f();"));
        assert!(
            unimported.is_empty(),
            "an inline module does not inherit its parent's names"
        );
        let imported = targets_at(&graph, &set, "src/lib.rs", last_offset(text, "f();"));
        assert_eq!(imported, [target("f", 0)]);
    }

    #[test]
    fn test_binding_alias_import_binds_alias_only() {
        let text = "mod m { pub fn f() {} }\nuse m::f as g;\nfn h() { g(); }\n";
        let targets = lib_targets(text, offset(text, "g();"));
        assert_eq!(targets, [target("f", offset(text, "pub fn f"))]);
    }

    #[test]
    fn test_binding_explicit_import_shadows_wildcard() {
        let text = "mod a { pub fn f() {}\n    pub fn only() {} }\nmod b { pub fn f() {} }\nuse a::*;\nuse b::f;\nfn h() { f(); only(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let shadowed = targets_at(&graph, &set, "src/lib.rs", offset(text, "f();"));
        assert_eq!(shadowed, [target("f", last_offset(text, "pub fn f() {}"))]);
        let through_wildcard = targets_at(&graph, &set, "src/lib.rs", offset(text, "only();"));
        assert_eq!(
            through_wildcard,
            [target("only", offset(text, "pub fn only"))]
        );
    }

    #[test]
    fn test_binding_pub_crate_definition_visible_across_modules() {
        let text = "mod m { pub(crate) fn f() {} }\nfn h() { m::f(); }\n";
        let targets = lib_targets(text, offset(text, "m::f()"));
        assert_eq!(targets, [target("f", offset(text, "pub(crate) fn f"))]);
    }

    #[test]
    fn test_binding_private_definition_invisible_via_super_path() {
        let text = "mod a { fn hidden() {} }\nmod b { fn probe() { super::a::hidden(); } }\nmod c { pub fn shown() {} }\nmod d { fn probe() { super::c::shown(); } }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let hidden = targets_at(&graph, &set, "src/lib.rs", offset(text, "super::a::hidden"));
        assert!(hidden.is_empty(), "a private sibling item stays invisible");
        let shown = targets_at(&graph, &set, "src/lib.rs", offset(text, "super::c::shown"));
        assert_eq!(shown, [target("shown", offset(text, "pub fn shown"))]);
    }

    #[test]
    fn test_binding_unresolved_path_stays_empty() {
        let text = "fn h() { missing(); }\n";
        assert!(lib_targets(text, offset(text, "missing()")).is_empty());
    }

    #[test]
    fn test_binding_enum_variant_path_resolves() {
        let text = "enum Signal { Go, Stop }\nfn h() -> Signal { Signal::Go }\n";
        let targets = lib_targets(text, offset(text, "Signal::Go"));
        assert_eq!(targets, [target("Go", offset(text, "Go,"))]);
    }

    #[test]
    fn test_binding_associated_function_resolves_through_impl() {
        let text = "struct Beacon;\nimpl Beacon { pub fn create() -> Beacon { Beacon } }\nfn h() { Beacon::create(); }\n";
        let targets = lib_targets(text, offset(text, "Beacon::create"));
        assert_eq!(targets, [target("create", offset(text, "pub fn create"))]);
    }

    #[test]
    fn test_binding_trait_impl_members_attach_to_type() {
        let text = "trait Greet { fn greet(&self); }\nstruct Person;\nimpl Greet for Person { fn greet(&self) {} }\nfn h() { Person::greet; }\n";
        let targets = lib_targets(text, offset(text, "Person::greet;"));
        assert_eq!(
            targets,
            [target("greet", offset(text, "fn greet(&self) {}"))]
        );
    }

    #[test]
    fn test_binding_type_parameter_member_path_stays_unresolved() {
        let text = "fn h<Item>(value: Item) { Item::create(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let member = targets_at(&graph, &set, "src/lib.rs", offset(text, "Item::create"));
        assert!(member.is_empty(), "a type parameter opens no member scope");
        let annotation = targets_at(&graph, &set, "src/lib.rs", offset(text, "Item)"));
        assert_eq!(annotation, [target("Item", offset(text, "Item>"))]);
    }

    #[test]
    fn test_binding_nested_use_list_binds_each_entry() {
        let text = "mod a { pub fn b() {}\n    pub fn c() {} }\nuse a::{b, c as d};\nfn h() { b(); d(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let plain = targets_at(&graph, &set, "src/lib.rs", offset(text, "b();"));
        assert_eq!(plain, [target("b", offset(text, "pub fn b"))]);
        let aliased = targets_at(&graph, &set, "src/lib.rs", offset(text, "d();"));
        assert_eq!(aliased, [target("c", offset(text, "pub fn c"))]);
    }

    #[test]
    fn test_binding_use_self_path_resolves() {
        let text = "mod inner { pub fn x() {} }\nuse self::inner::x;\nfn h() { x(); }\n";
        let targets = lib_targets(text, offset(text, "x();"));
        assert_eq!(targets, [target("x", offset(text, "pub fn x"))]);
    }

    #[test]
    fn test_binding_use_inside_function_body_scopes_to_that_block() {
        let text = "mod m { pub fn f() {} }\nfn h() { use m::f; f(); }\nfn other() { f(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let inside = targets_at(&graph, &set, "src/lib.rs", offset(text, "f();"));
        assert_eq!(inside, [target("f", offset(text, "pub fn f"))]);
        let outside = targets_at(&graph, &set, "src/lib.rs", last_offset(text, "f();"));
        assert!(
            outside.is_empty(),
            "a body-local import ends with its block"
        );
    }

    #[test]
    fn test_binding_use_list_self_reexports_the_module_itself() {
        let text = "mod a { pub fn f() {} }\nmod b { pub use super::a::{self, f}; }\nfn h() { b::a::f(); b::f(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let through_module = targets_at(&graph, &set, "src/lib.rs", offset(text, "b::a::f()"));
        assert_eq!(through_module, [target("f", offset(text, "pub fn f"))]);
        let direct = targets_at(&graph, &set, "src/lib.rs", offset(text, "b::f()"));
        assert_eq!(direct, [target("f", offset(text, "pub fn f"))]);
    }

    #[test]
    fn test_binding_mod_declaration_links_sibling_unit() {
        let lib = "mod worker;\nfn h() { worker::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let worker_document = analyze("src/worker.rs", "pub fn run() {}\n");
        let (graph, set) = resolved(&[
            ("src/lib.rs", &lib_document),
            ("src/worker.rs", &worker_document),
        ]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "worker::run"));
        assert_eq!(targets, [("run".to_owned(), "src/worker.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_binding_mod_declaration_prefers_file_candidate() {
        let lib = "mod worker;\nfn h() { worker::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let file_document = analyze("src/worker.rs", "pub fn run() {}\n");
        let directory_document = analyze("src/worker/mod.rs", "pub fn run() {}\n");
        let (graph, set) = resolved(&[
            ("src/lib.rs", &lib_document),
            ("src/worker.rs", &file_document),
            ("src/worker/mod.rs", &directory_document),
        ]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "worker::run"));
        assert_eq!(targets, [("run".to_owned(), "src/worker.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_binding_mod_declaration_directory_candidate_when_file_absent() {
        let lib = "mod worker;\nfn h() { worker::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let directory_document = analyze("src/worker/mod.rs", "pub fn run() {}\n");
        let (graph, set) = resolved(&[
            ("src/lib.rs", &lib_document),
            ("src/worker/mod.rs", &directory_document),
        ]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "worker::run"));
        assert_eq!(
            targets,
            [("run".to_owned(), "src/worker/mod.rs".to_owned(), 0)]
        );
    }

    #[test]
    fn test_binding_mod_declaration_unknown_unit_stays_unresolved() {
        let lib = "mod worker;\nfn h() { worker::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let (graph, set) = resolved(&[("src/lib.rs", &lib_document)]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "worker::run"));
        assert!(
            targets.is_empty(),
            "an absent unit leaves the module unlinked"
        );
    }

    #[test]
    fn test_binding_malformed_source_keeps_analyze_alive() {
        let document = analyze("src/lib.rs", "fn broken( {\n");
        assert!(document.has_errors());
        let empty = analyze("src/lib.rs", "");
        assert!(empty.binding().is_none(), "an empty source holds no scope");
    }

    #[test]
    fn test_binding_item_definition_ranges_equal_syntax_item_ranges() {
        let text = "/// Doc.\n#[derive(Debug)]\npub struct Marked;\n/// Doc.\npub fn f() {}\npub enum E { A }\npub trait Tr {}\npub type Al = u8;\npub const C: u8 = 0;\npub static G: u8 = 0;\npub mod m {}\nmacro_rules! q { () => {}; }\nmod declared;\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let item_definitions: Vec<_> = facts
            .definitions()
            .iter()
            .filter(|definition| definition.is_item())
            .collect();
        assert_eq!(item_definitions.len(), document.symbols().len());
        for symbol in document.symbols() {
            let matched = item_definitions.iter().any(|definition| {
                definition.name().as_str() == symbol.name
                    && definition.range().start() == symbol.item_range.start
                    && definition.range().end() == symbol.item_range.end
            });
            assert!(
                matched,
                "syntax symbol {} has a binding definition at its item_range",
                symbol.qualified_name
            );
        }
    }

    #[test]
    fn test_binding_extraction_is_deterministic() {
        let text = "mod m { pub fn f() {} }\nuse m::f;\nfn h() { f(); }\n";
        let first = analyze("src/lib.rs", text);
        let second = analyze("src/lib.rs", text);
        assert!(first.binding().is_some());
        assert_eq!(
            format!("{:?}", first.binding()),
            format!("{:?}", second.binding())
        );
    }

    #[test]
    fn test_binding_reference_roles_cover_write_macro_and_struct() {
        let text = "struct Pair { left: u8 }\nmacro_rules! ping { () => {}; }\nfn h() {\n    let built = Pair { left: 1 };\n    let mut count = 0;\n    count = 2;\n    ping!();\n    touch(built, count);\n}\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let literal = targets_at(&graph, &set, "src/lib.rs", offset(text, "Pair { left: 1"));
        assert_eq!(literal, [target("Pair", 0)]);
        let write_at = offset(text, "count = 2");
        let write = targets_at(&graph, &set, "src/lib.rs", write_at);
        assert_eq!(write, [target("count", offset(text, "count = 0"))]);
        let written = graph
            .reference_ids()
            .find(|id| graph.reference(*id).range().start() == write_at)
            .expect("write reference");
        assert_eq!(graph.reference(written).role(), ReferenceRole::Write);
        let call = targets_at(&graph, &set, "src/lib.rs", offset(text, "ping!"));
        assert_eq!(call, [target("ping", offset(text, "macro_rules! ping"))]);
    }

    #[test]
    fn test_binding_control_flow_patterns_bind_locally() {
        let text = "fn h(items: [u8; 2]) {\n    for item in items { touch(item); }\n    while let Some(found) = probe() { touch(found); }\n    if let Some(inner) = probe() { touch(inner); }\n    match probe() {\n        Some(arm) => touch(arm),\n        None => (),\n    }\n    let apply = |argument: u8| touch(argument);\n}\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let cases = [
            ("touch(item", "item in"),
            ("touch(found", "found) = probe"),
            ("touch(inner", "inner) = probe"),
            ("touch(arm", "arm) =>"),
            ("touch(argument", "argument: u8"),
        ];
        for (reference, definition) in cases {
            let name = &reference["touch(".len()..];
            let at = offset(text, reference) + 6;
            let targets = targets_at(&graph, &set, "src/lib.rs", at);
            assert_eq!(
                targets,
                [target(name, offset(text, definition))],
                "{reference}"
            );
        }
    }

    #[test]
    fn test_binding_function_parameters_bind_in_function_scope() {
        let text = "fn h(first: u8, second: u8) -> u8 { first }\nfn i() { first; }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let inside = targets_at(&graph, &set, "src/lib.rs", offset(text, "first }"));
        assert_eq!(inside, [target("first", offset(text, "first: u8"))]);
        let outside = targets_at(&graph, &set, "src/lib.rs", offset(text, "first;"));
        assert!(outside.is_empty(), "parameters end with their function");
    }

    #[test]
    fn test_binding_grammar_resolves_every_kind_and_field() {
        let grammar = binding_grammar();
        assert!(
            grammar.roles.len() >= 35,
            "role table covers the dispatched kinds: {}",
            grammar.roles.len()
        );
        assert!(grammar.role(grammar.kinds.function_item).is_some());
        assert_eq!(grammar.role(0), None);
    }

    #[test]
    fn test_binding_module_candidates_follow_the_directory_rule() {
        assert_eq!(
            module_candidates("src/lib.rs", "x"),
            ["src/x.rs", "src/x/mod.rs"]
        );
        assert_eq!(
            module_candidates("src/worker.rs", "x"),
            ["src/worker/x.rs", "src/worker/x/mod.rs"]
        );
        assert_eq!(
            module_candidates("src/nested/mod.rs", "x"),
            ["src/nested/x.rs", "src/nested/x/mod.rs"]
        );
        assert_eq!(module_candidates("main.rs", "x"), ["x.rs", "x/mod.rs"]);
        assert_eq!(
            module_candidates("tool.rs", "x"),
            ["tool/x.rs", "tool/x/mod.rs"]
        );
    }

    #[test]
    fn test_binding_generic_union_and_qualified_shapes_extract() {
        let text = "pub union Raw { first: u8, second: i8 }\ntrait Owner { type Assoc; }\nstruct Point { x: u8 }\nfn generic<Item>(value: Vec<Item>) -> Item { identity::<Item>(value) }\nfn h(pair: (u8, u8), point: Point) {\n    let (left, right) = pair;\n    let Point { x: inner } = point;\n    if plain() { touch(left); }\n    let mut slot = (0, 0);\n    slot.0 = right;\n    point.method();\n    touch(inner);\n}\nfn q() -> <Raw as Owner>::Assoc { loop {} }\nmod outer2 { pub(super) fn shared() {} }\nfn caller() { outer2::shared(); }\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let names: Vec<&str> = facts
            .definitions()
            .iter()
            .map(|definition| definition.name().as_str())
            .collect();
        assert!(names.contains(&"Raw"), "union definition extracted");
        assert!(names.contains(&"Assoc"), "associated type extracted");
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let turbofish = targets_at(&graph, &set, "src/lib.rs", offset(text, "Item>(value)"));
        assert_eq!(turbofish, [target("Item", offset(text, "Item>"))]);
        let destructured = targets_at(&graph, &set, "src/lib.rs", offset(text, "inner);"));
        assert_eq!(destructured, [target("inner", offset(text, "inner }"))]);
        let restricted = targets_at(&graph, &set, "src/lib.rs", offset(text, "outer2::shared"));
        assert_eq!(
            restricted,
            [target("shared", offset(text, "pub(super) fn shared"))]
        );
        let plain_branch = targets_at(&graph, &set, "src/lib.rs", offset(text, "left);"));
        assert_eq!(plain_branch, [target("left", offset(text, "left, right"))]);
    }

    #[test]
    fn test_binding_match_guard_let_conditions_bind_in_arm_scope() {
        let text = "fn h() {\n    match probe() {\n        value if let Some(x) = one(value) && let Some(y) = two(value) => touch(x, y),\n        other if let Some(z) = three(other) => touch(z),\n        _ => (),\n    }\n}\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let chained = targets_at(&graph, &set, "src/lib.rs", offset(text, "x, y),"));
        assert_eq!(chained, [target("x", offset(text, "x) = one"))]);
        let single = targets_at(&graph, &set, "src/lib.rs", offset(text, "z),"));
        assert_eq!(single, [target("z", offset(text, "z) = three"))]);
    }

    #[test]
    fn test_binding_top_level_use_list_flattens() {
        let text = "mod a { pub fn f() {} }\nuse {a::f};\nfn h() { f(); }\n";
        let targets = lib_targets(text, offset(text, "f();"));
        assert_eq!(targets, [target("f", offset(text, "pub fn f"))]);
    }

    #[test]
    fn test_binding_turbofish_value_reference_resolves() {
        let text = "fn probe() {}\nfn h() { let f = probe::<u8>; }\n";
        let targets = lib_targets(text, offset(text, "probe::<"));
        assert_eq!(targets, [target("probe", 0)]);
    }

    #[test]
    fn test_binding_method_turbofish_receiver_reads() {
        let text = "fn h() { let receiver = 1; receiver.take::<u8>(); }\n";
        let targets = lib_targets(text, offset(text, "receiver.take"));
        assert_eq!(targets, [target("receiver", offset(text, "receiver ="))]);
    }

    #[test]
    fn test_binding_closure_pattern_parameters_bind() {
        let text = "fn h() { let f = |(left, right)| touch(left, right); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let bound = targets_at(&graph, &set, "src/lib.rs", offset(text, "left, right);"));
        assert_eq!(bound, [target("left", offset(text, "left, right)|"))]);
    }

    #[test]
    fn test_binding_const_lifetime_and_attributed_generic_parameters() {
        let text = "fn f<'life, const COUNT: usize, #[cfg(test)] Item>(value: [Item; COUNT]) -> Item { loop {} }\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let names: Vec<&str> = facts
            .definitions()
            .iter()
            .map(|definition| definition.name().as_str())
            .collect();
        assert!(names.contains(&"'life"), "lifetime parameter defined");
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let count = targets_at(&graph, &set, "src/lib.rs", offset(text, "COUNT])"));
        assert_eq!(count, [target("COUNT", offset(text, "COUNT: usize"))]);
    }

    #[test]
    fn test_binding_overlong_names_drop_facts_not_unit() {
        let long = "a".repeat(300);
        let text = format!(
            "fn f<{long}>() {{}}\nfn h() {{ let {long} = 1; }}\nuse m::f as {long};\nmod {long};\n"
        );
        let document = analyze("src/lib.rs", &text);
        let facts = document.binding().expect("binding facts extracted");
        assert!(
            facts
                .definitions()
                .iter()
                .all(|definition| definition.name().as_str().len() <= 256),
            "a name past the byte bound emits no definition"
        );
        assert!(
            facts.imports().is_empty(),
            "an overlong alias emits no import"
        );
        assert!(
            facts.module_declarations().is_empty(),
            "an overlong module name emits no declaration"
        );
    }

    #[test]
    fn test_binding_if_let_else_alternative_reads_outer_scope() {
        let text = "fn fallback() {}\nfn h() { if let Some(x) = probe() { touch(x); } else { fallback(); } }\n";
        let targets = lib_targets(text, offset(text, "fallback();"));
        assert_eq!(targets, [target("fallback", 0)]);
    }

    #[test]
    fn test_binding_while_plain_condition_reads_outer_scope() {
        let text = "fn running() {}\nfn h() { while running() { step(); } }\n";
        let targets = lib_targets(text, offset(text, "running() { step"));
        assert_eq!(targets, [target("running", 0)]);
    }

    #[test]
    fn test_binding_let_chain_plain_operand_reads_outer_scope() {
        let text = "fn ready() {}\nfn h() { if let Some(x) = probe() && ready() { touch(x); } }\n";
        let targets = lib_targets(text, offset(text, "ready() { touch"));
        assert_eq!(targets, [target("ready", 0)]);
    }

    #[test]
    fn test_binding_scoped_arm_pattern_emits_no_facts() {
        let text = "enum Direction { Up }\nfn h(d: Direction) { match d { Direction::Up => (), _ => () } }\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let at = offset(text, "Direction::Up =>");
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.range().start() != at),
            "a scoped arm pattern names an item and binds nothing"
        );
    }

    #[test]
    fn test_binding_use_edge_shapes_emit_expected_import_counts() {
        let cases = [
            ("use a::{super::b};", 0),
            ("use self;", 0),
            ("use crate;", 0),
            ("use a::{super::b as c};", 0),
            ("use a::{super::{b}};", 0),
            ("use *;", 0),
            ("use a::{super::*};", 0),
            ("use crate::*;", 0),
            ("use ::{f};", 1),
        ];
        for (source, expected) in cases {
            let document = analyze("src/lib.rs", source);
            let facts = document.binding().expect("binding facts extracted");
            assert_eq!(facts.imports().len(), expected, "{source}");
        }
    }

    #[test]
    fn test_binding_impl_for_tuple_type_reads_trait_only() {
        let text = "trait Probe {}\nimpl Probe for (u8, u8) {}\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let tuple_at = offset(text, "(u8, u8)");
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.range().start() != tuple_at),
            "a tuple type forms no reference path"
        );
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let trait_targets = targets_at(&graph, &set, "src/lib.rs", offset(text, "Probe for"));
        assert_eq!(trait_targets, [target("Probe", 0)]);
    }

    #[test]
    fn test_binding_path_chain_beyond_bound_skips_reference() {
        let segments = vec!["seg"; 70].join("::");
        let text = format!("fn h() {{ {segments}; }}\n");
        let document = analyze("src/lib.rs", &text);
        let facts = document.binding().expect("binding facts extracted");
        let at = offset(&text, "seg");
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.range().start() != at),
            "a chain past the walk bound forms no reference"
        );
    }

    #[test]
    fn test_binding_anchor_keyword_after_front_skips_reference() {
        let text = "fn h() { self::super::x; }\n";
        let document = analyze("src/lib.rs", text);
        let facts = document.binding().expect("binding facts extracted");
        let at = offset(text, "self::super");
        assert!(
            facts
                .references()
                .iter()
                .all(|reference| reference.range().start() != at),
            "an anchor keyword after the front forms no reference"
        );
    }

    #[test]
    fn test_binding_pub_self_visibility_narrows_to_private() {
        let text = "mod outer { pub(self) fn hidden() {} }\nfn h() { outer::hidden(); }\n";
        let document = analyze("src/lib.rs", text);
        let (graph, set) = resolved(&[("src/lib.rs", &document)]);
        let hidden = targets_at(&graph, &set, "src/lib.rs", offset(text, "outer::hidden"));
        assert!(hidden.is_empty(), "pub(self) narrows to private");
    }
}
