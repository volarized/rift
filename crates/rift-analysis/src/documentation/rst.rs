//! Bounded reStructuredText facts from the pinned Tree-sitter grammar.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};

use rift_core::{ProjectPath, line::line_of, line::line_starts};
use rift_protocol::read::TextRange;
use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser, Point, Tree};
use url::Url;

use rift_error::{RiftError, errors};

#[cfg(test)]
const SOURCE_BYTES_MAX: usize = 4 * 1_024 * 1_024;
#[cfg(test)]
const SYNTAX_ELEMENTS_MAX: usize = 250_000;
#[cfg(test)]
const SYNTAX_DEPTH_MAX: usize = 512;
#[cfg(test)]
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
#[cfg(test)]
pub(super) fn extract_rst_facts(text: &str, path: &ProjectPath) -> Result<RstFacts, RiftError> {
    extract_rst_facts_with_bounds(text, path, RstParseBounds::default())
}

pub(super) fn extract_rst_facts_with_limits(
    text: &str,
    path: &ProjectPath,
    limits: super::DocumentationLimits,
) -> Result<RstFacts, RiftError> {
    let source_bytes = usize::try_from(limits.source_bytes_max).map_err(|_| {
        errors::analysis::documentation_limit_exceeded()
            .field("source_bytes")
            .error()
    })?;
    let bounds = RstParseBounds {
        source_bytes,
        syntax_element_count: limits.syntax_elements_max as usize,
        depth: limits.depth_max as usize,
        progress_callbacks: limits.progress_callbacks_max as usize,
    };
    extract_rst_facts_with_bounds(text, path, bounds)
}

#[derive(Clone, Copy, Debug)]
struct RstParseBounds {
    source_bytes: usize,
    syntax_element_count: usize,
    depth: usize,
    progress_callbacks: usize,
}

#[cfg(test)]
impl RstParseBounds {
    const fn default() -> Self {
        Self {
            source_bytes: SOURCE_BYTES_MAX,
            syntax_element_count: SYNTAX_ELEMENTS_MAX,
            depth: SYNTAX_DEPTH_MAX,
            progress_callbacks: PROGRESS_CALLBACKS_MAX,
        }
    }
}

fn extract_rst_facts_with_bounds(
    text: &str,
    _path: &ProjectPath,
    bounds: RstParseBounds,
) -> Result<RstFacts, RiftError> {
    if text.len() > bounds.source_bytes {
        return errors::analysis::documentation_limit_exceeded()
            .field("source_bytes")
            .fail();
    }
    let tree = parse(text, bounds)?;
    let language: Language = tree_sitter_rst::LANGUAGE.into();
    let kinds = rst_kinds(&language);
    let entries = bounded_syntax_elements(tree.root_node(), bounds)?;
    build_facts(text, &tree, &entries, kinds)
}

fn parse(text: &str, bounds: RstParseBounds) -> Result<Tree, RiftError> {
    let language: Language = tree_sitter_rst::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|_| {
        errors::analysis::documentation_format_invalid()
            .field("rst_grammar")
            .error()
    })?;
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
        if exhausted {
            errors::analysis::documentation_limit_exceeded()
                .field("rst_progress")
                .error()
        } else {
            errors::analysis::documentation_format_invalid()
                .field("rst_parse")
                .error()
        }
    })
}

#[derive(Clone, Copy, Debug)]
struct RstElement<'tree> {
    syntax_element: Node<'tree>,
    parent: Option<usize>,
}

