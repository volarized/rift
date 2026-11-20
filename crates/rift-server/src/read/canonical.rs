use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use sha2::{Digest as _, Sha256};

use super::ReadService;

const VIEW_LIFETIME: Duration = Duration::from_mins(15);
// Released and expired keys stay recognizable up to the largest accepted view count,
// `QUERY_RETAINED_VIEWS_MAX`, so a later read of one answers `view_expired` rather than
// `view_context`. A test pins the two.
const RELEASED_KEYS_MAX: usize = 4096;

/// Count and material bounds on the views one store retains.
///
/// A capture past either bound releases the least recently used views first; only a view
/// whose own material exceeds `bytes_max` is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedViewLimits {
    views_max: usize,
    bytes_max: usize,
}

impl RetainedViewLimits {
    /// Bounds a store to `views_max` views holding at most `bytes_max` material together.
    ///
    /// # Panics
    /// Panics when `views_max` is zero: a store that retains no view cannot answer a capture.
    #[must_use]
    #[track_caller]
    pub fn new(views_max: usize, bytes_max: usize) -> Self {
        assert!(
            views_max > 0,
            "a retained view store must admit at least one view: views_max={views_max}"
        );
        Self {
            views_max,
            bytes_max,
        }
    }

    /// Most views the store retains at once.
    #[must_use]
    pub fn views_max(&self) -> usize {
        self.views_max
    }

    /// Most material the retained views hold together.
    #[must_use]
    pub fn bytes_max(&self) -> usize {
        self.bytes_max
    }
}

impl From<&rift_protocol::query::QueryConfiguration> for RetainedViewLimits {
    /// Reads the accepted `[search.query]` bounds; a bound past this platform's address
    /// space saturates, since no allocation can reach it.
    fn from(configuration: &rift_protocol::query::QueryConfiguration) -> Self {
        let views_max = usize::try_from(configuration.retained_views)
            .unwrap_or(usize::MAX)
            .max(1);
        let bytes_max = usize::try_from(configuration.retained_size.bytes()).unwrap_or(usize::MAX);
        Self::new(views_max, bytes_max)
    }
}

/// One immutable publication and its measured retained material.
#[derive(Debug)]
pub struct LocalSymbolViewInput {
    material: Arc<LocalSymbolMaterial>,
    /// Whether the publication completed its bounded discovery and assembly.
    pub ready: bool,
    /// Owning material charge, including retained source, facts and admitted caches.
    retained_bytes: usize,
    query: Option<HeldQuery>,
    expires: Option<SystemTime>,
}

/// How much of the selected captured symbol inventory one projection holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryCoverage {
    /// Every source selected for indexing contributed its declarations.
    Complete,
    /// Some selected sources are left out of the index or hold declarations without an
    /// established identity; the captured warnings name them. Every projected fact stands.
    Partial,
}

impl InventoryCoverage {
    /// The coverage of two projections read together.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Complete, Self::Complete) => Self::Complete,
            _ => Self::Partial,
        }
    }
}

impl LocalSymbolViewInput {
    /// Projects selected symbols and their complete commit timelines.
    ///
    /// # Errors
    /// Refuses unavailable history, incomplete timelines, and graph admission failures.
    pub fn project_history(
        &self,
        reads: &ReadService,
        selection: rift_search::sparql::SymbolSelection<'_>,
        dataset: &mut rift_search::sparql::GraphDatasetBuilder,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: std::time::Instant,
    ) -> Result<(), RiftError> {
        use rift_search::sparql::SymbolAddress;

        if self.project_symbols(selection, dataset, cancellation, deadline)?
            == InventoryCoverage::Partial
        {
            return errors::server::search_coverage_unavailable()
                .operation("graph history acquisition")
                .fail();
        }
        for symbol in self.material.index.unestablished_objects() {
            check_graph_projection(cancellation, deadline)?;
            let selected = match selection {
                rift_search::sparql::SymbolSelection::All => true,
                rift_search::sparql::SymbolSelection::Named(name) => symbol?.facts().name() == name,
                rift_search::sparql::SymbolSelection::Identified(_) => false,
            };
            if selected {
                return errors::server::search_coverage_unavailable()
                    .operation("graph history identity acquisition")
                    .fail();
            }
        }
        let mut timelines = reads.symbol_timelines()?;
        for (identity, symbol) in self.material.index.objects() {
            check_graph_projection(cancellation, deadline)?;
            let id = rift_protocol::read::SymbolId(identity.as_str().to_owned());
            if !matches_symbol(
                selection,
                SymbolAddress::Established(&id),
                symbol?.facts().name(),
            ) {
                continue;
            }
            let (bindings, _) = self.material.index.symbols_by_identity_page(
                identity,
                0,
                reads.index.results_max(),
            )?;
            let history = timelines.symbol_timeline(&reads.index, &bindings, id)?;
            if !history.complete {
                return errors::server::search_coverage_unavailable()
                    .operation("graph history acquisition")
                    .fail();
            }
            dataset.insert_history(&history)?;
        }
        check_graph_projection(cancellation, deadline)
    }

