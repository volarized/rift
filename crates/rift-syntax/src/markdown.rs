//! Markdown syntax facts from the pinned tree-sitter-md block grammar.
//!
//! Only the block grammar is read; inline content stays raw bytes -
//! `**bold**` in a heading is part of the name - so the inline grammar
//! never loads. Each named heading declares one `heading` symbol.
//!
//! Decisions this module fixes:
//! - Nesting follows the grammar's section tree. The pinned block grammar
//!   opens and closes `section` nodes at ATX headings, one nesting level per
//!   heading level; a section starting with a heading declares that
//!   heading's symbol and spans the whole section, subsections included. A
//!   setext heading never closes a section: one that starts a section
//!   declares it, and any later setext heading in the same section stays a
//!   loose child - it declares from its own node, spans only its own lines,
//!   owns no `body_range`, and files under the section that holds it,
//!   whatever its underline level, so containers nest exactly as ranges
//!   nest.
//! - A symbol's `name` is the heading text: the content after an ATX marker
//!   with an optional closing `#` run removed, or a setext heading's content
//!   line, trimmed of surrounding whitespace only. A heading with no content
//!   (`#` alone) names nothing and emits no symbol.
//! - Qualified names join nested headings with ` > ` (`Install > Requirements`).
//!   Heading text may itself contain `/`, and a symbol address splits its
//!   path from its qualified name at the last `/` while the identity escape
//!   set keeps `/` literal - a `/`-joined heading path would not parse back.
//!   Every byte of ` > ` escapes in the minted address, so the address
//!   round-trips, and the spelling cannot be mistaken for path structure.
//! - Nothing attaches in front of a declaration, so `range` equals
//!   `item_range`. A declaring section's `body_range` is the section minus
//!   its heading line, absent for a section that is only a heading.
//! - Headings carry no visibility and no portable symbol facet: `visibility`
//!   stays `None` and `facets` stays empty.
//! - A document with no headings emits no symbols; its nodes still serve.
//! - `signatures` and `documentation` stay empty for every heading: a
//!   heading has no callable form, and its `body_range` is the section's own
//!   prose, not a preceding comment, so there is nothing to attach as
//!   documentation.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet};
use tree_sitter::{Node, Parser};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Grammar spelling of a `section`, one nesting level per heading.
const SECTION_KIND: &str = "section";
/// Grammar spelling of an `atx_heading` (`# Title`).
const ATX_HEADING_KIND: &str = "atx_heading";
/// Grammar spelling of a `setext_heading` (a content line over `===` or `---`).
const SETEXT_HEADING_KIND: &str = "setext_heading";
/// Grammar field holding a heading's content: the inline node of an ATX
/// heading, the content paragraph of a setext heading. Absent on a heading
/// with no content.
const HEADING_CONTENT_FIELD: &str = "heading_content";

/// The one markdown kind word behind the wire kind `markdown.heading`.
const HEADING_KIND_WORD: &str = "heading";

/// The separator markdown qualified names join nested headings with.
const HEADING_QUALIFICATION_SEPARATOR: &str = " > ";

/// Numeric grammar ids for every kind and field this module reads, resolved
/// once so each walk decision compares integers.
#[derive(Debug)]
struct MarkdownKinds {
    section: u16,
    atx_heading: u16,
    setext_heading: u16,
    heading_content: NonZeroU16,
}

impl MarkdownKinds {
    /// Resolves the block grammar's heading vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            section: kind_id(language, SECTION_KIND),
            atx_heading: kind_id(language, ATX_HEADING_KIND),
            setext_heading: kind_id(language, SETEXT_HEADING_KIND),
            heading_content: field_id(language, HEADING_CONTENT_FIELD),
        }
    }
}

/// Resolves one node kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned markdown grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one grammar field id, proving the pinned grammar defines it.
fn field_id(language: &tree_sitter::Language, field: &str) -> NonZeroU16 {
    language.field_id_for_name(field).unwrap_or_else(|| {
        panic!(
            "pinned markdown grammar must define field used by symbol \
             extraction: field={field}"
        )
    })
}

