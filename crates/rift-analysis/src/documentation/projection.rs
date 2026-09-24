//! Projects documentation metadata onto ranking identities.
//!
//! A [`DocumentationLayer`] maps every ranking identity of a fixed set of collections onto
//! the blocks it reaches, once. A [`DocumentationProjection`] joins the layers one read
//! answers from and looks each ranked candidate up in them, so a read never rebuilds the
//! mappings of collections that did not change.

use std::collections::BTreeSet;
use std::sync::Arc;

use rift_protocol::documentation::{
    DOCUMENTATION_SOURCES_MAX, DocumentationBlock, DocumentationContentIdentity,
    DocumentationDigest, DocumentationSource, DocumentationSourceFormat,
    DocumentationSourceIdentity,
};
use rift_protocol::read::TextRange;
use rift_ranking::{DocumentIdentity, FieldSet, RankedIdentity, RankingInput};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::publication::DocumentationCollection;

/// Blocks one layer holds across its collections.
///
/// A layer joins every collection it is built over, and one dependency package's
/// collection holds up to `DOCUMENTATION_BLOCKS_MAX` blocks by itself. The dependency
/// packages of the Next.js corpus tree hold 722,000 blocks together.
pub const LAYER_BLOCKS_MAX: usize = 2_000_000;

/// Ranking identities one layer maps onto blocks, counted before duplicates collapse.
///
/// A block takes one mapping per chunk it overlaps, one per Markdown heading on its path,
/// and one or two for its owning declaration, so mappings outnumber blocks about three to
/// one: the dependency packages of the Next.js corpus tree hold 2.15 million.
pub const LAYER_MAPPINGS_MAX: usize = 6_000_000;

/// The bounds one layer is built under.
#[derive(Clone, Copy, Debug)]
struct LayerBounds {
    sources: usize,
    blocks: usize,
    mappings: usize,
}

impl LayerBounds {
    const DEFAULT: Self = Self {
        sources: DOCUMENTATION_SOURCES_MAX as usize,
        blocks: LAYER_BLOCKS_MAX,
        mappings: LAYER_MAPPINGS_MAX,
    };
}

/// Which ranking documents receive documentation projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentationProjectionTarget {
    /// Keep only candidates mapped to documentation blocks.
    Documentation,
    /// Project documentation candidates and keep other ranking candidates.
    All,
}

/// One collection a layer reads: shared with its owner, or borrowed for one call.
#[derive(Debug)]
enum HeldCollection<'collection> {
    Shared(Arc<DocumentationCollection>),
    Borrowed(&'collection DocumentationCollection),
}

impl HeldCollection<'_> {
    fn collection(&self) -> &DocumentationCollection {
        match self {
            Self::Shared(collection) => collection,
            Self::Borrowed(collection) => collection,
        }
    }
}

/// Position of one block: its collection in the layer, then its place in that collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BlockAddress {
    collection: u32,
    block: u32,
}

#[derive(Clone, Debug)]
struct Mapping {
    block: BlockAddress,
    document_range: Option<TextRange>,
    owner: bool,
}

/// One mapping resolved against the collection that holds its block.
#[derive(Clone, Copy, Debug)]
struct ResolvedMapping<'layer> {
    block: &'layer DocumentationBlock,
    document_range: Option<&'layer TextRange>,
    owner: bool,
}

/// Ranking-identity mappings over a fixed set of validated collections.
///
/// Every table is sorted once when the layer is built, and a lookup is a binary search,
/// so the layer is built when its collections change and read by every projection until
/// then. Collections are joined in the order given; a lookup answers mappings in that
/// order.
#[derive(Debug)]
pub struct DocumentationLayer<'collection> {
    collections: Vec<HeldCollection<'collection>>,
    mappings: Vec<(DocumentIdentity, Mapping)>,
    blocks: Vec<(DocumentationDigest, BlockAddress)>,
    sources: Vec<(DocumentationContentIdentity, u32)>,
    mapping_bound: usize,
}

impl DocumentationLayer<'static> {
    /// Builds one layer over collections shared with their owners.
    ///
    /// # Errors
    ///
    /// Refuses a source identity or block identity held by two collections, and source,
    /// block, or mapping counts over their layer bounds.
    pub fn shared(
        collections: impl IntoIterator<Item = Arc<DocumentationCollection>>,
    ) -> Result<Self, DocumentationError> {
        Self::build(
            collections
                .into_iter()
                .map(HeldCollection::Shared)
                .collect(),
            LayerBounds::DEFAULT,
        )
    }
}

