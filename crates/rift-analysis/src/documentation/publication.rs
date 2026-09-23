//! Validates documentation metadata and computes complete replacement sets.

use std::collections::{BTreeMap, BTreeSet};

#[cfg(feature = "collector")]
use rift_protocol::documentation::DocumentationStage;
use rift_protocol::documentation::{
    DOCUMENTATION_BLOCKS_MAX, DOCUMENTATION_HEADING_DEPTH_MAX, DOCUMENTATION_REFERENCES_MAX,
    DOCUMENTATION_SOURCES_MAX, DOCUMENTATION_TEXT_BYTES_MAX, DOCUMENTATION_TOTAL_BYTES_MAX,
    DOCUMENTATION_WARNINGS_MAX, DocumentationBlock, DocumentationBlockKind,
    DocumentationContentIdentity, DocumentationDigest, DocumentationIndex, DocumentationLink,
    DocumentationLinkResolution, DocumentationReference, DocumentationSource, DocumentationTarget,
    DocumentationWarning, DocumentationWarningKind,
};
use rift_protocol::read::{Digest, SymbolId, TextRange};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::{is_digest, is_revision_digest};
use super::input::{source_selection_digest, validate_identity, validate_source_metadata};

/// Validates one read payload without requiring its remote revision to equal this build.
///
/// Callers compare the payload revision with their negotiated or captured publication.
///
/// # Errors
///
/// Refuses source, block, identity, range, digest, and count violations.
pub fn validate_documentation_hit(
    hit: &rift_protocol::documentation::DocumentationHit,
) -> Result<(), DocumentationError> {
    if !is_revision_digest(&hit.documentation_revision) || hit.block.source != hit.source.identity {
        return Err(refused(DocumentationViolation::Identity, "documentation"));
    }
    validate_source_metadata(&hit.source)?;
    validate_block(&hit.block, &hit.source)
}

/// Validates exact-symbol context, source facts, reference ordering, and total excerpt bytes.
///
/// # Errors
///
/// Refuses mismatched references, revisions, duplicate identities, and output bounds.
pub fn validate_documentation_context(
    context: &rift_protocol::documentation::DocumentationContext,
    symbol: &SymbolId,
) -> Result<(), DocumentationError> {
    use rift_protocol::documentation::{
        DOCUMENTATION_EXCERPT_BYTES_MAX, DOCUMENTATION_SYMBOL_REFERENCES_MAX,
    };
    if !is_revision_digest(&context.documentation_revision)
        || !valid_symbol_identity(symbol)
        || context.references.len() > DOCUMENTATION_SYMBOL_REFERENCES_MAX as usize
        || context.warnings.len() > DOCUMENTATION_WARNINGS_MAX as usize
    {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "documentation",
        ));
    }
    let mut identities = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut previous = None;
    let mut excerpt_bytes = 0_usize;
    for hit in &context.references {
        validate_documentation_hit(&hit.documentation)?;
        let reference = &hit.reference;
        let block = &hit.documentation.block;
        let source = &hit.documentation.source;
        if reference.target != *symbol
            || reference.block != block.identity
            || block.symbol.is_some()
            || !is_digest(&reference.identity)
            || !identities.insert(&reference.identity)
            || !valid_text(&reference.authored)
            || !contains(&block.range, &reference.range)
            || hit.documentation.documentation_revision != context.documentation_revision
        {
            return Err(refused(DocumentationViolation::Identity, "reference"));
        }
        if let Some(prior) = sources.insert(&source.identity, source)
            && prior != source
        {
            return Err(refused(DocumentationViolation::Identity, "source"));
        }
        let order = (
            reference.evidence,
            &block.source,
            block.range.start,
            &block.identity,
            reference.range.start,
        );
        if previous.is_some_and(|prior| prior >= order) {
            return Err(refused(DocumentationViolation::Order, "references"));
        }
        previous = Some(order);
        let bytes = hit.excerpt.as_ref().map_or(0, String::len);
        excerpt_bytes = excerpt_bytes
            .checked_add(bytes)
            .filter(|total| *total <= DOCUMENTATION_EXCERPT_BYTES_MAX as usize)
            .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "excerpt"))?;
        if bytes as u64 > block.range.end - block.range.start {
            return Err(refused(DocumentationViolation::Range, "excerpt"));
        }
    }
    // A bounded read may warn about the first reference outside its returned prefix.
    validate_warnings(&context.warnings, &sources, false)
}

/// A validated documentation index with exact-symbol reverse references.
#[derive(Debug)]
pub struct DocumentationCollection {
    index: DocumentationIndex,
    references: BTreeMap<SymbolId, Vec<usize>>,
    blocks: BTreeMap<DocumentationDigest, usize>,
    sources: BTreeMap<DocumentationContentIdentity, usize>,
    #[cfg(feature = "collector")]
    extraction_cache: Option<super::collect::ExtractionCache>,
    #[cfg(feature = "collector")]
    resolution_cache: Option<super::resolution::ResolutionCache>,
}

