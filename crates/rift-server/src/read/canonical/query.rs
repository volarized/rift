//! Query material retained by the existing captured-view admission owner.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

use super::{CaptureInput, HeldMaterial, LocalSymbolViews, invalid};

/// Owned query bytes admitted together under one captured view.
#[derive(Debug)]
pub struct QueryViewInput {
    root: PathBuf,
    owner: SymbolOwner,
    records: Vec<u8>,
    metadata: Vec<u8>,
    facts: Vec<u8>,
    sources: Vec<u8>,
    expires: SystemTime,
}

impl QueryViewInput {
    /// Starts a query selection limited by its original provider expiry.
    ///
    /// # Errors
    /// Refuses an empty root or invalid owner.
    pub fn new(root: PathBuf, owner: SymbolOwner, expires: SystemTime) -> Result<Self, RiftError> {
        if root.as_os_str().is_empty() {
            return Err(invalid("root", "query root must not be empty"));
        }
        owner.validate()?;
        Ok(Self {
            root,
            owner,
            records: Vec::new(),
            metadata: Vec::new(),
            facts: Vec::new(),
            sources: Vec::new(),
            expires,
        })
    }

    /// Retains the encoded answer and its existing record offsets.
    #[must_use]
    pub fn with_records(mut self, records: Vec<u8>) -> Self {
        self.records = records;
        self
    }

    /// Retains bounded paging, provider and source lookup metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Vec<u8>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Retains encoded original facts without copying their captured source bodies.
    #[must_use]
    pub fn with_facts(mut self, facts: Vec<u8>) -> Self {
        self.facts = facts;
        self
    }

    /// Retains original UTF-8 source bodies addressed by admitted metadata offsets.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<u8>) -> Self {
        self.sources = sources;
        self
    }

    pub(super) fn into_capture(self) -> Result<CaptureInput, RiftError> {
        if std::str::from_utf8(&self.sources).is_err() {
            return Err(invalid("source", "captured source bytes must be UTF-8"));
        }
        let material = QueryMaterial {
            root: self.root,
            owner: self.owner,
            records: Arc::from(self.records),
            metadata: Arc::from(self.metadata),
            facts: Arc::from(self.facts),
            sources: Arc::from(self.sources),
        };
        let retained_bytes = std::mem::size_of::<QueryMaterial>()
            .saturating_add(10 * std::mem::size_of::<usize>())
            .saturating_add(get_size2::GetSize::get_heap_size(&material.root))
            .saturating_add(get_size2::GetSize::get_heap_size(&material.owner))
            .saturating_add(material.records.len())
            .saturating_add(material.metadata.len())
            .saturating_add(material.facts.len())
            .saturating_add(material.sources.len());
        Ok(CaptureInput {
            material: HeldMaterial::Query(Arc::new(material)),
            ready: true,
            retained_bytes,
            expiry_limit: Some(self.expires),
            index_bytes: 0,
        })
    }
}

/// Immutable query bytes sharing the captured-view byte, count and expiry bounds.
#[derive(Debug)]
pub struct QueryMaterial {
    root: PathBuf,
    owner: SymbolOwner,
    records: Arc<[u8]>,
    metadata: Arc<[u8]>,
    facts: Arc<[u8]>,
    sources: Arc<[u8]>,
}

impl QueryMaterial {
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) const fn owner(&self) -> &SymbolOwner {
        &self.owner
    }

    /// Encoded answer records, read through their admitted page offsets.
    #[must_use]
    pub fn records(&self) -> &[u8] {
        &self.records
    }

    /// Bounded lookup metadata; source and page reads do not decode original facts.
    #[must_use]
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    /// Original fact context, decoded only for an admitted query acquisition.
    #[must_use]
    pub fn facts(&self) -> &[u8] {
        &self.facts
    }

    /// Exact original source bytes, read through validated UTF-8 offsets.
    #[must_use]
    pub fn sources(&self) -> &[u8] {
        &self.sources
    }
}

/// One selected query material and its unchanged captured expiry.
#[derive(Clone, Debug)]
pub struct CapturedQueryView {
    material: Arc<QueryMaterial>,
    view: CapturedView,
}

impl CapturedQueryView {
    /// Returns the original captured query material without copying its bytes.
    #[must_use]
    pub fn material(&self) -> &QueryMaterial {
        &self.material
    }