impl<'collection> DocumentationLayer<'collection> {
    /// Builds one layer over borrowed collections.
    ///
    /// # Errors
    ///
    /// Refuses the same conditions as [`DocumentationLayer::shared`].
    pub fn borrowed(
        collections: &[&'collection DocumentationCollection],
    ) -> Result<Self, DocumentationError> {
        Self::build(
            collections
                .iter()
                .copied()
                .map(HeldCollection::Borrowed)
                .collect(),
            LayerBounds::DEFAULT,
        )
    }

    fn build(
        collections: Vec<HeldCollection<'collection>>,
        bounds: LayerBounds,
    ) -> Result<Self, DocumentationError> {
        if u32::try_from(collections.len()).is_err() {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.collections",
            ));
        }
        let sources = layer_sources(&collections, bounds.sources)?;
        let blocks = layer_blocks(&collections, bounds.blocks)?;
        let mappings = layer_mappings(&collections, bounds.mappings)?;
        let mapping_bound = bounds.mappings;
        Ok(Self {
            collections,
            mappings,
            blocks,
            sources,
            mapping_bound,
        })
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
        let address = self
            .block_address(block_identity)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.identity"))?;
        if self.block_at(address).symbol.is_none() {
            return Err(refused(DocumentationViolation::Format, "block.symbol"));
        }
        let start = self
            .mappings
            .partition_point(|(existing, _)| existing < &identity);
        let end = self
            .mappings
            .partition_point(|(existing, _)| existing <= &identity);
        if self.mappings[start..end]
            .iter()
            .any(|(_, mapping)| mapping.block == address && mapping.owner)
        {
            return Ok(());
        }
        if self.mappings.len() >= self.mapping_bound {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "projection.mappings",
            ));
        }
        self.mappings.insert(
            end,
            (
                identity,
                Mapping {
                    block: address,
                    document_range: None,
                    owner: true,
                },
            ),
        );
        Ok(())
    }

    /// Blocks this layer holds across its collections.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Ranking identities this layer maps, one per identity and block.
    #[must_use]
    pub fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    fn mappings(&self, identity: &DocumentIdentity) -> impl Iterator<Item = ResolvedMapping<'_>> {
        let start = self
            .mappings
            .partition_point(|(existing, _)| existing < identity);
        self.mappings[start..]
            .iter()
            .take_while(move |(existing, _)| existing == identity)
            .map(|(_, mapping)| ResolvedMapping {
                block: self.block_at(mapping.block),
                document_range: mapping.document_range.as_ref(),
                owner: mapping.owner,
            })
    }

    fn block_address(&self, identity: &DocumentationDigest) -> Option<BlockAddress> {
        let position = self
            .blocks
            .binary_search_by(|(existing, _)| existing.cmp(identity))
            .ok()?;
        Some(self.blocks[position].1)
    }

    fn located_block(
        &self,
        identity: &DocumentationDigest,
    ) -> Option<(&DocumentationCollection, &DocumentationBlock)> {
        let address = self.block_address(identity)?;
        Some((self.collection_at(address), self.block_at(address)))
    }

    fn source(&self, identity: &DocumentationContentIdentity) -> Option<&DocumentationSource> {
        let position = self
            .sources
            .binary_search_by(|(existing, _)| existing.cmp(identity))
            .ok()?;
        let collection = self.sources[position].1;
        self.collections[collection as usize]
            .collection()
            .source(identity)
    }

    fn collection_at(&self, address: BlockAddress) -> &DocumentationCollection {
        self.collections[address.collection as usize].collection()
    }

    fn block_at(&self, address: BlockAddress) -> &DocumentationBlock {
        &self.collection_at(address).index().blocks[address.block as usize]
    }
}

/// One layer a projection reads, held by its owner or by the projection itself.
#[derive(Debug)]
enum ProjectionLayer<'layer> {
    Shared(&'layer DocumentationLayer<'layer>),
    Owned(Box<DocumentationLayer<'layer>>),
}

impl<'layer> ProjectionLayer<'layer> {
    fn layer(&self) -> &DocumentationLayer<'layer> {
        match self {
            Self::Shared(layer) => layer,
            Self::Owned(layer) => layer,
        }
    }
}

/// The layers one read projects its ranked candidates onto, in lookup order.
///
/// Joining a layer costs nothing beyond the reference: the mappings were built with the
/// layer. Two layers never hold the same block, because a block identity digests its
/// source identity and a project path and a package unit never name the same source.
#[derive(Debug, Default)]
pub struct DocumentationProjection<'layer> {
    layers: Vec<ProjectionLayer<'layer>>,
}

impl<'layer> DocumentationProjection<'layer> {
    /// Builds a projection over one validated collection.
    ///
    /// # Errors
    ///
    /// Refuses the conditions [`DocumentationLayer::borrowed`] refuses.
    pub fn new(collection: &'layer DocumentationCollection) -> Result<Self, DocumentationError> {
        Self::from_collections(&[collection])
    }