impl DocumentationCollection {
    /// Validates a bounded set of stored blocks and their source facts for search projection.
    ///
    /// The caller reads every record from one immutable publication set and checks its
    /// revision before this call. This view contains no links or reverse references;
    /// it does not replace the persisted complete collection or its selection digest.
    ///
    /// # Errors
    ///
    /// Refuses incompatible revisions, duplicate identities, invalid ranges, or bounds.
    pub fn from_candidate_blocks(
        documentation_revision: Digest,
        mut sources: Vec<DocumentationSource>,
        mut blocks: Vec<DocumentationBlock>,
    ) -> Result<Self, DocumentationError> {
        if sources.len() > DOCUMENTATION_SOURCES_MAX as usize
            || blocks.len() > DOCUMENTATION_BLOCKS_MAX as usize
        {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.records",
            ));
        }
        sources.sort_by(|left, right| left.identity.cmp(&right.identity));
        blocks.sort_by(|left, right| {
            (&left.source, left.range.start, left.kind, &left.identity).cmp(&(
                &right.source,
                right.range.start,
                right.kind,
                &right.identity,
            ))
        });
        let selected = u32::try_from(sources.len())
            .map_err(|_| refused(DocumentationViolation::LimitExceeded, "sources"))?;
        let selection_digest = source_selection_digest(sources.iter())?;
        Self::new(DocumentationIndex {
            documentation_revision,
            selection_digest,
            sources,
            blocks,
            links: Vec::new(),
            references: Vec::new(),
            unresolved_references: Vec::new(),
            coverage: rift_protocol::documentation::DocumentationCoverage {
                selected,
                parsed: selected,
                omitted: 0,
                truncated: 0,
            },
            warnings: Vec::new(),
        })
    }

    /// Validates a complete candidate before its caller publishes it.
    ///
    /// The caller keeps its prior collection when this candidate is refused.
    /// Validation uses ordered lookups under source, block, and reference bounds.
    ///
    /// # Errors
    ///
    /// Returns a typed refusal for invalid identities, ranges, ordering, or relationships.
    pub fn new(index: DocumentationIndex) -> Result<Self, DocumentationError> {
        validate_counts(&index)?;
        if index.documentation_revision != super::documentation_revision() {
            return Err(refused(
                DocumentationViolation::Revision,
                "documentation_revision",
            ));
        }
        let sources = validate_sources(&index)?;
        let selection_digest = source_selection_digest(index.sources.iter())?;
        if index.selection_digest != selection_digest {
            return Err(refused(DocumentationViolation::Digest, "selection_digest"));
        }
        let blocks = validate_blocks(&index, &sources)?;
        validate_links(&index, &sources, &blocks)?;
        validate_warnings(&index.warnings, &sources, true)?;
        let mut references = validate_references(&index, &blocks)?;
        for positions in references.values_mut() {
            positions.sort_by_key(|position| {
                let reference = &index.references[*position];
                let block = blocks[&reference.block];
                (
                    reference.evidence,
                    &block.source,
                    block.range.start,
                    &block.identity,
                    reference.range.start,
                )
            });
        }
        let blocks = index
            .blocks
            .iter()
            .enumerate()
            .map(|(position, block)| (block.identity.clone(), position))
            .collect();
        let sources = index
            .sources
            .iter()
            .enumerate()
            .map(|(position, source)| (source.identity.clone(), position))
            .collect();
        Ok(Self {
            index,
            references,
            blocks,
            sources,
            #[cfg(feature = "collector")]
            extraction_cache: None,
            #[cfg(feature = "collector")]
            resolution_cache: None,
        })
    }

    #[cfg(feature = "collector")]
    pub(super) fn with_extraction_cache(mut self, cache: super::collect::ExtractionCache) -> Self {
        self.extraction_cache = Some(cache);
        self
    }

    #[cfg(feature = "collector")]
    pub(super) fn extraction_cache(&self) -> Option<&super::collect::ExtractionCache> {
        self.extraction_cache.as_ref()
    }

    #[cfg(feature = "collector")]
    pub(super) fn resolution_cache(&self) -> Option<&super::resolution::ResolutionCache> {
        self.resolution_cache.as_ref()
    }

    #[cfg(feature = "collector")]
    pub(super) fn with_resolution_cache(
        mut self,
        cache: Option<super::resolution::ResolutionCache>,
    ) -> Self {
        self.resolution_cache = cache;
        self
    }

    #[cfg(feature = "collector")]
    /// Appends source warnings and omissions, then validates complete publication.
    ///
    /// # Errors
    ///
    /// Refuses omission counts outside the documentation source bound or invalid metadata.
    pub fn with_source_omissions(
        self,
        omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    ) -> Result<Self, DocumentationError> {
        if omissions.len() > DOCUMENTATION_SOURCES_MAX as usize {
            return Err(refused(DocumentationViolation::LimitExceeded, "sources"));
        }
        let omitted_count = u32::try_from(omissions.len())
            .map_err(|_| refused(DocumentationViolation::LimitExceeded, "sources"))?;
        let DocumentationCollection {
            mut index,
            #[cfg(feature = "collector")]
            extraction_cache,
            #[cfg(feature = "collector")]
            resolution_cache,
            ..
        } = self;
        index.coverage.selected = index
            .coverage
            .selected
            .checked_add(omitted_count)
            .filter(|count| *count <= DOCUMENTATION_SOURCES_MAX)
            .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "sources"))?;
        index.coverage.omitted = index
            .coverage
            .omitted
            .checked_add(omitted_count)
            .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "sources"))?;
        let warning_slots =
            (DOCUMENTATION_WARNINGS_MAX as usize).saturating_sub(index.warnings.len());
        for (source, kind) in omissions.into_iter().take(warning_slots) {
            index.warnings.push(DocumentationWarning {
                source,
                stage: DocumentationStage::Source,
                kind,
                count: 1,
            });
        }
        let mut collection = Self::new(index)?;
        #[cfg(feature = "collector")]
        {
            collection.extraction_cache = extraction_cache;
            collection.resolution_cache = resolution_cache;
        }
        Ok(collection)
    }

    /// Returns the complete metadata publication.
    #[must_use]
    pub const fn index(&self) -> &DocumentationIndex {
        &self.index
    }

    /// Looks up one block by its exact identity.
    #[must_use]
    pub fn block(&self, identity: &DocumentationDigest) -> Option<&DocumentationBlock> {
        self.blocks
            .get(identity)
            .map(|position| &self.index.blocks[*position])
    }

    /// Looks up one content owner's source facts.
    #[must_use]
    pub fn source(&self, identity: &DocumentationContentIdentity) -> Option<&DocumentationSource> {
        self.sources
            .get(identity)
            .map(|position| &self.index.sources[*position])
    }

    /// Takes the complete metadata publication for persistence.
    #[must_use]
    pub fn into_index(self) -> DocumentationIndex {
        self.index
    }

    /// Returns references to one exact declaration without repeating name matching.
    pub fn references_to<'collection>(
        &'collection self,
        symbol: &SymbolId,
    ) -> impl Iterator<Item = &'collection DocumentationReference> {
        self.references
            .get(symbol)
            .into_iter()
            .flatten()
            .map(|position| &self.index.references[*position])
    }

    /// Computes block and reference changes against a previous complete collection.
    #[must_use]
    pub fn changes_from(&self, previous: Option<&Self>) -> DocumentationChanges {
        let blocks: BTreeMap<_, _> = self
            .index
            .blocks
            .iter()
            .map(|block| (&block.identity, block))
            .collect();
        let references: BTreeMap<_, _> = self
            .index
            .references
            .iter()
            .map(|reference| (&reference.identity, reference))
            .collect();
        let prior_blocks = previous
            .map(|prior| {
                prior
                    .index
                    .blocks
                    .iter()
                    .map(|block| (&block.identity, block))
                    .collect()
            })
            .unwrap_or_default();
        let prior_references = previous
            .map(|prior| {
                prior
                    .index
                    .references
                    .iter()
                    .map(|reference| (&reference.identity, reference))
                    .collect()
            })
            .unwrap_or_default();
        DocumentationChanges {
            sources: source_changes(
                &self.index.sources,
                previous.map(|p| p.index.sources.as_slice()),
            ),
            blocks: changes(&blocks, &prior_blocks),
            links: link_changes(
                &self.index.links,
                previous.map(|p| p.index.links.as_slice()),
            ),
            references: changes(&references, &prior_references),
        }
    }
}

