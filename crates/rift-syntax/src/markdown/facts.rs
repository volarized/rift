use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use rift_core::{Error, ProjectPath};
use tree_sitter::{Node, ParseOptions, ParseState, Parser, Point, Range, Tree};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, ChildIndices};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Maximum Tree-sitter progress callbacks for one Markdown source.
pub const MARKDOWN_PROGRESS_CALLBACKS_MAX: usize = 131_072;
/// Maximum byte ranges supplied to inline parses for one Markdown source.
pub const MARKDOWN_INLINE_RANGES_MAX: usize = 65_536;

/// Kind of searchable content a Markdown block carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownBlockKind {
    /// Authored prose or a heading.
    Prose,
    /// A fenced or indented code block.
    Code,
}

/// Structure of one Markdown block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownBlockStructure {
    /// A heading line.
    Heading,
    /// A paragraph.
    Paragraph,
    /// A list and its items.
    List,
    /// A table.
    Table,
    /// A block quote.
    BlockQuote,
    /// A fenced or indented code block.
    Code,
    /// An authored link reference definition.
    LinkDefinition,
}

/// One block range and its heading context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownBlockFact {
    /// Half-open byte range in the original source.
    pub range: ByteRange,
    /// One-based source line at `range.start`.
    pub line: u64,
    /// Whether this block carries prose or code.
    pub kind: MarkdownBlockKind,
    /// Grammar structure represented by this block.
    pub structure: MarkdownBlockStructure,
    /// Index of the innermost heading in [`MarkdownFacts::headings`].
    pub heading: Option<usize>,
    /// Declared language on a fenced code block.
    pub code_language: Option<String>,
}

/// One Markdown heading linked to its syntax declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownHeadingFact {
    /// Index of this heading in [`SyntaxDocument::symbols`].
    pub symbol_index: usize,
    /// Half-open byte range of the heading line.
    pub range: ByteRange,
    /// Heading level, from 1 through 6 for ATX headings and 1 or 2 for setext headings.
    pub level: u8,
    /// Parent heading index, when nested under another heading.
    pub parent: Option<usize>,
}

/// Kind of authored Markdown link fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownLinkKind {
    /// An inline link or image destination.
    Authored,
    /// A link reference definition.
    ReferenceDefinition,
    /// A use of a link reference definition.
    ReferenceUse,
}

/// One authored link or link reference and its source ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLinkFact {
    /// Kind of link fact.
    pub kind: MarkdownLinkKind,
    /// Range of the block that contains this link.
    pub block_range: ByteRange,
    /// Half-open source range of the link syntax.
    pub range: ByteRange,
    /// Half-open range of an authored destination, when present.
    pub destination_range: Option<ByteRange>,
    /// Half-open range of the destination fragment, without its `#` marker.
    pub fragment_range: Option<ByteRange>,
    /// Half-open range of a reference label, when present.
    pub label_range: Option<ByteRange>,
}

/// One inline code span that can name a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownReferenceCandidate {
    /// Range of the block that contains this candidate.
    pub block_range: ByteRange,
    /// Half-open range of inline code contents, without delimiters.
    pub range: ByteRange,
}

/// Markdown structure and relationship facts from one syntax document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownFacts {
    blocks: Vec<MarkdownBlockFact>,
    headings: Vec<MarkdownHeadingFact>,
    links: Vec<MarkdownLinkFact>,
    reference_candidates: Vec<MarkdownReferenceCandidate>,
    error_ranges: Vec<ByteRange>,
    omitted_ranges: Vec<ByteRange>,
}

impl MarkdownFacts {
    /// Returns blocks in source order.
    #[must_use]
    pub fn blocks(&self) -> &[MarkdownBlockFact] {
        &self.blocks
    }

    /// Returns headings in source order.
    #[must_use]
    pub fn headings(&self) -> &[MarkdownHeadingFact] {
        &self.headings
    }

    /// Returns links and link references in source order.
    #[must_use]
    pub fn links(&self) -> &[MarkdownLinkFact] {
        &self.links
    }

    /// Returns inline code candidates in source order.
    #[must_use]
    pub fn reference_candidates(&self) -> &[MarkdownReferenceCandidate] {
        &self.reference_candidates
    }

    /// Returns parser error ranges.
    #[must_use]
    pub fn error_ranges(&self) -> &[ByteRange] {
        &self.error_ranges
    }

    /// Returns ranges omitted by the selected metadata policy.
    #[must_use]
    pub fn omitted_ranges(&self) -> &[ByteRange] {
        &self.omitted_ranges
    }

    /// Resolves an ordered heading path from its innermost heading index.
    #[must_use]
    pub fn heading_path(&self, heading: Option<usize>) -> Vec<&MarkdownHeadingFact> {
        let mut path = Vec::new();
        let mut current = heading;
        while let Some(index) = current {
            let Some(item) = self.headings.get(index) else {
                break;
            };
            path.push(item);
            current = item.parent;
        }
        path.reverse();
        path
    }

