//! Persistence for the semantic tier's vectors, beside the lexical index.
//!
//! A vector is addressed by the model that produced it and the digest of the
//! text it was produced from, never by a declaration's identity. Two models'
//! vectors share no space, so the model is part of the address; a declaration
//! that moves or is renamed keeps its text, so it keeps its vector and the
//! refresh embeds only what actually changed.
//!
//! The rows live in the workspace database the lexical index already owns.
//! Toasty records applied migrations in one table per file, so one migration
//! set covers both tiers and each store applies the same idempotent set.

use std::collections::BTreeSet;
use std::path::Path;

use toasty::Db;
use toasty::db::Connection;
use toasty::stmt::{Type, Value};
use toasty_driver_sqlite::Sqlite;

use crate::lexical::{
    LexicalIndexError, LexicalIndexLimits, MIGRATIONS, bound_as_usize, storage_error,
};

/// One stored vector: what produced it, what it came from, and its values.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredVector {
    digest: String,
    values: Vec<f32>,
}

impl StoredVector {
    /// Builds one stored vector from its digest and values.
    #[must_use]
    pub const fn new(digest: String, values: Vec<f32>) -> Self {
        Self { digest, values }
    }

    /// The digest of the text this vector was embedded from.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The vector's values, unit length as the encoder produced them.
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// The values alone, for a caller building a matrix from many rows.
    #[must_use]
    pub fn into_values(self) -> Vec<f32> {
        self.values
    }
}

/// The workspace's stored vectors, one row per model and digest.
#[derive(Debug)]
pub struct SemanticVectorStore {
    database: Db,
    limits: LexicalIndexLimits,
}

impl SemanticVectorStore {
    /// Opens (creating if absent) the vector store at `database_path`.
    ///
    /// The path is the workspace database the lexical index also opens. Both
    /// apply the same migration set, which is idempotent, so the open order of
    /// the two tiers does not matter.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the database cannot be opened or its
    /// schema migration fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the database file created without its schema
    /// applied. Reopening retries safely: schema migrations are idempotent.
    pub async fn open(
        database_path: &Path,
        limits: LexicalIndexLimits,
    ) -> Result<Self, LexicalIndexError> {
        let mut builder = Db::builder();
        builder
            .models(toasty::models!(SemanticVectorRecord))
            .max_pool_size(bound_as_usize(limits.pool_slots()));
        let database = builder
            .build(Sqlite::open(database_path))
            .await
            .map_err(storage_error)?;
        let _migration_report = MIGRATIONS.apply(&database).await.map_err(storage_error)?;
        Ok(Self { database, limits })
    }

    /// Every vector one model has produced, in digest order.
    ///
    /// A row whose stored width differs from `dimension` is skipped: the model
    /// identifier alone cannot rule out a checkpoint that changed shape, and a
    /// vector of another width is noise rather than an answer.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the query fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only query.
    pub async fn vectors(
        &self,
        model: &str,
        dimension: usize,
    ) -> Result<Vec<StoredVector>, LexicalIndexError> {
        let mut connection = self.connection().await?;
        let rows = toasty::sql::query(
            "SELECT digest, vector FROM semantic_vectors
             WHERE model = ?1 AND dimension = ?2 ORDER BY digest",
        )
        .bind(model.to_owned())
        .bind(width_as_i64(dimension))
        .column_types([Type::String, Type::Bytes])
        .exec(&mut connection)
        .await
        .map_err(storage_error)?;
        Ok(rows
            .iter()
            .filter_map(|row| decode_row(row, dimension))
            .collect())
    }

    /// The digests one model already holds a vector for.
    ///
    /// A refresh reads this to decide what to embed, without paying to decode
    /// every vector it is going to keep.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the query fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only query.
    pub async fn digests(&self, model: &str) -> Result<BTreeSet<String>, LexicalIndexError> {
        let mut connection = self.connection().await?;
        let rows = toasty::sql::query("SELECT digest FROM semantic_vectors WHERE model = ?1")
            .bind(model.to_owned())
            .column_types([Type::String])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(rows.iter().filter_map(digest_of).collect())
    }

