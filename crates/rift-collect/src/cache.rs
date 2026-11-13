use std::path::Path;

use crate::{DEPENDENCIES_MAX, SOURCE_COUNT_MAX, SourceDependencies};
use rift_core::{FileDigest, SourceUnitId};
use toasty::{
    Db,
    db::Connection,
    migration::{MigrationFile, MigrationSet},
};
use toasty_driver_sqlite::Sqlite;

use crate::{
    Batch, CONTRACT_VERSION, Category, CollectionInput, CollectionSource, Collector, Coverage,
    Error, Inventory, MESSAGE_BYTES_MAX, Producer,
};

// A source whose dependencies have changed many times may fall out of the reuse window.
const CHECKPOINT_CANDIDATES_MAX: usize = 16;

// Adopt existing v1 caches in place. The model owns queries and writes.
const MIGRATIONS: MigrationSet = MigrationSet::new(&[MigrationFile::new(
    1,
    "collection_checkpoints",
    "CREATE TABLE IF NOT EXISTS collection_batch_v1 (
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
     )
-- #[toasty::breakpoint]
CREATE INDEX IF NOT EXISTS collection_batch_lookup_v1
ON collection_batch_v1 (producer, category, unit, contract, revision, context, source, sequence DESC)",
)]);

mod model;

use model::Checkpoint;

struct CheckpointKey<'a> {
    producer: &'a Producer,
    category: Category,
    source: &'a CollectionSource,
}

impl CheckpointKey<'_> {
    fn query(&self) -> toasty::stmt::Query<toasty::stmt::List<Checkpoint>> {
        let unit = self.source.unit().to_string();
        let revision = self.producer.revision();
        let context = self.producer.context();
        let source = self.source.revision();
        let fields = Checkpoint::fields();
        toasty::stmt::Query::all()
            .filter(fields.producer().eq(self.producer.id().as_str()))
            .filter(fields.category().eq(self.category.as_str()))
            .filter(fields.unit().eq(unit))
            .filter(fields.contract().eq(i64::from(CONTRACT_VERSION)))
            .filter(fields.revision().eq(revision.as_bytes().to_vec()))
            .filter(fields.context().eq(context.as_bytes().to_vec()))
            .filter(fields.source().eq(source.as_bytes().to_vec()))
    }
}

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
    database: Db,
}

impl CollectionCache {
    /// Opens or creates a checkpoint database.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot open or initialize the database.
    pub async fn open(path: &Path) -> Result<Self, Error> {
        let driver = if path == Path::new(":memory:") {
            Sqlite::in_memory()
        } else {
            Sqlite::open(path)
        };
        let database = Db::builder()
            .models(toasty::models!(Checkpoint))
            .max_pool_size(1)
            .build(driver)
            .await?;
        let cache = Self { database };
        {
            let mut connection = cache.database.connection().await?;
            configure(&mut connection).await?;
        }
        MIGRATIONS.apply(&cache.database).await?;
        Ok(cache)
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
            let key = CheckpointKey {
                producer,
                category,
                source,
            };
            let batch = if let Some(batch) = self.load(&key, inventory).await? {
                stats.reused += 1;
                batch
            } else {
                let batch = collector.collect(&mut input, category).await?;
                input.validate(&batch)?;
                if batch.coverage == Coverage::Partial {
                    stats.partial += 1;
                    batch
                } else {
                    self.store(&key, input.dependencies(), &batch).await?;
                    stats.collected += 1;
                    batch
                }
            };
            visit(source, &batch)?;
        }
        Ok(stats)
    }

    async fn load(
        &self,
        key: &CheckpointKey<'_>,
        inventory: &Inventory,
    ) -> Result<Option<Batch>, Error> {
        let mut connection = self.database.connection().await?;
        let fields = Checkpoint::fields();
        let mut transaction = connection.transaction().await?;
        let candidates = key
            .query()
            .order_by(fields.sequence().desc())
            .limit(CHECKPOINT_CANDIDATES_MAX)
            .select((fields.sequence(), fields.witness(), fields.dependencies()))
            .exec(&mut transaction)
            .await?;
        for (sequence, witness, bytes) in candidates {
            if bytes.len() > MESSAGE_BYTES_MAX || witness != FileDigest::of(&bytes).as_bytes() {
                continue;
            }
            let Ok(dependencies) = serde_json::from_slice::<SourceDependencies>(&bytes) else {
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
            if let Some((checksum, record)) = Checkpoint::filter_by_sequence(sequence)
                .select((fields.checksum(), fields.record()))
                .first()
                .exec(&mut transaction)
                .await?
                && record.len() <= MESSAGE_BYTES_MAX
                && checksum == FileDigest::of(&record).as_bytes()
                && let Ok(batch) = serde_json::from_slice::<Batch>(&record)
                && batch.coverage != Coverage::Partial
            {
                transaction.commit().await?;
                return Ok(Some(batch));
            }
        }
        transaction.commit().await?;
        Ok(None)
    }

    async fn store(
        &self,
        key: &CheckpointKey<'_>,
        dependencies: &SourceDependencies,
        batch: &Batch,
    ) -> Result<(), Error> {
        let bytes = serde_json::to_vec(batch)?;
        let dependencies = serde_json::to_vec(dependencies)?;
        if bytes.len() > MESSAGE_BYTES_MAX || dependencies.len() > MESSAGE_BYTES_MAX {
            return Err(Error::Protocol("checkpoint exceeds its byte bound".into()));
        }
        let checksum = FileDigest::of(&bytes);
        let witness = FileDigest::of(&dependencies).as_bytes().to_vec();
        let unit = key.source.unit().to_string();
        let revision = key.producer.revision().as_bytes().to_vec();
        let context = key.producer.context().as_bytes().to_vec();
        let source = key.source.revision().as_bytes().to_vec();
        let mut connection = self.database.connection().await?;
        configure(&mut connection).await?;
        let mut transaction = connection.transaction().await?;
        // Deleting and recreating a checkpoint promotes it into the most recent window.
        key.query()
            .filter(Checkpoint::fields().witness().eq(witness.clone()))
            .delete()
            .exec(&mut transaction)
            .await?;
        toasty::create!(Checkpoint {
            producer: key.producer.id().as_str(),
            category: key.category.as_str(),
            unit,
            contract: i64::from(CONTRACT_VERSION),
            revision,
            context,
            source,
            witness,
            dependencies,
            checksum: checksum.as_bytes().to_vec(),
            record: bytes,
        })
        .exec(&mut transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

async fn configure(connection: &mut Connection) -> Result<(), Error> {
    for pragma in [
        "PRAGMA busy_timeout = 5000",
        "PRAGMA page_size = 4096",
        "PRAGMA max_page_count = 262144",
        "PRAGMA synchronous = FULL",
    ] {
        toasty::sql::query(pragma).exec(&mut *connection).await?;
    }
    Ok(())
}