    /// Applies the conservative MDX metadata filter to these Markdown facts.
    ///
    /// `source` must be the bytes used to produce these facts. The filter omits a prose
    /// block with MDX markers outside inline code, or whose first token is `import` or
    /// `export`. It does not parse MDX.
    #[must_use]
    pub fn for_mdx(&self, source: &str) -> Self {
        let mut kept_blocks = Vec::with_capacity(self.blocks.len());
        let mut omitted_ranges = self.omitted_ranges.clone();
        let mut omitted_headings = vec![false; self.headings.len()];
        for (index, block) in self.blocks.iter().enumerate() {
            if mdx_block_is_omitted(block, source, &self.reference_candidates) {
                omitted_ranges.push(block.range);
                if block.structure == MarkdownBlockStructure::Heading
                    && let Some(heading_index) = self
                        .headings
                        .iter()
                        .position(|heading| heading.range == block.range)
                {
                    omitted_headings[heading_index] = true;
                }
            } else {
                kept_blocks.push((index, block.clone()));
            }
        }

        let mut heading_remap = vec![None; self.headings.len()];
        let mut headings = Vec::new();
        for (index, heading) in self.headings.iter().enumerate() {
            if omitted_headings[index] {
                continue;
            }
            let mapped_parent =
                nearest_kept_heading(heading.parent, &omitted_headings, &self.headings)
                    .and_then(|parent| heading_remap.get(parent).copied().flatten());
            heading_remap[index] = Some(headings.len());
            headings.push(MarkdownHeadingFact {
                parent: mapped_parent,
                ..*heading
            });
        }

        let mut facts = self.clone();
        facts.blocks.clear();
        facts.headings = headings;
        for (_, mut block) in kept_blocks {
            block.heading = nearest_kept_heading(block.heading, &omitted_headings, &self.headings)
                .and_then(|heading| heading_remap.get(heading).copied().flatten());
            facts.blocks.push(block);
        }
        let kept_block_ranges = facts
            .blocks
            .iter()
            .map(|block| block.range)
            .collect::<HashSet<_>>();
        facts
            .links
            .retain(|link| kept_block_ranges.contains(&link.block_range));
        facts
            .reference_candidates
            .retain(|candidate| kept_block_ranges.contains(&candidate.block_range));
        facts.omitted_ranges = sorted_unique_ranges(omitted_ranges);
        facts
    }
}

/// One parsed inline tree and the block range whose inline bytes it covers.
#[derive(Debug)]
pub(crate) struct InlineTreeFact {
    parent_id: usize,
    ranges: Vec<Range>,
    tree: Tree,
    base_depth: usize,
}

/// One block tree and its inline trees, parsed under one progress budget.
#[derive(Debug)]
pub(crate) struct MarkdownTrees {
    pub(crate) block: Tree,
    pub(crate) inline: Vec<InlineTreeFact>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MarkdownParseBounds {
    pub(crate) progress_callbacks_max: usize,
    pub(crate) inline_ranges_max: usize,
}

impl MarkdownParseBounds {
    pub(crate) const fn default() -> Self {
        Self {
            progress_callbacks_max: MARKDOWN_PROGRESS_CALLBACKS_MAX,
            inline_ranges_max: MARKDOWN_INLINE_RANGES_MAX,
        }
    }
}

/// Parses Markdown block and inline trees under one callback and range bound.
pub(crate) fn parse_markdown_trees(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    bounds: MarkdownParseBounds,
) -> Result<MarkdownTrees, SyntaxError> {
    let block_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    let inline_language: tree_sitter::Language = tree_sitter_md::INLINE_LANGUAGE.into();
    let mut callback_budget = ProgressBudget::new(bounds.progress_callbacks_max);
    let block = parse_block_tree(source, &block_language, &mut callback_budget)?;
    let fact_kinds = markdown_fact_kinds(&block_language, &inline_language);
    let block_nodes = bounded_tree_nodes(
        block.root_node(),
        0,
        source.path,
        limits.syntax_nodes_max(),
        limits.syntax_depth_max(),
    )?;
    let (inline_nodes, inline_ranges) =
        inline_range_plan(source.path, &block_nodes, fact_kinds, bounds)?;
    let inline = parse_inline_trees(
        source,
        &inline_language,
        inline_nodes,
        inline_ranges,
        &mut callback_budget,
    )?;
    check_combined_node_bound(source.path, limits, block_nodes.len(), &inline)?;
    Ok(MarkdownTrees { block, inline })
}

fn parse_block_tree(
    source: SyntaxSource<'_>,
    language: &tree_sitter::Language,
    callback_budget: &mut ProgressBudget,
) -> Result<Tree, SyntaxError> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|_| incompatible_grammar(language))?;
    let mut input =
        |byte: usize, _point: Point| source.text.as_bytes().get(byte..).unwrap_or_default();
    let tree = {
        let mut progress = |_: &ParseState| callback_budget.call();
        parser.parse_with_options(
            &mut input,
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
    };
    tree.ok_or_else(|| {
        callback_budget
            .exhausted_error(source.path)
            .unwrap_or_else(|| {
                Error::new(SyntaxFault::ParseCancelled {
                    path: Some(source.path.clone()),
                })
            })
    })
}

