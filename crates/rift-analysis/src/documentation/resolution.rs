//! Reuses documentation resolution results when resolver facts stay unchanged.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::documentation::{
    DOCUMENTATION_BLOCKS_MAX, DOCUMENTATION_REFERENCES_MAX, DOCUMENTATION_SOURCES_MAX,
    DocumentationBlock, DocumentationContentIdentity, DocumentationDigest, DocumentationLink,
    DocumentationReference, DocumentationReferenceCandidate, DocumentationReferenceEvidence,
    DocumentationSource, DocumentationUnresolvedReason,
};
use rift_protocol::read::{Digest, Language, SymbolId};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::canonical_digest;
use super::links::DocumentationFragment;
use super::links::{DeclarationLinkMatch, DeclarationLinkNames, Destination, local_destination};
use super::references::{DeclarationNames, DocumentationDeclaration};

const RESOLUTION_DEPENDENCIES_MAX: usize = DOCUMENTATION_REFERENCES_MAX as usize * 5;

/// Current facts consumed by documentation link and declaration resolution.
pub(super) struct ResolutionInput<'a> {
    pub sources: &'a [DocumentationSource],
    pub blocks: &'a [DocumentationBlock],
    pub resolvable_links: &'a [DocumentationLink],
    pub fixed_unresolved_links: &'a [DocumentationLink],
    pub fragments: &'a [DocumentationFragment],
    pub candidates: &'a [DocumentationReferenceCandidate],
    pub declarations: &'a [DocumentationDeclaration<'a>],
}

/// Resolved documentation facts and the next bounded process-local cache.
pub(super) struct ResolutionOutput {
    pub resolvable_links: Vec<DocumentationLink>,
    pub fixed_unresolved_links: Vec<DocumentationLink>,
    pub references: Vec<DocumentationReference>,
    pub unresolved_references: Vec<DocumentationReferenceCandidate>,
    pub next_cache: Option<ResolutionCache>,
}

/// Resolved outputs keyed by block and their exact resolver dependencies.
#[derive(Debug, Default)]
pub(super) struct ResolutionCache {
    revision: Option<Digest>,
    complete: bool,
    blocks: BTreeMap<DocumentationDigest, CachedBlock>,
    reverse: BTreeMap<DependencyKey, BTreeSet<DocumentationDigest>>,
    states: BTreeMap<DependencyKey, DependencyState>,
    #[cfg(test)]
    recomputed_blocks: usize,
}