    /// Attaches encoded query material to the existing view and byte bounds.
    ///
    /// The view expires no later than `expires` when supplied.
    #[must_use]
    pub fn with_query(
        mut self,
        key: [u8; 32],
        bytes: Vec<u8>,
        expires: Option<SystemTime>,
    ) -> Self {
        self.query = Some(HeldQuery {
            key,
            bytes: Arc::from(bytes),
        });
        self.expires = expires;
        self
    }

    /// Retains source bytes from the same request's admitted file selection.
    ///
    /// # Errors
    /// Refuses a different root, owner or captured file identity.
    pub fn with_sources(self, sources: &rift_index::WorkspaceIndex) -> Result<Self, RiftError> {
        Ok(Self {
            query: self.query,
            expires: self.expires,
            ..local_view_input(
                Arc::new(self.material.index.with_sources(sources)?),
                self.material.metadata.to_vec(),
                self.ready,
            )
        })
    }

    /// Returns the warnings retained by this immutable input.
    ///
    /// # Errors
    /// Returns the original captured metadata read failure.
    pub fn warnings(&self) -> Result<Vec<rift_protocol::read::ReadWarning>, RiftError> {
        let metadata: CapturedMetadata = serde_json::from_slice(&self.material.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        Ok(metadata.warnings)
    }

    /// Adds caller-owned warnings before the immutable material is admitted.
    ///
    /// # Errors
    /// Returns the original captured metadata read or encoding failure.
    pub fn with_warnings(
        self,
        warnings: Vec<rift_protocol::read::ReadWarning>,
    ) -> Result<Self, RiftError> {
        if warnings.is_empty() {
            return Ok(self);
        }
        let mut metadata: CapturedMetadata = serde_json::from_slice(&self.material.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        metadata.warnings.extend(warnings);
        let metadata = serde_json::to_vec(&metadata)
            .map_err(|error| material_error("capture read metadata", &error))?;
        Ok(Self {
            query: self.query,
            expires: self.expires,
            ..local_view_input(Arc::clone(&self.material.index), metadata, self.ready)
        })
    }

    /// Reads the exact bytes of a physical occurrence in this publication.
    #[must_use]
    pub fn occurrence_source(&self, source: &rift_protocol::read::SourceUnitSpan) -> Option<&str> {
        let path = rift_core::ProjectPath::new(source.unit.project_path()?).ok()?;
        let start = usize::try_from(source.range.start).ok()?;
        let end = usize::try_from(source.range.end).ok()?;
        self.material.index.file(&path)?.source().get(start..end)
    }

    /// Collects canonical declarations with source names for a bounded engine read.
    ///
    /// # Errors
    /// Refuses incomplete source coverage and a seed inventory above the bound.
    pub fn semantic_seeds(
        &self,
        mut remaining: usize,
    ) -> Result<Vec<rift_protocol::read::SymbolId>, RiftError> {
        let unavailable = || {
            errors::server::search_coverage_unavailable()
                .operation("local semantic inventory")
                .error()
        };
        let index = &self.material.index;
        if !self.ready || !index.objects_complete() {
            return Err(unavailable());
        }
        let mut seeds = Vec::new();
        for (id, object) in index.objects() {
            let object = object?;
            // A Python file module has no source name token for an engine request.
            // Its declaration remains in the captured symbol inventory.
            let facts = object.facts();
            if facts.language().name == "python" && facts.kind().0 == "module" {
                continue;
            }
            let declarations = object.assembled().contributions().len().max(1);
            remaining = remaining
                .checked_sub(declarations)
                .ok_or_else(unavailable)?;
            seeds.push(rift_protocol::read::SymbolId(id.as_str().to_owned()));
        }
        Ok(seeds)
    }

    /// Collects every established canonical declaration for a captured inventory read,
    /// with the coverage of the inventory they come from.
    ///
    /// A source left out of the index makes the inventory partial. Objects without
    /// identities stay outside this selection; callers report their omission.
    ///
    /// # Errors
    /// Returns the captured publication failure.
    pub fn inventory_seeds(
        &self,
    ) -> Result<(Vec<rift_protocol::read::SymbolId>, InventoryCoverage), RiftError> {
        let index = &self.material.index;
        let coverage = if self.ready && index.objects_complete() {
            InventoryCoverage::Complete
        } else {
            InventoryCoverage::Partial
        };
        let mut seeds = Vec::new();
        for (id, object) in index.objects() {
            object?;
            seeds.push(rift_protocol::read::SymbolId(id.as_str().to_owned()));
        }
        Ok((seeds, coverage))
    }

    /// Projects captured symbol facts for a query whose algebra requires only symbols.
    ///
    /// An identified symbol answers exactly or refuses: its absence is a claim only complete
    /// coverage of its language makes. A named or whole inventory projects every captured
    /// object and reports [`InventoryCoverage::Partial`] when some selected source is left
    /// out of the index.
    ///
    /// # Errors
    /// Returns the captured publication failure or a graph admission refusal.
    pub fn project_symbols(
        &self,
        selection: rift_search::sparql::SymbolSelection<'_>,
        dataset: &mut rift_search::sparql::GraphDatasetBuilder,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: std::time::Instant,
    ) -> Result<InventoryCoverage, RiftError> {
        use rift_search::sparql::{GraphFact, RevisionIri, SymbolAddress, SymbolSelection};

        if !self.ready {
            return errors::server::search_coverage_unavailable()
                .operation("graph query acquisition")
                .fail();
        }
        let metadata: CapturedMetadata = serde_json::from_slice(&self.material.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let revision = RevisionIri::parse(&metadata.revision.to_string())?;
        let index = &self.material.index;
        let check = || check_graph_projection(cancellation, deadline);
        let mut write = |symbol: &rift_index::ReadableSymbol, address: SymbolAddress<'_>| {
            let facts = symbol.facts();
            if !matches_symbol(selection, address, facts.name()) {
                return Ok(());
            }
            dataset.insert(GraphFact::SymbolDetails {
                address,
                name: facts.name(),
                kind: &facts.kind().0,
                language: facts.language(),
                revision: &revision,
            })
        };

        if let SymbolSelection::Identified(selected) = selection {
            let identity = rift_protocol::identity::SymbolIdentity::parse(selected)?;
            if identity.owner() != self.owner() {
                return errors::server::search_coverage_unavailable()
                    .operation("graph symbol acquisition")
                    .fail();
            }
            let key = rift_core::SymbolId::new(selected.to_owned()).map_err(|_| {
                errors::server::search_coverage_unavailable()
                    .operation("graph symbol acquisition")
                    .error()
            })?;
            if let Some(symbol) = index.object_by_identity(&key)? {
                let id = rift_protocol::read::SymbolId(selected.to_owned());
                write(&symbol, SymbolAddress::Established(&id))?;
            } else if !index.identity_coverage_complete(identity.language()) {
                return errors::server::search_coverage_unavailable()
                    .operation("graph symbol acquisition")
                    .fail();
            }
            check()?;
            return Ok(InventoryCoverage::Complete);
        }
        let coverage = if index.objects_complete() {
            InventoryCoverage::Complete
        } else {
            InventoryCoverage::Partial
        };
        for (identity, symbol) in index.objects() {
            check()?;
            let identity = rift_protocol::read::SymbolId(identity.as_str().to_owned());
            write(symbol?, SymbolAddress::Established(&identity))?;
        }
        for symbol in index.unestablished_objects() {
            check()?;
            let symbol = symbol?;
            let contribution = symbol.assembled().contributions().first().ok_or_else(|| {
                errors::server::search_coverage_unavailable()
                    .operation("graph query acquisition")
                    .error()
            })?;
            write(
                &symbol,
                SymbolAddress::Unestablished(contribution.reference()),
            )?;
        }
        check()?;
        Ok(coverage)
    }

    /// Projects the revision-pinned syntax read of each selected canonical declaration,
    /// with the coverage [`Self::project_symbols`] reports for the selected symbols.
    ///
    /// # Errors
    /// Returns a syntax read or graph admission refusal before completing the projection.
    pub fn project_syntax_elements(
        &self,
        reads: &ReadService,
        selection: rift_search::sparql::SyntaxSelection<'_>,
        dataset: &mut rift_search::sparql::GraphDatasetBuilder,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: std::time::Instant,
    ) -> Result<InventoryCoverage, RiftError> {
        use rift_protocol::read::{FacetsParams, SymbolId};
        use rift_search::sparql::SymbolAddress;

        let coverage = self.project_symbols(selection.symbols, dataset, cancellation, deadline)?;
        for (identity, symbol) in self.material.index.objects() {
            check_graph_projection(cancellation, deadline)?;
            let id = SymbolId(identity.as_str().to_owned());
            if !matches_symbol(
                selection.symbols,
                SymbolAddress::Established(&id),
                symbol?.facts().name(),
            ) {
                continue;
            }
            let mut request = FacetsParams::of(id.clone());
            loop {
                check_graph_projection(cancellation, deadline)?;
                let page = reads.facets(&request)?;
                for syntax_element in page
                    .declarations
                    .into_iter()
                    .flat_map(|declaration| declaration.syntax)
                {
                    dataset.insert_declared_syntax_element(
                        &id,
                        &syntax_element,
                        selection.source,
                    )?;
                }
                if request.page >= page.pagination.pages {
                    break;
                }
                request.revision = Some(page.revision);
                request.page += 1;
            }
        }
        Ok(coverage)
    }

    /// Root of the immutable publication admitted by this input.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.material.index.root()
    }

    /// Fixed owner of the immutable publication admitted by this input.
    #[must_use]
    pub fn owner(&self) -> &SymbolOwner {
        self.material.index.owner()
    }
}

#[derive(Debug)]
struct LocalSymbolMaterial {
    index: Arc<rift_index::WorkspaceSymbolView>,
    metadata: Arc<[u8]>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CapturedMetadata {
    revision: rift_protocol::revision::Revision,
    warnings: Vec<rift_protocol::read::ReadWarning>,
}

#[derive(Debug, Clone)]
struct HeldQuery {
    key: [u8; 32],
    bytes: Arc<[u8]>,
}

#[derive(Debug, Clone)]
struct HeldView {
    material: Arc<LocalSymbolMaterial>,
    ready: bool,
    retained_bytes: usize,
    view: CapturedView,
    expires: SystemTime,
    query: Option<HeldQuery>,
    /// Position of the last capture or read of this view in the store's use order.
    used: u64,
}

#[derive(Debug, Default)]
struct State {
    views: BTreeMap<String, HeldView>,
    expired: BTreeSet<String>,
    serial: u64,
    uses: u64,
}

impl State {
    fn expire(&mut self, now: SystemTime) {
        let expired: Vec<_> = self
            .views
            .iter()
            .filter(|(_, held)| now >= held.expires)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.views.remove(&key);
            self.expired.insert(key);
        }
        self.bound_released_keys();
    }

