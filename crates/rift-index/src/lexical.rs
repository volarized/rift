//! Full-text search over index documents: code symbols and visible text files.
//!
//! Document granularity is the symbol or the text file, chosen so the vector
//! ranking can attach one vector per document without reshaping this store.
//!
//! The searchable fields, their `bm25` weights, and the tokenizer all come
//! from [`rift_ranking`], so the column list this module declares and the
//! ranking every other reader runs cannot drift. The corpus revision the
//! state row carries covers exactly that shape: a store stamped with another
//! revision answers nothing until the workspace republishes.
//!
//! `SQLite` FTS5 is reached through raw SQL because Toasty 0.10 has no typed
//! virtual-table or `MATCH` API, the same boundary the Toasty compatibility
//! test documents. Raw SQL stays isolated to the FTS virtual table and its
//! rows; the authoritative `lexical_documents`, `lexical_files`, and
//! `lexical_index_state` tables are ordinary Toasty models.
//!
//! The FTS table is an external-content index over `lexical_documents`: it
//! holds the token index alone and reads column values from the typed row its
//! rowid names. FTS5 leaves the two in step to the writer: "It is still the
//! responsibility of the user to ensure that the contents of an external
//! content FTS5 table are kept up to date with the content table"
//! (<https://www.sqlite.org/fts5.html>). Every write here therefore removes a
//! row's index entries with the `'delete'` command while the typed row still
//! holds the values they were indexed from, and indexes a typed row only after
//! it is written.
//!
//! A second external-content index, `lexical_documents_trigram`, serves regex `pattern`
//! search. [`crate::trigram_store`] owns its statements: a write files its new file rows
//! for it, and [`LexicalSearchIndex::index_trigrams`] indexes them later, in bounded
//! transactions of their own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

use rift_core::ProjectPath;
use rift_error::{ErrorContext, RiftError, errors};
use rift_protocol::configuration::{
    LEXICAL_TRANSACTION_BYTES_DEFAULT, LEXICAL_TRANSACTION_UNITS_DEFAULT, LEXICAL_UNITS_MAX_DEFAULT,
};
use rift_ranking::{
    BodyTerms, CorpusRevision, DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation,
    FieldSet, FileRowFrequencies, IndexCapabilities, IndexDocument, IndexReader, ParsedQuery,
    Prefilter, PublicationFormat, QueryPhase, RankRequest, RankedIdentity, RankingInput,
    RankingInputKind, RankingInputSet, ReaderFuture, SearchableField,
};
use std::sync::Arc;

use toasty::Executor;
use toasty::db::Connection;
use toasty::migration::{MigrationFile, MigrationSet};
use toasty::stmt::{IntoInsert, Type, Value};

use crate::change_set::{FileDigest, WorkspaceDigests};
use crate::database::{DatabaseName, WorkspaceDatabase};
use crate::trigram_store::{PatternCandidates, TrigramBatch};

/// Returns whether one storage error failed to obtain a pooled connection: a checkout
/// refusal names its database through `index.database_failed`.
///
/// Other storage errors remain refusals. Callers may skip this read only when the
/// connection pool itself could not serve it.
#[must_use]
pub fn is_connection_unavailable(error: &RiftError) -> bool {
    if error.slug() != errors::index::database_failed::SLUG {
        return false;
    }
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<toasty::Error>())
        .is_some_and(toasty::Error::is_connection_pool)
}

/// Default maximum content bytes accepted for one lexical document: the largest chunk
/// `[search.text] max_chunk` accepts (16 MiB), so the store takes every row the
/// configuration can derive, whatever `max_chunk` a later reload sets.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the shared default is 16 MiB and fits u32"
)]
const LEXICAL_UNIT_BYTES_MAX_DEFAULT: u32 =
    rift_protocol::configuration::LEXICAL_CONTENT_BYTES_DEFAULT as u32;
/// Default maximum search results returned per query.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the shared default is 1000 and fits u32"
)]
const LEXICAL_MATCHES_MAX_DEFAULT: u32 =
    rift_protocol::configuration::LEXICAL_MATCHES_DEFAULT as u32;
/// Default pooled `SQLite` connection slots.
const LEXICAL_POOL_SLOTS_DEFAULT: u32 = 4;
/// Default busy-wait budget, in milliseconds, `SQLite` grants a connection
/// before returning `SQLITE_BUSY`.
const LEXICAL_BUSY_TIMEOUT_MS_DEFAULT: u32 = 5_000;
/// The byte width of one recorded file digest, a SHA-256.
const RECORDED_DIGEST_BYTES: usize = 32;
/// Typed rows inserted per statement. Each row binds at most 12 values, so one statement
/// uses at most 768 variables, below bundled SQLite's 32,766-variable limit.
const LEXICAL_INSERT_ROWS_MAX: usize = 64;

/// Primary key of the single `lexical_index_state` row this adapter maintains.
const LEXICAL_INDEX_STATE_ID: i64 = 1;

/// The schema of the index database, from its first migration.
///
/// The index database is a file of its own: the vector rows live in the vectors database
/// and the log records in the metrics database, each with a migration record of its own.
const INDEX_MIGRATION_FILES: &[MigrationFile] = &[MigrationFile::new(
    1,
    "index_schema",
    "CREATE TABLE lexical_documents(
        id INTEGER PRIMARY KEY,
        identity TEXT NOT NULL UNIQUE,
        path TEXT NOT NULL,
        kind TEXT NOT NULL,
        digest TEXT NOT NULL,
        byte_length BIGINT NOT NULL,
        byte_offset BIGINT,
        name TEXT,
        qualified_name TEXT,
        identifier_terms TEXT,
        signature TEXT,
        documentation TEXT,
        file_content TEXT
    )
-- #[toasty::breakpoint]
CREATE INDEX lexical_documents_path ON lexical_documents(path)
-- #[toasty::breakpoint]
CREATE INDEX lexical_documents_file_rows ON lexical_documents(id) WHERE file_content IS NOT NULL
-- #[toasty::breakpoint]
CREATE VIRTUAL TABLE lexical_documents_fts USING fts5(name, qualified_name, identifier_terms, \
signature, documentation, file_content, content='lexical_documents', content_rowid='id', \
tokenize='unicode61 remove_diacritics 0')
-- #[toasty::breakpoint]
CREATE VIRTUAL TABLE lexical_documents_vocabulary USING fts5vocab('lexical_documents_fts', 'col')
-- #[toasty::breakpoint]
CREATE VIRTUAL TABLE lexical_documents_trigram USING fts5(file_content, \
content='lexical_documents', content_rowid='id', tokenize='trigram', detail='none', \
columnsize=0)
-- #[toasty::breakpoint]
CREATE TABLE lexical_trigram_pending(id INTEGER PRIMARY KEY)
-- #[toasty::breakpoint]
CREATE TABLE lexical_files(path TEXT PRIMARY KEY NOT NULL, digest BLOB NOT NULL)
-- #[toasty::breakpoint]
CREATE TABLE lexical_index_state(
        id BIGINT PRIMARY KEY NOT NULL,
        tree_revision TEXT,
        corpus_revision TEXT NOT NULL,
        derivation_revision TEXT NOT NULL
    )
-- #[toasty::breakpoint]
CREATE TABLE documentation_manifest(id BIGINT PRIMARY KEY NOT NULL, payload TEXT NOT NULL)
-- #[toasty::breakpoint]
CREATE TABLE documentation_sources(identity TEXT PRIMARY KEY NOT NULL, digest BLOB NOT NULL, payload TEXT NOT NULL)
-- #[toasty::breakpoint]
CREATE TABLE documentation_references(identity TEXT PRIMARY KEY NOT NULL, source TEXT NOT NULL, target TEXT NOT NULL, block TEXT NOT NULL)
-- #[toasty::breakpoint]
CREATE INDEX documentation_references_target ON documentation_references(target)
-- #[toasty::breakpoint]
CREATE INDEX documentation_references_source ON documentation_references(source)",
)];
/// The index database's migration set; Toasty records it in the file's own
/// `__toasty_migrations` table.
pub(crate) const INDEX_MIGRATIONS: MigrationSet = MigrationSet::new(INDEX_MIGRATION_FILES);

/// What one revision-qualified read of the store found.
///
/// A caller reads the store to answer for one published tree, so the stored stamp is part
/// of the read rather than a separate lookup before it: the two run in one transaction,
/// over one database snapshot, and a commit landing between them cannot go unseen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionScoped<T> {
    /// The store held the expected tree revision, and this is what it answered.
    Matched(T),
    /// The store held another tree revision, named here. The caller's published tree was
    /// superseded, and the answer it wanted is under the publication it has yet to see.
    OtherRevision(String),
    /// The store carries no tree revision at all, so no publication has ever landed in it.
    NoRevision,
}

/// What one lexical search ranked, and whether the store held a match past its bound.
///
/// A ranking stops at `limit` capped by `matches_max`. The query reads one row past that
/// bound, so a store holding more than the bound reports it here and a caller never has
/// to compare the answer's length with a bound it may not know: a query with exactly as
/// many matches as the bound is not truncated.
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalRanking {
    matches: Vec<LexicalMatch>,
    truncated_at: Option<u32>,
}

impl LexicalRanking {
    /// Keeps `bound` rows of a probe that read one row past it: a row past the bound
    /// proves the store held a match the bound cut.
    fn from_probe(mut matches: Vec<LexicalMatch>, bound: u32) -> Self {
        let kept = bound_as_usize(bound);
        let truncated_at = (matches.len() > kept).then_some(bound);
        matches.truncate(kept);
        Self {
            matches,
            truncated_at,
        }
    }

    /// The ranked matches, best first.
    #[must_use]
    pub fn matches(&self) -> &[LexicalMatch] {
        &self.matches
    }

    /// The ranked matches, best first, owned.
    #[must_use]
    pub fn into_matches(self) -> Vec<LexicalMatch> {
        self.matches
    }

    /// The bound the ranking stopped at while the store held a match past it, or `None`
    /// when every match was ranked.
    #[must_use]
    pub const fn truncated_at(&self) -> Option<u32> {
        self.truncated_at
    }

    /// This ranking as one fusion input: the identities in rank order, each
    /// carrying the columns that placed it.
    #[must_use]
    pub fn into_input(self) -> RankingInput {
        RankingInput::new(
            RankingInputKind::Lexical,
            self.matches
                .into_iter()
                .map(|matched| {
                    let ranked = RankedIdentity::new(matched.identity, matched.fields);
                    match matched.file_range {
                        Some(file_range) => ranked.with_file_range(file_range),
                        None => ranked,
                    }
                })
                .collect(),
        )
    }
}

/// One lexical search hit.
///
/// `bm25` is negative; lower is better. `rank` is only comparable within the
/// results of one `search` call, and fusion reads the position it produces
/// rather than the value itself.
///
/// `fields` names every column that carried a query member. The store proves
/// it by asking `bm25` for each column alone, so a documentation-only hit and
/// a name hit are distinguishable in the answer rather than both reported as
/// "the full-text ranking placed it".
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalMatch {
    identity: DocumentIdentity,
    path: ProjectPath,
    kind: DocumentKind,
    rank: f64,
    fields: FieldSet,
    file_range: Option<Range<u64>>,
}

impl LexicalMatch {
    /// Constructs one lexical match directly. Production code only ever builds these from
    /// a live [`LexicalSearchIndex::search`]; this constructor exists for callers that
    /// merge or resolve matches from a search result they already hold - most notably
    /// tests exercising that merge without a live database.
    #[must_use]
    pub const fn new(
        identity: DocumentIdentity,
        path: ProjectPath,
        kind: DocumentKind,
        rank: f64,
        fields: FieldSet,
    ) -> Self {
        Self {
            identity,
            path,
            kind,
            rank,
            fields,
            file_range: None,
        }
    }

    /// The bytes of its file the matched row holds - the whole file, or one chunk of a
    /// large one - or `None` for a row holding no file text.
    #[must_use]
    pub const fn file_range(&self) -> Option<&Range<u64>> {
        self.file_range.as_ref()
    }

    /// Returns the matched document's stable identity.
    #[must_use]
    pub const fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }

    /// Returns the matched document's project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns the matched document's granularity.
    #[must_use]
    pub const fn kind(&self) -> DocumentKind {
        self.kind
    }

    /// Returns this hit's `bm25` rank, comparable only within one search.
    #[must_use]
    pub const fn rank(&self) -> f64 {
        self.rank
    }

    /// Returns every column that carried a query member.
    #[must_use]
    pub const fn fields(&self) -> FieldSet {
        self.fields
    }
}

