//! Projects documentation metadata onto ranking identities.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::documentation::{
    DOCUMENTATION_BLOCKS_MAX, DocumentationBlock, DocumentationContentIdentity,
    DocumentationDigest, DocumentationSource, DocumentationSourceFormat,
};
use rift_protocol::read::TextRange;
use rift_ranking::{DocumentIdentity, FieldSet, RankedIdentity, RankingInput};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::publication::DocumentationCollection;

/// Which ranking documents receive documentation projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentationProjectionTarget {
    /// Keep only candidates mapped to documentation blocks.
    Documentation,
    /// Project documentation candidates and keep other ranking candidates.
    All,
}

/// Bounded mapping from existing ranking documents to validated documentation blocks.
#[derive(Clone, Debug, Default)]
pub struct DocumentationProjection<'collection> {
    mappings: BTreeMap<DocumentIdentity, Vec<Mapping>>,
    blocks: BTreeMap<
        DocumentationDigest,
        (
            &'collection DocumentationCollection,
            &'collection DocumentationBlock,
        ),
    >,
    source_records: BTreeMap<DocumentationContentIdentity, &'collection DocumentationSource>,
    sources: usize,
    block_count: usize,
    mapping_count: usize,
}

#[derive(Clone, Debug)]
struct Mapping {
    block: DocumentationDigest,
    source: DocumentationContentIdentity,
    range: TextRange,
    document_range: Option<TextRange>,
    owner: bool,
}

impl<'collection> DocumentationProjection<'collection> {
    /// Builds ranking mappings from one validated collection.
    ///
    /// # Errors
    ///
    /// Refuses mapping counts over the documentation block bound or identities over ranking's
    /// document identity bound.
    pub fn new(
        collection: &'collection DocumentationCollection,
    ) -> Result<Self, DocumentationError> {
        Self::from_collections(&[collection])
    }