    fn bound_released_keys(&mut self) {
        while self.expired.len() > RELEASED_KEYS_MAX {
            self.expired.pop_first();
        }
    }

    /// Advances the use order; the counter saturates, which only ties the newest uses.
    fn next_use(&mut self) -> u64 {
        self.uses = self.uses.saturating_add(1);
        self.uses
    }

    /// Releases the least recently used view other than `kept`, answering whether one was.
    ///
    /// One call scans every retained view once, at most `views_max` entries.
    fn release_least_recent(&mut self, kept: Option<&str>) -> bool {
        let Some(key) = self
            .views
            .iter()
            .filter(|(key, _)| Some(key.as_str()) != kept)
            .min_by_key(|(_, held)| held.used)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        self.views.remove(&key);
        self.expired.insert(key);
        self.bound_released_keys();
        true
    }

    /// Material one view charges when it is the only one retained.
    fn single_view_bytes(key: &str, held: &HeldView, bytes_max: usize) -> usize {
        let mut alone = Self::default();
        alone.views.insert(key.to_owned(), held.clone());
        alone.admitted_material_bytes(bytes_max)
    }

    fn exceeds(&self, bytes_max: usize) -> bool {
        let bytes = self.admitted_material_bytes(bytes_max);
        bytes == usize::MAX || bytes > bytes_max
    }