/// Resource bounds for one [`LexicalSearchIndex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexicalIndexLimits {
    units_max: u32,
    unit_bytes_max: u32,
    matches_max: u32,
    pool_slots: u32,
    busy_timeout_ms: u32,
    documentation_bytes_max: usize,
    transaction_units_max: usize,
    transaction_bytes_max: usize,
}

impl LexicalIndexLimits {
    /// Constructs explicit lexical index bounds.
    #[must_use]
    pub fn new(
        units_max: u32,
        unit_bytes_max: u32,
        matches_max: u32,
        pool_slots: u32,
        busy_timeout_ms: u32,
    ) -> Self {
        Self {
            units_max,
            unit_bytes_max,
            matches_max,
            pool_slots,
            busy_timeout_ms,
            documentation_bytes_max: crate::documentation_store::METADATA_BYTES_MAX,
            transaction_units_max: bound_u64_as_usize(LEXICAL_TRANSACTION_UNITS_DEFAULT),
            transaction_bytes_max: bound_u64_as_usize(LEXICAL_TRANSACTION_BYTES_DEFAULT),
        }
    }

    /// Bounds the units and content bytes one transaction writes. A write past either
    /// commits in several transactions, one file's units always in one.
    #[must_use]
    pub const fn with_transaction_bounds(
        self,
        transaction_units_max: usize,
        transaction_bytes_max: usize,
    ) -> Self {
        Self {
            transaction_units_max,
            transaction_bytes_max,
            ..self
        }
    }

    /// Returns the most units one transaction writes.
    #[must_use]
    pub const fn transaction_units_max(self) -> usize {
        self.transaction_units_max
    }

    /// Returns the most content bytes one transaction writes.
    #[must_use]
    pub const fn transaction_bytes_max(self) -> usize {
        self.transaction_bytes_max
    }

    /// Bounds the encoded documentation metadata one commit stores. A commit whose
    /// metadata encodes past it stores its lexical documents without the metadata.
    #[must_use]
    pub const fn with_documentation_bytes_max(self, documentation_bytes_max: usize) -> Self {
        Self {
            documentation_bytes_max,
            ..self
        }
    }

    /// Returns the encoded documentation metadata bytes one commit stores, at most.
    #[must_use]
    pub const fn documentation_bytes_max(self) -> usize {
        self.documentation_bytes_max
    }

    /// Returns maximum indexed documents accepted per `replace_all`.
    #[must_use]
    pub const fn units_max(self) -> u32 {
        self.units_max
    }

    /// Returns maximum content bytes accepted for one document field.
    #[must_use]
    pub const fn unit_bytes_max(self) -> u32 {
        self.unit_bytes_max
    }

    /// Returns maximum search results returned per query.
    #[must_use]
    pub const fn matches_max(self) -> u32 {
        self.matches_max
    }

    /// Returns the pooled `SQLite` connection slot count.
    #[must_use]
    pub const fn pool_slots(self) -> u32 {
        self.pool_slots
    }

    /// Returns the busy-wait budget, in milliseconds, `SQLite` grants a
    /// connection before returning `SQLITE_BUSY`.
    #[must_use]
    pub const fn busy_timeout_ms(self) -> u32 {
        self.busy_timeout_ms
    }

    /// Narrows one accepted `[search.lexical] units_max` value to this
    /// adapter's width.
    ///
    /// Acceptance caps the key at `LEXICAL_UNITS_MAX_MAX`, which `u32` holds,
    /// so the refusal arm cannot be reached by an accepted value; it answers
    /// the width's ceiling.
    #[must_use]
    pub fn accepted_units_max(units_max: u64) -> u32 {
        u32::try_from(units_max).unwrap_or(u32::MAX)
    }
}

impl Default for LexicalIndexLimits {
    /// Uses the default collection bounds from `[search.lexical]`, with the default
    /// `[search]` connection pool and busy timeout.
    ///
    /// The query's own bounds are not here: [`ParsedQuery`] owns them, and
    /// every reader parses the caller's text through it before this store
    /// sees an expression.
    fn default() -> Self {
        Self::new(
            Self::accepted_units_max(LEXICAL_UNITS_MAX_DEFAULT),
            LEXICAL_UNIT_BYTES_MAX_DEFAULT,
            LEXICAL_MATCHES_MAX_DEFAULT,
            LEXICAL_POOL_SLOTS_DEFAULT,
            LEXICAL_BUSY_TIMEOUT_MS_DEFAULT,
        )
    }
}

/// Widens a bounded `usize` count into the `u64` domain; every count this module bounds
/// already fits comfortably, so the fallback only guards the conversion.
fn limit_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Narrows an accepted `u64` bound into `usize`, answering the platform's ceiling for a
/// bound no in-memory collection could reach anyway.
pub(crate) fn bound_u64_as_usize(bound: u64) -> usize {
    usize::try_from(bound).unwrap_or(usize::MAX)
}

/// Widens a `u32` domain bound into `usize` for in-memory comparisons; `u32`
/// always fits `usize` on every platform Rift targets.
pub(crate) fn bound_as_usize(bound: u32) -> usize {
    usize::try_from(bound).unwrap_or_else(|_| {
        unreachable!("u32 bound must fit usize on supported platforms: bound={bound}")
    })
}

/// Refuses an indexed set larger than `units_max`.
///
/// A whole replacement knows its resulting size before it opens a transaction. An
/// incremental apply knows it only after its deletions have run, so it counts the table
/// inside the transaction and checks the same bound here.
fn validate_indexed_count(units: usize, limits: LexicalIndexLimits) -> Result<(), RiftError> {
    if units > bound_as_usize(limits.units_max()) {
        return errors::index::lexical_unit_limit()
            .field("units_max")
            .observed(limit_count(units))
            .maximum(u64::from(limits.units_max()))
            .fail();
    }
    Ok(())
}

/// Refuses a document whose content field breaks the configured bound, one
/// addressed by a source unit, and a batch spelling one identity twice.
///
/// Field byte bounds of their own are already enforced where the document is
/// built: [`IndexDocument::new`] refuses a name, signature, or documentation
/// past the shape's own ceiling. What is left to this store is the operator's
/// `unit_bytes_max`, which only the content field can reach.
fn validate_lexical_units(
    documents: &[IndexDocument],
    limits: LexicalIndexLimits,
) -> Result<(), RiftError> {
    let mut identities_seen = std::collections::HashSet::with_capacity(documents.len());
    for document in documents {
        let path = project_location(document)?;
        let field = document.kind().content_field();
        let observed = document.content().len();
        if observed > bound_as_usize(limits.unit_bytes_max()) {
            return errors::index::lexical_unit_too_large()
                .path(path.as_str())
                .field(field.column())
                .observed(limit_count(observed))
                .maximum(u64::from(limits.unit_bytes_max()))
                .fail();
        }
        if !identities_seen.insert(document.identity()) {
            return errors::index::lexical_duplicate_identity()
                .with(ErrorContext::new("identity", document.identity().as_str()))
                .fail();
        }
    }
    Ok(())
}