    /// Builds one projection across project and package collections.
    ///
    /// # Errors
    ///
    /// Refuses aggregate source, block, or mapping counts over their bounds and duplicate block
    /// identities across collections.
    pub fn from_collections(
        collections: &[&'collection DocumentationCollection],
    ) -> Result<Self, DocumentationError> {
        let mut projection = Self::default();
        for collection in collections {
            projection.extend_collection(collection)?;
        }
        Ok(projection)
    }

    /// Adds one validated collection to this projection.
    ///
    /// # Errors
    ///
    /// Refuses aggregate bounds or duplicate block identities across collections.
    pub fn extend_collection(
        &mut self,
        collection: &'collection DocumentationCollection,
    ) -> Result<(), DocumentationError> {
        let next_sources = self.sources.checked_add(collection.index().sources.len());
        let next_blocks = self
            .block_count
            .checked_add(collection.index().blocks.len());
        if next_sources.is_none_or(|count| {
            count > rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX as usize
        }) || next_blocks.is_none_or(|count| count > DOCUMENTATION_BLOCKS_MAX as usize)
        {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.collections",
            ));
        }
        if collection
            .index()
            .blocks
            .iter()
            .any(|block| self.blocks.contains_key(&block.identity))
        {
            return Err(refused(
                DocumentationViolation::Identity,
                "projection.block",
            ));
        }
        if collection
            .index()
            .sources
            .iter()
            .any(|source| self.source_records.contains_key(&source.identity))
        {
            return Err(refused(
                DocumentationViolation::DuplicateSource,
                "projection.source",
            ));
        }
        let additions = collection_mappings(collection)?;
        let next_mappings = self.mapping_count.checked_add(additions.len());
        if next_mappings.is_none_or(|count| count > DOCUMENTATION_BLOCKS_MAX as usize) {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.mappings",
            ));
        }
        let (Some(next_sources), Some(next_blocks)) = (next_sources, next_blocks) else {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.collections",
            ));
        };
        self.sources = next_sources;
        self.block_count = next_blocks;
        for source in &collection.index().sources {
            self.source_records.insert(source.identity.clone(), source);
        }
        for block in &collection.index().blocks {
            self.blocks
                .insert(block.identity.clone(), (collection, block));
        }
        for (identity, mapping) in additions {
            let mappings = self.mappings.entry(identity).or_default();
            if !mappings
                .iter()
                .any(|existing| existing.block == mapping.block && existing.owner == mapping.owner)
            {
                mappings.push(mapping);
                self.mapping_count += 1;
            }
        }
        Ok(())
    }

    /// Associates one package declaration identity with an attached-comment block.
    ///
    /// # Errors
    ///
    /// Refuses unknown blocks, blocks without an owning symbol, or mappings over the bound.
    pub fn associate_document(
        &mut self,
        identity: DocumentIdentity,
        block_identity: &DocumentationDigest,
    ) -> Result<(), DocumentationError> {
        let (_, block) = self
            .blocks
            .get(block_identity)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.identity"))?;
        if block.symbol.is_none() {
            return Err(refused(DocumentationViolation::Format, "block.symbol"));
        }
        self.add(
            identity,
            Mapping {
                block: block.identity.clone(),
                source: block.source.clone(),
                range: block.range.clone(),
                document_range: None,
                owner: true,
            },
        )
    }

    /// Returns one projected block for its private ranking identity.
    #[must_use]
    pub fn block(&self, identity: &DocumentIdentity) -> Option<&DocumentationBlock> {
        let block_identity = identity
            .as_str()
            .strip_prefix("\u{1f}documentation-block/")?;
        let digest = DocumentationDigest(block_identity.to_owned());
        self.blocks.get(&digest).map(|(_, block)| *block)
    }

    /// Copies the validated metadata answering one projected ranking identity.
    #[must_use]
    pub fn hit(
        &self,
        identity: &DocumentIdentity,
    ) -> Option<rift_protocol::documentation::DocumentationHit> {
        let block = self.block(identity)?;
        let (collection, _) = self.blocks.get(&block.identity)?;
        Some(rift_protocol::documentation::DocumentationHit {
            block: block.clone(),
            source: self.source(&block.source)?.clone(),
            documentation_revision: collection.index().documentation_revision.clone(),
        })
    }

    /// Looks up source facts in any collection joined to this projection.
    #[must_use]
    pub fn source(&self, identity: &DocumentationContentIdentity) -> Option<&DocumentationSource> {
        self.source_records.get(identity).copied()
    }

    /// Returns source owner mapped from one existing ranking identity.
    #[must_use]
    pub fn document_source(
        &self,
        identity: &DocumentIdentity,
    ) -> Option<&DocumentationContentIdentity> {
        self.mappings
            .get(identity)?
            .first()
            .map(|mapping| &mapping.source)
    }

    /// Returns source range of one existing chunk identity, when identity names a chunk.
    #[must_use]
    pub fn document_range(&self, identity: &DocumentIdentity) -> Option<&TextRange> {
        self.mappings
            .get(identity)?
            .iter()
            .find_map(|mapping| mapping.document_range.as_ref())
    }

    /// Projects ranking inputs onto the blocks selected by their original match ranges.
    ///
    /// A locator result selects the narrowest containing block. When no source range is
    /// available, the first mapped block wins. Duplicate identities keep their best position
    /// and union their matched fields before projection.
    ///
    /// # Errors
    ///
    /// Refuses input candidate totals over the documentation block bound or an invalid
    /// projected ranking identity.
    pub fn project<F>(
        &self,
        inputs: &[RankingInput],
        target: DocumentationProjectionTarget,
        mut locate: F,
    ) -> Result<Vec<RankingInput>, DocumentationError>
    where
        F: FnMut(&DocumentIdentity, &DocumentationContentIdentity) -> Option<TextRange>,
    {
        let candidate_count = inputs.iter().try_fold(0_usize, |count, input| {
            count.checked_add(input.order().len())
        });
        if candidate_count.is_none_or(|count| count > DOCUMENTATION_BLOCKS_MAX as usize) {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.candidates",
            ));
        }

        let owner_answers = inputs
            .iter()
            .flat_map(RankingInput::order)
            .filter_map(|ranked| self.mappings.get(ranked.identity()))
            .flat_map(|mappings| mappings.iter())
            .filter(|mapping| mapping.owner)
            .map(|mapping| mapping.block.clone())
            .collect::<BTreeSet<_>>();

        let mut projected = Vec::with_capacity(inputs.len());
        let mut examined_mappings = 0_usize;
        for input in inputs {
            let collapsed = collapse_input(input);
            let mut order = Vec::<RankedIdentity>::new();
            let mut positions = BTreeMap::<DocumentIdentity, usize>::new();
            for ranked in collapsed {
                let Some(mappings) = self.mappings.get(ranked.identity()) else {
                    if target == DocumentationProjectionTarget::All {
                        push_or_union(
                            &mut order,
                            &mut positions,
                            ranked.identity().clone(),
                            ranked.fields(),
                        );
                    }
                    continue;
                };
                examined_mappings = examined_mappings
                    .checked_add(mappings.len())
                    .filter(|count| *count <= DOCUMENTATION_BLOCKS_MAX as usize)
                    .ok_or_else(|| {
                        refused(DocumentationViolation::LimitExceeded, "projection.mappings")
                    })?;
                let Some(mapping) = choose_mapping(mappings, ranked.identity(), &mut locate) else {
                    if target == DocumentationProjectionTarget::All {
                        push_or_union(
                            &mut order,
                            &mut positions,
                            ranked.identity().clone(),
                            ranked.fields(),
                        );
                    }
                    continue;
                };
                let (_, block) = self.blocks.get(&mapping.block).ok_or_else(|| {
                    refused(DocumentationViolation::MissingTarget, "projection.block")
                })?;
                if target == DocumentationProjectionTarget::All
                    && mapping.owner
                    && block.symbol.is_some()
                {
                    push_or_union(
                        &mut order,
                        &mut positions,
                        ranked.identity().clone(),
                        ranked.fields(),
                    );
                    continue;
                }
                if target == DocumentationProjectionTarget::All
                    && block.symbol.is_some()
                    && owner_answers.contains(&mapping.block)
                {
                    continue;
                }
                let identity = DocumentIdentity::for_documentation_block(&block.identity.0)
                    .map_err(|error| {
                        caused_by(
                            DocumentationViolation::LimitExceeded,
                            "projection.identity",
                            error,
                        )
                    })?;
                push_or_union(&mut order, &mut positions, identity, ranked.fields());
            }
            projected.push(RankingInput::new(input.kind(), order));
        }
        Ok(projected)
    }

    fn add(
        &mut self,
        identity: DocumentIdentity,
        mapping: Mapping,
    ) -> Result<(), DocumentationError> {
        if self.mapping_count >= DOCUMENTATION_BLOCKS_MAX as usize {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.mappings",
            ));
        }
        let mappings = self.mappings.entry(identity).or_default();
        if !mappings
            .iter()
            .any(|existing| existing.block == mapping.block && existing.owner == mapping.owner)
        {
            mappings.push(mapping);
            self.mapping_count += 1;
        }
        Ok(())
    }
}

