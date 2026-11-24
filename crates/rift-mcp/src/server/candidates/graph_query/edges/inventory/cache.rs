//! Inventory answers retained by their source and configuration publication.

use std::sync::Arc;

use rift_protocol::read::ReadWarning;
use rift_search::sparql::GraphLimits;
use rift_server::EngineInventorySelection;
use tokio::time::Instant;

use super::InventoryRead;

/// Engine answers owned by one published source and configuration.
#[derive(Debug, Default)]
pub(crate) struct EngineInventoryCache {
    pub(super) entries: tokio::sync::Mutex<InventoryEntries>,
}

#[cfg(test)]
impl EngineInventoryCache {
    /// Forgets every answer, so the next read asks the engine again.
    pub(crate) async fn clear(&self) {
        self.entries.lock().await.reads.clear();
    }
}

#[derive(Debug, Default)]
pub(super) struct InventoryEntries {
    reads: Vec<SavedInventory>,
}

#[derive(Debug)]
struct SavedInventory {
    selection: EngineInventorySelection,
    read: Arc<InventoryRead>,
    expires: Option<Instant>,
}

impl InventoryEntries {
    pub(super) fn get(
        &mut self,
        selection: EngineInventorySelection,
        now: Instant,
    ) -> Option<Arc<InventoryRead>> {
        self.reads
            .retain(|held| held.expires.is_none_or(|end| now < end));
        self.reads
            .iter()
            .find(|held| held.selection == selection)
            .map(|held| Arc::clone(&held.read))
    }

    pub(super) fn remember(
        &mut self,
        selection: EngineInventorySelection,
        read: Arc<InventoryRead>,
        end: Instant,
    ) {
        // Installed bytes and engine positions belong to this request's capture.
        // Reread external targets on the next request rather than retain unverified files.
        if !read.installed.is_empty() || !read.named_package_symbols.is_empty() {
            return;
        }
        // Let the next read observe engine progress before this request's deadline.
        if read
            .warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::EngineAnalysisUnavailable { .. }))
        {
            return;
        }
        let maximum = GraphLimits::default().triples_max();
        let charged = read.records.saturating_add(1);
        if charged > maximum {
            return;
        }
        self.reads.retain(|held| held.selection != selection);
        while !self.reads.is_empty()
            && self
                .reads
                .iter()
                .map(|held| held.read.records.saturating_add(1))
                .sum::<usize>()
                .saturating_add(charged)
                > maximum
        {
            self.reads.remove(0);
        }
        let provisional = read.partial
            || read
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::EngineReadinessUnconfirmed { .. }));
        self.reads.push(SavedInventory {
            selection,
            read,
            expires: provisional.then_some(end),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection() -> EngineInventorySelection {
        EngineInventorySelection {
            calls: true,
            references: false,
            types: rift_search::sparql::TypeSelection::default(),
            enclosing: false,
            occurrences: None,
            batch_inputs: 4,
            records_max: GraphLimits::default().triples_max(),
        }
    }

    #[test]
    fn unconfirmed_inventory_is_reused_until_its_request_deadline() {
        let now = Instant::now();
        let end = now + std::time::Duration::from_secs(1);
        let read = Arc::new(InventoryRead {
            warnings: vec![ReadWarning::EngineReadinessUnconfirmed {
                processes: vec!["python".to_owned()],
                detail: "the engine stayed quiet without progress evidence".to_owned(),
            }],
            ..InventoryRead::default()
        });
        let mut entries = InventoryEntries::default();
        entries.remember(selection(), Arc::clone(&read), end);
        assert!(Arc::ptr_eq(
            &entries.get(selection(), now).expect("retained"),
            &read
        ));
        assert!(entries.get(selection(), end).is_none());
    }

    #[test]
    fn unavailable_inventory_is_retried_on_the_same_publication() {
        let now = Instant::now();
        let end = now + std::time::Duration::from_secs(1);
        let unavailable = Arc::new(InventoryRead {
            warnings: vec![ReadWarning::EngineAnalysisUnavailable {
                language: None,
                detail: "the engine was still analyzing".to_owned(),
            }],
            partial: true,
            ..InventoryRead::default()
        });
        let mut entries = InventoryEntries::default();
        entries.remember(selection(), unavailable, end);
        assert!(entries.get(selection(), now).is_none());

        let ready = Arc::new(InventoryRead::default());
        entries.remember(selection(), Arc::clone(&ready), end);
        assert!(Arc::ptr_eq(
            &entries
                .get(selection(), end)
                .expect("retained ready answer"),
            &ready
        ));
    }

    #[test]
    fn complete_inventory_survives_the_request_deadline() {
        let now = Instant::now();
        let read = Arc::new(InventoryRead::default());
        let mut entries = InventoryEntries::default();
        entries.remember(selection(), Arc::clone(&read), now);
        assert!(Arc::ptr_eq(
            &entries.get(selection(), now).expect("retained"),
            &read
        ));
    }

    #[test]
    fn retained_records_evict_the_earlier_selection() {
        let now = Instant::now();
        let mut entries = InventoryEntries::default();
        entries.remember(
            selection(),
            Arc::new(InventoryRead {
                records: GraphLimits::default().triples_max() - 1,
                ..InventoryRead::default()
            }),
            now,
        );
        let mut references = selection();
        references.calls = false;
        references.references = true;
        entries.remember(references, Arc::new(InventoryRead::default()), now);
        assert!(entries.get(selection(), now).is_none());
        assert!(entries.get(references, now).is_some());
    }
}