fn inline_range_plan<'tree>(
    path: &ProjectPath,
    block_nodes: &'tree [BoundedNode<'tree>],
    kinds: &MarkdownFactKinds,
    bounds: MarkdownParseBounds,
) -> Result<(Vec<&'tree BoundedNode<'tree>>, Vec<Vec<Range>>), SyntaxError> {
    let inline_nodes: Vec<_> = block_nodes
        .iter()
        .filter(|node| {
            node.node.kind_id() == kinds.inline || node.node.kind_id() == kinds.pipe_table_cell
        })
        .collect();
    let inline_ranges = inline_nodes
        .iter()
        .map(|node| inline_parse_ranges(node.node, kinds.block_continuation))
        .collect::<Vec<_>>();
    let count = inline_ranges
        .iter()
        .try_fold(0_usize, |count, ranges| count.checked_add(ranges.len()))
        .ok_or_else(|| too_many_inline_ranges(path, bounds.inline_ranges_max, usize::MAX))?;
    if count > bounds.inline_ranges_max {
        return Err(too_many_inline_ranges(
            path,
            bounds.inline_ranges_max,
            count,
        ));
    }
    Ok((inline_nodes, inline_ranges))
}

fn too_many_inline_ranges(
    path: &ProjectPath,
    inline_ranges_max: usize,
    observed: usize,
) -> SyntaxError {
    Error::new(SyntaxFault::TooManyMarkdownInlineRanges {
        path: path.clone(),
        inline_ranges_max,
        observed,
    })
}

fn parse_inline_trees(
    source: SyntaxSource<'_>,
    language: &tree_sitter::Language,
    inline_nodes: Vec<&BoundedNode<'_>>,
    inline_ranges: Vec<Vec<Range>>,
    callback_budget: &mut ProgressBudget,
) -> Result<Vec<InlineTreeFact>, SyntaxError> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|_| incompatible_grammar(language))?;
    let mut inline = Vec::with_capacity(inline_nodes.len());
    for (node, ranges) in inline_nodes.into_iter().zip(inline_ranges) {
        let Some(tree) = parse_inline_tree(source, &mut parser, &ranges, callback_budget)? else {
            continue;
        };
        inline.push(InlineTreeFact {
            parent_id: node.node.id(),
            ranges,
            tree,
            base_depth: node.depth,
        });
    }
    Ok(inline)
}

