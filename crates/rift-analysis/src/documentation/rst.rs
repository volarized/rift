//! Bounded reStructuredText facts from the pinned Tree-sitter grammar.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use rift_core::{ProjectPath, line::line_of, line::line_starts};
use rift_protocol::read::TextRange;
use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser, Point, Tree};

use super::failure::{DocumentationError, DocumentationViolation, refused};

const SOURCE_BYTES_MAX: usize = 4 * 1_024 * 1_024;
const SYNTAX_NODES_MAX: usize = 250_000;
const SYNTAX_DEPTH_MAX: usize = 512;
const PROGRESS_CALLBACKS_MAX: usize = 131_072;

/// Prose or code range carried by one reStructuredText block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RstBlockKind {
    /// Authored text and markup.
    Prose,
    /// Authored code retained without execution.
    Code,
}

/// One heading in a block's ordered section path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RstHeading {
    /// Depth derived from first-seen adornment order.
    pub(crate) level: u32,
    /// Authored title with duplicate-heading suffix when needed.
    pub(crate) name: String,
}

/// One supported reStructuredText block.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RstBlockFact {
    /// Exact half-open source byte range.
    pub(crate) range: TextRange,
    /// One-based source line.
    pub(crate) line: u64,
    /// Prose or code range.
    pub(crate) kind: RstBlockKind,
    /// Parser structure name.
    pub(crate) structure: &'static str,
    /// Headings that own this block, in increasing depth order.
    pub(crate) heading_path: Vec<RstHeading>,
    /// Language declared by a code directive.
    pub(crate) language: Option<String>,
}

/// One authored reStructuredText link or local reference.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RstLinkFact {
    /// Block containing the authored link.
    pub(crate) block_range: TextRange,
    /// Exact source range of the authored destination text.
    pub(crate) range: TextRange,
    /// Exact authored destination text.
    pub(crate) authored: String,
    /// Reference name or direct destination.
    pub(crate) kind: RstLinkKind,
}

/// Authored reference form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RstLinkKind {
    /// Local reference name, resolved through explicit targets.
    Reference,
    /// Authored destination, retained without fetching it.
    Destination,
}

/// One explicit target defined in source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RstTargetFact {
    /// Exact authored target name.
    pub(crate) name: String,
    /// Exact source range of the target name.
    pub(crate) range: TextRange,
    /// Authored destination when the target names one.
    pub(crate) destination: Option<String>,
    /// Exact source range of the destination when present.
    pub(crate) destination_range: Option<TextRange>,
}

/// reStructuredText blocks and authored local reference facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RstFacts {
    /// Supported blocks in source order.
    pub(crate) blocks: Vec<RstBlockFact>,
    /// Authored destinations in source order.
    pub(crate) links: Vec<RstLinkFact>,
    /// Explicit targets in source order.
    pub(crate) targets: Vec<RstTargetFact>,
    /// Source ranges omitted by the supported syntax policy.
    pub(crate) omitted_ranges: Vec<TextRange>,
    /// Parser error ranges.
    pub(crate) error_ranges: Vec<TextRange>,
}

/// Extracts supported reStructuredText facts without executing directives.
pub(super) fn extract_rst_facts(
    text: &str,
    path: &ProjectPath,
) -> Result<RstFacts, DocumentationError> {
    extract_rst_facts_with_bounds(text, path, RstParseBounds::default())
}

#[derive(Clone, Copy, Debug)]
struct RstParseBounds {
    source_bytes: usize,
    node_count: usize,
    depth: usize,
    progress_callbacks: usize,
}

impl RstParseBounds {
    const fn default() -> Self {
        Self {
            source_bytes: SOURCE_BYTES_MAX,
            node_count: SYNTAX_NODES_MAX,
            depth: SYNTAX_DEPTH_MAX,
            progress_callbacks: PROGRESS_CALLBACKS_MAX,
        }
    }
}

fn extract_rst_facts_with_bounds(
    text: &str,
    _path: &ProjectPath,
    bounds: RstParseBounds,
) -> Result<RstFacts, DocumentationError> {
    if text.len() > bounds.source_bytes {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "source_bytes",
        ));
    }
    let tree = parse(text, bounds)?;
    let language: Language = tree_sitter_rst::LANGUAGE.into();
    let kinds = rst_kinds(&language);
    let entries = bounded_nodes(tree.root_node(), bounds)?;
    build_facts(text, &tree, &entries, kinds)
}