    fn material_bytes(&self) -> usize {
        self.material_bytes_with_shared_indexes(false)
    }

    fn admitted_material_bytes(&self, bytes_max: usize) -> usize {
        let cached = self.material_bytes();
        if cached != usize::MAX && cached > bytes_max {
            self.material_bytes_with_shared_indexes(true)
        } else {
            cached
        }
    }

    fn material_bytes_with_shared_indexes(&self, shared: bool) -> usize {
        let mut charged = get_size2::StandardTracker::default();
        let mut total = self.store_bytes(self.views.len());
        for (key, held) in &self.views {
            let (bytes, next) = Self::view_bytes(key.capacity(), held, shared, charged);
            charged = next;
            total = total.saturating_add(bytes);
        }
        total
    }

    /// Material the store itself charges while it holds `views` views: the two maps and
    /// the released keys.
    fn store_bytes(&self, views: usize) -> usize {
        let maps = rift_protocol::btree_material_bytes::<String, HeldView>(views).saturating_add(
            rift_protocol::btree_material_bytes::<String, ()>(self.expired.len()),
        );
        self.expired
            .iter()
            .fold(maps, |bytes, key| bytes.saturating_add(key.capacity()))
    }

    /// Material one view adds to the views `charged` has seen. With `shared`, an index
    /// is measured against every allocation already charged; without, by its own cached
    /// charge.
    fn view_bytes(
        key_bytes: usize,
        held: &HeldView,
        shared: bool,
        mut charged: get_size2::StandardTracker,
    ) -> (usize, get_size2::StandardTracker) {
        use get_size2::{GetSize, GetSizeTracker};
        let mut total = key_bytes
            .saturating_add(std::mem::size_of::<HeldView>())
            .saturating_add(held.view.id.get_heap_size())
            .saturating_add(held.view.expires_at.capacity());
        if let Some(query) = &held.query {
            total = total
                .saturating_add(query.bytes.len())
                .saturating_add(2 * std::mem::size_of::<usize>());
        }
        if charged.track(Arc::as_ptr(&held.material.index)) {
            let bytes = if shared {
                let (bytes, next) = held.material.index.as_ref().get_size_with_tracker(charged);
                charged = next;
                bytes
            } else {
                held.material.index.retained_material_bytes()
            };
            total = total
                .saturating_add(bytes)
                .saturating_add(2 * std::mem::size_of::<usize>());
        }
        if charged.track(Arc::as_ptr(&held.material)) {
            total = total.saturating_add(held.retained_bytes);
        }
        (total, charged)
    }

