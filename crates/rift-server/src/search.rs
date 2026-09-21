//! `search` tool execution: lexical symbol and content-line search over syntax-indexed and
//! `[search.text]` files alike, narrowed or extended by a request's `paths` selector. Extracted
//! from `read` so that module stays below its size bound. The bounded relationship `traversal`
//! lane lives in the sibling `traversal` module.

use std::cmp::Ordering;
use std::path::Path;

use rift_core::constants::{
    FORCE_INCLUDE_FILES_MAX, SEARCH_RESULTS_DEFAULT, SOURCE_UNIT_URI_PREFIX, SYMBOL_URI_PREFIX,
};
use rift_core::line;
use rift_core::{ProjectPath, SourceUnitId};
use rift_index::{
    DependencyIndex, DependencySymbolMatch, IndexedFile, LexicalChange, PathChanges, PathMatcher,
    SymbolMatch, TextSourceFile, WorkspaceIndex,
};
use rift_protocol::read::{
    CHANGE_BASE_FIELD, CHANGE_HEAD_FIELD, MatchedField, PathPattern, PathSelector,
    ProjectPath as WireProjectPath, ReadWarning, ResultOrder, SearchChange, SearchHit,
    SearchHitTarget, SearchInclude, SearchParams, SearchParamsTarget, SearchResult, SearchScope,
    SearchTraversal, SourceUnitId as WireSourceUnitId, Symbol, SymbolId,
};
use rift_ranking::{
    DocumentIdentity, DocumentKind, DocumentLocation, FusedCandidate, IdentifierMatchClass,
    IdentifierRanking, IndexDocument, PARSED_QUERY_MEMBERS_MAX, ParsedQuery, QueryPhase,
    RankRequest, RankedCandidates, RankingInput, RankingInputKind, RankingWeights, SearchableField,
    fuse,
};
use rift_search::{Declaration, DescribedUnit};
use rift_syntax::{ByteRange, SyntaxSymbol};

use crate::engine_read::EngineReferences;
use crate::packages::PackageFallback;
use crate::read::parse_symbol_address;
use crate::read::{
    ReadError, ReadFault, ReadService, accepted_limit, dependency_symbol, excerpt,
    package_warnings, page, project_path, results_truncation_warning, source_warnings, text_range,
    validate_common, wire_symbol,
};
use crate::traversal::{
    TraversalReport, collect_traversal_hits, traversal_truncation_warning, validate_traversal,
};

/// What the search store answered for one request, and the shares the three ranking
/// inputs fuse under.
///
/// Both phases arrive together because the reader that runs them sits above this layer
/// and cannot know, before fusion, whether the precise answer filled the pool. Running
/// the broad phase costs one more full-text query; letting its results appear when the
/// precise phase already filled the pool would cost correctness, so the decision stays
/// here and the extra query is the price.
#[derive(Clone, Debug)]
pub struct StoreAnswer {
    precise: Vec<RankingInput>,
    broad: Vec<RankingInput>,
    weights: RankingWeights,
}

impl StoreAnswer {
    /// Names what the store answered for each phase, and the configured shares.
    #[must_use]
    pub const fn new(
        precise: Vec<RankingInput>,
        broad: Vec<RankingInput>,
        weights: RankingWeights,
    ) -> Self {
        Self {
            precise,
            broad,
            weights,
        }
    }

    /// No store answer, under the operator's own shares.
    ///
    /// The project's full-text store contributed nothing, but a selected package index
    /// answers that input from its own documents, so the shares stay what
    /// `[search.ranking]` states rather than collapsing onto identifier matching.
    #[must_use]
    pub const fn without_store(weights: RankingWeights) -> Self {
        Self {
            precise: Vec::new(),
            broad: Vec::new(),
            weights,
        }
    }

    /// No store answer and no other index that could give one: the identifier ranking
    /// is the only input there is, so it carries the whole share whatever the operator
    /// configured.
    #[must_use]
    pub fn identifier_only() -> Self {
        Self {
            precise: Vec::new(),
            broad: Vec::new(),
            weights: RankingWeights::identifier_only(),
        }
    }

    /// The inputs the precise phase produced.
    #[must_use]
    pub fn precise(&self) -> &[RankingInput] {
        &self.precise
    }

    /// The inputs the broad phase produced, empty when the query has no broad phase.
    #[must_use]
    pub fn broad(&self) -> &[RankingInput] {
        &self.broad
    }

    /// The shares the three inputs fuse under.
    #[must_use]
    pub const fn weights(&self) -> RankingWeights {
        self.weights
    }
}

impl Default for StoreAnswer {
    fn default() -> Self {
        Self::identifier_only()
    }
}

/// Parses one caller query through the shared bounded parser.
///
/// Every reader parses the same text the same way, so the terms the full-text ranking
/// matches, the identifiers the identifier ranking extracts, and the excerpt a hit points
/// at all come from one value.
fn parsed_query(query: &str) -> Result<ParsedQuery, ReadError> {
    ParsedQuery::parse(query).map_err(|error| ReadFault::invalid("query", error.detail()))
}

/// The warning a query the parser narrowed carries: the terms the bound dropped matched
/// nothing, so the caller shortens the question rather than reading the answer as though
/// every term had been asked for.
fn query_narrowing_warning() -> ReadWarning {
    ReadWarning::QueryNarrowed {
        terms_max: u64::try_from(PARSED_QUERY_MEMBERS_MAX).unwrap_or(u64::MAX),
    }
}

impl ReadService {
    /// Searches one publication, fusing the caller's identifiers with what the search
    /// store answered.
    ///
    /// `store` carries the full-text and vector inputs for both query phases; this method
    /// builds the identifier input over every selected index, fuses the three under one
    /// set of shares, and resolves the ordered identities into hits. A store that is
    /// unavailable, or whose stamped revision no longer matches what is published,
    /// contributes no input and the identifier ranking answers alone. `params.scope`
    /// selects which indexes take part: the project's own, the attached package indexes,
    /// or both, ordered together by `params.order`.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for an invalid `paths` glob, a `force_include` bound crossed,
    /// a scope beyond `local` beside a revision, `global` beside `traversal`, a query the
    /// bounded parser refuses, and a poisoned package branch.
    pub fn search(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
    ) -> Result<SearchResult, ReadError> {
        self.search_with_references(params, store, &EngineReferences::default())
    }

    /// Searches one publication with references resolved by its configured engines.
    ///
    /// # Errors
    ///
    /// Returns the search refusal, or rejects references from a different source revision.
    pub fn search_with_references(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
        references: &EngineReferences,
    ) -> Result<SearchResult, ReadError> {
        references.validate_revision(self)?;
        validate_search(params)?;
        if params.change.is_some() {
            // One snapshot holds one tree; a comparison needs two, and reaches its own
            // through `search_change`.
            return Err(ReadFault::unsupported(
                "a revision comparison on a captured snapshot",
            ));
        }
        self.validate_dependency_scope(params.scope, params.rev.as_ref())?;
        if self.revision().is_some() && force_include_requested(params) {
            return Err(ReadFault::unsupported("force_include at a revision"));
        }
        let query = accepted_query(params)?;
        let limit = accepted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
        let selected = self.selected_paths(params.paths.as_ref())?;
        let payloads = HitPayloads::requested(params);
        // The whole candidate pool is collected up to the index's own `results_max` bound -
        // bounded work whatever the page size - then ordered and truncated to that same
        // bound, so `pagination.total_pages` counts the full result set and every page is
        // one window of the same ordering.
        let fetch_limit = self.index().results_max();
        let fallback = self.fill_packages(params.scope)?;
        let dependencies = self.dependency_index(params.scope)?;

        let mut results = Vec::new();
        let mut warnings = self.warnings();
        warnings.extend(selected.warnings());
        warnings.extend(references.analysis_unavailable().cloned());
        let mut ranked_truncated_at = None;
        if let Some(query) = query {
            let parsed = parsed_query(query)?;
            if parsed.is_narrowed() {
                warnings.push(query_narrowing_warning());
            }
            let criteria = SearchCriteria {
                query: &parsed,
                target: params.target,
                payloads,
            };
            ranked_truncated_at = self.collect_query_hits(
                criteria,
                params.scope,
                &selected,
                (&parsed, store),
                dependencies.as_deref(),
                &mut results,
            )?;
        }
        let mut traversal_report = TraversalReport::default();
        // `validate_search` refuses a `traversal` naming no `seed` unless `change` names
        // the walk's starting declarations, and this path serves no comparison.
        if let Some(traversal) = params.traversal.as_ref()
            && let Some(seed) = traversal.seed.as_ref()
            && matches!(
                params.target,
                SearchParamsTarget::All | SearchParamsTarget::Symbol
            )
        {
            traversal_report = collect_traversal_hits(
                self,
                selected.matcher.as_ref(),
                self.index().root(),
                (traversal, seed),
                references,
                payloads,
                &mut results,
            )?;
        }
        // Either bound reaching `results_max` costs the caller the same hits, so the two
        // carry one warning: the candidate pool cut before resolution, and the hit set
        // cut after it.
        let results_max_reached =
            order_and_bound_hits(&mut results, params.order, fetch_limit).or(ranked_truncated_at);
        let (mut results, pagination) = page(results, params.page_index, limit);
        populate_symbol_lines(&mut results, self.index(), selected.force_include.as_ref())?;
        if !payloads.score {
            for hit in &mut results {
                hit.score = None;
            }
        }
        Ok(SearchResult {
            results,
            pagination,
            warnings: self.search_warnings(
                warnings,
                params.scope,
                (dependencies.as_deref(), fallback),
                traversal_report,
                results_max_reached,
            ),
        })
    }

    pub(crate) fn validate_engine_search(&self, params: &SearchParams) -> Result<(), ReadError> {
        validate_search(params)?;
        self.validate_dependency_scope(params.scope, params.rev.as_ref())?;
        accepted_query(params)?;
        accepted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
        path_matcher(self.index().root(), params.paths.as_ref())?;
        if let Some(selector) = params.paths.as_ref() {
            PathMatcher::build(
                self.index().root(),
                &pattern_strings(&selector.force_include),
                &[],
            )
            .map_err(ReadFault::index)?;
        }
        Ok(())
    }

    /// The warnings one search answer carries: those the collection gathered - the
    /// snapshot's own and the force-included files it left out - then what the traversal
    /// lane reported, the result bound when the pool reached it, and the package warnings
    /// when `scope` reaches packages.
    fn search_warnings(
        &self,
        mut warnings: Vec<ReadWarning>,
        scope: SearchScope,
        (dependencies, fallback): (Option<&DependencyIndex>, PackageFallback),
        traversal: TraversalReport,
        results_max_reached: Option<usize>,
    ) -> Vec<ReadWarning> {
        warnings.extend(traversal.coverage_missing);
        if traversal.truncated {
            warnings.push(traversal_truncation_warning());
        }
        if let Some(results_max) = results_max_reached {
            warnings.push(results_truncation_warning(results_max));
        }
        if scope != SearchScope::Local {
            warnings.extend(package_warnings(
                dependencies,
                self.dependency_context(),
                fallback,
            ));
        }
        warnings
    }

    /// Compiles `selector` into the files the request reaches: no matcher when neither
    /// `include` nor `exclude` is set, no force-include index when `force_include` is
    /// empty.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for an invalid glob, or a `force_include` matching more
    /// files than `FORCE_INCLUDE_FILES_MAX`.
    fn selected_paths(&self, selector: Option<&PathSelector>) -> Result<SelectedPaths, ReadError> {
        let index = self.index();
        let matcher = path_matcher(index.root(), selector)?;
        let force_include = match selector {
            Some(selector) if !selector.force_include.is_empty() => Some(
                index
                    .force_include_index(
                        &pattern_strings(&selector.force_include),
                        FORCE_INCLUDE_FILES_MAX,
                    )
                    .map_err(ReadFault::index)?,
            ),
            _ => None,
        };
        Ok(SelectedPaths {
            matcher,
            force_include,
        })
    }

    /// Fuses the three ranking inputs for one `query` and resolves what they ordered,
    /// answering the candidate bound when fusion stopped at it.
    ///
    /// The precise phase runs first, over the identifier ranking this method builds and
    /// whatever the store answered. The broad phase joins it only when the precise
    /// answer left the candidate pool short, and its identities append after the precise
    /// ones, so one index's widened hit can never displace another index's exact hit.
    ///
    /// Every input is screened by the request's own `paths` and `target` filters before
    /// it is fused, so a candidate the request cannot answer never occupies a slot in the
    /// bounded pool.
    fn collect_query_hits(
        &self,
        criteria: SearchCriteria<'_>,
        scope: SearchScope,
        selected: &SelectedPaths,
        (query, store): (&ParsedQuery, &StoreAnswer),
        dependencies: Option<&DependencyIndex>,
        results: &mut Vec<SearchHit>,
    ) -> Result<Option<usize>, ReadError> {
        let index = self.index();
        let root = index.root();
        let matcher = selected.matcher.as_ref();
        let fetch_limit = index.results_max();
        let sources = IdentifierSources {
            project: scope != SearchScope::Global,
            force_include: (scope != SearchScope::Global)
                .then_some(selected.force_include.as_ref())
                .flatten(),
            packages: dependencies,
        };
        let resolution = Resolution {
            force_include: sources.force_include,
            packages: dependencies,
        };
        let screen = CandidateScreen {
            index,
            matcher,
            root,
            target: criteria.target,
            resolution,
        };
        let mut inputs = vec![identifier_input(
            index,
            matcher,
            root,
            query,
            sources,
            fetch_limit,
        )?];
        inputs.extend(store.precise().iter().cloned());
        inputs.extend(package_inputs(
            dependencies,
            query,
            QueryPhase::Precise,
            fetch_limit,
        ));
        let mut ranked = fuse(
            &screen.screened(&inputs),
            store.weights(),
            QueryPhase::Precise,
            fetch_limit,
        );
        // The widened inputs are built only when the precise phase came up short,
        // because ranking every package's documents again is work the full pool
        // would throw away.
        if ranked.len() < fetch_limit {
            let mut widened: Vec<RankingInput> = store.broad().to_vec();
            widened.extend(package_inputs(
                dependencies,
                query,
                QueryPhase::Broad,
                fetch_limit,
            ));
            let broad = fuse(
                &screen.screened(&widened),
                store.weights(),
                QueryPhase::Broad,
                fetch_limit,
            );
            ranked.append_phase(broad, fetch_limit);
        }
        resolve_ranked_hits(index, criteria, resolution, &ranked, results)?;
        Ok(ranked.truncated_at())
    }

    /// Derives the lexical write one change set owes: the paths whose stored units go, and
    /// the units this snapshot derived for the paths it read.
    ///
    /// Every named path is replaced, so a path this snapshot read appears in both halves
    /// and one it found gone appears only in the first. Replacing rather than adding is
    /// what lets the same change set be written twice: two rebuilds captured from one
    /// publication both write what they read, and the second leaves what the first left.
    #[must_use]
    pub fn lexical_change(&self, changes: &PathChanges) -> LexicalChange {
        LexicalChange::new(
            changes.paths().cloned().collect(),
            self.index().index_documents_for(changes.indexed()),
        )
    }

    /// Pairs each symbol unit in `units` with the declaration the vector ranking embeds for
    /// it.
    ///
    /// Only a symbol document carries a declaration: a text file's chunk describes none, so
    /// it has no entry and the two slices are never parallel. Each pair is built from one
    /// document's own resolution, so a document can never pick up another's declaration.
    ///
    /// The declaration's signature, its attached documentation, and its own source all
    /// travel with the pair, because the published document already holds each of them in
    /// its own field and the embedding text reads all three.
    ///
    /// The walk runs over `documents`, whose length this snapshot's own file and symbol
    /// bounds already fixed, and each symbol document costs one scan of the file it names,
    /// which is how `resolve_symbol` narrows the lookup.
    #[must_use]
    pub fn described_units<'a>(&'a self, documents: &'a [IndexDocument]) -> Vec<DescribedUnit<'a>> {
        documents
            .iter()
            .filter(|document| document.kind() == DocumentKind::Symbol)
            .filter_map(|document| self.described_unit(document))
            .collect()
    }

    /// How many visible files this snapshot indexes across syntax and baseline text.
    ///
    /// A caller estimates the vector ranking's preparation work from this count.
    #[must_use]
    pub fn file_count(&self) -> u64 {
        let files = self.index().files().len() + self.index().text_files().len();
        u64::try_from(files).unwrap_or(u64::MAX)
    }

    /// One symbol unit paired with its declaration, or nothing when this snapshot no longer
    /// holds the symbol the unit names.
    fn described_unit<'a>(&'a self, unit: &'a IndexDocument) -> Option<DescribedUnit<'a>> {
        let DocumentLocation::Project(path) = unit.location() else {
            return None;
        };
        let (_, symbol) = resolve_symbol(self.index(), path, unit.identity().as_str())?;
        let fields = unit.fields();
        let declaration = Declaration::new(symbol.kind, &symbol.qualified_name)
            .signature(fields.get(SearchableField::Signature).unwrap_or_default())
            .documentation(
                fields
                    .get(SearchableField::Documentation)
                    .unwrap_or_default(),
            )
            .source(
                fields
                    .get(SearchableField::DeclarationSource)
                    .unwrap_or_default(),
            );
        Some(DescribedUnit::new(unit, declaration))
    }
}

/// Whether the request's `paths` selector names any `force_include` glob.
/// Reaching index-excluded files is a walk of the working tree, which a
/// revision-addressed search has no tree to run against.
fn force_include_requested(params: &SearchParams) -> bool {
    params
        .paths
        .as_ref()
        .is_some_and(|selector| !selector.force_include.is_empty())
}

