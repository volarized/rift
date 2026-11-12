use std::path::Path;
use std::time::Duration;

use crate::{DEPENDENCIES_MAX, SOURCE_COUNT_MAX, SourceDependencies};
use rift_core::{FileDigest, SourceUnitId};
use rusqlite::{Connection, params};

use crate::{
    Batch, CONTRACT_VERSION, Category, CollectionInput, CollectionSource, Collector, Coverage,
    Error, Inventory, MESSAGE_BYTES_MAX, Producer,
};

// A source whose dependencies have changed many times may fall out of the reuse window.
const CHECKPOINT_CANDIDATES_MAX: i64 = 16;
const CHECKPOINT_BYTES_MAX: i64 = 8 << 20;

/// Counts of batches returned by a collection run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CollectionStats {
    /// Batches read from durable checkpoints.
    pub reused: usize,
    /// Batches run and checkpointed.
    pub collected: usize,
    /// Batches returned with work still owed.
    pub partial: usize,
}

/// Durable checkpoints for one local collection. Each source and category commits separately.
pub struct CollectionCache {
    connection: Connection,
}

impl CollectionCache {
    /// Opens or creates a checkpoint database.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot open or initialize the database.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Use durable commits. The default page size is 4 KiB, so this caps new stores at 1 GiB.
        connection.execute_batch(
            "PRAGMA page_size = 4096;
             PRAGMA max_page_count = 262144;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS collection_batch_v1 (
                 sequence INTEGER PRIMARY KEY,
                 producer TEXT NOT NULL,
                 category TEXT NOT NULL,
                 unit TEXT NOT NULL,
                 contract INTEGER NOT NULL,
                 revision BLOB NOT NULL,
                 context BLOB NOT NULL,
                 source BLOB NOT NULL,
                 witness BLOB NOT NULL,
                 dependencies BLOB NOT NULL,
                 checksum BLOB NOT NULL,
                 record BLOB NOT NULL,
                 UNIQUE (producer, category, unit, contract, revision, context, source, witness)
             );
             CREATE INDEX IF NOT EXISTS collection_batch_lookup_v1
             ON collection_batch_v1 (producer, category, unit, contract, revision, context, source, sequence DESC);",
        )?;
        Ok(Self { connection })
    }

    /// Collects selected units in order and streams each validated batch to the caller.
    ///
    /// Selection is explicit. An omitted or removed unit never appears in the output. A failed
    /// run retains earlier checkpoints, while a partial batch is due again on the next run.
    ///
    /// # Errors
    ///
    /// Returns the first source admission, collector, checkpoint or visitor error.
    pub async fn collect<C: Collector>(
        &self,
        producer: &Producer,
        collector: &mut C,
        inventory: &Inventory,
        selection: &[SourceUnitId],
        category: Category,
        mut visit: impl FnMut(&CollectionSource, &Batch) -> Result<(), Error>,
    ) -> Result<CollectionStats, Error>
    where
        Error: From<C::Error>,
    {
        if selection.len() > SOURCE_COUNT_MAX {
            return Err(Error::Input("selection exceeds its source bound".into()));
        }
        let mut selected = std::collections::BTreeSet::new();
        for unit in selection {
            if !selected.insert(unit) || inventory.get(unit).is_none() {
                return Err(Error::Input(format!(
                    "missing or repeated selected source {unit}"
                )));
            }
        }

        let mut stats = CollectionStats::default();
        for unit in selection {
            let mut input = CollectionInput::new(inventory, unit)?;
            let source = input.selected();
            let batch = if let Some(batch) = self.load(producer, category, source, inventory)? {
                stats.reused += 1;
                batch
            } else {
                let batch = collector.collect(&mut input, category).await?;
                input.validate(&batch)?;
                if batch.coverage == Coverage::Partial {
                    stats.partial += 1;
                    batch
                } else {
                    self.store(producer, category, source, input.dependencies(), &batch)?;
                    stats.collected += 1;
                    batch
                }
            };
            visit(source, &batch)?;
        }
        Ok(stats)
    }

    fn load(
        &self,
        producer: &Producer,
        category: Category,
        selected: &CollectionSource,
        inventory: &Inventory,
    ) -> Result<Option<Batch>, Error> {
        let mut query = self.connection.prepare_cached(
            "SELECT sequence, witness, dependencies FROM collection_batch_v1
             WHERE producer = ?1 AND category = ?2 AND unit = ?3 AND contract = ?4
               AND revision = ?5 AND context = ?6 AND source = ?7
             ORDER BY sequence DESC LIMIT ?8",
        )?;
        let mut rows = query.query(params![
            producer.id().as_str(),
            category.as_str(),
            selected.unit().to_string(),
            CONTRACT_VERSION,
            producer.revision().as_bytes(),
            producer.context().as_bytes(),
            selected.revision().as_bytes(),
            CHECKPOINT_CANDIDATES_MAX,
        ])?;
        let mut payload = self.connection.prepare_cached(
            "SELECT checksum, record FROM collection_batch_v1
             WHERE sequence = ?1 AND length(record) <= ?2",
        )?;
        while let Some(row) = rows.next()? {
            let witness: &[u8] = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
            let bytes: &[u8] = row.get_ref(2)?.as_blob().map_err(rusqlite::Error::from)?;
            if bytes.len() > MESSAGE_BYTES_MAX || witness != FileDigest::of(bytes).as_bytes() {
                continue;
            }
            let Ok(dependencies) = serde_json::from_slice::<SourceDependencies>(bytes) else {
                continue;
            };
            if dependencies.len() > DEPENDENCIES_MAX
                || !dependencies.iter().all(|(address, expected)| {
                    SourceUnitId::parse(address).is_ok_and(|unit| {
                        let actual = inventory
                            .get(&unit)
                            .map(|source| *source.revision().as_bytes());
                        actual == *expected
                    })
                })
            {
                continue;
            }
            let mut results =
                payload.query(params![row.get::<_, i64>(0)?, CHECKPOINT_BYTES_MAX])?;
            if let Some(result) = results.next()? {
                let checksum = result
                    .get_ref(0)?
                    .as_blob()
                    .map_err(rusqlite::Error::from)?;
                let record = result
                    .get_ref(1)?
                    .as_blob()
                    .map_err(rusqlite::Error::from)?;
                if checksum == FileDigest::of(record).as_bytes()
                    && let Ok(batch) = serde_json::from_slice::<Batch>(record)
                    && batch.coverage != Coverage::Partial
                {
                    return Ok(Some(batch));
                }
            }
        }
        Ok(None)
    }

    fn store(
        &self,
        producer: &Producer,
        category: Category,
        source: &CollectionSource,
        dependencies: &SourceDependencies,
        batch: &Batch,
    ) -> Result<(), Error> {
        let bytes = serde_json::to_vec(batch)?;
        let dependencies = serde_json::to_vec(dependencies)?;
        if bytes.len() > MESSAGE_BYTES_MAX || dependencies.len() > MESSAGE_BYTES_MAX {
            return Err(Error::Protocol("checkpoint exceeds its byte bound".into()));
        }
        let checksum = FileDigest::of(&bytes);
        let witness = FileDigest::of(&dependencies);
        let mut statement = self.connection.prepare_cached(
            "INSERT INTO collection_batch_v1
             (producer, category, unit, contract, revision, context, source, witness, dependencies, checksum, record)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (producer, category, unit, contract, revision, context, source, witness)
             DO UPDATE SET sequence = excluded.sequence, dependencies = excluded.dependencies,
                           checksum = excluded.checksum, record = excluded.record",
        )?;
        statement.execute(params![
            producer.id().as_str(),
            category.as_str(),
            source.unit().to_string(),
            CONTRACT_VERSION,
            producer.revision().as_bytes(),
            producer.context().as_bytes(),
            source.revision().as_bytes(),
            witness.as_bytes(),
            dependencies,
            checksum.as_bytes(),
            bytes,
        ])?;
        Ok(())
    }
}