fn parse_inline_tree(
    source: SyntaxSource<'_>,
    parser: &mut Parser,
    ranges: &[Range],
    callback_budget: &mut ProgressBudget,
) -> Result<Option<Tree>, SyntaxError> {
    if ranges.is_empty() {
        return Ok(None);
    }
    parser.set_included_ranges(ranges).map_err(|_| {
        Error::new(SyntaxFault::InvalidMarkdownRanges {
            path: source.path.clone(),
        })
    })?;
    let mut input =
        |byte: usize, _point: Point| source.text.as_bytes().get(byte..).unwrap_or_default();
    let tree = {
        let mut progress = |_: &ParseState| callback_budget.call();
        parser.parse_with_options(
            &mut input,
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
    };
    tree.map(Some).ok_or_else(|| {
        callback_budget
            .exhausted_error(source.path)
            .unwrap_or_else(|| {
                Error::new(SyntaxFault::ParseCancelled {
                    path: Some(source.path.clone()),
                })
            })
    })
}

fn check_combined_node_bound(
    path: &ProjectPath,
    limits: SyntaxLimits,
    mut node_count: usize,
    inline: &[InlineTreeFact],
) -> Result<(), SyntaxError> {
    for tree in inline {
        let remaining = limits.syntax_nodes_max().saturating_sub(node_count);
        let nodes = bounded_tree_nodes(
            tree.tree.root_node(),
            tree.base_depth,
            path,
            remaining,
            limits.syntax_depth_max(),
        )?;
        node_count = node_count.saturating_add(nodes.len());
    }
    Ok(())
}

/// Builds Markdown facts against symbols after their qualified names are disambiguated.
pub(crate) fn extract_markdown_facts(
    source: SyntaxSource<'_>,
    trees: &MarkdownTrees,
    syntax: &SyntaxDocument,
    limits: SyntaxLimits,
) -> Result<MarkdownFacts, SyntaxError> {
    let block_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    let inline_language: tree_sitter::Language = tree_sitter_md::INLINE_LANGUAGE.into();
    let kinds = markdown_fact_kinds(&block_language, &inline_language);
    let block_facts = extract_block_facts(source, trees, syntax, limits, kinds)?;
    let authored_facts =
        extract_authored_facts(source, trees, limits, kinds, &block_facts.node_blocks)?;
    let mut links = authored_facts.links;
    let mut reference_candidates = authored_facts.reference_candidates;
    let mut error_ranges = authored_facts.error_ranges;
    let block_root = trees.block.root_node();
    if block_root.has_error() && error_ranges.is_empty() {
        error_ranges.push(ByteRange {
            start: 0,
            end: u64::try_from(source.text.len()).unwrap_or(u64::MAX),
        });
    }
    let error_ranges = sorted_unique_ranges(error_ranges);
    let mut blocks = block_facts.blocks;
    blocks.retain(|block| {
        !error_ranges
            .iter()
            .any(|error| ranges_overlap(block.range, *error))
    });
    links.retain(|link| blocks.iter().any(|block| block.range == link.block_range));
    reference_candidates.retain(|candidate| {
        blocks
            .iter()
            .any(|block| block.range == candidate.block_range)
    });
    blocks.sort_by_key(|block| (block.range.start, block.range.end, block.structure as u8));
    links.sort_by_key(|link| (link.range.start, link.range.end, link.kind as u8));
    reference_candidates.sort_by_key(|candidate| (candidate.range.start, candidate.range.end));
    Ok(MarkdownFacts {
        blocks,
        headings: block_facts.headings,
        links,
        reference_candidates,
        error_ranges,
        omitted_ranges: Vec::new(),
    })
}

struct MarkdownBlockExtraction {
    blocks: Vec<MarkdownBlockFact>,
    headings: Vec<MarkdownHeadingFact>,
    node_blocks: HashMap<usize, ByteRange>,
}

fn extract_block_facts(
    source: SyntaxSource<'_>,
    trees: &MarkdownTrees,
    syntax: &SyntaxDocument,
    limits: SyntaxLimits,
    kinds: &MarkdownFactKinds,
) -> Result<MarkdownBlockExtraction, SyntaxError> {
    let block_nodes = bounded_tree_nodes(
        trees.block.root_node(),
        0,
        source.path,
        limits.syntax_nodes_max(),
        limits.syntax_depth_max(),
    )?;
    let line_starts = rift_core::line::line_starts(source.text);
    let mut headings = Vec::new();
    let mut heading_for_node = HashMap::new();
    let mut blocks = Vec::new();
    let mut node_blocks = HashMap::with_capacity(block_nodes.len());
    let mut stack = vec![(
        trees.block.root_node(),
        0_usize,
        None::<usize>,
        None::<ByteRange>,
    )];
    while let Some((node, depth, inherited_heading, inherited_block)) = stack.pop() {
        if depth > limits.syntax_depth_max() {
            return Err(Error::new(SyntaxFault::TooDeep {
                path: source.path.clone(),
                syntax_depth_max: limits.syntax_depth_max(),
            }));
        }
        let mut child_heading = inherited_heading;
        if node.kind_id() == kinds.section {
            if let Some(declared) = declaring_heading(node, kinds.atx_heading, kinds.setext_heading)
                && let Some(index) = add_heading(
                    declared,
                    inherited_heading,
                    source.text,
                    syntax,
                    &mut headings,
                )?
            {
                heading_for_node.insert(declared.id(), index);
                child_heading = Some(index);
            }
        } else if node.kind_id() == kinds.setext_heading
            && !declares_its_section(node, kinds.atx_heading, kinds.setext_heading)
            && let Some(index) =
                add_heading(node, inherited_heading, source.text, syntax, &mut headings)?
        {
            heading_for_node.insert(node.id(), index);
        }

        let mut current_block = inherited_block;
        if let Some(structure) = block_structure(node.kind_id(), kinds) {
            let range = extract::byte_range(node)?;
            current_block = Some(range);
            let heading = if structure == MarkdownBlockStructure::Heading {
                heading_for_node
                    .get(&node.id())
                    .copied()
                    .or(inherited_heading)
            } else {
                inherited_heading
            };
            let code_language = block_is_code(node.kind_id(), kinds)
                .then(|| code_language(node, source.text, kinds.info_string))
                .flatten();
            blocks.push(MarkdownBlockFact {
                range,
                line: rift_core::line::line_of(&line_starts, range.start),
                kind: if block_is_code(node.kind_id(), kinds) {
                    MarkdownBlockKind::Code
                } else {
                    MarkdownBlockKind::Prose
                },
                structure,
                heading,
                code_language,
            });
        }
        if let Some(block_range) = current_block {
            node_blocks.insert(node.id(), block_range);
        }

        for child_index in node.child_indices().rev() {
            let Some(child) = node.child(child_index) else {
                continue;
            };
            if !child.is_named() {
                continue;
            }
            stack.push((child, depth + 1, child_heading, current_block));
        }
    }

    Ok(MarkdownBlockExtraction {
        blocks,
        headings,
        node_blocks,
    })
}

struct MarkdownAuthoredExtraction {
    links: Vec<MarkdownLinkFact>,
    reference_candidates: Vec<MarkdownReferenceCandidate>,
    error_ranges: Vec<ByteRange>,
}

fn extract_authored_facts(
    source: SyntaxSource<'_>,
    trees: &MarkdownTrees,
    limits: SyntaxLimits,
    kinds: &MarkdownFactKinds,
    node_blocks: &HashMap<usize, ByteRange>,
) -> Result<MarkdownAuthoredExtraction, SyntaxError> {
    let mut links = Vec::new();
    let mut reference_candidates = Vec::new();
    let mut error_ranges = tree_error_ranges(trees.block.root_node())?;
    for inline_tree in &trees.inline {
        let block_range = node_blocks.get(&inline_tree.parent_id).copied();
        let Some(block_range) = block_range else {
            continue;
        };
        let root = inline_tree.tree.root_node();
        let error_count_before = error_ranges.len();
        append_tree_errors(root, &mut error_ranges)?;
        if root.has_error() && error_ranges.len() == error_count_before {
            for range in &inline_tree.ranges {
                error_ranges.push(ByteRange {
                    start: u64::try_from(range.start_byte).unwrap_or(u64::MAX),
                    end: u64::try_from(range.end_byte).unwrap_or(u64::MAX),
                });
            }
        }
        let inline_nodes = bounded_tree_nodes(
            root,
            inline_tree.base_depth,
            source.path,
            limits.syntax_nodes_max(),
            limits.syntax_depth_max(),
        )?;
        for entry in inline_nodes {
            if entry.node.kind_id() == kinds.code_span {
                let range = code_span_content_range(entry.node, kinds.code_span_delimiter)?;
                if range.start < range.end {
                    reference_candidates.push(MarkdownReferenceCandidate { block_range, range });
                }
            }
            collect_inline_link(entry.node, block_range, source.text, kinds, &mut links)?;
        }
    }
    collect_block_links(trees.block.root_node(), source.text, kinds, &mut links)?;
    Ok(MarkdownAuthoredExtraction {
        links,
        reference_candidates,
        error_ranges,
    })
}

#[derive(Debug, Clone, Copy)]
struct ProgressBudget {
    callbacks_max: usize,
    callbacks: usize,
    exhausted: bool,
}

impl ProgressBudget {
    const fn new(callbacks_max: usize) -> Self {
        Self {
            callbacks_max,
            callbacks: 0,
            exhausted: false,
        }
    }

    fn call(&mut self) -> ControlFlow<()> {
        if self.callbacks == self.callbacks_max {
            self.exhausted = true;
            return ControlFlow::Break(());
        }
        self.callbacks += 1;
        ControlFlow::Continue(())
    }

    fn exhausted_error(self, path: &ProjectPath) -> Option<SyntaxError> {
        self.exhausted.then(|| {
            Error::new(SyntaxFault::MarkdownProgressExceeded {
                path: path.clone(),
                progress_callbacks_max: self.callbacks_max,
            })
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct BoundedNode<'tree> {
    node: Node<'tree>,
    depth: usize,
}

#[derive(Debug)]
struct MarkdownFactKinds {
    section: u16,
    atx_heading: u16,
    setext_heading: u16,
    paragraph: u16,
    list: u16,
    pipe_table: u16,
    block_quote: u16,
    fenced_code_block: u16,
    indented_code_block: u16,
    link_reference_definition: u16,
    inline: u16,
    pipe_table_cell: u16,
    block_continuation: u16,
    info_string: u16,
    link_destination: u16,
    block_link_destination: u16,
    inline_link: u16,
    image: u16,
    full_reference_link: u16,
    collapsed_reference_link: u16,
    shortcut_link: u16,
    link_label: u16,
    link_text: u16,
    code_span: u16,
    code_span_delimiter: u16,
}

fn markdown_fact_kinds(
    block_language: &tree_sitter::Language,
    inline_language: &tree_sitter::Language,
) -> &'static MarkdownFactKinds {
    static KINDS: std::sync::OnceLock<MarkdownFactKinds> = std::sync::OnceLock::new();
    KINDS.get_or_init(|| MarkdownFactKinds {
        section: required_kind(block_language, "section"),
        atx_heading: required_kind(block_language, "atx_heading"),
        setext_heading: required_kind(block_language, "setext_heading"),
        paragraph: required_kind(block_language, "paragraph"),
        list: required_kind(block_language, "list"),
        pipe_table: required_kind(block_language, "pipe_table"),
        block_quote: required_kind(block_language, "block_quote"),
        fenced_code_block: required_kind(block_language, "fenced_code_block"),
        indented_code_block: required_kind(block_language, "indented_code_block"),
        link_reference_definition: required_kind(block_language, "link_reference_definition"),
        inline: required_kind(block_language, "inline"),
        pipe_table_cell: required_kind(block_language, "pipe_table_cell"),
        block_continuation: required_kind(block_language, "block_continuation"),
        info_string: required_kind(block_language, "info_string"),
        link_destination: required_kind(inline_language, "link_destination"),
        block_link_destination: required_kind(block_language, "link_destination"),
        inline_link: required_kind(inline_language, "inline_link"),
        image: required_kind(inline_language, "image"),
        full_reference_link: required_kind(inline_language, "full_reference_link"),
        collapsed_reference_link: required_kind(inline_language, "collapsed_reference_link"),
        shortcut_link: required_kind(inline_language, "shortcut_link"),
        link_label: required_kind(inline_language, "link_label"),
        link_text: required_kind(inline_language, "link_text"),
        code_span: required_kind(inline_language, "code_span"),
        code_span_delimiter: required_kind(inline_language, "code_span_delimiter"),
    })
}

fn required_kind(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(id != 0, "pinned Markdown grammar must define kind={kind}");
    id
}

fn bounded_tree_nodes<'tree>(
    root: Node<'tree>,
    base_depth: usize,
    path: &ProjectPath,
    nodes_max: usize,
    depth_max: usize,
) -> Result<Vec<BoundedNode<'tree>>, SyntaxError> {
    let mut result = Vec::new();
    let mut pending = vec![(root, base_depth)];
    while let Some((node, depth)) = pending.pop() {
        if depth > depth_max {
            return Err(Error::new(SyntaxFault::TooDeep {
                path: path.clone(),
                syntax_depth_max: depth_max,
            }));
        }
        if result.len() == nodes_max {
            return Err(Error::new(SyntaxFault::TooManyNodes {
                path: path.clone(),
                syntax_nodes_max: nodes_max,
            }));
        }
        result.push(BoundedNode { node, depth });
        for index in node.child_indices().rev() {
            let Some(child) = node.child(index) else {
                continue;
            };
            if !child.is_named() {
                continue;
            }
            if result.len() + pending.len() >= nodes_max {
                return Err(Error::new(SyntaxFault::TooManyNodes {
                    path: path.clone(),
                    syntax_nodes_max: nodes_max,
                }));
            }
            pending.push((child, depth + 1));
        }
    }
    Ok(result)
}

fn inline_parse_ranges(node: Node<'_>, block_continuation: u16) -> Vec<Range> {
    let mut ranges = Vec::new();
    let mut start_byte = node.start_byte();
    let mut start_point = node.start_position();
    for index in node.child_indices() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if !child.is_named() || child.kind_id() != block_continuation {
            continue;
        }
        let child_range = child.range();
        if start_byte < child_range.start_byte {
            ranges.push(Range {
                start_byte,
                start_point,
                end_byte: child_range.start_byte,
                end_point: child_range.start_point,
            });
        }
        start_byte = child_range.end_byte;
        start_point = child_range.end_point;
    }
    let end_byte = node.end_byte();
    if start_byte < end_byte {
        ranges.push(Range {
            start_byte,
            start_point,
            end_byte,
            end_point: node.end_position(),
        });
    }
    ranges
}