/// Which extra payload `params.include` asked to attach to every hit, derived once per
/// request: `source` attaches the excerpt, `score` attaches the fused ranking value.
/// Project symbol lines wait for pagination when this is a search request.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HitPayloads {
    source: bool,
    score: bool,
    defer_line: bool,
}

impl HitPayloads {
    /// The payloads one comparison attaches. Its hits come from the compared revisions'
    /// own indexes rather than the published one, so the line is resolved where the hit is
    /// built instead of being deferred to the page.
    pub(crate) fn for_change(params: &SearchParams) -> Self {
        Self {
            defer_line: false,
            ..Self::requested(params)
        }
    }

    fn requested(params: &SearchParams) -> Self {
        let include = params.include.as_deref().unwrap_or_default();
        Self {
            source: include.contains(&SearchInclude::Source),
            score: include.contains(&SearchInclude::Score),
            defer_line: true,
        }
    }
}

/// Query term, kind selector, and requested payloads shared by every hit-collection pass
/// one `search` call runs: identifier matching, `force_include`, and the merge of ranked
/// units.
#[derive(Clone, Copy, Debug)]
struct SearchCriteria<'a> {
    query: &'a ParsedQuery,
    target: SearchParamsTarget,
    payloads: HitPayloads,
}

/// The files one request's `paths` selector reaches, compiled before any hit is
/// collected: the `include`/`exclude` matcher screening the indexed candidates, and the
/// index over the `force_include` files the persistent index never held. Building that
/// index enforces the `force_include` file bound on the request itself, whatever the
/// query and however many hits the persistent index yields.
struct SelectedPaths {
    matcher: Option<PathMatcher>,
    force_include: Option<WorkspaceIndex>,
}

impl SelectedPaths {
    /// The `source_unavailable` warnings for the `force_include` files the on-demand index
    /// left out. They describe the request's selection, so the answer carries them whatever
    /// the query yields and whether or not the pool had room for the selected files.
    fn warnings(&self) -> Vec<ReadWarning> {
        self.force_include
            .as_ref()
            .map(|extra| source_warnings(extra.warnings()))
            .unwrap_or_default()
    }
}

pub(crate) fn validate_search(params: &SearchParams) -> Result<(), ReadError> {
    validate_common(params.rev.is_some())?;
    if let Some(selector) = params.paths.as_ref() {
        validate_path_selector(selector)?;
    }
    if let Some(change) = params.change.as_ref() {
        validate_change(change, params)?;
    }
    if let Some(traversal) = params.traversal.as_ref() {
        validate_traversal(traversal)?;
        validate_traversal_seed(params, traversal)?;
        if params.rev.is_some() {
            return Err(ReadFault::unsupported("traversal at a revision"));
        }
        // The `all` scope still walks the project graph beside the package
        // declarations; `global` alone leaves the walk nothing to run over.
        if params.scope == SearchScope::Global {
            return Err(ReadFault::invalid(
                "traversal",
                "the relationship graph serves the project alone",
            ));
        }
    }
    Ok(())
}

/// The capability a walk beside a comparison names.
///
/// The language engine lane resolves the references a walk follows, and an engine session
/// serves the current tree; a comparison names two committed revisions instead. No resend
/// of the same request clears that.
pub(crate) const CHANGE_TRAVERSAL_CAPABILITY: &str = "relationship traversal beside a comparison";

/// Refuses a `traversal` that names no `seed`, and one riding beside `change`.
///
/// A walk has one starting declaration, and `seed` names it. `schemars`' cross-field rules
/// are advisory only, so this mirrors the rule `require_traversal_seed` advertises.
fn validate_traversal_seed(
    params: &SearchParams,
    traversal: &SearchTraversal,
) -> Result<(), ReadError> {
    if params.change.is_some() {
        return Err(ReadFault::unsupported(CHANGE_TRAVERSAL_CAPABILITY));
    }
    if traversal.seed.is_none() {
        return Err(ReadFault::invalid("seed", "a traversal starts at seed"));
    }
    Ok(())
}

/// Refuses a `change` beside a field that selects another result set or another tree, and
/// a revision spelling that breaks the charset [`RevisionId`] advertises.
///
/// The `change` block names both sides of the comparison itself, so `rev` has nothing left
/// to address, and `query` selects a result set of its own. A `scope` past `local`
/// follows the rule every revision-addressed read applies, since the package branch
/// serves the current tree alone. A `traversal` is refused beside a comparison by
/// [`validate_traversal_seed`], since no lane resolves references for a committed
/// revision.
fn validate_change(change: &SearchChange, params: &SearchParams) -> Result<(), ReadError> {
    if params.rev.is_some() {
        return Err(ReadFault::invalid(
            "change",
            "change names its own revisions",
        ));
    }
    if params.query.is_some() {
        return Err(ReadFault::invalid(
            "change",
            "query and change select different result sets",
        ));
    }
    if params.scope != SearchScope::Local {
        return Err(ReadFault::invalid(
            "scope",
            "package facts are served for the current tree alone",
        ));
    }
    let sides = [
        (CHANGE_BASE_FIELD, &change.base),
        (CHANGE_HEAD_FIELD, &change.head),
    ];
    for (field, revision) in sides {
        if let Some(violation) = revision.violation() {
            return Err(ReadFault::invalid(field, violation.as_str()));
        }
    }
    Ok(())
}

/// The lexical `query` the request carries, if any: refused when the request carries
/// none of `query`, `traversal`, and `change`, and when the query is empty.
fn accepted_query(params: &SearchParams) -> Result<Option<&str>, ReadError> {
    let query = params.query.as_deref();
    if query.is_none() && params.traversal.is_none() && params.change.is_none() {
        return Err(ReadFault::invalid("query", "missing"));
    }
    if query.is_some_and(str::is_empty) {
        return Err(ReadFault::invalid("query", "empty"));
    }
    Ok(query)
}

/// Refuses `selector` when any `include`, `exclude`, or `force_include` pattern breaks
/// [`PathPattern`]'s forward-slash-only contract, before it reaches a glob engine that would
/// otherwise read a stray backslash as an escape.
fn validate_path_selector(selector: &PathSelector) -> Result<(), ReadError> {
    let patterns = selector
        .include
        .iter()
        .chain(&selector.exclude)
        .chain(&selector.force_include);
    for pattern in patterns {
        if let Some(violation) = pattern.violation() {
            return Err(ReadFault::invalid("paths", violation.as_str()));
        }
    }
    Ok(())
}

/// Compiles `selector`'s `include`/`exclude` globs into one matcher, or none when neither list
/// is set. `force_include` is compiled separately, into [`SelectedPaths`]: it reaches files
/// the index never held, so it never narrows the indexed candidate set this matcher
/// screens.
pub(crate) fn path_matcher(
    root: &Path,
    selector: Option<&PathSelector>,
) -> Result<Option<PathMatcher>, ReadError> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    if selector.include.is_empty() && selector.exclude.is_empty() {
        return Ok(None);
    }
    PathMatcher::build(
        root,
        &pattern_strings(&selector.include),
        &pattern_strings(&selector.exclude),
    )
    .map(Some)
    .map_err(ReadFault::index)
}

fn pattern_strings(patterns: &[PathPattern]) -> Vec<String> {
    patterns.iter().map(|pattern| pattern.0.clone()).collect()
}

/// Whether `path` (project-relative) passes `matcher`, absent a matcher including every path.
pub(crate) fn includes(matcher: Option<&PathMatcher>, root: &Path, path: &ProjectPath) -> bool {
    matcher.is_none_or(|matcher| matcher.includes(&root.join(path.as_str())))
}

/// The identifiers a caller's query carried, matched against every selected index.
///
/// One input, whichever indexes answered it: the project's own declarations, the
/// `force_include` files this request pulled in, and the cataloged packages when `scope`
/// reaches them. Each extracted candidate is matched separately and an identity keeps its
/// best class, so a question naming two identifiers ranks a declaration by the stronger
/// of the two rather than by whichever was written first.
///
/// Work is bounded twice over: the query contributes at most
/// `IDENTIFIER_CANDIDATES_MAX` candidates, and each index answers at most `bound` of
/// them.
fn identifier_input(
    index: &WorkspaceIndex,
    matcher: Option<&PathMatcher>,
    root: &Path,
    query: &ParsedQuery,
    sources: IdentifierSources<'_>,
    bound: usize,
) -> Result<RankingInput, ReadError> {
    let mut ranking = IdentifierRanking::new();
    if sources.project {
        index
            .observe_identifiers(
                query,
                bound,
                |file| includes(matcher, root, file.path()),
                &mut ranking,
            )
            .map_err(ReadFault::index)?;
    }
    if let Some(extra) = sources.force_include {
        // A force-included file is reached by a glob the request named, so the
        // selector's `exclude` does not take it back.
        extra
            .observe_identifiers(query, bound, |_| true, &mut ranking)
            .map_err(ReadFault::index)?;
    }
    if let Some(packages) = sources.packages {
        for candidate in query.candidates() {
            for found in packages.symbols(candidate.text(), bound) {
                let Some(identity) = package_symbol_identity(&found) else {
                    continue;
                };
                ranking.observe(identity, found.matched.rank, &candidate);
            }
        }
    }
    Ok(ranking.into_input(bound))
}

/// The full-text ranking every held package answers, one input per package.
///
/// A package holds its documents in memory and ranks them through the shared
/// reader contract, so a package declaration reaches an answer by its
/// documentation and its source as well as by its name. Every package answers
/// the same kind, so fusion splits what full-text matching is worth among
/// them rather than letting a wide dependency outvote the project.
///
/// The work is one pass over each package's own documents, each bounded by the
/// declarations that package published, and the answer is cut to `bound`.
fn package_inputs(
    dependencies: Option<&DependencyIndex>,
    query: &ParsedQuery,
    phase: QueryPhase,
    bound: usize,
) -> Vec<RankingInput> {
    let Some(dependencies) = dependencies else {
        return Vec::new();
    };
    if phase == QueryPhase::Broad && !query.has_broad_phase() {
        return Vec::new();
    }
    dependencies
        .packages()
        .map(|package| {
            package.reader().ranked(RankRequest::new(
                query,
                RankingInputKind::Lexical,
                phase,
                bound,
            ))
        })
        .collect()
}

/// Which indexes the identifier ranking reads.
#[derive(Clone, Copy)]
struct IdentifierSources<'a> {
    project: bool,
    force_include: Option<&'a WorkspaceIndex>,
    packages: Option<&'a DependencyIndex>,
}

/// One package declaration's ranking identity, in the one spelling the package index
/// publishes its documents under.
fn package_symbol_identity(found: &DependencySymbolMatch<'_>) -> Option<DocumentIdentity> {
    let unit = found.package.unit_of(found.matched.file)?;
    DocumentIdentity::for_unit(unit, &found.matched.symbol.qualified_name).ok()
}

/// The request's own filters, applied to a ranked identity before it is fused.
///
/// A candidate the request cannot answer must not occupy a slot in the bounded candidate
/// pool. `results_max` full-text matches outside the `paths` selector would otherwise
/// fill that pool, leave the matches inside the selector unranked, and answer nothing;
/// the same holds for a pool of declarations under `target: "file"`, which also skips the
/// broad phase because the pool came back full.
///
/// The screen is the one place those filters run, so a filter cannot be applied to one
/// input and forgotten on another. Each identity costs one resolution against the
/// selected indexes, and every input the store and the identifier ranking answer with is
/// already bounded by `results_max`.
#[derive(Clone, Copy)]
struct CandidateScreen<'a> {
    index: &'a WorkspaceIndex,
    matcher: Option<&'a PathMatcher>,
    root: &'a Path,
    target: SearchParamsTarget,
    resolution: Resolution<'a>,
}

impl CandidateScreen<'_> {
    /// `inputs` with every identity this request's filters exclude removed, each input
    /// keeping the order it answered in. An input screened down to nothing no longer
    /// answers, so fusion redistributes its share across the inputs that did.
    fn screened(&self, inputs: &[RankingInput]) -> Vec<RankingInput> {
        inputs
            .iter()
            .map(|input| {
                let order = input
                    .order()
                    .iter()
                    .filter(|ranked| self.admits(ranked.identity()))
                    .cloned()
                    .collect();
                RankingInput::new(input.kind(), order)
            })
            .collect()
    }

    /// Whether this request answers `identity`. It does not when no selected index holds
    /// it, when the `paths` selector excludes the path it names, or when `target`
    /// excludes the kind it names.
    fn admits(&self, identity: &DocumentIdentity) -> bool {
        resolve_candidate(self.index, self.resolution, identity).is_some_and(|resolved| {
            resolved.reaches(self.index, self.matcher, self.root) && resolved.answers(self.target)
        })
    }
}

/// Resolves every fused identity into a hit, in the order fusion produced.
///
/// Resolution reads the identity alone: a `rift://symbol/` address names a project or
/// `force_include` declaration, a `rift://source/` address names a package declaration,
/// and anything else is a project path, optionally carrying the chunk index a large text
/// file was split at. [`CandidateScreen`] resolved each identity once already, so the
/// identities that reach here are the ones this snapshot holds and this request answers.
fn resolve_ranked_hits(
    index: &WorkspaceIndex,
    criteria: SearchCriteria<'_>,
    resolution: Resolution<'_>,
    ranked: &RankedCandidates,
    results: &mut Vec<SearchHit>,
) -> Result<(), ReadError> {
    let mut answered: Vec<ProjectPath> = Vec::new();
    for candidate in ranked.candidates() {
        let Some(resolved) = resolve_candidate(index, resolution, candidate.identity()) else {
            continue;
        };
        // A text file past the chunk bound publishes one document per chunk, and each
        // carries its own identity through fusion. The answer names files, not chunks,
        // so the best-ranked chunk is the one that becomes the hit and the rest of that
        // file's chunks are already answered.
        if let Some(path) = resolved.answered_path() {
            if answered.contains(path) {
                continue;
            }
            answered.push(path.clone());
        }
        results.push(resolved.into_hit(criteria, candidate)?);
    }
    Ok(())
}

/// The indexes one resolution may read.
#[derive(Clone, Copy)]
struct Resolution<'a> {
    force_include: Option<&'a WorkspaceIndex>,
    packages: Option<&'a DependencyIndex>,
}

/// What one fused identity turned out to name.
enum ResolvedCandidate<'a> {
    /// A declaration, and the index that holds it: the served project's own,
    /// or the one this request built over its `force_include` files. The
    /// index travels with the declaration because assembling its wire symbol
    /// reads the graph the declaration was indexed into, and the two indexes
    /// hold different graphs.
    Declaration(&'a WorkspaceIndex, SymbolMatch<'a>),
    /// A declaration in a cataloged package.
    Package(DependencySymbolMatch<'a>),
    /// A syntax-indexed file this request matched whole.
    SourceFile(&'a IndexedFile),
    /// A baseline text file this request matched whole.
    TextFile(&'a TextSourceFile),
}

impl ResolvedCandidate<'_> {
    /// Whether the request's `target` asks for this kind of hit.
    const fn answers(&self, target: SearchParamsTarget) -> bool {
        match self {
            Self::Declaration(..) | Self::Package(_) => {
                matches!(target, SearchParamsTarget::All | SearchParamsTarget::Symbol)
            }
            Self::SourceFile(_) | Self::TextFile(_) => {
                matches!(target, SearchParamsTarget::All | SearchParamsTarget::File)
            }
        }
    }

    /// Whether the request's path selector reaches this candidate. A package declaration
    /// carries no project path, so a project glob never excludes one.
    fn reaches(
        &self,
        project: &WorkspaceIndex,
        matcher: Option<&PathMatcher>,
        root: &Path,
    ) -> bool {
        match self {
            Self::Declaration(held, found) => {
                // A force-included file is reached by a glob the selector's
                // `exclude` cannot then take back: the request named it.
                !std::ptr::eq(*held, project) || includes(matcher, root, found.file.path())
            }
            Self::SourceFile(file) => includes(matcher, root, file.path()),
            Self::TextFile(file) => includes(matcher, root, file.path()),
            Self::Package(_) => true,
        }
    }

    /// The path this candidate answers whole, for a candidate that answers a file
    /// rather than a declaration. A declaration has its own identity, so two of them
    /// never collapse.
    const fn answered_path(&self) -> Option<&ProjectPath> {
        match self {
            Self::SourceFile(file) => Some(file.path()),
            Self::TextFile(file) => Some(file.path()),
            Self::Declaration(..) | Self::Package(_) => None,
        }
    }

    /// The wire hit this candidate becomes, scored by its place in the fused order.
    fn into_hit(
        self,
        criteria: SearchCriteria<'_>,
        candidate: &FusedCandidate,
    ) -> Result<SearchHit, ReadError> {
        let score = Some(candidate.score());
        let matched_by = matched_fields(candidate);
        match self {
            Self::Declaration(held, found) => {
                build_symbol_hit(held, found, score, matched_by, criteria.payloads)
            }
            Self::Package(found) => {
                dependency_symbol_hit(found, score, matched_by, criteria.payloads)
            }
            Self::SourceFile(file) => {
                let (line, range, text) = locate_query_line(file.source(), criteria.query);
                Ok(source_file_hit(
                    file,
                    (line, range, text),
                    (score, matched_by),
                    criteria.payloads,
                ))
            }
            Self::TextFile(file) => {
                let (line, range, text) = locate_query_line(file.content(), criteria.query);
                Ok(text_file_hit(
                    file,
                    (line, range, text),
                    (score, matched_by),
                    criteria.payloads,
                ))
            }
        }
    }
}

/// The wire fields one fused candidate matched through.
///
/// A candidate the full-text ranking placed names the columns that carried the term. A
/// candidate only the vector ranking placed names no column at all, because no literal
/// byte of the query appears in it, and answers [`MatchedField::Ranked`] instead.
fn matched_fields(candidate: &FusedCandidate) -> Vec<MatchedField> {
    let mut fields: Vec<MatchedField> = Vec::new();
    for field in candidate.fields().fields() {
        let wire = match field {
            SearchableField::Name
            | SearchableField::QualifiedName
            | SearchableField::IdentifierTerms => MatchedField::Name,
            SearchableField::Signature => MatchedField::Signature,
            SearchableField::Documentation => MatchedField::Documentation,
            SearchableField::DeclarationSource | SearchableField::FileContent => {
                MatchedField::Content
            }
        };
        if !fields.contains(&wire) {
            fields.push(wire);
        }
    }
    if fields.is_empty() {
        fields.push(MatchedField::Ranked);
    }
    fields
}

/// What one fused identity names, or `None` when no selected index holds it.
fn resolve_candidate<'a>(
    index: &'a WorkspaceIndex,
    resolution: Resolution<'a>,
    identity: &DocumentIdentity,
) -> Option<ResolvedCandidate<'a>> {
    let value = identity.as_str();
    if value.starts_with(SYMBOL_URI_PREFIX) {
        return resolve_declaration(index, resolution.force_include, value);
    }
    if value.starts_with(SOURCE_UNIT_URI_PREFIX) {
        return resolve_package_declaration(resolution.packages, identity);
    }
    resolve_file(index, value)
}