    /// Builds a projection over one layer joining the given collections.
    ///
    /// # Errors
    ///
    /// Refuses the conditions [`DocumentationLayer::borrowed`] refuses.
    pub fn from_collections(
        collections: &[&'layer DocumentationCollection],
    ) -> Result<Self, DocumentationError> {
        Ok(Self::default().with_owned_layer(DocumentationLayer::borrowed(collections)?))
    }

    /// Joins a layer its owner keeps, after the layers already joined.
    #[must_use]
    pub fn with_layer(mut self, layer: &'layer DocumentationLayer<'layer>) -> Self {
        self.layers.push(ProjectionLayer::Shared(layer));
        self
    }

    /// Joins a layer this projection keeps, after the layers already joined.
    #[must_use]
    pub fn with_owned_layer(mut self, layer: DocumentationLayer<'layer>) -> Self {
        self.layers.push(ProjectionLayer::Owned(Box::new(layer)));
        self
    }

    /// Returns one projected block for its private ranking identity.
    #[must_use]
    pub fn block(&self, identity: &DocumentIdentity) -> Option<&DocumentationBlock> {
        self.located_block(identity).map(|(_, block)| block)
    }

    /// Copies the validated metadata answering one projected ranking identity.
    #[must_use]
    pub fn hit(
        &self,
        identity: &DocumentIdentity,
    ) -> Option<rift_protocol::documentation::DocumentationHit> {
        let (collection, block) = self.located_block(identity)?;
        Some(rift_protocol::documentation::DocumentationHit {
            block: block.clone(),
            source: collection.source(&block.source)?.clone(),
            documentation_revision: collection.index().documentation_revision.clone(),
        })
    }

    /// Looks up source facts in any layer joined to this projection.
    #[must_use]
    pub fn source(&self, identity: &DocumentationContentIdentity) -> Option<&DocumentationSource> {
        self.layers().find_map(|layer| layer.source(identity))
    }

    /// Returns source owner mapped from one existing ranking identity.
    #[must_use]
    pub fn document_source(
        &self,
        identity: &DocumentIdentity,
    ) -> Option<&DocumentationContentIdentity> {
        self.mappings(identity)
            .next()
            .map(|mapping| &mapping.block.source)
    }

    /// Returns source range of one existing chunk identity, when identity names a chunk.
    #[must_use]
    pub fn document_range(&self, identity: &DocumentIdentity) -> Option<&TextRange> {
        self.mappings(identity)
            .find_map(|mapping| mapping.document_range)
    }

    /// Projects ranking inputs onto the blocks selected by their original match ranges.
    ///
    /// A locator result selects the narrowest containing block. When no source range is
    /// available, the first mapped block wins. Duplicate identities keep their best position
    /// and union their matched fields before projection.
    ///
    /// Each input is read once and each of its identities is looked up once per joined
    /// layer, so the work is bounded by the inputs, which their producers already bound,
    /// and by the mappings one identity holds, which [`LAYER_MAPPINGS_MAX`] bounds.
    ///
    /// # Errors
    ///
    /// Refuses a projected ranking identity past ranking's identity bound, or a mapped
    /// block no joined layer holds.
    pub fn project<F>(
        &self,
        inputs: &[RankingInput],
        target: DocumentationProjectionTarget,
        mut locate: F,
    ) -> Result<Vec<RankingInput>, DocumentationError>
    where
        F: FnMut(&DocumentIdentity, &DocumentationContentIdentity) -> Option<TextRange>,
    {
        let owner_answers = inputs
            .iter()
            .flat_map(RankingInput::order)
            .flat_map(|ranked| self.mappings(ranked.identity()))
            .filter(|mapping| mapping.owner)
            .map(|mapping| &mapping.block.identity)
            .collect::<BTreeSet<_>>();

        let mut projected = Vec::with_capacity(inputs.len());
        for input in inputs {
            let mut order = ProjectedOrder::default();
            for ranked in collapse_input(input) {
                let mappings = self.mappings(ranked.identity()).collect::<Vec<_>>();
                let chosen = choose_mapping(&mappings, ranked.identity(), &mut locate);
                match Projected::for_mapping(chosen, target, &owner_answers)? {
                    Projected::Original if target == DocumentationProjectionTarget::All => {
                        order.push(ranked.identity().clone(), ranked.fields());
                    }
                    Projected::Original | Projected::Suppressed => {}
                    Projected::Block(identity) => order.push(identity, ranked.fields()),
                }
            }
            projected.push(RankingInput::new(input.kind(), order.into_order()));
        }
        Ok(projected)
    }

    fn layers(&self) -> impl Iterator<Item = &DocumentationLayer<'layer>> {
        self.layers.iter().map(ProjectionLayer::layer)
    }

    fn mappings(&self, identity: &DocumentIdentity) -> impl Iterator<Item = ResolvedMapping<'_>> {
        self.layers()
            .flat_map(move |layer| layer.mappings(identity))
    }

    fn located_block(
        &self,
        identity: &DocumentIdentity,
    ) -> Option<(&DocumentationCollection, &DocumentationBlock)> {
        let digest = DocumentationDigest(
            identity
                .as_str()
                .strip_prefix("\u{1f}documentation-block/")?
                .to_owned(),
        );
        self.layers().find_map(|layer| layer.located_block(&digest))
    }
}