/// Added, replaced, and removed identities for one metadata record family.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DocumentationRecordChanges {
    /// Records absent from the prior collection.
    pub added: Vec<DocumentationDigest>,
    /// Stable identities whose content or metadata changed.
    pub replaced: Vec<DocumentationDigest>,
    /// Prior records absent from the current collection.
    pub removed: Vec<DocumentationDigest>,
}

/// Source identities added, replaced, or removed by one collection.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DocumentationSourceChanges {
    /// Source identities absent from the prior collection.
    pub added: Vec<DocumentationContentIdentity>,
    /// Stable source identities whose accepted metadata changed.
    pub replaced: Vec<DocumentationContentIdentity>,
    /// Prior source identities absent from the current collection.
    pub removed: Vec<DocumentationContentIdentity>,
}

/// Authored links added, replaced, or removed by one collection.
#[derive(Debug, Default, PartialEq)]
pub struct DocumentationLinkChanges {
    /// Links absent from the prior collection.
    pub added: Vec<DocumentationLink>,
    /// Links at the same block range whose destination or resolution changed.
    pub replaced: Vec<DocumentationLinkReplacement>,
    /// Prior links absent from the current collection.
    pub removed: Vec<DocumentationLink>,
}

/// Previous and current links at one stable block range.
#[derive(Debug, PartialEq)]
pub struct DocumentationLinkReplacement {
    /// Prior link record.
    pub previous: DocumentationLink,
    /// Current link record.
    pub current: DocumentationLink,
}

/// Complete changes computed before documentation publication.
#[derive(Debug, PartialEq)]
pub struct DocumentationChanges {
    /// Changes to selected source owners.
    pub sources: DocumentationSourceChanges,
    /// Changes to addressed prose and code blocks.
    pub blocks: DocumentationRecordChanges,
    /// Changes to authored destinations and their resolutions.
    pub links: DocumentationLinkChanges,
    /// Changes to exact declaration references.
    pub references: DocumentationRecordChanges,
}

fn source_changes(
    current: &[DocumentationSource],
    previous: Option<&[DocumentationSource]>,
) -> DocumentationSourceChanges {
    let current: BTreeMap<_, _> = current
        .iter()
        .map(|source| (&source.identity, source))
        .collect();
    let previous: BTreeMap<_, _> = previous
        .into_iter()
        .flatten()
        .map(|source| (&source.identity, source))
        .collect();
    let mut changes = DocumentationSourceChanges::default();
    for (identity, source) in &current {
        match previous.get(identity) {
            None => changes.added.push((*identity).clone()),
            Some(before) if before != source => changes.replaced.push((*identity).clone()),
            Some(_) => {}
        }
    }
    changes.removed = previous
        .keys()
        .filter(|identity| !current.contains_key(*identity))
        .map(|identity| (*identity).clone())
        .collect();
    changes
}