/// The declaration one `rift://symbol/` identity names, in the project index or in the
/// `force_include` index this request built.
fn resolve_declaration<'a>(
    index: &'a WorkspaceIndex,
    force_include: Option<&'a WorkspaceIndex>,
    identity: &str,
) -> Option<ResolvedCandidate<'a>> {
    let address = parse_symbol_address(identity).ok()?;
    if let Some((file, symbol)) = resolve_symbol(index, &address.path, identity) {
        return Some(ResolvedCandidate::Declaration(
            index,
            declared(file, symbol),
        ));
    }
    let extra = force_include?;
    let (file, symbol) = resolve_symbol(extra, &address.path, identity)?;
    Some(ResolvedCandidate::Declaration(
        extra,
        declared(file, symbol),
    ))
}

/// One resolved declaration as a match. The class is the strongest one, because
/// resolution answers the identity fusion already placed rather than classing a
/// name again.
const fn declared<'a>(file: &'a IndexedFile, symbol: &'a SyntaxSymbol) -> SymbolMatch<'a> {
    SymbolMatch {
        file,
        symbol,
        rank: IdentifierMatchClass::QualifiedExact,
    }
}

/// The package declaration one `rift://source/` identity names.
fn resolve_package_declaration<'a>(
    packages: Option<&'a DependencyIndex>,
    identity: &DocumentIdentity,
) -> Option<ResolvedCandidate<'a>> {
    let (unit, qualified_name) = identity.as_unit()?;
    let unit = SourceUnitId::parse(unit).ok()?;
    packages?
        .symbol_at(&unit, qualified_name)
        .map(ResolvedCandidate::Package)
}

/// Separates a split text file's path from the index of one of its chunks.
const CHUNK_SEPARATOR: char = '#';

/// The file one path identity names: the file the whole identity spells when this
/// snapshot holds one, and the file left when a chunk suffix is stripped otherwise, so
/// every chunk of one split text file resolves to that file.
///
/// The whole identity is read first because a file name may carry the separator itself.
/// A project path admits `#`, so `docs/note#1` is a file this index can hold; stripping
/// first would read it as chunk 1 of `docs/note`, find no such file, and drop the hit.
/// A held `docs/note#1` therefore wins over chunk 1 of a split `docs/note`, whose other
/// chunks still name that file.
fn resolve_file<'a>(index: &'a WorkspaceIndex, identity: &str) -> Option<ResolvedCandidate<'a>> {
    held_file(index, identity).or_else(|| held_file(index, chunked_path(identity)?))
}

/// The path one chunk identity names, or nothing when `identity` carries no chunk suffix:
/// the separator followed by the chunk's index and nothing else.
fn chunked_path(identity: &str) -> Option<&str> {
    let (path, chunk) = identity.rsplit_once(CHUNK_SEPARATOR)?;
    let numbered = !chunk.is_empty() && chunk.bytes().all(|byte| byte.is_ascii_digit());
    numbered.then_some(path)
}

/// The file `path` names in `index`, syntax-indexed or baseline text.
fn held_file<'a>(index: &'a WorkspaceIndex, path: &str) -> Option<ResolvedCandidate<'a>> {
    let path = ProjectPath::new(path.to_owned()).ok()?;
    if let Some(file) = index.file(&path) {
        return Some(ResolvedCandidate::SourceFile(file));
    }
    index.text_file(&path).map(ResolvedCandidate::TextFile)
}

/// Builds one dependency symbol hit's wire shape: the assembly [`build_symbol_hit`] gives
/// a project declaration, addressed by `unit` in place of `path`, at the identifier
/// rank's score.
fn dependency_symbol_hit(
    found: DependencySymbolMatch<'_>,
    score: Option<f64>,
    matched_by: Vec<MatchedField>,
    payloads: HitPayloads,
) -> Result<SearchHit, ReadError> {
    let matched = found.matched;
    let (symbol, unit) = dependency_symbol(found)?;
    Ok(assembled_symbol_hit(
        symbol,
        matched,
        (None, Some(unit)),
        score,
        matched_by,
        payloads,
    ))
}

/// Builds one symbol hit's wire shape. `symbol_search_hit` and `merge_symbol_hit` share
/// this: both surface the same declaration, differing only in score and which indexed field
/// produced the match; `dependency_symbol_hit` shares the assembly below it, addressed by
/// `unit` rather than `path`.
pub(crate) fn build_symbol_hit(
    index: &WorkspaceIndex,
    matched: SymbolMatch<'_>,
    score: Option<f64>,
    matched_by: Vec<MatchedField>,
    payloads: HitPayloads,
) -> Result<SearchHit, ReadError> {
    // A retained disagreement surfaces as a `symbol_disagreement` warning on `get_symbol`
    // (crates/rift-server/src/read.rs); doing the same for a search hit needs the same
    // warnings accumulator threaded through every collector this file merges hits
    // through, and no shipped provider produces a disagreement today. Scoped out of this
    // change; `get_symbol` already carries it.
    let (symbol, _disagreement) = wire_symbol(index, matched)?;
    Ok(assembled_symbol_hit(
        symbol,
        matched,
        (Some(project_path(matched.file.path())), None),
        score,
        matched_by,
        payloads,
    ))
}

/// One symbol hit over `matched`'s declaration bytes: `symbol` already assembled, addressed
/// by exactly one of `path` and `unit`. The excerpt behind `source` is sliced only when
/// `payloads` asked for it, so a request that omits `include` never pays that lookup.
fn assembled_symbol_hit(
    symbol: Symbol,
    matched: SymbolMatch<'_>,
    (path, unit): (Option<WireProjectPath>, Option<WireSourceUnitId>),
    score: Option<f64>,
    matched_by: Vec<MatchedField>,
    payloads: HitPayloads,
) -> SearchHit {
    let line = (!payloads.defer_line || path.is_none())
        .then(|| line::line_number_at(matched.file.source(), matched.symbol.range.start));
    SearchHit {
        hit: SearchHitTarget::Symbol {
            symbol: Box::new(symbol),
        },
        score,
        matched_by,
        source: payloads
            .source
            .then(|| excerpt(matched.file, matched.symbol.range)),
        range: Some(text_range(matched.symbol.range)),
        line,
        path,
        unit,
        traversal_path: None,
        distance: None,
        change: None,
    }
}

/// Resolves project symbol lines only for the returned page. Line numbers never take part
/// in ordering or merging, and source comes from the same held index that supplied the hit.
/// Dependency hits already carry their lines from their package's held source.
fn populate_symbol_lines(
    results: &mut [SearchHit],
    index: &WorkspaceIndex,
    force_include: Option<&WorkspaceIndex>,
) -> Result<(), ReadError> {
    for hit in results {
        if hit.line.is_some() || !matches!(hit.hit, SearchHitTarget::Symbol { .. }) {
            continue;
        }
        let (Some(path), Some(range)) = (hit.path.as_ref(), hit.range.as_ref()) else {
            unreachable!(
                "a deferred project symbol carries its path and range: has_path={}, has_range={}",
                hit.path.is_some(),
                hit.range.is_some()
            );
        };
        let path = ProjectPath::new(path.0.clone())
            .map_err(|error| ReadFault::invalid("path", error.to_string()))?;
        let file = index
            .file(&path)
            .or_else(|| force_include.and_then(|extra| extra.file(&path)))
            .ok_or_else(|| ReadFault::not_found(path.as_str()))?;
        hit.line = Some(line::line_number_at(file.source(), range.start));
    }
    Ok(())
}

/// Builds one syntax-indexed file's hit: the whole file as the target, positioned at the
/// first line carrying a query term.
fn source_file_hit(
    file: &IndexedFile,
    (line, range, text): (u64, ByteRange, String),
    (score, matched_by): (Option<f64>, Vec<MatchedField>),
    payloads: HitPayloads,
) -> SearchHit {
    SearchHit {
        hit: SearchHitTarget::File {
            size: u64::try_from(file.source().len()).unwrap_or(u64::MAX),
            languages: vec![file.syntax().language().clone()],
        },
        score,
        matched_by,
        source: payloads.source.then_some(text),
        range: Some(text_range(range)),
        line: Some(line),
        path: Some(project_path(file.path())),
        unit: None,
        traversal_path: None,
        distance: None,
        change: None,
    }
}

/// Builds one baseline content file hit's target: a `[search.text]` file carries no
/// language claim, unlike a syntax-indexed file's own.
fn text_file_hit_target(file: &TextSourceFile) -> SearchHitTarget {
    SearchHitTarget::File {
        size: u64::try_from(file.content().len()).unwrap_or(u64::MAX),
        languages: Vec::new(),
    }
}

/// Builds one `[search.text]` file's hit, the same shape [`source_file_hit`] builds for a
/// syntax-indexed file.
fn text_file_hit(
    file: &TextSourceFile,
    (line, range, text): (u64, ByteRange, String),
    (score, matched_by): (Option<f64>, Vec<MatchedField>),
    payloads: HitPayloads,
) -> SearchHit {
    SearchHit {
        hit: text_file_hit_target(file),
        score,
        matched_by,
        source: payloads.source.then_some(text),
        range: Some(text_range(range)),
        line: Some(line),
        path: Some(project_path(file.path())),
        unit: None,
        traversal_path: None,
        distance: None,
        change: None,
    }
}

/// Resolves one ranked symbol unit's identity back to its declaration in `index`. `path`
/// narrows the search to the one file the unit named, so this stays a scan of that file's
/// own symbols rather than the whole index. The address is decoded once, names are
/// compared without encoding, and the resolved declaration's identity is checked last.
pub(crate) fn resolve_symbol<'a>(
    index: &'a WorkspaceIndex,
    path: &ProjectPath,
    identity: &str,
) -> Option<(&'a IndexedFile, &'a SyntaxSymbol)> {
    let file = index.file(path)?;
    let language_segment = file.syntax().language().identity_segment();
    let address = parse_symbol_address(identity).ok()?;
    if address.path != *path || address.language_segment != language_segment {
        return None;
    }
    let symbol = file
        .syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == address.qualified_name)?;
    (address.wire_symbol().0 == identity).then_some((file, symbol))
}

/// Finds the first line of `content` carrying one of the query's parsed members,
/// case-insensitively, byte-exact so its span survives a CRLF file unchanged. Falls back to
/// line 1 with a whole-file span when no line matches.
///
/// The members come from the same parse the full-text ranking matched through, so a quoted
/// phrase is looked for as a phrase and a term never carries the punctuation around it.
/// Splitting the caller's raw text here instead would leave `"impact` and `radius"` with
/// their quotes attached, match nothing, and hand back the whole file as the excerpt.
fn locate_query_line(content: &str, query: &ParsedQuery) -> (u64, ByteRange, String) {
    let terms: Vec<String> = query
        .members()
        .iter()
        .map(|member| member.text().to_lowercase())
        .collect();
    let mut offset: u64 = 0;
    for (index, raw_line) in line::lines_inclusive(content).enumerate() {
        let text = line::without_ending(raw_line);
        if terms
            .iter()
            .any(|term| text.to_lowercase().contains(term.as_str()))
        {
            let start = offset;
            let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
            let line_number = u64::try_from(index + 1).unwrap_or(u64::MAX);
            return (line_number, ByteRange { start, end }, text.to_owned());
        }
        offset = offset.saturating_add(u64::try_from(raw_line.len()).unwrap_or(u64::MAX));
    }
    let end = u64::try_from(content.len()).unwrap_or(u64::MAX);
    (1, ByteRange { start: 0, end }, content.to_owned())
}

/// Finds `results`' existing hit for `file`/`symbol`'s wire identity, if a lexical or
/// traversal-walk lane already placed one. `merge_symbol_hit` and the traversal module's
/// `merge_traversal_hit` share this: both recompute the same identity and must not create two
/// hits for one symbol.
pub(crate) fn find_symbol_hit_mut<'a>(
    results: &'a mut [SearchHit],
    file: &IndexedFile,
    symbol: &SyntaxSymbol,
) -> Option<&'a mut SearchHit> {
    let identity = SymbolId(rift_core::symbol_identity(
        &file.syntax().language().identity_segment(),
        file.path().as_str(),
        &symbol.qualified_name,
    ));
    results
        .iter_mut()
        .find(|hit| hit_symbol_id(hit) == Some(&identity))
}

/// One hit's declaration identity: absent for a node or file hit, and for a symbol hit
/// whose identity no accepted evidence established.
pub(crate) fn hit_symbol_id(hit: &SearchHit) -> Option<&SymbolId> {
    match &hit.hit {
        SearchHitTarget::Symbol { symbol } => symbol.id.as_ref(),
        SearchHitTarget::Node { .. } | SearchHitTarget::File { .. } => None,
    }
}

/// Orders the whole pool, then cuts it to `results_max`, answering that bound when the
/// pool reached it. Every collector stops at `results_max`, so a pool of exactly that
/// many hits counts as reached: the answer warns that the bound was met, whether or not
/// a hit past it existed.
fn order_and_bound_hits(
    results: &mut Vec<SearchHit>,
    order: ResultOrder,
    results_max: usize,
) -> Option<usize> {
    order_hits(results, order);
    bound_hits(results, results_max)
}

/// Cuts an ordered hit set to the server's result bound, answering the bound when the set
/// reached it so the answer can warn `results_truncated`.
pub(crate) fn bound_hits(results: &mut Vec<SearchHit>, results_max: usize) -> Option<usize> {
    let reached = results.len() >= results_max;
    results.truncate(results_max);
    reached.then_some(results_max)
}

/// Sorts merged hits by the request's `order`, every order ending in a hit's own stable wire
/// identity so the result does not depend on the arrival order of lexical matches, which
/// carries no guaranteed order of its own, and so two results that tie never swap places
/// between pages.
///
/// `path` order lists every project path first, then the dependency hits, which carry
/// `unit` in its place, in unit order, so a mixed `all` answer never interleaves the two.
pub(crate) fn order_hits(results: &mut [SearchHit], order: ResultOrder) {
    results.sort_by(|left, right| hit_ordering(left, right, order));
}

fn hit_ordering(left: &SearchHit, right: &SearchHit, order: ResultOrder) -> Ordering {
    match order {
        ResultOrder::Relevance => right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| hit_identity(left).cmp(hit_identity(right))),
        ResultOrder::Path => left
            .path
            .is_none()
            .cmp(&right.path.is_none())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.unit.cmp(&right.unit))
            .then_with(|| hit_identity(left).cmp(hit_identity(right))),
        ResultOrder::Identity => hit_identity(left).cmp(hit_identity(right)),
    }
}

