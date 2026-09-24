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
//! rows; the authoritative `lexical_documents` and `lexical_index_state` tables are
//! ordinary Toasty models.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, ProjectPath, fault_label,
};
use rift_protocol::configuration::LEXICAL_UNITS_MAX_DEFAULT;
use rift_ranking::{
    CorpusRevision, DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, FieldSet,
    IndexCapabilities, IndexDocument, IndexReader, ParsedQuery, PublicationFormat, QueryPhase,
    RankRequest, RankedIdentity, RankingError, RankingFault, RankingInput, RankingInputKind,
    RankingInputSet, RankingViolation, ReaderFuture, SearchableField,
};
use serde::Serialize;
use std::sync::Arc;

use toasty::Executor;
use toasty::db::Connection;
use toasty::migration::{MigrationFile, MigrationSet};
use toasty::stmt::{Type, Value};

use crate::database::WorkspaceDatabase;

/// Default maximum content bytes accepted for one lexical document (1 MiB).
const LEXICAL_UNIT_BYTES_MAX_DEFAULT: u32 = 1_048_576;
/// Default maximum search results returned per query.
const LEXICAL_MATCHES_MAX_DEFAULT: u32 = 1_000;
/// Default pooled `SQLite` connection slots.
const LEXICAL_POOL_SLOTS_DEFAULT: u32 = 4;
/// Default busy-wait budget, in milliseconds, `SQLite` grants a connection
/// before returning `SQLITE_BUSY`.
const LEXICAL_BUSY_TIMEOUT_MS_DEFAULT: u32 = 5_000;
/// `bm25` column weight applied to the FTS `identity` column. `identity` is
/// `UNINDEXED` and never matches a term, but `bm25` still numbers every
/// declared column positionally, `identity` included, so this placeholder
/// keeps the searchable columns' weights aligned to their own positions.
const LEXICAL_IDENTITY_RANK_WEIGHT: f64 = 0.0;

/// Primary key of the single `lexical_index_state` row this adapter maintains.
const LEXICAL_INDEX_STATE_ID: i64 = 1;

const MIGRATION_FILES: &[MigrationFile] = &[
    MigrationFile::new(
        1,
        "lexical_schema",
        "CREATE TABLE lexical_units(
        identity TEXT PRIMARY KEY NOT NULL,
        path TEXT NOT NULL,
        kind TEXT NOT NULL,
        name TEXT,
        byte_length BIGINT NOT NULL,
        content TEXT NOT NULL
    )
-- #[toasty::breakpoint]
CREATE TABLE lexical_index_state(
        id BIGINT PRIMARY KEY NOT NULL,
        tree_revision TEXT NOT NULL
    )
-- #[toasty::breakpoint]
CREATE VIRTUAL TABLE lexical_units_fts USING fts5(identity UNINDEXED, name, content)",
    ),
    MigrationFile::new(
        2,
        "semantic_vectors",
        "CREATE TABLE semantic_vectors(
        identity TEXT PRIMARY KEY NOT NULL,
        model TEXT NOT NULL,
        digest TEXT NOT NULL,
        dimension BIGINT NOT NULL,
        vector BLOB NOT NULL
    )
-- #[toasty::breakpoint]
CREATE INDEX semantic_vectors_model ON semantic_vectors(model)",
    ),
    MigrationFile::new(
        3,
        "lexical_units_path",
        "CREATE INDEX lexical_units_path ON lexical_units(path)",
    ),
    MigrationFile::new(
        4,
        "log_records",
        "CREATE TABLE log_records(
        id BIGINT PRIMARY KEY NOT NULL,
        recorded_at BIGINT NOT NULL,
        level TEXT NOT NULL,
        target TEXT NOT NULL,
        component TEXT NOT NULL,
        operation TEXT NOT NULL,
        message TEXT NOT NULL,
        fields TEXT NOT NULL
    )
-- #[toasty::breakpoint]
CREATE INDEX log_records_level ON log_records(level)
-- #[toasty::breakpoint]
CREATE INDEX log_records_component ON log_records(component)",
    ),
    MigrationFile::new(
        5,
        "lexical_documents",
        "DROP TABLE lexical_units_fts
-- #[toasty::breakpoint]
DROP TABLE lexical_units
-- #[toasty::breakpoint]
CREATE TABLE lexical_documents(
        identity TEXT PRIMARY KEY NOT NULL,
        path TEXT NOT NULL,
        kind TEXT NOT NULL,
        digest TEXT NOT NULL,
        byte_length BIGINT NOT NULL,
        name TEXT,
        qualified_name TEXT,
        identifier_terms TEXT,
        signature TEXT,
        documentation TEXT,
        declaration_source TEXT,
        file_content TEXT
    )
-- #[toasty::breakpoint]
CREATE INDEX lexical_documents_path ON lexical_documents(path)
-- #[toasty::breakpoint]
CREATE VIRTUAL TABLE lexical_documents_fts USING fts5(identity UNINDEXED, name, \
qualified_name, identifier_terms, signature, documentation, declaration_source, \
file_content, tokenize='unicode61 remove_diacritics 0')
-- #[toasty::breakpoint]
ALTER TABLE lexical_index_state ADD COLUMN corpus_revision TEXT NOT NULL DEFAULT ''",
    ),
    MigrationFile::new(
        6,
        "documentation_metadata",
        "CREATE TABLE documentation_manifest(id BIGINT PRIMARY KEY NOT NULL, payload TEXT NOT NULL)
-- #[toasty::breakpoint]
CREATE TABLE documentation_references(identity TEXT PRIMARY KEY NOT NULL, target TEXT NOT NULL, block TEXT NOT NULL, position BIGINT NOT NULL)
-- #[toasty::breakpoint]
CREATE INDEX documentation_references_target ON documentation_references(target)",
    ),
];
pub(crate) const MIGRATIONS: MigrationSet = MigrationSet::new(MIGRATION_FILES);

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
                .map(|matched| RankedIdentity::new(matched.identity, matched.fields))
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
        }
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
}