fn parse(text: &str, bounds: RstParseBounds) -> Result<Tree, DocumentationError> {
    let language: Language = tree_sitter_rst::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| refused(DocumentationViolation::Format, "rst_grammar"))?;
    let (tree, exhausted) = {
        let mut calls = 0_usize;
        let mut exhausted = false;
        let mut progress = |_: &ParseState| {
            if calls == bounds.progress_callbacks {
                exhausted = true;
                ControlFlow::Break(())
            } else {
                calls += 1;
                ControlFlow::Continue(())
            }
        };
        let mut input =
            |byte: usize, _point: Point| text.as_bytes().get(byte..).unwrap_or_default();
        let tree = parser.parse_with_options(
            &mut input,
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        (tree, exhausted)
    };
    tree.ok_or_else(|| {
        let field = if exhausted {
            "rst_progress"
        } else {
            "rst_parse"
        };
        let violation = if exhausted {
            DocumentationViolation::LimitExceeded
        } else {
            DocumentationViolation::Format
        };
        refused(violation, field)
    })
}

#[derive(Clone, Copy, Debug)]
struct RstNode<'tree> {
    node: Node<'tree>,
    parent: Option<usize>,
}

fn bounded_nodes(
    root: Node<'_>,
    bounds: RstParseBounds,
) -> Result<Vec<RstNode<'_>>, DocumentationError> {
    let mut nodes = Vec::new();
    let mut pending = vec![(root, 0_usize, None)];
    while let Some((node, depth, parent)) = pending.pop() {
        if depth > bounds.depth {
            return Err(refused(DocumentationViolation::LimitExceeded, "rst_depth"));
        }
        if nodes.len() >= bounds.node_count {
            return Err(refused(DocumentationViolation::LimitExceeded, "rst_nodes"));
        }
        let node_id = node.id();
        nodes.push(RstNode { node, parent });
        for index in (0..node.child_count()).rev() {
            let child_index = u32::try_from(index)
                .map_err(|_| refused(DocumentationViolation::LimitExceeded, "rst_nodes"))?;
            let Some(child) = node.child(child_index) else {
                continue;
            };
            if child.is_named() {
                if nodes.len() + pending.len() >= bounds.node_count {
                    return Err(refused(DocumentationViolation::LimitExceeded, "rst_nodes"));
                }
                pending.push((child, depth + 1, Some(node_id)));
            }
        }
    }
    Ok(nodes)
}

#[derive(Debug)]
struct RstKinds {
    section: u16,
    title: u16,
    paragraph: u16,
    literal_block: u16,
    bullet_list: u16,
    enumerated_list: u16,
    definition_list: u16,
    field_list: u16,
    block_quote: u16,
    line_block: u16,
    doctest_block: u16,
    directive: u16,
    content: u16,
    arguments: u16,
    target: u16,
    inline_target: u16,
    reference: u16,
    standalone_hyperlink: u16,
    adornment: u16,
    comment: u16,
    footnote: u16,
    citation: u16,
    substitution_definition: u16,
    name_field: u16,
    body_field: u16,
    link_field: u16,
}

fn rst_kinds(language: &Language) -> &'static RstKinds {
    static KINDS: std::sync::OnceLock<RstKinds> = std::sync::OnceLock::new();
    KINDS.get_or_init(|| RstKinds {
        section: kind_id(language, "section", true),
        title: kind_id(language, "title", true),
        paragraph: kind_id(language, "paragraph", true),
        literal_block: kind_id(language, "literal_block", true),
        bullet_list: kind_id(language, "bullet_list", true),
        enumerated_list: kind_id(language, "enumerated_list", true),
        definition_list: kind_id(language, "definition_list", true),
        field_list: kind_id(language, "field_list", true),
        block_quote: kind_id(language, "block_quote", true),
        line_block: kind_id(language, "line_block", true),
        doctest_block: kind_id(language, "doctest_block", true),
        directive: kind_id(language, "directive", true),
        content: kind_id(language, "content", true),
        arguments: kind_id(language, "arguments", true),
        target: kind_id(language, "target", true),
        inline_target: kind_id(language, "inline_target", true),
        reference: kind_id(language, "reference", true),
        standalone_hyperlink: kind_id(language, "standalone_hyperlink", true),
        adornment: kind_id(language, "adornment", false),
        comment: kind_id(language, "comment", true),
        footnote: kind_id(language, "footnote", true),
        citation: kind_id(language, "citation", true),
        substitution_definition: kind_id(language, "substitution_definition", true),
        name_field: field_id(language, "name"),
        body_field: field_id(language, "body"),
        link_field: field_id(language, "link"),
    })
}