/// The block grammar's decisions for the shared bounded walk.
#[derive(Debug)]
struct MarkdownRules {
    kinds: &'static MarkdownKinds,
}

impl MarkdownRules {
    /// The heading a `section` declares: its first named child, when that
    /// child is a heading. `None` for a headingless section - leading
    /// content before the first heading, or a whole document with no
    /// headings.
    fn declaring_heading<'tree>(&self, section: Node<'tree>) -> Option<Node<'tree>> {
        let first = section.named_child(0)?;
        let heading = first.kind_id() == self.kinds.atx_heading
            || first.kind_id() == self.kinds.setext_heading;
        heading.then_some(first)
    }

    /// Whether `heading` is the one its parent section declares. A loose
    /// setext heading - one the grammar left mid-section - is not, and
    /// declares from its own node instead.
    fn declares_its_section(&self, heading: Node<'_>) -> bool {
        heading
            .parent()
            .and_then(|parent| self.declaring_heading(parent))
            .is_some_and(|declaring| declaring.id() == heading.id())
    }

    /// The heading's name: its content text trimmed of structural markers
    /// and surrounding whitespace. `None` for a heading naming nothing.
    fn heading_name(&self, heading: Node<'_>, text: &str) -> Option<String> {
        let content = heading.child_by_field_id(self.kinds.heading_content.get())?;
        let content = text.get(content.byte_range())?;
        let name = if heading.kind_id() == self.kinds.atx_heading {
            without_closing_hash_run(content.trim())
        } else {
            content.trim()
        };
        (!name.is_empty()).then(|| name.to_owned())
    }
}

/// The section's content span past its heading line; `None` when the
/// section is only a heading.
fn section_body_range(
    section: Node<'_>,
    heading: Node<'_>,
) -> Result<Option<ByteRange>, SyntaxError> {
    let section_range = extract::byte_range(section)?;
    let heading_range = extract::byte_range(heading)?;
    Ok(
        (heading_range.end < section_range.end).then_some(ByteRange {
            start: heading_range.end,
            end: section_range.end,
        }),
    )
}

/// An ATX heading's content without its optional closing `#` run. The run
/// counts as a marker only when whitespace separates it from the text
/// (`## Title ##`) or it is the whole content; a `#` touching the text
/// (`# C#`) is part of the name.
fn without_closing_hash_run(content: &str) -> &str {
    let kept = content.trim_end_matches('#');
    if kept.is_empty() || kept.ends_with(char::is_whitespace) {
        kept.trim_end()
    } else {
        content
    }
}

impl MarkdownRules {
    /// The declaration of a section starting with a heading: named by that
    /// heading, spanning the walker's node range, with the content past the
    /// heading line as the body.
    fn section_declaration(
        &self,
        section: Node<'_>,
        text: &str,
    ) -> Result<Option<Declaration>, SyntaxError> {
        let Some(heading) = self.declaring_heading(section) else {
            return Ok(None);
        };
        let Some(name) = self.heading_name(heading, text) else {
            return Ok(None);
        };
        Ok(Some(Declaration {
            name,
            kind: HEADING_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range: section_body_range(section, heading)?,
            documentation: Vec::new(),
        }))
    }

    /// The declaration of a loose setext heading, spanning only its own
    /// lines: the grammar keeps no content under it, so it has no body.
    fn loose_setext_declaration(&self, heading: Node<'_>, text: &str) -> Option<Declaration> {
        if self.declares_its_section(heading) {
            return None;
        }
        let name = self.heading_name(heading, text)?;
        Some(Declaration {
            name,
            kind: HEADING_KIND_WORD,
            facets: Vec::new(),
            visibility: None,
            body_range: None,
            documentation: Vec::new(),
        })
    }
}