impl LexicalIndexLimits {
    /// Constructs explicit lexical index bounds.
    #[must_use]
    pub const fn new(
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
        }
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
    /// Defaults accept the `[search.lexical] units_max` default of 1,000,000
    /// documents, 1 MiB per content field, 1,000 returned matches, 4 pooled
    /// connections, and a 5,000ms busy timeout.
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

/// Stable lexical indexing failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalIndexViolation {
    /// A `SQLite` or Toasty operation failed.
    Storage,
    /// A `replace_all` batch exceeded `units_max`.
    UnitLimit,
    /// One document's content field exceeded `unit_bytes_max`.
    UnitTooLarge,
    /// A stored row's path failed [`ProjectPath`] validation.
    StoredPathInvalid,
    /// A stored row's kind failed to parse as a known [`DocumentKind`].
    StoredKindInvalid,
    /// A write carried a document addressed by a source unit. This store
    /// holds project documents; a package document belongs to a package
    /// index.
    DocumentLocationUnsupported,
    /// A `replace_all` batch repeated one identity across documents.
    DuplicateIdentity,
    /// One write carried more records than the batch bound accepts.
    RecordLimit,
}

/// The bound and observed value behind a `limit_exceeded` violation, minted from the exact
/// numbers the caller already computed. The wire [`LimitEvidence`] and this fault's own
/// rendered `observed`/`maximum` context both derive from this one typed value, so they
/// cannot drift apart the way reparsing rendered text back into numbers could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LimitBreach {
    field: &'static str,
    observed: u64,
    maximum: u64,
}

impl LimitBreach {
    /// A breach minted from two in-memory counts, widened the way [`limit_count`] widens.
    pub(crate) fn from_counts(field: &'static str, observed: usize, maximum: usize) -> Self {
        Self {
            field,
            observed: limit_count(observed),
            maximum: limit_count(maximum),
        }
    }

    /// The rendered `field`, `observed`, and `maximum` context a limit fault carries.
    pub(crate) fn context(&self) -> [ErrorContext; 3] {
        [
            ErrorContext::new("field", self.field),
            ErrorContext::new("observed", self.observed.to_string()),
            ErrorContext::new("maximum", self.maximum.to_string()),
        ]
    }

    /// The wire evidence: the bound in force and what the request would have needed.
    pub(crate) fn evidence(&self) -> LimitEvidence {
        LimitEvidence {
            field: self.field.to_owned(),
            limit: self.maximum,
            required: self.observed,
        }
    }
}

/// One lexical indexing failure: its violation, the offending path when
/// known, the underlying cause, and - for a limit violation - the typed
/// bound it crossed.
#[derive(Debug)]
pub struct LexicalIndexFault {
    violation: LexicalIndexViolation,
    path: Option<PathBuf>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    limit: Option<LimitBreach>,
}

impl LexicalIndexFault {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> LexicalIndexViolation {
        self.violation
    }

    /// Returns involved path when available.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Fault for LexicalIndexFault {
    fn name(&self) -> ErrorName {
        match self.violation {
            LexicalIndexViolation::Storage => ErrorName::Wire(ErrorCode::StorageFailure),
            LexicalIndexViolation::UnitLimit
            | LexicalIndexViolation::UnitTooLarge
            | LexicalIndexViolation::RecordLimit => ErrorName::Wire(ErrorCode::LimitExceeded),
            LexicalIndexViolation::StoredPathInvalid
            | LexicalIndexViolation::StoredKindInvalid
            | LexicalIndexViolation::DocumentLocationUnsupported
            | LexicalIndexViolation::DuplicateIdentity => ErrorName::Wire(ErrorCode::InternalError),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.display().to_string()));
        }
        if let Some(breach) = &self.limit {
            context.extend(breach.context());
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.limit.as_ref().map(LimitBreach::evidence)
    }
}

/// Opaque lexical indexing failure.
pub type LexicalIndexError = Error<LexicalIndexFault>;

fn lexical_error(violation: LexicalIndexViolation) -> LexicalIndexError {
    Error::new(LexicalIndexFault {
        violation,
        path: None,
        source: None,
        limit: None,
    })
}