    /// Releases the least recently used views other than `kept` that the shared measure
    /// shows cannot stay under `bytes_max`, answering how many it released and whether
    /// the measure it took settles that the views left fit.
    ///
    /// It acts only when the cached charges pass the bound. The shared material is then
    /// measured once, the most recently used view first: each view adds what the views
    /// before it do not already hold, so the views that fit are a prefix of that order
    /// and the rest are the least recently used. Measuring stops at the first view that
    /// passes the bound. Measuring every remaining view again after each release answered
    /// the same and walked each retained index once per release.
    fn release_measured(&mut self, kept: &str, bytes_max: usize) -> (usize, bool) {
        let cached = self.material_bytes();
        if cached == usize::MAX || cached <= bytes_max {
            return (0, false);
        }
        let mut order = self.views.iter().collect::<Vec<_>>();
        order.sort_by_key(|(key, held)| (key.as_str() != kept, std::cmp::Reverse(held.used)));
        let mut charged = get_size2::StandardTracker::default();
        let mut views_bytes = 0_usize;
        let mut fits = 0_usize;
        for (position, (key, held)) in order.iter().enumerate() {
            let (bytes, next) = Self::view_bytes(key.capacity(), held, true, charged);
            charged = next;
            views_bytes = views_bytes.saturating_add(bytes);
            let total = self.store_bytes(position + 1).saturating_add(views_bytes);
            if position > 0 && (total == usize::MAX || total > bytes_max) {
                break;
            }
            fits = position + 1;
        }
        let released = self.views.len() - fits.max(1);
        for _ in 0..released {
            self.release_least_recent(Some(kept));
        }
        // With nothing released the measure is the one `exceeds` takes. A release grows
        // the released keys by a few bytes the measure did not count, so the caller
        // checks the views left once more; their cached charges usually answer.
        (released, released == 0)
    }
}

/// Retains exact local read authority and continuations under count, byte and expiry bounds.
///
/// A capture past the count or byte bound releases the least recently used views; a later
/// read of a released view answers `view_expired`, and a later query page evaluates again.
#[derive(Debug)]
pub struct LocalSymbolViews {
    service_key: [u8; 32],
    state: Mutex<State>,
}

impl LocalSymbolViews {
    /// Creates a service-local authority store; each capture states its own bounds.
    #[must_use]
    pub fn new(service_key: [u8; 32]) -> Self {
        Self {
            service_key,
            state: Mutex::new(State::default()),
        }
    }

