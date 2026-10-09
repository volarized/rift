//! Native declarations from C, C++, and Cython grammars.

use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::{ShippedLanguage, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource};
use rift_error::RiftError;
use rift_protocol::read::{Language, NodeFacet, SymbolFacet};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::OnceLock;
use tree_sitter::{Language as Grammar, Node};

const FUNCTION: &str = "function";
const STRUCT: &str = "struct";
const CLASS: &str = "class";
const ENUM: &str = "enum";
const NAMESPACE: &str = "namespace";
const VARIABLE: &str = "variable";
const TYPE: &str = "type";
const MACRO: &str = "macro";
const INCLUDE: &str = "include";

/// Bounded syntax provider for C, C++, or Cython sources.
#[derive(Debug, Clone)]
pub(crate) struct NativeSyntaxProvider {
    shipped: ShippedLanguage,
    language: Language,
}

impl NativeSyntaxProvider {
    pub(crate) fn new(shipped: ShippedLanguage) -> Self {
        assert!(
            matches!(
                shipped,
                ShippedLanguage::C | ShippedLanguage::Cpp | ShippedLanguage::Cython
            ),
            "native provider requires a native grammar: shipped={shipped:?}"
        );
        Self {
            shipped,
            language: shipped.language(),
        }
    }
}

impl SyntaxProvider for NativeSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        let grammar = grammar(self.shipped);
        let rules = NativeRules {
            kinds: kinds(self.shipped),
            cython: self.shipped == ShippedLanguage::Cython,
            depth_max: limits.syntax_depth_max(),
            carriers: RefCell::new(BTreeMap::new()),
            carriers_left: Cell::new(limits.syntax_nodes_max()),
            cython_functions: RefCell::new(BTreeMap::new()),
        };
        crate::parse::document(source, limits, &self.language, &grammar, &rules, &[])
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            "function_definition"
            | "class_definition"
            | "struct_specifier"
            | "class_specifier"
            | "enum_specifier"
            | "declaration"
            | "cvar_def"
            | "cvar_decl" => vec![NodeFacet::Declaration],
            "preproc_include" | "include_statement" => vec![NodeFacet::Import],
            "comment" => vec![NodeFacet::Comment],
            _ => Vec::new(),
        }
    }
}

pub(crate) fn grammar(shipped: ShippedLanguage) -> Grammar {
    match shipped {
        ShippedLanguage::C => tree_sitter_c::LANGUAGE.into(),
        ShippedLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        ShippedLanguage::Cython => tree_sitter_cython::language(),
        _ => unreachable!("native grammar requires a native language"),
    }
}

#[derive(Debug)]
struct NativeKinds {
    declarations: Vec<(u16, &'static str)>,
    scopes: Vec<u16>,
    name: NonZeroU16,
    declarator: Option<NonZeroU16>,
    body: Option<NonZeroU16>,
    path: Option<NonZeroU16>,
    identifier: u16,
    declaration_identifiers: Vec<u16>,
    function_declarator: Option<u16>,
    value_declarators: Vec<u16>,
    typed_name: Option<u16>,
    c_function: Option<u16>,
    comma: Option<u16>,
    type_modifier: Option<u16>,
}

impl NativeKinds {
    fn resolve(shipped: ShippedLanguage) -> Self {
        let grammar = grammar(shipped);
        let declarations: &[(&str, &str)] = match shipped {
            ShippedLanguage::C => &[
                ("function_definition", FUNCTION),
                ("struct_specifier", STRUCT),
                ("union_specifier", STRUCT),
                ("enum_specifier", ENUM),
                ("type_definition", TYPE),
                ("declaration", VARIABLE),
                ("preproc_def", MACRO),
                ("preproc_function_def", MACRO),
                ("preproc_include", INCLUDE),
            ],
            ShippedLanguage::Cpp => &[
                ("function_definition", FUNCTION),
                ("struct_specifier", STRUCT),
                ("union_specifier", STRUCT),
                ("class_specifier", CLASS),
                ("enum_specifier", ENUM),
                ("namespace_definition", NAMESPACE),
                ("type_definition", TYPE),
                ("alias_declaration", TYPE),
                ("declaration", VARIABLE),
                ("field_declaration", VARIABLE),
                ("preproc_def", MACRO),
                ("preproc_function_def", MACRO),
                ("preproc_include", INCLUDE),
            ],
            ShippedLanguage::Cython => &[
                ("function_definition", FUNCTION),
                ("class_definition", CLASS),
                ("cvar_def", VARIABLE),
                ("cvar_decl", VARIABLE),
                ("struct", STRUCT),
                ("cppclass", CLASS),
                ("enum", ENUM),
                ("include_statement", INCLUDE),
            ],
            _ => unreachable!("native kind table requires a native grammar"),
        };
        let scopes = declarations
            .iter()
            .filter(|(_, kind)| matches!(*kind, STRUCT | CLASS | ENUM | NAMESPACE))
            .map(|(name, _)| crate::parse::kind(&grammar, name))
            .collect();
        let cython = shipped == ShippedLanguage::Cython;
        Self {
            declarations: declarations
                .iter()
                .map(|(name, kind)| (crate::parse::kind(&grammar, name), *kind))
                .collect(),
            scopes,
            name: grammar
                .field_id_for_name("name")
                .expect("native grammar defines name field"),
            declarator: grammar.field_id_for_name("declarator"),
            body: grammar.field_id_for_name("body"),
            path: grammar.field_id_for_name("path"),
            identifier: crate::parse::kind(&grammar, "identifier"),
            declaration_identifiers: Self::declaration_identifiers(&grammar, cython),
            function_declarator: (!cython)
                .then(|| crate::parse::kind(&grammar, "function_declarator")),
            value_declarators: Self::value_declarators(&grammar, shipped),
            typed_name: cython.then(|| crate::parse::kind(&grammar, "maybe_typed_name")),
            c_function: cython.then(|| crate::parse::kind(&grammar, "c_function_definition")),
            comma: cython.then(|| {
                let id = grammar.id_for_node_kind(",", false);
                assert!(
                    id != 0,
                    "shipped grammar must define extraction kind: name=,"
                );
                id
            }),
            type_modifier: cython.then(|| crate::parse::kind(&grammar, "type_modifier")),
        }
    }