#[derive(Debug)]
struct CachedBlock {
    input: DocumentationDigest,
    resolvable_links: Vec<DocumentationLink>,
    fixed_unresolved_links: Vec<DocumentationLink>,
    references: Vec<DocumentationReference>,
    candidate_results: Vec<Option<DocumentationReferenceCandidate>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DependencyKey {
    Target {
        source: DocumentationContentIdentity,
        fragment: FragmentKey,
    },
    DirectSymbol(String),
    Qualified {
        source: DocumentationContentIdentity,
        name: String,
    },
    Reference {
        authored: String,
        language: Option<LanguageKey>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FragmentKey {
    WholeSource,
    Name(String),
    Invalid,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LanguageKey {
    name: String,
    dialect: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DependencyState {
    Target(TargetState),
    DirectSymbol(Option<SymbolId>),
    Qualified(NameState),
    Reference(ReferenceState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TargetState {
    MissingSource,
    WholeSource {
        byte_length: u64,
    },
    Fragment {
        byte_length: u64,
        result: FragmentState,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FragmentState {
    Range { start: u64, end: u64 },
    Missing,
    Ambiguous,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NameState {
    Symbol(SymbolId),
    Missing,
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReferenceState {
    Match(SymbolId, DocumentationReferenceEvidence),
    Missing,
    Ambiguous,
}

#[derive(Default)]
struct BlockFacts<'a> {
    resolvable_links: Vec<&'a DocumentationLink>,
    fixed_unresolved_links: Vec<&'a DocumentationLink>,
    candidates: Vec<&'a DocumentationReferenceCandidate>,
}

impl ResolutionCache {
    /// Reuses results for blocks whose raw facts and resolver dependencies are unchanged.
    ///
    /// The cache retains identities, ranges, authored spellings, and resolved metadata. Source
    /// text remains with the caller. A missing or over-bound cache triggers one complete bounded
    /// resolution pass and may omit the next cache.
    pub(super) fn resolve(
        previous: Option<&Self>,
        input: &ResolutionInput<'_>,
    ) -> Result<ResolutionOutput, DocumentationError> {
        validate_input(input)?;
        let indexes = ResolverIndexes::new(input)?;
        let facts = collect_block_facts(input)?;
        let plan = ResolutionPlan::new(previous, &facts, &indexes)?;
        let affected = plan.affected_blocks(previous, &facts);
        let recomputed_blocks = affected
            .iter()
            .filter(|block_id| facts.contains_key(*block_id))
            .count();
        let results = resolve_affected_blocks(
            input,
            &facts,
            &indexes,
            &plan.fingerprints,
            &affected,
            previous,
        )?;
        assemble_output(input, results, plan, recomputed_blocks)
    }

    #[cfg(test)]
    fn recomputed_blocks(&self) -> usize {
        self.recomputed_blocks
    }
}

struct ResolverIndexes<'a> {
    sources: BTreeMap<&'a DocumentationContentIdentity, &'a DocumentationSource>,
    blocks: BTreeMap<&'a DocumentationDigest, &'a DocumentationBlock>,
    fragments: super::links::Fragments<'a>,
    reference_names: DeclarationNames<'a>,
    declaration_names: DeclarationLinkNames<'a>,
}

impl<'a> ResolverIndexes<'a> {
    fn new(input: &ResolutionInput<'a>) -> Result<Self, DocumentationError> {
        Ok(Self {
            sources: input
                .sources
                .iter()
                .map(|source| (&source.identity, source))
                .collect(),
            blocks: input
                .blocks
                .iter()
                .map(|block| (&block.identity, block))
                .collect(),
            fragments: super::links::fragment_index(input.fragments)?,
            reference_names: DeclarationNames::new(input.declarations),
            declaration_names: DeclarationLinkNames::new(input.declarations),
        })
    }
}

struct ResolutionPlan {
    revision: Digest,
    cacheable: bool,
    fingerprints: BTreeMap<DocumentationDigest, DocumentationDigest>,
    current_reverse: BTreeMap<DependencyKey, BTreeSet<DocumentationDigest>>,
    current_states: BTreeMap<DependencyKey, DependencyState>,
}

impl ResolutionPlan {
    fn new(
        previous: Option<&ResolutionCache>,
        facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>,
        indexes: &ResolverIndexes<'_>,
    ) -> Result<Self, DocumentationError> {
        let revision = super::documentation_revision();
        let mut cacheable = dependency_count(facts) <= RESOLUTION_DEPENDENCIES_MAX;
        let mut fingerprints = BTreeMap::new();
        let mut dependencies = BTreeMap::new();
        for (block_id, block_facts) in facts {
            let block = indexes.blocks.get(block_id);
            let source = block.and_then(|block| indexes.sources.get(&block.source).copied());
            fingerprints.insert(
                block_id.clone(),
                block_fingerprint(block, source, block_facts)?,
            );
            if cacheable {
                dependencies.insert(
                    block_id.clone(),
                    block_dependencies(source, block_facts, &indexes.declaration_names)?,
                );
            }
        }
        let mut current_reverse = reverse_dependencies(&dependencies);
        cacheable = cacheable
            && current_reverse.len() <= RESOLUTION_DEPENDENCIES_MAX
            && reverse_membership_count(&current_reverse) <= RESOLUTION_DEPENDENCIES_MAX;
        if !cacheable {
            dependencies.clear();
            current_reverse.clear();
        }
        let previous = valid_previous(previous, &revision, cacheable);
        let keys = dependency_keys(previous, &current_reverse);
        let previous = previous.filter(|_| keys.len() <= RESOLUTION_DEPENDENCIES_MAX);
        let state_keys = if !cacheable {
            BTreeSet::new()
        } else if previous.is_some() {
            keys
        } else {
            current_reverse.keys().cloned().collect()
        };
        let current_states = states_for(&state_keys, indexes);
        Ok(Self {
            revision,
            cacheable,
            fingerprints,
            current_reverse,
            current_states,
        })
    }

    fn affected_blocks(
        &self,
        previous: Option<&ResolutionCache>,
        facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>,
    ) -> BTreeSet<DocumentationDigest> {
        let previous = valid_previous(previous, &self.revision, self.cacheable);
        let Some(previous) = previous else {
            return facts.keys().cloned().collect();
        };
        let mut affected = BTreeSet::new();
        for (block_id, fingerprint) in &self.fingerprints {
            if previous
                .blocks
                .get(block_id)
                .is_none_or(|cached| cached.input != *fingerprint)
            {
                affected.insert(block_id.clone());
            }
        }
        for (key, state) in &self.current_states {
            if previous.states.get(key) != Some(state) {
                extend_dependents(&mut affected, previous.reverse.get(key));
                extend_dependents(&mut affected, self.current_reverse.get(key));
            }
        }
        affected
    }

    fn into_cache(
        self,
        blocks: BTreeMap<DocumentationDigest, CachedBlock>,
        recomputed_blocks: usize,
    ) -> Option<ResolutionCache> {
        #[cfg(not(test))]
        let _ = recomputed_blocks;
        if !self.cacheable {
            return None;
        }
        let states = self
            .current_reverse
            .keys()
            .filter_map(|key| {
                self.current_states
                    .get(key)
                    .map(|state| (key.clone(), state.clone()))
            })
            .collect();
        Some(ResolutionCache {
            revision: Some(self.revision),
            complete: true,
            blocks,
            reverse: self.current_reverse,
            states,
            #[cfg(test)]
            recomputed_blocks,
        })
    }
}

fn valid_previous<'a>(
    previous: Option<&'a ResolutionCache>,
    revision: &Digest,
    cacheable: bool,
) -> Option<&'a ResolutionCache> {
    previous.filter(|cache| {
        cache.revision.as_ref() == Some(revision)
            && cache.complete
            && cacheable
            && cache.blocks.len() <= DOCUMENTATION_BLOCKS_MAX as usize
            && cache.states.len() <= RESOLUTION_DEPENDENCIES_MAX
            && cache.reverse.len() <= RESOLUTION_DEPENDENCIES_MAX
            && reverse_membership_count(&cache.reverse) <= RESOLUTION_DEPENDENCIES_MAX
    })
}

fn dependency_keys(
    previous: Option<&ResolutionCache>,
    current: &BTreeMap<DependencyKey, BTreeSet<DocumentationDigest>>,
) -> BTreeSet<DependencyKey> {
    previous
        .into_iter()
        .flat_map(|cache| cache.states.keys())
        .chain(current.keys())
        .cloned()
        .collect()
}

fn states_for(
    keys: &BTreeSet<DependencyKey>,
    indexes: &ResolverIndexes<'_>,
) -> BTreeMap<DependencyKey, DependencyState> {
    keys.iter()
        .map(|key| {
            (
                key.clone(),
                dependency_state(
                    key,
                    &indexes.sources,
                    &indexes.fragments,
                    &indexes.reference_names,
                    &indexes.declaration_names,
                ),
            )
        })
        .collect()
}

fn collect_block_facts<'a>(
    input: &ResolutionInput<'a>,
) -> Result<BTreeMap<DocumentationDigest, BlockFacts<'a>>, DocumentationError> {
    let mut facts = BTreeMap::<DocumentationDigest, BlockFacts<'_>>::new();
    for link in input.resolvable_links {
        facts
            .entry(link.block.clone())
            .or_default()
            .resolvable_links
            .push(link);
    }
    for link in input.fixed_unresolved_links {
        facts
            .entry(link.block.clone())
            .or_default()
            .fixed_unresolved_links
            .push(link);
    }
    for candidate in input.candidates {
        super::references::validate_candidate(candidate)?;
        facts
            .entry(candidate.block.clone())
            .or_default()
            .candidates
            .push(candidate);
    }
    Ok(facts)
}

fn reverse_dependencies(
    dependencies: &BTreeMap<DocumentationDigest, BTreeSet<DependencyKey>>,
) -> BTreeMap<DependencyKey, BTreeSet<DocumentationDigest>> {
    let mut reverse = BTreeMap::new();
    for (block_id, keys) in dependencies {
        for key in keys {
            reverse
                .entry(key.clone())
                .or_insert_with(BTreeSet::new)
                .insert(block_id.clone());
        }
    }
    reverse
}

fn extend_dependents(
    affected: &mut BTreeSet<DocumentationDigest>,
    dependents: Option<&BTreeSet<DocumentationDigest>>,
) {
    if let Some(dependents) = dependents {
        affected.extend(dependents.iter().cloned());
    }
}

fn resolve_affected_blocks(
    input: &ResolutionInput<'_>,
    facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>,
    indexes: &ResolverIndexes<'_>,
    fingerprints: &BTreeMap<DocumentationDigest, DocumentationDigest>,
    affected: &BTreeSet<DocumentationDigest>,
    previous: Option<&ResolutionCache>,
) -> Result<BTreeMap<DocumentationDigest, CachedBlock>, DocumentationError> {
    let mut results = copy_unaffected(facts, affected, previous);
    initialize_affected(&mut results, facts, affected, fingerprints)?;
    let selected = resolve_selected(input, affected, &indexes.reference_names)?;
    store_selected(&mut results, selected)?;
    Ok(results)
}

struct SelectedResults {
    resolvable_links: Vec<DocumentationLink>,
    fixed_unresolved_links: Vec<DocumentationLink>,
    references: Vec<DocumentationReference>,
    candidates: Vec<(
        DocumentationReferenceCandidate,
        Option<DocumentationReferenceCandidate>,
    )>,
}

fn resolve_selected(
    input: &ResolutionInput<'_>,
    affected: &BTreeSet<DocumentationDigest>,
    reference_names: &DeclarationNames<'_>,
) -> Result<SelectedResults, DocumentationError> {
    let mut resolvable = select_links(input.resolvable_links, affected);
    let fixed = select_links(input.fixed_unresolved_links, affected);
    super::resolve_links(
        input.sources,
        input.blocks,
        &mut resolvable,
        input.fragments,
    )?;
    let resolved_count = resolvable.len();
    resolvable.extend(fixed);
    let mut references = super::links::resolve_declaration_links(
        input.sources,
        input.blocks,
        &mut resolvable,
        input.declarations,
    )?;
    let fixed_unresolved_links = resolvable.split_off(resolved_count);
    let candidates = input
        .candidates
        .iter()
        .filter(|candidate| affected.contains(&candidate.block))
        .cloned()
        .collect::<Vec<_>>();
    let (mut candidate_references, _) =
        super::resolve_references(input.declarations, &candidates)?.into_parts();
    references.append(&mut candidate_references);
    let candidates = candidates
        .into_iter()
        .map(|candidate| {
            let unresolved = reference_names.resolve(&candidate).err().map(|reason| {
                let mut unresolved = candidate.clone();
                unresolved.reason = reason;
                unresolved
            });
            (candidate, unresolved)
        })
        .collect();
    Ok(SelectedResults {
        resolvable_links: resolvable,
        fixed_unresolved_links,
        references,
        candidates,
    })
}

fn select_links(
    links: &[DocumentationLink],
    affected: &BTreeSet<DocumentationDigest>,
) -> Vec<DocumentationLink> {
    links
        .iter()
        .filter(|link| affected.contains(&link.block))
        .cloned()
        .collect()
}

fn copy_unaffected(
    facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>,
    affected: &BTreeSet<DocumentationDigest>,
    previous: Option<&ResolutionCache>,
) -> BTreeMap<DocumentationDigest, CachedBlock> {
    let mut results = BTreeMap::new();
    if let Some(previous) = previous {
        for (block_id, cached) in &previous.blocks {
            if facts.contains_key(block_id) && !affected.contains(block_id) {
                results.insert(block_id.clone(), clone_cached(cached));
            }
        }
    }
    results
}

fn initialize_affected(
    results: &mut BTreeMap<DocumentationDigest, CachedBlock>,
    facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>,
    affected: &BTreeSet<DocumentationDigest>,
    fingerprints: &BTreeMap<DocumentationDigest, DocumentationDigest>,
) -> Result<(), DocumentationError> {
    for block_id in affected {
        if facts.contains_key(block_id) {
            let input = fingerprints.get(block_id).ok_or_else(|| {
                refused(
                    DocumentationViolation::MissingTarget,
                    "resolution.fingerprint",
                )
            })?;
            results.insert(block_id.clone(), empty_cached(input.clone()));
        }
    }
    Ok(())
}

fn empty_cached(input: DocumentationDigest) -> CachedBlock {
    CachedBlock {
        input,
        resolvable_links: Vec::new(),
        fixed_unresolved_links: Vec::new(),
        references: Vec::new(),
        candidate_results: Vec::new(),
    }
}

fn store_selected(
    results: &mut BTreeMap<DocumentationDigest, CachedBlock>,
    selected: SelectedResults,
) -> Result<(), DocumentationError> {
    for link in selected.resolvable_links {
        cached_block_mut(results, &link.block, "link.block")?
            .resolvable_links
            .push(link);
    }
    for link in selected.fixed_unresolved_links {
        cached_block_mut(results, &link.block, "link.block")?
            .fixed_unresolved_links
            .push(link);
    }
    for reference in selected.references {
        cached_block_mut(results, &reference.block, "reference.block")?
            .references
            .push(reference);
    }
    for (candidate, unresolved) in selected.candidates {
        cached_block_mut(results, &candidate.block, "candidate.block")?
            .candidate_results
            .push(unresolved);
    }
    Ok(())
}

fn cached_block_mut<'a>(
    results: &'a mut BTreeMap<DocumentationDigest, CachedBlock>,
    block: &DocumentationDigest,
    field: &'static str,
) -> Result<&'a mut CachedBlock, DocumentationError> {
    results
        .get_mut(block)
        .ok_or_else(|| refused(DocumentationViolation::MissingTarget, field))
}

fn assemble_output(
    input: &ResolutionInput<'_>,
    results: BTreeMap<DocumentationDigest, CachedBlock>,
    plan: ResolutionPlan,
    recomputed_blocks: usize,
) -> Result<ResolutionOutput, DocumentationError> {
    let resolvable_links = assemble_links(input.resolvable_links, &results, true)?;
    let fixed_unresolved_links = assemble_links(input.fixed_unresolved_links, &results, false)?;
    let mut references = results
        .values()
        .flat_map(|block| block.references.iter().cloned())
        .collect::<Vec<_>>();
    references.sort_by(reference_order);
    let unresolved_references = assemble_candidates(input.candidates, &results)?;
    let next_cache = plan.into_cache(results, recomputed_blocks);
    Ok(ResolutionOutput {
        resolvable_links,
        fixed_unresolved_links,
        references,
        unresolved_references,
        next_cache,
    })
}

fn validate_input(input: &ResolutionInput<'_>) -> Result<(), DocumentationError> {
    if input.sources.len() > DOCUMENTATION_SOURCES_MAX as usize
        || input.blocks.len() > DOCUMENTATION_BLOCKS_MAX as usize
        || input
            .resolvable_links
            .len()
            .saturating_add(input.fixed_unresolved_links.len())
            > DOCUMENTATION_REFERENCES_MAX as usize
        || input.candidates.len() > DOCUMENTATION_REFERENCES_MAX as usize
        || input.fragments.len() > DOCUMENTATION_REFERENCES_MAX as usize
        || input.declarations.len() > rift_protocol::index::PACKAGE_SYMBOLS_MAX as usize
    {
        return Err(refused(DocumentationViolation::LimitExceeded, "resolution"));
    }
    Ok(())
}

fn dependency_count(facts: &BTreeMap<DocumentationDigest, BlockFacts<'_>>) -> usize {
    facts.values().fold(0_usize, |total, block| {
        total
            .saturating_add(block.resolvable_links.len().saturating_mul(3))
            .saturating_add(block.fixed_unresolved_links.len().saturating_mul(2))
            .saturating_add(block.candidates.len())
    })
}

fn reverse_membership_count(
    reverse: &BTreeMap<DependencyKey, BTreeSet<DocumentationDigest>>,
) -> usize {
    reverse
        .values()
        .fold(0_usize, |total, blocks| total.saturating_add(blocks.len()))
}

fn block_fingerprint(
    block: Option<&&DocumentationBlock>,
    source: Option<&DocumentationSource>,
    facts: &BlockFacts<'_>,
) -> Result<DocumentationDigest, DocumentationError> {
    let source_identity = block.map(|block| &block.source);
    let package = source.and_then(|source| source.origin.package.as_ref());
    canonical_digest(&(
        source_identity,
        package,
        &facts.resolvable_links,
        &facts.fixed_unresolved_links,
        &facts.candidates,
    ))
}

fn block_dependencies(
    source: Option<&DocumentationSource>,
    facts: &BlockFacts<'_>,
    declarations: &DeclarationLinkNames<'_>,
) -> Result<BTreeSet<DependencyKey>, DocumentationError> {
    let mut keys = BTreeSet::new();
    for &link in &facts.resolvable_links {
        add_link_dependencies(&mut keys, source, link, declarations, true)?;
    }
    for &link in &facts.fixed_unresolved_links {
        add_link_dependencies(&mut keys, source, link, declarations, false)?;
    }
    for candidate in &facts.candidates {
        keys.insert(DependencyKey::Reference {
            authored: candidate.authored.clone(),
            language: candidate.language.as_ref().map(language_key),
        });
    }
    Ok(keys)
}

fn add_link_dependencies(
    keys: &mut BTreeSet<DependencyKey>,
    source: Option<&DocumentationSource>,
    link: &DocumentationLink,
    declarations: &DeclarationLinkNames<'_>,
    source_resolution: bool,
) -> Result<(), DocumentationError> {
    keys.insert(DependencyKey::DirectSymbol(link.authored.clone()));
    if declarations.direct(&link.authored).is_some() {
        return Ok(());
    }
    let source =
        source.ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.source"))?;
    let Destination::Local { identity, fragment } = local_destination(source, &link.authored)?
    else {
        return Ok(());
    };
    let fragment_key = match fragment.as_deref() {
        None | Some("") => FragmentKey::WholeSource,
        Some(fragment) => match percent_encoding::percent_decode_str(fragment).decode_utf8() {
            Ok(name) => {
                let name = name.into_owned();
                if name.is_empty() {
                    FragmentKey::WholeSource
                } else {
                    let qualified = declarations.qualified(&identity, &name);
                    keys.insert(DependencyKey::Qualified {
                        source: identity.clone(),
                        name: name.clone(),
                    });
                    if qualified != DeclarationLinkMatch::Missing {
                        return Ok(());
                    }
                    FragmentKey::Name(name)
                }
            }
            Err(_) => FragmentKey::Invalid,
        },
    };
    if source_resolution {
        keys.insert(DependencyKey::Target {
            source: identity,
            fragment: fragment_key,
        });
    }
    Ok(())
}

fn language_key(language: &Language) -> LanguageKey {
    LanguageKey {
        name: language.name.clone(),
        dialect: language.dialect.clone(),
    }
}

fn dependency_state(
    key: &DependencyKey,
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
    fragments: &super::links::Fragments<'_>,
    reference_names: &DeclarationNames<'_>,
    declaration_names: &DeclarationLinkNames<'_>,
) -> DependencyState {
    match key {
        DependencyKey::Target { source, fragment } => {
            let Some(record) = sources.get(source) else {
                return DependencyState::Target(TargetState::MissingSource);
            };
            let target = match fragment {
                FragmentKey::WholeSource => TargetState::WholeSource {
                    byte_length: record.byte_length,
                },
                FragmentKey::Invalid => TargetState::Fragment {
                    byte_length: record.byte_length,
                    result: FragmentState::Invalid,
                },
                FragmentKey::Name(name) => TargetState::Fragment {
                    byte_length: record.byte_length,
                    result: fragment_state(record, name, fragments),
                },
            };
            DependencyState::Target(target)
        }
        DependencyKey::DirectSymbol(authored) => {
            DependencyState::DirectSymbol(declaration_names.direct(authored).cloned())
        }
        DependencyKey::Qualified { source, name } => {
            let state = match declaration_names.qualified(source, name) {
                DeclarationLinkMatch::Symbol(symbol) => NameState::Symbol(symbol),
                DeclarationLinkMatch::Ambiguous => NameState::Ambiguous,
                DeclarationLinkMatch::Missing => NameState::Missing,
            };
            DependencyState::Qualified(state)
        }
        DependencyKey::Reference { authored, language } => {
            let language = language.as_ref().map(|language| Language {
                name: language.name.clone(),
                dialect: language.dialect.clone(),
            });
            let state = match reference_names.resolve_name(authored, language.as_ref()) {
                Ok((symbol, evidence)) => ReferenceState::Match(symbol.clone(), evidence),
                Err(DocumentationUnresolvedReason::Ambiguous) => ReferenceState::Ambiguous,
                Err(_) => ReferenceState::Missing,
            };
            DependencyState::Reference(state)
        }
    }
}

fn fragment_state(
    record: &DocumentationSource,
    name: &str,
    fragments: &super::links::Fragments<'_>,
) -> FragmentState {
    match fragments.get(&(&record.identity, name)) {
        Some(Some(range)) if range.end <= record.byte_length => FragmentState::Range {
            start: range.start,
            end: range.end,
        },
        Some(None) => FragmentState::Ambiguous,
        _ => FragmentState::Missing,
    }
}

fn clone_cached(cached: &CachedBlock) -> CachedBlock {
    CachedBlock {
        input: cached.input.clone(),
        resolvable_links: cached.resolvable_links.clone(),
        fixed_unresolved_links: cached.fixed_unresolved_links.clone(),
        references: cached.references.clone(),
        candidate_results: cached.candidate_results.clone(),
    }
}

fn assemble_links(
    raw: &[DocumentationLink],
    blocks: &BTreeMap<DocumentationDigest, CachedBlock>,
    resolvable: bool,
) -> Result<Vec<DocumentationLink>, DocumentationError> {
    let mut offsets = BTreeMap::<DocumentationDigest, usize>::new();
    raw.iter()
        .map(|link| {
            let offset = offsets.entry(link.block.clone()).or_default();
            let cached = blocks
                .get(&link.block)
                .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "link.block"))?;
            let resolved = if resolvable {
                &cached.resolvable_links
            } else {
                &cached.fixed_unresolved_links
            };
            let result = resolved
                .get(*offset)
                .cloned()
                .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "link.cache"))?;
            *offset += 1;
            Ok(result)
        })
        .collect()
}