fn declaring_heading(section: Node<'_>, atx_heading: u16, setext_heading: u16) -> Option<Node<'_>> {
    let first = section.named_child(0)?;
    (first.kind_id() == atx_heading || first.kind_id() == setext_heading).then_some(first)
}

fn declares_its_section(node: Node<'_>, atx_heading: u16, setext_heading: u16) -> bool {
    node.parent()
        .and_then(|parent| declaring_heading(parent, atx_heading, setext_heading))
        .is_some_and(|heading| heading.id() == node.id())
}

fn add_heading(
    node: Node<'_>,
    parent: Option<usize>,
    source: &str,
    syntax: &SyntaxDocument,
    headings: &mut Vec<MarkdownHeadingFact>,
) -> Result<Option<usize>, SyntaxError> {
    let range = extract::byte_range(node)?;
    let Some((symbol_index, _)) = syntax
        .symbols()
        .iter()
        .enumerate()
        .find(|(_, symbol)| symbol.kind == "heading" && symbol.range.start == range.start)
    else {
        return Ok(None);
    };
    let level = heading_level(node, source);
    let index = headings.len();
    headings.push(MarkdownHeadingFact {
        symbol_index,
        range,
        level,
        parent,
    });
    Ok(Some(index))
}

fn heading_level(node: Node<'_>, source: &str) -> u8 {
    let Ok(range) = extract::byte_range(node) else {
        return 1;
    };
    let start = usize::try_from(range.start)
        .unwrap_or(source.len())
        .min(source.len());
    let end = usize::try_from(range.end)
        .unwrap_or(source.len())
        .min(source.len());
    let text = &source[start..end];
    if node.kind() == "atx_heading" {
        return u8::try_from(
            text.bytes()
                .take_while(|byte| *byte == b'#')
                .count()
                .clamp(1, 6),
        )
        .expect("heading level fits in u8");
    }
    let underline = text.split_inclusive('\n').nth(1).unwrap_or("").trim();
    if underline.starts_with('=') { 1 } else { 2 }
}

fn block_structure(node_kind: u16, kinds: &MarkdownFactKinds) -> Option<MarkdownBlockStructure> {
    if node_kind == kinds.atx_heading || node_kind == kinds.setext_heading {
        Some(MarkdownBlockStructure::Heading)
    } else if node_kind == kinds.paragraph {
        Some(MarkdownBlockStructure::Paragraph)
    } else if node_kind == kinds.list {
        Some(MarkdownBlockStructure::List)
    } else if node_kind == kinds.pipe_table {
        Some(MarkdownBlockStructure::Table)
    } else if node_kind == kinds.block_quote {
        Some(MarkdownBlockStructure::BlockQuote)
    } else if node_kind == kinds.fenced_code_block || node_kind == kinds.indented_code_block {
        Some(MarkdownBlockStructure::Code)
    } else if node_kind == kinds.link_reference_definition {
        Some(MarkdownBlockStructure::LinkDefinition)
    } else {
        None
    }
}

fn block_is_code(node_kind: u16, kinds: &MarkdownFactKinds) -> bool {
    node_kind == kinds.fenced_code_block || node_kind == kinds.indented_code_block
}

fn code_language(node: Node<'_>, source: &str, info_string_kind: u16) -> Option<String> {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.kind_id() == info_string_kind {
            let start = current.start_byte();
            let end = current.end_byte();
            let info = source.get(start..end)?.trim();
            return info.split_whitespace().next().map(str::to_owned);
        }
        for index in current.child_indices().rev() {
            if let Some(child) = current.child(index)
                && child.is_named()
            {
                pending.push(child);
            }
        }
    }
    None
}