fn collection_mappings(
    collection: &DocumentationCollection,
) -> Result<Vec<(DocumentIdentity, Mapping)>, DocumentationError> {
    let mut additions = Vec::new();
    for block in &collection.index().blocks {
        for chunk in &block.chunks {
            let identity = DocumentIdentity::new(chunk.identity.as_str()).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "chunk.identity", error)
            })?;
            additions.push((
                identity,
                Mapping {
                    block: block.identity.clone(),
                    source: block.source.clone(),
                    range: block.range.clone(),
                    document_range: Some(chunk.range.clone()),
                    owner: false,
                },
            ));
        }
        if let Some(source) = collection.source(&block.source)
            && matches!(
                source.format,
                DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
            )
        {
            for heading in &block.heading_path {
                let identity = heading_identity(&block.source, &heading.name)?;
                additions.push((
                    identity,
                    Mapping {
                        block: block.identity.clone(),
                        source: block.source.clone(),
                        range: block.range.clone(),
                        document_range: None,
                        owner: false,
                    },
                ));
            }
        }
        if let Some(symbol) = &block.symbol {
            let identity = DocumentIdentity::new(symbol.0.clone()).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "block.symbol", error)
            })?;
            additions.push((
                identity,
                Mapping {
                    block: block.identity.clone(),
                    source: block.source.clone(),
                    range: block.range.clone(),
                    document_range: None,
                    owner: true,
                },
            ));
            if let rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } =
                &block.source.source
            {
                let unit = rift_core::SourceUnitId::parse(&unit.0).map_err(|error| {
                    caused_by(DocumentationViolation::Identity, "block.source", error)
                })?;
                let symbol = rift_core::parse_symbol_identity(&symbol.0).map_err(|error| {
                    caused_by(DocumentationViolation::Identity, "block.symbol", error)
                })?;
                let identity = DocumentIdentity::for_unit(&unit, symbol.qualified_name()).map_err(
                    |error| caused_by(DocumentationViolation::Identity, "block.symbol", error),
                )?;
                additions.push((
                    identity,
                    Mapping {
                        block: block.identity.clone(),
                        source: block.source.clone(),
                        range: block.range.clone(),
                        document_range: None,
                        owner: true,
                    },
                ));
            }
        }
    }
    Ok(additions)
}