/// The project path a document is addressed by, or the refusal a package
/// document earns from this store.
fn project_location(document: &IndexDocument) -> Result<&ProjectPath, RiftError> {
    match document.location() {
        DocumentLocation::Project(path) => Ok(path),
        DocumentLocation::Unit(unit) => errors::index::lexical_document_location_unsupported()
            .with(ErrorContext::new("unit", unit.to_string()))
            .fail(),
    }
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_documents"]
pub(crate) struct LexicalDocumentRecord {
    #[key]
    identity: String,
    path: String,
    kind: String,
    digest: String,
    byte_length: i64,
    byte_offset: Option<i64>,
    name: Option<String>,
    qualified_name: Option<String>,
    identifier_terms: Option<String>,
    signature: Option<String>,
    documentation: Option<String>,
    file_content: Option<String>,
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_index_state"]
pub(crate) struct LexicalIndexStateRecord {
    #[key]
    id: i64,
    tree_revision: Option<String>,
    corpus_revision: String,
    derivation_revision: String,
}

/// One indexed file's content digest: the bytes its stored rows were derived from.
///
/// The key field is named for what it holds rather than for its column: a key field named
/// `path` collides with a name the model derive generates for its own key filter.
#[derive(Debug, toasty::Model)]
#[table = "lexical_files"]
pub(crate) struct LexicalFileRecord {
    #[key]
    #[column("path")]
    project_path: String,
    digest: Vec<u8>,
}

/// Converts a content byte length already bounded by `unit_bytes_max` (a
/// `u32`) into the wire-width integer `lexical_documents.byte_length` stores.
fn checked_byte_length(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or_else(|_| {
        unreachable!(
            "content byte length must fit i64 once bounded by unit_bytes_max: bytes={bytes}"
        )
    })
}

/// Converts a row's offset in its file, bounded by the index's per-file byte bound, into
/// the integer `lexical_documents.byte_offset` stores.
fn checked_byte_offset(offset: u64) -> i64 {
    i64::try_from(offset).unwrap_or_else(|_| {
        unreachable!("a file offset must fit i64 once bounded by file_bytes_max: offset={offset}")
    })
}

/// Reads a stored row offset back, refusing a negative value no write produces.
fn stored_byte_offset(offset: i64) -> Result<u64, RiftError> {
    u64::try_from(offset).map_err(|_| {
        errors::index::lexical_storage()
            .with(ErrorContext::new(
                "byte offset",
                format!("a stored row offset is negative: offset={offset}"),
            ))
            .error()
    })
}

/// Verifies a single-row pragma result matches the value just requested.
pub(crate) fn require_pragma_row(rows: &[Value], expected: &[Value]) -> Result<(), RiftError> {
    if let [Value::Record(record)] = rows
        && record.as_slice() == expected
    {
        return Ok(());
    }
    errors::index::lexical_storage()
        .with(ErrorContext::new(
            "pragma",
            format!("unexpected pragma row: rows={rows:?}, expected={expected:?}"),
        ))
        .fail()
}

/// The searchable columns, comma separated, in the order [`SearchableField::ALL`] declares:
/// the FTS table's own column list, and the typed table's columns of the same names.
fn searchable_columns() -> String {
    SearchableField::ALL.map(SearchableField::column).join(", ")
}

/// Deletes every unit filed under one path, from both FTS indexes and the typed table
/// alike, and forgets the digest recorded for the path.
///
/// The index entries go first, through the FTS5 `'delete'` command, while the typed rows
/// still hold the values they were indexed from: the command "must match the values
/// currently stored in the table" or "the results may be unpredictable"
/// (<https://www.sqlite.org/fts5.html>). The path index finds the rows, so the work is one
/// lookup per path and one delete per unit, never a scan of an FTS table.
///
/// The trigram index gives up the path's rows through
/// [`crate::trigram_store::delete_path`], while the typed rows still hold them as well.
async fn delete_path_units(
    executor: &mut dyn Executor,
    path: &ProjectPath,
) -> Result<(), RiftError> {
    let columns = searchable_columns();
    toasty::sql::statement(format!(
        "INSERT INTO lexical_documents_fts(lexical_documents_fts, rowid, {columns}) \
         SELECT 'delete', id, {columns} FROM lexical_documents WHERE path = ?1"
    ))
    .bind(path.as_str().to_owned())
    .exec(&mut *executor)
    .await
    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    crate::trigram_store::delete_path(&mut *executor, path).await?;
    toasty::sql::statement("DELETE FROM lexical_documents WHERE path = ?1")
        .bind(path.as_str().to_owned())
        .exec(&mut *executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    LexicalFileRecord::filter_by_project_path(path.as_str())
        .delete()
        .exec(executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(())
}

/// Deletes every unit, every index entry, and every recorded digest.
///
/// `'delete-all'` clears an external-content index without reading a typed row, which is
/// what lets it run before the typed rows go: FTS5 offers it "only with external content
/// and contentless tables" and it "deletes all entries from the full-text index".
async fn delete_every_unit(executor: &mut dyn Executor) -> Result<(), RiftError> {
    toasty::sql::statement(
        "INSERT INTO lexical_documents_fts(lexical_documents_fts) VALUES('delete-all')",
    )
    .exec(&mut *executor)
    .await
    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    crate::trigram_store::clear(&mut *executor).await?;
    LexicalDocumentRecord::all()
        .delete()
        .exec(&mut *executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    LexicalFileRecord::all()
        .delete()
        .exec(executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(())
}

/// What one publication stamped the store with.
///
/// The tree revision says which publication the rows answer for, and is absent while a
/// write too large for one transaction is under way. The corpus revision says whether this
/// build can read the rows at all, and the derivation revision whether this build would
/// derive the same rows from the same bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
struct StoredStamp {
    tree_revision: Option<String>,
    corpus_revision: CorpusRevision,
    derivation_revision: String,
}

/// The stamp the store carries, read through `executor` so a caller can place
/// it in the same transaction as the query it qualifies.
async fn stored_stamp(executor: &mut dyn Executor) -> Result<Option<StoredStamp>, RiftError> {
    let record = LexicalIndexStateRecord::filter_by_id(LEXICAL_INDEX_STATE_ID)
        .first()
        .exec(executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(record.map(|record| StoredStamp {
        tree_revision: record.tree_revision,
        corpus_revision: CorpusRevision::stored(record.corpus_revision),
        derivation_revision: record.derivation_revision,
    }))
}

/// What a reader answers before it ranks, when the stored stamp does not name
/// `tree_revision`: nothing to rank, or a store that moved to another tree.
///
/// A store holding no stamp, a stamp naming no tree, or rows under another corpus revision
/// answers [`RevisionScoped::NoRevision`]: none of them holds a publication this build can
/// read. `None` means the store holds `tree_revision` and the reader goes on to rank.
fn stamp_scope<T>(stored: Option<StoredStamp>, tree_revision: &str) -> Option<RevisionScoped<T>> {
    let Some(stored) = stored else {
        return Some(RevisionScoped::NoRevision);
    };
    if stored.corpus_revision != CorpusRevision::current() {
        return Some(RevisionScoped::NoRevision);
    }
    match stored.tree_revision {
        None => Some(RevisionScoped::NoRevision),
        Some(stored) if stored != tree_revision => Some(RevisionScoped::OtherRevision(stored)),
        Some(_) => None,
    }
}

/// Stamps the store with `stamp` and the corpus revision this build derives, so a later
/// read can tell which publication the rows belong to and whether it can read them at
/// all, and a later write whether it can keep them.
async fn stamp(executor: &mut dyn Executor, stamp: &LexicalStamp) -> Result<(), RiftError> {
    LexicalIndexStateRecord::upsert_by_id(LEXICAL_INDEX_STATE_ID)
        .tree_revision(stamp.tree_revision.clone())
        .corpus_revision(CorpusRevision::current().as_str().to_owned())
        .derivation_revision(stamp.derivation_revision.clone())
        .exec(executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(())
}

/// How many documents the typed table holds right now.
async fn indexed_unit_count(executor: &mut dyn Executor) -> Result<usize, RiftError> {
    let counted = single_i64(executor, "SELECT count(*) FROM lexical_documents", "count").await?;
    Ok(usize::try_from(counted).unwrap_or(usize::MAX))
}

/// How many typed rows hold file text: whole files and the chunks of large ones. A
/// partial index over those rows alone answers the count without reading a symbol row.
pub(crate) async fn file_row_count(executor: &mut dyn Executor) -> Result<u64, RiftError> {
    let counted = single_i64(
        executor,
        "SELECT count(*) FROM lexical_documents WHERE file_content IS NOT NULL",
        "file rows",
    )
    .await?;
    Ok(u64::try_from(counted).unwrap_or(0))
}

/// How many file rows hold `term`, read from the word index's own `file_content` column
/// through its `fts5vocab` table: one term lookup, never a scan of the rows.
async fn file_rows_holding(executor: &mut dyn Executor, term: &str) -> Result<u64, RiftError> {
    let rows = toasty::sql::query(
        "SELECT doc FROM lexical_documents_vocabulary \
         WHERE term = ?1 AND col = 'file_content'",
    )
    .bind(term.to_owned())
    .column_types([Type::I64])
    .exec(executor)
    .await
    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(match rows.as_slice() {
        [Value::Record(record)] => match record.as_slice() {
            [Value::I64(holding)] => u64::try_from(*holding).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    })
}

/// The highest typed-row id right now, or zero in an empty table.
///
/// An `INTEGER PRIMARY KEY` row inserted without an id takes one past the highest id the
/// table holds, so every row inserted after this read has an id above it.
async fn last_unit_id(executor: &mut dyn Executor) -> Result<i64, RiftError> {
    single_i64(
        executor,
        "SELECT coalesce(max(id), 0) FROM lexical_documents",
        "last id",
    )
    .await
}

/// Runs one query answering one integer.
pub(crate) async fn single_i64(
    executor: &mut dyn Executor,
    sql: &'static str,
    label: &'static str,
) -> Result<i64, RiftError> {
    let rows = toasty::sql::query(sql)
        .column_types([Type::I64])
        .exec(executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    if let [Value::Record(record)] = rows.as_slice()
        && let [Value::I64(value)] = record.as_slice()
    {
        return Ok(*value);
    }
    errors::index::lexical_storage()
        .with(ErrorContext::new(
            label,
            format!("unexpected {label} rows: rows={rows:?}"),
        ))
        .fail()
}

/// Writes bounded batches of typed rows, then indexes every row this call wrote in one
/// statement, and files the file rows among them for the trigram index.
///
/// The typed row and the index entry carry the same fields because the index reads them
/// from the typed row: a column filled in one and absent from the other would rank a
/// document the reader cannot then describe. Indexing the batch's rows together, selected
/// by the ids they took above [`last_unit_id`], hands FTS5 one set-based insert rather
/// than one statement per document. The file rows wait for the trigram index, which
/// [`LexicalSearchIndex::index_trigrams`] fills.
async fn insert_documents(
    executor: &mut dyn Executor,
    documents: &[IndexDocument],
) -> Result<(), RiftError> {
    let last = last_unit_id(&mut *executor).await?;
    for documents in documents.chunks(LEXICAL_INSERT_ROWS_MAX) {
        let mut insert = LexicalDocumentRecord::create_many();
        for document in documents {
            insert = insert.item(insert_document(document, project_location(document)?));
        }
        insert
            .exec(&mut *executor)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    }
    let columns = searchable_columns();
    toasty::sql::statement(format!(
        "INSERT INTO lexical_documents_fts(rowid, {columns}) \
         SELECT id, {columns} FROM lexical_documents WHERE id > ?1"
    ))
    .bind(last)
    .exec(&mut *executor)
    .await
    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    crate::trigram_store::file_rows_above(executor, last).await
}

/// Builds one document's typed insert, for the batch that writes it.
fn insert_document(
    document: &IndexDocument,
    path: &ProjectPath,
) -> impl IntoInsert<Model = LexicalDocumentRecord> {
    let fields = document.fields();
    let field = |searchable: SearchableField| fields.get(searchable).map(str::to_owned);
    let content = document.content();
    toasty::create!(LexicalDocumentRecord {
        identity: document.identity().as_str().to_owned(),
        path: path.as_str().to_owned(),
        kind: document.kind().stored_value().to_owned(),
        digest: document.digest().to_owned(),
        byte_length: checked_byte_length(content.len()),
        byte_offset: document.byte_offset().map(checked_byte_offset),
        name: field(SearchableField::Name),
        qualified_name: field(SearchableField::QualifiedName),
        identifier_terms: field(SearchableField::IdentifierTerms),
        signature: field(SearchableField::Signature),
        documentation: field(SearchableField::Documentation),
        file_content: field(SearchableField::FileContent),
    })
}

/// Records the digest each named file's rows were derived from.
async fn record_files(
    executor: &mut dyn Executor,
    recorded: &[(ProjectPath, FileDigest)],
) -> Result<(), RiftError> {
    for (path, digest) in recorded {
        toasty::create!(LexicalFileRecord {
            project_path: path.as_str().to_owned(),
            digest: digest.as_bytes().to_vec(),
        })
        .exec(&mut *executor)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    }
    Ok(())
}

/// Reads one recorded row back as the path and digest it was written from.
fn decode_recorded(record: LexicalFileRecord) -> Result<(ProjectPath, FileDigest), RiftError> {
    let path = ProjectPath::new(record.project_path).map_err(|source| {
        errors::index::lexical_stored_path_invalid()
            .source(source)
            .error()
    })?;
    let observed = record.digest.len();
    let bytes: [u8; RECORDED_DIGEST_BYTES] = record.digest.try_into().map_err(|_| {
        errors::index::lexical_storage()
            .with(ErrorContext::new(
                "recorded digest",
                format!(
                    "path {path} records a {observed}-byte digest; a file digest holds \
                 {RECORDED_DIGEST_BYTES} bytes"
                ),
            ))
            .error()
    })?;
    Ok((path, FileDigest::from_bytes(bytes)))
}

/// Rebuilds one published document from its typed row.
///
/// The row carries the same fields the document was written from, so this is the
/// inverse of [`insert_document`] and nothing is derived a second time.
fn decode_document(record: LexicalDocumentRecord) -> Result<IndexDocument, RiftError> {
    let path = ProjectPath::new(record.path).map_err(|source| {
        errors::index::lexical_stored_path_invalid()
            .source(source)
            .error()
    })?;
    let kind = DocumentKind::from_stored(&record.kind).ok_or_else(|| {
        errors::index::lexical_stored_kind_invalid()
            .with(ErrorContext::new("kind", record.kind.clone()))
            .error()
    })?;
    let identity = DocumentIdentity::new(record.identity).map_err(|source| {
        errors::index::lexical_stored_kind_invalid()
            .source(source)
            .error()
    })?;
    let stored = [
        (SearchableField::Name, record.name),
        (SearchableField::QualifiedName, record.qualified_name),
        (SearchableField::IdentifierTerms, record.identifier_terms),
        (SearchableField::Signature, record.signature),
        (SearchableField::Documentation, record.documentation),
        (SearchableField::FileContent, record.file_content),
    ];
    let fields = stored
        .into_iter()
        .fold(DocumentFields::empty(), |held, (field, value)| {
            held.with_optional(field, value)
        });
    let document = IndexDocument::new(
        identity,
        DocumentLocation::Project(path),
        kind,
        record.digest,
        fields,
    )
    .map_err(|source| {
        errors::index::lexical_stored_kind_invalid()
            .source(source)
            .error()
    })?;
    Ok(match record.byte_offset {
        Some(offset) => document.at_byte_offset(stored_byte_offset(offset)?),
        None => document,
    })
}

/// Reconstructs one search hit from a raw joined row.
///
/// The row shape follows directly from this module's own `SELECT` and
/// declared `column_types`; a mismatch is a programmer invariant, not an
/// operating failure.
fn decode_lexical_match(row: &Value) -> Result<LexicalMatch, RiftError> {
    let Value::Record(record) = row else {
        unreachable!("lexical search row must be a record: row={row:?}");
    };
    let [
        Value::String(identity),
        Value::String(path),
        Value::String(kind),
        byte_offset,
        Value::I64(byte_length),
        Value::F64(rank),
        isolated @ ..,
    ] = record.as_slice()
    else {
        unreachable!("lexical search row must match its declared column types: row={row:?}");
    };
    let path = ProjectPath::new(path.clone()).map_err(|source| {
        errors::index::lexical_stored_path_invalid()
            .source(source)
            .error()
    })?;
    let kind = DocumentKind::from_stored(kind).ok_or_else(|| {
        errors::index::lexical_stored_kind_invalid()
            .with(ErrorContext::new("kind", kind.clone()))
            .error()
    })?;
    let identity = DocumentIdentity::new(identity.clone()).map_err(|source| {
        errors::index::lexical_stored_kind_invalid()
            .source(source)
            .error()
    })?;
    let file_range = match byte_offset {
        Value::I64(offset) => Some(stored_file_range(*offset, *byte_length)?),
        _ => None,
    };
    Ok(LexicalMatch {
        identity,
        path,
        kind,
        rank: *rank,
        fields: matched_fields(isolated),
        file_range,
    })
}

/// The bytes of its file one stored row holds: its offset, and the length of the text
/// it stores.
pub(crate) fn stored_file_range(offset: i64, length: i64) -> Result<Range<u64>, RiftError> {
    let start = stored_byte_offset(offset)?;
    let length = stored_byte_offset(length)?;
    Ok(start..start.saturating_add(length))
}

/// The columns that carried a query member.
///
/// Each value is `bm25` run with every weight zeroed but one. A column that
/// matched no member contributes nothing and the call answers zero, so a
/// non-zero value is exactly the proof that this column placed the hit.
fn matched_fields(isolated: &[Value]) -> FieldSet {
    SearchableField::ALL
        .into_iter()
        .zip(isolated)
        .filter(|(_, value)| matches!(value, Value::F64(score) if *score != 0.0))
        .map(|(field, _)| field)
        .collect()
}

/// The `bm25` weight arguments, in FTS column order: one weight per searchable field.
///
/// The weights come from [`SearchableField`] rather than from constants here,
/// so the ranking this store runs and the ranking every other reader runs are
/// the same numbers. FTS5 takes them as literal arguments, never as bind
/// parameters, which is why they are rendered into the statement.
fn rank_weights() -> String {
    SearchableField::ALL
        .map(SearchableField::rank_weight)
        .map(|weight| weight.to_string())
        .join(", ")
}

/// The `bm25` weight arguments that isolate one column: every weight zero
/// except this field's, set to one.
fn isolated_weights(field: SearchableField) -> String {
    SearchableField::ALL
        .map(|declared| f64::from(u8::from(declared == field)))
        .map(|weight| weight.to_string())
        .join(", ")
}

/// Runs the ranked full-text statement for one rendered `MATCH` expression and decodes
/// every row it answers, at most `probe_limit`.
async fn ranked_rows(
    transaction: &mut dyn Executor,
    expression: String,
    probe_limit: i64,
) -> Result<Vec<LexicalMatch>, RiftError> {
    let rows = toasty::sql::query(lexical_search_sql())
        .bind(expression)
        .bind(probe_limit)
        .column_types(lexical_search_column_types())
        .exec(transaction)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    rows.iter().map(decode_lexical_match).collect()
}

/// Builds the joined ranking query.
///
/// Equal ranks order by identity, because `SQLite` would otherwise return them
/// in whatever order it read the rows and two readers over one publication
/// must answer one order.
///
/// One `bm25` call ranks the row and one more per column proves which columns
/// carried a member. FTS5 evaluates an auxiliary function per candidate row,
/// so each candidate row costs one `bm25` evaluation for its rank and one per
/// column of `SearchableField::ALL`; the `matches_max` bound keeps that
/// bounded, and what it buys is a `matched_by` a reader can act on.
fn lexical_search_sql() -> String {
    let ranked = rank_weights();
    let mut isolated = String::new();
    for field in SearchableField::ALL {
        let weights = isolated_weights(field);
        let column = field.column();
        let _ = write!(
            isolated,
            ", bm25(lexical_documents_fts, {weights}) AS {column}_hit"
        );
    }
    format!(
        "SELECT lexical_documents.identity, lexical_documents.path, \
         lexical_documents.kind, lexical_documents.byte_offset, \
         lexical_documents.byte_length, \
         bm25(lexical_documents_fts, {ranked}) AS rank{isolated} \
         FROM lexical_documents_fts \
         JOIN lexical_documents \
         ON lexical_documents.id = lexical_documents_fts.rowid \
         WHERE lexical_documents_fts MATCH ?1 \
         ORDER BY rank, lexical_documents.identity LIMIT ?2"
    )
}

/// The declared result types of [`lexical_search_sql`], in column order.
fn lexical_search_column_types() -> Vec<Type> {
    let mut types = vec![
        Type::String,
        Type::String,
        Type::String,
        Type::I64,
        Type::I64,
        Type::F64,
    ];
    types.extend(SearchableField::ALL.map(|_| Type::F64));
    types
}

/// What one change set does to the lexical index.
///
/// A rebuild writes what it read under each path it named and nothing else, so `replaced`
/// names every one of those paths and `inserted` carries the units the rebuilt index
/// derived for them. A path the rebuild read appears in both halves; a path it found gone
/// appears only in the first. `recorded` carries the content digest of every replaced path
/// the rebuild still holds, which is what a later process compares its own tree against
/// before it writes anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LexicalChange {
    replaced: Vec<ProjectPath>,
    inserted: Vec<IndexDocument>,
    recorded: Vec<(ProjectPath, FileDigest)>,
}

impl LexicalChange {
    /// Builds one change from the paths whose units go and the units that replace them,
    /// recording no digest.
    #[must_use]
    pub fn new(replaced: Vec<ProjectPath>, inserted: Vec<IndexDocument>) -> Self {
        Self {
            replaced,
            inserted,
            recorded: Vec::new(),
        }
    }

    /// This change, recording the content digest each replaced path's units were derived
    /// from.
    #[must_use]
    pub fn with_recorded(self, recorded: Vec<(ProjectPath, FileDigest)>) -> Self {
        Self { recorded, ..self }
    }

    /// The paths whose stored units this change deletes before it inserts.
    #[must_use]
    pub fn replaced(&self) -> &[ProjectPath] {
        &self.replaced
    }

    /// The units this change inserts.
    #[must_use]
    pub fn inserted(&self) -> &[IndexDocument] {
        &self.inserted
    }

    /// The content digests this change records, one per replaced path the rebuild holds.
    #[must_use]
    pub fn recorded(&self) -> &[(ProjectPath, FileDigest)] {
        &self.recorded
    }

    /// Whether this change writes nothing, so the stored set already answers for the tree
    /// that produced it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replaced.is_empty() && self.inserted.is_empty() && self.recorded.is_empty()
    }

    /// This change as consecutive parts, each writing at most `units_max` units and
    /// `bytes_max` content bytes, in replaced-path order.
    ///
    /// A path's deletion, its units, and its recorded digest always land in one part, so
    /// applying the parts in order leaves exactly what applying the whole change leaves,
    /// and a store that stops between two parts holds whole paths alone. A path whose own
    /// units pass either bound takes a part of its own. A unit filed under a path the
    /// change does not replace joins the last part. An empty change answers one empty
    /// part, since the part that lands last is what stamps the store.
    #[must_use]
    pub fn into_parts_within(self, units_max: usize, bytes_max: usize) -> Vec<Self> {
        let Self {
            replaced,
            inserted,
            recorded,
        } = self;
        let mut units_by_path: BTreeMap<ProjectPath, Vec<IndexDocument>> = replaced
            .iter()
            .cloned()
            .map(|path| (path, Vec::new()))
            .collect();
        let mut unfiled = Vec::new();
        for unit in inserted {
            if let DocumentLocation::Project(path) = unit.location()
                && let Some(units) = units_by_path.get_mut(path)
            {
                units.push(unit);
            } else {
                unfiled.push(unit);
            }
        }
        let mut digests: BTreeMap<ProjectPath, FileDigest> = recorded.into_iter().collect();
        let mut parts = vec![Self::default()];
        let mut room = PartRoom::new(units_max, bytes_max);
        for path in replaced {
            let units = units_by_path.remove(&path).unwrap_or_default();
            if !room.admits(&units) {
                parts.push(Self::default());
                room = PartRoom::new(units_max, bytes_max);
            }
            room.take(&units);
            let last = parts.len() - 1;
            let part = &mut parts[last];
            part.recorded
                .extend(digests.remove(&path).map(|digest| (path.clone(), digest)));
            part.replaced.push(path);
            part.inserted.extend(units);
        }
        let last = parts.len() - 1;
        parts[last].inserted.extend(unfiled);
        parts[last].recorded.extend(digests);
        parts
    }

    /// The change as its three halves, for a caller that rebuilds it over a narrowed
    /// unit list.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<ProjectPath>,
        Vec<IndexDocument>,
        Vec<(ProjectPath, FileDigest)>,
    ) {
        (self.replaced, self.inserted, self.recorded)
    }
}

/// What one part of a split change has left to hold, in units and content bytes.
///
/// A part that holds nothing yet admits any path, which is what lets a path larger than
/// either bound take a part of its own.
struct PartRoom {
    units_left: usize,
    bytes_left: usize,
    empty: bool,
}

impl PartRoom {
    const fn new(units_max: usize, bytes_max: usize) -> Self {
        Self {
            units_left: units_max,
            bytes_left: bytes_max,
            empty: true,
        }
    }

    /// Whether this part can take one path's `units` whole.
    fn admits(&self, units: &[IndexDocument]) -> bool {
        let fits_units = units.len() <= self.units_left;
        let fits_bytes = content_bytes(units) <= self.bytes_left;
        self.empty || (fits_units && fits_bytes)
    }

    /// Counts one path's `units` against this part.
    fn take(&mut self, units: &[IndexDocument]) {
        self.units_left = self.units_left.saturating_sub(units.len());
        self.bytes_left = self.bytes_left.saturating_sub(content_bytes(units));
        self.empty = false;
    }
}

/// The content bytes `units` carry together.
fn content_bytes(units: &[IndexDocument]) -> usize {
    units
        .iter()
        .map(|unit| unit.content().len())
        .fold(0, usize::saturating_add)
}

/// What one lexical transaction stamps the store with.
///
/// `tree_revision` names the publication the rows answer for once the transaction
/// commits. A write too large for one transaction commits its earlier parts with no tree
/// revision, so a reader meets no publication rather than a mix of two, and only its last
/// part names the tree. `derivation_revision` names what, besides a file's bytes, decided
/// the rows: a later write keeps rows stamped with its own derivation revision and
/// replaces every row stamped with another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalStamp {
    tree_revision: Option<String>,
    derivation_revision: String,
}

impl LexicalStamp {
    /// A stamp naming the publication `tree_revision` the rows answer for.
    #[must_use]
    pub fn published(tree_revision: &str, derivation_revision: &str) -> Self {
        Self {
            tree_revision: Some(tree_revision.to_owned()),
            derivation_revision: derivation_revision.to_owned(),
        }
    }

    /// A stamp naming no publication, for the parts of a write that commit before its last.
    #[must_use]
    pub fn unpublished(derivation_revision: &str) -> Self {
        Self {
            tree_revision: None,
            derivation_revision: derivation_revision.to_owned(),
        }
    }

    /// The publication this stamp names, if any.
    #[must_use]
    pub fn tree_revision(&self) -> Option<&str> {
        self.tree_revision.as_deref()
    }

    /// What besides a file's bytes decided the rows this stamp covers.
    #[must_use]
    pub fn derivation_revision(&self) -> &str {
        &self.derivation_revision
    }
}

/// Which documentation metadata one transaction writes.
enum DocumentationWrite {
    /// The stored metadata stays as it is.
    Kept,
    /// The stored metadata is replaced by this encoding, or cleared when there is none.
    Replaced(Option<crate::documentation_store::EncodedDocumentation>),
}

/// Which stored units one write deletes before it inserts.
#[derive(Clone, Copy)]
enum StoredUnits<'a> {
    /// Every stored unit and recorded digest goes, then these units land.
    Every(&'a [IndexDocument]),
    /// The units and digests under the change's replaced paths go, then its units and
    /// digests land.
    Named(&'a LexicalChange),
}

impl<'a> StoredUnits<'a> {
    /// The write's form, as its span names it.
    const fn mode(self) -> &'static str {
        match self {
            Self::Every(_) => "replace",
            Self::Named(_) => "apply",
        }
    }

    /// The units this write inserts.
    fn inserted(self) -> &'a [IndexDocument] {
        match self {
            Self::Every(inserted) => inserted,
            Self::Named(change) => change.inserted(),
        }
    }

    /// Deletes what this write replaces and inserts what it carries, inside the caller's
    /// transaction.
    async fn write(
        self,
        executor: &mut dyn Executor,
        limits: LexicalIndexLimits,
    ) -> Result<(), RiftError> {
        match self {
            Self::Every(inserted) => {
                delete_every_unit(&mut *executor).await?;
                insert_documents(executor, inserted).await
            }
            Self::Named(change) => {
                for path in change.replaced() {
                    delete_path_units(&mut *executor, path).await?;
                }
                let stored = indexed_unit_count(&mut *executor).await?;
                validate_indexed_count(stored.saturating_add(change.inserted().len()), limits)?;
                insert_documents(&mut *executor, change.inserted()).await?;
                record_files(executor, change.recorded()).await
            }
        }
    }
}

/// `SQLite` FTS5-backed lexical search index.
///
/// [`IndexDocument`] is the one shape every Rift index publishes, so a
/// project row and a package row carry the same fields and the same identity
/// spelling. This store holds project documents: a document addressed by a
/// source unit belongs to a package index, and the write path refuses it
/// rather than filing a unit URI in the path column.
#[derive(Debug)]
pub struct LexicalSearchIndex {
    database: Arc<WorkspaceDatabase>,
    limits: LexicalIndexLimits,
}

impl LexicalSearchIndex {
    /// Attaches the lexical tier to the open index database.
    ///
    /// The pool is shared with the documentation and trigram stores in the file, because
    /// `SQLite` serializes writers per file: a second pool would only add a connection
    /// that loses the same write lock.
    ///
    /// # Panics
    ///
    /// Panics when `database` is not the index database: its file holds no lexical table.
    #[must_use]
    #[track_caller]
    pub fn attached(database: Arc<WorkspaceDatabase>, limits: LexicalIndexLimits) -> Self {
        database.name().assert_is(DatabaseName::Index);
        Self { database, limits }
    }

    /// A pooled connection carrying the workspace database's required pragmas.
    async fn configured_connection(&self) -> Result<Connection, RiftError> {
        self.database.connection().await
    }

    /// Atomically replaces every indexed unit and stamps `tree_revision`.
    ///
    /// Refuses before opening any transaction when `units` exceeds
    /// `units_max`, when one unit's content exceeds `unit_bytes_max`, or when
    /// two units share one identity: the previously indexed state stays
    /// fully intact in every refusal case.
    ///
    /// The replacement records no file digest and stamps no derivation revision, so a
    /// later [`Self::recorded_files`] finds nothing it can keep and the next write that
    /// compares against the store replaces this set whole.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on bound violations or storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before commit leaves the previously indexed state fully
    /// intact, since `SQLite` has not yet committed the replacing
    /// transaction. Cancellation after commit is indistinguishable from
    /// normal completion.
    pub async fn replace_all(
        &self,
        documents: &[IndexDocument],
        tree_revision: &str,
    ) -> Result<(), RiftError> {
        validate_indexed_count(documents.len(), self.limits)?;
        validate_lexical_units(documents, self.limits)?;
        self.write(
            StoredUnits::Every(documents),
            &LexicalStamp::published(tree_revision, ""),
            DocumentationWrite::Replaced(None),
        )
        .await
    }

    /// Replaces lexical documents and validated documentation metadata in one transaction.
    ///
    /// # Errors
    ///
    /// Refuses invalid document batches or storage failure. Metadata that encodes past
    /// [`LexicalIndexLimits::documentation_bytes_max`] is left out of the commit instead.
    pub async fn replace_all_with_documentation(
        &self,
        documents: &[IndexDocument],
        tree_revision: &str,
        documentation: &rift_analysis::documentation::DocumentationCollection,
    ) -> Result<(), RiftError> {
        validate_indexed_count(documents.len(), self.limits)?;
        validate_lexical_units(documents, self.limits)?;
        let metadata = crate::documentation_store::encode_within(
            Some(documentation),
            self.limits.documentation_bytes_max(),
        )?;
        self.write(
            StoredUnits::Every(documents),
            &LexicalStamp::published(tree_revision, ""),
            DocumentationWrite::Replaced(metadata),
        )
        .await
    }

    /// Deletes every unit, every recorded digest, and the documentation metadata, and
    /// stamps no publication under `derivation_revision`.
    ///
    /// A write that finds the stored rows derived under another derivation revision
    /// clears them first, then fills the store with changes stamped under its own.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before commit leaves every row and the stamp intact.
    pub async fn clear(&self, derivation_revision: &str) -> Result<(), RiftError> {
        self.write(
            StoredUnits::Every(&[]),
            &LexicalStamp::unpublished(derivation_revision),
            DocumentationWrite::Replaced(None),
        )
        .await
    }

    /// Applies one change set's documents and digests and stamps `stamp`, in one
    /// transaction, leaving the documentation metadata as it is.
    ///
    /// The transaction deletes every unit filed under a replaced path, inserts the units the
    /// change derived, records the digests it carries, and stamps the store. Deleting by
    /// path is what makes this incremental: a text file split into chunks files every chunk
    /// under its own path, so one delete reaches all of them.
    ///
    /// The resulting set is counted inside the transaction, after the deletions and before
    /// the inserts, because an incremental apply cannot know its own resulting size any
    /// earlier - and `units_max` binds the indexed set, not one batch.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when one unit breaks a field bound, when two units
    /// share one identity, when the resulting set would exceed `units_max`, or on storage
    /// failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before commit leaves the previously indexed units, digests, and stamp
    /// fully intact. Cancellation after commit has the same durable result as completion.
    pub async fn apply(
        &self,
        change: &LexicalChange,
        stamp: &LexicalStamp,
    ) -> Result<(), RiftError> {
        validate_lexical_units(change.inserted(), self.limits)?;
        self.write(StoredUnits::Named(change), stamp, DocumentationWrite::Kept)
            .await
    }

    /// Applies lexical changes and replaces their documentation metadata atomically.
    ///
    /// # Errors
    ///
    /// Refuses invalid document batches or storage failure. Metadata that encodes past
    /// [`LexicalIndexLimits::documentation_bytes_max`] is left out of the commit instead.
    pub async fn apply_with_documentation(
        &self,
        change: &LexicalChange,
        stamp: &LexicalStamp,
        documentation: &rift_analysis::documentation::DocumentationCollection,
    ) -> Result<(), RiftError> {
        validate_lexical_units(change.inserted(), self.limits)?;
        let metadata = crate::documentation_store::encode_within(
            Some(documentation),
            self.limits.documentation_bytes_max(),
        )?;
        self.write(
            StoredUnits::Named(change),
            stamp,
            DocumentationWrite::Replaced(metadata),
        )
        .await
    }

    /// The content digest each stored file's rows were derived from, when those rows were
    /// derived under `derivation_revision` and this build's corpus revision.
    ///
    /// `None` means no stored row can be kept: the store holds no stamp, or rows another
    /// build, another configuration, or another corpus shape derived. A store whose last
    /// write stopped between its parts answers what those parts recorded, because each
    /// part recorded its paths' digests in the transaction that wrote their rows.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the store records more files than `units_max`,
    /// when a recorded row fails to decode, or on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the transaction is read-only.
    pub async fn recorded_files(
        &self,
        derivation_revision: &str,
    ) -> Result<Option<WorkspaceDigests>, RiftError> {
        let mut connection = self.database.connection().await?;
        let mut transaction = connection
            .transaction()
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        let Some(stored) = stored_stamp(&mut transaction).await? else {
            return Ok(None);
        };
        let corpus_matches = stored.corpus_revision == CorpusRevision::current();
        let derivation_matches = stored.derivation_revision == derivation_revision;
        if !(corpus_matches && derivation_matches) {
            return Ok(None);
        }
        let bound = bound_as_usize(self.limits.units_max());
        let records = LexicalFileRecord::all()
            .limit(bound.saturating_add(1))
            .exec(&mut transaction)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        if records.len() > bound {
            return errors::index::lexical_record_limit()
                .field("lexical.files")
                .observed(limit_count(records.len()))
                .maximum(u64::from(self.limits.units_max()))
                .fail();
        }
        let recorded = records
            .into_iter()
            .map(decode_recorded)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(WorkspaceDigests::new(recorded)))
    }

    /// Runs one write as one transaction: the units, then the documentation metadata the
    /// write replaces, then the stamp.
    async fn write(
        &self,
        units: StoredUnits<'_>,
        stamp_with: &LexicalStamp,
        documentation: DocumentationWrite,
    ) -> Result<(), RiftError> {
        let limits = self.limits;
        let mode = units.mode();
        let documents = units.inserted().len();
        rift_tracing::traced!(
            component = "lexical",
            operation = "lexical.commit",
            open = true,
            mode = mode,
            async move {
                let mut access = rift_tracing::traced!(
                    component = "lexical",
                    operation = "lexical.write_turn",
                    async move { self.database.writing().await }
                )
                .await?;
                let mut transaction = access.transaction().await?;

                let executor = &mut transaction;
                rift_tracing::traced!(
                    component = "lexical",
                    operation = "lexical.documents",
                    documents = documents,
                    async move { units.write(executor, limits).await }
                )
                .await?;

                if let DocumentationWrite::Replaced(metadata) = documentation {
                    let executor = &mut transaction;
                    rift_tracing::traced!(
                        component = "lexical",
                        operation = "lexical.documentation",
                        async move {
                            crate::documentation_store::replace(executor, metadata.as_ref()).await
                        }
                    )
                    .await?;
                }
                stamp(&mut transaction, stamp_with).await?;

                transaction
                    .commit()
                    .await
                    .map_err(|source| errors::index::lexical_storage().source(source).error())
            }
        )
        .await
    }

    /// Reads validated documentation metadata from the same revision as lexical documents.
    ///
    /// # Errors
    ///
    /// Returns a storage refusal for corrupt metadata or database failure.
    pub async fn documentation(
        &self,
        tree_revision: &str,
    ) -> Result<
        RevisionScoped<Option<rift_analysis::documentation::DocumentationCollection>>,
        RiftError,
    > {
        let mut connection = self.database.connection().await?;
        let mut transaction = connection
            .transaction()
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        let stored = stored_stamp(&mut transaction).await?;
        if let Some(scoped) = stamp_scope(stored, tree_revision) {
            return Ok(scoped);
        }
        crate::documentation_store::read(&mut transaction, self.limits.documentation_bytes_max())
            .await
            .map(RevisionScoped::Matched)
    }

    /// Searches the documents stamped with `tree_revision`, best matches first.
    ///
    /// The stored stamp and the matching rows are read in one transaction, so a commit
    /// that lands between them cannot slip rows from another tree into the answer.
    /// A store holding another tree returns [`RevisionScoped::OtherRevision`] rather than
    /// rows the caller cannot place, and one holding no tree at all returns
    /// [`RevisionScoped::NoRevision`].
    ///
    /// A store built under another corpus revision answers
    /// [`RevisionScoped::NoRevision`] as well: its columns, derivation,
    /// tokenizer, or weights are not this build's, so it holds no publication
    /// this build can read, and the next publication replaces it.
    ///
    /// A query carrying no member matches nothing. The effective result count is
    /// `limit` capped by `matches_max`; the ranking says when the store held a match past
    /// that count, so a caller can tell a full answer from a cut one.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `query` carries more distinct terms
    /// than `query_terms_max`, when a stored row fails to decode, or on
    /// storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the transaction is read-only and is
    /// rolled back when it is dropped.
    pub async fn search(
        &self,
        tree_revision: &str,
        query: &ParsedQuery,
        phase: QueryPhase,
        limit: u32,
    ) -> Result<RevisionScoped<LexicalRanking>, RiftError> {
        // The statement carries no path predicate. A caller narrowing by path
        // screens the ranked identities it reads back, because the request's
        // glob selector and this statement would be two spellings of one
        // predicate, in two languages, that can disagree. What the caller pays
        // is a query whose matches all sit outside its selector: the bound cuts
        // before the screen runs, the answer is short, and `results_truncated`
        // says the bound cut it.
        let expression = query.render(phase);
        let bound = limit.min(self.limits.matches_max());
        // One row past the bound tells whether the store holds a match the bound cuts.
        let probe_limit = i64::from(bound) + 1;

        let phase_label = phase.label();
        rift_tracing::traced!(
            component = "lexical",
            operation = "lexical.search",
            phase = phase_label,
            async move {
                let mut connection = rift_tracing::traced!(
                    component = "lexical",
                    operation = "lexical.connection",
                    async move { self.database.connection().await }
                )
                .await?;
                let mut transaction = connection
                    .transaction()
                    .await
                    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
                let stamp_reader = &mut transaction;
                let stored = rift_tracing::traced!(
                    component = "lexical",
                    operation = "lexical.stamp",
                    async move { stored_stamp(stamp_reader).await }
                )
                .await?;
                if let Some(scoped) = stamp_scope(stored, tree_revision) {
                    return Ok(scoped);
                }
                let Some(expression) = expression else {
                    return Ok(RevisionScoped::Matched(LexicalRanking::from_probe(
                        Vec::new(),
                        bound,
                    )));
                };
                let query_reader = &mut transaction;
                let matches = rift_tracing::traced!(
                    component = "lexical",
                    operation = "lexical.query",
                    async move { ranked_rows(query_reader, expression, probe_limit).await }
                )
                .await?;
                Ok(RevisionScoped::Matched(LexicalRanking::from_probe(
                    matches, bound,
                )))
            }
        )
        .await
    }

    /// How many file rows the store holds for `tree_revision`, and how many of them hold
    /// each of `terms`: the document frequencies a body match scores declarations with.
    ///
    /// Both reads run in one transaction beside the stamp, so the counts answer for the
    /// same rows the ranking of that tree read. The work is one indexed count and one
    /// `fts5vocab` lookup per term.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the transaction is read-only.
    pub async fn file_row_frequencies(
        &self,
        tree_revision: &str,
        terms: &BodyTerms,
    ) -> Result<RevisionScoped<FileRowFrequencies>, RiftError> {
        let mut connection = self.database.connection().await?;
        let mut transaction = connection
            .transaction()
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        let stored = stored_stamp(&mut transaction).await?;
        if let Some(scoped) = stamp_scope(stored, tree_revision) {
            return Ok(scoped);
        }
        let rows = file_row_count(&mut transaction).await?;
        let mut by_term = Vec::new();
        for term in terms.iter() {
            let holding = file_rows_holding(&mut transaction, term).await?;
            by_term.push((term.to_owned(), holding));
        }
        Ok(RevisionScoped::Matched(FileRowFrequencies::new(
            rows, by_term,
        )))
    }

    /// The files a regex pattern's prefilter selects from the trigram index, for the tree
    /// stamped `tree_revision`, in project-path order, and the file rows the index lacks.
    ///
    /// A `line_bound` pattern matches inside one line, and a line sits inside one row,
    /// since a chunk packs whole lines: one `MATCH` of the whole formula selects the rows,
    /// and each candidate carries the spans of its selected rows. A formula nested past
    /// [`rift_ranking::ROW_EXPRESSION_DEPTH_MAX`], and any pattern that may cross a line,
    /// runs one `MATCH` per literal and combines their files instead, each file a whole
    /// candidate. At most `rows_max` rows are read, counted over every `MATCH` the formula
    /// runs; past it the answer says so and carries no candidate.
    ///
    /// The index holds the rows [`Self::index_trigrams`] has reached, so a row a write
    /// stored since then selects nothing. The answer names those rows in
    /// [`PatternCandidates::unindexed`], merged into the selection when they fit the rows
    /// `rows_max` leaves, and counted for the caller to report when they do not.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a stored row fails to decode, or on storage
    /// failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the transaction is read-only.
    pub async fn pattern_candidates(
        &self,
        tree_revision: &str,
        prefilter: &Prefilter,
        line_bound: bool,
        rows_max: u32,
    ) -> Result<RevisionScoped<PatternCandidates>, RiftError> {
        let mut connection = self.database.connection().await?;
        let mut transaction = connection
            .transaction()
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        let stored = stored_stamp(&mut transaction).await?;
        if let Some(scoped) = stamp_scope(stored, tree_revision) {
            return Ok(scoped);
        }
        crate::trigram_store::candidates(&mut transaction, prefilter, line_bound, rows_max)
            .await
            .map(RevisionScoped::Matched)
    }

    /// Indexes the oldest file rows the trigram index lacks in one transaction, at most
    /// [`LexicalIndexLimits::transaction_units_max`] rows and
    /// [`LexicalIndexLimits::transaction_bytes_max`] bytes of their text, and answers how
    /// many it still lacks.
    ///
    /// The first row always goes, whatever its size, so every call that finds a row lacking
    /// indexes at least one and repeated calls reach zero. The transaction takes the write
    /// turn every lexical write takes, and the two bounds are what a write queued behind it
    /// waits on; readers are never blocked, since they read committed WAL snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on storage failure; the rows stay lacking.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before commit leaves the index and the lacking rows as they were.
    pub async fn index_trigrams(&self) -> Result<TrigramBatch, RiftError> {
        let rows_max = self.limits.transaction_units_max();
        let bytes_max = u64::try_from(self.limits.transaction_bytes_max()).unwrap_or(u64::MAX);
        rift_tracing::traced!(
            component = "lexical",
            operation = "lexical.trigrams",
            async move {
                let mut access = self.database.writing().await?;
                let mut transaction = access.transaction().await?;
                let batch =
                    crate::trigram_store::index_batch(&mut transaction, rows_max, bytes_max)
                        .await?;
                transaction
                    .commit()
                    .await
                    .map_err(|source| errors::index::lexical_storage().source(source).error())?;
                Ok(batch)
            }
        )
        .await
    }

    /// Runs the full-text ranking as one fusion input, best first.
    ///
    /// The input carries the identities in rank order together with the
    /// columns that placed each of them, which is what lets an answer report
    /// a documentation hit and a name hit apart.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a stored row fails to decode, or on
    /// storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the transaction is read-only.
    pub async fn rank(
        &self,
        tree_revision: &str,
        query: &ParsedQuery,
        phase: QueryPhase,
        limit: u32,
    ) -> Result<RevisionScoped<RankingInput>, RiftError> {
        Ok(
            match self.search(tree_revision, query, phase, limit).await? {
                RevisionScoped::Matched(ranking) => RevisionScoped::Matched(ranking.into_input()),
                RevisionScoped::OtherRevision(stored) => RevisionScoped::OtherRevision(stored),
                RevisionScoped::NoRevision => RevisionScoped::NoRevision,
            },
        )
    }

    /// Returns one document's indexed content by its identity, or `None` when
    /// no document with that identity holds any.
    ///
    /// The content is a file document's text, or the one chunk of a large file
    /// its row stores: the one field a reader excerpts from. A symbol document
    /// holds none, since its declaration's source is a range of that text.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn content(&self, identity: &DocumentIdentity) -> Result<Option<String>, RiftError> {
        let mut connection = self.configured_connection().await?;
        let record = LexicalDocumentRecord::filter_by_identity(identity.as_str())
            .first()
            .exec(&mut connection)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        Ok(record.and_then(|record| record.file_content))
    }

    /// Reads one document back by its stable identity, or `None` when this store
    /// does not hold it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a stored row fails to decode, or on storage
    /// failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn document(
        &self,
        identity: &DocumentIdentity,
    ) -> Result<Option<IndexDocument>, RiftError> {
        let mut connection = self.configured_connection().await?;
        let record = LexicalDocumentRecord::filter_by_identity(identity.as_str())
            .first()
            .exec(&mut connection)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        record.map(decode_document).transpose()
    }

    /// Returns the tree revision the stored rows answer for, or `None` while no
    /// publication has landed or a write too large for one transaction is under way.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn tree_revision(&self) -> Result<Option<String>, RiftError> {
        let mut connection = self.configured_connection().await?;
        Ok(stored_stamp(&mut connection)
            .await?
            .and_then(|stamp| stamp.tree_revision))
    }
}

/// One published snapshot of the project store, read through the shared contract.
///
/// [`LexicalSearchIndex::rank`] answers a revision-qualified result, because a caller that
/// captured a publication has to tell a store that moved on from one that never published.
/// The shared contract has no place for that distinction: it is the shape a reader holding
/// no local tree implements. Binding the store to one publication is what lets the two be
/// compared over one set of documents, and a store that has moved on answers nothing here.
#[derive(Debug)]
pub struct PublishedIndex<'a> {
    store: &'a LexicalSearchIndex,
    tree_revision: &'a str,
    analyzer_revision: String,
}