fn collect_inline_link(
    node: Node<'_>,
    block_range: ByteRange,
    source: &str,
    kinds: &MarkdownFactKinds,
    links: &mut Vec<MarkdownLinkFact>,
) -> Result<(), SyntaxError> {
    let kind = node.kind_id();
    if kind == kinds.link_destination {
        let parent_link = nearest_ancestor(node, &[kinds.inline_link, kinds.image]);
        let Some(parent_link) = parent_link else {
            return Ok(());
        };
        let range = extract::byte_range(parent_link)?;
        let destination_range = extract::byte_range(node)?;
        links.push(MarkdownLinkFact {
            kind: MarkdownLinkKind::Authored,
            block_range,
            range,
            destination_range: Some(destination_range),
            fragment_range: fragment_range(source, destination_range),
            label_range: child_range(parent_link, kinds.link_text)?,
        });
    } else if kind == kinds.full_reference_link
        || kind == kinds.collapsed_reference_link
        || kind == kinds.shortcut_link
    {
        links.push(MarkdownLinkFact {
            kind: MarkdownLinkKind::ReferenceUse,
            block_range,
            range: extract::byte_range(node)?,
            destination_range: None,
            fragment_range: None,
            label_range: child_range(node, kinds.link_label)?
                .or(child_range(node, kinds.link_text)?),
        });
    }
    Ok(())
}