pub(crate) fn lexical_error_caused_by(
    violation: LexicalIndexViolation,
    path: Option<&Path>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> LexicalIndexError {
    Error::new(LexicalIndexFault {
        violation,
        path: path.map(Path::to_path_buf),
        source: Some(Box::new(source)),
        limit: None,
    })
}

/// Refuses a `limit_exceeded`-classified violation, carrying the bound and the value that
/// crossed it as one typed [`LimitBreach`] the wire [`LimitEvidence`] and the rendered
/// `observed`/`maximum` context both derive from.
fn lexical_error_over_limit(
    violation: LexicalIndexViolation,
    path: Option<&Path>,
    field: &'static str,
    observed: u64,
    maximum: u64,
) -> LexicalIndexError {
    Error::new(LexicalIndexFault {
        violation,
        path: path.map(Path::to_path_buf),
        source: None,
        limit: Some(LimitBreach {
            field,
            observed,
            maximum,
        }),
    })
}

/// Widens a bounded `usize` count into the `u64` domain [`LimitBreach`] and the wire
/// `LimitEvidence` share; every count this module bounds already fits comfortably, so the
/// fallback only guards the conversion, never fires in practice.
fn limit_count(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Refuses one oversized write batch, naming the field the bound belongs to.
pub(crate) fn batch_limit_error(
    field: &'static str,
    observed: u64,
    maximum: u64,
) -> LexicalIndexError {
    lexical_error_over_limit(
        LexicalIndexViolation::RecordLimit,
        None,
        field,
        observed,
        maximum,
    )
}

/// Maps one Toasty operating failure onto [`LexicalIndexViolation::Storage`].
pub(crate) fn storage_error(source: toasty::Error) -> LexicalIndexError {
    lexical_error_caused_by(LexicalIndexViolation::Storage, None, source)
}

/// Widens a `u32` domain bound into `usize` for in-memory comparisons; `u32`
/// always fits `usize` on every platform Rift targets.
pub(crate) fn bound_as_usize(bound: u32) -> usize {
    usize::try_from(bound).unwrap_or_else(|_| {
        unreachable!("u32 bound must fit usize on supported platforms: bound={bound}")
    })
}

/// Refuses one unit's out-of-bound `name` or `content` field, naming which
/// field and the offending path.
fn oversized_field_error(
    path: &ProjectPath,
    field: &'static str,
    observed: usize,
    maximum: usize,
) -> LexicalIndexError {
    lexical_error_over_limit(
        LexicalIndexViolation::UnitTooLarge,
        Some(Path::new(path.as_str())),
        field,
        limit_count(observed),
        limit_count(maximum),
    )
    .with_context(ErrorContext::new("field", field))
}

/// Refuses a `replace_all` batch that violates a configured bound, before any
/// database work begins. Bounds apply to the batch size and to each
/// document's content field (`unit_bytes_max`).
fn validate_lexical_batch(
    documents: &[IndexDocument],
    limits: LexicalIndexLimits,
) -> Result<(), LexicalIndexError> {
    validate_indexed_count(documents.len(), limits)?;
    validate_lexical_units(documents, limits)
}

/// Refuses an indexed set larger than `units_max`.
///
/// A whole replacement knows its resulting size before it opens a transaction. An
/// incremental apply knows it only after its deletions have run, so it counts the table
/// inside the transaction and checks the same bound here.
fn validate_indexed_count(
    units: usize,
    limits: LexicalIndexLimits,
) -> Result<(), LexicalIndexError> {
    if units > bound_as_usize(limits.units_max()) {
        return Err(lexical_error_over_limit(
            LexicalIndexViolation::UnitLimit,
            None,
            "units_max",
            limit_count(units),
            u64::from(limits.units_max()),
        ));
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
) -> Result<(), LexicalIndexError> {
    let mut identities_seen = std::collections::HashSet::with_capacity(documents.len());
    for document in documents {
        let path = project_location(document)?;
        let field = document.kind().content_field();
        let observed = document.content().len();
        if observed > bound_as_usize(limits.unit_bytes_max()) {
            return Err(oversized_field_error(
                path,
                field.column(),
                observed,
                bound_as_usize(limits.unit_bytes_max()),
            ));
        }
        if !identities_seen.insert(document.identity()) {
            return Err(
                lexical_error(LexicalIndexViolation::DuplicateIdentity).with_context(
                    ErrorContext::new("identity", document.identity().as_str().to_owned()),
                ),
            );
        }
    }
    Ok(())
}

/// The project path a document is addressed by, or the refusal a package
/// document earns from this store.
fn project_location(document: &IndexDocument) -> Result<&ProjectPath, LexicalIndexError> {
    match document.location() {
        DocumentLocation::Project(path) => Ok(path),
        DocumentLocation::Unit(unit) => Err(lexical_error(
            LexicalIndexViolation::DocumentLocationUnsupported,
        )
        .with_context(ErrorContext::new("unit", unit.to_string()))),
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
    name: Option<String>,
    qualified_name: Option<String>,
    identifier_terms: Option<String>,
    signature: Option<String>,
    documentation: Option<String>,
    declaration_source: Option<String>,
    file_content: Option<String>,
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_index_state"]
pub(crate) struct LexicalIndexStateRecord {
    #[key]
    id: i64,
    tree_revision: String,
    corpus_revision: String,
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

/// Verifies a single-row pragma result matches the value just requested.
pub(crate) fn require_pragma_row(
    rows: &[Value],
    expected: &[Value],
) -> Result<(), LexicalIndexError> {
    if let [Value::Record(record)] = rows
        && record.as_slice() == expected
    {
        return Ok(());
    }
    Err(
        lexical_error(LexicalIndexViolation::Storage).with_context(ErrorContext::new(
            "pragma",
            format!("unexpected pragma row: rows={rows:?}, expected={expected:?}"),
        )),
    )
}

/// Inserts one unit's typed row and its derived FTS row.
/// Deletes every unit filed under one path, from the typed table and the FTS index alike.
///
/// The FTS rows go first, while `lexical_documents` still holds the identities that name
/// them:
/// the virtual table carries no path column of its own.
async fn delete_path_units(
    executor: &mut dyn Executor,
    path: &ProjectPath,
) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(
        "DELETE FROM lexical_documents_fts WHERE identity IN \
         (SELECT identity FROM lexical_documents WHERE path = ?1)",
    )
    .bind(path.as_str().to_owned())
    .exec(executor)
    .await
    .map_err(storage_error)?;
    toasty::sql::statement("DELETE FROM lexical_documents WHERE path = ?1")
        .bind(path.as_str().to_owned())
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// What one publication stamped the store with.
///
/// The two revisions travel together because a read needs both: the tree
/// revision says which publication the rows belong to, and the corpus
/// revision says whether this build can read them at all.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredStamp {
    tree_revision: String,
    corpus_revision: CorpusRevision,
}

/// The stamp the store carries, read through `executor` so a caller can place
/// it in the same transaction as the query it qualifies.
async fn stored_stamp(
    executor: &mut dyn Executor,
) -> Result<Option<StoredStamp>, LexicalIndexError> {
    let record = LexicalIndexStateRecord::filter_by_id(LEXICAL_INDEX_STATE_ID)
        .first()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(record.map(|record| StoredStamp {
        tree_revision: record.tree_revision,
        corpus_revision: CorpusRevision::stored(record.corpus_revision),
    }))
}

/// Stamps the store with `tree_revision` and the corpus revision this build
/// derives, so a later read can tell both which publication the rows belong
/// to and whether it can read them at all.
async fn stamp(executor: &mut dyn Executor, tree_revision: &str) -> Result<(), LexicalIndexError> {
    LexicalIndexStateRecord::upsert_by_id(LEXICAL_INDEX_STATE_ID)
        .tree_revision(tree_revision.to_owned())
        .corpus_revision(CorpusRevision::current().as_str().to_owned())
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// How many documents the typed table holds right now.
async fn indexed_unit_count(executor: &mut dyn Executor) -> Result<usize, LexicalIndexError> {
    let rows = toasty::sql::query("SELECT count(*) FROM lexical_documents")
        .column_types([Type::I64])
        .exec(executor)
        .await
        .map_err(storage_error)?;
    let [Value::Record(record)] = rows.as_slice() else {
        return Err(
            lexical_error(LexicalIndexViolation::Storage).with_context(ErrorContext::new(
                "count",
                format!("unexpected unit-count rows: rows={rows:?}"),
            )),
        );
    };
    let [Value::I64(counted)] = record.as_slice() else {
        return Err(
            lexical_error(LexicalIndexViolation::Storage).with_context(ErrorContext::new(
                "count",
                format!("unexpected unit-count row: row={record:?}"),
            )),
        );
    };
    Ok(usize::try_from(*counted).unwrap_or(usize::MAX))
}

/// Writes one document's typed row and its FTS row.
///
/// The two carry the same fields, in the same order, because the typed
/// row is what a read resolves through and the FTS row is what a term
/// matches: a column filled in one and absent from the other would rank a
/// document the reader cannot then describe.
async fn insert_document(
    executor: &mut dyn Executor,
    document: &IndexDocument,
    path: &ProjectPath,
) -> Result<(), LexicalIndexError> {
    let fields = document.fields();
    let field = |searchable: SearchableField| fields.get(searchable).map(str::to_owned);
    let content = document.content();
    toasty::create!(LexicalDocumentRecord {
        identity: document.identity().as_str().to_owned(),
        path: path.as_str().to_owned(),
        kind: document.kind().stored_value().to_owned(),
        digest: document.digest().to_owned(),
        byte_length: checked_byte_length(content.len()),
        name: field(SearchableField::Name),
        qualified_name: field(SearchableField::QualifiedName),
        identifier_terms: field(SearchableField::IdentifierTerms),
        signature: field(SearchableField::Signature),
        documentation: field(SearchableField::Documentation),
        declaration_source: field(SearchableField::DeclarationSource),
        file_content: field(SearchableField::FileContent),
    })
    .exec(executor)
    .await
    .map_err(storage_error)?;

    let mut insert =
        toasty::sql::statement(fts_insert_sql()).bind(document.identity().as_str().to_owned());
    for searchable in SearchableField::ALL {
        insert = insert.bind(fields.get(searchable).unwrap_or_default().to_owned());
    }
    insert.exec(executor).await.map_err(storage_error)?;
    Ok(())
}

/// The FTS insert, with one placeholder for `identity` and one per searchable
/// field, in the column order [`SearchableField::ALL`] declares.
fn fts_insert_sql() -> String {
    let columns = SearchableField::ALL.map(SearchableField::column).join(", ");
    let placeholders = (1..=SearchableField::ALL.len() + 1)
        .map(|position| format!("?{position}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO lexical_documents_fts(identity, {columns}) VALUES ({placeholders})")
}

/// Rebuilds one published document from its typed row.
///
/// The row carries the same fields the document was written from, so this is the
/// inverse of [`insert_document`] and nothing is derived a second time.
fn decode_document(record: LexicalDocumentRecord) -> Result<IndexDocument, LexicalIndexError> {
    let path = ProjectPath::new(record.path).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredPathInvalid, None, source)
    })?;
    let kind = DocumentKind::from_stored(&record.kind).ok_or_else(|| {
        lexical_error(LexicalIndexViolation::StoredKindInvalid)
            .with_context(ErrorContext::new("kind", record.kind.clone()))
    })?;
    let identity = DocumentIdentity::new(record.identity).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredKindInvalid, None, source)
    })?;
    let stored = [
        (SearchableField::Name, record.name),
        (SearchableField::QualifiedName, record.qualified_name),
        (SearchableField::IdentifierTerms, record.identifier_terms),
        (SearchableField::Signature, record.signature),
        (SearchableField::Documentation, record.documentation),
        (
            SearchableField::DeclarationSource,
            record.declaration_source,
        ),
        (SearchableField::FileContent, record.file_content),
    ];
    let fields = stored
        .into_iter()
        .fold(DocumentFields::empty(), |held, (field, value)| {
            held.with_optional(field, value)
        });
    IndexDocument::new(
        identity,
        DocumentLocation::Project(path),
        kind,
        record.digest,
        fields,
    )
    .map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredKindInvalid, None, source)
    })
}