fn field_id(language: &Language, field: &str) -> u16 {
    let id = language
        .field_id_for_name(field)
        .unwrap_or_else(|| panic!("pinned RST grammar must define field={field}"));
    id.get()
}

fn kind_id(language: &Language, kind: &str, named: bool) -> u16 {
    let id = language.id_for_node_kind(kind, named);
    assert!(id != 0, "pinned RST grammar must define kind={kind}");
    id
}

#[derive(Clone, Debug)]
struct SectionDraft {
    node_id: usize,
    section_start: usize,
    title_range: TextRange,
    level: u32,
    name: String,
    key: Vec<(u32, String)>,
}

#[derive(Default)]
struct SectionFacts {
    paths: HashMap<usize, Vec<RstHeading>>,
    title_ranges: HashMap<usize, TextRange>,
    events: Vec<(usize, TextRange, Vec<RstHeading>)>,
}

fn collect_section_facts(
    text: &str,
    entries: &[RstNode<'_>],
    kinds: &RstKinds,
) -> Result<SectionFacts, DocumentationError> {
    let mut adornment_levels = HashMap::<char, u32>::new();
    let mut section_stack = Vec::<(u32, String)>::new();
    let mut sections = Vec::new();
    let mut duplicate_counts = HashMap::<Vec<(u32, String)>, usize>::new();
    let mut section_title_ranges = HashMap::<usize, TextRange>::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.node.kind_id() == kinds.section)
    {
        let node = entry.node;
        let Some(title) = child_of_kind(node, kinds.title) else {
            continue;
        };
        let Some(style) = section_adornment(node, kinds.adornment, text) else {
            continue;
        };
        let level = if let Some(level) = adornment_levels.get(&style) {
            *level
        } else {
            let level = u32::try_from(adornment_levels.len() + 1).unwrap_or(u32::MAX);
            adornment_levels.insert(style, level);
            level
        };
        while section_stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= level)
        {
            section_stack.pop();
        }
        let name = text
            .get(title.byte_range())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut key = section_stack.clone();
        key.push((level, name.clone()));
        *duplicate_counts.entry(key.clone()).or_default() += 1;
        section_stack.push((level, name.clone()));
        sections.push(SectionDraft {
            node_id: node.id(),
            section_start: node.start_byte(),
            title_range: byte_range(title)?,
            level,
            name,
            key,
        });
        section_title_ranges.insert(node.id(), byte_range(title)?);
    }

    let mut section_stack = Vec::<(u32, RstHeading)>::new();
    let mut duplicate_ordinals = HashMap::<Vec<(u32, String)>, u32>::new();
    let mut section_paths = HashMap::<usize, Vec<RstHeading>>::new();
    let mut section_events = Vec::with_capacity(sections.len());
    for section in sections {
        while section_stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= section.level)
        {
            section_stack.pop();
        }
        let name = if duplicate_counts.get(&section.key).copied().unwrap_or(0) > 1 {
            let ordinal = duplicate_ordinals.entry(section.key.clone()).or_default();
            *ordinal += 1;
            format!("{}~{}", section.name, ordinal)
        } else {
            section.name.clone()
        };
        section_stack.push((
            section.level,
            RstHeading {
                level: section.level,
                name,
            },
        ));
        let path = section_stack
            .iter()
            .map(|(_, heading)| heading.clone())
            .collect::<Vec<_>>();
        section_paths.insert(section.node_id, path.clone());
        section_events.push((section.section_start, section.title_range, path));
    }

    Ok(SectionFacts {
        paths: section_paths,
        title_ranges: section_title_ranges,
        events: section_events,
    })
}