impl<'a> PublishedIndex<'a> {
    /// Reads `store` as the publication `tree_revision` names.
    #[must_use]
    pub fn new(
        store: &'a LexicalSearchIndex,
        tree_revision: &'a str,
        analyzer_revision: impl Into<String>,
    ) -> Self {
        Self {
            store,
            tree_revision,
            analyzer_revision: analyzer_revision.into(),
        }
    }
}

impl IndexReader for PublishedIndex<'_> {
    fn capabilities(&self) -> IndexCapabilities {
        IndexCapabilities::new(
            PublicationFormat::CURRENT,
            self.analyzer_revision.clone(),
            CorpusRevision::current(),
            // The corpus declares every column. Which of them one document filled is
            // the document's own answer, not the store's.
            FieldSet::all(),
            RankingInputSet::of(RankingInputKind::Lexical),
        )
    }

    fn rank<'a>(
        &'a self,
        request: RankRequest<'a>,
    ) -> ReaderFuture<'a, Result<RankingInput, RiftError>> {
        Box::pin(async move {
            if request.input() != RankingInputKind::Lexical {
                // Identifier matching reads the declarations the workspace index holds,
                // and vector similarity reads a corpus the search tier publishes. Neither
                // is this store's to answer.
                return Ok(RankingInput::unanswered(request.input()));
            }
            let bound = u32::try_from(request.bound()).unwrap_or(u32::MAX);
            match self
                .store
                .rank(self.tree_revision, request.query(), request.phase(), bound)
                .await
            {
                Ok(RevisionScoped::Matched(input)) => Ok(input),
                Ok(_) => Ok(RankingInput::unanswered(request.input())),
                Err(error) => error
                    .with(ErrorContext::new("operation", "lexical ranking"))
                    .fail(),
            }
        })
    }

    fn document<'a>(
        &'a self,
        identity: &'a DocumentIdentity,
    ) -> ReaderFuture<'a, Result<Option<IndexDocument>, RiftError>> {
        Box::pin(async move {
            self.store
                .document(identity)
                .await
                .map_err(|error| error.with(ErrorContext::new("operation", "lexical ranking")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        INDEX_MIGRATION_FILES, LexicalChange, LexicalDocumentRecord, LexicalFileRecord,
        LexicalIndexLimits, LexicalIndexStateRecord, LexicalMatch, LexicalRanking,
        LexicalSearchIndex, checked_byte_length, checked_byte_offset, decode_document,
        decode_lexical_match, decode_recorded, is_connection_unavailable, isolated_weights,
        lexical_search_column_types, lexical_search_sql, matched_fields, project_location,
        rank_weights, require_pragma_row, searchable_columns, validate_indexed_count,
        validate_lexical_units,
    };
    use crate::trigram_store::TRIGRAM_ROWS;
    use rift_core::{ProjectPath, SourceUnitId};
    use rift_error::{ErrorSlug, errors};
    use rift_ranking::{
        CORPUS_TOKENIZER, DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation,
        FieldSet, IndexDocument, RankingInputKind, SearchableField,
    };
    use toasty::stmt::Value;

    fn identity(value: &str) -> DocumentIdentity {
        DocumentIdentity::new(value).expect("fixture identity must be accepted")
    }

    fn fixture_path() -> ProjectPath {
        ProjectPath::new("docs/a.md").expect("fixture path must be valid")
    }

    fn ranked(value: &str) -> LexicalMatch {
        LexicalMatch::new(
            identity(value),
            fixture_path(),
            DocumentKind::TextFile,
            -1.0,
            FieldSet::of(SearchableField::FileContent),
        )
    }

    /// One unit filed under `path`, carrying `bytes` of file text.
    fn unit_under(path: &str, name: &str, bytes: usize) -> IndexDocument {
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::FileContent, "x".repeat(bytes));
        let digest = fields.digest();
        IndexDocument::new(
            identity(&format!("{path}#{name}")),
            DocumentLocation::Project(ProjectPath::new(path).expect("fixture path must be valid")),
            DocumentKind::TextFile,
            digest,
            fields,
        )
        .expect("fixture document must construct")
        .at_byte_offset(0)
    }

    fn fixture_change(paths: &[(&str, usize)], unit_bytes: usize) -> LexicalChange {
        let replaced = paths
            .iter()
            .map(|(path, _)| ProjectPath::new(*path).expect("fixture path must be valid"))
            .collect::<Vec<_>>();
        let inserted = paths
            .iter()
            .flat_map(|(path, units)| {
                (0..*units).map(move |index| unit_under(path, &format!("unit{index}"), unit_bytes))
            })
            .collect();
        let recorded = replaced
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    crate::FileDigest::of(path.as_str().as_bytes()),
                )
            })
            .collect();
        LexicalChange::new(replaced, inserted).with_recorded(recorded)
    }

    fn part_paths(parts: &[LexicalChange]) -> Vec<Vec<String>> {
        parts
            .iter()
            .map(|part| {
                part.replaced()
                    .iter()
                    .map(|path| path.as_str().to_owned())
                    .collect()
            })
            .collect()
    }

    /// One file document under `src/a.rs` whose text is `content`.
    fn file_document(name: &str, content: &str) -> IndexDocument {
        let path = ProjectPath::new("src/a.rs").expect("fixture path must be valid");
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::FileContent, content);
        let digest = fields.digest();
        IndexDocument::new(
            identity(&format!("src/a.rs#{name}")),
            DocumentLocation::Project(path),
            DocumentKind::TextFile,
            digest,
            fields,
        )
        .expect("fixture document must construct")
    }

    #[test]
    fn test_lexical_ranking_below_the_bound_is_not_truncated() {
        let ranking = LexicalRanking::from_probe(vec![ranked("a")], 2);
        assert_eq!(ranking.matches().len(), 1);
        assert_eq!(ranking.truncated_at(), None);
    }

    #[test]
    fn test_lexical_ranking_at_exactly_the_bound_is_not_truncated() {
        let ranking = LexicalRanking::from_probe(vec![ranked("a"), ranked("b")], 2);
        assert_eq!(ranking.matches().len(), 2);
        assert_eq!(ranking.truncated_at(), None);
    }

    #[test]
    fn test_lexical_ranking_one_past_the_bound_keeps_the_bound_and_names_it() {
        let probe = vec![ranked("a"), ranked("b"), ranked("c")];
        let ranking = LexicalRanking::from_probe(probe, 2);
        assert_eq!(
            ranking
                .matches()
                .iter()
                .map(|matched| matched.identity().as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(ranking.truncated_at(), Some(2));
    }

    #[test]
    fn test_a_ranking_becomes_one_fusion_input_in_rank_order() {
        let ranking = LexicalRanking::from_probe(vec![ranked("a"), ranked("b")], 8);
        let input = ranking.into_input();
        assert_eq!(input.kind(), RankingInputKind::Lexical);
        assert_eq!(
            input
                .order()
                .iter()
                .map(|entry| entry.identity().as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(
            input.order()[0]
                .fields()
                .holds(SearchableField::FileContent)
        );
    }

    #[test]
    fn test_a_match_reports_the_columns_that_placed_it() {
        let matched = ranked("a");
        assert_eq!(matched.path(), &fixture_path());
        assert_eq!(matched.kind(), DocumentKind::TextFile);
        assert!((matched.rank() + 1.0).abs() < 1e-12);
        assert!(matched.fields().holds(SearchableField::FileContent));
    }

    #[test]
    fn test_lexical_index_limits_default_accepts_documented_pool_and_timeout() {
        let limits = LexicalIndexLimits::default();
        assert_eq!(limits.units_max(), 1_000_000);
        assert_eq!(limits.pool_slots(), 4);
        assert_eq!(limits.busy_timeout_ms(), 5_000);
        assert_eq!(limits.matches_max(), 1_000);
        assert_eq!(
            u64::from(limits.unit_bytes_max()),
            rift_protocol::configuration::TEXT_CHUNK_BYTES_MAX
        );
    }

    #[test]
    fn test_accepted_units_max_keeps_the_accepted_range_and_answers_the_ceiling_past_it() {
        use rift_protocol::configuration::{LEXICAL_UNITS_MAX_MAX, LEXICAL_UNITS_MAX_MIN};

        assert_eq!(
            LexicalIndexLimits::accepted_units_max(LEXICAL_UNITS_MAX_MIN),
            1_000
        );
        assert_eq!(
            LexicalIndexLimits::accepted_units_max(LEXICAL_UNITS_MAX_MAX),
            50_000_000
        );
        assert_eq!(LexicalIndexLimits::accepted_units_max(u64::MAX), u32::MAX);
    }

    #[test]
    fn test_a_content_field_at_the_configured_bound_passes() {
        let document = file_document("a", "12345678");
        let limits = LexicalIndexLimits::new(100, 8, 100, 4, 1_000);
        validate_indexed_count(1, limits).expect("one unit must fit configured count");
        validate_lexical_units(&[document], limits)
            .expect("content at the exact bound must not refuse");
    }

    #[test]
    fn test_a_content_field_over_the_configured_bound_refuses_naming_the_column() {
        let document = file_document("a", "way too much body content");
        let limits = LexicalIndexLimits::new(100, 8, 100, 4, 1_000);
        validate_indexed_count(1, limits).expect("one unit must fit configured count");
        let error = validate_lexical_units(&[document], limits)
            .expect_err("content one over the bound must refuse");
        assert_eq!(
            error.slug(),
            ErrorSlug::new("rift.index.lexical_unit_too_large")
        );
        let field = error.context().find(|(key, _)| *key == "field");
        assert_eq!(field, Some(("field", "file_content".to_owned())));
        assert_eq!(
            error.context().find(|(key, _)| *key == "path"),
            Some(("path", "src/a.rs".to_owned()))
        );
    }

    #[test]
    fn test_a_batch_repeating_one_identity_refuses() {
        let documents = vec![file_document("a", "body"), file_document("a", "other")];
        validate_indexed_count(documents.len(), LexicalIndexLimits::default())
            .expect("two units must fit default count");
        let error = validate_lexical_units(&documents, LexicalIndexLimits::default())
            .expect_err("a repeated identity must refuse");
        assert_eq!(
            error.slug(),
            ErrorSlug::new("rift.index.lexical_duplicate_identity")
        );
    }

    #[test]
    fn test_a_package_document_is_refused_by_the_project_store() {
        let unit = SourceUnitId::parse("rift://source/cargo/helper@0.1.0/src/lib.rs")
            .expect("fixture unit must parse");
        let fields = DocumentFields::empty().with(SearchableField::Name, "helper");
        let digest = fields.digest();
        let document = IndexDocument::new(
            identity("rift://source/cargo/helper@0.1.0/src/lib.rs"),
            DocumentLocation::Unit(unit),
            DocumentKind::Symbol,
            digest,
            fields,
        )
        .expect("fixture document must construct");
        let error = project_location(&document).expect_err("a unit location must refuse");
        assert_eq!(
            error.slug(),
            ErrorSlug::new("rift.index.lexical_document_location_unsupported")
        );
    }

    #[test]
    fn test_unit_limit_exposes_typed_limit_evidence() {
        let documents = [file_document("a", "body"), file_document("b", "body")];
        let limits = LexicalIndexLimits::new(1, 1_048_576, 1_000, 4, 1_000);
        let error = validate_indexed_count(documents.len(), limits)
            .expect_err("a batch over units_max must refuse");
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("field", "units_max".to_owned())));
        assert!(context.contains(&("observed", "2".to_owned())));
        assert!(context.contains(&("maximum", "1".to_owned())));
    }

    #[test]
    fn test_registry_storage_error_preserves_underlying_source() {
        let error = errors::index::lexical_storage()
            .source(std::io::Error::other("disk unavailable"))
            .error();
        let source = std::error::Error::source(&error).expect("storage error exposes source");
        assert_eq!(source.to_string(), "disk unavailable");
    }

    #[test]
    fn connection_unavailable_requires_toasty_pool_error() {
        let path = std::path::Path::new(".rift/index");
        let pooled = crate::DatabaseName::Index.failed(
            path,
            toasty::Error::connection_pool(std::io::Error::other("pool exhausted")),
        );
        assert!(is_connection_unavailable(&pooled));

        let storage =
            crate::DatabaseName::Index.failed(path, std::io::Error::other("disk unavailable"));
        assert!(!is_connection_unavailable(&storage));

        let lexical = errors::index::lexical_storage()
            .source(toasty::Error::connection_pool(std::io::Error::other(
                "pool exhausted",
            )))
            .error();
        assert!(
            !is_connection_unavailable(&lexical),
            "a pool checkout names its database; lexical code raises no pool refusal"
        );
    }

    #[test]
    fn test_duplicate_identity_uses_registered_slug_without_source() {
        let error = errors::index::lexical_duplicate_identity().error();
        assert_eq!(
            error.slug().as_str(),
            "rift.index.lexical_duplicate_identity"
        );
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn test_the_corpus_migration_declares_the_columns_in_the_order_bm25_weighs_them() {
        let sql = index_statement("CREATE VIRTUAL TABLE lexical_documents_fts ");
        // `bm25`'s weights are positional, and the weight list is generated from
        // `SearchableField::ALL` while this declaration is written by hand. Asserting
        // that each name appears somewhere would pass on a reordered declaration,
        // because the typed table names the same columns in the same string.
        let declared = sql
            .split_once("USING fts5(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(columns, _)| columns.to_owned())
            .expect("the migration must declare one FTS5 virtual table");
        let expected = SearchableField::ALL
            .map(|field| field.column().to_owned())
            .join(", ");
        assert!(
            declared.starts_with(&expected),
            "the FTS columns must be declared in the order the weights are rendered:\n\
             declared {declared}\nexpected {expected}"
        );
        assert_eq!(searchable_columns(), expected);
        assert!(
            declared.contains("content='lexical_documents'")
                && declared.contains("content_rowid='id'"),
            "the FTS table must index the typed table's rows by their id: {declared}"
        );
        assert!(
            declared.contains(CORPUS_TOKENIZER),
            "the FTS table must declare the corpus tokenizer: {CORPUS_TOKENIZER}"
        );
    }

    #[test]
    fn test_the_typed_table_holds_every_column_the_index_reads_by_name() {
        let sql = index_statement("CREATE TABLE lexical_documents(");
        let typed = sql
            .split_once("CREATE TABLE lexical_documents(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(columns, _)| columns.to_owned())
            .expect("the migration must declare the typed table");
        // An external-content index reads `SELECT <content_rowid>, <cols> FROM <content>`
        // with the FTS column names, so a searchable column the typed table lacks fails
        // every read of the index rather than one insert.
        for field in SearchableField::ALL {
            assert!(
                typed.contains(&format!(" {} TEXT", field.column())),
                "the typed table must declare {}: {typed}",
                field.column()
            );
        }
        assert!(typed.contains("id INTEGER PRIMARY KEY"));
        assert!(
            typed.contains("byte_offset BIGINT"),
            "a row records where its text starts in its file: {typed}"
        );
        assert!(
            !typed.contains("declaration_source"),
            "no row stores a declaration's source beside its file's text: {typed}"
        );
    }

    /// The index migration's one statement that starts with `prefix`, as one line.
    fn index_statement(prefix: &str) -> String {
        let [migration] = INDEX_MIGRATION_FILES else {
            panic!("the index database has one migration");
        };
        let statements: Vec<&str> = migration
            .sql()
            .split("-- #[toasty::breakpoint]")
            .map(str::trim)
            .filter(|statement| statement.starts_with(prefix))
            .collect();
        let [statement] = statements.as_slice() else {
            panic!("one statement must start with {prefix}: {statements:?}");
        };
        statement.replace('\n', " ")
    }

    /// The trigram index is declared over the file text alone, under the tokenizer the
    /// prefilter's trigram rule ports, and the file-row index selects exactly the rows
    /// every write indexes.
    #[test]
    fn test_the_trigram_index_holds_the_file_rows_under_the_ported_tokenizer() {
        let sql = index_statement("CREATE VIRTUAL TABLE lexical_documents_trigram ");
        let declared = sql
            .split_once("USING fts5(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(columns, _)| columns.to_owned())
            .expect("the migration must declare one FTS5 virtual table");
        for clause in [
            "file_content,".to_owned(),
            "content='lexical_documents'".to_owned(),
            "content_rowid='id'".to_owned(),
            format!("tokenize='{}'", rift_ranking::TRIGRAM_TOKENIZER),
            "detail='none'".to_owned(),
            "columnsize=0".to_owned(),
        ] {
            assert!(declared.contains(&clause), "{clause} in {declared}");
        }
        let file_rows = index_statement("CREATE INDEX lexical_documents_file_rows ");
        assert!(
            file_rows.ends_with(&format!("WHERE {TRIGRAM_ROWS}")),
            "the file-row index selects the rows every write indexes: {file_rows}"
        );
    }

    /// The rows the trigram index lacks are filed by id alone, so a row id is all the
    /// pending set records and the typed row keeps the only copy of its text.
    #[test]
    fn test_the_pending_table_files_rows_by_id_alone() {
        assert_eq!(
            index_statement("CREATE TABLE lexical_trigram_pending("),
            "CREATE TABLE lexical_trigram_pending(id INTEGER PRIMARY KEY)"
        );
    }

    #[test]
    fn test_the_rank_weights_follow_the_declared_field_order() {
        let rendered = rank_weights();
        let expected = SearchableField::ALL
            .map(|field| field.rank_weight().to_string())
            .join(", ");
        assert_eq!(rendered, expected);
    }

    #[test]
    fn test_isolated_weights_raise_exactly_one_column() {
        let rendered = isolated_weights(SearchableField::Documentation);
        assert_eq!(rendered, "0, 0, 0, 0, 1, 0");
    }

    #[test]
    fn test_the_ranking_query_declares_one_type_per_selected_column() {
        let sql = lexical_search_sql();
        let aliased = sql.matches(" AS ").count();
        assert_eq!(
            aliased,
            SearchableField::ALL.len() + 1,
            "one alias ranks the row and one more isolates each column"
        );
        // Five plain columns precede the aliases: identity, path, kind, and the bytes of
        // its file the row holds.
        assert_eq!(lexical_search_column_types().len(), aliased + 5);
        for field in SearchableField::ALL {
            assert!(
                sql.contains(&format!("AS {}_hit", field.column())),
                "the statement must isolate {}",
                field.column()
            );
        }
    }

    #[test]
    fn test_matched_fields_reads_a_non_zero_isolated_score_as_a_hit() {
        let isolated: Vec<Value> = SearchableField::ALL
            .into_iter()
            .map(|field| {
                Value::F64(if field == SearchableField::Name {
                    -1.5
                } else {
                    0.0
                })
            })
            .collect();
        let fields = matched_fields(&isolated);
        assert!(fields.holds(SearchableField::Name));
        assert_eq!(fields.fields().count(), 1);
    }

    #[test]
    fn test_require_pragma_row_mismatch_refuses_naming_observed_and_expected() {
        let rows = [Value::record_from_vec(vec![Value::String(
            "delete".to_owned(),
        )])];
        let expected = [Value::String("wal".to_owned())];
        let error =
            require_pragma_row(&rows, &expected).expect_err("mismatched pragma row must refuse");
        assert_eq!(error.slug().as_str(), "rift.index.lexical_storage");
        let pragma = error
            .context()
            .find(|(key, _)| *key == "pragma")
            .map(|(_, value)| value);
        assert!(
            pragma.is_some_and(|value| value.contains("unexpected pragma row")),
            "pragma mismatch must name the observed and expected rows"
        );
    }

    #[test]
    #[should_panic(expected = "content byte length must fit i64 once bounded by unit_bytes_max")]
    fn test_checked_byte_length_usize_max_panics_on_i64_overflow() {
        let _ = checked_byte_length(usize::MAX);
    }

    #[test]
    #[should_panic(expected = "a file offset must fit i64 once bounded by file_bytes_max")]
    fn test_checked_byte_offset_u64_max_panics_on_i64_overflow() {
        let _ = checked_byte_offset(u64::MAX);
    }

    /// A typed row holding one file chunk: `symbol_name` and `byte_offset` are what the
    /// decoding cases vary.
    fn chunk_record(symbol_name: String, byte_offset: i64) -> LexicalDocumentRecord {
        LexicalDocumentRecord {
            identity: "docs/a.md#1".to_owned(),
            path: "docs/a.md".to_owned(),
            kind: "text_file".to_owned(),
            digest: "0f1e2d3c".to_owned(),
            byte_length: 5,
            byte_offset: Some(byte_offset),
            name: Some(symbol_name),
            qualified_name: None,
            identifier_terms: None,
            signature: None,
            documentation: None,
            file_content: Some("chunk".to_owned()),
        }
    }

    #[test]
    fn test_a_stored_row_decodes_at_the_offset_it_was_written_at() {
        let document =
            decode_document(chunk_record("a".to_owned(), 64)).expect("a written row decodes");
        assert_eq!(document.byte_offset(), Some(64));
    }

    #[test]
    fn test_a_stored_row_with_a_negative_offset_refuses_naming_the_offset() {
        let error = decode_document(chunk_record("a".to_owned(), -1))
            .expect_err("no write stores a negative offset");
        assert_eq!(error.slug().as_str(), "rift.index.lexical_storage");
        let offset = error
            .context()
            .find(|(key, _)| *key == "byte offset")
            .map(|(_, value)| value);
        assert_eq!(
            offset.as_deref(),
            Some("a stored row offset is negative: offset=-1")
        );
    }

    #[test]
    fn test_a_stored_row_with_a_field_past_its_bound_refuses() {
        let overlong = "n".repeat(rift_ranking::NAME_BYTES_MAX + 1);
        let error = decode_document(chunk_record(overlong, 0))
            .expect_err("a name past its bound is no document");
        assert_eq!(
            error.slug().as_str(),
            "rift.index.lexical_stored_kind_invalid"
        );
    }

    #[test]
    fn test_lexical_document_record_debug_formats_declared_fields() {
        let record = LexicalDocumentRecord {
            identity: "crate::a".to_owned(),
            path: "src/a.rs".to_owned(),
            kind: "symbol".to_owned(),
            digest: "0f1e2d3c".to_owned(),
            byte_length: 4,
            byte_offset: None,
            name: Some("a".to_owned()),
            qualified_name: Some("crate::a".to_owned()),
            identifier_terms: None,
            signature: Some("fn a()".to_owned()),
            documentation: None,
            file_content: None,
        };
        let formatted = format!("{record:?}");
        assert!(formatted.contains("crate::a"));
        assert!(formatted.contains("src/a.rs"));
        assert!(formatted.contains("0f1e2d3c"));
    }

    #[test]
    fn test_lexical_index_state_record_debug_formats_declared_fields() {
        let record = LexicalIndexStateRecord {
            id: 1,
            tree_revision: Some("deadbeef".to_owned()),
            corpus_revision: "0f1e2d3c".to_owned(),
            derivation_revision: "a1b2c3d4".to_owned(),
        };
        let formatted = format!("{record:?}");
        assert!(formatted.contains("deadbeef"));
        assert!(formatted.contains("0f1e2d3c"));
        assert!(formatted.contains("a1b2c3d4"));
    }

    #[test]
    fn test_an_empty_change_splits_into_one_empty_part() {
        let parts = LexicalChange::default().into_parts_within(10, 1_000);
        assert_eq!(parts, vec![LexicalChange::default()]);
    }

    #[test]
    fn test_a_change_within_both_bounds_stays_one_part() {
        let change = fixture_change(&[("a.rs", 2), ("b.rs", 3)], 10);
        let parts = change.clone().into_parts_within(5, 50);
        assert_eq!(parts, vec![change]);
    }

    #[test]
    fn test_a_split_keeps_every_path_whole_and_starts_a_part_at_the_unit_bound() {
        let change = fixture_change(&[("a.rs", 2), ("b.rs", 3), ("c.rs", 1)], 10);
        let parts = change.into_parts_within(4, 1_000);
        assert_eq!(part_paths(&parts), vec![vec!["a.rs"], vec!["b.rs", "c.rs"]]);
        for part in &parts {
            for unit in part.inserted() {
                let DocumentLocation::Project(path) = unit.location() else {
                    panic!("fixture units are project documents");
                };
                assert!(
                    part.replaced().contains(path),
                    "{path} left its path's part"
                );
            }
            let recorded: Vec<_> = part
                .recorded()
                .iter()
                .map(|(path, _)| path.clone())
                .collect();
            assert_eq!(
                recorded,
                part.replaced().to_vec(),
                "a digest travels with its path"
            );
        }
    }

    #[test]
    fn test_a_path_past_the_bound_takes_a_part_of_its_own() {
        let change = fixture_change(&[("a.rs", 1), ("big.json", 7), ("c.rs", 1)], 10);
        let parts = change.into_parts_within(3, 1_000);
        assert_eq!(
            part_paths(&parts),
            vec![vec!["a.rs"], vec!["big.json"], vec!["c.rs"]]
        );
        assert_eq!(parts[1].inserted().len(), 7);
    }

    #[test]
    fn test_a_split_preserves_replacement_order_and_empty_deletions() {
        let change = fixture_change(&[("z.rs", 2), ("a.rs", 2), ("empty.rs", 0)], 10);
        let expected_units = change.inserted().to_vec();
        let expected_digests = change.recorded().to_vec();
        let parts = change.into_parts_within(2, 1_000);
        assert_eq!(
            part_paths(&parts),
            vec![vec!["z.rs"], vec!["a.rs", "empty.rs"]]
        );
        assert_eq!(
            parts
                .iter()
                .flat_map(LexicalChange::inserted)
                .collect::<Vec<_>>(),
            expected_units.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            parts
                .iter()
                .flat_map(LexicalChange::recorded)
                .collect::<Vec<_>>(),
            expected_digests.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_a_split_starts_a_part_at_the_byte_bound() {
        let change = fixture_change(&[("a.rs", 1), ("b.rs", 1), ("c.rs", 1)], 40);
        let parts = change.into_parts_within(100, 100);
        assert_eq!(part_paths(&parts), vec![vec!["a.rs", "b.rs"], vec!["c.rs"]]);
    }

    #[test]
    fn test_units_and_digests_under_no_replaced_path_join_the_last_part() {
        let stray_path = ProjectPath::new("stray.rs").expect("fixture path must be valid");
        let (replaced, mut inserted, mut recorded) =
            fixture_change(&[("a.rs", 2), ("b.rs", 2)], 10).into_parts();
        inserted.push(unit_under("stray.rs", "stray", 10));
        recorded.push((stray_path.clone(), crate::FileDigest::of(b"stray")));
        let parts = LexicalChange::new(replaced, inserted)
            .with_recorded(recorded)
            .into_parts_within(2, 1_000);
        let last = parts.last().expect("a split answers at least one part");
        assert!(
            last.inserted()
                .iter()
                .any(|unit| unit.location() == &DocumentLocation::Project(stray_path.clone()))
        );
        assert!(last.recorded().iter().any(|(path, _)| path == &stray_path));
    }

    #[test]
    fn test_a_stamp_names_its_publication_only_when_published() {
        let published = super::LexicalStamp::published("revision-one", "derivation-a");
        assert_eq!(published.tree_revision(), Some("revision-one"));
        assert_eq!(published.derivation_revision(), "derivation-a");
        let unpublished = super::LexicalStamp::unpublished("derivation-a");
        assert_eq!(
            unpublished.tree_revision(),
            None,
            "a part committed before the last names no publication"
        );
        assert_eq!(unpublished.derivation_revision(), "derivation-a");
    }

    #[tokio::test]
    async fn test_an_integer_query_answering_more_than_one_row_refuses_naming_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabaseName::Index,
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let mut connection = database.connection().await?;
        let error = super::single_i64(&mut connection, "SELECT 1 UNION ALL SELECT 2", "probe")
            .await
            .expect_err("two rows are not one integer");
        assert_eq!(error.slug().as_str(), "rift.index.lexical_storage");
        let (_, probe) = error
            .context()
            .find(|(key, _)| *key == "probe")
            .expect("the refusal names the query it ran");
        assert!(
            probe.starts_with("unexpected probe rows"),
            "the refusal carries the rows it read: {probe}"
        );
        Ok(())
    }

    #[test]
    fn test_a_recorded_digest_reads_back_as_the_digest_written() {
        let digest = crate::FileDigest::of(b"pub fn beacon() {}");
        let record = LexicalFileRecord {
            project_path: "src/lib.rs".to_owned(),
            digest: digest.as_bytes().to_vec(),
        };
        let (path, read) = decode_recorded(record).expect("a 32-byte digest decodes");
        assert_eq!(path.as_str(), "src/lib.rs");
        assert_eq!(read, digest);
    }

    #[test]
    fn test_a_recorded_digest_of_another_width_refuses_naming_both_widths() {
        let record = LexicalFileRecord {
            project_path: "src/lib.rs".to_owned(),
            digest: vec![1, 2, 3],
        };
        let error = decode_recorded(record).expect_err("a 3-byte digest is not a file digest");
        assert_eq!(error.slug().as_str(), "rift.index.lexical_storage");
        let rendered = error.to_string();
        assert!(rendered.contains("3-byte digest"), "{rendered}");
        assert!(rendered.contains("32 bytes"), "{rendered}");
    }

    #[test]
    fn test_a_recorded_path_that_is_not_a_project_path_refuses() {
        let record = LexicalFileRecord {
            project_path: "/absolute".to_owned(),
            digest: vec![0; 32],
        };
        let error = decode_recorded(record).expect_err("an absolute path is not a project path");
        assert_eq!(
            error.slug().as_str(),
            "rift.index.lexical_stored_path_invalid"
        );
    }

    #[test]
    #[should_panic(expected = "lexical search row must be a record")]
    fn test_decode_lexical_match_non_record_row_panics() {
        let row = Value::String("not-a-record".to_owned());
        let _ = decode_lexical_match(&row);
    }

    #[test]
    #[should_panic(expected = "lexical search row must match its declared column types")]
    fn test_decode_lexical_match_wrong_shaped_record_panics() {
        let row = Value::record_from_vec(vec![Value::String("only-one-field".to_owned())]);
        let _ = decode_lexical_match(&row);
    }

    #[test]
    fn test_lexical_search_index_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LexicalSearchIndex>();
    }
}