/// Reconstructs one search hit from a raw joined row.
///
/// The row shape follows directly from this module's own `SELECT` and
/// declared `column_types`; a mismatch is a programmer invariant, not an
/// operating failure.
fn decode_lexical_match(row: &Value) -> Result<LexicalMatch, LexicalIndexError> {
    let Value::Record(record) = row else {
        unreachable!("lexical search row must be a record: row={row:?}");
    };
    let [
        Value::String(identity),
        Value::String(path),
        Value::String(kind),
        Value::F64(rank),
        isolated @ ..,
    ] = record.as_slice()
    else {
        unreachable!("lexical search row must match its declared column types: row={row:?}");
    };
    let path = ProjectPath::new(path.clone()).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredPathInvalid, None, source)
    })?;
    let kind = DocumentKind::from_stored(kind).ok_or_else(|| {
        lexical_error(LexicalIndexViolation::StoredKindInvalid)
            .with_context(ErrorContext::new("kind", kind.clone()))
    })?;
    let identity = DocumentIdentity::new(identity.clone()).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredKindInvalid, None, source)
    })?;
    Ok(LexicalMatch {
        identity,
        path,
        kind,
        rank: *rank,
        fields: matched_fields(isolated),
    })
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

/// The `bm25` weight arguments, in FTS column order: the `identity`
/// placeholder first, then one weight per searchable field.
///
/// The weights come from [`SearchableField`] rather than from constants here,
/// so the ranking this store runs and the ranking every other reader runs are
/// the same numbers. FTS5 takes them as literal arguments, never as bind
/// parameters, which is why they are rendered into the statement.
fn rank_weights() -> String {
    std::iter::once(LEXICAL_IDENTITY_RANK_WEIGHT)
        .chain(SearchableField::ALL.map(SearchableField::rank_weight))
        .map(|weight| weight.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `bm25` weight arguments that isolate one column: every weight zero
/// except this field's, set to one.
fn isolated_weights(field: SearchableField) -> String {
    std::iter::once(0.0_f64)
        .chain(SearchableField::ALL.map(|declared| f64::from(u8::from(declared == field))))
        .map(|weight| weight.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the joined ranking query.
///
/// Equal ranks order by identity, because `SQLite` would otherwise return them
/// in whatever order it read the rows and two readers over one publication
/// must answer one order.
///
/// One `bm25` call ranks the row and one more per column proves which columns
/// carried a member. FTS5 evaluates an auxiliary function per candidate row,
/// so the answer costs eight evaluations where it used to cost one; the
/// `matches_max` bound is what keeps that bounded, and what it buys is a
/// `matched_by` a reader can act on.
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
        "SELECT lexical_documents_fts.identity, lexical_documents.path, \
         lexical_documents.kind, \
         bm25(lexical_documents_fts, {ranked}) AS rank{isolated} \
         FROM lexical_documents_fts \
         JOIN lexical_documents \
         ON lexical_documents.identity = lexical_documents_fts.identity \
         WHERE lexical_documents_fts MATCH ?1 \
         ORDER BY rank, lexical_documents_fts.identity LIMIT ?2"
    )
}

/// The declared result types of [`lexical_search_sql`], in column order.
fn lexical_search_column_types() -> Vec<Type> {
    let mut types = vec![Type::String, Type::String, Type::String, Type::F64];
    types.extend(SearchableField::ALL.map(|_| Type::F64));
    types
}

/// What one change set does to the lexical index.
///
/// A rebuild writes what it read under each path it named and nothing else, so `replaced`
/// names every one of those paths and `inserted` carries the units the rebuilt index
/// derived for them. A path the rebuild read appears in both halves; a path it found gone
/// appears only in the first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LexicalChange {
    replaced: Vec<ProjectPath>,
    inserted: Vec<IndexDocument>,
}

impl LexicalChange {
    /// Builds one change from the paths whose units go and the units that replace them.
    #[must_use]
    pub fn new(replaced: Vec<ProjectPath>, inserted: Vec<IndexDocument>) -> Self {
        Self { replaced, inserted }
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

    /// Whether this change writes nothing, so the stored set already answers for the tree
    /// that produced it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replaced.is_empty() && self.inserted.is_empty()
    }

    /// The change as its two halves, for a caller that rebuilds it over a narrowed
    /// unit list.
    #[must_use]
    pub fn into_parts(self) -> (Vec<ProjectPath>, Vec<IndexDocument>) {
        (self.replaced, self.inserted)
    }
}

/// `SQLite` FTS5-backed lexical search index.
///
/// [`IndexDocument`] is the one shape every Rift index publishes, so a
/// project row, a package row, and an in-memory fixture carry the same fields
/// and the same identity spelling. This store holds project documents: a
/// document addressed by a source unit belongs to a package index, and the
/// write path refuses it rather than filing a unit URI in the path column.
#[derive(Debug)]
pub struct LexicalSearchIndex {
    database: Arc<WorkspaceDatabase>,
    limits: LexicalIndexLimits,
}

impl LexicalSearchIndex {
    /// Attaches the lexical tier to one already-open workspace database.
    ///
    /// The pool is shared with every other store in the file, because `SQLite`
    /// serializes writers per file: a second pool would only add a connection
    /// that loses the same write lock.
    #[must_use]
    pub fn attached(database: Arc<WorkspaceDatabase>, limits: LexicalIndexLimits) -> Self {
        Self { database, limits }
    }

    /// A pooled connection carrying the workspace database's required pragmas.
    async fn configured_connection(&self) -> Result<Connection, LexicalIndexError> {
        self.database.connection().await
    }

    /// Atomically replaces every indexed unit and stamps `tree_revision`.
    ///
    /// Refuses before opening any transaction when `units` exceeds
    /// `units_max`, when one unit's content exceeds `unit_bytes_max`, or when
    /// two units share one identity: the previously indexed state stays
    /// fully intact in every refusal case.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] on bound violations or storage failure.
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
    ) -> Result<(), LexicalIndexError> {
        self.replace_all_metadata(documents, tree_revision, None)
            .await
    }

    /// Replaces lexical documents and validated documentation metadata in one transaction.
    ///
    /// # Errors
    ///
    /// Refuses invalid document batches, metadata byte excess, or storage failure.
    pub async fn replace_all_with_documentation(
        &self,
        documents: &[IndexDocument],
        tree_revision: &str,
        documentation: &rift_analysis::documentation::DocumentationCollection,
    ) -> Result<(), LexicalIndexError> {
        self.replace_all_metadata(documents, tree_revision, Some(documentation))
            .await
    }

    async fn replace_all_metadata(
        &self,
        documents: &[IndexDocument],
        tree_revision: &str,
        documentation: Option<&rift_analysis::documentation::DocumentationCollection>,
    ) -> Result<(), LexicalIndexError> {
        rift_core::traced_async!(
            component = "lexical",
            operation = "lexical.commit",
            mode = "replace",
            {
                validate_lexical_batch(documents, self.limits)?;
                let metadata = documentation
                    .map(crate::documentation_store::EncodedDocumentation::new)
                    .transpose()?;

                let mut access = self.database.writing().await?;
                let mut transaction = access.transaction().await?;

                toasty::sql::statement("DELETE FROM lexical_documents_fts")
                    .exec(&mut transaction)
                    .await
                    .map_err(storage_error)?;
                LexicalDocumentRecord::all()
                    .delete()
                    .exec(&mut transaction)
                    .await
                    .map_err(storage_error)?;

                for document in documents {
                    insert_document(&mut transaction, document, project_location(document)?)
                        .await?;
                }

                crate::documentation_store::replace(&mut transaction, metadata.as_ref()).await?;
                stamp(&mut transaction, tree_revision).await?;

                transaction.commit().await.map_err(storage_error)
            }
        )
        .await
    }

    /// Applies one change set's documents and stamps `tree_revision`, in one transaction.
    ///
    /// The transaction deletes every unit filed under a dropped path, inserts the units the
    /// change derived, and stamps the revision. Deleting by path is what makes this
    /// incremental: a text file split into chunks files every chunk under its own path, so
    /// one delete reaches all of them.
    ///
    /// The resulting set is counted inside the transaction, after the deletions and before
    /// the inserts, because an incremental apply cannot know its own resulting size any
    /// earlier - and `units_max` binds the indexed set, not one batch.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when one unit breaks a field bound, when two units
    /// share one identity, when the resulting set would exceed `units_max`, or on storage
    /// failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation before commit leaves the previously indexed units and tree-revision
    /// stamp fully intact. Cancellation after commit has the same durable result as
    /// completion.
    pub async fn apply(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
    ) -> Result<(), LexicalIndexError> {
        self.apply_metadata(change, tree_revision, None).await
    }

    /// Applies lexical changes and replaces their documentation metadata atomically.
    ///
    /// # Errors
    ///
    /// Refuses invalid document batches, metadata byte excess, or storage failure.
    pub async fn apply_with_documentation(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
        documentation: &rift_analysis::documentation::DocumentationCollection,
    ) -> Result<(), LexicalIndexError> {
        self.apply_metadata(change, tree_revision, Some(documentation))
            .await
    }

    async fn apply_metadata(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
        documentation: Option<&rift_analysis::documentation::DocumentationCollection>,
    ) -> Result<(), LexicalIndexError> {
        rift_core::traced_async!(
            component = "lexical",
            operation = "lexical.commit",
            mode = "apply",
            {
                validate_lexical_units(change.inserted(), self.limits)?;
                let metadata = documentation
                    .map(crate::documentation_store::EncodedDocumentation::new)
                    .transpose()?;

                let mut access = self.database.writing().await?;
                let mut transaction = access.transaction().await?;

                for path in change.replaced() {
                    delete_path_units(&mut transaction, path).await?;
                }
                let stored = indexed_unit_count(&mut transaction).await?;
                validate_indexed_count(
                    stored.saturating_add(change.inserted().len()),
                    self.limits,
                )?;

                for document in change.inserted() {
                    insert_document(&mut transaction, document, project_location(document)?)
                        .await?;
                }

                crate::documentation_store::replace(&mut transaction, metadata.as_ref()).await?;
                stamp(&mut transaction, tree_revision).await?;

                transaction.commit().await.map_err(storage_error)
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
        LexicalIndexError,
    > {
        let mut connection = self.database.connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;
        match stored_stamp(&mut transaction).await? {
            None => return Ok(RevisionScoped::NoRevision),
            Some(stored) if stored.corpus_revision != CorpusRevision::current() => {
                return Ok(RevisionScoped::NoRevision);
            }
            Some(stored) if stored.tree_revision != tree_revision => {
                return Ok(RevisionScoped::OtherRevision(stored.tree_revision));
            }
            Some(_) => {}
        }
        crate::documentation_store::read(&mut transaction)
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
    /// Returns [`LexicalIndexError`] when `query` carries more distinct terms
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
    ) -> Result<RevisionScoped<LexicalRanking>, LexicalIndexError> {
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

        let mut connection = self.database.connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;
        let stored = stored_stamp(&mut transaction).await?;
        match stored {
            None => return Ok(RevisionScoped::NoRevision),
            Some(stored) if stored.corpus_revision != CorpusRevision::current() => {
                return Ok(RevisionScoped::NoRevision);
            }
            Some(stored) if stored.tree_revision != tree_revision => {
                return Ok(RevisionScoped::OtherRevision(stored.tree_revision));
            }
            Some(_) => {}
        }
        let Some(expression) = expression else {
            return Ok(RevisionScoped::Matched(LexicalRanking::from_probe(
                Vec::new(),
                bound,
            )));
        };
        let rows = toasty::sql::query(lexical_search_sql())
            .bind(expression)
            .bind(probe_limit)
            .column_types(lexical_search_column_types())
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;

        let matches = rows
            .iter()
            .map(decode_lexical_match)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RevisionScoped::Matched(LexicalRanking::from_probe(
            matches, bound,
        )))
    }

    /// Runs the full-text ranking as one fusion input, best first.
    ///
    /// The input carries the identities in rank order together with the
    /// columns that placed each of them, which is what lets an answer report
    /// a documentation hit and a name hit apart.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when a stored row fails to decode, or on
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
    ) -> Result<RevisionScoped<RankingInput>, LexicalIndexError> {
        Ok(
            match self.search(tree_revision, query, phase, limit).await? {
                RevisionScoped::Matched(ranking) => RevisionScoped::Matched(ranking.into_input()),
                RevisionScoped::OtherRevision(stored) => RevisionScoped::OtherRevision(stored),
                RevisionScoped::NoRevision => RevisionScoped::NoRevision,
            },
        )
    }

    /// Returns one document's indexed content by its identity, or `None` when
    /// no document with that identity is indexed.
    ///
    /// The content is the declaration source of a symbol document and the
    /// text of a file document: the one field a reader excerpts from.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn content(
        &self,
        identity: &DocumentIdentity,
    ) -> Result<Option<String>, LexicalIndexError> {
        let mut connection = self.configured_connection().await?;
        let record = LexicalDocumentRecord::filter_by_identity(identity.as_str())
            .first()
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(record.and_then(|record| record.declaration_source.or(record.file_content)))
    }

    /// Reads one document back by its stable identity, or `None` when this store
    /// does not hold it.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when a stored row fails to decode, or on storage
    /// failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn document(
        &self,
        identity: &DocumentIdentity,
    ) -> Result<Option<IndexDocument>, LexicalIndexError> {
        let mut connection = self.configured_connection().await?;
        let record = LexicalDocumentRecord::filter_by_identity(identity.as_str())
            .first()
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        record.map(decode_document).transpose()
    }

    /// Returns the tree revision stamped by the most recent `replace_all`,
    /// or `None` before the first successful `replace_all`.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn tree_revision(&self) -> Result<Option<String>, LexicalIndexError> {
        let mut connection = self.configured_connection().await?;
        Ok(stored_stamp(&mut connection)
            .await?
            .map(|stamp| stamp.tree_revision))
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
    ) -> ReaderFuture<'a, Result<RankingInput, RankingError>> {
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
                Err(error) => Err(reader_refused(error)),
            }
        })
    }

    fn document<'a>(
        &'a self,
        identity: &'a DocumentIdentity,
    ) -> ReaderFuture<'a, Result<Option<IndexDocument>, RankingError>> {
        Box::pin(async move { self.store.document(identity).await.map_err(reader_refused) })
    }
}