/// What one ranked identity becomes in a projected input.
enum Projected {
    /// No block answers it: kept as ranked when the target keeps other candidates.
    Original,
    /// Its attached comment is answered by the declaration that owns it.
    Suppressed,
    /// The block that answers it, by the block's private ranking identity.
    Block(DocumentIdentity),
}

impl Projected {
    /// Decides what one ranked identity becomes, given the mapping chosen for it.
    fn for_mapping(
        chosen: Option<ResolvedMapping<'_>>,
        target: DocumentationProjectionTarget,
        owner_answers: &BTreeSet<&DocumentationDigest>,
    ) -> Result<Self, DocumentationError> {
        let Some(mapping) = chosen else {
            return Ok(Self::Original);
        };
        let attached = mapping.block.symbol.is_some();
        if target == DocumentationProjectionTarget::All && attached {
            if mapping.owner {
                return Ok(Self::Original);
            }
            if owner_answers.contains(&mapping.block.identity) {
                return Ok(Self::Suppressed);
            }
        }
        DocumentIdentity::for_documentation_block(&mapping.block.identity.0)
            .map(Self::Block)
            .map_err(|error| {
                caused_by(
                    DocumentationViolation::LimitExceeded,
                    "projection.identity",
                    error,
                )
            })
    }
}

/// One projected input's order, with duplicate identities unioned in place.
#[derive(Default)]
struct ProjectedOrder {
    order: Vec<RankedIdentity>,
    positions: std::collections::BTreeMap<DocumentIdentity, usize>,
}

impl ProjectedOrder {
    fn push(&mut self, identity: DocumentIdentity, fields: FieldSet) {
        push_or_union(&mut self.order, &mut self.positions, identity, fields);
    }

    fn into_order(self) -> Vec<RankedIdentity> {
        self.order
    }
}

/// Source identities across a layer's collections, sorted, each held by one collection.
fn layer_sources(
    collections: &[HeldCollection<'_>],
    bound: usize,
) -> Result<Vec<(DocumentationContentIdentity, u32)>, DocumentationError> {
    let count = collections.iter().try_fold(0_usize, |count, held| {
        count.checked_add(held.collection().index().sources.len())
    });
    if count.is_none_or(|count| count > bound) {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "projection.sources",
        ));
    }
    let mut sources = Vec::with_capacity(count.unwrap_or_default());
    for (position, held) in (0_u32..).zip(collections) {
        sources.extend(
            held.collection()
                .index()
                .sources
                .iter()
                .map(|source| (source.identity.clone(), position)),
        );
    }
    sort_by_key(&mut sources);
    if sources.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(refused(
            DocumentationViolation::DuplicateSource,
            "projection.source",
        ));
    }
    Ok(sources)
}

/// Block identities across a layer's collections, sorted, each held by one collection.
fn layer_blocks(
    collections: &[HeldCollection<'_>],
    bound: usize,
) -> Result<Vec<(DocumentationDigest, BlockAddress)>, DocumentationError> {
    let count = collections.iter().try_fold(0_usize, |count, held| {
        count.checked_add(held.collection().index().blocks.len())
    });
    if count.is_none_or(|count| count > bound) {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "projection.blocks",
        ));
    }
    let mut blocks = Vec::with_capacity(count.unwrap_or_default());
    for (collection, held) in (0_u32..).zip(collections) {
        blocks.extend(
            (0_u32..)
                .zip(&held.collection().index().blocks)
                .map(|(block, found)| (found.identity.clone(), BlockAddress { collection, block })),
        );
    }
    sort_by_key(&mut blocks);
    if blocks.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(refused(
            DocumentationViolation::Identity,
            "projection.block",
        ));
    }
    Ok(blocks)
}

/// Every ranking identity a layer maps, sorted by identity, in collection order within one
/// identity, with a repeated block and owner kept once.
///
/// Each collection's mappings are derived independently, so with the `parallel` feature
/// they are derived across the rayon pool and sorted with its stable parallel sort; the
/// order a lookup answers in is the same either way.
fn layer_mappings(
    collections: &[HeldCollection<'_>],
    bound: usize,
) -> Result<Vec<(DocumentIdentity, Mapping)>, DocumentationError> {
    let derived = derive_mappings(collections)?;
    let count = derived.iter().try_fold(0_usize, |count, additions| {
        count.checked_add(additions.len())
    });
    if count.is_none_or(|count| count > bound) {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "projection.mappings",
        ));
    }
    let mut mappings = derived.into_iter().flatten().collect::<Vec<_>>();
    sort_by_key(&mut mappings);
    Ok(without_repeated_blocks(mappings))
}

#[cfg(feature = "parallel")]
fn derive_mappings(
    collections: &[HeldCollection<'_>],
) -> Result<Vec<Vec<(DocumentIdentity, Mapping)>>, DocumentationError> {
    use rayon::prelude::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};

    collections
        .par_iter()
        .enumerate()
        .map(|(position, held)| {
            collection_mappings(held.collection(), collection_position(position))
        })
        .collect()
}