type LinkAddress = (DocumentationDigest, u64, u64);

fn link_changes(
    current: &[DocumentationLink],
    previous: Option<&[DocumentationLink]>,
) -> DocumentationLinkChanges {
    let current: BTreeMap<_, _> = current
        .iter()
        .map(|link| (link_address(link), link))
        .collect();
    let previous: BTreeMap<_, _> = previous
        .into_iter()
        .flatten()
        .map(|link| (link_address(link), link))
        .collect();
    let mut changes = DocumentationLinkChanges::default();
    for (address, link) in &current {
        match previous.get(address) {
            None => changes.added.push((*link).clone()),
            Some(before) if before != link => {
                changes.replaced.push(DocumentationLinkReplacement {
                    previous: (*before).clone(),
                    current: (*link).clone(),
                });
            }
            Some(_) => {}
        }
    }
    changes.removed = previous
        .iter()
        .filter(|(address, _)| !current.contains_key(address))
        .map(|(_, link)| (*link).clone())
        .collect();
    changes
}

fn link_address(link: &DocumentationLink) -> LinkAddress {
    (link.block.clone(), link.range.start, link.range.end)
}

fn changes<T: PartialEq>(
    current: &BTreeMap<&DocumentationDigest, &T>,
    previous: &BTreeMap<&DocumentationDigest, &T>,
) -> DocumentationRecordChanges {
    let mut changes = DocumentationRecordChanges::default();
    for (identity, value) in current {
        match previous.get(identity) {
            None => changes.added.push((*identity).clone()),
            Some(before) if before != value => changes.replaced.push((*identity).clone()),
            Some(_) => {}
        }
    }
    changes.removed = previous
        .keys()
        .filter(|identity| !current.contains_key(*identity))
        .map(|identity| (*identity).clone())
        .collect();
    changes
}

fn validate_counts(index: &DocumentationIndex) -> Result<(), DocumentationError> {
    let counts = [
        ("sources", index.sources.len(), DOCUMENTATION_SOURCES_MAX),
        ("blocks", index.blocks.len(), DOCUMENTATION_BLOCKS_MAX),
        ("links", index.links.len(), DOCUMENTATION_REFERENCES_MAX),
        (
            "references",
            index.references.len(),
            DOCUMENTATION_REFERENCES_MAX,
        ),
        (
            "unresolved_references",
            index.unresolved_references.len(),
            DOCUMENTATION_REFERENCES_MAX,
        ),
        ("warnings", index.warnings.len(), DOCUMENTATION_WARNINGS_MAX),
    ];
    if let Some((field, _, _)) = counts
        .into_iter()
        .find(|(_, count, bound)| *count > *bound as usize)
    {
        return Err(refused(DocumentationViolation::LimitExceeded, field));
    }
    if !is_revision_digest(&index.documentation_revision) {
        return Err(refused(
            DocumentationViolation::Revision,
            "documentation_revision",
        ));
    }
    if !is_digest(&index.selection_digest) {
        return Err(refused(DocumentationViolation::Digest, "selection_digest"));
    }
    let accounted = index.coverage.parsed.checked_add(index.coverage.omitted);
    let counts_match = accounted == Some(index.coverage.selected);
    let truncation_bounded = index.coverage.truncated <= index.coverage.parsed;
    // Acquisition can omit a source before a complete source record exists. Those
    // sources still count as selected and omitted and carry a source warning.
    let retained = u32::try_from(index.sources.len()).unwrap_or(u32::MAX);
    let selected_matches = retained >= index.coverage.parsed
        && retained <= index.coverage.selected
        && index.coverage.selected <= DOCUMENTATION_SOURCES_MAX;
    if !counts_match || !truncation_bounded || !selected_matches {
        return Err(refused(DocumentationViolation::LimitExceeded, "coverage"));
    }
    Ok(())
}

fn validate_sources(
    index: &DocumentationIndex,
) -> Result<BTreeMap<&DocumentationContentIdentity, &DocumentationSource>, DocumentationError> {
    let mut sources = BTreeMap::new();
    let mut previous = None;
    let mut total_bytes = 0_u64;
    for source in &index.sources {
        validate_source_metadata(source)?;
        if previous.is_some_and(|identity| identity >= &source.identity) {
            return Err(refused(DocumentationViolation::Order, "sources"));
        }
        total_bytes = total_bytes
            .checked_add(source.byte_length)
            .filter(|bytes| *bytes <= DOCUMENTATION_TOTAL_BYTES_MAX)
            .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "source_bytes"))?;
        previous = Some(&source.identity);
        sources.insert(&source.identity, source);
    }
    Ok(sources)
}

fn validate_blocks<'index>(
    index: &'index DocumentationIndex,
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
) -> Result<BTreeMap<&'index DocumentationDigest, &'index DocumentationBlock>, DocumentationError> {
    let mut blocks = BTreeMap::new();
    let mut previous = None;
    for block in &index.blocks {
        let source = sources
            .get(&block.source)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.source"))?;
        validate_block(block, source)?;
        let order = (
            &block.source,
            block.range.start,
            block.kind,
            &block.identity,
        );
        if previous.is_some_and(|prior| prior >= order) {
            return Err(refused(DocumentationViolation::Order, "blocks"));
        }
        if blocks.insert(&block.identity, block).is_some() {
            return Err(refused(DocumentationViolation::Identity, "block.identity"));
        }
        previous = Some(order);
    }
    Ok(blocks)
}