/// One store refusal, as the shared contract's own.
///
/// The contract is storage-independent, so it cannot restate this store's violation. What
/// it does carry is the failure itself, on the source chain, so a caller still reaches the
/// driver text and the classification the store gave it. Reporting a disk failure as a
/// capability mismatch would send that caller to compare two publications instead.
fn reader_refused(error: LexicalIndexError) -> RankingError {
    RankingError::new(RankingFault::new(RankingViolation::ReaderFailed).caused_by(error))
}

#[cfg(test)]
mod tests {
    use super::{
        LexicalDocumentRecord, LexicalIndexFault, LexicalIndexLimits, LexicalIndexStateRecord,
        LexicalIndexViolation, LexicalMatch, LexicalRanking, LexicalSearchIndex, MIGRATION_FILES,
        checked_byte_length, decode_lexical_match, fts_insert_sql, isolated_weights, lexical_error,
        lexical_error_caused_by, lexical_search_column_types, lexical_search_sql, matched_fields,
        project_location, rank_weights, require_pragma_row, validate_lexical_batch,
    };
    use rift_core::{ErrorCode, ErrorName, Fault, ProjectPath, SourceUnitId};
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

    fn symbol_document(name: &str, source: &str) -> IndexDocument {
        let path = ProjectPath::new("src/a.rs").expect("fixture path must be valid");
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::QualifiedName, format!("crate::{name}"))
            .with(SearchableField::DeclarationSource, source);
        let digest = fields.digest();
        IndexDocument::new(
            identity(&format!("crate::{name}")),
            DocumentLocation::Project(path),
            DocumentKind::Symbol,
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
        assert_eq!(limits.unit_bytes_max(), 1_048_576);
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
        let document = symbol_document("a", "12345678");
        let limits = LexicalIndexLimits::new(100, 8, 100, 4, 1_000);
        validate_lexical_batch(&[document], limits)
            .expect("content at the exact bound must not refuse");
    }

