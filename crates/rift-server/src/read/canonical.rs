use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{GetSymbolInclude, SymbolId};
use rift_protocol::symbol_read::{
    CapturedView, CapturedViewId, GetSymbolParams, GetSymbolResult, SymbolNotFound,
    SymbolUnavailableReason,
};
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
    expiry_limit: Option<SystemTime>,
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
            Arc::clone(&self.material.history),
            self.material.traversal.clone(),
            self.ready,
        )
        .with_expiry_limit(self.expiry_limit)
        .with_query_result_arc(self.material.query_result.clone())
        .with_package_facts_arc(self.material.package_facts.clone())
        .with_query_references_arc(self.material.query_references.clone())
        .with_provider_selection_arc(self.material.provider_selection.clone())
        .with_selected_indices(Arc::clone(&self.material.selected)))
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

    /// Returns the version-control revision retained by this immutable input.
    ///
    /// # Errors
    /// Returns the original captured metadata read failure.
    pub fn captured_revision(&self) -> Result<Option<rift_protocol::read::RevisionId>, RiftError> {
        let metadata: CapturedMetadata = serde_json::from_slice(&self.material.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        Ok(metadata.revision)
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
            Arc::clone(&self.material.history),
            self.material.traversal.clone(),
            self.ready,
        )
        .with_expiry_limit(self.expiry_limit)
        .with_query_result_arc(self.material.query_result.clone())
        .with_package_facts_arc(self.material.package_facts.clone())
        .with_query_references_arc(self.material.query_references.clone())
        .with_provider_selection_arc(self.material.provider_selection.clone())
        .with_selected_indices(Arc::clone(&self.material.selected)))
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

    pub(crate) fn index(&self) -> &rift_index::WorkspaceSymbolView {
        &self.material.index
    }

    /// Retains one encoded execution result under the existing material admission bound.
    #[must_use]
    pub fn with_query_result(self, bytes: Vec<u8>) -> Self {
        self.with_query_result_arc(Some(Arc::from(bytes)))
    }

    fn with_query_result_arc(mut self, bytes: Option<Arc<[u8]>>) -> Self {
        let previous = self
            .material
            .query_result
            .as_ref()
            .map_or(0, |bytes| bytes.len());
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(previous)
            .saturating_add(bytes.as_ref().map_or(0, |bytes| bytes.len()));
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            selected: Arc::clone(&self.material.selected),
            metadata: Arc::clone(&self.material.metadata),
            history: Arc::clone(&self.material.history),
            traversal: self.material.traversal.clone(),
            query_result: bytes,
            package_facts: self.material.package_facts.clone(),
            provider_selection: self.material.provider_selection.clone(),
            query_references: self.material.query_references.clone(),
        });
        self
    }

    /// Returns encoded results from this exact selection without copying their allocation.
    #[must_use]
    pub fn query_result(&self) -> Option<Arc<[u8]>> {
        self.material.query_result.clone()
    }

    /// Retains the original serving context after checking this immutable publication.
    ///
    /// # Errors
    /// Refuses a different root, owner or captured revision.
    pub fn with_provider_selection(
        self,
        selection: rift_search::query::QueryProviderSelection,
    ) -> Result<Self, RiftError> {
        self.validate_provider_selection(&selection)?;
        if self
            .captured_provider_selection()
            .is_some_and(|captured| captured != &selection)
        {
            return Err(invalid(
                "view",
                "captured query provider selection does not match",
            ));
        }
        Ok(self.with_provider_selection_arc(Some(Arc::new(selection))))
    }

    /// Returns the original serving context retained by this immutable publication.
    #[must_use]
    pub fn captured_provider_selection(
        &self,
    ) -> Option<&rift_search::query::QueryProviderSelection> {
        self.material.provider_selection.as_deref()
    }

    fn with_provider_selection_arc(
        mut self,
        selection: Option<Arc<rift_search::query::QueryProviderSelection>>,
    ) -> Self {
        let bytes = |selection: &rift_search::query::QueryProviderSelection| {
            get_size2::GetSize::get_size(selection).saturating_add(2 * std::mem::size_of::<usize>())
        };
        let previous = self.material.provider_selection.as_deref().map_or(0, bytes);
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(previous)
            .saturating_add(selection.as_deref().map_or(0, bytes));
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            selected: Arc::clone(&self.material.selected),
            metadata: Arc::clone(&self.material.metadata),
            history: Arc::clone(&self.material.history),
            traversal: self.material.traversal.clone(),
            query_result: self.material.query_result.clone(),
            package_facts: self.material.package_facts.clone(),
            provider_selection: selection,
            query_references: self.material.query_references.clone(),
        });
        self
    }

    fn with_expiry_limit(mut self, expiry_limit: Option<SystemTime>) -> Self {
        self.expiry_limit = expiry_limit;
        self
    }

    /// Combines selections of the same immutable workspace publication.
    ///
    /// # Errors
    /// Refuses selections backed by different publications.
    pub fn combine(mut self, other: &Self) -> Result<Self, SymbolUnavailableReason> {
        if !Arc::ptr_eq(&self.material.index, &other.material.index) {
            return Err(SymbolUnavailableReason::ViewContext);
        }
        match (
            &self.material.provider_selection,
            &other.material.provider_selection,
        ) {
            (Some(left), Some(right)) if left != right => {
                return Err(SymbolUnavailableReason::ViewContext);
            }
            (None, Some(selected)) => {
                self = self.with_provider_selection_arc(Some(Arc::clone(selected)));
            }
            _ => {}
        }
        self.expiry_limit = match (self.expiry_limit, other.expiry_limit) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        self.ready &= other.ready;
        for selected in other.material.selected.iter() {
            self = self
                .with_selected_view(Arc::clone(selected))
                .map_err(|_| SymbolUnavailableReason::ViewContext)?;
        }
        self = self
            .combine_histories(other)
            .map_err(|_| SymbolUnavailableReason::ViewContext)?;
        if let Some(facts) = other.material.package_facts.as_deref() {
            self = self
                .with_package_facts(facts.clone())
                .map_err(|_| SymbolUnavailableReason::ViewContext)?;
        }
        if let Some(references) = &other.material.query_references {
            let combined = match &self.material.query_references {
                Some(existing) if Arc::ptr_eq(existing, references) => Arc::clone(existing),
                Some(existing) => Arc::new(
                    existing
                        .combine_provider_references(references)
                        .map_err(|_| SymbolUnavailableReason::ViewContext)?,
                ),
                None => Arc::clone(references),
            };
            self = self.with_query_references_arc(Some(combined));
        }
        Ok(self)
    }

    /// Limits a derived selection to the provider's original expiry.
    ///
    /// # Errors
    /// Returns the original error when the provider expiry cannot be decoded.
    pub fn limited_by(mut self, view: &CapturedView) -> Result<Self, RiftError> {
        let expiry = chrono::DateTime::parse_from_rfc3339(&view.expires_at)
            .map_err(|error| material_error("read provider view expiry", &error))?;
        let expiry = SystemTime::from(expiry);
        self.expiry_limit = Some(self.expiry_limit.map_or(expiry, |held| held.min(expiry)));
        Ok(self)
    }
}