#[cfg(not(feature = "parallel"))]
fn derive_mappings(
    collections: &[HeldCollection<'_>],
) -> Result<Vec<Vec<(DocumentIdentity, Mapping)>>, DocumentationError> {
    collections
        .iter()
        .enumerate()
        .map(|(position, held)| {
            collection_mappings(held.collection(), collection_position(position))
        })
        .collect()
}

/// A collection's position, which [`DocumentationLayer::build`] checked fits `u32`.
fn collection_position(position: usize) -> u32 {
    u32::try_from(position).unwrap_or(u32::MAX)
}

/// Stable sort on the first member, across the rayon pool with the `parallel` feature.
fn sort_by_key<K: Ord + Send, V: Send>(entries: &mut [(K, V)]) {
    #[cfg(feature = "parallel")]
    {
        use rayon::slice::ParallelSliceMut;
        entries.par_sort_by(|left, right| left.0.cmp(&right.0));
    }
    #[cfg(not(feature = "parallel"))]
    entries.sort_by(|left, right| left.0.cmp(&right.0));
}

/// Keeps the first mapping for each identity, block, and owner, in sorted order.
///
/// One heading identity can map onto every block under that heading, so the blocks one
/// identity already kept are remembered in a set rather than rescanned.
fn without_repeated_blocks(
    mappings: Vec<(DocumentIdentity, Mapping)>,
) -> Vec<(DocumentIdentity, Mapping)> {
    let mut kept: Vec<(DocumentIdentity, Mapping)> = Vec::with_capacity(mappings.len());
    let mut group_start = 0;
    let mut seen = BTreeSet::<(u32, u32, bool)>::new();
    for (identity, mapping) in mappings {
        if kept
            .get(group_start)
            .is_none_or(|(first, _)| first != &identity)
        {
            group_start = kept.len();
            seen.clear();
        }
        if seen.insert((mapping.block.collection, mapping.block.block, mapping.owner)) {
            kept.push((identity, mapping));
        }
    }
    kept
}

fn collection_mappings(
    collection: &DocumentationCollection,
    position: u32,
) -> Result<Vec<(DocumentIdentity, Mapping)>, DocumentationError> {
    let mut additions = Vec::new();
    for (block_position, block) in (0_u32..).zip(&collection.index().blocks) {
        let address = BlockAddress {
            collection: position,
            block: block_position,
        };
        for chunk in &block.chunks {
            let identity = DocumentIdentity::new(chunk.identity.as_str()).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "chunk.identity", error)
            })?;
            additions.push((
                identity,
                Mapping {
                    block: address,
                    document_range: Some(chunk.range.clone()),
                    owner: false,
                },
            ));
        }
        for identity in heading_identities(collection, block)? {
            additions.push((
                identity,
                Mapping {
                    block: address,
                    document_range: None,
                    owner: false,
                },
            ));
        }
        for identity in owner_identities(block)? {
            additions.push((
                identity,
                Mapping {
                    block: address,
                    document_range: None,
                    owner: true,
                },
            ));
        }
    }
    Ok(additions)
}

/// The heading identities a Markdown or MDX block sits under, outermost first.
fn heading_identities(
    collection: &DocumentationCollection,
    block: &DocumentationBlock,
) -> Result<Vec<DocumentIdentity>, DocumentationError> {
    let markdown = collection.source(&block.source).is_some_and(|source| {
        matches!(
            source.format,
            DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
        )
    });
    if !markdown {
        return Ok(Vec::new());
    }
    block
        .heading_path
        .iter()
        .map(|heading| heading_identity(&block.source, &heading.name))
        .collect()
}

/// The identities of the declaration an attached comment belongs to: its symbol, and for
/// a package block also the unit-scoped identity package declarations rank under.
fn owner_identities(
    block: &DocumentationBlock,
) -> Result<Vec<DocumentIdentity>, DocumentationError> {
    let Some(symbol) = &block.symbol else {
        return Ok(Vec::new());
    };
    let mut identities = vec![
        DocumentIdentity::new(symbol.0.clone())
            .map_err(|error| caused_by(DocumentationViolation::Identity, "block.symbol", error))?,
    ];
    if let DocumentationSourceIdentity::Package { unit } = &block.source.source {
        let unit = rift_core::SourceUnitId::parse(&unit.0)
            .map_err(|error| caused_by(DocumentationViolation::Identity, "block.source", error))?;
        let symbol = rift_core::parse_symbol_identity(&symbol.0)
            .map_err(|error| caused_by(DocumentationViolation::Identity, "block.symbol", error))?;
        identities.push(
            DocumentIdentity::for_unit(&unit, symbol.qualified_name()).map_err(|error| {
                caused_by(DocumentationViolation::Identity, "block.symbol", error)
            })?,
        );
    }
    Ok(identities)
}