fn bounded_syntax_elements(
    root: Node<'_>,
    bounds: RstParseBounds,
) -> Result<Vec<RstElement<'_>>, RiftError> {
    let mut syntax_elements = Vec::new();
    let mut pending = vec![(root, 0_usize, None)];
    let mut cursor = root.walk();
    while let Some((syntax_element, depth, parent)) = pending.pop() {
        if depth > bounds.depth {
            return errors::analysis::documentation_limit_exceeded()
                .field("rst_depth")
                .fail();
        }
        if syntax_elements.len() >= bounds.syntax_element_count {
            return errors::analysis::documentation_limit_exceeded()
                .field("rst_syntax_elements")
                .fail();
        }
        let syntax_element_id = syntax_element.id();
        syntax_elements.push(RstElement {
            syntax_element,
            parent,
        });
        if syntax_elements.len() + pending.len() + syntax_element.named_child_count()
            > bounds.syntax_element_count
        {
            return errors::analysis::documentation_limit_exceeded()
                .field("rst_syntax_elements")
                .fail();
        }
        // The cursor steps sibling to sibling; `Node::child` restarts from the
        // first child on every call, which makes a wide syntax element quadratic. The
        // children go on reversed so the worklist pops them in source order.
        let first_child = pending.len();
        pending.extend(
            syntax_element
                .named_children(&mut cursor)
                .map(|child| (child, depth + 1, Some(syntax_element_id))),
        );
        pending[first_child..].reverse();
    }
    Ok(syntax_elements)
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
    syntax_element_id: usize,
    section_start: usize,
    title_range: TextRange,
    level: u32,
    name: String,
}

#[derive(Default)]
struct SectionFacts {
    paths: HashMap<usize, Vec<RstHeading>>,
    title_ranges: HashMap<usize, TextRange>,
    events: Vec<(usize, TextRange, Vec<RstHeading>)>,
}