    /// Returns the service-issued view and its original expiry.
    #[must_use]
    pub const fn view(&self) -> &CapturedView {
        &self.view
    }
}

impl LocalSymbolViews {
    /// Captures query bytes under the same admission and keys as other exact reads.
    ///
    /// The callback admits the bounded response while this store is locked. It must
    /// not call this store. Refusal removes only the new query selection.
    ///
    /// # Errors
    /// Returns a registered key, expiry or response admission failure.
    pub fn capture_query_with(
        &self,
        input: QueryViewInput,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.capture_material_with_at(input.into_capture()?, SystemTime::now(), admit)
    }

    /// Selects only an issued query view under its original root, owner and expiry.
    ///
    /// # Errors
    /// Refuses expired, unknown, non-query or differently owned selections.
    pub fn query_view(
        &self,
        id: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<CapturedQueryView, SymbolUnavailableReason> {
        let held = self.selected_view_at(id, root, owner, SystemTime::now())?;
        let HeldMaterial::Query(material) = held.material else {
            return Err(SymbolUnavailableReason::ViewContext);
        };
        Ok(CapturedQueryView {
            material,
            view: held.view,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn query_material_shares_local_admission_and_preserves_original_expiry() {
        let (_directory, reads, _) = super::super::tests::fixture(true);
        let store = LocalSymbolViews::new(1 << 20, [82; 32]);
        let root = reads.index.root();
        let owner = reads.index.owner();
        let now = SystemTime::now();
        let expires = now + Duration::from_secs(60);
        let source = "pub fn beacon() {}\n".repeat(4096);
        let input = QueryViewInput::new(root.to_path_buf(), owner.clone(), expires)
            .expect("original query context")
            .with_records(b"[]".to_vec())
            .with_metadata(b"{}".to_vec())
            .with_facts(b"{}".to_vec())
            .with_sources(source.as_bytes().to_vec());
        let view = store
            .capture_material_with_at(input.into_capture().expect("query bytes"), now, |_| {
                Ok(true)
            })
            .expect("query admission")
            .expect("captured query");
        let selected = store
            .query_view(&view.id, root, owner)
            .expect("original query");
        assert_eq!(selected.view(), &view);
        assert_eq!(
            selected.material().sources().get(4..12),
            source.as_bytes().get(4..12)
        );
        assert_eq!(selected.material().facts(), b"{}");
        assert!(matches!(
            store.selected_input(&view.id, root, owner),
            Err(SymbolUnavailableReason::ViewContext)
        ));
        assert!(matches!(
            store.selected_view_at(&view.id, root, owner, expires),
            Err(SymbolUnavailableReason::ViewExpired)
        ));
        assert!(store.state.lock().expect("view state").views.is_empty());

        let input = reads
            .local_search_view_input(true)
            .expect("local publication");
        let local = store
            .capture(input)
            .expect("local capture")
            .expect("local view");
        assert!(matches!(
            store.query_view(&local.id, root, owner),
            Err(SymbolUnavailableReason::ViewContext)
        ));
        let refused = QueryViewInput::new(root.to_path_buf(), owner.clone(), expires)
            .expect("original query context")
            .with_sources(source.into_bytes());
        assert!(
            store
                .capture_query_with(refused, |_| Ok(false))
                .expect("response refusal")
                .is_none()
        );
        assert_eq!(store.state.lock().expect("view state").views.len(), 1);
        assert!(store.selected_input(&local.id, root, owner).is_ok());
    }

    #[test]
    fn query_material_refuses_invalid_utf8_and_zero_admission() {
        let root = PathBuf::from("/workspace");
        let expiry = SystemTime::now() + Duration::from_secs(60);
        let input = QueryViewInput::new(root.clone(), SymbolOwner::Local, expiry)
            .expect("query context")
            .with_sources(vec![0xff]);
        assert!(input.into_capture().is_err());
        let store = LocalSymbolViews::new(0, [83; 32]);
        let input = QueryViewInput::new(root, SymbolOwner::Local, expiry)
            .expect("query context")
            .with_records(b"[]".to_vec());
        assert!(
            store
                .capture_query_with(input, |_| Ok(true))
                .expect("bounded query admission")
                .is_none()
        );
        assert!(store.state.lock().expect("view state").views.is_empty());
    }
}