fn heading_identity(
    source: &DocumentationContentIdentity,
    name: &str,
) -> Result<DocumentIdentity, DocumentationError> {
    match &source.source {
        rift_protocol::documentation::DocumentationSourceIdentity::Project { path } => {
            DocumentIdentity::new(rift_core::symbol_identity("markdown", &path.0, name)).map_err(
                |error| caused_by(DocumentationViolation::Identity, "heading.identity", error),
            )
        }
        rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } => {
            let unit = rift_core::SourceUnitId::parse(&unit.0).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "heading.identity", error)
            })?;
            DocumentIdentity::for_unit(&unit, name).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "heading.identity", error)
            })
        }
    }
}

fn caused_by(
    violation: DocumentationViolation,
    field: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> DocumentationError {
    super::failure::DocumentationFault::new(violation, field)
        .caused_by(source)
        .into()
}

fn choose_mapping<'a, F>(
    mappings: &'a [Mapping],
    identity: &DocumentIdentity,
    locate: &mut F,
) -> Option<&'a Mapping>
where
    F: FnMut(&DocumentIdentity, &DocumentationContentIdentity) -> Option<TextRange>,
{
    let mut located = false;
    let mut best = None;
    for mapping in mappings {
        let Some(range) = locate(identity, &mapping.source) else {
            continue;
        };
        located = true;
        if !contains(&mapping.range, &range) {
            continue;
        }
        if best.is_none_or(|current: &Mapping| {
            range_len(&mapping.range) < range_len(&current.range)
                || (range_len(&mapping.range) == range_len(&current.range)
                    && (&mapping.source, &mapping.range.start, &mapping.block)
                        < (&current.source, &current.range.start, &current.block))
        }) {
            best = Some(mapping);
        }
    }
    if located { best } else { mappings.first() }
}

fn contains(outer: &TextRange, inner: &TextRange) -> bool {
    outer.start <= inner.start && inner.start < inner.end && inner.end <= outer.end
}

fn range_len(range: &TextRange) -> u64 {
    range.end - range.start
}

fn collapse_input(input: &RankingInput) -> Vec<RankedIdentity> {
    let mut order = Vec::<RankedIdentity>::new();
    let mut positions = BTreeMap::<DocumentIdentity, usize>::new();
    for ranked in input.order() {
        push_or_union(
            &mut order,
            &mut positions,
            ranked.identity().clone(),
            ranked.fields(),
        );
    }
    order
}