    /// Stores one embedding pass's vectors, replacing any row at the same
    /// address.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when a write fails.
    ///
    /// # Cancel safety
    ///
    /// The pass runs in one transaction. Dropping this future before it commits
    /// leaves the previously stored vectors intact, never partially replaced,
    /// and the next pass recomputes exactly what is still missing.
    pub async fn store(
        &self,
        model: &str,
        dimension: usize,
        vectors: &[StoredVector],
    ) -> Result<(), LexicalIndexError> {
        if vectors.is_empty() {
            return Ok(());
        }
        let width = width_as_i64(dimension);
        let mut connection = self.connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;
        for vector in vectors {
            toasty::sql::statement(
                "INSERT INTO semantic_vectors(identity, model, digest, dimension, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(identity) DO UPDATE SET
                     dimension = excluded.dimension, vector = excluded.vector",
            )
            .bind(address(model, vector.digest()))
            .bind(model.to_owned())
            .bind(vector.digest().to_owned())
            .bind(width)
            .bind(encode(vector.values()))
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    /// Drops every vector this model did not produce, and reports how many.
    ///
    /// Changing the configured model addresses a different space, and the rows
    /// the previous model wrote can never be read again.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the delete fails.
    ///
    /// # Cancel safety
    ///
    /// One statement, so the delete either lands whole or not at all.
    pub async fn prune_other_models(&self, model: &str) -> Result<u64, LexicalIndexError> {
        let mut connection = self.connection().await?;
        let dropped = self.count_other_models(&mut connection, model).await?;
        toasty::sql::statement("DELETE FROM semantic_vectors WHERE model <> ?1")
            .bind(model.to_owned())
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(dropped)
    }

    /// Drops this model's vectors whose digest no live declaration addresses,
    /// and reports how many.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the query or a delete fails.
    ///
    /// # Cancel safety
    ///
    /// The sweep runs in one transaction, so a dropped future leaves every row
    /// in place.
    pub async fn prune_absent(
        &self,
        model: &str,
        live: &BTreeSet<String>,
    ) -> Result<u64, LexicalIndexError> {
        let stale: Vec<String> = self
            .digests(model)
            .await?
            .into_iter()
            .filter(|digest| !live.contains(digest))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut connection = self.connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;
        for digest in &stale {
            toasty::sql::statement("DELETE FROM semantic_vectors WHERE identity = ?1")
                .bind(address(model, digest))
                .exec(&mut transaction)
                .await
                .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(stale.len() as u64)
    }

    /// Rows some other model owns, which is what a model change drops.
    async fn count_other_models(
        &self,
        connection: &mut Connection,
        model: &str,
    ) -> Result<u64, LexicalIndexError> {
        let rows = toasty::sql::query("SELECT COUNT(*) FROM semantic_vectors WHERE model <> ?1")
            .bind(model.to_owned())
            .column_types([Type::I64])
            .exec(connection)
            .await
            .map_err(storage_error)?;
        let Some(Value::Record(record)) = rows.first() else {
            return Ok(0);
        };
        match record.as_slice() {
            [Value::I64(count)] => Ok(u64::try_from(*count).unwrap_or_default()),
            _ => Ok(0),
        }
    }

    /// A pooled connection with this store's busy timeout applied.
    async fn connection(&self) -> Result<Connection, LexicalIndexError> {
        let mut connection = self.database.connection().await.map_err(storage_error)?;
        let busy_timeout_ms = i64::from(self.limits.busy_timeout_ms());
        toasty::sql::query(format!("PRAGMA busy_timeout = {busy_timeout_ms}"))
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(connection)
    }
}

/// One stored row back, skipping a blob that is not a vector of this width.
fn decode_row(row: &Value, dimension: usize) -> Option<StoredVector> {
    let Value::Record(record) = row else {
        return None;
    };
    let [Value::String(digest), Value::Bytes(blob)] = record.as_slice() else {
        return None;
    };
    decode(blob, dimension).map(|values| StoredVector::new(digest.clone(), values))
}

/// One digest column back.
fn digest_of(row: &Value) -> Option<String> {
    let Value::Record(record) = row else {
        return None;
    };
    match record.as_slice() {
        [Value::String(digest)] => Some(digest.clone()),
        _ => None,
    }
}

/// The row address one model and digest share.
fn address(model: &str, digest: &str) -> String {
    format!("{model}/{digest}")
}

/// The stored width, as the wire-width integer `semantic_vectors.dimension`
/// holds. An encoder dimension is a model's hidden size, far below the bound.
fn width_as_i64(dimension: usize) -> i64 {
    i64::try_from(dimension).unwrap_or(i64::MAX)
}

/// Vector values as stored: little-endian single precision, no header, so a row
/// is a slice.
fn encode(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// One stored vector back, or `None` when the blob is not a vector of the
/// expected width. A model whose shape changed invalidates rather than
/// corrupts.
fn decode(blob: &[u8], dimension: usize) -> Option<Vec<f32>> {
    if dimension == 0 || blob.len() != dimension * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|four| f32::from_le_bytes([four[0], four[1], four[2], four[3]]))
            .collect(),
    )
}

#[derive(Debug, toasty::Model)]
#[table = "semantic_vectors"]
struct SemanticVectorRecord {
    #[key]
    identity: String,
    #[index]
    model: String,
    digest: String,
    dimension: i64,
    vector: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::{SemanticVectorStore, StoredVector, address, decode, encode};
    use crate::lexical::LexicalIndexLimits;
    use std::collections::BTreeSet;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const MODEL: &str = "BAAI/bge-small-en-v1.5";
    const OTHER_MODEL: &str = "nomic-ai/CodeRankEmbed";
    const WIDTH: usize = 4;

    fn limits() -> LexicalIndexLimits {
        LexicalIndexLimits::new(64, 1 << 20, 32, 64, 4, 1_000)
    }

    /// Whether a value round-tripped through the store unchanged. Storage is
    /// byte-exact, so the two differ only by the error the literal carries.
    fn is_value(stored: f32, expected: f32) -> bool {
        (stored - expected).abs() < f32::EPSILON
    }

    fn vector(digest: &str, seed: f32) -> StoredVector {
        StoredVector::new(
            digest.to_owned(),
            (0..WIDTH)
                .map(|index| seed + f32::from(u8::try_from(index).unwrap_or_default()))
                .collect(),
        )
    }

    async fn opened(
        directory: &std::path::Path,
    ) -> Result<SemanticVectorStore, Box<dyn std::error::Error>> {
        Ok(SemanticVectorStore::open(&directory.join("db"), limits()).await?)
    }

    #[tokio::test]
    async fn test_stored_vectors_come_back_in_digest_order() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store
            .store(MODEL, WIDTH, &[vector("bbb", 1.0), vector("aaa", 2.0)])
            .await?;
        let read = store.vectors(MODEL, WIDTH).await?;
        assert_eq!(
            read.iter().map(StoredVector::digest).collect::<Vec<_>>(),
            ["aaa", "bbb"]
        );
        assert_eq!(read[0].values(), [2.0, 3.0, 4.0, 5.0]);
        assert_eq!(read[1].values(), [1.0, 2.0, 3.0, 4.0]);
        Ok(())
    }

    #[tokio::test]
    async fn test_storing_one_digest_twice_replaces_rather_than_duplicates() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store.store(MODEL, WIDTH, &[vector("aaa", 1.0)]).await?;
        store.store(MODEL, WIDTH, &[vector("aaa", 9.0)]).await?;
        let read = store.vectors(MODEL, WIDTH).await?;
        assert_eq!(read.len(), 1, "one address holds one vector");
        assert_eq!(read[0].values(), [9.0, 10.0, 11.0, 12.0]);
        Ok(())
    }