    /// Captures one exact publication under `limits`, releasing least recently used views.
    ///
    /// Refuses admission, keeping every retained view, only when the publication's own
    /// material exceeds the byte bound.
    ///
    /// # Errors
    /// Returns a registered read failure if the service-issued key cannot be constructed.
    pub fn capture(
        &self,
        input: LocalSymbolViewInput,
        limits: RetainedViewLimits,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_at(input, limits, SystemTime::now())
    }

    /// Captures a view only when the caller also admits its bounded response.
    ///
    /// The callback runs after material admission while the store is locked. It
    /// must not call this store. Refusal removes only the newly captured view; views
    /// released to admit it stay released.
    ///
    /// # Errors
    /// Returns a registered read failure or the callback's registered error.
    pub fn capture_with(
        &self,
        input: LocalSymbolViewInput,
        limits: RetainedViewLimits,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_with_at(input, limits, SystemTime::now(), admit)
    }

    fn capture_at(
        &self,
        input: LocalSymbolViewInput,
        limits: RetainedViewLimits,
        now: SystemTime,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_with_at(input, limits, now, |_| Ok(true))
    }

    fn capture_with_at(
        &self,
        input: LocalSymbolViewInput,
        limits: RetainedViewLimits,
        now: SystemTime,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(now);
        let retained_bytes = input.retained_bytes;
        let index_bytes = input.material.index.retained_material_bytes();
        let ready = input.ready;
        if retained_bytes == 0 {
            refuse_material(&state, limits, retained_bytes, index_bytes, ready, 0);
            return Ok(None);
        }
        let id = self.next_key(&mut state)?;
        let expires = now
            .checked_add(VIEW_LIFETIME)
            .ok_or_else(|| invalid("view", "view expiry exceeds the clock bound"))?;
        let expires = input
            .expires
            .map_or(expires, |provider| expires.min(provider));
        let view = CapturedView {
            id: CapturedViewId::parse(&id)?,
            expires_at: chrono::DateTime::<chrono::Utc>::from(expires).to_rfc3339(),
        };
        let held = HeldView {
            material: input.material,
            ready,
            retained_bytes,
            view: view.clone(),
            expires,
            query: input.query,
            used: state.next_use(),
        };
        let alone = State::single_view_bytes(&id, &held, limits.bytes_max);
        if alone == usize::MAX || alone > limits.bytes_max {
            refuse_material(&state, limits, retained_bytes, index_bytes, ready, alone);
            return Ok(None);
        }
        let released = Self::admit_material(&mut state, &id, held, limits);
        if state.exceeds(limits.bytes_max) {
            // Every other view is released; the released keys alone push past the bound.
            state.views.remove(&id);
            refuse_material(&state, limits, retained_bytes, index_bytes, ready, alone);
            return Ok(None);
        }
        if released > 0 {
            rift_tracing::debug!(
                component = "server",
                released,
                rows = state.views.len(),
                views_max = limits.views_max,
                bytes_max = limits.bytes_max,
                "retained views released to admit a new view"
            );
        }
        match admit(&view) {
            Ok(true) => {}
            Ok(false) => {
                state.views.remove(&id);
                return Ok(None);
            }
            Err(error) => {
                state.views.remove(&id);
                return Err(error);
            }
        }
        Ok(Some(view))
    }