fn collect_entry_facts(
    text: &str,
    entries: &[RstNode<'_>],
    kinds: &RstKinds,
    starts: &[usize],
    sections: &SectionFacts,
) -> Result<RstFacts, DocumentationError> {
    let mut state = RstEntryState {
        facts: RstFacts::default(),
        omitted_node_ids: HashSet::new(),
        block_for_node: HashMap::with_capacity(entries.len()),
    };
    for entry in entries {
        collect_entry(entry, text, kinds, starts, sections, &mut state)?;
    }
    Ok(state.facts)
}

#[derive(Default)]
struct RstEntryState {
    facts: RstFacts,
    omitted_node_ids: HashSet<usize>,
    block_for_node: HashMap<usize, TextRange>,
}

#[expect(
    clippy::too_many_lines,
    reason = "One tree walk keeps omitted ancestry and authored facts in source order"
)]
fn collect_entry(
    entry: &RstNode<'_>,
    text: &str,
    kinds: &RstKinds,
    starts: &[usize],
    sections: &SectionFacts,
    state: &mut RstEntryState,
) -> Result<(), DocumentationError> {
    let RstEntryState {
        facts,
        omitted_node_ids,
        block_for_node,
    } = state;
    let node = entry.node;
    if entry
        .parent
        .is_some_and(|parent| omitted_node_ids.contains(&parent))
    {
        omitted_node_ids.insert(node.id());
        return Ok(());
    }
    let mut current_block = entry
        .parent
        .and_then(|parent| block_for_node.get(&parent).cloned());

    if node.kind_id() == kinds.directive {
        let name = node
            .child_by_field_id(kinds.name_field)
            .and_then(|name| text.get(name.byte_range()))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if name == "code" || name == "code-block" {
            let body = node.child_by_field_id(kinds.body_field);
            let code = body.and_then(|body| child_of_kind(body, kinds.content));
            if let Some(code) = code {
                let range = byte_range(code)?;
                let line = line_of(starts, range.start);
                let language = body
                    .and_then(|body| child_of_kind(body, kinds.arguments))
                    .and_then(|arguments| text.get(arguments.byte_range()))
                    .and_then(|arguments| arguments.split_whitespace().next())
                    .map(str::to_owned);
                facts.blocks.push(RstBlockFact {
                    range: range.clone(),
                    line,
                    kind: RstBlockKind::Code,
                    structure: "code",
                    heading_path: heading_path_at(&sections.events, node.start_byte()),
                    language,
                });
                current_block = Some(range);
            } else {
                facts.omitted_ranges.push(byte_range(node)?);
            }
        } else {
            facts.omitted_ranges.push(byte_range(node)?);
        }
        omitted_node_ids.insert(node.id());
    } else if matches!(
        node.kind_id(),
        id if id == kinds.comment
            || id == kinds.footnote
            || id == kinds.citation
            || id == kinds.substitution_definition
            || id == kinds.doctest_block
    ) {
        facts.omitted_ranges.push(byte_range(node)?);
        omitted_node_ids.insert(node.id());
    } else if node.kind_id() == kinds.section {
        let Some(path) = sections.paths.get(&node.id()).cloned() else {
            return Ok(());
        };
        let Some(title_range) = sections.title_ranges.get(&node.id()).cloned() else {
            return Ok(());
        };
        facts.blocks.push(RstBlockFact {
            range: title_range.clone(),
            line: line_of(starts, title_range.start),
            kind: RstBlockKind::Prose,
            structure: "heading",
            heading_path: path,
            language: None,
        });
        current_block = Some(title_range);
    } else if node.kind_id() == kinds.paragraph {
        let full = byte_range(node)?;
        let literal = child_of_kind(node, kinds.literal_block);
        let range = literal.map_or(full.clone(), |literal| TextRange {
            start: full.start,
            end: byte_range(literal).map_or(full.end, |range| range.start),
        });
        if range.start < range.end {
            let line = line_of(starts, range.start);
            facts.blocks.push(RstBlockFact {
                range: range.clone(),
                line,
                kind: RstBlockKind::Prose,
                structure: "paragraph",
                heading_path: heading_path_at(&sections.events, node.start_byte()),
                language: None,
            });
            current_block = Some(range);
        }
    } else if node.kind_id() == kinds.literal_block {
        let range = byte_range(node)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Code,
            structure: "literal_block",
            heading_path: heading_path_at(&sections.events, node.start_byte()),
            language: None,
        });
        current_block = Some(range);
    } else if node.kind_id() == kinds.bullet_list
        || node.kind_id() == kinds.enumerated_list
        || node.kind_id() == kinds.definition_list
        || node.kind_id() == kinds.field_list
    {
        let range = byte_range(node)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Prose,
            structure: "list",
            heading_path: heading_path_at(&sections.events, node.start_byte()),
            language: None,
        });
        current_block = Some(range);
    } else if node.kind_id() == kinds.block_quote || node.kind_id() == kinds.line_block {
        let range = byte_range(node)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Prose,
            structure: if node.kind_id() == kinds.block_quote {
                "block_quote"
            } else {
                "line_block"
            },
            heading_path: heading_path_at(&sections.events, node.start_byte()),
            language: None,
        });
        current_block = Some(range);
    }

    if let Some(block) = &current_block {
        block_for_node.insert(node.id(), block.clone());
    }
    collect_target(node, text, kinds, &mut facts.targets)?;
    collect_link(node, text, kinds, current_block, &mut facts.links)?;
    Ok(())
}