/// The wire identity `order_hits` breaks a `relevance` or `path` tie on.
fn hit_identity(hit: &SearchHit) -> &str {
    match &hit.hit {
        SearchHitTarget::Symbol { symbol } => symbol
            .id
            .as_ref()
            .map_or(symbol.name.as_str(), |identity| identity.0.as_str()),
        // Every file hit is built with its project path set; the empty fallback
        // only orders a hit no constructor in this crate produces.
        SearchHitTarget::File { .. } => hit.path.as_ref().map_or("", |path| path.0.as_str()),
        SearchHitTarget::Node { node } => node.0.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use rift_core::{Fault as _, SourceVisibility};
    use rift_index::{LexicalIndexLimits, WorkspaceIndexLimits};
    use rift_protocol::configuration::{HistoryConfiguration, RankingConfiguration};
    use rift_protocol::read::{
        MatchedField, NodeId, PackageIdentity, ReadWarning, ResultOrder, SearchParams,
        SearchParamsTarget, SearchResult, SearchScope, SourceLocationKind, SourceUnitId,
    };
    use rift_ranking::{
        DocumentIdentity, DocumentKind, FieldSet, PARSED_QUERY_MEMBERS_MAX, ParsedQuery,
        QueryPhase, RankedIdentity, RankingInput, RankingInputKind, RankingWeights,
        SearchableField, fuse,
    };
    use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        ByteRange, CandidateScreen, HitPayloads, IdentifierSources, ReadFault, ReadService,
        Resolution, SearchCriteria, SearchHit, SearchHitTarget, StoreAnswer,
    };
    use crate::packages::PackageBranch;
    use crate::read::tests::{helper_store, helper_unit, project_fixture};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// The shares `[search.ranking]` fuses under when an operator sets none.
    fn configured_weights() -> RankingWeights {
        let ranking = RankingConfiguration::default();
        RankingWeights::new(
            ranking.identifier_weight,
            ranking.lexical_weight,
            ranking.vector_weight,
            ranking.fusion_k,
        )
        .expect("the configured shares must fuse")
    }

    /// One search store over `database`, vector ranking off, holding `service`'s own index
    /// documents. A [`RankingInput`] the store produces has no constructor a test can
    /// reach, so publishing to a real store is the only way to obtain one.
    async fn published_store(database: &Path, service: &ReadService) -> TestResult<SearchIndex> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_vector()
            .build();
        let store = SearchIndex::open(database, limits).await?;
        let documents = service.index_documents();
        store
            .replace_lexical(&documents, service.tree_revision())
            .await?;
        Ok(store)
    }

    /// What `store` answered for both phases of `query`, under the configured shares.
    async fn store_answer(
        store: &SearchIndex,
        revision: &str,
        query: &str,
    ) -> TestResult<StoreAnswer> {
        let parsed = ParsedQuery::parse(query)?;
        let precise = phase_inputs(store, revision, &parsed, QueryPhase::Precise).await?;
        let broad = if parsed.has_broad_phase() {
            phase_inputs(store, revision, &parsed, QueryPhase::Broad).await?
        } else {
            Vec::new()
        };
        Ok(StoreAnswer::new(precise, broad, configured_weights()))
    }

    /// The inputs one phase produced, refusing a store stamped with another revision.
    async fn phase_inputs(
        store: &SearchIndex,
        revision: &str,
        query: &ParsedQuery,
        phase: QueryPhase,
    ) -> TestResult<Vec<RankingInput>> {
        let ranked = store.rank(revision, query, phase, 32).await?;
        let RevisionScoped::Matched(ranking) = ranked else {
            return Err("the store must hold the revision it was just stamped with".into());
        };
        Ok(ranking.into_inputs())
    }

    /// One store publication of `service`, asked for `query`: the shape almost every
    /// store-backed case here needs.
    async fn answered(
        database: &Path,
        service: &ReadService,
        query: &str,
    ) -> TestResult<StoreAnswer> {
        let store = published_store(database, service).await?;
        store_answer(&store, service.tree_revision(), query).await
    }

    /// The identities `inputs` fuse to, resolved against `service`'s own index the way
    /// `collect_query_hits` resolves them.
    fn resolved_hits(
        service: &ReadService,
        inputs: &[RankingInput],
        criteria: SearchCriteria<'_>,
        resolution: Resolution<'_>,
    ) -> TestResult<Vec<SearchHit>> {
        let index = service.index();
        let screen = CandidateScreen {
            index,
            matcher: None,
            root: index.root(),
            target: criteria.target,
            resolution,
        };
        let ranked = fuse(
            &screen.screened(inputs),
            configured_weights(),
            QueryPhase::Precise,
            32,
        );
        let mut results = Vec::new();
        super::resolve_ranked_hits(index, criteria, resolution, &ranked, &mut results)?;
        Ok(results)
    }

    /// The hits the identifier ranking alone places for `query`, fused and resolved the
    /// way a request with no store answer runs them.
    fn identifier_hits(
        service: &ReadService,
        query: &str,
        target: SearchParamsTarget,
        payloads: HitPayloads,
    ) -> TestResult<Vec<SearchHit>> {
        let index = service.index();
        let parsed = ParsedQuery::parse(query)?;
        let sources = IdentifierSources {
            project: true,
            force_include: None,
            packages: None,
        };
        let input = super::identifier_input(index, None, index.root(), &parsed, sources, 32)?;
        let criteria = SearchCriteria {
            query: &parsed,
            target,
            payloads,
        };
        let resolution = Resolution {
            force_include: None,
            packages: None,
        };
        resolved_hits(service, &[input], criteria, resolution)
    }

    /// One ordered full-text input, as the store would have answered it.
    fn lexical_input(order: Vec<(DocumentIdentity, FieldSet)>) -> RankingInput {
        ranked_input(RankingInputKind::Lexical, order)
    }

    /// One ordered input of `kind`.
    fn ranked_input(
        kind: RankingInputKind,
        order: Vec<(DocumentIdentity, FieldSet)>,
    ) -> RankingInput {
        RankingInput::new(
            kind,
            order
                .into_iter()
                .map(|(identity, fields)| RankedIdentity::new(identity, fields))
                .collect(),
        )
    }

    /// One project declaration's ranking identity, the address `get_symbol` answers with.
    fn declaration_identity(path: &str, qualified_name: &str) -> TestResult<DocumentIdentity> {
        Ok(DocumentIdentity::new(rift_core::symbol_identity(
            "rust",
            path,
            qualified_name,
        ))?)
    }

    /// Each hit's own wire identity, in answer order: a declaration's address, or a file
    /// hit's project path.
    fn hit_identities(result: &SearchResult) -> Vec<String> {
        result
            .results
            .iter()
            .map(|hit| super::hit_identity(hit).to_owned())
            .collect()
    }

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.txt"), "Beacon docs")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    const RICH_SOURCE: &str = r#"pub enum Level { Low, High }

pub trait Speaks {
    fn say(&self);
}

pub type Alias = u32;

pub const MAX: u32 = 10;

pub static NAME: &str = "beacon";

pub mod inner {
    pub fn nested() {}
}

macro_rules! noop {
    () => {};
}

struct Hidden;

pub(crate) fn scoped() {}

pub fn compute() -> i32 {
    // lookout marker
    let total = 1 + 2;
    total;
    0
}
"#;

    fn rich_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), RICH_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// One declaration at each identifier match class for the candidate `beacon`:
    /// `beacon` equals the qualified name, `Tower::beacon` equals the short name,
    /// `beacon_relay` starts with it, and `Tower::relay_to_beacon` carries it elsewhere.
    const MATCH_CLASS_SOURCE: &str = r"pub fn beacon() {}

pub fn beacon_relay() {}

pub struct Tower;