fn heading_identity(
    source: &DocumentationContentIdentity,
    name: &str,
) -> Result<DocumentIdentity, DocumentationError> {
    match &source.source {
        DocumentationSourceIdentity::Project { path } => DocumentIdentity::new(
            rift_core::symbol_identity("markdown", &path.0, name),
        )
        .map_err(|error| caused_by(DocumentationViolation::Identity, "heading.identity", error)),
        DocumentationSourceIdentity::Package { unit } => {
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

/// Picks the narrowest block containing the located match, ties broken by source, start,
/// then block identity; with no located match, the first mapping.
fn choose_mapping<'layer, F>(
    mappings: &[ResolvedMapping<'layer>],
    identity: &DocumentIdentity,
    locate: &mut F,
) -> Option<ResolvedMapping<'layer>>
where
    F: FnMut(&DocumentIdentity, &DocumentationContentIdentity) -> Option<TextRange>,
{
    let mut located = false;
    let mut best: Option<ResolvedMapping<'layer>> = None;
    for mapping in mappings {
        let Some(range) = locate(identity, &mapping.block.source) else {
            continue;
        };
        located = true;
        if !contains(&mapping.block.range, &range) {
            continue;
        }
        if best.is_none_or(|current| narrower(mapping.block, current.block)) {
            best = Some(*mapping);
        }
    }
    if located {
        best
    } else {
        mappings.first().copied()
    }
}

/// Whether `candidate` is a narrower match than `current`, ties broken by source, start,
/// then block identity.
fn narrower(candidate: &DocumentationBlock, current: &DocumentationBlock) -> bool {
    let candidate_key = (
        range_len(&candidate.range),
        &candidate.source,
        candidate.range.start,
        &candidate.identity,
    );
    let current_key = (
        range_len(&current.range),
        &current.source,
        current.range.start,
        &current.identity,
    );
    candidate_key < current_key
}

fn contains(outer: &TextRange, inner: &TextRange) -> bool {
    outer.start <= inner.start && inner.start < inner.end && inner.end <= outer.end
}

fn range_len(range: &TextRange) -> u64 {
    range.end - range.start
}

fn collapse_input(input: &RankingInput) -> Vec<RankedIdentity> {
    let mut order = ProjectedOrder::default();
    for ranked in input.order() {
        order.push(ranked.identity().clone(), ranked.fields());
    }
    order.into_order()
}

fn push_or_union(
    order: &mut Vec<RankedIdentity>,
    positions: &mut std::collections::BTreeMap<DocumentIdentity, usize>,
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

    use super::{
        DocumentationLayer, DocumentationProjection, DocumentationProjectionTarget, HeldCollection,
        LayerBounds,
    };

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

    fn source_named(path: &str) -> DocumentationSource {
        source(path, "Guide", DocumentationSourceFormat::Markdown)
    }

    fn bounded(
        collections: &[&DocumentationCollection],
        bounds: LayerBounds,
    ) -> Result<DocumentationLayer<'static>, crate::documentation::DocumentationError> {
        let shared = collections
            .iter()
            .map(|collection| {
                HeldCollection::Shared(std::sync::Arc::new(
                    DocumentationCollection::new(collection.index().clone())
                        .expect("fixture collection"),
                ))
            })
            .collect();
        DocumentationLayer::build(shared, bounds)
    }

    #[test]
    fn repeated_source_and_block_are_refused_by_the_layer() {
        let source = source_named("guide.md");
        let first = collection(
            vec![source.clone()],
            vec![block(
                &source,
                "guide",
                TextRange { start: 0, end: 5 },
                vec![("guide-chunk", TextRange { start: 0, end: 5 })],
                &[],
                None,
            )],
        );
        let duplicate_source = collection(vec![source.clone()], Vec::new());
        let error = DocumentationLayer::borrowed(&[&first, &duplicate_source])
            .expect_err("duplicate source refused");
        assert_eq!(error.fault().field(), "projection.source");

        let other = source_named("other.md");
        let same_block = collection(
            vec![other.clone()],
            vec![block(
                &other,
                "guide",
                TextRange { start: 0, end: 5 },
                Vec::new(),
                &[],
                None,
            )],
        );
        let error = DocumentationLayer::borrowed(&[&first, &same_block])
            .expect_err("duplicate block refused");
        assert_eq!(error.fault().field(), "projection.block");

        let layer = DocumentationLayer::borrowed(&[&first]).expect("single collection layer");
        let projected = DocumentationProjection::default()
            .with_layer(&layer)
            .project(
                &[input("guide-chunk", FieldSet::EMPTY)],
                DocumentationProjectionTarget::Documentation,
                |_, _| Some(TextRange { start: 1, end: 3 }),
            )
            .expect("layer projects");
        assert_eq!(projected[0].order().len(), 1);
    }

    #[test]
    fn layer_bounds_accept_the_exact_count_and_refuse_one_more() {
        let first = source_named("one.md");
        let second = source_named("two.md");
        let one = collection(
            vec![first.clone()],
            vec![block(
                &first,
                "one",
                TextRange { start: 0, end: 5 },
                vec![("one-chunk", TextRange { start: 0, end: 5 })],
                &[],
                None,
            )],
        );
        let two = collection(
            vec![second.clone()],
            vec![block(
                &second,
                "two",
                TextRange { start: 0, end: 5 },
                vec![("two-chunk", TextRange { start: 0, end: 5 })],
                &[],
                None,
            )],
        );
        let exact = LayerBounds {
            sources: 2,
            blocks: 2,
            mappings: 2,
        };
        let layer = bounded(&[&one, &two], exact).expect("exact bounds");
        assert_eq!(layer.block_count(), 2);
        assert_eq!(layer.mapping_count(), 2);
        for (bounds, field) in [
            (
                LayerBounds {
                    sources: 1,
                    ..exact
                },
                "projection.sources",
            ),
            (LayerBounds { blocks: 1, ..exact }, "projection.blocks"),
            (
                LayerBounds {
                    mappings: 1,
                    ..exact
                },
                "projection.mappings",
            ),
        ] {
            let error = bounded(&[&one, &two], bounds).expect_err("one past the bound");
            assert_eq!(error.fault().field(), field);
        }
    }

    #[test]
    fn shared_layer_answers_every_projection_that_joins_it() {
        let source = source_named("guide.md");
        let shared = std::sync::Arc::new(collection(
            vec![source.clone()],
            vec![block(
                &source,
                "guide",
                TextRange { start: 0, end: 5 },
                vec![("guide-chunk", TextRange { start: 0, end: 5 })],
                &["Guide"],
                None,
            )],
        ));
        let layer =
            DocumentationLayer::shared([std::sync::Arc::clone(&shared)]).expect("shared layer");
        assert_eq!(layer.mapping_count(), 2);
        for _ in 0..2 {
            let projection = DocumentationProjection::default().with_layer(&layer);
            let projected = projection
                .project(
                    &[input("guide-chunk", FieldSet::EMPTY)],
                    DocumentationProjectionTarget::Documentation,
                    |_, _| None,
                )
                .expect("projection");
            let block = projection
                .block(projected[0].order()[0].identity())
                .expect("projected block");
            assert_eq!(block.identity, content_digest(b"guide"));
            assert!(projection.source(&source.identity).is_some());
        }
    }

    #[test]
    fn a_repeated_identity_block_and_owner_is_kept_once_in_join_order() {
        let source = source(
            "docs/guide.md",
            "0123456789",
            DocumentationSourceFormat::Markdown,
        );
        let blocks = vec![
            block(
                &source,
                "first",
                TextRange { start: 0, end: 5 },
                vec![("docs/chunk", TextRange { start: 0, end: 5 })],
                &["Guide", "Guide"],
                None,
            ),
            block(
                &source,
                "second",
                TextRange { start: 5, end: 10 },
                vec![("docs/chunk", TextRange { start: 5, end: 10 })],
                &["Guide"],
                None,
            ),
        ];
        let collection = collection(vec![source], blocks);
        let layer = DocumentationLayer::borrowed(&[&collection]).expect("layer");
        // `docs/chunk` and the heading identity each reach both blocks; the repeated
        // heading on the first block's path maps onto it once.
        assert_eq!(layer.mapping_count(), 4);
        let projection = DocumentationProjection::default().with_layer(&layer);
        let first = projection
            .project(
                &[input("docs/chunk", FieldSet::EMPTY)],
                DocumentationProjectionTarget::Documentation,
                |_, _| None,
            )
            .expect("unlocated projection");
        assert_eq!(
            projection
                .block(first[0].order()[0].identity())
                .expect("first mapped block")
                .identity,
            content_digest(b"first")
        );
    }

    #[test]
    fn layers_answer_in_join_order() {
        let project_source = source_named("docs/project.md");
        let package_source = source_named("docs/package.md");
        let project = collection(
            vec![project_source.clone()],
            vec![block(
                &project_source,
                "project",
                TextRange { start: 0, end: 5 },
                vec![("shared-chunk", TextRange { start: 0, end: 5 })],
                &[],
                None,
            )],
        );
        let package = collection(
            vec![package_source.clone()],
            vec![block(
                &package_source,
                "package",
                TextRange { start: 0, end: 5 },
                vec![("shared-chunk", TextRange { start: 0, end: 5 })],
                &[],
                None,
            )],
        );
        let project_layer = DocumentationLayer::borrowed(&[&project]).expect("project layer");
        let package_layer = DocumentationLayer::borrowed(&[&package]).expect("package layer");
        let projection = DocumentationProjection::default()
            .with_layer(&project_layer)
            .with_layer(&package_layer);
        let identity = DocumentIdentity::new("shared-chunk").expect("identity");
        assert_eq!(
            projection.document_source(&identity),
            Some(&project_source.identity)
        );
        let projected = projection
            .project(
                &[input("shared-chunk", FieldSet::EMPTY)],
                DocumentationProjectionTarget::Documentation,
                |_, _| None,
            )
            .expect("projection");
        assert_eq!(
            projection
                .hit(projected[0].order()[0].identity())
                .expect("hit")
                .source
                .identity,
            project_source.identity
        );
        assert!(projection.source(&package_source.identity).is_some());
    }

    #[test]
    fn overlapping_blocks_choose_shortest_range_then_earliest_start() {
        let source = source(
            "guide.md",
            "0123456789abcdefghij",
            DocumentationSourceFormat::Markdown,
        );
        let blocks = [("outer", 0, 20), ("earlier", 2, 10), ("later", 3, 11)]
            .into_iter()
            .map(|(name, start, end)| {
                block(
                    &source,
                    name,
                    TextRange { start, end },
                    vec![("guide-chunk", TextRange { start: 0, end: 20 })],
                    &[],
                    None,
                )
            })
            .collect();
        let collection = collection(vec![source], blocks);
        let projection = DocumentationProjection::new(&collection).expect("projection");
        let projected = projection
            .project(
                &[input("guide-chunk", FieldSet::EMPTY)],
                DocumentationProjectionTarget::Documentation,
                |_, _| Some(TextRange { start: 4, end: 6 }),
            )
            .expect("overlapping projection");
        let selected = projection
            .block(projected[0].order()[0].identity())
            .expect("selected block");
        assert_eq!(selected.identity, content_digest(b"earlier"));
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
        let mut layer = DocumentationLayer::borrowed(&[&collection]).expect("layer");
        let package_owner = DocumentIdentity::new("package-owner").expect("ranking identity");
        layer
            .associate_document(package_owner.clone(), &block_identity)
            .expect("owner association");
        layer
            .associate_document(package_owner.clone(), &block_identity)
            .expect("repeated association is kept once");
        let projection = DocumentationProjection::default().with_owned_layer(layer);
        let inputs = [RankingInput::new(
            RankingInputKind::Lexical,
            vec![RankedIdentity::new(package_owner.clone(), FieldSet::EMPTY)],
        )];
        let result = projection
            .project(&inputs, DocumentationProjectionTarget::All, |_, _| None)
            .expect("all projection");
        assert_eq!(result[0].order()[0].identity(), &package_owner);
    }

    #[test]
    fn association_refuses_unknown_and_unowned_blocks() {
        let source = source(
            "docs/guide.md",
            "guide",
            DocumentationSourceFormat::Markdown,
        );
        let block = block(
            &source,
            "guide",
            TextRange { start: 0, end: 5 },
            vec![("docs/guide#chunk/0", TextRange { start: 0, end: 5 })],
            &[],
            None,
        );
        let block_identity = block.identity.clone();
        let collection = collection(vec![source], vec![block]);
        let mut layer = DocumentationLayer::borrowed(&[&collection]).expect("layer");

        let unknown = layer
            .associate_document(
                DocumentIdentity::new("package-owner").expect("identity"),
                &content_digest(b"unknown"),
            )
            .expect_err("unknown block");
        assert_eq!(unknown.fault().field(), "block.identity");

        let owner = layer
            .associate_document(
                DocumentIdentity::new("package-owner").expect("identity"),
                &block_identity,
            )
            .expect_err("block has no symbol owner");
        assert_eq!(owner.fault().field(), "block.symbol");
    }

    /// Regression for #362: a candidate set past the collection block bound projects,
    /// where the projection used to refuse the whole search.
    #[test]
    fn candidates_past_the_block_bound_still_project() {
        let source = source(
            "docs/guide.md",
            "guide",
            DocumentationSourceFormat::Markdown,
        );
        let block = block(
            &source,
            "guide",
            TextRange { start: 0, end: 5 },
            vec![("docs/guide#chunk/0", TextRange { start: 0, end: 5 })],
            &[],
            None,
        );
        let collection = collection(vec![source], vec![block]);
        let projection = DocumentationProjection::new(&collection).expect("projection");
        let candidates = (0..=rift_protocol::documentation::DOCUMENTATION_BLOCKS_MAX)
            .map(|index| {
                RankedIdentity::new(
                    DocumentIdentity::new(format!("candidate-{index}"))
                        .expect("candidate identity"),
                    FieldSet::EMPTY,
                )
            })
            .chain(std::iter::once(RankedIdentity::new(
                DocumentIdentity::new("docs/guide#chunk/0").expect("chunk identity"),
                FieldSet::EMPTY,
            )))
            .collect();
        let inputs = [RankingInput::new(RankingInputKind::Lexical, candidates)];
        let projected = projection
            .project(
                &inputs,
                DocumentationProjectionTarget::Documentation,
                |_, _| None,
            )
            .expect("no candidate bound refuses the read");
        assert_eq!(projected[0].order().len(), 1);
    }
}