fn collect_block_links(
    root: Node<'_>,
    source: &str,
    kinds: &MarkdownFactKinds,
    links: &mut Vec<MarkdownLinkFact>,
) -> Result<(), SyntaxError> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind_id() == kinds.link_reference_definition {
            let range = extract::byte_range(node)?;
            let destination = child_node(node, kinds.block_link_destination);
            let label = child_node(node, kinds.link_label);
            let destination_range = destination.map(extract::byte_range).transpose()?;
            links.push(MarkdownLinkFact {
                kind: MarkdownLinkKind::ReferenceDefinition,
                block_range: range,
                range,
                destination_range,
                fragment_range: destination_range.and_then(|target| fragment_range(source, target)),
                label_range: label.map(extract::byte_range).transpose()?,
            });
        }
        for index in node.child_indices().rev() {
            if let Some(child) = node.child(index)
                && child.is_named()
            {
                pending.push(child);
            }
        }
    }
    Ok(())
}

fn child_node(node: Node<'_>, kind: u16) -> Option<Node<'_>> {
    node.child_indices()
        .filter_map(|index| node.child(index))
        .find(|child| child.kind_id() == kind)
}

fn child_range(node: Node<'_>, kind: u16) -> Result<Option<ByteRange>, SyntaxError> {
    child_node(node, kind).map(extract::byte_range).transpose()
}

fn nearest_ancestor<'tree>(node: Node<'tree>, kinds: &[u16]) -> Option<Node<'tree>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if kinds.contains(&parent.kind_id()) {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

fn fragment_range(source: &str, destination: ByteRange) -> Option<ByteRange> {
    let start = usize::try_from(destination.start).ok()?;
    let end = usize::try_from(destination.end).ok()?;
    let text = source.get(start..end)?;
    let fragment = text.find('#')? + 1;
    let tail = text[fragment..]
        .strip_suffix('>')
        .map_or(text.len(), |rest| rest.len() + fragment);
    Some(ByteRange {
        start: destination
            .start
            .saturating_add(u64::try_from(fragment).ok()?),
        end: destination.start.saturating_add(u64::try_from(tail).ok()?),
    })
}