fn validate_block(
    block: &DocumentationBlock,
    source: &DocumentationSource,
) -> Result<(), DocumentationError> {
    let range_valid = block.range.end > block.range.start && block.range.end <= source.byte_length;
    let line_valid = block.line > 0 && block.line <= source.byte_length;
    if !range_valid || !line_valid {
        return Err(refused(DocumentationViolation::Range, "block.range"));
    }
    if !is_digest(&block.identity) || !is_digest(&block.content_digest) {
        return Err(refused(DocumentationViolation::Digest, "block.identity"));
    }
    let symbol_valid = block.symbol.as_ref().is_none_or(valid_symbol_identity);
    let attached_comment_matches = (source.format
        == rift_protocol::documentation::DocumentationSourceFormat::AttachedComment)
        == block.symbol.is_some();
    let language_valid = block
        .language
        .as_ref()
        .is_none_or(|language| valid_text(language));
    let prose_valid = block.kind != DocumentationBlockKind::Prose || block.language.is_none();
    if !symbol_valid {
        return Err(refused(DocumentationViolation::Identity, "block.symbol"));
    }
    if !attached_comment_matches || !language_valid || !prose_valid {
        return Err(refused(DocumentationViolation::Format, "block.language"));
    }
    validate_headings(block)?;
    validate_chunks(block, source)
}

fn valid_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= DOCUMENTATION_TEXT_BYTES_MAX as usize
}

fn valid_symbol_identity(symbol: &SymbolId) -> bool {
    rift_core::parse_symbol_identity(&symbol.0).is_ok()
}

fn validate_headings(block: &DocumentationBlock) -> Result<(), DocumentationError> {
    if block.heading_path.len() > DOCUMENTATION_HEADING_DEPTH_MAX as usize {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "heading_path",
        ));
    }
    let mut previous_level = 0;
    for heading in &block.heading_path {
        let ordered =
            heading.level > previous_level && heading.level <= DOCUMENTATION_HEADING_DEPTH_MAX;
        if !ordered || !valid_text(&heading.name) {
            return Err(refused(DocumentationViolation::Order, "heading_path"));
        }
        previous_level = heading.level;
    }
    Ok(())
}

fn validate_chunks(
    block: &DocumentationBlock,
    source: &DocumentationSource,
) -> Result<(), DocumentationError> {
    if block.chunks.len() > DOCUMENTATION_BLOCKS_MAX as usize {
        return Err(refused(DocumentationViolation::LimitExceeded, "chunks"));
    }
    let mut identities = BTreeSet::new();
    let mut previous_start = 0;
    for chunk in &block.chunks {
        let valid_identity = rift_ranking::DocumentIdentity::new(&chunk.identity).is_ok()
            && identities.insert(&chunk.identity);
        let ordered = chunk.range.start >= previous_start;
        let valid_range =
            chunk.range.end > chunk.range.start && chunk.range.end <= source.byte_length;
        let intersects = chunk.range.start < block.range.end && block.range.start < chunk.range.end;
        if !valid_identity || !ordered || !valid_range || !intersects {
            return Err(refused(DocumentationViolation::Range, "chunks"));
        }
        previous_start = chunk.range.start;
    }
    Ok(())
}

fn contains(outer: &TextRange, inner: &TextRange) -> bool {
    outer.start <= inner.start && inner.start < inner.end && inner.end <= outer.end
}

fn contains_link_range(block: &DocumentationBlock, range: &TextRange) -> bool {
    if range.start == range.end {
        block.range.start <= range.start && range.start <= block.range.end
    } else {
        contains(&block.range, range)
    }
}

fn validate_links(
    index: &DocumentationIndex,
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
    blocks: &BTreeMap<&DocumentationDigest, &DocumentationBlock>,
) -> Result<(), DocumentationError> {
    let mut addresses = BTreeSet::new();
    for link in &index.links {
        let block = blocks
            .get(&link.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "link.block"))?;
        let unique = addresses.insert(link_address(link));
        let range_valid = contains_link_range(block, &link.range);
        let text_bounded = link.authored.len() <= DOCUMENTATION_TEXT_BYTES_MAX as usize;
        if !unique || !range_valid || !text_bounded {
            return Err(refused(DocumentationViolation::Range, "link"));
        }
        if let DocumentationLinkResolution::Resolved { target } = &link.resolution {
            validate_link_target(target, sources, blocks)?;
        }
    }
    Ok(())
}

fn validate_link_target(
    target: &DocumentationTarget,
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
    blocks: &BTreeMap<&DocumentationDigest, &DocumentationBlock>,
) -> Result<(), DocumentationError> {
    match target {
        DocumentationTarget::Block { identity } => {
            if blocks.contains_key(identity) {
                Ok(())
            } else {
                Err(refused(
                    DocumentationViolation::MissingTarget,
                    "link.target.block",
                ))
            }
        }
        DocumentationTarget::Source { source, range } => {
            let target = sources.get(source).ok_or_else(|| {
                refused(DocumentationViolation::MissingTarget, "link.target.source")
            })?;
            let empty_source_range = target.byte_length == 0 && range.start == 0 && range.end == 0;
            let valid_range = range.start <= range.end
                && range.end <= target.byte_length
                && (range.start < range.end || empty_source_range);
            if valid_range {
                Ok(())
            } else {
                Err(refused(DocumentationViolation::Range, "link.target.range"))
            }
        }
        DocumentationTarget::Symbol { symbol } => {
            if valid_symbol_identity(symbol) {
                Ok(())
            } else {
                Err(refused(
                    DocumentationViolation::Identity,
                    "link.target.symbol",
                ))
            }
        }
    }
}