    #[test]
    fn test_a_content_field_over_the_configured_bound_refuses_naming_the_column() {
        let document = symbol_document("a", "way too much body content");
        let limits = LexicalIndexLimits::new(100, 8, 100, 4, 1_000);
        let error = validate_lexical_batch(&[document], limits)
            .expect_err("content one over the bound must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::UnitTooLarge
        );
        let context = error.context();
        let field = context
            .iter()
            .find(|entry| entry.key() == "field")
            .map(rift_core::ErrorContext::value);
        assert_eq!(field, Some("declaration_source"));
        assert_eq!(error.fault().path(), Some(std::path::Path::new("src/a.rs")));
    }

    #[test]
    fn test_a_batch_repeating_one_identity_refuses() {
        let documents = vec![symbol_document("a", "body"), symbol_document("a", "other")];
        let error = validate_lexical_batch(&documents, LexicalIndexLimits::default())
            .expect_err("a repeated identity must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::DuplicateIdentity
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
            error.fault().violation(),
            LexicalIndexViolation::DocumentLocationUnsupported
        );
    }

    #[test]
    fn test_unit_limit_exposes_typed_limit_evidence() {
        let documents = vec![symbol_document("a", "body"), symbol_document("b", "body")];
        let limits = LexicalIndexLimits::new(1, 1_048_576, 1_000, 4, 1_000);
        let error = validate_lexical_batch(&documents, limits)
            .expect_err("a batch over units_max must refuse");
        assert_eq!(
            error.fault().limit_evidence(),
            Some(rift_core::LimitEvidence {
                field: "units_max".to_owned(),
                limit: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn test_a_non_limit_violation_exposes_no_limit_evidence() {
        let error = lexical_error(LexicalIndexViolation::DuplicateIdentity);
        assert_eq!(error.fault().limit_evidence(), None);
    }

    #[test]
    fn test_lexical_error_caused_by_context_includes_violation_and_path() {
        let cause = std::io::Error::other("disk unavailable");
        let error = lexical_error_caused_by(
            LexicalIndexViolation::UnitTooLarge,
            Some(std::path::Path::new("docs/big.md")),
            cause,
        );
        let keys: Vec<&str> = error
            .context()
            .iter()
            .map(rift_core::ErrorContext::key)
            .collect();
        assert_eq!(keys, ["violation", "path"]);
    }

    #[test]
    fn test_lexical_error_caused_by_exposes_underlying_source() {
        let cause = std::io::Error::other("disk unavailable");
        let error = lexical_error_caused_by(LexicalIndexViolation::Storage, None, cause);
        let source = std::error::Error::source(&error).expect("caused-by error must expose source");
        assert_eq!(source.to_string(), "disk unavailable");
    }

    #[test]
    fn test_lexical_error_without_cause_exposes_no_source() {
        let error = lexical_error(LexicalIndexViolation::DuplicateIdentity);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn test_the_corpus_migration_declares_the_columns_in_the_order_bm25_weighs_them() {
        let migration = MIGRATION_FILES
            .iter()
            .find(|file| file.name() == "lexical_documents")
            .expect("the corpus migration must exist");
        let sql = migration.sql().replace('\n', " ");
        // `bm25`'s weights are positional, and the weight list is generated from
        // `SearchableField::ALL` while this declaration is written by hand. Asserting
        // that each name appears somewhere would pass on a reordered declaration,
        // because the typed table names the same columns in the same string.
        let declared = sql
            .split_once("USING fts5(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(columns, _)| columns.to_owned())
            .expect("the migration must declare one FTS5 virtual table");
        let expected = std::iter::once("identity UNINDEXED".to_owned())
            .chain(SearchableField::ALL.map(|field| field.column().to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            declared.starts_with(&expected),
            "the FTS columns must be declared in the order the weights are rendered:\n\
             declared {declared}\nexpected {expected}"
        );
        assert!(
            declared.contains(CORPUS_TOKENIZER),
            "the FTS table must declare the corpus tokenizer: {CORPUS_TOKENIZER}"
        );
    }

    #[test]
    fn test_the_rank_weights_follow_the_declared_field_order() {
        let rendered = rank_weights();
        let expected = std::iter::once("0".to_owned())
            .chain(SearchableField::ALL.map(|field| field.rank_weight().to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(rendered, expected);
    }

    #[test]
    fn test_isolated_weights_raise_exactly_one_column() {
        let rendered = isolated_weights(SearchableField::Documentation);
        assert_eq!(rendered, "0, 0, 0, 0, 0, 1, 0, 0");
    }

    #[test]
    fn test_the_insert_places_one_holder_per_declared_column() {
        let rendered = fts_insert_sql();
        for field in SearchableField::ALL {
            assert!(rendered.contains(field.column()));
        }
        assert!(rendered.contains("?8"), "every field follows identity");
        assert!(!rendered.contains("?9"));
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
        // Three plain columns precede the aliases: identity, path, and kind.
        assert_eq!(lexical_search_column_types().len(), aliased + 3);
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
        assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
        let context = error.context();
        let pragma = context
            .iter()
            .find(|entry| entry.key() == "pragma")
            .map(rift_core::ErrorContext::value);
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
    fn test_lexical_document_record_debug_formats_declared_fields() {
        let record = LexicalDocumentRecord {
            identity: "crate::a".to_owned(),
            path: "src/a.rs".to_owned(),
            kind: "symbol".to_owned(),
            digest: "0f1e2d3c".to_owned(),
            byte_length: 4,
            name: Some("a".to_owned()),
            qualified_name: Some("crate::a".to_owned()),
            identifier_terms: None,
            signature: None,
            documentation: None,
            declaration_source: Some("body".to_owned()),
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
            tree_revision: "deadbeef".to_owned(),
            corpus_revision: "0f1e2d3c".to_owned(),
        };
        let formatted = format!("{record:?}");
        assert!(formatted.contains("deadbeef"));
        assert!(formatted.contains("0f1e2d3c"));
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
    fn test_lexical_index_violation_every_arm_maps_to_registry_identity() {
        let cases = [
            (LexicalIndexViolation::Storage, ErrorCode::StorageFailure),
            (LexicalIndexViolation::UnitLimit, ErrorCode::LimitExceeded),
            (
                LexicalIndexViolation::UnitTooLarge,
                ErrorCode::LimitExceeded,
            ),
            (LexicalIndexViolation::RecordLimit, ErrorCode::LimitExceeded),
            (
                LexicalIndexViolation::StoredPathInvalid,
                ErrorCode::InternalError,
            ),
            (
                LexicalIndexViolation::StoredKindInvalid,
                ErrorCode::InternalError,
            ),
            (
                LexicalIndexViolation::DocumentLocationUnsupported,
                ErrorCode::InternalError,
            ),
            (
                LexicalIndexViolation::DuplicateIdentity,
                ErrorCode::InternalError,
            ),
        ];
        for (violation, expected) in cases {
            assert_eq!(
                lexical_error(violation).name(),
                ErrorName::Wire(expected),
                "violation={violation:?}"
            );
        }
    }

    #[test]
    fn test_lexical_index_fault_context_carries_violation_and_path() {
        let error = lexical_error(LexicalIndexViolation::UnitTooLarge)
            .with_context(rift_core::ErrorContext::new("path", "src/big.rs"));
        let keys: Vec<&str> = error
            .context()
            .iter()
            .map(rift_core::ErrorContext::key)
            .collect();
        assert_eq!(keys, ["violation", "path"]);
    }

    #[test]
    fn test_lexical_index_fault_path_accessor_reports_known_path() {
        let fault = LexicalIndexFault {
            violation: LexicalIndexViolation::Storage,
            path: Some(std::path::PathBuf::from("index.db")),
            source: None,
            limit: None,
        };
        assert_eq!(fault.path(), Some(std::path::Path::new("index.db")));
        assert_eq!(fault.violation(), LexicalIndexViolation::Storage);
    }

    #[test]
    fn test_lexical_index_error_display_names_violation_and_action() {
        let error = lexical_error(LexicalIndexViolation::DuplicateIdentity);
        assert_eq!(
            error.to_string(),
            "the server failed in a way it did not classify: violation duplicate_identity; \
             retry once, and report the full message if the failure repeats"
        );
    }

    #[test]
    fn test_lexical_search_index_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LexicalSearchIndex>();
    }
}