    fn declaration_identifiers(grammar: &Grammar, cython: bool) -> Vec<u16> {
        let names: &[&str] = if cython {
            &["identifier"]
        } else {
            &["identifier", "field_identifier", "type_identifier"]
        };
        names
            .iter()
            .map(|name| crate::parse::kind(grammar, name))
            .collect()
    }

    fn value_declarators(grammar: &Grammar, shipped: ShippedLanguage) -> Vec<u16> {
        let names: &[&str] = match shipped {
            ShippedLanguage::C => &["pointer_declarator", "array_declarator"],
            ShippedLanguage::Cpp => &[
                "pointer_declarator",
                "array_declarator",
                "reference_declarator",
            ],
            _ => &[],
        };
        names
            .iter()
            .map(|name| crate::parse::kind(grammar, name))
            .collect()
    }

    fn declared_name<'tree>(&self, node: Node<'tree>, depth_max: usize) -> Option<Node<'tree>> {
        if self.declaration_identifiers.contains(&node.kind_id()) {
            return Some(node);
        }
        if let Some(name) = node.child_by_field_id(self.name.get()) {
            return Some(name);
        }
        if let Some(declarator) = self
            .declarator
            .and_then(|field| node.child_by_field_id(field.get()))
        {
            return self.declarator_name(declarator, depth_max);
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor).find_map(|child| {
            if Some(child.kind_id()) == self.typed_name {
                child.child_by_field_id(self.name.get())
            } else if child.kind_id() == self.identifier {
                Some(child)
            } else {
                child.child_by_field_id(self.name.get())
            }
        })
    }

    fn declarator_name<'tree>(
        &self,
        mut node: Node<'tree>,
        depth_max: usize,
    ) -> Option<Node<'tree>> {
        for _ in 0..depth_max {
            if self.declaration_identifiers.contains(&node.kind_id()) {
                return Some(node);
            }
            if let Some(name) = node.child_by_field_id(self.name.get()) {
                return Some(name);
            }
            node = self
                .declarator
                .and_then(|field| node.child_by_field_id(field.get()))
                .or_else(|| node.named_child(0))?;
        }
        None
    }

    fn function_declaration(
        &self,
        name: Node<'_>,
        declaration: Node<'_>,
        depth_max: usize,
    ) -> bool {
        let mut node = name;
        for _ in 0..depth_max {
            if node == declaration {
                return false;
            }
            if Some(node.kind_id()) == self.function_declarator {
                return true;
            }
            if self.value_declarators.contains(&node.kind_id()) {
                return false;
            }
            let Some(parent) = node.parent() else {
                return false;
            };
            node = parent;
        }
        false
    }
}

fn kinds(shipped: ShippedLanguage) -> &'static NativeKinds {
    static C: OnceLock<NativeKinds> = OnceLock::new();
    static CPP: OnceLock<NativeKinds> = OnceLock::new();
    static CYTHON: OnceLock<NativeKinds> = OnceLock::new();
    match shipped {
        ShippedLanguage::C => C.get_or_init(|| NativeKinds::resolve(shipped)),
        ShippedLanguage::Cpp => CPP.get_or_init(|| NativeKinds::resolve(shipped)),
        ShippedLanguage::Cython => CYTHON.get_or_init(|| NativeKinds::resolve(shipped)),
        _ => unreachable!("native kind table requires a native language"),
    }
}

struct NativeRules {
    kinds: &'static NativeKinds,
    cython: bool,
    depth_max: usize,
    carriers: RefCell<BTreeMap<usize, BTreeSet<usize>>>,
    carriers_left: Cell<usize>,
    cython_functions: RefCell<BTreeMap<usize, bool>>,
}