fn build_facts(
    text: &str,
    tree: &Tree,
    entries: &[RstNode<'_>],
    kinds: &RstKinds,
) -> Result<RstFacts, DocumentationError> {
    let starts = line_starts(text);
    let sections = collect_section_facts(text, entries, kinds)?;
    let mut facts = collect_entry_facts(text, entries, kinds, &starts, &sections)?;
    collect_errors(tree.root_node(), &mut facts.error_ranges)?;
    if tree.root_node().has_error() && facts.error_ranges.is_empty() {
        facts.error_ranges.push(TextRange {
            start: 0,
            end: u64::try_from(text.len()).unwrap_or(u64::MAX),
        });
    }
    facts
        .error_ranges
        .sort_by_key(|range| (range.start, range.end));
    facts
        .error_ranges
        .dedup_by_key(|range| (range.start, range.end));
    facts
        .blocks
        .retain(|block| !overlaps_any(&block.range, &facts.error_ranges));
    facts
        .blocks
        .sort_by_key(|block| (block.range.start, block.range.end));
    facts
        .links
        .sort_by_key(|link| (link.range.start, link.range.end));
    facts
        .targets
        .sort_by_key(|target| (target.range.start, target.range.end));
    facts
        .omitted_ranges
        .sort_by_key(|range| (range.start, range.end));
    facts
        .omitted_ranges
        .dedup_by_key(|range| (range.start, range.end));
    Ok(facts)
}

fn child_of_kind(node: Node<'_>, kind: u16) -> Option<Node<'_>> {
    (0..node.child_count()).find_map(|index| {
        u32::try_from(index)
            .ok()
            .and_then(|index| node.child(index))
            .filter(|child| child.kind_id() == kind)
    })
}

fn section_adornment(node: Node<'_>, kind: u16, text: &str) -> Option<char> {
    for index in 0..node.child_count() {
        let child = u32::try_from(index)
            .ok()
            .and_then(|index| node.child(index))?;
        if child.kind_id() == kind {
            return text.get(child.byte_range())?.chars().next();
        }
    }
    None
}

fn heading_path_at(
    sections: &[(usize, TextRange, Vec<RstHeading>)],
    position: usize,
) -> Vec<RstHeading> {
    let index = sections.partition_point(|(start, _, _)| *start <= position);
    index
        .checked_sub(1)
        .and_then(|index| sections.get(index))
        .map_or_else(Vec::new, |(_, _, path)| path.clone())
}

fn collect_target(
    node: Node<'_>,
    text: &str,
    kinds: &RstKinds,
    targets: &mut Vec<RstTargetFact>,
) -> Result<(), DocumentationError> {
    if node.kind_id() == kinds.target {
        if let Some(name) = node.child_by_field_id(kinds.name_field) {
            let raw_range = byte_range(name)?;
            let mut start = usize::try_from(raw_range.start)
                .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?;
            let mut end = usize::try_from(raw_range.end)
                .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?;
            let raw = text
                .get(start..end)
                .ok_or_else(|| refused(DocumentationViolation::Range, "rst_range"))?;
            if raw.starts_with('_') {
                start += 1;
            }
            if text.get(start..end).is_some_and(|name| name.ends_with(':')) {
                end = end.saturating_sub(1);
            }
            let authored = text
                .get(start..end)
                .ok_or_else(|| refused(DocumentationViolation::Range, "rst_range"))?
                .trim()
                .to_owned();
            let range = text_range(start, end)?;
            let destination = node.child_by_field_id(kinds.link_field);
            let destination_range = destination.map(byte_range).transpose()?;
            let destination_text = destination
                .and_then(|destination| text.get(destination.byte_range()))
                .map(str::trim)
                .map(str::to_owned);
            targets.push(RstTargetFact {
                name: authored,
                range,
                destination: destination_text,
                destination_range,
            });
        }
    } else if node.kind_id() == kinds.inline_target {
        let full = byte_range(node)?;
        let mut start = usize::try_from(full.start).unwrap_or(text.len());
        let mut end = usize::try_from(full.end).unwrap_or(text.len());
        if text
            .get(start..end)
            .is_some_and(|raw| raw.starts_with("_`"))
        {
            start += 2;
            end = end.saturating_sub(1);
        }
        if let Some(name) = text.get(start..end) {
            targets.push(RstTargetFact {
                name: name.trim().to_owned(),
                range: text_range(start, end)?,
                destination: None,
                destination_range: None,
            });
        }
    }
    Ok(())
}

fn collect_link(
    node: Node<'_>,
    text: &str,
    kinds: &RstKinds,
    block_range: Option<TextRange>,
    links: &mut Vec<RstLinkFact>,
) -> Result<(), DocumentationError> {
    let Some(block_range) = block_range else {
        return Ok(());
    };
    let (authored_range, kind) = if node.kind_id() == kinds.standalone_hyperlink {
        (Some(byte_range(node)?), RstLinkKind::Destination)
    } else if node.kind_id() == kinds.reference {
        (reference_name_range(node, text)?, RstLinkKind::Reference)
    } else {
        (None, RstLinkKind::Reference)
    };
    if let Some(range) = authored_range
        && let Some(authored) = range_slice(text, &range)
    {
        links.push(RstLinkFact {
            block_range,
            range,
            authored: authored.to_owned(),
            kind,
        });
    }
    Ok(())
}

fn reference_name_range(
    node: Node<'_>,
    text: &str,
) -> Result<Option<TextRange>, DocumentationError> {
    let mut range = byte_range(node)?;
    let mut start = usize::try_from(range.start)
        .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?;
    let mut end = usize::try_from(range.end)
        .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?;
    let Some(raw) = text.get(start..end) else {
        return Err(refused(DocumentationViolation::Range, "rst_range"));
    };
    if raw.starts_with('`') {
        start += 1;
    }
    if raw.ends_with("__") {
        end = end.saturating_sub(2);
    } else if raw.ends_with('_') {
        end = end.saturating_sub(1);
    }
    if text.get(start..end).is_some_and(|name| name.ends_with('`')) {
        end = end.saturating_sub(1);
    }
    range = text_range(start, end)?;
    Ok((range.start < range.end).then_some(range))
}

fn collect_errors(node: Node<'_>, output: &mut Vec<TextRange>) -> Result<(), DocumentationError> {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.is_error() || current.is_missing() {
            output.push(byte_range(current)?);
        }
        for index in (0..current.child_count()).rev() {
            if let Some(child) = u32::try_from(index)
                .ok()
                .and_then(|index| current.child(index))
                .filter(Node::is_named)
            {
                pending.push(child);
            }
        }
    }
    Ok(())
}