#[derive(Debug)]
struct LocalSymbolMaterial {
    index: Arc<rift_index::WorkspaceSymbolView>,
    selected: Arc<[Arc<rift_index::WorkspaceSymbolView>]>,
    metadata: Arc<[u8]>,
    history: Arc<history::CapturedHistories>,
    traversal: Option<(Arc<[u8]>, Arc<crate::EngineReferences>)>,
    query_result: Option<Arc<[u8]>>,
    package_facts: Option<Arc<rift_search::query::QueryFacts>>,
    provider_selection: Option<Arc<rift_search::query::QueryProviderSelection>>,
    query_references: Option<Arc<crate::EngineReferences>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CapturedMetadata {
    revision: Option<rift_protocol::read::RevisionId>,
    warnings: Vec<rift_protocol::read::ReadWarning>,
}

#[derive(Debug, Clone)]
struct HeldView {
    material: HeldMaterial,
    ready: bool,
    retained_bytes: usize,
    view: CapturedView,
    expires: SystemTime,
}

#[derive(Debug, Clone)]
enum HeldMaterial {
    Local(Arc<LocalSymbolMaterial>),
    Query(Arc<QueryMaterial>),
}

impl HeldMaterial {
    fn root(&self) -> &Path {
        match self {
            Self::Local(material) => material.index.root(),
            Self::Query(material) => material.root(),
        }
    }