impl NativeRules {
    fn declaration_kind(&self, node: Node<'_>) -> Option<&'static str> {
        self.kinds
            .declarations
            .iter()
            .find_map(|(id, kind)| (*id == node.kind_id()).then_some(*kind))
    }

    fn declaration_parent<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        if self.declaration_kind(node).is_some() {
            return None;
        }
        let parent = node.parent()?;
        let kind = self.declaration_kind(parent)?;
        if !matches!(kind, VARIABLE | TYPE) {
            return None;
        }
        let mut carriers = self.carriers.borrow_mut();
        let owned = carriers
            .entry(parent.id())
            .or_insert_with(|| self.additional_carriers(parent));
        owned.contains(&node.id()).then_some(parent)
    }

    fn additional_carriers(&self, parent: Node<'_>) -> BTreeSet<usize> {
        let first = self.kinds.declared_name(parent, self.depth_max);
        let mut cursor = parent.walk();
        let mut carriers = BTreeSet::new();
        if !cursor.goto_first_child() {
            return carriers;
        }
        loop {
            if self.carriers_left.get() == 0 {
                break;
            }
            let child = cursor.node();
            let owned = if self.cython {
                child.kind_id() == self.kinds.identifier && self.follows_comma(child)
            } else {
                cursor.field_id() == self.kinds.declarator
            };
            if owned
                && self
                    .kinds
                    .declared_name(child, self.depth_max)
                    .is_some_and(|name| Some(name) != first)
            {
                carriers.insert(child.id());
                self.carriers_left.set(self.carriers_left.get() - 1);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        carriers
    }

    fn follows_comma(&self, node: Node<'_>) -> bool {
        let mut sibling = node.prev_sibling();
        for _ in 0..self.depth_max {
            let Some(previous) = sibling else {
                return false;
            };
            if Some(previous.kind_id()) == self.kinds.comma {
                return true;
            }
            if Some(previous.kind_id()) != self.kinds.type_modifier {
                return false;
            }
            sibling = previous.prev_sibling();
        }
        false
    }

    fn declaration_name<'tree>(&self, node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
        if kind == INCLUDE {
            return self
                .kinds
                .path
                .and_then(|field| node.child_by_field_id(field.get()))
                .or_else(|| node.named_child(0));
        }
        self.kinds.declared_name(node, self.depth_max)
    }
}

impl GrammarRules for NativeRules {
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let carrier = visited.node();
        let node = self.declaration_parent(carrier).unwrap_or(carrier);
        let Some(mut kind) = self.declaration_kind(node) else {
            return Ok(None);
        };
        let Some(name_node) = self.declaration_name(carrier, kind) else {
            return Ok(None);
        };
        let Some(name) = text
            .get(name_node.byte_range())
            .filter(|name| !name.is_empty())
        else {
            return Ok(None);
        };
        if self.cython && kind == VARIABLE {
            let mut functions = self.cython_functions.borrow_mut();
            let function = functions.entry(node.id()).or_insert_with(|| {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .any(|child| Some(child.kind_id()) == self.kinds.c_function)
            });
            if *function {
                kind = FUNCTION;
            }
        } else if kind == VARIABLE
            && self
                .kinds
                .function_declaration(name_node, node, self.depth_max)
        {
            kind = FUNCTION;
        }
        let facets = match kind {
            FUNCTION => vec![SymbolFacet::Value, SymbolFacet::Callable],
            STRUCT | CLASS | ENUM | TYPE => vec![SymbolFacet::Type],
            VARIABLE => vec![SymbolFacet::Value],
            _ => Vec::new(),
        };
        let body_range = self
            .kinds
            .body
            .and_then(|field| node.child_by_field_id(field.get()))
            .map(extract::byte_range)
            .transpose()?;
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind,
            facets,
            visibility: None,
            body_range,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        }))
    }
    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        self.kinds
            .scopes
            .contains(&node.kind_id())
            .then(|| {
                self.kinds
                    .declared_name(node, self.depth_max)
                    .and_then(|name| text.get(name.byte_range()))
                    .map(str::to_owned)
            })
            .flatten()
    }
    fn declaration_start(&self, visited: Visited<'_, '_>, _: &str) -> usize {
        self.declaration_node(visited.node()).start_byte()
    }

    fn declaration_node<'tree>(&self, node: Node<'tree>) -> Node<'tree> {
        self.declaration_parent(node).unwrap_or(node)
    }
    fn name_range(&self, node: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        self.kinds
            .declared_name(node, self.depth_max)
            .map(extract::byte_range)
            .transpose()
    }
    fn qualification_separator(&self) -> &'static str {
        "::"
    }
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    [
        FUNCTION, STRUCT, CLASS, ENUM, NAMESPACE, VARIABLE, TYPE, MACRO, INCLUDE,
    ]
    .into_iter()
    .find(|kind| *kind == name)
}