    #[tokio::test]
    async fn test_digests_answer_without_decoding_the_vectors() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store
            .store(MODEL, WIDTH, &[vector("aaa", 1.0), vector("bbb", 2.0)])
            .await?;
        let held = store.digests(MODEL).await?;
        assert_eq!(held, BTreeSet::from(["aaa".to_owned(), "bbb".to_owned()]));
        assert!(store.digests(OTHER_MODEL).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_a_vector_of_another_width_is_not_read_back() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store.store(MODEL, WIDTH, &[vector("aaa", 1.0)]).await?;
        assert!(
            store.vectors(MODEL, WIDTH + 1).await?.is_empty(),
            "a checkpoint that changed shape invalidates rather than corrupts"
        );
        assert_eq!(store.vectors(MODEL, WIDTH).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_two_models_keep_separate_spaces() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store.store(MODEL, WIDTH, &[vector("aaa", 1.0)]).await?;
        store
            .store(OTHER_MODEL, WIDTH, &[vector("aaa", 5.0)])
            .await?;
        assert!(is_value(
            store.vectors(MODEL, WIDTH).await?[0].values()[0],
            1.0
        ));
        assert!(is_value(
            store.vectors(OTHER_MODEL, WIDTH).await?[0].values()[0],
            5.0
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_a_model_change_drops_every_other_models_vectors() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store
            .store(
                OTHER_MODEL,
                WIDTH,
                &[vector("aaa", 1.0), vector("bbb", 2.0)],
            )
            .await?;
        store.store(MODEL, WIDTH, &[vector("aaa", 3.0)]).await?;
        assert_eq!(store.prune_other_models(MODEL).await?, 2);
        assert!(store.digests(OTHER_MODEL).await?.is_empty());
        assert_eq!(store.digests(MODEL).await?.len(), 1);
        assert_eq!(
            store.prune_other_models(MODEL).await?,
            0,
            "a second sweep has nothing left to drop"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_a_digest_no_declaration_addresses_is_swept() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store
            .store(MODEL, WIDTH, &[vector("aaa", 1.0), vector("bbb", 2.0)])
            .await?;
        let live = BTreeSet::from(["aaa".to_owned()]);
        assert_eq!(store.prune_absent(MODEL, &live).await?, 1);
        assert_eq!(store.digests(MODEL).await?, live);
        assert_eq!(
            store.prune_absent(MODEL, &live).await?,
            0,
            "a sweep with nothing stale writes nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_storing_nothing_is_accepted_and_writes_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = opened(directory.path()).await?;
        store.store(MODEL, WIDTH, &[]).await?;
        assert!(store.digests(MODEL).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_a_reopened_store_reads_what_the_first_one_wrote() -> TestResult {
        let directory = tempfile::tempdir()?;
        opened(directory.path())
            .await?
            .store(MODEL, WIDTH, &[vector("aaa", 1.0)])
            .await?;
        let reopened = opened(directory.path()).await?;
        assert_eq!(reopened.vectors(MODEL, WIDTH).await?.len(), 1);
        Ok(())
    }

    #[test]
    fn test_blob_round_trips_and_refuses_another_width() {
        let values: Vec<f32> = (0..WIDTH)
            .map(|index| f32::from(u8::try_from(index).unwrap_or_default()) / 8.0)
            .collect();
        assert_eq!(
            decode(&encode(&values), WIDTH).as_deref(),
            Some(values.as_slice())
        );
        assert_eq!(decode(&encode(&values), WIDTH + 1), None);
        assert_eq!(decode(&[], 0), None, "a zero width reads nothing");
        assert_eq!(decode(&[0, 1, 2], WIDTH), None);
    }

    #[test]
    fn test_the_address_is_the_model_and_the_digest() {
        assert_eq!(address("owner/model", "abc"), "owner/model/abc");
        assert_ne!(address("one", "a"), address("two", "a"));
    }

    #[test]
    fn test_a_stored_vector_reports_and_surrenders_its_values() {
        let stored = vector("aaa", 1.0);
        assert_eq!(stored.digest(), "aaa");
        assert_eq!(stored.values().len(), WIDTH);
        assert_eq!(stored.into_values().len(), WIDTH);
    }
}