    /// Reads encoded query material while its original view remains retained.
    #[must_use]
    pub fn query(&self, key: &[u8; 32]) -> Option<(CapturedView, Arc<[u8]>)> {
        self.query_at(key, SystemTime::now())
    }

    fn query_at(&self, key: &[u8; 32], now: SystemTime) -> Option<(CapturedView, Arc<[u8]>)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(now);
        let used = state.next_use();
        // The configured view bound caps this lookup at `retained_views` entries.
        state.views.values_mut().find_map(|held| {
            let query = held.query.as_ref()?;
            let selected = held.ready && &query.key == key;
            selected.then(|| {
                held.used = used;
                (held.view.clone(), Arc::clone(&query.bytes))
            })
        })
    }

    /// Inserts `held` under `id`, releasing least recently used views past either bound.
    ///
    /// Returns the released count. At most every previously retained view is released, so
    /// the loops run at most `views_max` times; each byte check costs one walk of the views.
    fn admit_material(
        state: &mut State,
        id: &str,
        held: HeldView,
        limits: RetainedViewLimits,
    ) -> usize {
        let mut released = 0;
        while state.views.len() >= limits.views_max && state.release_least_recent(None) {
            released += 1;
        }
        state.views.insert(id.to_owned(), held);
        let (measured, settled) = state.release_measured(id, limits.bytes_max);
        released += measured;
        if settled {
            return released;
        }
        while state.exceeds(limits.bytes_max) && state.release_least_recent(Some(id)) {
            released += 1;
        }
        released
    }

    fn next_key(&self, state: &mut State) -> Result<String, RiftError> {
        state.serial = state
            .serial
            .checked_add(1)
            .ok_or_else(|| invalid("view", "view key count exceeds its bound"))?;
        let mut hash = Sha256::new();
        hash.update(self.service_key);
        hash.update(state.serial.to_be_bytes());
        Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
    }

    /// Returns the retained view selected by a complete service-issued key.
    ///
    /// The inner result carries an unavailable reason for expired views, changed
    /// contexts, or an index that is preparing. Success preserves the original expiry.
    ///
    /// # Errors
    /// Returns the registered read error when the view key is malformed or oversized.
    pub fn captured_view(
        &self,
        id: &str,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<Result<CapturedView, SymbolUnavailableReason>, RiftError> {
        let id = CapturedViewId::parse(id)?;
        Ok(self.captured_view_at(&id, root, owner, SystemTime::now()))
    }

    /// Returns the retained view `id` names as of `now`.
    ///
    /// # Errors
    /// Returns `view_expired` for a view that expired by `now`, `view_context` for an unknown
    /// view or another root or owner, and `index_preparing` for a view captured while the
    /// index prepared.
    pub fn captured_view_at(
        &self,
        id: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
        now: SystemTime,
    ) -> Result<CapturedView, SymbolUnavailableReason> {
        self.held_view_at(id, root, owner, now)
            .map(|held| held.view)
    }

    fn held_view_at(
        &self,
        id: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
        now: SystemTime,
    ) -> Result<HeldView, SymbolUnavailableReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(now);
        let used = state.next_use();
        let Some(held) = state.views.get_mut(id.as_str()) else {
            return Err(if state.expired.contains(id.as_str()) {
                SymbolUnavailableReason::ViewExpired
            } else {
                SymbolUnavailableReason::ViewContext
            });
        };
        if held.material.index.root() != root || held.material.index.owner() != owner {
            return Err(SymbolUnavailableReason::ViewContext);
        }
        if !held.ready {
            return Err(SymbolUnavailableReason::IndexPreparing);
        }
        held.used = used;
        Ok(held.clone())
    }
}

