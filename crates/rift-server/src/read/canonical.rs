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
const VIEW_SLOTS: usize = 256;

/// One immutable publication and its measured retained material.
#[derive(Debug)]
pub struct LocalSymbolViewInput {
    material: Arc<LocalSymbolMaterial>,
    /// Whether the publication completed its bounded discovery and assembly.
    pub ready: bool,
    /// Owning material charge, including retained source, facts and admitted caches.
    retained_bytes: usize,
}

impl LocalSymbolViewInput {
    /// Retains source bytes from the same request's admitted file selection.
    ///
    /// # Errors
    /// Refuses a different root, owner or captured file identity.
    pub fn with_sources(self, sources: &rift_index::WorkspaceIndex) -> Result<Self, RiftError> {
        Ok(local_view_input(
            Arc::new(self.material.index.with_sources(sources)?),
            self.material.metadata.to_vec(),
            self.ready,
        ))
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
        Ok(local_view_input(
            Arc::clone(&self.material.index),
            metadata,
            self.ready,
        ))
    }

    /// Reads the exact bytes of a physical occurrence in this publication.
    #[must_use]
    pub fn occurrence_source(&self, source: &rift_protocol::read::SourceUnitSpan) -> Option<&str> {
        let path = rift_core::ProjectPath::new(source.unit.project_path()?).ok()?;
        let start = usize::try_from(source.range.start).ok()?;
        let end = usize::try_from(source.range.end).ok()?;
        self.material.index.file(&path)?.source().get(start..end)
    }

    /// Collects the complete canonical seed inventory for a bounded local semantic read.
    ///
    /// # Errors
    /// Refuses incomplete source or identity coverage and a seed inventory above the bound.
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
        if !self.ready
            || !index.objects_complete()
            || index.unestablished_objects().next().is_some()
        {
            return Err(unavailable());
        }
        let mut seeds = Vec::new();
        for (id, object) in index.objects() {
            let object = object?;
            let declarations = object.assembled().contributions().len().max(1);
            remaining = remaining
                .checked_sub(declarations)
                .ok_or_else(unavailable)?;
            if !index.identity_coverage_complete(object.facts().language()) {
                return Err(unavailable());
            }
            seeds.push(rift_protocol::read::SymbolId(id.as_str().to_owned()));
        }
        Ok(seeds)
    }

    /// Projects captured symbol facts for a query whose algebra requires only symbols.
    ///
    /// # Errors
    /// Returns the captured publication failure or a graph admission refusal.
    pub fn project_symbols(
        &self,
        selection: rift_search::sparql::SymbolSelection<'_>,
        dataset: &mut rift_search::sparql::GraphDatasetBuilder,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: std::time::Instant,
    ) -> Result<(), RiftError> {
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
            return check();
        }
        if !index.objects_complete() {
            return errors::server::search_coverage_unavailable()
                .operation("graph symbol acquisition")
                .fail();
        }
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
        check()
    }

    /// Projects the revision-pinned syntax read of each selected canonical declaration.
    ///
    /// # Errors
    /// Returns a syntax read or graph admission refusal before completing the projection.
    pub fn project_nodes(
        &self,
        reads: &ReadService,
        selection: rift_search::sparql::NodeSelection<'_>,
        dataset: &mut rift_search::sparql::GraphDatasetBuilder,
        cancellation: &rift_search::sparql::QueryCancellation,
        deadline: std::time::Instant,
    ) -> Result<(), RiftError> {
        use rift_protocol::read::{NodesParams, SymbolId};
        use rift_search::sparql::SymbolAddress;

        self.project_symbols(selection.symbols, dataset, cancellation, deadline)?;
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
            let mut request = NodesParams::of(id.clone());
            loop {
                check_graph_projection(cancellation, deadline)?;
                let page = reads.nodes(&request)?;
                for node in page.nodes {
                    dataset.insert_declared_node(&id, &node, selection.source)?;
                }
                let Some(cursor) = page.node_cursor else {
                    break;
                };
                if request.node_cursor.as_ref() == Some(&cursor) {
                    return errors::server::search_coverage_unavailable()
                        .operation("graph syntax nodes")
                        .fail();
                }
                request.node_cursor = Some(cursor);
            }
        }
        Ok(())
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
struct HeldView {
    material: Arc<LocalSymbolMaterial>,
    ready: bool,
    retained_bytes: usize,
    view: CapturedView,
    expires: SystemTime,
}