fn collect_section_facts(
    text: &str,
    entries: &[RstElement<'_>],
    kinds: &RstKinds,
) -> Result<SectionFacts, RiftError> {
    let mut adornment_levels = HashMap::<char, u32>::new();
    let mut section_stack = Vec::<(u32, String)>::new();
    let mut sections = Vec::new();
    let mut section_title_ranges = HashMap::<usize, TextRange>::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.syntax_element.kind_id() == kinds.section)
    {
        let syntax_element = entry.syntax_element;
        let Some(title) = child_of_kind(syntax_element, kinds.title) else {
            continue;
        };
        let Some(style) = section_adornment(syntax_element, kinds.adornment, text) else {
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
        section_stack.push((level, name.clone()));
        sections.push(SectionDraft {
            syntax_element_id: syntax_element.id(),
            section_start: syntax_element.start_byte(),
            title_range: block_range(title, text)?,
            level,
            name,
        });
        section_title_ranges.insert(syntax_element.id(), block_range(title, text)?);
    }

    let mut section_stack = Vec::<(u32, RstHeading)>::new();
    let mut section_paths = HashMap::<usize, Vec<RstHeading>>::new();
    let mut section_events = Vec::with_capacity(sections.len());
    for section in sections {
        while section_stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= section.level)
        {
            section_stack.pop();
        }
        section_stack.push((
            section.level,
            RstHeading {
                level: section.level,
                name: section.name.clone(),
            },
        ));
        let path = section_stack
            .iter()
            .map(|(_, heading)| heading.clone())
            .collect::<Vec<_>>();
        section_paths.insert(section.syntax_element_id, path.clone());
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
    entries: &[RstElement<'_>],
    kinds: &RstKinds,
    starts: &[usize],
    sections: &SectionFacts,
) -> Result<RstFacts, RiftError> {
    let mut state = RstEntryState {
        facts: RstFacts::default(),
        omitted_syntax_element_ids: HashSet::new(),
        block_for_syntax_element: HashMap::with_capacity(entries.len()),
        paragraph_prose: HashMap::new(),
    };
    for entry in entries {
        collect_entry(entry, text, kinds, starts, sections, &mut state)?;
    }
    Ok(state.facts)
}

#[derive(Default)]
struct RstEntryState {
    facts: RstFacts,
    omitted_syntax_element_ids: HashSet<usize>,
    block_for_syntax_element: HashMap<usize, TextRange>,
    /// Prose ranges of each paragraph that continues after a literal block.
    paragraph_prose: HashMap<usize, Vec<TextRange>>,
}

#[expect(
    clippy::too_many_lines,
    reason = "One tree walk keeps omitted ancestry and authored facts in source order"
)]
fn collect_entry(
    entry: &RstElement<'_>,
    text: &str,
    kinds: &RstKinds,
    starts: &[usize],
    sections: &SectionFacts,
    state: &mut RstEntryState,
) -> Result<(), RiftError> {
    let RstEntryState {
        facts,
        omitted_syntax_element_ids,
        block_for_syntax_element,
        paragraph_prose,
    } = state;
    let syntax_element = entry.syntax_element;
    if entry
        .parent
        .is_some_and(|parent| omitted_syntax_element_ids.contains(&parent))
    {
        omitted_syntax_element_ids.insert(syntax_element.id());
        return Ok(());
    }
    let mut current_block = entry.parent.and_then(|parent| {
        let prose = paragraph_prose.get(&parent).and_then(|prose| {
            let start = syntax_element.start_byte() as u64;
            prose_range_at(prose, start)
        });
        prose
            .or_else(|| block_for_syntax_element.get(&parent))
            .cloned()
    });

    if syntax_element.kind_id() == kinds.directive {
        let name = syntax_element
            .child_by_field_id(kinds.name_field)
            .and_then(|name| text.get(name.byte_range()))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if name == "code" || name == "code-block" {
            let body = syntax_element.child_by_field_id(kinds.body_field);
            let code = body.and_then(|body| child_of_kind(body, kinds.content));
            if let Some(code) = code {
                let range = block_range(code, text)?;
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
                    heading_path: heading_path_at(&sections.events, syntax_element.start_byte()),
                    language,
                });
                current_block = Some(range);
            } else {
                facts.omitted_ranges.push(byte_range(syntax_element)?);
            }
        } else {
            facts.omitted_ranges.push(byte_range(syntax_element)?);
        }
        omitted_syntax_element_ids.insert(syntax_element.id());
    } else if matches!(
        syntax_element.kind_id(),
        id if id == kinds.comment
            || id == kinds.footnote
            || id == kinds.citation
            || id == kinds.substitution_definition
            || id == kinds.doctest_block
    ) {
        facts.omitted_ranges.push(byte_range(syntax_element)?);
        omitted_syntax_element_ids.insert(syntax_element.id());
    } else if syntax_element.kind_id() == kinds.section {
        let Some(path) = sections.paths.get(&syntax_element.id()).cloned() else {
            return Ok(());
        };
        let Some(title_range) = sections.title_ranges.get(&syntax_element.id()).cloned() else {
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
    } else if syntax_element.kind_id() == kinds.paragraph {
        let prose = paragraph_prose_ranges(syntax_element, text, kinds.literal_block)?;
        for range in &prose {
            facts.blocks.push(RstBlockFact {
                range: range.clone(),
                line: line_of(starts, range.start),
                kind: RstBlockKind::Prose,
                structure: "paragraph",
                heading_path: heading_path_at(&sections.events, syntax_element.start_byte()),
                language: None,
            });
        }
        current_block = prose.first().cloned().or(current_block);
        if prose.len() > 1 {
            paragraph_prose.insert(syntax_element.id(), prose);
        }
    } else if syntax_element.kind_id() == kinds.literal_block {
        let range = block_range(syntax_element, text)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Code,
            structure: "literal_block",
            heading_path: heading_path_at(&sections.events, syntax_element.start_byte()),
            language: None,
        });
        current_block = Some(range);
    } else if syntax_element.kind_id() == kinds.bullet_list
        || syntax_element.kind_id() == kinds.enumerated_list
        || syntax_element.kind_id() == kinds.definition_list
        || syntax_element.kind_id() == kinds.field_list
    {
        let range = block_range(syntax_element, text)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Prose,
            structure: "list",
            heading_path: heading_path_at(&sections.events, syntax_element.start_byte()),
            language: None,
        });
        current_block = Some(range);
    } else if syntax_element.kind_id() == kinds.block_quote
        || syntax_element.kind_id() == kinds.line_block
    {
        let range = block_range(syntax_element, text)?;
        let line = line_of(starts, range.start);
        facts.blocks.push(RstBlockFact {
            range: range.clone(),
            line,
            kind: RstBlockKind::Prose,
            structure: if syntax_element.kind_id() == kinds.block_quote {
                "block_quote"
            } else {
                "line_block"
            },
            heading_path: heading_path_at(&sections.events, syntax_element.start_byte()),
            language: None,
        });
        current_block = Some(range);
    }

    if let Some(block) = &current_block {
        block_for_syntax_element.insert(syntax_element.id(), block.clone());
    }
    collect_target(syntax_element, text, kinds, &mut facts.targets)?;
    collect_link(syntax_element, text, kinds, current_block, &mut facts.links)?;
    Ok(())
}