fn refuse_material(
    state: &State,
    limits: RetainedViewLimits,
    incoming_bytes: usize,
    index_bytes: usize,
    ready: bool,
    view_bytes: usize,
) {
    rift_tracing::warn!(
        component = "server",
        rows = state.views.len(),
        views_max = limits.views_max,
        view_bytes,
        incoming_bytes,
        index_bytes,
        ready,
        bytes_max = limits.bytes_max,
        "retained read material exceeds the byte bound"
    );
}

fn invalid(field: &'static str, violation: &'static str) -> RiftError {
    errors::server::read_invalid()
        .field(field)
        .violation(violation)
        .error()
}

fn matches_symbol(
    selection: rift_search::sparql::SymbolSelection<'_>,
    address: rift_search::sparql::SymbolAddress<'_>,
    name: &str,
) -> bool {
    use rift_search::sparql::{SymbolAddress, SymbolSelection};
    match selection {
        SymbolSelection::All => true,
        SymbolSelection::Named(selected) => selected == name,
        SymbolSelection::Identified(selected) => {
            matches!(address, SymbolAddress::Established(id) if selected == id.as_str())
        }
    }
}

fn check_graph_projection(
    cancellation: &rift_search::sparql::QueryCancellation,
    deadline: std::time::Instant,
) -> Result<(), RiftError> {
    if cancellation.is_cancelled() {
        return errors::search::query_cancelled().fail();
    }
    if std::time::Instant::now() >= deadline {
        return errors::search::query_deadline_exceeded()
            .timeout_ms(
                u64::try_from(rift_search::sparql::EVALUATION_TIMEOUT.as_millis())
                    .unwrap_or(u64::MAX),
            )
            .fail();
    }
    Ok(())
}

impl ReadService {
    /// Captures immutable search, source, graph and documentation material without history.
    ///
    /// # Errors
    /// Returns the original captured index or read failure.
    pub fn local_search_view_input(&self, ready: bool) -> Result<LocalSymbolViewInput, RiftError> {
        let (index, metadata) = self.local_view_material()?;
        Ok(local_view_input(index, metadata, ready))
    }

    fn local_view_material(
        &self,
    ) -> Result<(Arc<rift_index::WorkspaceSymbolView>, Vec<u8>), RiftError> {
        let index = self.index.symbol_view()?;
        let metadata = serde_json::to_vec(&CapturedMetadata {
            revision: self.served_revision()?,
            warnings: self.warnings(),
        })
        .map_err(|error| material_error("capture read metadata", &error))?;
        Ok((index, metadata))
    }
}

fn material_error(operation: &str, error: &impl std::fmt::Display) -> RiftError {
    errors::server::read_unavailable()
        .operation(operation)
        .detail(error.to_string())
        .error()
}

fn local_view_input(
    index: Arc<rift_index::WorkspaceSymbolView>,
    metadata: Vec<u8>,
    ready: bool,
) -> LocalSymbolViewInput {
    let material = Arc::new(LocalSymbolMaterial {
        index,
        metadata: Arc::from(metadata),
    });
    let retained_bytes = std::mem::size_of::<LocalSymbolMaterial>()
        .saturating_add(4 * std::mem::size_of::<usize>())
        .saturating_add(material.metadata.len());
    LocalSymbolViewInput {
        material,
        ready,
        retained_bytes,
        query: None,
        expires: None,
    }
}

#[cfg(test)]
mod tests;