fn code_span_content_range(node: Node<'_>, delimiter_kind: u16) -> Result<ByteRange, SyntaxError> {
    let mut first = None;
    let mut last = None;
    for index in node.child_indices() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if child.kind_id() == delimiter_kind {
            let range = extract::byte_range(child)?;
            first.get_or_insert(range);
            last = Some(range);
        }
    }
    let full = extract::byte_range(node)?;
    let Some((first, last)) = first.zip(last) else {
        return Ok(full);
    };
    Ok(ByteRange {
        start: first.end,
        end: last.start.max(first.end),
    })
}

fn tree_error_ranges(root: Node<'_>) -> Result<Vec<ByteRange>, SyntaxError> {
    let mut ranges = Vec::new();
    append_tree_errors(root, &mut ranges)?;
    Ok(ranges)
}

fn append_tree_errors(root: Node<'_>, ranges: &mut Vec<ByteRange>) -> Result<(), SyntaxError> {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_error() || node.is_missing() {
            ranges.push(extract::byte_range(node)?);
        }
        for index in node.child_indices().rev() {
            if let Some(child) = node.child(index)
                && child.is_named()
            {
                pending.push(child);
            }
        }
    }
    Ok(())
}

fn mdx_block_is_omitted(
    block: &MarkdownBlockFact,
    source: &str,
    code_candidates: &[MarkdownReferenceCandidate],
) -> bool {
    if block.kind == MarkdownBlockKind::Code {
        return false;
    }
    let Ok(start) = usize::try_from(block.range.start) else {
        return true;
    };
    let Ok(end) = usize::try_from(block.range.end) else {
        return true;
    };
    let Some(text) = source.get(start..end) else {
        return true;
    };
    let mut cursor = 0_usize;
    for code in code_candidates
        .iter()
        .filter(|candidate| candidate.block_range == block.range)
    {
        let code_start = usize::try_from(code.range.start)
            .unwrap_or(end)
            .saturating_sub(start);
        let code_end = usize::try_from(code.range.end)
            .unwrap_or(end)
            .saturating_sub(start);
        if text
            .get(cursor..code_start.min(text.len()))
            .is_some_and(contains_mdx_marker)
        {
            return true;
        }
        cursor = cursor.max(code_end.min(text.len()));
    }
    if text.get(cursor..).is_some_and(contains_mdx_marker) {
        return true;
    }
    matches!(text.split_whitespace().next(), Some("import" | "export"))
}

fn contains_mdx_marker(text: &str) -> bool {
    text.bytes()
        .any(|byte| matches!(byte, b'{' | b'}' | b'<' | b'>'))
}

fn nearest_kept_heading(
    mut heading: Option<usize>,
    omitted: &[bool],
    headings: &[MarkdownHeadingFact],
) -> Option<usize> {
    while let Some(index) = heading {
        if !omitted.get(index).copied().unwrap_or(true) {
            return Some(index);
        }
        heading = headings.get(index).and_then(|item| item.parent);
    }
    None
}

fn ranges_overlap(left: ByteRange, right: ByteRange) -> bool {
    left.start < right.end && right.start < left.end
        || left.start == left.end && right.start <= left.start && left.start <= right.end
        || right.start == right.end && left.start <= right.start && right.start <= left.end
}

fn sorted_unique_ranges(mut ranges: Vec<ByteRange>) -> Vec<ByteRange> {
    ranges.sort();
    ranges.dedup();
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_range_limit_is_checked_before_inline_parse() {
        let text = "[one](target) and `Name`\n";
        let path = ProjectPath::new("docs/facts.md").expect("valid path");
        let source = SyntaxSource { path: &path, text };
        let error = parse_markdown_trees(
            source,
            SyntaxLimits::declared(1024, 128, 16),
            MarkdownParseBounds {
                progress_callbacks_max: MARKDOWN_PROGRESS_CALLBACKS_MAX,
                inline_ranges_max: 0,
            },
        )
        .expect_err("inline ranges exceed zero limit");
        assert!(matches!(
            error.fault(),
            SyntaxFault::TooManyMarkdownInlineRanges {
                inline_ranges_max: 0,
                observed: 1..,
                ..
            }
        ));
    }

    #[test]
    fn progress_callback_limit_is_shared_by_block_parse() {
        let text = "word ".repeat(20_000);
        let path = ProjectPath::new("docs/facts.md").expect("valid path");
        let source = SyntaxSource {
            path: &path,
            text: &text,
        };
        let error = parse_markdown_trees(
            source,
            SyntaxLimits::declared(text.len(), 100_000, 128),
            MarkdownParseBounds {
                progress_callbacks_max: 0,
                inline_ranges_max: MARKDOWN_INLINE_RANGES_MAX,
            },
        )
        .expect_err("block parser exceeds zero callbacks");
        assert!(matches!(
            error.fault(),
            SyntaxFault::MarkdownProgressExceeded {
                progress_callbacks_max: 0,
                ..
            }
        ));
    }
}