impl Tower {
    pub fn beacon() {}
    pub fn relay_to_beacon() {}
}
";

    fn match_class_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), MATCH_CLASS_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// Three files under `src/`, so `*` vs `**` and include/exclude composition all have
    /// something to disagree about.
    fn multi_file_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src/nested"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon_top() {}\n",
        )?;
        fs::write(
            directory.path().join("src/nested/deep.rs"),
            "pub fn beacon_nested() {}\n",
        )?;
        fs::write(
            directory.path().join("other.rs"),
            "pub fn beacon_other() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    /// One indexed file, one hidden by `.gitignore`, one hidden by `[source].exclude`, and one
    /// under the hard floor (`.git/`) - the fixture `force_include` tests reach into.
    fn force_include_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "gitignored.rs\n")?;
        fs::write(
            directory.path().join("visible.rs"),
            "pub fn visible_symbol() {}\n",
        )?;
        fs::write(
            directory.path().join("gitignored.rs"),
            "pub fn phantom_gitignored() {}\n",
        )?;
        fs::write(
            directory.path().join("configured_out.rs"),
            "pub fn phantom_configured() {}\n",
        )?;
        fs::create_dir_all(directory.path().join(".git"))?;
        fs::write(
            directory.path().join(".git/floor.rs"),
            "pub fn floor() {}\n",
        )?;
        let visibility =
            SourceVisibility::new(Vec::new(), vec!["configured_out.rs".to_owned()], true);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn project_symbol_lines_wait_for_paging_and_use_the_held_source() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "// café\r\npub fn page_a() {}\r\n\r\npub fn page_b() {}";
        fs::write(directory.path().join("lib.rs"), source)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({"query": "page"}))?;
        let payloads = HitPayloads::requested(&params);
        let mut hits = identifier_hits(&service, "page", SearchParamsTarget::Symbol, payloads)?;
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter().all(|hit| hit.line.is_none()),
            "unpaged hits must not scan source for lines"
        );
        super::order_hits(&mut hits, ResultOrder::Identity);
        let (mut selected, _) = crate::read::page(hits, 1, 1);
        fs::write(directory.path().join("lib.rs"), "pub fn moved() {}\n")?;
        super::populate_symbol_lines(&mut selected, service.index(), None)?;
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].line,
            Some(4),
            "line must come from the held CRLF source"
        );
        Ok(())
    }

    #[tokio::test]
    async fn symbol_lines_and_order_stay_exact_across_search_pages() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "forced.rs\n")?;
        fs::write(
            directory.path().join("lib.rs"),
            "// café\r\npub fn page_a() {}\r\n\r\npub fn page_b() {}\r\npub fn page_c() {}",
        )?;
        fs::write(
            directory.path().join("forced.rs"),
            "// café\n\npub fn page_forced() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let store = answered(&database, &service, "page").await?;
        for order in ["relevance", "identity", "path"] {
            let mut request = json!({
                "query": "page", "target": "symbol", "limit": 10,
                "order": order, "include": ["source", "score"],
                "paths": {"force_include": ["forced.rs"]}
            });
            let full = service.search(&serde_json::from_value(request.clone())?, &store)?;
            assert_eq!(full.results.len(), 4);
            for hit in &full.results {
                let SearchHitTarget::Symbol { symbol } = &hit.hit else {
                    return Err("fixture must return symbols".into());
                };
                let line = match symbol.name.as_str() {
                    "page_a" => 2,
                    "page_b" => 4,
                    "page_c" => 5,
                    "page_forced" => 3,
                    other => return Err(format!("unexpected symbol: {other}").into()),
                };
                assert_eq!(hit.line, Some(line));
            }
            request["limit"] = json!(1);
            let mut pages = Vec::new();
            for page_index in 0..4 {
                request["page_index"] = json!(page_index);
                let page = service.search(&serde_json::from_value(request.clone())?, &store)?;
                assert_eq!(page.warnings, full.warnings);
                pages.extend(page.results);
            }
            assert_eq!(
                pages, full.results,
                "paging must preserve every hit and order: {order}"
            );
        }
        Ok(())
    }

    /// Symbol hits from a TypeScript file carry the `typescript` language
    /// and its composed wire kind.
    #[test]
    fn search_symbol_hits_carry_the_typescript_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("routes.ts"),
            "export interface Route {\n  path: string;\n}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Route",
            "target": "symbol",
            "limit": 5
        }))?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        let symbol = &results[0]["hit"]["symbol"];
        assert_eq!(symbol["language"], json!("typescript"));
        assert_eq!(symbol["kind"], json!("interface"));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/typescript/routes.ts/Route")
        );
        Ok(())
    }

    /// Symbol hits from a markdown file carry the `markdown` language, the composed wire
    /// kind, and an id escaping the heading text. The heading is two plain words, which
    /// the identifier ranking never extracts, so the store's full-text input places it.
    #[tokio::test]
    async fn search_symbol_hits_carry_the_markdown_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        let notes_md = "# Beacon Notes\n\nCalibration steps.\n";
        fs::write(directory.path().join("notes.md"), notes_md)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let store = answered(&database, &service, "Beacon Notes").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon Notes",
            "target": "symbol",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        let symbol = &results[0]["hit"]["symbol"];
        assert_eq!(symbol["language"], json!("markdown"));
        assert_eq!(symbol["kind"], json!("heading"));
        assert_eq!(
            symbol["id"],
            json!("rift://symbol/markdown/notes.md/Beacon%20Notes")
        );
        Ok(())
    }

    /// Symbol hits from JSON and YAML files carry their languages, the
    /// composed wire kinds, and ids escaping the key path.
    #[tokio::test]
    async fn search_symbol_hits_carry_the_json_and_yaml_languages() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("config.json"),
            "{\"beacon port\": 8080}\n",
        )?;
        fs::write(directory.path().join("deploy.yaml"), "beacon retries: 3\n")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let store = published_store(&directory.path().join("search.db"), &service).await?;
        let expectations = [
            (
                "beacon port",
                "json",
                "member",
                "rift://symbol/json/config.json/beacon%20port",
            ),
            (
                "beacon retries",
                "yaml",
                "mapping_entry",
                "rift://symbol/yaml/deploy.yaml/beacon%20retries",
            ),
        ];
        for (query, language, kind, id) in expectations {
            let answer = store_answer(&store, service.tree_revision(), query).await?;
            let request = json!({
                "query": query,
                "target": "symbol",
                "limit": 5
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let value = serde_json::to_value(service.search(&params, &answer)?)?;
            let results = value["results"].as_array().ok_or("results must be array")?;
            assert!(!results.is_empty(), "{query} must return a hit");
            let symbol = &results[0]["hit"]["symbol"];
            assert_eq!(symbol["language"], json!(language));
            assert_eq!(symbol["kind"], json!(kind));
            assert_eq!(symbol["id"], json!(id));
        }
        Ok(())
    }

    /// One answer carries the declarations and the whole files one query reached: the
    /// two declarations of `src/lib.rs`, that file itself, and the `README.txt` the
    /// store's full-text input placed. Scores fall strictly with position, so the answer
    /// states its own order.
    #[tokio::test]
    async fn search_combines_symbol_and_file_hits_on_one_page() -> TestResult {
        let (directory, service) = fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "Beacon").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "limit": 4,
            "include": ["score"]
        }))?;
        let result = service.search(&params, &store)?;
        let mut identities = hit_identities(&result);
        identities.sort();
        assert_eq!(
            identities,
            [
                "README.txt",
                "rift://symbol/rust/src/lib.rs/Beacon",
                "rift://symbol/rust/src/lib.rs/Beacon::signal",
                "src/lib.rs",
            ],
            "{result:#?}"
        );
        assert_eq!(
            serde_json::to_value(&result.pagination)?,
            json!({ "page_index": 0, "total_pages": 1 })
        );
        let scores: Vec<f64> = result.results.iter().filter_map(|hit| hit.score).collect();
        assert_eq!(scores.len(), 4, "{result:#?}");
        assert!(
            scores.windows(2).all(|pair| pair[0] > pair[1]),
            "a fused score falls strictly with position: {scores:?}"
        );
        assert!(
            result
                .results
                .iter()
                .all(|hit| hit.path.as_ref().is_some_and(|path| !path.0.is_empty())),
            "every hit must carry a non-empty project-relative path: {result:#?}"
        );
        Ok(())
    }

    /// The candidate pool stops at `results_max` whatever the page size asked for: the
    /// third matching declaration never enters it, and the answer warns the bound.
    #[test]
    fn search_pool_stops_at_the_result_bound_and_leaves_a_later_candidate_out() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_alpha() {}\npub fn beacon_beta() {}\npub fn beacon_gamma() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 2)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            hit_identities(&result),
            [
                "rift://symbol/rust/lib.rs/beacon_alpha",
                "rift://symbol/rust/lib.rs/beacon_beta",
            ],
            "{result:#?}"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::ResultsTruncated { results_max } if *results_max == 2)),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// The `paths` selector screens the ranking inputs before they are fused. The store
    /// ranked more matches outside the selector than the candidate pool holds; screening
    /// after fusion cut the pool to those outside matches, dropped every one of them, and
    /// answered nothing although the selector holds a match.
    #[test]
    fn search_paths_selector_screens_the_pool_before_ranking() -> TestResult {
        let (_directory, service, store) = selector_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout",
            "target": "file",
            "paths": {"include": ["src/**"]},
            "limit": 10
        }))?;
        let result = service.search(&params, &store)?;
        assert_eq!(hit_identities(&result), ["src/keep.txt"], "{result:#?}");
        Ok(())
    }

    /// `target` screens the same way: a pool filled with declarations under
    /// `target: "file"` left the file match unranked and answered nothing.
    #[test]
    fn search_target_screens_the_pool_before_ranking() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn lookout_alpha() {}\npub fn lookout_beta() {}\npub fn lookout_gamma() {}\n",
        )?;
        fs::write(directory.path().join("notes.txt"), "lookout marker")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 2)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let mut order = Vec::new();
        for name in ["lookout_alpha", "lookout_beta", "lookout_gamma"] {
            order.push((
                declaration_identity("lib.rs", name)?,
                FieldSet::of(SearchableField::Name),
            ));
        }
        order.push((
            DocumentIdentity::new("notes.txt")?,
            FieldSet::of(SearchableField::FileContent),
        ));
        let store = StoreAnswer::new(vec![lexical_input(order)], Vec::new(), configured_weights());
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout",
            "target": "file",
            "limit": 10
        }))?;
        let result = service.search(&params, &store)?;
        assert_eq!(hit_identities(&result), ["notes.txt"], "{result:#?}");
        Ok(())
    }

    /// Three text files outside `src`, one inside it, and a store answer ranking the
    /// outside files first, over a workspace whose candidate pool holds two.
    fn selector_fixture() -> TestResult<(TempDir, ReadService, StoreAnswer)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("out"))?;
        fs::create_dir(directory.path().join("src"))?;
        let outside = ["out/one.txt", "out/two.txt", "out/three.txt"];
        for path in outside {
            fs::write(directory.path().join(path), "lookout marker")?;
        }
        fs::write(directory.path().join("src/keep.txt"), "lookout marker")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 2)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let mut order = Vec::new();
        for path in outside.into_iter().chain(["src/keep.txt"]) {
            order.push((
                DocumentIdentity::new(path)?,
                FieldSet::of(SearchableField::FileContent),
            ));
        }
        let store = StoreAnswer::new(vec![lexical_input(order)], Vec::new(), configured_weights());
        Ok((directory, service, store))
    }

    /// The candidate pool reaches `results_max` and still resolves to fewer hits, because
    /// every chunk of one split text file collapses into that file. The answer warns the
    /// bound all the same: the candidates past it never reach a page.
    #[test]
    fn search_warns_the_result_bound_when_fusion_cut_the_pool() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.txt"), "word ".repeat(1000))?;
        fs::write(directory.path().join("notes.txt"), "word marker")?;
        // The smallest accepted chunk bound against a several-kilobyte guide forces the
        // file into more than one document, each of which the query matches.
        let text_inclusion = rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 65_536, 131_072, 8, 2)?,
            &SourceVisibility::default(),
            &text_inclusion,
            HistoryConfiguration::default(),
        )?;
        let documents = service.index_documents();
        let mut order: Vec<(DocumentIdentity, FieldSet)> = documents
            .iter()
            .filter(|document| document.identity().as_str().starts_with("guide.txt#"))
            .map(|document| {
                (
                    document.identity().clone(),
                    FieldSet::of(SearchableField::FileContent),
                )
            })
            .collect();
        assert!(
            order.len() > 1,
            "the oversized guide must split into more than one document: {order:?}"
        );
        order.truncate(2);
        order.push((
            DocumentIdentity::new("notes.txt")?,
            FieldSet::of(SearchableField::FileContent),
        ));
        let store = StoreAnswer::new(vec![lexical_input(order)], Vec::new(), configured_weights());
        let params: SearchParams = serde_json::from_value(json!({
            "query": "word",
            "target": "file",
            "limit": 10
        }))?;
        let result = service.search(&params, &store)?;
        assert_eq!(hit_identities(&result), ["guide.txt"], "{result:#?}");
        assert!(
            result.warnings.iter().any(|warning| matches!(
                warning,
                ReadWarning::ResultsTruncated { results_max } if *results_max == 2
            )),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// A query past the parser's member bound drops its shortest unquoted terms, and the
    /// answer says so rather than reading as though every term had been matched.
    #[test]
    fn search_warns_query_narrowed_when_the_parser_dropped_terms() -> TestResult {
        let (_directory, service) = fixture()?;
        let query = (0..PARSED_QUERY_MEMBERS_MAX + 8)
            .map(|index| format!("beacon{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let params: SearchParams = serde_json::from_value(json!({
            "query": query,
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(
            result.warnings.iter().any(|warning| matches!(
                warning,
                ReadWarning::QueryNarrowed { terms_max } if *terms_max == 32
            )),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// A project path admits `#`, so a file name ending in the chunk separator and a
    /// number is a path of its own. Stripping the suffix first resolved `docs/note#1` to
    /// `docs/note`, found no such file, and dropped the hit.
    #[test]
    fn a_file_path_ending_in_the_chunk_separator_resolves_whole() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("docs"))?;
        fs::write(directory.path().join("docs/note#1"), "lookout marker")?;
        let text_inclusion = rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1_048_576);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &text_inclusion,
            HistoryConfiguration::default(),
        )?;
        let store = StoreAnswer::new(
            vec![lexical_input(vec![(
                DocumentIdentity::new("docs/note#1")?,
                FieldSet::of(SearchableField::FileContent),
            )])],
            Vec::new(),
            configured_weights(),
        );
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout",
            "target": "file",
            "limit": 10
        }))?;
        let result = service.search(&params, &store)?;
        assert_eq!(hit_identities(&result), ["docs/note#1"], "{result:#?}");
        Ok(())
    }

    /// A symbol hit's `path` used to render `null`, with the project-relative location
    /// reachable only by parsing the symbol's own id.
    #[test]
    fn search_hits_carry_project_relative_path_for_nested_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10
        }))?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|hit| hit["path"].as_str().is_some_and(|path| !path.is_empty())),
            "every symbol hit must carry a non-empty project-relative path: {results:#?}"
        );
        assert!(
            results
                .iter()
                .any(|hit| hit["path"] == json!("src/nested/deep.rs")),
            "the nested file's hit must carry its nested project-relative path: {results:#?}"
        );
        Ok(())
    }

    /// The excerpt used to duplicate the hit's own `span` inside `source`; it now carries
    /// text only, and the range a caller needs is the one already on the hit.
    #[test]
    fn search_hit_source_excerpt_is_text_only_with_no_duplicate_span() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "symbol",
            "include": ["source"],
            "limit": 1
        }))?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        let hit = &value["results"][0];
        assert!(
            hit["source"].is_string(),
            "excerpt must serialize as a bare string, not an object carrying a span: {hit}"
        );
        assert_eq!(hit["source"], json!("pub struct Beacon;"));
        assert_eq!(hit["range"]["start"], 0);
        Ok(())
    }

    /// An omitted `include` never pays the `source` lookup: every hit still carries its
    /// symbol or file, `path`, `range`, and `line`, and none carries `source`.
    #[tokio::test]
    async fn search_without_include_omits_source_but_keeps_symbol_path_span_and_line() -> TestResult
    {
        let (directory, service) = fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "Beacon").await?;
        let request = json!({ "query": "Beacon", "limit": 10 });
        let params: SearchParams = serde_json::from_value(request)?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(
            results.iter().all(|hit| hit["source"].is_null()),
            "an omitted include must never carry source: {results:#?}"
        );
        assert!(
            results.iter().all(|hit| !hit["path"].is_null()
                && !hit["range"].is_null()
                && !hit["line"].is_null()
                && !hit["hit"].is_null()),
            "an omitted include must still carry the hit's symbol or file, path, range, and \
             line: {results:#?}"
        );
        Ok(())
    }

    /// The identifier ranking orders by match class: the qualified-name match, then the
    /// short-name match, then the prefix, then the declaration that carries the candidate
    /// somewhere else.
    #[test]
    fn search_orders_identifier_matches_by_class_strongest_first() -> TestResult {
        let (_directory, service) = match_class_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            hit_identities(&result),
            [
                declaration_identity("src/lib.rs", "beacon")?.as_str(),
                declaration_identity("src/lib.rs", "Tower::beacon")?.as_str(),
                declaration_identity("src/lib.rs", "beacon_relay")?.as_str(),
                declaration_identity("src/lib.rs", "Tower::relay_to_beacon")?.as_str(),
            ],
            "{result:#?}"
        );
        Ok(())
    }

    /// With a store that answered nothing the identifier ranking is the whole answer, and
    /// each hit's score is its place in the fused order: the best scores 1.0 and every
    /// later one scores strictly less.
    #[test]
    fn search_without_a_store_answer_scores_every_hit_by_its_fused_position() -> TestResult {
        let (_directory, service) = match_class_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "include": ["score"]
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        let scores: Vec<Option<f64>> = result.results.iter().map(|hit| hit.score).collect();
        assert_eq!(
            scores,
            [Some(1.0), Some(0.75), Some(0.5), Some(0.25)],
            "{result:#?}"
        );
        Ok(())
    }

    /// Two declarations reaching one match class at one candidate position tie, and the
    /// tie breaks on the declaration's own identity rather than on declaration order.
    #[test]
    fn search_ties_at_one_match_class_order_by_the_declarations_identity() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_beta() {}\npub fn beacon_alpha() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            hit_identities(&result),
            [
                "rift://symbol/rust/lib.rs/beacon_alpha",
                "rift://symbol/rust/lib.rs/beacon_beta",
            ],
            "{result:#?}"
        );
        Ok(())
    }

    /// An input that did not answer carries no share, so adding an unanswered vector
    /// ranking beside the identifier and full-text inputs changes neither the order nor
    /// the scores.
    #[test]
    fn an_unanswered_vector_input_leaves_the_fused_order_unchanged() -> TestResult {
        let alpha = DocumentIdentity::new("alpha")?;
        let beta = DocumentIdentity::new("beta")?;
        let gamma = DocumentIdentity::new("gamma")?;
        let identifier = ranked_input(
            RankingInputKind::Identifier,
            vec![
                (alpha.clone(), FieldSet::of(SearchableField::Name)),
                (beta.clone(), FieldSet::of(SearchableField::QualifiedName)),
            ],
        );
        let lexical = lexical_input(vec![
            (gamma, FieldSet::of(SearchableField::FileContent)),
            (alpha, FieldSet::of(SearchableField::Documentation)),
            (beta, FieldSet::of(SearchableField::Signature)),
        ]);
        let answered = vec![identifier, lexical];
        let mut with_vector = answered.clone();
        with_vector.push(RankingInput::unanswered(RankingInputKind::Vector));

        let without = fuse(&answered, configured_weights(), QueryPhase::Precise, 32);
        let with = fuse(&with_vector, configured_weights(), QueryPhase::Precise, 32);

        assert_eq!(
            with.candidates(),
            without.candidates(),
            "an unanswered input must not move a candidate"
        );
        Ok(())
    }

    /// `target: "node"` left `SearchParamsTarget`'s served variants; a request naming it is
    /// refused at deserialization, not accepted and silently ignored.
    #[test]
    fn search_target_node_is_refused_as_an_unknown_enum_value() {
        let result: Result<SearchParams, _> =
            serde_json::from_value(json!({"query": "Beacon", "target": "node"}));
        assert!(
            result.is_err(),
            "a withdrawn target value must fail deserialization"
        );
    }

    #[test]
    fn search_requires_query_rejects_empty_query_and_zero_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let missing_query: SearchParams = serde_json::from_value(json!({}))?;
        assert!(matches!(
            service
                .search(&missing_query, &StoreAnswer::identifier_only())
                .expect_err("missing query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let empty_query: SearchParams = serde_json::from_value(json!({"query": ""}))?;
        assert!(matches!(
            service
                .search(&empty_query, &StoreAnswer::identifier_only())
                .expect_err("empty query must fail")
                .fault(),
            ReadFault::Invalid { .. }
        ));

        let zero_limit: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "limit": 0}))?;
        let error = service
            .search(&zero_limit, &StoreAnswer::identifier_only())
            .expect_err("zero limit must fail");
        assert!(matches!(error.fault(), ReadFault::Invalid { .. }));
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field limit, \
             violation zero; correct the reported field and resend the request"
        );
        Ok(())
    }

    /// One snapshot holds one tree; a request carrying `change` names two, and the
    /// comparison that reads them reaches its own revisions instead.
    #[test]
    fn search_on_a_captured_snapshot_refuses_a_change_comparison() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams =
            serde_json::from_value(json!({"change": {"base": "main", "head": "HEAD"}}))?;

        let error = service
            .search(&params, &StoreAnswer::identifier_only())
            .expect_err("a comparison on one snapshot must refuse");

        assert!(matches!(error.fault(), ReadFault::Unsupported { .. }));
        assert!(
            error
                .to_string()
                .contains("a revision comparison on a captured snapshot"),
            "{error}"
        );
        Ok(())
    }

    /// Collection is bounded by `results_max` whatever the page size, so a `limit` above
    /// that bound serves the whole bounded result set as one page, and the answer warns
    /// `results_truncated` naming the bound.
    #[test]
    fn search_limit_above_the_result_bound_serves_the_whole_set_on_one_page() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_alpha() {}\npub fn beacon_beta() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 1)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 2
        }))?;
        let value =
            serde_json::to_value(service.search(&params, &StoreAnswer::identifier_only())?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1, "{results:#?}");
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 0, "total_pages": 1 })
        );
        assert!(
            value["warnings"].as_array().is_some_and(|warnings| {
                warnings.contains(&json!({ "code": "results_truncated", "results_max": 1 }))
            }),
            "{value:#}"
        );
        Ok(())
    }

    /// A result set under `results_max` carries no `results_truncated` warning.
    #[test]
    fn search_under_the_result_bound_carries_no_results_truncated_warning() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({ "query": "Beacon" }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::ResultsTruncated { .. })),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// The store ranks the `README.txt` document beside the two declarations; a
    /// `symbol` target leaves the file candidate out of the answer, and a `file` target
    /// proves it was there to leave out.
    #[tokio::test]
    async fn search_target_symbol_excludes_file_hits() -> TestResult {
        let (directory, service) = fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "Beacon").await?;
        let declarations: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "symbol",
            "limit": 5
        }))?;
        let result = service.search(&declarations, &store)?;
        assert!(!result.results.is_empty());
        assert!(
            result
                .results
                .iter()
                .all(|hit| matches!(hit.hit, SearchHitTarget::Symbol { .. })),
            "{result:#?}"
        );
        let files: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "file",
            "limit": 5
        }))?;
        let files = service.search(&files, &store)?;
        assert!(
            hit_identities(&files).contains(&"README.txt".to_owned()),
            "the file candidate the symbol target drops must exist: {files:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_reports_multi_line_file_match_position() -> TestResult {
        let (directory, service) = rich_fixture()?;
        let store = answered(
            &directory.path().join("search.db"),
            &service,
            "lookout marker",
        )
        .await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout marker",
            "target": "file",
            "include": ["source"]
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1, "{results:#?}");
        assert!(results[0]["line"].as_u64().is_some_and(|line| line > 1));
        assert_eq!(results[0]["source"], "    // lookout marker");
        Ok(())
    }

    /// A sentence present in both a provider-claimed file (markdown, syntax index) and a
    /// `.mdx` file (`[search.text]`, no provider claims it) returns both hits: each file
    /// contributes its own text document to the store, and the two resolve to the two
    /// file shapes the answer distinguishes by language.
    #[tokio::test]
    async fn search_returns_a_text_file_hit_alongside_a_provider_claimed_hit_for_one_sentence()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let sentence = "agentic development toolkit";
        fs::write(
            directory.path().join("README.md"),
            format!("# Rift\n\nRift is an {sentence}.\n"),
        )?;
        fs::write(
            directory.path().join("guide.mdx"),
            format!("Rift is an {sentence} for editors.\n"),
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let store = answered(&directory.path().join("search.db"), &service, sentence).await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": sentence,
            "target": "file",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        let readme = results
            .iter()
            .find(|hit| hit["path"] == json!("README.md"))
            .ok_or("the provider-claimed file must return a hit")?;
        assert_eq!(
            readme["hit"]["languages"],
            json!(["markdown"]),
            "a provider-claimed file carries the language that claimed it: {readme:#?}"
        );
        let mdx = results
            .iter()
            .find(|hit| hit["path"] == json!("guide.mdx"))
            .ok_or("the text-lane file must return a hit through the store")?;
        assert_eq!(
            mdx["hit"]["languages"],
            serde_json::Value::Null,
            "a text-lane file no provider claims carries no language: {mdx:#?}"
        );
        Ok(())
    }

    /// An explicit `[search.text]` path rule reaches an extensionless `justfile`, and a
    /// query for content only it holds returns it.
    #[tokio::test]
    async fn search_returns_an_explicitly_included_justfile_hit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("justfile"),
            "test:\n\tcargo test --workspace\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;
        let store = answered(
            &directory.path().join("search.db"),
            &service,
            "cargo test --workspace",
        )
        .await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "cargo test --workspace",
            "target": "file",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(
            results.len(),
            1,
            "only the justfile holds this content: {results:#?}"
        );
        assert_eq!(results[0]["path"], json!("justfile"));
        assert_eq!(
            results[0]["hit"]["languages"],
            serde_json::Value::Null,
            "a baseline [search.text] file claims no language"
        );
        Ok(())
    }

    #[test]
    fn search_resolves_every_rust_symbol_kind_and_visibility() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let cases = [
            ("Level", "enum", None),
            ("Speaks", "trait", None),
            ("Alias", "type_alias", None),
            ("MAX", "constant", None),
            ("NAME", "static", None),
            ("inner", "module", None),
            ("noop", "macro", None),
            ("Hidden", "struct", Some("private")),
            ("scoped", "function", Some("pub(crate)")),
        ];
        for (name, kind, visibility) in cases {
            let params: SearchParams = serde_json::from_value(json!({
                "query": name,
                "target": "symbol",
                "limit": 1
            }))?;
            let answer = service.search(&params, &StoreAnswer::identifier_only())?;
            let value = serde_json::to_value(answer)?;
            let hit = &value["results"][0]["hit"]["symbol"];
            assert_eq!(hit["kind"], kind, "unexpected kind for {name}");
            if let Some(expected_visibility) = visibility {
                assert_eq!(
                    hit["visibility"], expected_visibility,
                    "unexpected visibility for {name}"
                );
                assert!(
                    !hit["facets"]
                        .as_array()
                        .is_some_and(|facets| facets.contains(&json!("public"))),
                    "{name} must not carry public facet"
                );
            }
        }
        Ok(())
    }

    /// `filter` left the served schema; a request naming it is refused as an unknown field,
    /// alone or alongside `paths`. `traversal` is this PR's return: a well-formed request
    /// carrying it parses, alone or alongside `paths` and `query`.
    #[test]
    fn search_filter_stays_refused_and_traversal_now_parses_alone_and_with_paths() {
        let filter = json!({
            "kind": "field",
            "field": {"field": "name", "op": "eq", "value": "Beacon"}
        });
        let traversal = json!({"seed": "rift://symbol/rust/src/lib.rs/Beacon"});
        let paths = json!({"include": ["src/lib.rs"]});
        let refused = [
            json!({"query": "Beacon", "filter": filter.clone()}),
            json!({"query": "Beacon", "filter": filter, "paths": paths.clone()}),
        ];
        for case in refused {
            let result: Result<SearchParams, _> = serde_json::from_value(case.clone());
            assert!(
                result.is_err(),
                "a withdrawn field must fail deserialization: {case}"
            );
        }
        let accepted = [
            json!({"traversal": traversal.clone()}),
            json!({"query": "Beacon", "traversal": traversal, "paths": paths}),
        ];
        for case in accepted {
            let result: Result<SearchParams, _> = serde_json::from_value(case.clone());
            assert!(
                result.is_ok(),
                "a well-formed traversal request must parse: {case} ({result:?})"
            );
        }
    }

    #[test]
    fn search_query_with_paths_succeeds_end_to_end() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["src/lib.rs"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(!result.results.is_empty());
        Ok(())
    }

    #[test]
    fn search_paths_include_narrows_to_matching_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["other.rs"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(!result.results.is_empty());
        assert!(
            hit_identities(&result)
                .iter()
                .all(|identity| identity.contains("other.rs")),
            "{result:#?}"
        );
        Ok(())
    }

    #[test]
    fn search_paths_exclude_drops_matching_files() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"exclude": ["other.rs"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(!result.results.is_empty());
        assert!(
            hit_identities(&result)
                .iter()
                .all(|identity| !identity.contains("other.rs")),
            "{result:#?}"
        );
        Ok(())
    }

    #[test]
    fn search_paths_include_and_exclude_compose() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/**"], "exclude": ["src/nested/**"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            hit_identities(&result),
            ["rift://symbol/rust/src/lib.rs/beacon_top"],
            "{result:#?}"
        );
        Ok(())
    }

    #[test]
    fn search_paths_star_does_not_cross_slash() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/*.rs"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            hit_identities(&result),
            ["rift://symbol/rust/src/lib.rs/beacon_top"],
            "{result:#?}"
        );
        Ok(())
    }

    #[test]
    fn search_paths_double_star_crosses_slash() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 10,
            "paths": {"include": ["src/**"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(result.results.len(), 2, "{result:#?}");
        Ok(())
    }

    #[tokio::test]
    async fn search_paths_target_file_returns_only_matching_tree_entries() -> TestResult {
        let (directory, service) = multi_file_fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "beacon").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "file",
            "paths": {"include": ["src/lib.rs"]}
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| hit["hit"]["target"] == "file"));
        assert!(results.iter().all(|hit| hit["path"] == json!("src/lib.rs")));
        Ok(())
    }

    #[test]
    fn search_paths_include_invalid_glob_refuses() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["["]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &StoreAnswer::identifier_only())
                .expect_err("an invalid include glob must refuse")
                .fault(),
            ReadFault::Index(_)
        ));
        Ok(())
    }

    #[test]
    fn search_paths_backslash_pattern_refuses_before_reaching_the_glob_engine() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"include": ["src\\lib.rs"]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &StoreAnswer::identifier_only())
                .expect_err("a backslash pattern must be refused")
                .fault(),
            ReadFault::Invalid { field: "paths", .. }
        ));
        Ok(())
    }

    #[test]
    fn search_paths_scoped_results_form_stable_prefix_across_limits() -> TestResult {
        // `paths` filtering happens before one page is cut out of the ordered pool, so the
        // top hit of a `paths`-scoped search does not move as `limit` grows.
        let (_directory, service) = multi_file_fixture()?;
        let narrow: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["**/*.rs"]},
            "limit": 1
        }))?;
        let wide: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "paths": {"include": ["**/*.rs"]},
            "limit": 3
        }))?;
        let narrow_result = service.search(&narrow, &StoreAnswer::identifier_only())?;
        let wide_result = service.search(&wide, &StoreAnswer::identifier_only())?;
        assert_eq!(narrow_result.results[0], wide_result.results[0]);
        Ok(())
    }

    #[test]
    fn search_pages_partition_the_ordered_pool_without_overlap() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let mut seen = Vec::new();
        for page_index in 0..3_u64 {
            let request = json!({
                "query": "beacon",
                "target": "symbol",
                "limit": 1,
                "page_index": page_index
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let answer = service.search(&params, &StoreAnswer::identifier_only())?;
            let value = serde_json::to_value(answer)?;
            assert_eq!(
                value["pagination"],
                json!({ "page_index": page_index, "total_pages": 3 })
            );
            let id = value["results"][0]["hit"]["symbol"]["id"]
                .as_str()
                .ok_or("every page must carry one symbol hit")?
                .to_owned();
            assert!(!seen.contains(&id), "pages must not overlap: {id}");
            seen.push(id);
        }
        Ok(())
    }

    #[test]
    fn search_page_past_the_end_is_empty_with_the_true_page_count() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let request = json!({
            "query": "beacon",
            "target": "symbol",
            "limit": 1,
            "page_index": 30
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        assert_eq!(value["results"], json!([]));
        assert_eq!(
            value["pagination"],
            json!({ "page_index": 30, "total_pages": 3 })
        );
        Ok(())
    }

    #[test]
    fn search_order_path_sorts_by_project_path() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "path",
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        let paths: Vec<String> = result
            .results
            .iter()
            .map(|hit| {
                hit.path
                    .as_ref()
                    .map(|path| path.0.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(paths, ["other.rs", "src/lib.rs", "src/nested/deep.rs"]);
        Ok(())
    }

    #[test]
    fn search_order_identity_sorts_by_the_hits_own_identity() -> TestResult {
        let (_directory, service) = multi_file_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "identity",
            "limit": 10
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        let ids = hit_identities(&result);
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "identity order sorts purely by each hit's own id, independent of score"
        );
        Ok(())
    }

    /// Two symbols in one file tie on `order: "path"`; the tie breaks on identity, and that
    /// relative order survives whether the page is cut at `limit: 1` or `limit: 2`.
    #[test]
    fn search_order_path_keeps_tied_hits_in_the_same_relative_order_across_page_sizes() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon_alpha() {}\npub fn beacon_beta() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let mut narrow_ids = Vec::new();
        for page_index in 0..2_u64 {
            let request = json!({
                "query": "beacon",
                "target": "symbol",
                "order": "path",
                "limit": 1,
                "page_index": page_index
            });
            let params: SearchParams = serde_json::from_value(request)?;
            let result = service.search(&params, &StoreAnswer::identifier_only())?;
            narrow_ids.extend(hit_identities(&result));
        }
        let wide: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "target": "symbol",
            "order": "path",
            "limit": 2
        }))?;
        let wide_ids = hit_identities(&service.search(&wide, &StoreAnswer::identifier_only())?);
        assert_eq!(
            narrow_ids, wide_ids,
            "tied hits must keep their relative order across page sizes"
        );
        assert_eq!(
            wide_ids,
            [
                "rift://symbol/rust/src/lib.rs/beacon_alpha",
                "rift://symbol/rust/src/lib.rs/beacon_beta"
            ]
        );
        Ok(())
    }

    #[test]
    fn search_force_include_reaches_excluded_files_with_correct_path_and_span() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "phantom",
            "target": "symbol",
            "paths": {"force_include": ["gitignored.rs", "configured_out.rs"]}
        }))?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 2);
        for hit in results {
            assert!(hit["range"]["start"].is_u64());
            let id = hit["hit"]["symbol"]["id"]
                .as_str()
                .ok_or("force_include hit must carry a symbol id")?;
            assert!(id.contains("gitignored.rs") || id.contains("configured_out.rs"));
        }
        Ok(())
    }

    #[test]
    fn search_force_include_of_indexed_file_does_not_duplicate_hits() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "visible_symbol",
            "target": "symbol",
            "paths": {"force_include": ["visible.rs"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(
            result.results.len(),
            1,
            "one declaration matched through two indexes stays one identity: {result:#?}"
        );
        Ok(())
    }

    #[test]
    fn search_force_include_hard_floor_unreachable() -> TestResult {
        let (_directory, service) = force_include_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "floor",
            "paths": {"force_include": [".git/**"]}
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(
            result.results.is_empty(),
            "the hard floor must stay unreachable via force_include"
        );
        Ok(())
    }

    /// A force-included file holding a declaration the Contribution contract refuses is
    /// left out of the on-demand index, and the answer names it in `source_unavailable`
    /// instead of failing the request.
    #[test]
    fn search_force_include_leaves_out_a_file_the_contract_refuses_and_warns() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "wide.rs\n")?;
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n")?;
        fs::write(
            directory.path().join("wide.rs"),
            format!(
                "pub struct {};\n",
                "S".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX)
            ),
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let plain: SearchParams = serde_json::from_value(json!({"query": "kept"}))?;
        let plain_answer = service.search(&plain, &StoreAnswer::identifier_only())?;
        let plain_value = serde_json::to_value(plain_answer)?;
        let names_nothing = plain_value["warnings"].as_array().is_none_or(|warnings| {
            warnings
                .iter()
                .all(|warning| warning["code"] != "source_unavailable")
        });
        assert!(
            names_nothing,
            "the walk never reaches the ignored file: {plain_value:#}"
        );
        let params: SearchParams = serde_json::from_value(json!({
            "query": "kept",
            "paths": {"force_include": ["wide.rs"]}
        }))?;
        let answer = service.search(&params, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        assert!(
            value["results"]
                .as_array()
                .is_some_and(|results| results.iter().any(|hit| hit["path"] == "lib.rs")),
            "{value:#}"
        );
        let names_wide = value["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "source_unavailable"
                    && warning["unit"] == "rift://file/wide.rs"
                    && warning["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("provider_symbol"))
            })
        });
        assert!(names_wide, "{value:#}");
        Ok(())
    }

    /// A `force_include` past its file bound refuses through the index fault, whose
    /// evidence names the field, the bound, and the match count.
    #[test]
    fn search_force_include_bound_breach_refuses() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "extra_*.rs\n")?;
        fs::write(directory.path().join("lib.rs"), "pub fn kept() {}\n")?;
        for index in 0..=super::FORCE_INCLUDE_FILES_MAX {
            fs::write(
                directory.path().join(format!("extra_{index:04}.rs")),
                "pub fn extra() {}\n",
            )?;
        }
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "extra",
            "paths": {"force_include": ["extra_*.rs"]}
        }))?;
        let error = service
            .search(&params, &StoreAnswer::identifier_only())
            .expect_err("a force_include match count above the bound must refuse");
        assert!(matches!(error.fault(), ReadFault::Index(_)));
        assert_eq!(
            error.fault().limit_evidence().map(|evidence| (
                evidence.field,
                evidence.limit,
                evidence.required
            )),
            Some((
                "paths.force_include".to_owned(),
                super::FORCE_INCLUDE_FILES_MAX as u64,
                super::FORCE_INCLUDE_FILES_MAX as u64 + 1
            )),
            "the refusal carries the bound and the match count"
        );
        Ok(())
    }

    /// A `force_include` within its bound is built even when the persistent index alone
    /// fills `results_max`: the request still answers, and its warnings are the ones the
    /// same query carries without the selector.
    #[test]
    fn search_force_include_within_bound_beside_a_full_index_still_answers() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".gitignore"), "gitignored.rs\n")?;
        fs::write(
            directory.path().join("visible.rs"),
            "pub fn visible_symbol() {}\n",
        )?;
        fs::write(
            directory.path().join("gitignored.rs"),
            "pub fn phantom_gitignored() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::new(10, 4_096, 8_192, 8, 1)?,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let plain: SearchParams = serde_json::from_value(json!({ "query": "visible" }))?;
        let reaching: SearchParams = serde_json::from_value(json!({
            "query": "visible",
            "paths": {"force_include": ["gitignored.rs"]}
        }))?;
        let without = service.search(&plain, &StoreAnswer::identifier_only())?;
        let with = service.search(&reaching, &StoreAnswer::identifier_only())?;
        assert_eq!(with.results.len(), 1, "{:#?}", with.results);
        assert_eq!(with.results[0].path, without.results[0].path);
        assert_eq!(with.warnings, without.warnings);
        Ok(())
    }

    #[test]
    fn search_force_include_invalid_glob_refuses() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "paths": {"force_include": ["["]}
        }))?;
        assert!(matches!(
            service
                .search(&params, &StoreAnswer::identifier_only())
                .expect_err("an invalid force_include glob must refuse")
                .fault(),
            ReadFault::Index(_)
        ));
        Ok(())
    }

    /// One committed source file, then uncommitted drift, so a revision
    /// search and a working-tree search answer differently.
    fn committed_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn committed_probe() {}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce probe");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn drifted_probe() {}\n",
        )?;
        let service = ReadService::at_revision(
            directory.path(),
            &rift_protocol::read::RevisionId("main".to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn search_at_a_revision_serves_committed_matches_only() -> TestResult {
        let (_directory, service) = committed_fixture()?;
        let committed: SearchParams = serde_json::from_value(json!({"query": "committed_probe"}))?;
        let answer = service.search(&committed, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        let results = value["results"].as_array().ok_or("results array")?;
        assert!(!results.is_empty(), "the committed declaration matches");
        assert!(
            value.get("warnings").is_none(),
            "a search served from one index warns nothing, so warnings is omitted"
        );
        let drifted: SearchParams = serde_json::from_value(json!({"query": "drifted_probe"}))?;
        let drifted_answer = service.search(&drifted, &StoreAnswer::identifier_only())?;
        let drifted_value = serde_json::to_value(drifted_answer)?;
        assert_eq!(
            drifted_value["results"].as_array().map(Vec::len),
            Some(0),
            "uncommitted drift is invisible at the revision"
        );
        Ok(())
    }

    /// A committed file the syntax provider refuses under its depth bound is absent from
    /// the revision index, so a revision search still answers from the file beside it.
    #[test]
    fn search_at_a_revision_leaves_out_a_file_past_a_syntax_bound_and_serves_the_rest() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        let committed = "pub fn committed_probe() {}\n";
        fs::write(directory.path().join("lib.rs"), committed)?;
        let deep = format!(
            "pub fn deep_probe() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(600),
            close = ")".repeat(600),
        );
        fs::write(directory.path().join("deep.rs"), deep)?;
        rift_history::fixture::commit_all(
            directory.path(),
            "introduce a probe beside a refused file",
        );
        let root = directory.path();
        let revision = rift_protocol::read::RevisionId("HEAD".to_owned());
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let history = HistoryConfiguration::default();
        let service = ReadService::at_revision(root, &revision, limits, &visibility, history)?;
        let committed: SearchParams =
            serde_json::from_value(json!({"query": "committed_probe", "rev": "HEAD"}))?;
        let answer = service.search(&committed, &StoreAnswer::identifier_only())?;
        let value = serde_json::to_value(answer)?;
        assert!(
            value["results"]
                .as_array()
                .is_some_and(|results| !results.is_empty()),
            "{value:#}"
        );
        let names_deep = value["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "source_unavailable" && warning["unit"] == "rift://file/deep.rs"
            })
        });
        assert!(names_deep, "{value:#}");
        let refused: SearchParams =
            serde_json::from_value(json!({"query": "deep_probe", "rev": "HEAD"}))?;
        let refused_answer = service.search(&refused, &StoreAnswer::identifier_only())?;
        let refused_value = serde_json::to_value(refused_answer)?;
        assert_eq!(
            refused_value["results"].as_array().map(Vec::len),
            Some(0),
            "{refused_value:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_finds_every_explicitly_included_utf8_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let files = [
            ("history.lua", "lua_catalog_marker"),
            ("worker.go", "go_catalog_marker"),
            ("notes.unknown", "unknown_catalog_marker"),
            ("buildfile", "extensionless_catalog_marker"),
        ];
        for (path, marker) in files {
            fs::write(directory.path().join(path), marker)?;
        }
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;
        let store = published_store(&directory.path().join("search.db"), &service).await?;

        for (path, marker) in files {
            let answer = store_answer(&store, service.tree_revision(), marker).await?;
            let params: SearchParams = serde_json::from_value(json!({
                "query": marker,
                "target": "file",
                "paths": { "include": [path] },
                "limit": 10
            }))?;
            let value = serde_json::to_value(service.search(&params, &answer)?)?;
            assert_eq!(
                value["results"].as_array().map(Vec::len),
                Some(1),
                "one exact visible file must match: path={path}, value={value:#}"
            );
            assert_eq!(value["results"][0]["path"], json!(path));
        }
        Ok(())
    }

    /// A provider-claimed file contributes one text document beside its declarations, so
    /// its content answers one file hit rather than one per declaration.
    #[tokio::test]
    async fn search_indexes_provider_file_content_once() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("calls.rs"),
            "fn caller() { wire_symbol(alpha, beta); }\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let database = directory.path().join("search.db");
        let store = answered(&database, &service, "wire symbol beta").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "wire symbol beta",
            "target": "file",
            "limit": 10
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        let results = value["results"].as_array().ok_or("results array")?;
        assert_eq!(
            results.len(),
            1,
            "provider content must have one file identity: {results:#?}"
        );
        assert_eq!(results[0]["path"], json!("calls.rs"));
        let documents = service.index_documents();
        assert_eq!(
            documents
                .iter()
                .filter(|document| document.kind() == DocumentKind::TextFile
                    && document.identity().as_str() == "calls.rs")
                .count(),
            1,
            "provider content must join the index documents once: {documents:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn revision_search_finds_visible_text_without_a_provider() -> TestResult {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(
            directory.path().join("rift_history.py"),
            "RIFT_HISTORY_PYTHON_MARKER\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "add history fixture");
        let service = ReadService::at_revision_with_languages(
            directory.path(),
            &rift_protocol::read::RevisionId("main".to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            &rift_core::LanguageFileSelections::default(),
            HistoryConfiguration::default(),
        )?;
        let store = answered(
            &directory.path().join("search.db"),
            &service,
            "RIFT_HISTORY_PYTHON_MARKER",
        )
        .await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "RIFT_HISTORY_PYTHON_MARKER",
            "target": "file",
            "paths": { "include": ["rift_history.py"] }
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        assert_eq!(value["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["results"][0]["path"], json!("rift_history.py"));
        Ok(())
    }

    #[tokio::test]
    async fn binary_invalid_and_oversized_unknown_files_do_not_hide_valid_text() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("visible.py"), "VISIBLE_TEXT_MARKER\n")?;
        fs::write(directory.path().join("binary.unknown"), b"binary\0payload")?;
        fs::write(directory.path().join("invalid.unknown"), [0xff, 0xfe])?;
        fs::write(directory.path().join("oversized.unknown"), vec![b'x'; 33])?;
        let limits = WorkspaceIndexLimits::new(10, 32, 1_024, 8, 10)?;
        let service = ReadService::build(
            directory.path(),
            limits,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1 << 20),
            HistoryConfiguration::default(),
        )?;
        let store = answered(
            &directory.path().join("search.db"),
            &service,
            "VISIBLE_TEXT_MARKER",
        )
        .await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "VISIBLE_TEXT_MARKER",
            "target": "file"
        }))?;
        let value = serde_json::to_value(service.search(&params, &store)?)?;
        assert_eq!(value["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["results"][0]["path"], json!("visible.py"));
        Ok(())
    }

    #[test]
    fn search_at_a_revision_refuses_force_include() -> TestResult {
        let (_directory, service) = committed_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "committed_probe",
            "paths": {"force_include": ["lib.rs"]}
        }))?;
        let error = service
            .search(&params, &StoreAnswer::identifier_only())
            .expect_err(
                "force_include walks the working tree, which a revision search has none of",
            );
        assert!(matches!(
            error.fault(),
            ReadFault::Unsupported { capability }
            if capability == "force_include at a revision"
        ));
        Ok(())
    }

    /// One workspace holding neither the fixture's declarations nor its text file, so every
    /// address the fixture ranked is one this index cannot resolve.
    fn unrelated_workspace() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("other.rs"), "pub fn unrelated() {}\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text_inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let root = directory.path();
        let service = ReadService::build(root, limits, &visibility, &text_inclusion, history)?;
        Ok((directory, service))
    }

    #[test]
    fn described_units_pair_every_symbol_and_no_text_file_with_its_own_declaration() -> TestResult {
        let (_directory, service) = fixture()?;
        let documents = service.index_documents();
        let described = service.described_units(&documents);
        let symbols = documents
            .iter()
            .filter(|document| document.kind() == DocumentKind::Symbol)
            .count();
        let texts = documents
            .iter()
            .filter(|document| document.kind() == DocumentKind::TextFile)
            .count();
        assert!(
            texts > 0,
            "the fixture must contribute a text-file document"
        );
        assert_eq!(
            described.len(),
            symbols,
            "every symbol document is described and no text-file document is"
        );
        for one in &described {
            assert_eq!(
                one.unit().kind(),
                DocumentKind::Symbol,
                "a text-file document must never be described"
            );
            let identity = one.unit().identity();
            let text = rift_search::document(one.declaration()).into_text();
            assert!(
                text.contains(one.unit().content()),
                "each description must carry its own document's declaration: {identity} {text}"
            );
        }
        Ok(())
    }

    #[test]
    fn ranked_symbol_resolution_preserves_encoded_names_and_exact_addresses() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source =
            r#"{"js/bun": 1, "js%2Fbun": 2, "café/test": 3, "duplicate": 4, "duplicate": 5}"#;
        fs::write(directory.path().join("names space.json"), source)?;
        fs::write(directory.path().join("other.json"), source)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let path = rift_core::ProjectPath::new("names space.json")?;
        let file = service.index().file(&path).ok_or("fixture file missing")?;
        let mut names = Vec::new();
        for symbol in file.syntax().symbols() {
            let identity =
                rift_core::symbol_identity("json", path.as_str(), &symbol.qualified_name);
            let (resolved_file, resolved_symbol) =
                super::resolve_symbol(service.index(), &path, &identity)
                    .ok_or("minted identity must resolve")?;
            assert_eq!(resolved_file.path(), &path);
            assert_eq!(resolved_symbol.range, symbol.range);
            names.push(resolved_symbol.qualified_name.as_str());
            for wrong in [
                identity.replace("/json/", "/yaml/"),
                identity.replace("names%20space.json", "other.json"),
                identity.replace("names%20space.json", "names space.json"),
                format!("{identity}%"),
            ] {
                assert!(
                    super::resolve_symbol(service.index(), &path, &wrong).is_none(),
                    "a different or noncanonical identity must not resolve: {wrong}"
                );
            }
        }
        assert_eq!(
            names,
            [
                "js/bun",
                "js%2Fbun",
                "café/test",
                "duplicate~1",
                "duplicate~2"
            ]
        );
        for refused in [
            "not a symbol address",
            "rift://symbol/json/names%20space.json/missing",
            "rift://symbol/json/names%20space.json/js%2fbun",
            "rift://symbol/json/names%20space.json/caf%C3%A9%2ftest",
        ] {
            assert!(
                super::resolve_symbol(service.index(), &path, refused).is_none(),
                "unresolved or noncanonical identity must stay absent: {refused}"
            );
        }
        Ok(())
    }

    /// A fused identity naming a declaration this snapshot has moved past is skipped, so
    /// the request answers instead of failing.
    #[test]
    fn resolve_ranked_hits_skips_a_declaration_identity_no_selected_index_holds() -> TestResult {
        let (_other, unrelated) = unrelated_workspace()?;
        let input = lexical_input(vec![(
            declaration_identity("src/lib.rs", "Beacon")?,
            FieldSet::of(SearchableField::Name),
        )]);
        let criteria = SearchCriteria {
            query: &ParsedQuery::parse("Beacon")?,
            target: SearchParamsTarget::Symbol,
            payloads: HitPayloads::default(),
        };
        let resolution = Resolution {
            force_include: None,
            packages: None,
        };
        let results = resolved_hits(&unrelated, &[input], criteria, resolution)?;
        assert!(
            results.is_empty(),
            "a declaration identity absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    /// The same skip for a file path: the store ranked a publication this snapshot has
    /// already moved past.
    #[test]
    fn resolve_ranked_hits_skips_a_file_path_no_selected_index_holds() -> TestResult {
        let (_other, unrelated) = unrelated_workspace()?;
        let input = lexical_input(vec![(
            DocumentIdentity::new("README.txt")?,
            FieldSet::of(SearchableField::FileContent),
        )]);
        let criteria = SearchCriteria {
            query: &ParsedQuery::parse("Beacon")?,
            target: SearchParamsTarget::File,
            payloads: HitPayloads::default(),
        };
        let resolution = Resolution {
            force_include: None,
            packages: None,
        };
        let results = resolved_hits(&unrelated, &[input], criteria, resolution)?;
        assert!(
            results.is_empty(),
            "a file path absent from the index must be skipped silently: {results:#?}"
        );
        Ok(())
    }

    /// A package declaration resolves back through its source unit and qualified name; a
    /// project declaration resolves through its `rift://symbol/` address.
    #[test]
    fn resolve_ranked_hits_resolves_a_package_unit_and_a_project_declaration() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        let packages = service
            .dependency_index(SearchScope::Global)?
            .ok_or("the fixture must attach a package index")?;
        let input = lexical_input(vec![
            (
                declaration_identity("src/lib.rs", "beacon")?,
                FieldSet::of(SearchableField::Name),
            ),
            (
                DocumentIdentity::new(format!("{}#helper_beacon", helper_unit().0))?,
                FieldSet::of(SearchableField::QualifiedName),
            ),
        ]);
        let criteria = SearchCriteria {
            query: &ParsedQuery::parse("beacon")?,
            target: SearchParamsTarget::Symbol,
            payloads: HitPayloads::default(),
        };
        let resolution = Resolution {
            force_include: None,
            packages: Some(&packages),
        };
        let results = resolved_hits(&service, &[input], criteria, resolution)?;
        assert_eq!(results.len(), 2, "{results:#?}");
        assert_eq!(
            results[0].path,
            Some(rift_protocol::read::ProjectPath("src/lib.rs".to_owned()))
        );
        assert_eq!(results[0].unit, None);
        assert_eq!(results[1].unit, Some(helper_unit()));
        assert_eq!(results[1].path, None);
        Ok(())
    }

    /// `matched_by` names the columns that placed a candidate: the three name columns
    /// collapse to one member, the two content columns to another, and a candidate no
    /// column placed - one the vector ranking alone reached - answers `ranked`.
    #[test]
    fn matched_by_names_the_columns_that_placed_a_candidate() -> TestResult {
        let (_directory, service) = fixture()?;
        let resolution = Resolution {
            force_include: None,
            packages: None,
        };
        let every_column = lexical_input(vec![(
            declaration_identity("src/lib.rs", "Beacon")?,
            FieldSet::EMPTY
                .with(SearchableField::Name)
                .with(SearchableField::QualifiedName)
                .with(SearchableField::IdentifierTerms)
                .with(SearchableField::Signature)
                .with(SearchableField::Documentation)
                .with(SearchableField::DeclarationSource),
        )]);
        let declarations = SearchCriteria {
            query: &ParsedQuery::parse("Beacon")?,
            target: SearchParamsTarget::Symbol,
            payloads: HitPayloads::default(),
        };
        let placed = resolved_hits(&service, &[every_column], declarations, resolution)?;
        assert_eq!(
            placed.first().map(|hit| hit.matched_by.clone()),
            Some(vec![
                MatchedField::Name,
                MatchedField::Signature,
                MatchedField::Documentation,
                MatchedField::Content,
            ]),
            "{placed:#?}"
        );

        let files = SearchCriteria {
            query: &ParsedQuery::parse("Beacon")?,
            target: SearchParamsTarget::File,
            payloads: HitPayloads::default(),
        };
        let content = lexical_input(vec![(
            DocumentIdentity::new("README.txt")?,
            FieldSet::of(SearchableField::FileContent),
        )]);
        let by_content = resolved_hits(&service, &[content], files, resolution)?;
        assert_eq!(
            by_content.first().map(|hit| hit.matched_by.clone()),
            Some(vec![MatchedField::Content]),
            "{by_content:#?}"
        );

        let ranked_alone = ranked_input(
            RankingInputKind::Vector,
            vec![(DocumentIdentity::new("README.txt")?, FieldSet::EMPTY)],
        );
        let by_vector = resolved_hits(&service, &[ranked_alone], files, resolution)?;
        assert_eq!(
            by_vector.first().map(|hit| hit.matched_by.clone()),
            Some(vec![MatchedField::Ranked]),
            "a candidate no column placed answers ranked: {by_vector:#?}"
        );
        Ok(())
    }

    /// One declaration both the identifier ranking and the store's full-text ranking
    /// placed answers once, carrying every column that placed it.
    #[tokio::test]
    async fn search_matched_by_carries_the_identifier_and_the_full_text_columns() -> TestResult {
        let (directory, service) = fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "Beacon").await?;
        let params: SearchParams = serde_json::from_value(json!({"query": "Beacon", "limit": 50}))?;
        let answer = service.search(&params, &store)?;

        let declarations: Vec<&SearchHit> = answer
            .results
            .iter()
            .filter(|hit| {
                matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "Beacon")
            })
            .collect();
        assert_eq!(
            declarations.len(),
            1,
            "two inputs placing one identity answer once: {answer:#?}"
        );
        let beacon = declarations[0];
        assert!(
            beacon.matched_by.contains(&MatchedField::Name)
                && beacon.matched_by.contains(&MatchedField::Content),
            "the identifier ranking's name column and the store's source column both \
             placed it: {beacon:#?}"
        );

        let readme = answer
            .results
            .iter()
            .find(|hit| {
                hit.path
                    .as_ref()
                    .is_some_and(|path| path.0.as_str() == "README.txt")
            })
            .ok_or("the text file must reach the answer")?;
        assert_eq!(
            readme.matched_by,
            [MatchedField::Content],
            "the file content column alone placed it: {readme:#?}"
        );
        Ok(())
    }

    /// The broad phase joins only what the precise phase left out, and its identities
    /// append after every precise one: `both.txt` carries all the query's terms, and the
    /// two files carrying one term each follow it.
    #[tokio::test]
    async fn search_appends_the_broad_phase_after_every_precise_identity() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("alpha.txt"), "alpha lantern\n")?;
        fs::write(directory.path().join("beta.txt"), "beta lantern\n")?;
        fs::write(directory.path().join("both.txt"), "alpha beta\n")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let store = answered(&directory.path().join("search.db"), &service, "alpha beta").await?;
        assert!(
            !store.broad().is_empty(),
            "a two-term query has a broad phase"
        );
        let params: SearchParams = serde_json::from_value(json!({
            "query": "alpha beta",
            "target": "file",
            "limit": 10
        }))?;
        let result = service.search(&params, &store)?;
        let mut identities = hit_identities(&result);
        assert_eq!(
            identities.first().map(String::as_str),
            Some("both.txt"),
            "the precise identity leads: {result:#?}"
        );
        identities.sort();
        assert_eq!(
            identities,
            ["alpha.txt", "beta.txt", "both.txt"],
            "the broad phase appends what the precise phase left out, once: {result:#?}"
        );
        Ok(())
    }

    /// A text file past `[search.text].max_chunk` contributes one document per chunk, and
    /// every chunk identity resolves to the same file, so the answer carries one hit for
    /// it rather than one per chunk.
    #[tokio::test]
    async fn text_file_chunks_collapse_to_one_hit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.txt"), "word ".repeat(1000))?;
        // The smallest accepted chunk bound against a several-kilobyte guide forces the
        // file into more than one document, each of which the query matches.
        let text_inclusion = rift_core::TextFileInclusion::new(vec!["**".to_owned()], 1_024);
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &text_inclusion,
            HistoryConfiguration::default(),
        )?;
        let documents = service.index_documents();
        let chunks = documents
            .iter()
            .filter(|document| document.kind() == DocumentKind::TextFile)
            .count();
        assert!(
            chunks > 1,
            "the oversized guide must split into more than one document: {chunks}"
        );
        let store = answered(&directory.path().join("search.db"), &service, "word").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "word",
            "target": "file",
            "limit": 50
        }))?;
        let result = service.search(&params, &store)?;
        assert_eq!(
            hit_identities(&result),
            ["guide.txt"],
            "the chunk documents of one file must collapse to one hit: {result:#?}"
        );
        Ok(())
    }

    /// Every identity the store ranked reaches the answer, each carrying the score its
    /// place in the fused order derived.
    #[tokio::test]
    async fn search_answers_every_identity_the_store_ranked() -> TestResult {
        let (directory, service) = fixture()?;
        let store = answered(&directory.path().join("search.db"), &service, "Beacon").await?;
        let params: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "include": ["score"], "limit": 50}))?;
        let answer = service.search(&params, &store)?;
        let identities = hit_identities(&answer);
        for input in store.precise() {
            for entry in input.order() {
                assert!(
                    identities
                        .iter()
                        .any(|held| held == entry.identity().as_str()),
                    "every ranked identity must reach the answer: identity={}, answer={answer:#?}",
                    entry.identity()
                );
            }
        }
        assert!(
            answer
                .results
                .iter()
                .all(|hit| hit.score.is_some_and(|score| score > 0.0 && score <= 1.0)),
            "a fused score reaches the wire when requested, inside 0 to 1: {answer:#?}"
        );
        Ok(())
    }

    /// The only file holding the query's content sits at an excluded path; `paths.exclude`
    /// narrows the store's order exactly as it narrows the identifier ranking, so the
    /// answer is empty rather than leaking the excluded file's hit.
    #[tokio::test]
    async fn search_paths_exclude_narrows_the_store_order_to_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("secret.txt"), "lighthouse guidance")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let store = answered(&directory.path().join("search.db"), &service, "lighthouse").await?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lighthouse",
            "paths": {"exclude": ["secret.txt"]}
        }))?;
        let answer = service.search(&params, &store)?;
        assert!(
            answer.results.is_empty(),
            "an excluded path's only ranked match must not reach the answer: {answer:#?}"
        );
        Ok(())
    }

    /// Slices `content` at `range`'s byte offsets, for tests proving a returned span
    /// addresses exactly the bytes its accompanying text carries.
    fn byte_slice(content: &str, range: ByteRange) -> &str {
        let start = usize::try_from(range.start).expect("test fixture bytes must fit usize");
        let end = usize::try_from(range.end).expect("test fixture bytes must fit usize");
        &content[start..end]
    }

    #[test]
    fn locate_query_line_finds_first_matching_line_case_insensitively() {
        let content = "intro line\nSEARCH replace ALL units here\nend line\n";
        let (line_number, range, text) = super::locate_query_line(
            content,
            &ParsedQuery::parse("replace all").expect("the fixture query parses"),
        );
        assert_eq!(line_number, 2);
        assert_eq!(text, "SEARCH replace ALL units here");
        assert_eq!(
            byte_slice(content, range),
            text,
            "the returned span must address the exact matched line's bytes"
        );
    }

    #[test]
    fn locate_query_line_reports_byte_exact_spans_in_a_crlf_file() {
        let content = "one\r\ntwo replace\r\nthree\r\n";
        let (line_number, range, text) = super::locate_query_line(
            content,
            &ParsedQuery::parse("replace").expect("the fixture query parses"),
        );
        assert_eq!(line_number, 2);
        assert_eq!(text, "two replace");
        assert_eq!(byte_slice(content, range), "two replace");
    }

    #[test]
    fn locate_query_line_reads_a_quoted_phrase_as_one_phrase() {
        // The caller's raw text carries the quotes; the parsed member does not. Splitting
        // the raw text would leave `"impact` and `radius"` and match no line at all.
        let content = "one line\nthe impact radius of one change\nlast line\n";
        let (line_number, _range, text) = super::locate_query_line(
            content,
            &ParsedQuery::parse("\"impact radius\"").expect("the fixture query parses"),
        );
        assert_eq!(line_number, 2);
        assert_eq!(text, "the impact radius of one change");
    }

    #[test]
    fn locate_query_line_falls_back_to_a_whole_file_span_without_a_term_match() {
        let content = "alpha\nbeta\n";
        let (line_number, range, text) = super::locate_query_line(
            content,
            &ParsedQuery::parse("gamma").expect("the fixture query parses"),
        );
        assert_eq!(line_number, 1);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, content.len() as u64);
        assert_eq!(text, content);
    }

    fn file_hit_stub(path: &str, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::File {
                size: 0,
                languages: Vec::new(),
            },
            score: Some(score),
            matched_by: vec![MatchedField::Content],
            source: None,
            range: None,
            line: None,
            path: Some(rift_protocol::read::ProjectPath(path.to_owned())),
            unit: None,
            traversal_path: None,
            distance: None,
            change: None,
        }
    }

    #[test]
    fn order_hits_relevance_sorts_by_score_then_a_deterministic_identity_tie_break() {
        let mut first_arrival = vec![
            file_hit_stub("rift://file/z.rs", 0.5),
            file_hit_stub("rift://file/b.rs", 0.9),
            file_hit_stub("rift://file/a.rs", 0.9),
        ];
        super::order_hits(&mut first_arrival, ResultOrder::Relevance);
        let ordered_ids: Vec<&str> = first_arrival.iter().map(super::hit_identity).collect();
        assert_eq!(
            ordered_ids,
            ["rift://file/a.rs", "rift://file/b.rs", "rift://file/z.rs"],
            "higher score sorts first; a tie breaks on ascending wire identity"
        );

        // The same three hits arriving in a different order sort to the identical result:
        // the tie-break is independent of arrival order, not merely input-stable.
        let mut second_arrival = vec![
            file_hit_stub("rift://file/b.rs", 0.9),
            file_hit_stub("rift://file/z.rs", 0.5),
            file_hit_stub("rift://file/a.rs", 0.9),
        ];
        super::order_hits(&mut second_arrival, ResultOrder::Relevance);
        let reordered_ids: Vec<&str> = second_arrival.iter().map(super::hit_identity).collect();
        assert_eq!(reordered_ids, ordered_ids);
    }

    /// `order: "path"` groups by project path, breaking a tie - two hits at the same path -
    /// on the hit's own wire identity, the same tie-break `relevance` uses. A file hit's
    /// own identity is its `path`, so two file hits can never tie this way; a node hit's
    /// identity is its own witnessed id, distinct from the path it also carries, so this
    /// proves the tie-break through node hits instead.
    #[test]
    fn order_hits_path_sorts_by_path_then_breaks_ties_on_identity() {
        let mut hits = vec![
            node_hit_stub_with_path("rift://node/rust/z.rs@0-1#00000000", Some("z.rs"), 0.9),
            node_hit_stub_with_path("rift://node/rust/a.rs@0-1#00000002", Some("a.rs"), 0.1),
            node_hit_stub_with_path("rift://node/rust/a.rs@0-1#00000001", Some("a.rs"), 0.5),
        ];
        super::order_hits(&mut hits, ResultOrder::Path);
        let ids: Vec<&str> = hits.iter().map(super::hit_identity).collect();
        assert_eq!(
            ids,
            [
                "rift://node/rust/a.rs@0-1#00000001",
                "rift://node/rust/a.rs@0-1#00000002",
                "rift://node/rust/z.rs@0-1#00000000",
            ],
            "path order groups by path; two hits at the same path break the tie on identity"
        );
    }

    /// `order: "identity"` sorts by each hit's own wire id alone, ignoring score.
    #[test]
    fn order_hits_identity_sorts_by_the_hits_own_identity_alone() {
        let mut hits = vec![
            file_hit_stub("rift://file/z.rs", 0.9),
            file_hit_stub("rift://file/a.rs", 0.1),
        ];
        super::order_hits(&mut hits, ResultOrder::Identity);
        let ids: Vec<&str> = hits.iter().map(super::hit_identity).collect();
        assert_eq!(ids, ["rift://file/a.rs", "rift://file/z.rs"]);
    }

    fn node_hit_stub(id: &str, score: f64) -> SearchHit {
        node_hit_stub_with_path(id, None, score)
    }

    fn node_hit_stub_with_path(id: &str, path: Option<&str>, score: f64) -> SearchHit {
        SearchHit {
            hit: SearchHitTarget::Node {
                node: NodeId(id.to_owned()),
            },
            score: Some(score),
            matched_by: vec![MatchedField::Content],
            source: None,
            range: None,
            line: None,
            path: path.map(|path| rift_protocol::read::ProjectPath(path.to_owned())),
            unit: None,
            traversal_path: None,
            distance: None,
            change: None,
        }
    }

    /// A node hit addressed by `unit` alone, the location shape a dependency hit takes.
    fn node_hit_stub_with_unit(id: &str, unit: &str, score: f64) -> SearchHit {
        let mut hit = node_hit_stub(id, score);
        hit.unit = Some(SourceUnitId(unit.to_owned()));
        hit
    }

    /// `hit_identity`'s `Node` arm never runs through the live `search` path - a `target:
    /// "node"` request is refused before any hit is ever built - so this proves the arm
    /// directly: a `Node` hit tied in score with a `File` hit still breaks the tie on the
    /// node's own wire id, exactly as the `File` and `Symbol` arms already do.
    #[test]
    fn hit_identity_uses_the_node_id_as_tiebreak() {
        let mut results = vec![
            file_hit_stub("rift://file/z.rs", 0.5),
            node_hit_stub("rift://node/rust/lib.rs@0-1#00000000", 0.5),
        ];
        super::order_hits(&mut results, ResultOrder::Relevance);
        let ids: Vec<&str> = results.iter().map(super::hit_identity).collect();
        // "rift://file/..." sorts before "rift://node/..." lexicographically ('f' < 'n').
        assert_eq!(
            ids,
            ["rift://file/z.rs", "rift://node/rust/lib.rs@0-1#00000000",]
        );
    }

    /// `path` order lists every project path before any unit, then the units in their own
    /// order, then identity.
    #[test]
    fn order_hits_path_lists_project_paths_before_units_and_units_in_unit_order() {
        let mut results = vec![
            node_hit_stub_with_unit(
                "rift://node/rust/lib.rs@0-1#00000000",
                "rift://source/cargo/zeta@1.0.0/src/lib.rs",
                0.5,
            ),
            file_hit_stub("z.rs", 0.5),
            node_hit_stub_with_unit(
                "rift://node/rust/lib.rs@2-3#00000000",
                "rift://source/cargo/alpha@1.0.0/src/lib.rs",
                0.5,
            ),
        ];
        super::order_hits(&mut results, ResultOrder::Path);
        let located: Vec<(Option<&str>, Option<&str>)> = results
            .iter()
            .map(|hit| {
                (
                    hit.path.as_ref().map(|path| path.0.as_str()),
                    hit.unit.as_ref().map(|unit| unit.0.as_str()),
                )
            })
            .collect();
        assert_eq!(
            located,
            [
                (Some("z.rs"), None),
                (None, Some("rift://source/cargo/alpha@1.0.0/src/lib.rs")),
                (None, Some("rift://source/cargo/zeta@1.0.0/src/lib.rs")),
            ]
        );
    }

    /// The project `beacon` beside a store holding the built helper package: the fixture
    /// `get_symbol`'s scope tests share.
    fn dependency_fixture() -> TestResult<(TempDir, ReadService)> {
        let (directory, service) = project_fixture("pub fn beacon() {}\n")?;
        Ok((directory, service.with_packages(helper_store()?)))
    }

    /// Each hit's declaration name and whether it is addressed by `unit`, in answer order.
    fn located_names(result: &SearchResult) -> Vec<(String, bool)> {
        result
            .results
            .iter()
            .map(|hit| {
                let SearchHitTarget::Symbol { symbol } = &hit.hit else {
                    panic!("a symbol hit was expected: {hit:?}");
                };
                (symbol.name.clone(), hit.unit.is_some())
            })
            .collect()
    }

    /// An omitted `scope` runs the project index alone: the helper's declaration does not
    /// answer, the project `beacon` answers by path, and no dependency warning rides.
    #[test]
    fn search_default_scope_answers_the_project_alone() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({"query": "helper_beacon"}))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(result.results.is_empty(), "{:?}", result.results);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);

        let params: SearchParams =
            serde_json::from_value(json!({"query": "beacon", "target": "symbol"}))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert_eq!(located_names(&result), [("beacon".to_owned(), false)]);
        assert_eq!(
            result.results[0].path,
            Some(rift_protocol::read::ProjectPath("src/lib.rs".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn search_global_scope_answers_the_helper_declaration_by_unit() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "helper_beacon",
            "scope": "global",
            "include": ["source", "score"]
        }))?;

        let result = service.search(&params, &StoreAnswer::identifier_only())?;

        assert_eq!(result.results.len(), 1, "{:?}", result.results);
        let hit = &result.results[0];
        assert_eq!(hit.unit, Some(helper_unit()));
        assert_eq!(hit.path, None);
        assert_eq!(hit.matched_by, [MatchedField::Name]);
        assert_eq!(
            hit.score,
            Some(1.0),
            "the only candidate leads the fused order"
        );
        assert_eq!(hit.line, Some(1));
        assert!(
            hit.source
                .as_deref()
                .is_some_and(|source| source.contains("pub fn helper_beacon")),
            "{hit:?}"
        );
        assert!(hit.traversal_path.is_none() && hit.distance.is_none());
        let SearchHitTarget::Symbol { symbol } = &hit.hit else {
            panic!("a symbol hit was expected: {hit:?}");
        };
        assert_eq!(symbol.name, "helper_beacon");
        assert_eq!(symbol.origin.location, Some(SourceLocationKind::Dependency));
        assert!(
            matches!(
                result.warnings.as_slice(),
                [ReadWarning::GlobalIndexUnavailable { indexed: 1, .. }]
            ),
            "{:?}",
            result.warnings
        );
        Ok(())
    }

    /// `global` skips the project index: the project's own `beacon` never answers, the
    /// helper's exact `beacon` orders above its substring `helper_beacon`, the two scores
    /// fall with position, and a `file` target answers empty since a package contributes
    /// declarations alone.
    #[test]
    fn search_global_scope_skips_the_project_index() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "scope": "global",
            "include": ["score"]
        }))?;

        let result = service.search(&params, &StoreAnswer::identifier_only())?;

        assert_eq!(
            located_names(&result),
            [
                ("beacon".to_owned(), true),
                ("helper_beacon".to_owned(), true)
            ]
        );
        assert!(result.results.iter().all(|hit| hit.path.is_none()));
        let scores: Vec<Option<f64>> = result.results.iter().map(|hit| hit.score).collect();
        assert_eq!(scores, [Some(1.0), Some(0.5)]);

        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "scope": "global",
            "target": "file"
        }))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(result.results.is_empty(), "{:?}", result.results);
        assert_eq!(result.pagination.total_pages, 0);
        Ok(())
    }

    /// Under `all`, relevance orders both sides by match class: the helper's exact
    /// `beacon` above the project's prefix match `beacon_tower`, above the helper's
    /// substring match `helper_beacon`.
    #[test]
    fn search_all_scope_orders_project_and_package_hits_by_score() -> TestResult {
        let (_directory, service) = project_fixture("pub fn beacon_tower() {}\n")?;
        let service = service.with_packages(helper_store()?);
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "scope": "all",
            "target": "symbol"
        }))?;

        let result = service.search(&params, &StoreAnswer::identifier_only())?;

        assert_eq!(
            located_names(&result),
            [
                ("beacon".to_owned(), true),
                ("beacon_tower".to_owned(), false),
                ("helper_beacon".to_owned(), true),
            ]
        );
        Ok(())
    }

    /// Under `all` with `path` order, the project hit lists first and the two helper hits
    /// follow in identity order within their one unit.
    #[test]
    fn search_all_scope_path_order_lists_the_project_hit_before_the_package_hits() -> TestResult {
        let (_directory, service) = project_fixture("pub fn beacon_tower() {}\n")?;
        let service = service.with_packages(helper_store()?);
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "scope": "all",
            "target": "symbol",
            "order": "path"
        }))?;

        let result = service.search(&params, &StoreAnswer::identifier_only())?;

        assert_eq!(
            located_names(&result),
            [
                ("beacon_tower".to_owned(), false),
                ("beacon".to_owned(), true),
                ("helper_beacon".to_owned(), true),
            ]
        );
        Ok(())
    }

    #[test]
    fn search_global_scope_with_traversal_refuses_naming_traversal() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "beacon",
            "scope": "global",
            "traversal": { "seed": "rift://symbol/rust/src/lib.rs/beacon" }
        }))?;

        let error = service
            .search(&params, &StoreAnswer::identifier_only())
            .expect_err("the relationship graph serves the project alone");

        assert!(
            matches!(
                error.fault(),
                ReadFault::Invalid {
                    field: "traversal",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(error.descriptor().code(), "invalid_request");
        Ok(())
    }

    /// A walk names its starting declaration through `seed`, and a request that omits it
    /// leaves the walk nowhere to start.
    #[test]
    fn validate_search_refuses_a_traversal_that_names_no_seed() {
        let arguments = json!({"traversal": {"direction": "incoming"}});
        let params: SearchParams =
            serde_json::from_value(arguments.clone()).expect("the request parses");

        let error = super::validate_search(&params).expect_err("the seed rule must refuse");

        assert!(
            matches!(error.fault(), ReadFault::Invalid { field: "seed", .. }),
            "{arguments}: {error}"
        );
        assert_eq!(error.descriptor().code(), "invalid_request", "{arguments}");
    }

    /// A configured language engine resolves the references a walk follows, and an engine
    /// session serves the current tree; a comparison names two committed revisions.
    #[test]
    fn validate_search_refuses_a_traversal_riding_beside_a_comparison() {
        let params: SearchParams = serde_json::from_value(json!({
            "change": {"base": "baseline"},
            "traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}
        }))
        .expect("the request parses");

        let error = super::validate_search(&params).expect_err("the pairing must refuse");

        assert_eq!(error.descriptor().code(), "capability_unavailable");
        assert!(
            error
                .to_string()
                .contains(super::CHANGE_TRAVERSAL_CAPABILITY),
            "{error}"
        );
    }

    /// A walk standing without a comparison and naming its seed passes the rule.
    #[test]
    fn validate_search_accepts_a_seeded_walk_standing_alone() {
        let arguments = json!({"traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}});
        let params: SearchParams =
            serde_json::from_value(arguments.clone()).expect("the request parses");
        assert!(
            super::validate_search(&params).is_ok(),
            "{arguments} must pass the seed rule"
        );
    }

    #[test]
    fn search_rev_with_a_global_scope_refuses_naming_scope() -> TestResult {
        let (_directory, service) = dependency_fixture()?;
        for scope in ["global", "all"] {
            let params: SearchParams =
                serde_json::from_value(json!({"query": "beacon", "scope": scope, "rev": "main"}))?;

            let error = service
                .search(&params, &StoreAnswer::identifier_only())
                .expect_err("rev pairs with the project scope alone");

            assert!(
                matches!(error.fault(), ReadFault::Invalid { field: "scope", .. }),
                "scope {scope}: {error}"
            );
            assert_eq!(error.descriptor().code(), "invalid_request");
        }
        Ok(())
    }

    /// The package warnings ride a `search` answer whose scope reaches packages, the
    /// same way they ride `get_symbol`'s, and no other.
    #[test]
    fn search_global_scope_warns_a_skipped_package() -> TestResult {
        let mut index =
            rift_index::DependencyIndex::empty(rift_index::DependencyIndexLimits::default());
        let zeta = PackageIdentity {
            manager: "cargo".to_owned(),
            name: "zeta".to_owned(),
            version: "1.0.0".to_owned(),
        };
        index.skip(zeta.clone(), "zeta refused".to_owned());
        let (_directory, service) = project_fixture("pub fn beacon() {}\n")?;
        let service = service.with_packages(Arc::new(PackageBranch::from_index(index)));

        for scope in ["global", "all"] {
            let params: SearchParams =
                serde_json::from_value(json!({"query": "beacon", "scope": scope}))?;
            let result = service.search(&params, &StoreAnswer::identifier_only())?;
            assert!(
                matches!(
                    result.warnings.first(),
                    Some(ReadWarning::GlobalIndexUnavailable { .. })
                ),
                "scope {scope}: {:?}",
                result.warnings
            );
            assert_eq!(
                result.warnings[1..],
                [ReadWarning::PackageSkipped {
                    package: zeta.clone(),
                    reason: "zeta refused".to_owned(),
                }],
                "scope {scope}"
            );
        }
        let params: SearchParams = serde_json::from_value(json!({"query": "beacon"}))?;
        let result = service.search(&params, &StoreAnswer::identifier_only())?;
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        Ok(())
    }
}
