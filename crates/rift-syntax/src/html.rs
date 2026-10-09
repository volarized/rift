//! HTML elements and attributes from the shipped grammar.

use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::{ShippedLanguage, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource};
use rift_error::RiftError;
use rift_protocol::read::{Language, NodeFacet};
use std::sync::OnceLock;
use tree_sitter::{Language as Grammar, Node};

const ELEMENT: &str = "element";
const ATTRIBUTE: &str = "attribute";

/// Bounded syntax provider for HTML and embedded scripts and styles.
#[derive(Debug, Clone)]
pub struct HtmlSyntaxProvider {
    language: Language,
}
impl Default for HtmlSyntaxProvider {
    fn default() -> Self {
        Self {
            language: ShippedLanguage::Html.language(),
        }
    }
}
impl SyntaxProvider for HtmlSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        let document = analyze_base(source, limits, &self.language)?;
        crate::embedded::append(source, limits, &document)
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            "element" | "script_element" | "style_element" | "attribute" => {
                vec![NodeFacet::Declaration]
            }
            "comment" => vec![NodeFacet::Comment],
            "attribute_value" | "text" => vec![NodeFacet::Literal],
            "tag_name" | "attribute_name" => vec![NodeFacet::Identifier],
            _ => Vec::new(),
        }
    }
}
pub(crate) fn html_grammar() -> Grammar {
    tree_sitter_html::LANGUAGE.into()
}
pub(crate) fn analyze_base(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
) -> Result<SyntaxDocument, RiftError> {
    crate::parse::document(
        source,
        limits,
        language,
        &html_grammar(),
        &HtmlRules { kinds: kinds() },
        &[],
    )
}
#[derive(Debug)]
struct HtmlKinds {
    elements: [u16; 3],
    attribute: u16,
    tag: u16,
    name: u16,
}
fn kinds() -> &'static HtmlKinds {
    static KINDS: OnceLock<HtmlKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = html_grammar();
        HtmlKinds {
            elements: ["element", "script_element", "style_element"]
                .map(|name| crate::parse::kind(&grammar, name)),
            attribute: crate::parse::kind(&grammar, "attribute"),
            tag: crate::parse::kind(&grammar, "tag_name"),
            name: crate::parse::kind(&grammar, "attribute_name"),
        }
    })
}
struct HtmlRules {
    kinds: &'static HtmlKinds,
}
impl HtmlRules {
    fn name<'tree>(&self, node: Node<'tree>) -> Option<(Node<'tree>, &'static str)> {
        let (parent, wanted, kind) = if self.kinds.elements.contains(&node.kind_id()) {
            (node.named_child(0)?, self.kinds.tag, ELEMENT)
        } else if node.kind_id() == self.kinds.attribute {
            (node, self.kinds.name, ATTRIBUTE)
        } else {
            return None;
        };
        let mut cursor = parent.walk();
        parent
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == wanted)
            .map(|child| (child, kind))
    }
}
impl GrammarRules for HtmlRules {
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let Some((child, kind)) = self.name(visited.node()) else {
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
        if !self.kinds.elements.contains(&node.kind_id()) {
            return None;
        }
        self.name(node)
            .and_then(|(child, _)| text.get(child.byte_range()))
            .map(str::to_owned)
    }
    fn declaration_start(&self, visited: Visited<'_, '_>, _: &str) -> usize {
        visited.node().start_byte()
    }
    fn name_range(&self, node: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        self.name(node)
            .map(|(child, _)| extract::byte_range(child))
            .transpose()
    }
    fn qualification_separator(&self) -> &'static str {
        " > "
    }
}
pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    [ELEMENT, ATTRIBUTE].into_iter().find(|kind| *kind == name)
}