fn build_facts(
    text: &str,
    tree: &Tree,
    entries: &[RstElement<'_>],
    kinds: &RstKinds,
) -> Result<RstFacts, RiftError> {
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

fn child_of_kind(syntax_element: Node<'_>, kind: u16) -> Option<Node<'_>> {
    let mut cursor = syntax_element.walk();
    syntax_element
        .children(&mut cursor)
        .find(|child| child.kind_id() == kind)
}

/// Byte range of one inline element, without the line ending the grammar can keep in it.
///
/// An element at the end of a CRLF line can hold the carriage return.
fn inline_range(syntax_element: Node<'_>, text: &str) -> Range<usize> {
    let range = syntax_element.byte_range();
    let trimmed = text
        .get(range.clone())
        .map_or(range.len(), |raw| raw.trim_end_matches(['\r', '\n']).len());
    range.start..range.start + trimmed
}

/// Byte range of one block, without the carriage return the grammar keeps at a CRLF line end.
fn block_bytes(syntax_element: Node<'_>, text: &str) -> Range<usize> {
    let range = syntax_element.byte_range();
    let end = text
        .get(range.clone())
        .and_then(|raw| raw.strip_suffix('\r'))
        .filter(|block| !block.is_empty())
        .map_or(range.end, |block| range.start + block.len());
    range.start..end
}

fn block_range(syntax_element: Node<'_>, text: &str) -> Result<TextRange, RiftError> {
    let range = block_bytes(syntax_element, text);
    text_range(range.start, range.end)
}

/// The prose ranges of one paragraph, in source order.
///
/// The grammar keeps a line that follows a literal block without a blank line in the same
/// paragraph, so a paragraph can hold prose before, between, and after its literal blocks.
fn paragraph_prose_ranges(
    paragraph: Node<'_>,
    text: &str,
    literal_kind: u16,
) -> Result<Vec<TextRange>, RiftError> {
    let mut ranges = Vec::new();
    let mut start = paragraph.start_byte();
    let mut cursor = paragraph.walk();
    let literals = paragraph
        .children(&mut cursor)
        .filter(|child| child.kind_id() == literal_kind);
    for literal in literals {
        push_prose_range(text, start, literal.start_byte(), &mut ranges)?;
        start = literal.end_byte();
    }
    let end = block_bytes(paragraph, text).end;
    push_prose_range(text, start, end, &mut ranges)?;
    Ok(ranges)
}

/// The range holding `start`, of `ranges` in source order.
fn prose_range_at(ranges: &[TextRange], start: u64) -> Option<&TextRange> {
    let following = ranges.partition_point(|range| range.start <= start);
    ranges
        .get(following.checked_sub(1)?)
        .filter(|range| start < range.end)
}

/// Appends the prose between `start` and `end`, without the whitespace before it. Unicode
/// whitespace alone is no prose; ahead of prose it stays, since the grammar can start a
/// reference name with it.
fn push_prose_range(
    text: &str,
    start: usize,
    end: usize,
    ranges: &mut Vec<TextRange>,
) -> Result<(), RiftError> {
    let Some(raw) = text.get(start..end) else {
        return Ok(());
    };
    let prose = raw.trim_ascii_start();
    if !prose.trim().is_empty() {
        ranges.push(text_range(end - prose.len(), end)?);
    }
    Ok(())
}

fn section_adornment(syntax_element: Node<'_>, kind: u16, text: &str) -> Option<char> {
    let mut cursor = syntax_element.walk();
    for child in syntax_element.children(&mut cursor) {
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
    syntax_element: Node<'_>,
    text: &str,
    kinds: &RstKinds,
    targets: &mut Vec<RstTargetFact>,
) -> Result<(), RiftError> {
    if syntax_element.kind_id() == kinds.target {
        if let Some(name) = syntax_element.child_by_field_id(kinds.name_field) {
            let Range { mut start, mut end } = inline_range(name, text);
            let raw = text.get(start..end).ok_or_else(|| {
                errors::analysis::documentation_range_invalid()
                    .field("rst_range")
                    .error()
            })?;
            if raw.starts_with('_') {
                start += 1;
            }
            if text.get(start..end).is_some_and(|name| name.ends_with(':')) {
                end = end.saturating_sub(1);
            }
            let authored = text
                .get(start..end)
                .ok_or_else(|| {
                    errors::analysis::documentation_range_invalid()
                        .field("rst_range")
                        .error()
                })?
                .trim()
                .to_owned();
            let range = text_range(start, end)?;
            let destination = syntax_element.child_by_field_id(kinds.link_field);
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
    } else if syntax_element.kind_id() == kinds.inline_target {
        let Range { mut start, mut end } = inline_range(syntax_element, text);
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
    syntax_element: Node<'_>,
    text: &str,
    kinds: &RstKinds,
    block_range: Option<TextRange>,
    links: &mut Vec<RstLinkFact>,
) -> Result<(), RiftError> {
    let Some(block_range) = block_range else {
        return Ok(());
    };
    let (authored_range, kind) = if syntax_element.kind_id() == kinds.standalone_hyperlink {
        let range = inline_range(syntax_element, text);
        (
            Some(text_range(range.start, range.end)?),
            RstLinkKind::Destination,
        )
    } else if syntax_element.kind_id() == kinds.reference {
        reference_link(syntax_element, text)?
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

fn reference_link(
    syntax_element: Node<'_>,
    text: &str,
) -> Result<(Option<TextRange>, RstLinkKind), RiftError> {
    let Range { mut start, mut end } = inline_range(syntax_element, text);
    let Some(raw) = text.get(start..end) else {
        return errors::analysis::documentation_range_invalid()
            .field("rst_range")
            .fail();
    };
    let quoted = raw.starts_with('`');
    if quoted {
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
    if quoted && let Some((target, kind)) = embedded_reference(text, start..end) {
        return Ok((Some(text_range(target.start, target.end)?), kind));
    }
    let range = text_range(start, end)?;
    Ok((
        (range.start < range.end).then_some(range),
        RstLinkKind::Reference,
    ))
}

/// An embedded target ends the quoted phrase. A target without a URI scheme and
/// ending in an underscore can name another reference.
fn embedded_reference(text: &str, phrase: Range<usize>) -> Option<(Range<usize>, RstLinkKind)> {
    let (label, destination) = text
        .get(phrase.clone())?
        .strip_suffix('>')?
        .rsplit_once('<')?;
    if !label.is_empty() && !label.ends_with(char::is_whitespace) {
        return None;
    }
    let target = destination.trim();
    if target.contains(['<', '>', '\\']) {
        return None;
    }
    let (authored, kind) = match target.strip_suffix('_') {
        Some(name) if Url::parse(target).is_err() => (name.trim_end(), RstLinkKind::Reference),
        _ if !target.chars().any(char::is_whitespace) => (target, RstLinkKind::Destination),
        _ => return None,
    };
    let leading = destination.len() - destination.trim_start().len();
    let start = phrase.start + label.len() + 1 + leading;
    (!authored.is_empty()).then_some((start..start + authored.len(), kind))
}

fn collect_errors(syntax_element: Node<'_>, output: &mut Vec<TextRange>) -> Result<(), RiftError> {
    let mut pending = vec![syntax_element];
    let mut cursor = syntax_element.walk();
    while let Some(current) = pending.pop() {
        if current.is_error() || current.is_missing() {
            output.push(byte_range(current)?);
        }
        let first_child = pending.len();
        pending.extend(current.named_children(&mut cursor));
        pending[first_child..].reverse();
    }
    Ok(())
}

fn byte_range(syntax_element: Node<'_>) -> Result<TextRange, RiftError> {
    Ok(TextRange {
        start: u64::try_from(syntax_element.start_byte()).map_err(|_| {
            errors::analysis::documentation_range_invalid()
                .field("rst_range")
                .error()
        })?,
        end: u64::try_from(syntax_element.end_byte()).map_err(|_| {
            errors::analysis::documentation_range_invalid()
                .field("rst_range")
                .error()
        })?,
    })
}

fn text_range(start: usize, end: usize) -> Result<TextRange, RiftError> {
    Ok(TextRange {
        start: u64::try_from(start).map_err(|_| {
            errors::analysis::documentation_range_invalid()
                .field("rst_range")
                .error()
        })?,
        end: u64::try_from(end).map_err(|_| {
            errors::analysis::documentation_range_invalid()
                .field("rst_range")
                .error()
        })?,
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
    use crate::documentation::DocumentationViolation;

    fn path() -> ProjectPath {
        ProjectPath::new("docs/guide.rst").expect("valid source path")
    }

    fn extract(text: &str) -> RstFacts {
        extract_rst_facts(text, &path()).expect("RST source parses")
    }

    #[test]
    fn a_prose_range_is_found_by_position() {
        let ranges = [text_range(2, 4), text_range(6, 9)].map(|range| range.expect("range"));
        let found = [0, 2, 3, 4, 5, 6, 8, 9]
            .map(|start| prose_range_at(&ranges, start).map(|range| range.start));
        assert_eq!(
            found,
            [None, Some(2), Some(2), None, None, Some(6), Some(6), None]
        );
        assert_eq!(prose_range_at(&[], 0), None);
    }

    #[test]
    fn first_seen_adornment_order_sets_heading_path_and_repeated_names() {
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
        assert_eq!(headings[0].0[0].name, "Top");
        assert_eq!(headings[2].0[0].name, "Top");
        assert_ne!(
            headings[0].1, headings[2].1,
            "each section keeps its own range"
        );
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
            crate::documentation::failure::violation(&error),
            DocumentationViolation::LimitExceeded
        );
        assert_eq!(
            crate::documentation::failure::context_value(&error, "field").as_deref(),
            Some("source_bytes")
        );
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
            crate::documentation::failure::violation(&error),
            DocumentationViolation::LimitExceeded
        );
        assert_eq!(
            crate::documentation::failure::context_value(&error, "field").as_deref(),
            Some("rst_progress")
        );
    }

    #[test]
    fn syntax_element_and_depth_bounds_refuse_supported_source() {
        let path = path();
        let text = "Title\n=====\n\nParagraph.\n";
        let syntax_element_error = extract_rst_facts_with_bounds(
            text,
            &path,
            RstParseBounds {
                syntax_element_count: 1,
                ..RstParseBounds::default()
            },
        )
        .expect_err("syntax element bound");
        assert_eq!(
            crate::documentation::failure::context_value(&syntax_element_error, "field").as_deref(),
            Some("rst_syntax_elements")
        );

        let depth_error = extract_rst_facts_with_bounds(
            text,
            &path,
            RstParseBounds {
                depth: 0,
                ..RstParseBounds::default()
            },
        )
        .expect_err("depth bound");
        assert_eq!(
            crate::documentation::failure::context_value(&depth_error, "field").as_deref(),
            Some("rst_depth")
        );
    }
}
