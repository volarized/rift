//! Native declarations from C, C++, and Cython grammars.

use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::{ShippedLanguage, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource};
use rift_error::RiftError;
use rift_protocol::read::{Language, NodeFacet, SymbolFacet};
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
    typed_name: Option<u16>,
    c_function: Option<u16>,
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
            typed_name: cython.then(|| crate::parse::kind(&grammar, "maybe_typed_name")),
            c_function: cython.then(|| crate::parse::kind(&grammar, "c_function_definition")),
        }
    }

    fn declared_name<'tree>(&self, node: Node<'tree>, depth_max: usize) -> Option<Node<'tree>> {
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
            if node.kind_id() == self.identifier {
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
}

impl NativeRules {
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
        let node = visited.node();
        let Some((_, mut kind)) = self
            .kinds
            .declarations
            .iter()
            .find(|(id, _)| *id == node.kind_id())
            .copied()
        else {
            return Ok(None);
        };
        let Some(name_node) = self.declaration_name(node, kind) else {
            return Ok(None);
        };
        let Some(name) = text
            .get(name_node.byte_range())
            .filter(|name| !name.is_empty())
        else {
            return Ok(None);
        };
        if self.cython && kind == VARIABLE {
            let mut cursor = node.walk();
            if node
                .named_children(&mut cursor)
                .any(|child| Some(child.kind_id()) == self.kinds.c_function)
            {
                kind = FUNCTION;
            }
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
        visited.node().start_byte()
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
