//! CSS selectors, properties, and at-rules from the shipped grammar.

use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::{ShippedLanguage, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource};
use rift_error::RiftError;
use rift_protocol::read::{Language, NodeFacet};
use std::sync::OnceLock;
use tree_sitter::{Language as Grammar, Node, Range};

const SELECTOR: &str = "selector";
const PROPERTY: &str = "property";
const AT_RULE: &str = "at_rule";

/// Bounded syntax provider for CSS stylesheets.
#[derive(Debug, Clone)]
pub struct CssSyntaxProvider {
    language: Language,
}
impl Default for CssSyntaxProvider {
    fn default() -> Self {
        Self {
            language: ShippedLanguage::Css.language(),
        }
    }
}
impl SyntaxProvider for CssSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        analyze(source, limits, &[])
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            "rule_set" | "declaration" | "at_rule" => vec![NodeFacet::Declaration],
            "block" => vec![NodeFacet::Block],
            "comment" => vec![NodeFacet::Comment],
            "string_value" | "integer_value" | "float_value" | "color_value" => {
                vec![NodeFacet::Literal]
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) fn css_grammar() -> Grammar {
    tree_sitter_css::LANGUAGE.into()
}

pub(crate) fn analyze_included(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    range: Range,
) -> Result<SyntaxDocument, RiftError> {
    analyze(source, limits, &[range])
}

fn analyze(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    included: &[Range],
) -> Result<SyntaxDocument, RiftError> {
    let rules = CssRules { kinds: kinds() };
    crate::parse::document(
        source,
        limits,
        &ShippedLanguage::Css.language(),
        &css_grammar(),
        &rules,
        included,
    )
}

#[derive(Debug)]
struct CssKinds {
    rule: u16,
    declaration: u16,
    at_rule: u16,
    selectors: u16,
    property: u16,
    keyword: u16,
}
fn kinds() -> &'static CssKinds {
    static KINDS: OnceLock<CssKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = css_grammar();
        CssKinds {
            rule: crate::parse::kind(&grammar, "rule_set"),
            declaration: crate::parse::kind(&grammar, "declaration"),
            at_rule: crate::parse::kind(&grammar, "at_rule"),
            selectors: crate::parse::kind(&grammar, "selectors"),
            property: crate::parse::kind(&grammar, "property_name"),
            keyword: crate::parse::kind(&grammar, "at_keyword"),
        }
    })
}
struct CssRules {
    kinds: &'static CssKinds,
}
impl CssRules {
    fn named_child<'tree>(&self, node: Node<'tree>) -> Option<(Node<'tree>, &'static str)> {
        let (wanted, kind) = match node.kind_id() {
            id if id == self.kinds.rule => (self.kinds.selectors, SELECTOR),
            id if id == self.kinds.declaration => (self.kinds.property, PROPERTY),
            id if id == self.kinds.at_rule => (self.kinds.keyword, AT_RULE),
            _ => return None,
        };
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind_id() == wanted)
            .map(|child| (child, kind))
    }
}
impl GrammarRules for CssRules {
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let Some((child, kind)) = self.named_child(visited.node()) else {
            return Ok(None);
        };
        let Some(name) = text.get(child.byte_range()).filter(|name| !name.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind,
            facets: Vec::new(),
            visibility: None,
            body_range: None,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        }))
    }
    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        if node.kind_id() != self.kinds.rule {
            return None;
        }
        self.named_child(node)
            .and_then(|(child, _)| text.get(child.byte_range()))
            .map(str::to_owned)
    }
    fn declaration_start(&self, visited: Visited<'_, '_>, _: &str) -> usize {
        visited.node().start_byte()
    }
    fn name_range(&self, node: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        self.named_child(node)
            .map(|(child, _)| extract::byte_range(child))
            .transpose()
    }
    fn qualification_separator(&self) -> &'static str {
        " > "
    }
}
pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    [SELECTOR, PROPERTY, AT_RULE]
        .into_iter()
        .find(|kind| *kind == name)
}