impl GrammarRules for MarkdownRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        if node.kind_id() == self.kinds.section {
            return self.section_declaration(node, text);
        }
        if node.kind_id() == self.kinds.setext_heading {
            return Ok(self.loose_setext_declaration(node, text));
        }
        Ok(None)
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        if node.kind_id() != self.kinds.section {
            return None;
        }
        let heading = self.declaring_heading(node)?;
        self.heading_name(heading, text)
    }

    /// A declaration starts at its own node: nothing attaches in front.
    fn declaration_start(&self, node: Node<'_>, _text: &str) -> usize {
        node.start_byte()
    }

    fn qualification_separator(&self) -> &'static str {
        HEADING_QUALIFICATION_SEPARATOR
    }
}

/// Bounded Tree-sitter markdown fact provider.
#[derive(Debug, Clone)]
pub struct MarkdownSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl MarkdownSyntaxProvider {
    /// File extensions this provider parses, without their leading dot.
    pub const SOURCE_EXTENSIONS: &'static [&'static str] = &["md"];

    /// Default maximum bytes this provider accepts from one markdown source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "markdown".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The markdown provider's declared default bounds, proven positive at
/// compile time.
const MARKDOWN_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    MarkdownSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for MarkdownSyntaxProvider {
    fn default() -> Self {
        Self::new(MARKDOWN_SYNTAX_LIMITS_DEFAULT)
    }
}

impl SyntaxProvider for MarkdownSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn extensions(&self) -> &'static [&'static str] {
        Self::SOURCE_EXTENSIONS
    }

    fn source_bytes_max(&self) -> usize {
        self.limits.source_bytes_max()
    }

    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
        if source.text.len() > self.limits.source_bytes_max() {
            return Err(Error::new(SyntaxFault::SourceTooLarge {
                path: Some(source.path.clone()),
                source_bytes: source.text.len(),
                source_bytes_max: self.limits.source_bytes_max(),
            }));
        }
        let grammar = markdown_grammar();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|_| incompatible_grammar(&grammar))?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let rules = MarkdownRules {
            kinds: markdown_kinds(),
        };
        let (nodes, symbols) = extract::extract(
            tree.root_node(),
            source,
            self.limits,
            &self.language,
            &rules,
        )?;
        Ok(SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            nodes,
            symbols,
            tree.root_node().has_error(),
        ))
    }

    /// Portable structural facets for one block grammar node kind. Most
    /// markdown kinds are prose structure no portable facet describes and
    /// carry none; the mapping covers the kinds with an honest portable
    /// reading.
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        match kind {
            // The section declares its heading's name and the heading spells
            // it; a link reference definition introduces a label other
            // content refers to.
            SECTION_KIND | ATX_HEADING_KIND | SETEXT_HEADING_KIND | "link_reference_definition" => {
                vec![NodeFacet::Declaration]
            }
            // Content the document carries verbatim, uninterpreted by the
            // block grammar.
            "fenced_code_block" | "indented_code_block" | "html_block" => vec![NodeFacet::Literal],
            // The info string qualifies its fenced code block, as `rust`
            // does on a fence.
            "info_string" => vec![NodeFacet::Annotation],
            _ => Vec::new(),
        }
    }
}

fn markdown_grammar() -> tree_sitter::Language {
    tree_sitter_md::LANGUAGE.into()
}