fn push_or_union(
    order: &mut Vec<RankedIdentity>,
    positions: &mut BTreeMap<DocumentIdentity, usize>,
    identity: DocumentIdentity,
    fields: FieldSet,
) {
    if let Some(position) = positions.get(&identity).copied() {
        let prior = &order[position];
        order[position] =
            RankedIdentity::new(prior.identity().clone(), prior.fields().union(fields));
    } else {
        positions.insert(identity.clone(), order.len());
        order.push(RankedIdentity::new(identity, fields));
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationBlockKind, DocumentationContentIdentity, DocumentationCoverage,
        DocumentationIndex, DocumentationSelectionReason, DocumentationSource,
        DocumentationSourceFormat, DocumentationSourceIdentity,
    };
    use rift_protocol::read::{
        ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin, TextRange,
    };
    use rift_ranking::{
        DocumentIdentity, FieldSet, RankedIdentity, RankingInput, RankingInputKind,
    };

    use crate::documentation::{DocumentationCollection, content_digest, documentation_revision};

    use super::{DocumentationProjection, DocumentationProjectionTarget};

    fn source(path: &str, text: &str, format: DocumentationSourceFormat) -> DocumentationSource {
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath(path.to_owned()),
            },
            cell: None,
        };
        DocumentationSource {
            identity,
            revision: content_digest(b"tree"),
            content_digest: content_digest(text.as_bytes()),
            origin: SymbolOrigin {
                location: Some(SourceLocationKind::Project),
                package: None,
                source_kind: SourceKind::Authored,
            },
            format,
            media_type: "text/markdown".to_owned(),
            selection: if format == DocumentationSourceFormat::AttachedComment {
                DocumentationSelectionReason::AttachedComment
            } else {
                DocumentationSelectionReason::Workspace
            },
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        }
    }

    fn block(
        source: &DocumentationSource,
        identity: &str,
        range: TextRange,
        chunks: Vec<(&str, TextRange)>,
        headings: &[&str],
        symbol: Option<SymbolId>,
    ) -> rift_protocol::documentation::DocumentationBlock {
        rift_protocol::documentation::DocumentationBlock {
            identity: content_digest(identity.as_bytes()),
            source: source.identity.clone(),
            content_digest: content_digest(identity.as_bytes()),
            heading_path: headings
                .iter()
                .enumerate()
                .map(
                    |(position, name)| rift_protocol::documentation::DocumentationHeading {
                        level: u32::try_from(position + 1).expect("fixture heading depth"),
                        name: (*name).to_owned(),
                    },
                )
                .collect(),
            range,
            line: 1,
            kind: DocumentationBlockKind::Prose,
            language: None,
            chunks: chunks
                .into_iter()
                .map(
                    |(identity, range)| rift_protocol::documentation::DocumentationChunk {
                        identity: identity.to_owned(),
                        range,
                    },
                )
                .collect(),
            symbol,
        }
    }

    fn collection(
        mut sources: Vec<DocumentationSource>,
        mut blocks: Vec<rift_protocol::documentation::DocumentationBlock>,
    ) -> DocumentationCollection {
        sources.sort_by(|left, right| left.identity.cmp(&right.identity));
        blocks.sort_by(|left, right| {
            (&left.source, left.range.start, left.kind, &left.identity).cmp(&(
                &right.source,
                right.range.start,
                right.kind,
                &right.identity,
            ))
        });
        let selection_digest =
            super::super::input::source_selection_digest(sources.iter()).expect("selection digest");
        DocumentationCollection::new(DocumentationIndex {
            documentation_revision: documentation_revision(),
            selection_digest,
            coverage: DocumentationCoverage {
                selected: u32::try_from(sources.len()).expect("fixture source count"),
                parsed: u32::try_from(sources.len()).expect("fixture source count"),
                ..DocumentationCoverage::default()
            },
            sources,
            blocks,
            links: Vec::new(),
            references: Vec::new(),
            unresolved_references: Vec::new(),
            warnings: Vec::new(),
        })
        .expect("valid collection")
    }

    fn input(identity: &str, fields: FieldSet) -> RankingInput {
        RankingInput::new(
            RankingInputKind::Lexical,
            vec![RankedIdentity::new(
                DocumentIdentity::new(identity).expect("document identity"),
                fields,
            )],
        )
    }

    #[test]
    fn projection_uses_most_specific_block_and_unions_duplicate_fields() {
        let text = "first block second block";
        let source = source("docs/guide.md", text, DocumentationSourceFormat::Markdown);
        let first = block(
            &source,
            "first",
            TextRange { start: 0, end: 11 },
            vec![("docs/chunks", TextRange { start: 0, end: 11 })],
            &[],
            None,
        );
        let second = block(
            &source,
            "second",
            TextRange { start: 12, end: 24 },
            vec![("docs/chunks", TextRange { start: 12, end: 24 })],
            &[],
            None,
        );
        let collection = collection(vec![source], vec![first, second]);
        let projection = DocumentationProjection::new(&collection).expect("projection");
        let fields = FieldSet::of(rift_ranking::SearchableField::Name);
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            vec![
                RankedIdentity::new(
                    DocumentIdentity::new("docs/chunks").expect("identity"),
                    fields,
                ),
                RankedIdentity::new(
                    DocumentIdentity::new("docs/chunks").expect("identity"),
                    FieldSet::of(rift_ranking::SearchableField::FileContent),
                ),
            ],
        )];
        let result = projection
            .project(
                &inputs,
                DocumentationProjectionTarget::Documentation,
                |_, _| Some(TextRange { start: 15, end: 18 }),
            )
            .expect("projected input");
        assert_eq!(result[0].order().len(), 1);
        assert!(
            result[0].order()[0]
                .fields()
                .holds(rift_ranking::SearchableField::Name)
        );
        assert!(
            result[0].order()[0]
                .fields()
                .holds(rift_ranking::SearchableField::FileContent)
        );
        let projected_block = projection
            .block(result[0].order()[0].identity())
            .expect("mapped block");
        assert_eq!(projected_block.range.start, 12);
    }

    #[test]
    fn identical_heading_text_in_distinct_collections_keeps_distinct_blocks() {
        let first_source = source(
            "docs/one.md",
            "# Guide",
            DocumentationSourceFormat::Markdown,
        );
        let second_source = source(
            "docs/two.md",
            "# Guide",
            DocumentationSourceFormat::Markdown,
        );
        let first = block(
            &first_source,
            "one",
            TextRange { start: 0, end: 7 },
            Vec::new(),
            &["Guide"],
            None,
        );
        let second = block(
            &second_source,
            "two",
            TextRange { start: 0, end: 7 },
            Vec::new(),
            &["Guide"],
            None,
        );
        let first_collection = collection(vec![first_source.clone()], vec![first]);
        let second_collection = collection(vec![second_source.clone()], vec![second]);
        let projection =
            DocumentationProjection::from_collections(&[&first_collection, &second_collection])
                .expect("combined projection");
        let inputs = [RankingInput::new(
            RankingInputKind::Identifier,
            vec![
                RankedIdentity::new(
                    DocumentIdentity::new(rift_core::symbol_identity(
                        "markdown",
                        "docs/one.md",
                        "Guide",
                    ))
                    .expect("heading identity"),
                    FieldSet::EMPTY,
                ),
                RankedIdentity::new(
                    DocumentIdentity::new(rift_core::symbol_identity(
                        "markdown",
                        "docs/two.md",
                        "Guide",
                    ))
                    .expect("heading identity"),
                    FieldSet::EMPTY,
                ),
            ],
        )];
        let result = projection
            .project(
                &inputs,
                DocumentationProjectionTarget::Documentation,
                |_, _| None,
            )
            .expect("projected inputs");
        assert_eq!(result[0].order().len(), 2);
        assert_ne!(
            result[0].order()[0].identity(),
            result[0].order()[1].identity()
        );
        assert!(projection.source(&first_source.identity).is_some());
        assert!(projection.source(&second_source.identity).is_some());
        assert!(
            projection
                .document_source(
                    &DocumentIdentity::new(rift_core::symbol_identity(
                        "markdown",
                        "docs/one.md",
                        "Guide",
                    ))
                    .expect("heading")
                )
                .is_some()
        );
    }

    #[test]
    fn outside_match_drops_for_documentation_and_keeps_original_for_all() {
        let source = source(
            "docs/guide.md",
            "0123456789abcdefghij",
            DocumentationSourceFormat::Markdown,
        );
        let block = block(
            &source,
            "block",
            TextRange { start: 0, end: 10 },
            vec![("docs/chunk", TextRange { start: 0, end: 10 })],
            &[],
            None,
        );
        let collection = collection(vec![source], vec![block]);
        let projection = DocumentationProjection::new(&collection).expect("projection");
        let inputs = [input("docs/chunk", FieldSet::EMPTY)];
        let locate = |_: &DocumentIdentity, _: &DocumentationContentIdentity| {
            Some(TextRange { start: 15, end: 19 })
        };
        let docs = projection
            .project(
                &inputs,
                DocumentationProjectionTarget::Documentation,
                locate,
            )
            .expect("documentation projection");
        assert!(docs[0].order().is_empty());
        let all = projection
            .project(&inputs, DocumentationProjectionTarget::All, locate)
            .expect("all projection");
        assert_eq!(all[0].order()[0].identity().as_str(), "docs/chunk");
    }

    #[test]
    fn all_keeps_symbol_owner_and_suppresses_answered_attached_comment() {
        let symbol = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "Beacon"));
        let source = source(
            "src/lib.rs",
            "attached docs",
            DocumentationSourceFormat::AttachedComment,
        );
        let block = block(
            &source,
            "attached",
            TextRange { start: 0, end: 13 },
            vec![("src/lib.rs#chunk/0", TextRange { start: 0, end: 13 })],
            &[],
            Some(symbol.clone()),
        );
        let collection = collection(vec![source], vec![block]);
        let projection = DocumentationProjection::new(&collection).expect("projection");
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            vec![
                RankedIdentity::new(
                    DocumentIdentity::new("src/lib.rs#chunk/0").expect("chunk identity"),
                    FieldSet::EMPTY,
                ),
                RankedIdentity::new(
                    DocumentIdentity::new(symbol.0.clone()).expect("symbol identity"),
                    FieldSet::EMPTY,
                ),
            ],
        )];
        let result = projection
            .project(&inputs, DocumentationProjectionTarget::All, |_, _| None)
            .expect("all projection");
        assert_eq!(result[0].order().len(), 1);
        assert_eq!(result[0].order()[0].identity().as_str(), symbol.0);
    }

    #[test]
    fn package_owner_association_targets_attached_comment_block() {
        let symbol = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "Beacon"));
        let source = source(
            "src/lib.rs",
            "attached docs",
            DocumentationSourceFormat::AttachedComment,
        );
        let block = block(
            &source,
            "attached-package",
            TextRange { start: 0, end: 13 },
            vec![("src/lib.rs#chunk/0", TextRange { start: 0, end: 13 })],
            &[],
            Some(symbol),
        );
        let block_identity = block.identity.clone();
        let collection = collection(vec![source], vec![block]);
        let mut projection = DocumentationProjection::new(&collection).expect("projection");
        let package_owner = DocumentIdentity::new("package-owner").expect("ranking identity");
        projection
            .associate_document(package_owner.clone(), &block_identity)
            .expect("owner association");
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            vec![RankedIdentity::new(package_owner.clone(), FieldSet::EMPTY)],
        )];
        let result = projection
            .project(&inputs, DocumentationProjectionTarget::All, |_, _| None)
            .expect("all projection");
        assert_eq!(result[0].order()[0].identity(), &package_owner);
    }
}