fn byte_range(node: Node<'_>) -> Result<TextRange, DocumentationError> {
    Ok(TextRange {
        start: u64::try_from(node.start_byte())
            .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?,
        end: u64::try_from(node.end_byte())
            .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?,
    })
}

fn text_range(start: usize, end: usize) -> Result<TextRange, DocumentationError> {
    Ok(TextRange {
        start: u64::try_from(start)
            .map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?,
        end: u64::try_from(end).map_err(|_| refused(DocumentationViolation::Range, "rst_range"))?,
    })
}

fn range_slice<'text>(text: &'text str, range: &TextRange) -> Option<&'text str> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    text.get(start..end)
}

fn overlaps_any(range: &TextRange, errors: &[TextRange]) -> bool {
    errors
        .iter()
        .any(|error| range.start < error.end && error.start < range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> ProjectPath {
        ProjectPath::new("docs/guide.rst").expect("valid source path")
    }

    fn extract(text: &str) -> RstFacts {
        extract_rst_facts(text, &path()).expect("RST source parses")
    }

    #[test]
    fn first_seen_adornment_order_sets_heading_path_and_duplicate_names() {
        let text = "Top\n===\n\nIntro.\n\nChild\n-----\n\nDetails.\n\nTop\n===\n\nAgain.\n";
        let facts = extract(text);
        let headings = facts
            .blocks
            .iter()
            .filter(|block| block.structure == "heading")
            .map(|block| (block.heading_path.clone(), block.range.clone()))
            .collect::<Vec<_>>();
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].0[0].level, 1);
        assert_eq!(headings[1].0[1].level, 2);
        assert_eq!(headings[0].0[0].name, "Top~1");
        assert_eq!(headings[2].0[0].name, "Top~2");
        assert_eq!(
            &text[usize::try_from(headings[1].1.start).expect("range start")
                ..usize::try_from(headings[1].1.end).expect("range end")],
            "Child"
        );
    }

    #[test]
    fn supported_blocks_and_targets_keep_exact_ranges_and_code_is_inert() {
        let text = "Guide\n=====\n\nSee target_ and https://example.invalid/path.\n\n- item\n\n.. _target:\n\n::\n\n    literal body\n\n.. code-block:: rust\n\n    fn main() {}\n\n.. note:: not fetched\n\n   body text\n";
        let facts = extract(text);
        assert!(
            facts
                .blocks
                .iter()
                .any(|block| block.structure == "heading")
        );
        assert!(
            facts
                .blocks
                .iter()
                .any(|block| block.structure == "paragraph")
        );
        assert!(facts.blocks.iter().any(|block| block.structure == "list"));
        assert!(facts.blocks.iter().any(|block| {
            block.kind == RstBlockKind::Code
                && block.structure == "literal_block"
                && range_slice(text, &block.range).is_some_and(|body| body.contains("literal body"))
        }));
        assert!(facts.blocks.iter().any(|block| {
            block.kind == RstBlockKind::Code
                && block.language.as_deref() == Some("rust")
                && range_slice(text, &block.range).is_some_and(|body| body.contains("fn main"))
        }));
        assert_eq!(facts.targets[0].name, "target");
        assert!(facts.links.iter().any(|link| link.authored == "target"));
        assert!(
            facts
                .links
                .iter()
                .any(|link| link.authored == "https://example.invalid/path")
        );
        assert_eq!(facts.omitted_ranges.len(), 1);
        assert!(
            range_slice(text, &facts.omitted_ranges[0]).is_some_and(|body| body.contains("note"))
        );
    }

    #[test]
    fn crlf_and_missing_final_newline_keep_original_byte_ranges() {
        let text = "Title\r\n=====\r\n\r\nLast paragraph";
        let facts = extract(text);
        let paragraph = facts
            .blocks
            .iter()
            .find(|block| block.structure == "paragraph")
            .expect("paragraph");
        assert_eq!(paragraph.line, 4);
        assert_eq!(paragraph.range.end, text.len() as u64);
        assert_eq!(range_slice(text, &paragraph.range), Some("Last paragraph"));
    }

    #[test]
    fn include_raw_and_execution_directives_are_inert_and_omitted() {
        let text = concat!(
            ".. include:: hidden.rst\n\n",
            ".. raw:: html\n\n",
            "   https://example.invalid/raw\n\n",
            ".. exec:: command\n\n",
            "   https://example.invalid/output\n",
        );
        let facts = extract(text);
        assert_eq!(facts.omitted_ranges.len(), 3);
        assert!(facts.blocks.is_empty());
        assert!(facts.links.is_empty());
        for range in facts.omitted_ranges {
            assert!(range_slice(text, &range).is_some());
        }
    }

    #[test]
    fn exact_source_bound_accepts_and_one_byte_over_refuses() {
        let path = path();
        let text = "Title\n=====\n";
        let exact = RstParseBounds {
            source_bytes: text.len(),
            ..RstParseBounds::default()
        };
        assert!(extract_rst_facts_with_bounds(text, &path, exact).is_ok());
        let over = RstParseBounds {
            source_bytes: text.len() - 1,
            ..RstParseBounds::default()
        };
        let error = extract_rst_facts_with_bounds(text, &path, over).expect_err("source too large");
        assert_eq!(
            error.fault().violation(),
            DocumentationViolation::LimitExceeded
        );
        assert_eq!(error.fault().field(), "source_bytes");
    }

    #[test]
    fn zero_progress_callbacks_refuse_parse() {
        let path = path();
        let text = "Word ".repeat(20_000);
        let bounds = RstParseBounds {
            progress_callbacks: 0,
            ..RstParseBounds::default()
        };
        let error =
            extract_rst_facts_with_bounds(&text, &path, bounds).expect_err("callback bound");
        assert_eq!(
            error.fault().violation(),
            DocumentationViolation::LimitExceeded
        );
        assert_eq!(error.fault().field(), "rst_progress");
    }
}