    fn owner(&self) -> &SymbolOwner {
        match self {
            Self::Local(material) => material.index.owner(),
            Self::Query(material) => material.owner(),
        }
    }
}

struct CaptureInput {
    material: HeldMaterial,
    ready: bool,
    retained_bytes: usize,
    expiry_limit: Option<SystemTime>,
    index_bytes: usize,
}

#[derive(Debug, Clone)]
struct HeldLocalView {
    material: Arc<LocalSymbolMaterial>,
    ready: bool,
    retained_bytes: usize,
    view: CapturedView,
    expires: SystemTime,
}

impl HeldView {
    fn into_local(self) -> Result<HeldLocalView, SymbolUnavailableReason> {
        let HeldMaterial::Local(material) = self.material else {
            return Err(SymbolUnavailableReason::ViewContext);
        };
        Ok(HeldLocalView {
            material,
            ready: self.ready,
            retained_bytes: self.retained_bytes,
            view: self.view,
            expires: self.expires,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Continuation {
    view: CapturedViewId,
    id: SymbolId,
    include: Vec<GetSymbolInclude>,
    limit: u64,
    offset: usize,
}

impl Continuation {
    fn matches(&self, request: &GetSymbolParams) -> bool {
        self.id == request.id
            && self.include == request.include
            && self.limit == request.declaration_limit
            && request.view.as_ref().is_none_or(|view| view == &self.view)
            && request.rev.is_none()
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.view.as_str().len())
            .saturating_add(self.id.as_str().len())
            .saturating_add(self.include.capacity() * std::mem::size_of::<GetSymbolInclude>())
    }
}

#[derive(Debug, Default)]
struct State {
    views: BTreeMap<String, HeldView>,
    cursors: BTreeMap<String, Continuation>,
    expired: BTreeSet<String>,
    serial: u64,
}

impl State {
    fn expire(&mut self, now: SystemTime, views_max: usize) {
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
        while self.expired.len() > views_max {
            self.expired.pop_first();
        }
        self.cursors.retain(|key, cursor| {
            if self.views.contains_key(cursor.view.as_str()) {
                true
            } else {
                self.expired.insert(key.clone());
                false
            }
        });
        while self.expired.len() > views_max {
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
            .saturating_add(rift_protocol::btree_material_bytes::<String, Continuation>(
                self.cursors.len(),
            ))
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
            if let HeldMaterial::Local(material) = &held.material {
                for index in std::iter::once(&material.index).chain(material.selected.iter()) {
                    if !charged.track(Arc::as_ptr(index)) {
                        continue;
                    }
                    let bytes = if shared {
                        let (bytes, next) = index.as_ref().get_size_with_tracker(charged);
                        charged = next;
                        bytes
                    } else {
                        index.retained_material_bytes()
                    };
                    total = total
                        .saturating_add(bytes)
                        .saturating_add(2 * std::mem::size_of::<usize>());
                }
            }
            let first = match &held.material {
                HeldMaterial::Local(material) => charged.track(Arc::as_ptr(material)),
                HeldMaterial::Query(material) => charged.track(Arc::as_ptr(material)),
            };
            if first {
                total = total.saturating_add(held.retained_bytes);
            }
        }
        for (key, cursor) in &self.cursors {
            total = total
                .saturating_add(key.capacity())
                .saturating_add(cursor.retained_bytes());
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
    views_max: usize,
    view_lifetime: Duration,
    service_key: [u8; 32],
    state: Mutex<State>,
}

impl LocalSymbolViews {
    /// Reuses one selected publication without extending its original expiry.
    ///
    /// # Errors
    /// Returns the selected view's typed expiry or ownership refusal.
    pub fn selected_input(
        &self,
        id: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<LocalSymbolViewInput, SymbolUnavailableReason> {
        let held = self
            .selected_view_at(id, root, owner, SystemTime::now())?
            .into_local()?;
        Ok(LocalSymbolViewInput {
            material: held.material,
            ready: held.ready,
            retained_bytes: held.retained_bytes,
            expiry_limit: Some(held.expires),
        })
    }
    /// Creates a service-local authority store with a caller-owned material budget.
    #[must_use]
    pub fn new(material_bytes_max: usize, service_key: [u8; 32]) -> Self {
        Self {
            material_bytes_max,
            views_max: VIEW_SLOTS,
            view_lifetime: VIEW_LIFETIME,
            service_key,
            state: Mutex::new(State::default()),
        }
    }

    /// Creates a service-local store with accepted byte, count and lifetime bounds.
    ///
    /// # Errors
    /// Returns a registered read refusal for a zero count, byte bound or lifetime.
    pub fn bounded(
        material_bytes_max: usize,
        service_key: [u8; 32],
        views_max: usize,
        view_lifetime: Duration,
    ) -> Result<Self, RiftError> {
        if material_bytes_max == 0 || views_max == 0 || view_lifetime.is_zero() {
            return Err(invalid("view", "captured view bounds must be positive"));
        }
        let mut views = Self::new(material_bytes_max, service_key);
        views.views_max = views_max;
        views.view_lifetime = view_lifetime;
        Ok(views)
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
        let mut expiry_limit = input.expiry_limit;
        if let Some((_, references)) = &input.material.traversal {
            for selected in references.package_views() {
                let selected = chrono::DateTime::parse_from_rfc3339(&selected.expires_at)
                    .map_err(|error| material_error("read package view expiry", &error))?;
                let selected = SystemTime::from(selected);
                expiry_limit = Some(expiry_limit.map_or(selected, |limit| limit.min(selected)));
            }
        }
        let index_bytes = input.material.index.retained_material_bytes();
        self.capture_material_with_at(
            CaptureInput {
                material: HeldMaterial::Local(input.material),
                ready: input.ready,
                retained_bytes: input.retained_bytes,
                expiry_limit,
                index_bytes,
            },
            now,
            admit,
        )
    }

    fn capture_material_with_at(
        &self,
        input: CaptureInput,
        now: SystemTime,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(now, self.views_max);
        if input.retained_bytes == 0 || state.views.len() >= self.views_max {
            rift_tracing::warn!(
                component = "server",
                rows = state.views.len(),
                views_max = self.views_max,
                incoming_bytes = input.retained_bytes,
                index_bytes = input.index_bytes,
                ready = input.ready,
                bytes_max = self.material_bytes_max,
                "retained read material exceeds the byte or view bound"
            );
            return Ok(None);
        }
        let retained_bytes = input.retained_bytes;
        let index_bytes = input.index_bytes;
        let ready = input.ready;
        let id = self.next_key(&mut state)?;
        let mut expires = now
            .checked_add(self.view_lifetime)
            .ok_or_else(|| invalid("view", "view expiry exceeds the clock bound"))?;
        if let Some(limit) = input.expiry_limit {
            expires = expires.min(limit);
        }
        if expires <= now {
            return Ok(None);
        }
        let elapsed = expires
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| invalid("view", "view expiry exceeds the clock bound"))?;
        let seconds = i64::try_from(elapsed.as_secs())
            .map_err(|_| invalid("view", "view expiry exceeds the clock bound"))?;
        let timestamp = chrono::DateTime::from_timestamp(seconds, elapsed.subsec_nanos())
            .ok_or_else(|| invalid("view", "view expiry exceeds the clock bound"))?;
        let expires_at = timestamp.to_rfc3339();
        let decoded = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map_err(|_| invalid("view", "view expiry exceeds the clock bound"))?;
        if SystemTime::from(decoded) != expires {
            return Err(invalid("view", "view expiry exceeds the clock bound"));
        }
        let view = CapturedView {
            id: CapturedViewId::parse(&id)?,
            expires_at,
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
                cursors = state.cursors.len(),
                views_max = self.views_max,
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
        let held = self.selected_view_at(id, root, owner, now)?;
        if !held.ready {
            return Err(SymbolUnavailableReason::IndexPreparing);
        }
        Ok(held.view)
    }

    fn held_view_at(
        &self,
        id: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
        now: SystemTime,
    ) -> Result<HeldLocalView, SymbolUnavailableReason> {
        let held = self.selected_view_at(id, root, owner, now)?;
        if !held.ready {
            return Err(SymbolUnavailableReason::IndexPreparing);
        }
        held.into_local()
    }

    fn selected_view_at(
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
        state.expire(now, self.views_max);
        let Some(held) = state.views.get(id.as_str()) else {
            return Err(if state.expired.contains(id.as_str()) {
                SymbolUnavailableReason::ViewExpired
            } else {
                SymbolUnavailableReason::ViewContext
            });
        };
        if held.material.root() != root || held.material.owner() != owner {
            return Err(SymbolUnavailableReason::ViewContext);
        }
        Ok(held.clone())
    }

    /// Reads only an issued view or continuation belonging to the expected root and owner.
    ///
    /// # Errors
    /// Returns registered request or captured provider failures; no failure selects latest.
    pub fn resolve(
        &self,
        request: &GetSymbolParams,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<GetSymbolResult, RiftError> {
        self.resolve_at(request, root, owner, SystemTime::now())
    }

    fn resolve_at(
        &self,
        request: &GetSymbolParams,
        root: &Path,
        owner: &SymbolOwner,
        now: SystemTime,
    ) -> Result<GetSymbolResult, RiftError> {
        let identity = symbol_request_identity(request)?;
        let (held, offset) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.expire(now, self.views_max);
            let continuation = match &request.declaration_cursor {
                Some(key) => match state.cursors.get(key) {
                    Some(cursor) if cursor.matches(request) => Some(cursor),
                    _ => {
                        let reason = if state.expired.contains(key) {
                            SymbolUnavailableReason::ViewExpired
                        } else {
                            SymbolUnavailableReason::ViewContext
                        };
                        return Ok(unavailable(request, None, reason));
                    }
                },
                None => None,
            };
            let selected = continuation
                .map(|cursor| &cursor.view)
                .or(request.view.as_ref());
            let Some(selected) = selected else {
                return Err(invalid(
                    "view",
                    "select the view returned by a captured read",
                ));
            };
            let Some(held) = state.views.get(selected.as_str()) else {
                let reason = if state.expired.contains(selected.as_str()) {
                    SymbolUnavailableReason::ViewExpired
                } else {
                    SymbolUnavailableReason::ViewContext
                };
                return Ok(unavailable(request, None, reason));
            };
            let held = match held.clone().into_local() {
                Ok(held) => held,
                Err(reason) => return Ok(unavailable(request, Some(held.view.clone()), reason)),
            };
            if held.material.index.root() != root
                || held.material.index.owner() != owner
                || (identity.owner() != owner && !held.material.admits_package_symbol(&request.id))
            {
                return Ok(unavailable(
                    request,
                    Some(held.view.clone()),
                    SymbolUnavailableReason::ViewContext,
                ));
            }
            (held.clone(), continuation.map_or(0, |cursor| cursor.offset))
        };
        if !held.ready {
            return Ok(unavailable(
                request,
                Some(held.view),
                SymbolUnavailableReason::IndexPreparing,
            ));
        }
        let (mut result, next) = held
            .material
            .canonical_symbol(request, &held.view, offset)?;
        if let Some(offset) = next {
            let continuation = Continuation {
                view: held.view.id.clone(),
                id: request.id.clone(),
                include: request.include.clone(),
                limit: request.declaration_limit,
                offset,
            };
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.expire(now, self.views_max);
            let key = self.next_key(&mut state)?;
            state.cursors.insert(key.clone(), continuation);
            if state.admitted_material_bytes(self.material_bytes_max) > self.material_bytes_max {
                state.cursors.remove(&key);
                return Ok(unavailable(
                    request,
                    Some(held.view),
                    SymbolUnavailableReason::InsufficientCoverage,
                ));
            }
            if let GetSymbolResult::Found {
                declaration_cursor, ..
            } = &mut result
            {
                *declaration_cursor = Some(key);
            }
        }
        Ok(result)
    }
}

fn symbol_request_identity(request: &GetSymbolParams) -> Result<SymbolIdentity, RiftError> {
    let identity = SymbolIdentity::parse(request.id.as_str())?;
    super::accepted_declaration_limit(request.declaration_limit)?;
    if request.rev.is_some() && (request.view.is_some() || request.declaration_cursor.is_some()) {
        return Err(invalid(
            "rev",
            "rev cannot accompany a captured view or continuation",
        ));
    }
    Ok(identity)
}

fn invalid(field: &'static str, violation: &'static str) -> RiftError {
    errors::server::read_invalid()
        .field(field)
        .violation(violation)
        .error()
}

fn unavailable(
    request: &GetSymbolParams,
    view: Option<CapturedView>,
    reason: SymbolUnavailableReason,
) -> GetSymbolResult {
    GetSymbolResult::Unavailable {
        id: request.id.clone(),
        view,
        reason,
        warnings: Vec::new(),
    }
}

impl ReadService {
    /// Captures immutable search, source, graph and documentation material without history.
    ///
    /// # Errors
    /// Returns the original captured index or read failure.
    pub fn local_search_view_input(&self, ready: bool) -> Result<LocalSymbolViewInput, RiftError> {
        let (index, metadata) = self.local_view_material()?;
        Ok(local_view_input(
            index,
            metadata,
            Arc::default(),
            None,
            ready,
        ))
    }

    /// Captures one traversal's engine rows from the same source and index revision.
    ///
    /// # Errors
    /// Returns the original index or read failure, or refuses rows from another revision.
    pub fn local_traversal_view_input(
        &self,
        traversal: &rift_protocol::search::SearchTraversal,
        references: crate::EngineReferences,
        ready: bool,
    ) -> Result<LocalSymbolViewInput, RiftError> {
        crate::traversal::validate_traversal(traversal)?;
        if traversal.seed.is_none() {
            return Err(invalid("seed", "a traversal starts at seed"));
        }
        references.validate_revision(self)?;
        let (index, metadata) = self.local_view_material()?;
        let traversal = if ready && references.captured_for(self) {
            let request = serde_json::to_vec(traversal)
                .map_err(|error| material_error("capture traversal", &error))?;
            Some((Arc::from(request), Arc::new(references)))
        } else {
            None
        };
        Ok(local_view_input(
            index,
            metadata,
            Arc::default(),
            traversal,
            ready,
        ))
    }

    fn local_view_material(
        &self,
    ) -> Result<(Arc<rift_index::WorkspaceSymbolView>, Vec<u8>), RiftError> {
        let index = self.index.symbol_view()?;
        let metadata = serde_json::to_vec(&CapturedMetadata {
            revision: self.revision.clone(),
            warnings: self.warnings(),
        })
        .map_err(|error| material_error("capture read metadata", &error))?;
        Ok((index, metadata))
    }

    /// Captures immutable read material and the requested bounded history before admission.
    ///
    /// # Errors
    /// Returns the original captured index, history or read failure.
    pub fn local_symbol_view_input(
        &self,
        request: &GetSymbolParams,
        ready: bool,
    ) -> Result<LocalSymbolViewInput, RiftError> {
        let key = rift_core::SymbolId::new(request.id.as_str())?;
        let (index, metadata) = self.local_view_material()?;
        let history = if ready && request.include.contains(&GetSymbolInclude::History) {
            let (bindings, _) = self.index.symbols_by_identity_page(&key, 0, 1)?;
            match bindings.first().copied() {
                Some(matched) => {
                    let mut timelines = crate::history::SymbolTimelines::open_captured(
                        self.index.root(),
                        self.revision.as_ref(),
                        &self.history,
                        self.index.limits().syntax(),
                        self.stored_history.get(),
                    )?;
                    let history = timelines.timeline(
                        super::language_provider(matched.file.syntax().language()),
                        matched,
                        request.id.clone(),
                    )?;
                    let bytes = serde_json::to_vec(&history)
                        .map_err(|error| material_error("capture symbol history", &error))?;
                    Some((request.id.as_str().to_owned(), Arc::<[u8]>::from(bytes)))
                }
                None => None,
            }
        } else {
            None
        };
        Ok(local_view_input(
            index,
            metadata,
            Arc::new(history::CapturedHistories::single(history)),
            None,
            ready,
        ))
    }
}

fn material_error(operation: &str, error: &impl std::fmt::Display) -> RiftError {
    errors::server::read_unavailable()
        .operation(operation)
        .detail(error.to_string())
        .error()
}

impl LocalSymbolMaterial {
    fn canonical_symbol(
        &self,
        request: &GetSymbolParams,
        view: &CapturedView,
        offset: usize,
    ) -> Result<(GetSymbolResult, Option<usize>), RiftError> {
        let identity = SymbolIdentity::parse(request.id.as_str())?;
        if identity.owner() != self.index.owner() {
            return self.package_symbol(request, view, offset);
        }
        let key = rift_core::SymbolId::new(request.id.as_str())?;
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        if request
            .rev
            .as_ref()
            .is_some_and(|revision| Some(revision) != metadata.revision.as_ref())
        {
            return Ok((
                unavailable(
                    request,
                    Some(view.clone()),
                    SymbolUnavailableReason::ViewContext,
                ),
                None,
            ));
        }
        let Some(readable) = self.selected_object(&key)? else {
            if !self
                .indices()
                .all(|index| index.identity_coverage_complete(identity.language()))
            {
                return Ok((
                    unavailable(
                        request,
                        Some(view.clone()),
                        SymbolUnavailableReason::InsufficientCoverage,
                    ),
                    None,
                ));
            }
            let (alternatives, detail) = self.symbol_alternatives(&identity)?;
            return Ok((
                GetSymbolResult::Missing {
                    symbol_not_found: SymbolNotFound {
                        id: request.id.clone(),
                        alternatives,
                        detail,
                    },
                    view: view.clone(),
                    warnings: metadata.warnings,
                },
                None,
            ));
        };
        let symbol = readable.to_protocol_symbol();
        if !symbol.name_is_public() {
            return Ok((
                self.refused_public_symbol(request, view, &key, readable.facts().qualified_name())?,
                None,
            ));
        }
        let (declarations, total) = self.declarations(request, &key, offset)?;
        let history = self.captured_history(request)?;
        if request.include.contains(&GetSymbolInclude::History) && history.is_none() {
            return Ok((
                unavailable(
                    request,
                    Some(view.clone()),
                    SymbolUnavailableReason::InsufficientCoverage,
                ),
                None,
            ));
        }
        let documentation = request
            .include
            .contains(&GetSymbolInclude::Documentation)
            .then(|| {
                let mut bytes_left =
                    rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
                rift_index::documentation_context_with_budget(
                    self.index.documentation(),
                    &request.id,
                    |source| self.index.documentation_content(source),
                    &mut bytes_left,
                )
            });
        let mut warnings = metadata.warnings;
        warnings.extend(super::symbol_disagreement_warning(readable.assembled()));
        let next = offset.saturating_add(declarations.len());
        Ok((
            GetSymbolResult::Found {
                symbol: Box::new(symbol),
                view: view.clone(),
                declarations,
                declaration_cursor: None,
                history,
                documentation,
                warnings,
            },
            (next < total).then_some(next),
        ))
    }

    fn symbol_alternatives(
        &self,
        identity: &SymbolIdentity,
    ) -> Result<
        (
            Vec<rift_protocol::symbol_read::SymbolAlternative>,
            Option<String>,
        ),
        RiftError,
    > {
        use rift_protocol::symbol_read::{SymbolAlternative, SymbolMatchContext};
        let proposed = crate::alternatives::captured_symbols(&self.index, identity);
        let keys = match proposed {
            Ok(keys) => keys,
            Err(error)
                if error.slug() == errors::server::read_symbol_alternatives_unavailable::SLUG =>
            {
                return Ok((Vec::new(), Some(error.to_string())));
            }
            Err(error) => return Err(error),
        };
        let mut alternatives = Vec::with_capacity(keys.len());
        for key in keys {
            let key_value = rift_core::SymbolId::new(key.as_str())?;
            let Some(symbol) = self.index.object_by_identity(&key_value)? else {
                continue;
            };
            let name = symbol.facts().qualified_name();
            if !rift_protocol::read::symbol_name_is_public(name) {
                continue;
            }
            alternatives.push(SymbolAlternative {
                id: key,
                name: name.to_owned(),
                match_context: vec![
                    SymbolMatchContext::Owner,
                    SymbolMatchContext::Language,
                    SymbolMatchContext::Name,
                ],
            });
        }
        Ok((alternatives, None))
    }

    fn refused_public_symbol(
        &self,
        request: &GetSymbolParams,
        view: &CapturedView,
        key: &rift_core::SymbolId,
        qualified_name: &str,
    ) -> Result<GetSymbolResult, RiftError> {
        let mut result = unavailable(
            request,
            Some(view.clone()),
            SymbolUnavailableReason::InsufficientCoverage,
        );
        if let GetSymbolResult::Unavailable { warnings, .. } = &mut result {
            let (bindings, _) = self.index.symbols_by_identity_page(key, 0, 1)?;
            warnings.push(bindings.first().copied().map_or_else(
                || super::object_unavailable_at(qualified_name, None, None),
                super::object_unavailable,
            ));
        }
        Ok(result)
    }

    fn declarations(
        &self,
        request: &GetSymbolParams,
        key: &rift_core::SymbolId,
        offset: usize,
    ) -> Result<(Vec<rift_protocol::symbol_read::SymbolDeclaration>, usize), RiftError> {
        let limit = super::accepted_declaration_limit(request.declaration_limit)?
            .min(self.index.results_max());
        let mut declarations = Vec::new();
        let mut total = 0usize;
        for (owner, index) in self.indices().enumerate() {
            for binding in index.canonical_declarations(key) {
                let (path, position, matched, declaration) = binding?;
                let mut duplicate = false;
                for previous in self.indices().take(owner) {
                    if let Some(held) = previous.canonical_declaration(key, path, position) {
                        if held != declaration {
                            return Err(invalid("view", "captured declaration bindings disagree"));
                        }
                        duplicate = true;
                    }
                }
                if duplicate {
                    continue;
                }
                if total >= offset && declarations.len() < limit {
                    let mut declaration = declaration.clone();
                    if request.include.contains(&GetSymbolInclude::Source) {
                        declaration.node = Some(super::node_id(matched.file, matched.symbol.range));
                        declaration.source =
                            Some(super::excerpt(matched.file, matched.symbol.range));
                        declaration.source_complete = true;
                    }
                    declarations.push(declaration);
                }
                total = total.checked_add(1).ok_or_else(|| {
                    invalid(
                        "view",
                        "captured declaration count exceeds the platform bound",
                    )
                })?;
            }
        }
        Ok((declarations, total))
    }

    fn captured_history(
        &self,
        request: &GetSymbolParams,
    ) -> Result<Option<rift_protocol::read::SymbolHistory>, RiftError> {
        if !request.include.contains(&GetSymbolInclude::History) {
            return Ok(None);
        }
        match self.history.get(request.id.as_str()) {
            Some(bytes) => serde_json::from_slice(bytes)
                .map(Some)
                .map_err(|error| material_error("decode captured history", &error)),
            _ => Ok(None),
        }
    }
}

fn local_view_input(
    index: Arc<rift_index::WorkspaceSymbolView>,
    metadata: Vec<u8>,
    history: Arc<history::CapturedHistories>,
    traversal: Option<(Arc<[u8]>, Arc<crate::EngineReferences>)>,
    ready: bool,
) -> LocalSymbolViewInput {
    let material = Arc::new(LocalSymbolMaterial {
        index,
        selected: Arc::from([]),
        metadata: Arc::from(metadata),
        history,
        traversal,
        query_result: None,
        package_facts: None,
        provider_selection: None,
        query_references: None,
    });
    let retained_bytes = std::mem::size_of::<LocalSymbolMaterial>()
        .saturating_add(6 * std::mem::size_of::<usize>())
        .saturating_add(material.metadata.len())
        .saturating_add(material.history.retained_bytes())
        .saturating_add(
            material
                .traversal
                .as_ref()
                .map_or(0, |(request, references)| {
                    request
                        .len()
                        .saturating_add(4 * std::mem::size_of::<usize>())
                        .saturating_add(references.retained_material_bytes())
                }),
        );
    LocalSymbolViewInput {
        material,
        ready,
        retained_bytes,
        expiry_limit: None,
    }
}

mod history;
mod nodes;
mod package;
mod query;
mod query_references;
pub use query::{CapturedQueryView, QueryMaterial, QueryViewInput};
mod selected;
mod source;
mod traversal;

#[cfg(test)]
mod tests;