fn assemble_candidates(
    candidates: &[DocumentationReferenceCandidate],
    blocks: &BTreeMap<DocumentationDigest, CachedBlock>,
) -> Result<Vec<DocumentationReferenceCandidate>, DocumentationError> {
    let mut offsets = BTreeMap::<DocumentationDigest, usize>::new();
    let mut unresolved = Vec::new();
    for candidate in candidates {
        let block = blocks
            .get(&candidate.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "candidate.block"))?;
        let offset = offsets.entry(candidate.block.clone()).or_default();
        let result = block
            .candidate_results
            .get(*offset)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "candidate.cache"))?;
        *offset += 1;
        if let Some(candidate) = result {
            unresolved.push(candidate.clone());
        }
    }
    Ok(unresolved)
}

fn reference_order(
    left: &DocumentationReference,
    right: &DocumentationReference,
) -> std::cmp::Ordering {
    (
        &left.evidence,
        &left.block,
        left.range.start,
        &left.identity,
    )
        .cmp(&(
            &right.evidence,
            &right.block,
            right.range.start,
            &right.identity,
        ))
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationBlock, DocumentationBlockKind, DocumentationChunk,
        DocumentationContentIdentity, DocumentationLink, DocumentationLinkResolution,
        DocumentationReferenceCandidate, DocumentationSelectionReason, DocumentationSource,
        DocumentationSourceFormat, DocumentationSourceIdentity, DocumentationUnresolvedReason,
    };
    use rift_protocol::read::{
        Language, ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin, TextRange,
    };

    use super::super::DocumentationInput;
    use super::*;

    fn source_record(path: &str, text: &str) -> DocumentationSource {
        let is_rst = std::path::Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rst"));
        let (format, media_type) = if is_rst {
            (DocumentationSourceFormat::RestructuredText, "text/x-rst")
        } else {
            (DocumentationSourceFormat::Markdown, "text/markdown")
        };
        DocumentationSource {
            identity: content_identity(path),
            revision: super::super::content_digest(b"revision"),
            content_digest: super::super::content_digest(text.as_bytes()),
            origin: SymbolOrigin {
                location: Some(SourceLocationKind::Project),
                package: None,
                source_kind: SourceKind::Authored,
            },
            format,
            media_type: media_type.to_owned(),
            selection: DocumentationSelectionReason::Workspace,
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        }
    }

    fn content_identity(path: &str) -> DocumentationContentIdentity {
        DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath(path.to_owned()),
            },
            cell: None,
        }
    }

    fn input<'a>(path: &str, text: &'a str) -> DocumentationInput<'a> {
        let chunks = crate::text_chunks(text, 16)
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| DocumentationChunk {
                identity: format!("{path}#{index}"),
                range: TextRange {
                    start: chunk.byte_offset(),
                    end: chunk.byte_offset() + chunk.content().len() as u64,
                },
            })
            .collect();
        DocumentationInput::new(source_record(path, text), text)
            .expect("source fixture")
            .with_chunks(chunks)
            .expect("chunk fixture")
    }

    fn source_set(inputs: Vec<DocumentationInput<'_>>) -> super::super::DocumentationSourceSet<'_> {
        super::super::DocumentationSourceSet::new(inputs).expect("source set")
    }

    fn ambiguous_declarations<'a>(
        owner: &DocumentationContentIdentity,
        rust_symbol: &'a SymbolId,
        python_symbol: &'a SymbolId,
        rust: &'a Language,
        python: &'a Language,
    ) -> [DocumentationDeclaration<'a>; 2] {
        [
            DocumentationDeclaration::new(
                rust_symbol,
                rust,
                "Compass",
                "Compass",
                owner,
                TextRange { start: 0, end: 20 },
            )
            .expect("Rust declaration"),
            DocumentationDeclaration::new(
                python_symbol,
                python,
                "Compass",
                "Compass",
                owner,
                TextRange { start: 0, end: 20 },
            )
            .expect("Python declaration"),
        ]
    }

    fn target_sources(
        include_target: bool,
        include_target_link: bool,
    ) -> super::super::DocumentationSourceSet<'static> {
        let guide = if include_target_link {
            "[target](target.md)\n\n[other](missing.md)\n"
        } else {
            "[other](missing.md)\n"
        };
        let mut inputs = vec![
            input("guide.md", guide),
            input("other.md", "[third](third.md)\n"),
        ];
        if include_target {
            inputs.push(input("target.md", "# Target\n"));
        }
        source_set(inputs)
    }

    fn fragment_sources(target: &'static str) -> super::super::DocumentationSourceSet<'static> {
        source_set(vec![
            input(
                "guide.md",
                "[target](target.rst#anchor)\n\n[missing](absent.md)\n",
            ),
            input("target.rst", target),
        ])
    }

    fn assert_parity(
        incremental: &super::super::DocumentationCollection,
        full: &super::super::DocumentationCollection,
    ) {
        assert_eq!(incremental.index(), full.index());
    }

    fn assert_recomputed(collection: &super::super::DocumentationCollection, expected: usize) {
        assert_eq!(
            collection
                .resolution_cache()
                .expect("resolution cache")
                .recomputed_blocks(),
            expected
        );
    }

    #[test]
    fn target_add_remove_reuses_unrelated_blocks_and_retires_removed_keys() {
        let initial_sources = target_sources(false, true);
        let initial =
            super::super::collect_documentation(&initial_sources, &[]).expect("initial collection");
        let added_sources = target_sources(true, true);
        let added =
            super::super::collect_documentation_incremental(Some(&initial), &added_sources, &[])
                .expect("target addition");
        let added_full =
            super::super::collect_documentation(&added_sources, &[]).expect("full addition");
        assert_parity(&added, &added_full);
        assert_recomputed(&added, 1);

        let removed_sources = target_sources(false, true);
        let removed =
            super::super::collect_documentation_incremental(Some(&added), &removed_sources, &[])
                .expect("target removal");
        let removed_full =
            super::super::collect_documentation(&removed_sources, &[]).expect("full removal");
        assert_parity(&removed, &removed_full);
        assert_recomputed(&removed, 1);
        assert!(removed.index().links.iter().any(|link| {
            link.authored == "target.md"
                && link.resolution
                    == DocumentationLinkResolution::Unresolved {
                        reason: DocumentationUnresolvedReason::Missing,
                    }
        }));

        let removed_link_sources = target_sources(false, false);
        let removed_link = super::super::collect_documentation_incremental(
            Some(&removed),
            &removed_link_sources,
            &[],
        )
        .expect("target link removal");
        let removed_full = super::super::collect_documentation(&removed_link_sources, &[])
            .expect("full target link removal");
        assert_parity(&removed_link, &removed_full);
        let target = content_identity("target.md");
        let cache = removed_link.resolution_cache().expect("next cache");
        assert!(!cache.states.keys().any(|key| matches!(
            key,
            DependencyKey::Target { source, .. } if source == &target
        )));
    }

    #[test]
    fn empty_fragment_tracks_whole_source_addition() {
        let guide = input("guide.md", "[target](target.md#)\n");
        let absent_sources = source_set(vec![guide.clone()]);
        let absent =
            super::super::collect_documentation(&absent_sources, &[]).expect("missing target");
        assert!(matches!(
            absent.index().links[0].resolution,
            DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            }
        ));

        let present_sources = source_set(vec![guide, input("target.md", "Target content.\n")]);
        let incremental =
            super::super::collect_documentation_incremental(Some(&absent), &present_sources, &[])
                .expect("whole-source target addition");
        let full = super::super::collect_documentation(&present_sources, &[])
            .expect("full whole-source target");
        assert_parity(&incremental, &full);
        assert_recomputed(&incremental, 1);
        assert!(matches!(
            &incremental.index().links[0].resolution,
            DocumentationLinkResolution::Resolved {
                target: rift_protocol::documentation::DocumentationTarget::Source {
                    range,
                    ..
                }
            } if range.start == 0 && range.end == "Target content.\n".len() as u64
        ));
    }

    #[test]
    fn ambiguous_same_source_declarations_invalidate_qualified_link() {
        let sources = source_set(vec![
            input("guide.md", "[target](lib.md#Compass)\n"),
            input("lib.md", "pub struct Compass;\n"),
        ]);
        let owner = content_identity("lib.md");
        let rust = Language::from_identity_segment("rust").expect("Rust language");
        let python = Language::from_identity_segment("python").expect("Python language");
        let rust_symbol = SymbolId(rift_core::symbol_identity("rust", "lib.md", "Compass"));
        let python_symbol = SymbolId(rift_core::symbol_identity("python", "lib.md", "Compass"));
        let declarations =
            ambiguous_declarations(&owner, &rust_symbol, &python_symbol, &rust, &python);
        let initial = super::super::collect_documentation(&sources, &declarations[..1])
            .expect("one declaration");
        let updated = super::super::collect_documentation_incremental(
            Some(&initial),
            &sources,
            &declarations,
        )
        .expect("ambiguous declarations");
        let full = super::super::collect_documentation(&sources, &declarations)
            .expect("full ambiguous declarations");
        assert_parity(&updated, &full);
        assert_recomputed(&updated, 1);
        assert!(matches!(
            updated.index().links[0].resolution,
            DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Ambiguous,
            }
        ));
    }

    #[test]
    fn fragment_range_and_duplicate_changes_recompute_link_block_only() {
        let previous_sources = fragment_sources(".. _anchor:\n\nFirst target.\n");
        let previous =
            super::super::collect_documentation(&previous_sources, &[]).expect("unique target");
        let moved_sources = fragment_sources("Introduction.\n\n.. _anchor:\n\nFirst target.\n");
        let moved =
            super::super::collect_documentation_incremental(Some(&previous), &moved_sources, &[])
                .expect("fragment range change");
        let moved_full =
            super::super::collect_documentation(&moved_sources, &[]).expect("full range change");
        assert_parity(&moved, &moved_full);
        assert_recomputed(&moved, 1);

        let duplicate_sources =
            fragment_sources(".. _anchor:\n\nFirst target.\n\n.. _anchor:\n\nSecond target.\n");
        let next =
            super::super::collect_documentation_incremental(Some(&moved), &duplicate_sources, &[])
                .expect("duplicate fragment");
        let full = super::super::collect_documentation(&duplicate_sources, &[])
            .expect("full duplicate fragment");
        assert_parity(&next, &full);
        assert_recomputed(&next, 1);
        assert!(next.index().links.iter().any(|link| {
            link.authored == "target.rst#anchor"
                && link.resolution
                    == DocumentationLinkResolution::Unresolved {
                        reason: DocumentationUnresolvedReason::Ambiguous,
                    }
        }));
    }

    struct CandidateCase {
        source: DocumentationSource,
        block: DocumentationBlock,
        candidates: [DocumentationReferenceCandidate; 6],
        rust_source: DocumentationContentIdentity,
        python_source: DocumentationContentIdentity,
        rust_symbol: SymbolId,
        python_symbol: SymbolId,
        rust: Language,
        python: Language,
    }

    fn candidate_case() -> CandidateCase {
        let source_text = "open missing open open open Client::open";
        let source = source_record("guide.md", source_text);
        let block_id = super::super::content_digest(b"guide:refs:0");
        let block = DocumentationBlock {
            identity: block_id.clone(),
            source: source.identity.clone(),
            content_digest: super::super::content_digest(source_text.as_bytes()),
            heading_path: Vec::new(),
            range: TextRange {
                start: 0,
                end: source_text.len() as u64,
            },
            line: 1,
            kind: DocumentationBlockKind::Prose,
            language: None,
            chunks: Vec::new(),
            symbol: None,
        };
        let rust_source = content_identity("src/rust.rs");
        let python_source = content_identity("src/python.py");
        let rust = Language::from_identity_segment("rust").expect("Rust language");
        let python = Language::from_identity_segment("python").expect("Python language");
        let rust_symbol = SymbolId(rift_core::symbol_identity(
            "rust",
            "src/rust.rs",
            "Client::open",
        ));
        let python_symbol = SymbolId(rift_core::symbol_identity(
            "python",
            "src/python.py",
            "Client::open",
        ));
        let candidates = [
            candidate(&block_id, "open", 0, None),
            candidate(&block_id, "missing", 5, None),
            candidate(&block_id, "open", 13, Some(rust.clone())),
            candidate(&block_id, "open", 18, Some(python.clone())),
            candidate(&block_id, "open", 23, Some(rust.clone())),
            candidate(&block_id, "Client::open", 28, Some(rust.clone())),
        ];
        CandidateCase {
            source,
            block,
            candidates,
            rust_source,
            python_source,
            rust_symbol,
            python_symbol,
            rust,
            python,
        }
    }

    fn candidate_declarations(
        case: &CandidateCase,
        include_python: bool,
    ) -> Vec<DocumentationDeclaration<'_>> {
        let mut declarations = vec![
            DocumentationDeclaration::new(
                &case.rust_symbol,
                &case.rust,
                "open",
                "Client::open",
                &case.rust_source,
                TextRange { start: 0, end: 5 },
            )
            .expect("Rust declaration"),
        ];
        if include_python {
            declarations.push(
                DocumentationDeclaration::new(
                    &case.python_symbol,
                    &case.python,
                    "open",
                    "Client::open",
                    &case.python_source,
                    TextRange { start: 0, end: 5 },
                )
                .expect("Python declaration"),
            );
        }
        declarations
    }

    fn resolve_candidates(
        previous: Option<&ResolutionCache>,
        case: &CandidateCase,
        declarations: &[DocumentationDeclaration<'_>],
    ) -> ResolutionOutput {
        ResolutionCache::resolve(
            previous,
            &ResolutionInput {
                sources: std::slice::from_ref(&case.source),
                blocks: std::slice::from_ref(&case.block),
                resolvable_links: &[],
                fixed_unresolved_links: &[],
                fragments: &[],
                candidates: &case.candidates,
                declarations,
            },
        )
        .expect("candidate resolution")
    }

    #[test]
    fn candidate_alignment_matches_baseline_and_preserves_occurrence_ids() {
        let case = candidate_case();
        let declarations = candidate_declarations(&case, false);
        let output = resolve_candidates(None, &case, &declarations);
        let expected = super::super::resolve_references(&declarations, &case.candidates)
            .expect("reference baseline");
        assert_eq!(output.references, expected.references());
        assert_eq!(output.unresolved_references, expected.unresolved());
        assert_eq!(output.unresolved_references.len(), 2);
        assert_eq!(output.unresolved_references[0].authored, "missing");
        assert_eq!(output.unresolved_references[1].authored, "open");
        assert_eq!(
            output.unresolved_references[1].reason,
            DocumentationUnresolvedReason::Missing
        );
        let repeated = output
            .references
            .iter()
            .filter(|reference| matches!(reference.range.start, 13 | 23))
            .map(|reference| &reference.identity)
            .collect::<Vec<_>>();
        assert_eq!(repeated.len(), 2);
        assert_ne!(repeated[0], repeated[1]);
    }

    #[test]
    fn candidate_cache_tracks_language_and_ambiguity_changes() {
        let case = candidate_case();
        let rust_only = candidate_declarations(&case, false);
        let cold = resolve_candidates(None, &case, &rust_only);
        let warm = resolve_candidates(cold.next_cache.as_ref(), &case, &rust_only);
        assert_eq!(warm.references, cold.references);
        assert_eq!(warm.unresolved_references, cold.unresolved_references);
        assert_eq!(
            warm.next_cache
                .as_ref()
                .expect("warm cache")
                .recomputed_blocks(),
            0
        );

        let both_languages = candidate_declarations(&case, true);
        let changed = resolve_candidates(warm.next_cache.as_ref(), &case, &both_languages);
        let expected = super::super::resolve_references(&both_languages, &case.candidates)
            .expect("updated reference baseline");
        assert_eq!(changed.references, expected.references());
        assert_eq!(changed.unresolved_references, expected.unresolved());
        assert_eq!(changed.unresolved_references[0].authored, "open");
        assert_eq!(
            changed.unresolved_references[0].reason,
            DocumentationUnresolvedReason::Ambiguous
        );
        assert_eq!(
            changed
                .next_cache
                .as_ref()
                .expect("updated cache")
                .recomputed_blocks(),
            1
        );
    }

    #[test]
    fn fixed_unresolved_reference_label_does_not_become_source_link() {
        let source = source_record("guide.rst", "missing_\n");
        let target = source_record("missing.md", "target content");
        let block_id = super::super::content_digest(b"guide:missing:0");
        let block = DocumentationBlock {
            identity: block_id.clone(),
            source: source.identity.clone(),
            content_digest: super::super::content_digest(b"missing_"),
            heading_path: Vec::new(),
            range: TextRange { start: 0, end: 8 },
            line: 1,
            kind: DocumentationBlockKind::Prose,
            language: None,
            chunks: Vec::new(),
            symbol: None,
        };
        let link = DocumentationLink {
            block: block_id,
            authored: "missing.md".to_owned(),
            range: TextRange { start: 0, end: 8 },
            resolution: DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            },
        };
        let output = ResolutionCache::resolve(
            None,
            &ResolutionInput {
                sources: &[source, target],
                blocks: std::slice::from_ref(&block),
                resolvable_links: &[],
                fixed_unresolved_links: std::slice::from_ref(&link),
                fragments: &[],
                candidates: &[],
                declarations: &[],
            },
        )
        .expect("fixed unresolved link");
        assert_eq!(output.fixed_unresolved_links, [link]);
        assert!(matches!(
            output.fixed_unresolved_links[0].resolution,
            DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            }
        ));
    }

    fn candidate(
        block: &DocumentationDigest,
        authored: &str,
        start: u64,
        language: Option<Language>,
    ) -> DocumentationReferenceCandidate {
        DocumentationReferenceCandidate {
            block: block.clone(),
            range: TextRange {
                start,
                end: start + authored.len() as u64,
            },
            authored: authored.to_owned(),
            language,
            reason: DocumentationUnresolvedReason::Missing,
        }
    }
}
