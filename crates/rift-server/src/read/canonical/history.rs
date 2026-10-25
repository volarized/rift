//! Encoded recorded timelines owned by the original captured local material.

use std::collections::BTreeMap;
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::SymbolHistory;

use super::{LocalSymbolMaterial, LocalSymbolViewInput, material_error};

#[derive(Debug, Default)]
pub(super) struct CapturedHistories {
    rows: BTreeMap<String, Arc<[u8]>>,
    complete: bool,
}

impl CapturedHistories {
    pub(super) fn single(row: Option<(String, Arc<[u8]>)>) -> Self {
        Self {
            rows: row.into_iter().collect(),
            complete: false,
        }
    }

    pub(super) fn get(&self, identity: &str) -> Option<&[u8]> {
        self.rows.get(identity).map(AsRef::as_ref)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.rows.iter().fold(
            std::mem::size_of::<Self>()
                .saturating_add(2 * std::mem::size_of::<usize>())
                .saturating_add(rift_protocol::btree_material_bytes::<String, Arc<[u8]>>(
                    self.rows.len(),
                )),
            |bytes, (id, encoded)| {
                bytes
                    .saturating_add(id.capacity())
                    .saturating_add(encoded.len())
                    .saturating_add(2 * std::mem::size_of::<usize>())
            },
        )
    }
}

impl LocalSymbolViewInput {
    pub(crate) fn captured_histories(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.material
            .history
            .rows
            .iter()
            .map(|(id, bytes)| (id.as_str(), bytes.as_ref()))
    }

    pub(crate) fn captured_histories_complete(&self) -> bool {
        self.material.history.complete
    }

    /// Retains recorded timelines under this original local publication and expiry.
    ///
    /// # Errors
    /// Refuses another owner, uncaptured canonical symbols, conflicting timelines,
    /// incomplete catalog claims, cancellation or exhausted request admission.
    pub fn with_histories(
        self,
        timelines: &[SymbolHistory],
        complete: bool,
        limits: rift_search::query::QueryLimits,
        budget: &mut crate::QueryFactBudget<'_>,
    ) -> Result<Self, RiftError> {
        if timelines.is_empty() && !complete {
            return Ok(self);
        }
        budget.check()?;
        if timelines.len() > limits.quads_max || self.material.history.rows.len() > limits.quads_max
        {
            return rift_error::errors::server::read_unavailable()
                .operation("capture symbol history")
                .detail("captured timelines exceed search.query.statements")
                .fail();
        }
        budget.charge_material(self.material.history.retained_bytes())?;
        let mut rows = self.material.history.rows.clone();
        for timeline in timelines {
            budget.check()?;
            budget.charge(timeline)?;
            budget.charge(&timeline.symbol)?;
            let identity = SymbolIdentity::parse(timeline.symbol.as_str())?;
            let key = rift_core::SymbolId::new(timeline.symbol.as_str())?;
            if identity.owner() != self.owner()
                || self.index().object_by_identity(&key)?.is_none()
                || self
                    .index()
                    .symbols_by_identity_page(&key, 0, 1)?
                    .0
                    .is_empty()
            {
                return Err(material_error(
                    "capture symbol history",
                    &"timeline belongs to another captured symbol",
                ));
            }
            if !rows.contains_key(timeline.symbol.as_str()) && rows.len() >= limits.quads_max {
                return rift_error::errors::server::read_unavailable()
                    .operation("capture symbol history")
                    .detail("captured timelines exceed search.query.statements")
                    .fail();
            }
            budget.charge_material(rift_protocol::btree_material_bytes::<String, Arc<[u8]>>(1))?;
            let encoded = serde_json::to_vec(timeline)
                .map_err(|error| material_error("capture symbol history", &error))?;
            if let Some(existing) = rows.get(timeline.symbol.as_str()) {
                if existing.as_ref() != encoded.as_slice() {
                    return Err(material_error(
                        "capture symbol history",
                        &"captured timelines disagree within the selection",
                    ));
                }
            } else {
                rows.insert(timeline.symbol.as_str().to_owned(), Arc::from(encoded));
            }
        }
        if complete {
            for (identity, object) in self.index().objects() {
                budget.check()?;
                object?;
                let (bindings, _) = self.index().symbols_by_identity_page(identity, 0, 1)?;
                if bindings.is_empty() {
                    continue;
                }
                let encoded = rows.get(identity.as_str()).ok_or_else(|| {
                    material_error(
                        "capture symbol history",
                        &"captured timeline catalog is incomplete",
                    )
                })?;
                budget.charge_bytes(encoded)?;
                let history: SymbolHistory = serde_json::from_slice(encoded)
                    .map_err(|error| material_error("decode captured history", &error))?;
                if !history.complete || history.symbol.as_str() != identity.as_str() {
                    return Err(material_error(
                        "capture symbol history",
                        &"captured timeline catalog is incomplete",
                    ));
                }
            }
        }
        let complete = complete || self.material.history.complete;
        Ok(self.with_histories_arc(Arc::new(CapturedHistories { rows, complete })))
    }

    pub(super) fn combine_histories(self, other: &Self) -> Result<Self, RiftError> {
        if Arc::ptr_eq(&self.material.history, &other.material.history) {
            return Ok(self);
        }
        let mut rows = self.material.history.rows.clone();
        for (identity, bytes) in &other.material.history.rows {
            if let Some(existing) = rows.get(identity) {
                if existing.as_ref() != bytes.as_ref() {
                    return Err(material_error(
                        "capture symbol history",
                        &"captured timelines disagree within the selection",
                    ));
                }
            } else {
                rows.insert(identity.clone(), Arc::clone(bytes));
            }
        }
        let complete = self.material.history.complete || other.material.history.complete;
        Ok(self.with_histories_arc(Arc::new(CapturedHistories { rows, complete })))
    }

    fn with_histories_arc(mut self, history: Arc<CapturedHistories>) -> Self {
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(self.material.history.retained_bytes())
            .saturating_add(history.retained_bytes());
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            metadata: Arc::clone(&self.material.metadata),
            history,
            traversal: self.material.traversal.clone(),
            query_result: self.material.query_result.clone(),
            package_facts: self.material.package_facts.clone(),
            provider_selection: self.material.provider_selection.clone(),
            query_references: self.material.query_references.clone(),
        });
        self
    }
}