#[derive(Debug, Default)]
struct State {
    views: BTreeMap<String, HeldView>,
    expired: BTreeSet<String>,
    serial: u64,
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
        while self.expired.len() > VIEW_SLOTS {
            self.expired.pop_first();
        }
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
        use get_size2::{GetSize, GetSizeTracker};
        let mut total = rift_protocol::btree_material_bytes::<String, HeldView>(self.views.len())
            .saturating_add(rift_protocol::btree_material_bytes::<String, ()>(
                self.expired.len(),
            ));
        let mut charged = get_size2::StandardTracker::default();
        for (key, held) in &self.views {
            total = total
                .saturating_add(key.capacity())
                .saturating_add(std::mem::size_of::<HeldView>())
                .saturating_add(held.view.id.get_heap_size())
                .saturating_add(held.view.expires_at.capacity());
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
        }
        self.expired
            .iter()
            .fold(total, |bytes, key| bytes.saturating_add(key.capacity()))
    }
}

/// Retains exact local read authority and continuations under count, byte and expiry bounds.
#[derive(Debug)]
pub struct LocalSymbolViews {
    material_bytes_max: usize,
    service_key: [u8; 32],
    state: Mutex<State>,
}

impl LocalSymbolViews {
    /// Creates a service-local authority store with a caller-owned material budget.
    #[must_use]
    pub fn new(material_bytes_max: usize, service_key: [u8; 32]) -> Self {
        Self {
            material_bytes_max,
            service_key,
            state: Mutex::new(State::default()),
        }
    }

    /// Captures one exact publication, or refuses admission without discarding active views.
    ///
    /// # Errors
    /// Returns a registered read failure if the service-issued key cannot be constructed.
    pub fn capture(&self, input: LocalSymbolViewInput) -> Result<Option<CapturedView>, RiftError> {
        self.capture_at(input, SystemTime::now())
    }

    /// Captures a view only when the caller also admits its bounded response.
    ///
    /// The callback runs after material admission while the store is locked. It
    /// must not call this store. Refusal removes only the newly captured view.
    ///
    /// # Errors
    /// Returns a registered read failure or the callback's registered error.
    pub fn capture_with(
        &self,
        input: LocalSymbolViewInput,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_with_at(input, SystemTime::now(), admit)
    }

    fn capture_at(
        &self,
        input: LocalSymbolViewInput,
        now: SystemTime,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_with_at(input, now, |_| Ok(true))
    }

    fn capture_with_at(
        &self,
        input: LocalSymbolViewInput,
        now: SystemTime,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(now);
        if input.retained_bytes == 0 || state.views.len() == VIEW_SLOTS {
            rift_tracing::warn!(
                component = "server",
                rows = state.views.len(),
                views_max = VIEW_SLOTS,
                incoming_bytes = input.retained_bytes,
                index_bytes = input.material.index.retained_material_bytes(),
                ready = input.ready,
                bytes_max = self.material_bytes_max,
                "retained read material exceeds the byte or view bound"
            );
            return Ok(None);
        }
        let retained_bytes = input.retained_bytes;
        let index_bytes = input.material.index.retained_material_bytes();
        let ready = input.ready;
        let id = self.next_key(&mut state)?;
        let expires = now
            .checked_add(VIEW_LIFETIME)
            .ok_or_else(|| invalid("view", "view expiry exceeds the clock bound"))?;
        let view = CapturedView {
            id: CapturedViewId::parse(&id)?,
            expires_at: chrono::DateTime::<chrono::Utc>::from(expires).to_rfc3339(),
        };
        state.views.insert(
            id.clone(),
            HeldView {
                material: input.material,
                ready: input.ready,
                retained_bytes: input.retained_bytes,
                view: view.clone(),
                expires,
            },
        );
        let material_bytes = state.admitted_material_bytes(self.material_bytes_max);
        if material_bytes == usize::MAX || material_bytes > self.material_bytes_max {
            rift_tracing::warn!(
                component = "server",
                rows = state.views.len(),
                views_max = VIEW_SLOTS,
                material_bytes,
                incoming_bytes = retained_bytes,
                index_bytes,
                ready,
                bytes_max = self.material_bytes_max,
                "retained read material exceeds the byte or view bound"
            );
            state.views.remove(&id);
            return Ok(None);
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

    fn captured_view_at(
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
        let Some(held) = state.views.get(id.as_str()) else {
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
        Ok(held.clone())
    }
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
    }
}

#[cfg(test)]
mod tests;