fn validate_warnings(
    warnings: &[DocumentationWarning],
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
    complete: bool,
) -> Result<(), DocumentationError> {
    let mut prior = Vec::new();
    for warning in warnings {
        validate_identity(&warning.source)?;
        if warning.count == 0 {
            return Err(refused(DocumentationViolation::Range, "warning.count"));
        }
        let key = (&warning.source, warning.stage, warning.kind);
        if prior.contains(&key) {
            return Err(refused(DocumentationViolation::Identity, "warning"));
        }
        prior.push(key);
        let may_name_omitted_source = matches!(
            warning.kind,
            DocumentationWarningKind::SourceUnavailable
                | DocumentationWarningKind::UnsupportedFormat
                | DocumentationWarningKind::MalformedSource
        );
        if complete && !sources.contains_key(&warning.source) && !may_name_omitted_source {
            return Err(refused(
                DocumentationViolation::MissingTarget,
                "warning.source",
            ));
        }
    }
    Ok(())
}

fn validate_references(
    index: &DocumentationIndex,
    blocks: &BTreeMap<&DocumentationDigest, &DocumentationBlock>,
) -> Result<BTreeMap<SymbolId, Vec<usize>>, DocumentationError> {
    let mut identities = BTreeSet::new();
    let mut reverse: BTreeMap<SymbolId, Vec<usize>> = BTreeMap::new();
    let mut previous = None;
    for (position, reference) in index.references.iter().enumerate() {
        let block = blocks
            .get(&reference.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "reference.block"))?;
        let unique = identities.insert(&reference.identity);
        let symbol_valid = valid_symbol_identity(&reference.target);
        if !unique
            || !symbol_valid
            || !valid_text(&reference.authored)
            || !is_digest(&reference.identity)
        {
            return Err(refused(
                DocumentationViolation::Identity,
                "reference.identity",
            ));
        }
        if !contains(&block.range, &reference.range) || block.symbol.is_some() {
            return Err(refused(DocumentationViolation::Range, "reference.range"));
        }
        let order = (
            reference.evidence,
            &reference.block,
            reference.range.start,
            &reference.identity,
        );
        if previous.is_some_and(|prior| prior >= order) {
            return Err(refused(DocumentationViolation::Order, "references"));
        }
        previous = Some(order);
        reverse
            .entry(reference.target.clone())
            .or_default()
            .push(position);
    }
    for candidate in &index.unresolved_references {
        let block = blocks
            .get(&candidate.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "reference.block"))?;
        let language_valid = candidate.language.as_ref().is_none_or(|language| {
            rift_protocol::read::Language::from_identity_segment(&language.identity_segment())
                .is_ok()
        });
        if !contains(&block.range, &candidate.range)
            || !valid_text(&candidate.authored)
            || !language_valid
        {
            return Err(refused(DocumentationViolation::Range, "reference.range"));
        }
    }
    Ok(reverse)
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationBlock, DocumentationBlockKind, DocumentationContentIdentity,
        DocumentationCoverage, DocumentationIndex, DocumentationLink, DocumentationLinkResolution,
        DocumentationReference, DocumentationReferenceCandidate, DocumentationReferenceEvidence,
        DocumentationSelectionReason, DocumentationSource, DocumentationSourceFormat,
        DocumentationSourceIdentity, DocumentationTarget, DocumentationUnresolvedReason,
    };
    use rift_protocol::read::{
        Digest, ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin, TextRange,
    };

    use crate::documentation::{content_digest, documentation_revision};

    use super::{DocumentationCollection, DocumentationViolation};

    fn source(path: &str, text: &str) -> DocumentationSource {
        DocumentationSource {
            identity: DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Project {
                    path: ProjectPath(path.to_owned()),
                },
                cell: None,
            },
            revision: content_digest(b"tree"),
            content_digest: content_digest(text.as_bytes()),
            origin: SymbolOrigin {
                location: Some(SourceLocationKind::Project),
                package: None,
                source_kind: SourceKind::Authored,
            },
            format: DocumentationSourceFormat::Markdown,
            media_type: "text/markdown".to_owned(),
            selection: DocumentationSelectionReason::Workspace,
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        }
    }

    fn block(source: &DocumentationSource) -> DocumentationBlock {
        DocumentationBlock {
            identity: content_digest(b"block"),
            source: source.identity.clone(),
            content_digest: source.content_digest.clone(),
            heading_path: Vec::new(),
            range: TextRange { start: 0, end: 11 },
            line: 1,
            kind: DocumentationBlockKind::Prose,
            language: None,
            chunks: Vec::new(),
            symbol: None,
        }
    }

    fn index(sources: Vec<DocumentationSource>) -> DocumentationIndex {
        let selected = u32::try_from(sources.len()).expect("fixture source count");
        let selection_digest =
            super::super::input::source_selection_digest(sources.iter()).expect("selection digest");
        let blocks = sources.iter().map(block).collect();
        DocumentationIndex {
            documentation_revision: documentation_revision(),
            selection_digest,
            sources,
            blocks,
            links: Vec::new(),
            references: Vec::new(),
            unresolved_references: Vec::new(),
            coverage: DocumentationCoverage {
                selected,
                parsed: selected,
                ..DocumentationCoverage::default()
            },
            warnings: Vec::new(),
        }
    }

    fn link(block: &DocumentationBlock, authored: &str) -> DocumentationLink {
        DocumentationLink {
            block: block.identity.clone(),
            authored: authored.to_owned(),
            range: TextRange { start: 1, end: 4 },
            resolution: DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            },
        }
    }

    fn refused_field(candidate: DocumentationIndex, expected: &str) {
        let error = DocumentationCollection::new(candidate).expect_err("invalid candidate");
        assert_eq!(error.fault().field(), expected);
    }

    #[test]
    fn candidate_validation_covers_sources_links_warnings_and_revisions() {
        let source = source("README.md", "hello world");
        let mut candidate = index(vec![source]);
        candidate.links.push(link(&candidate.blocks[0], "next.md"));
        let accepted = DocumentationCollection::new(candidate.clone()).expect("valid index");
        assert_eq!(accepted.index(), &candidate);

        let mut missing_link_block = candidate.clone();
        missing_link_block.links[0].block = content_digest(b"missing");
        assert_eq!(
            DocumentationCollection::new(missing_link_block)
                .expect_err("missing link block")
                .fault()
                .violation(),
            DocumentationViolation::MissingTarget
        );

        let mut invalid_source = candidate.clone();
        invalid_source.sources[0].origin.location = None;
        assert_eq!(
            DocumentationCollection::new(invalid_source)
                .expect_err("invalid source origin")
                .fault()
                .violation(),
            DocumentationViolation::Origin
        );

        let mut bad_selection = candidate.clone();
        bad_selection.selection_digest = content_digest(b"other selection");
        assert_eq!(
            DocumentationCollection::new(bad_selection)
                .expect_err("mismatched selection")
                .fault()
                .violation(),
            DocumentationViolation::Digest
        );

        let mut bad_revision = candidate.clone();
        bad_revision.documentation_revision = Digest("invalid".to_owned());
        assert_eq!(
            DocumentationCollection::new(bad_revision)
                .expect_err("stale revision")
                .fault()
                .violation(),
            DocumentationViolation::Revision
        );
    }

    #[test]
    fn invalid_candidate_does_not_replace_prior_complete_collection() {
        let source = source("README.md", "hello world");
        let mut active = DocumentationCollection::new(index(vec![source])).expect("prior");
        let prior = active.index().clone();
        let mut candidate = prior.clone();
        candidate
            .warnings
            .push(rift_protocol::documentation::DocumentationWarning {
                source: candidate.sources[0].identity.clone(),
                stage: rift_protocol::documentation::DocumentationStage::Extract,
                kind: rift_protocol::documentation::DocumentationWarningKind::OmittedRange,
                count: 0,
            });
        match DocumentationCollection::new(candidate) {
            Err(error) => assert_eq!(error.fault().violation(), DocumentationViolation::Range),
            Ok(replacement) => active = replacement,
        }
        assert_eq!(active.index(), &prior);
    }

    #[test]
    fn reverse_references_require_canonical_symbols_and_keep_exact_lookup() {
        let source = source("README.md", "hello world");
        let mut candidate = index(vec![source]);
        let target = SymbolId("rift://symbol/rust/crates/rift/src/lib.rs/Thing".to_owned());
        candidate.references.push(DocumentationReference {
            identity: content_digest(b"reference"),
            block: candidate.blocks[0].identity.clone(),
            target: target.clone(),
            range: TextRange { start: 6, end: 11 },
            authored: "Thing".to_owned(),
            evidence: DocumentationReferenceEvidence::UniqueName,
        });
        let accepted = DocumentationCollection::new(candidate.clone()).expect("valid reference");
        assert_eq!(accepted.references_to(&target).count(), 1);
        assert_eq!(
            accepted
                .references_to(&SymbolId("invalid".to_owned()))
                .count(),
            0
        );

        candidate.references[0].target = SymbolId("invalid".to_owned());
        assert_eq!(
            DocumentationCollection::new(candidate)
                .expect_err("invalid symbol")
                .fault()
                .violation(),
            DocumentationViolation::Identity
        );
    }

    #[test]
    fn resolved_link_targets_and_warning_sources_are_validated() {
        let source = source("README.md", "hello world");
        let mut candidate = index(vec![source]);
        candidate
            .links
            .push(link(&candidate.blocks[0], "README.md"));
        candidate.links[0].resolution = DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Source {
                source: candidate.sources[0].identity.clone(),
                range: TextRange { start: 0, end: 11 },
            },
        };
        candidate
            .warnings
            .push(rift_protocol::documentation::DocumentationWarning {
                source: DocumentationContentIdentity {
                    source: DocumentationSourceIdentity::Project {
                        path: ProjectPath("missing.md".to_owned()),
                    },
                    cell: None,
                },
                stage: rift_protocol::documentation::DocumentationStage::Source,
                kind: rift_protocol::documentation::DocumentationWarningKind::SourceUnavailable,
                count: 1,
            });
        assert!(DocumentationCollection::new(candidate.clone()).is_ok());

        let mut invalid_target = candidate.clone();
        invalid_target.links[0].resolution = DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Symbol {
                symbol: SymbolId("invalid".to_owned()),
            },
        };
        assert_eq!(
            DocumentationCollection::new(invalid_target)
                .expect_err("invalid symbol target")
                .fault()
                .violation(),
            DocumentationViolation::Identity
        );

        let mut duplicate_warning = candidate;
        let warning = duplicate_warning.warnings[0].clone();
        duplicate_warning.warnings.push(warning);
        assert_eq!(
            DocumentationCollection::new(duplicate_warning)
                .expect_err("duplicate warning")
                .fault()
                .violation(),
            DocumentationViolation::Identity
        );
    }

    #[test]
    fn changes_report_source_and_link_replacements() {
        let old_source = source("README.md", "hello world");
        let mut old = index(vec![old_source.clone()]);
        old.links.push(link(&old.blocks[0], "next.md"));
        let previous = DocumentationCollection::new(old).expect("previous");

        let new_source = source("README.md", "hello there");
        let mut current = index(vec![new_source]);
        current.links.push(link(&current.blocks[0], "other.md"));
        current.links[0].resolution = DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Source {
                source: current.sources[0].identity.clone(),
                range: TextRange { start: 0, end: 11 },
            },
        };
        let current = DocumentationCollection::new(current).expect("current");
        let changes = current.changes_from(Some(&previous));
        assert_eq!(changes.sources.replaced, vec![old_source.identity]);
        assert_eq!(changes.links.replaced.len(), 1);
        assert!(changes.links.added.is_empty());
        assert!(changes.links.removed.is_empty());
    }

    #[test]
    fn publication_total_source_bytes_are_bounded() {
        let source_bytes = u64::from(rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX);
        let source_count = usize::try_from(
            rift_protocol::documentation::DOCUMENTATION_TOTAL_BYTES_MAX / source_bytes,
        )
        .expect("fixture source count")
            + 1;
        let mut sources = Vec::with_capacity(source_count);
        for index in 0..source_count {
            let mut source = source(&format!("docs/{index:08}.md"), "hello world");
            source.byte_length = source_bytes;
            sources.push(source);
        }
        let candidate = index(sources);
        assert_eq!(
            DocumentationCollection::new(candidate)
                .expect_err("aggregate bytes over bound")
                .fault()
                .violation(),
            DocumentationViolation::LimitExceeded
        );
    }

    #[test]
    fn candidate_count_coverage_and_source_order_refusals_are_explicit() {
        let source_record = source("README.md", "hello world");
        let base = index(vec![source_record.clone()]);

        let mut over_count = base.clone();
        over_count.warnings = vec![
            rift_protocol::documentation::DocumentationWarning {
                source: source_record.identity.clone(),
                stage: rift_protocol::documentation::DocumentationStage::Extract,
                kind: rift_protocol::documentation::DocumentationWarningKind::OmittedRange,
                count: 1,
            };
            rift_protocol::documentation::DOCUMENTATION_WARNINGS_MAX as usize
                + 1
        ];
        refused_field(over_count, "warnings");

        let mut bad_coverage = base;
        bad_coverage.coverage.selected += 1;
        refused_field(bad_coverage, "coverage");

        let mut out_of_order = index(vec![
            source("a.md", "hello world"),
            source("b.md", "hello world"),
        ]);
        out_of_order.sources.reverse();
        refused_field(out_of_order, "sources");
    }

    #[test]
    fn block_link_and_reference_relationships_refuse_invalid_ranges() {
        let source_record = source("README.md", "hello world");
        let base = index(vec![source_record.clone()]);

        let mut bad_block = base.clone();
        bad_block.blocks[0].range.end = 0;
        refused_field(bad_block, "block.range");

        let mut bad_heading = base.clone();
        bad_heading.blocks[0].heading_path.push(
            rift_protocol::documentation::DocumentationHeading {
                level: 0,
                name: "Guide".to_owned(),
            },
        );
        refused_field(bad_heading, "heading_path");

        let mut bad_link = base.clone();
        bad_link.links.push(link(&bad_link.blocks[0], "next.md"));
        bad_link.links.push(bad_link.links[0].clone());
        refused_field(bad_link, "link");

        let mut bad_reference = base;
        bad_reference.references.push(DocumentationReference {
            identity: content_digest(b"reference"),
            block: bad_reference.blocks[0].identity.clone(),
            target: SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "Thing")),
            range: TextRange { start: 0, end: 12 },
            authored: "Thing".to_owned(),
            evidence: DocumentationReferenceEvidence::UniqueName,
        });
        refused_field(bad_reference, "reference.range");

        let mut bad_candidate = index(vec![source_record]);
        bad_candidate
            .unresolved_references
            .push(DocumentationReferenceCandidate {
                block: bad_candidate.blocks[0].identity.clone(),
                range: TextRange { start: 0, end: 5 },
                authored: "Thing".to_owned(),
                language: Some(rift_protocol::read::Language {
                    name: "invalid language".to_owned(),
                    dialect: None,
                }),
                reason: rift_protocol::documentation::DocumentationUnresolvedReason::Missing,
            });
        refused_field(bad_candidate, "reference.range");
    }
}