/// Returns the process-wide resolved markdown kind table, computing it once.
fn markdown_kinds() -> &'static MarkdownKinds {
    static KINDS: OnceLock<MarkdownKinds> = OnceLock::new();
    KINDS.get_or_init(|| MarkdownKinds::resolve(&markdown_grammar()))
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;
    use crate::failure::SyntaxViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("docs/guide.md").expect("valid fixture path")
    }

    fn analyze(text: &str) -> SyntaxDocument {
        MarkdownSyntaxProvider::default()
            .analyze(SyntaxSource {
                path: &path(),
                text,
            })
            .expect("markdown fixture must parse")
    }

    /// Resolution asserts every kind and field id is non-zero, so resolving
    /// the pinned grammar's table is the proof the vocabulary exists.
    #[test]
    fn test_kind_table_resolves_on_the_pinned_grammar() {
        let kinds = MarkdownKinds::resolve(&markdown_grammar());
        assert_ne!(kinds.section, 0);
        assert_ne!(kinds.atx_heading, 0);
        assert_ne!(kinds.setext_heading, 0);
    }

    #[test]
    #[should_panic(expected = "must define node kind used by symbol extraction: \
                               kind=no_such_kind")]
    fn test_kind_resolution_refuses_a_kind_the_grammar_lacks() {
        let _ = kind_id(&markdown_grammar(), "no_such_kind");
    }

    #[test]
    #[should_panic(expected = "must define field used by symbol extraction: \
                               field=no_such_field")]
    fn test_field_resolution_refuses_a_field_the_grammar_lacks() {
        let _ = field_id(&markdown_grammar(), "no_such_field");
    }

    #[test]
    fn test_provider_declares_language_extensions_and_byte_bound() {
        let provider = MarkdownSyntaxProvider::default();
        assert_eq!(provider.language().name, "markdown");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(provider.extensions(), ["md"]);
        assert_eq!(
            provider.source_bytes_max(),
            MarkdownSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }

    /// ATX headings h1 through h6 nest one section per level: each symbol's
    /// container is the full path of the headings above it.
    #[test]
    fn test_atx_headings_nest_through_all_six_levels() {
        let text = "# A\n\n## B\n\n### C\n\n#### D\n\n##### E\n\n###### F\n\nleaf\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.qualified_name.as_str(),
                    symbol.container.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("A", "A", None),
                ("B", "A > B", Some("A")),
                ("C", "A > B > C", Some("A > B")),
                ("D", "A > B > C > D", Some("A > B > C")),
                ("E", "A > B > C > D > E", Some("A > B > C > D")),
                ("F", "A > B > C > D > E > F", Some("A > B > C > D > E")),
            ]
        );
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.kind == "heading"
                    && symbol.facets.is_empty()
                    && symbol.visibility.is_none()),
            "every heading files under the one markdown kind with no facets and no visibility"
        );
        assert!(!document.has_errors());
    }

    /// A section skipping levels nests under the section that holds it: an
    /// h3 directly inside an h1 files under the h1.
    #[test]
    fn test_level_skip_nests_under_the_holding_section() {
        let document = analyze("# Top\n\n### Deep\n\nBody.\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Top", "Top > Deep"]);
    }

    /// Section spans are the grammar's own byte facts: the whole section for
    /// `range` and `item_range`, the content past the heading line for
    /// `body_range`.
    #[test]
    fn test_section_ranges_span_heading_through_content() {
        let text = "# Install\n\nIntro text.\n\n## Requirements\n\nA rust toolchain.\n\n\
                    ### Hardware\n\nAny.\n\n## Steps\n\nRun it.\n\n# Usage\n\nCall it.\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.map(|body| (body.start, body.end)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("Install", (0, 99), Some((10, 99))),
                ("Install > Requirements", (24, 80), Some((40, 80))),
                (
                    "Install > Requirements > Hardware",
                    (60, 80),
                    Some((73, 80))
                ),
                ("Install > Steps", (80, 99), Some((89, 99))),
                ("Usage", (99, 117), Some((107, 117))),
            ]
        );
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.range == symbol.item_range),
            "nothing attaches in front of a heading, so range equals item_range"
        );
        assert_eq!(&text[73..80], "\nAny.\n\n");
        assert_eq!(&text[60..73], "### Hardware\n");
    }

    /// A heading with no content below it has no body.
    #[test]
    fn test_heading_only_section_has_no_body_range() {
        let document = analyze("# Lone\n");
        let symbol = &document.symbols()[0];
        assert_eq!(symbol.name, "Lone");
        assert_eq!(symbol.body_range, None);
        assert_eq!((symbol.range.start, symbol.range.end), (0, 7));
    }

    /// A setext heading that starts a section declares it, with the content
    /// past the underline as the body; the ATX heading that closes it opens
    /// a sibling section.
    #[test]
    fn test_setext_heading_starting_a_section_declares_it() {
        let text = "Install\n===\n\ncontent\n\n## Atx\n\nmore\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    symbol.container.as_deref(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.map(|body| (body.start, body.end)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("Install", None, (0, 22), Some((12, 22))),
                ("Atx", None, (22, 35), Some((29, 35))),
            ]
        );
        assert_eq!(&text[12..22], "\ncontent\n\n");
    }

    /// The pinned grammar never closes a section at a setext heading: a
    /// loose one declares from its own node, spans only its own lines, has
    /// no body, and files under the section that holds it.
    #[test]
    fn test_loose_setext_heading_files_under_its_holding_section() {
        let document = analyze("# Top\n\nUnder\n=====\n\nBody.\n");
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.qualified_name.as_str(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.is_some(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [("Top", (0, 26), true), ("Top > Under", (7, 19), false)]
        );
    }

    /// A run of setext headings shares one section: the first declares the
    /// section, the rest are loose children under it, whatever their
    /// underline level.
    #[test]
    fn test_setext_heading_run_files_under_the_first_heading() {
        let document = analyze("A\n===\n\nB\n===\n\nC\n---\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["A", "A > B", "A > C"]);
    }

    /// The heading text keeps its exact bytes: inline formatting and `/`
    /// stay literal, a closing `#` run is a marker only when whitespace
    /// separates it from the text, and an ATX heading with no content emits
    /// no symbol.
    #[test]
    fn test_heading_names_keep_exact_bytes_and_trim_only_markers() {
        let text = "# **bold** name\n\n## Title ##\n\n### C#\n\n#### CI/CD\n\n#\n";
        let document = analyze(text);
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["**bold** name", "Title", "C#", "CI/CD"]);
    }

    /// An ATX heading whose content is only `#` runs names nothing.
    #[test]
    fn test_all_hash_heading_emits_no_symbol() {
        let document = analyze("# ##\n\nBody.\n");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// A document with no headings emits no symbols; its nodes still serve.
    #[test]
    fn test_document_without_headings_serves_nodes_and_no_symbols() {
        let document = analyze("just prose\n\nmore prose\n");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
        assert!(
            document.nodes().iter().any(|node| node.kind == "paragraph"),
            "the parsed tree must still carry the prose structure"
        );
    }

    /// Markdown accepts almost anything as prose: punctuation soup parses
    /// without symbols, and a file that is prose with no trailing newline
    /// parses clean.
    #[test]
    fn test_prose_and_punctuation_parse_without_errors() {
        let soup = analyze("<<<<]]]*** ~~~ ??? \n\t*\n");
        assert!(!soup.has_errors());
        assert!(soup.symbols().is_empty());

        let unterminated = analyze("Beacon docs");
        assert!(!unterminated.has_errors());
        assert!(unterminated.symbols().is_empty());
    }

    /// The grammar requires a line ending after an ATX heading: a heading as
    /// the file's last bytes with no trailing newline is the malformed case,
    /// reported without dropping facts.
    #[test]
    fn test_provider_reports_malformed_tree_without_dropping_facts() {
        let document = analyze("# Lone");
        assert!(document.has_errors());
        assert!(!document.nodes().is_empty());
        assert!(document.symbols().is_empty());
    }

    /// CRLF sources keep byte-exact spans: heading names exclude `\r`, and
    /// every range lands on the CRLF file's own byte offsets.
    #[test]
    fn test_crlf_source_keeps_byte_exact_ranges() {
        let text = "# Title\r\n\r\nBody line.\r\n\r\n## Sub\r\nMore.\r\n";
        let document = analyze(text);
        let spans = document
            .symbols()
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    (symbol.range.start, symbol.range.end),
                    symbol.body_range.map(|body| (body.start, body.end)),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            spans,
            [
                ("Title", (0, 40), Some((9, 40))),
                ("Sub", (25, 40), Some((33, 40))),
            ]
        );
        assert_eq!(&text[33..40], "More.\r\n");
        assert!(!document.has_errors());
    }

    /// Repeated headings take distinct qualified names and keep their own
    /// spans: none keeps the bare heading path, each takes a `~N` suffix,
    /// the same policy every provider applies.
    #[test]
    fn test_repeated_headings_take_distinct_qualified_names_with_distinct_spans() {
        let document = analyze("# Notes\n\n## Sub\n\n# Notes\n\n## Sub\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["Notes~1", "Notes > Sub~1", "Notes~2", "Notes > Sub~2"]
        );
        assert_ne!(
            document.symbols()[0].range,
            document.symbols()[2].range,
            "the two sections keep their own spans"
        );
    }

    #[test]
    fn test_provider_enforces_source_node_and_depth_limits() {
        let bounded = |limits: SyntaxLimits, text: &str| {
            MarkdownSyntaxProvider::new(limits).analyze(SyntaxSource {
                path: &path(),
                text,
            })
        };
        let source_error = bounded(
            SyntaxLimits::new(3, 10, 10).expect("positive limits"),
            "# Over\n",
        )
        .expect_err("source bound");
        assert_eq!(
            source_error.fault().violation(),
            SyntaxViolation::SourceTooLarge
        );

        let node_error = bounded(
            SyntaxLimits::new(100, 1, 10).expect("positive limits"),
            "# Over\n",
        )
        .expect_err("node bound");
        assert_eq!(
            node_error.fault().violation(),
            SyntaxViolation::TooManyNodes
        );

        let depth_error = bounded(
            SyntaxLimits::new(100, 50, 1).expect("positive limits"),
            "# Over\n",
        )
        .expect_err("depth bound");
        assert_eq!(depth_error.fault().violation(), SyntaxViolation::TooDeep);
    }

    /// Deeply nested headings stay well inside the default depth budget.
    #[test]
    fn test_six_level_nesting_fits_the_default_depth_budget() {
        let document = analyze("# A\n\n## B\n\n### C\n\n#### D\n\n##### E\n\n###### F\n\n- x\n");
        assert_eq!(document.symbols().len(), 6);
        assert!(!document.has_errors());
    }

    #[test]
    fn test_empty_source_parses_with_no_symbols() {
        let document = analyze("");
        assert!(document.symbols().is_empty());
        assert!(!document.has_errors());
    }

    /// The declaring kinds carry the `Declaration` facet; verbatim content
    /// reads as `Literal`, an info string as `Annotation`, and prose
    /// structure carries none.
    #[test]
    fn test_node_facets_classify_the_interpreted_kinds() {
        let provider = MarkdownSyntaxProvider::default();
        for kind in [SECTION_KIND, ATX_HEADING_KIND, SETEXT_HEADING_KIND] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Declaration],
                "kind {kind} must classify as a declaration"
            );
        }
        assert_eq!(
            provider.node_facets("link_reference_definition"),
            [NodeFacet::Declaration]
        );
        for kind in ["fenced_code_block", "indented_code_block", "html_block"] {
            assert_eq!(
                provider.node_facets(kind),
                [NodeFacet::Literal],
                "kind {kind} must classify as verbatim content"
            );
        }
        assert_eq!(provider.node_facets("info_string"), [NodeFacet::Annotation]);
        for kind in [
            "document",
            "paragraph",
            "inline",
            "list",
            "list_item",
            "block_quote",
        ] {
            assert_eq!(
                provider.node_facets(kind),
                [],
                "prose kind {kind} carries no portable facet"
            );
        }
    }
}
