//! Full-text search over lexical units: code symbols and non-source text
//! files.
//!
//! Unit granularity is the symbol or the text file, chosen so a later
//! dense-ranking layer can attach per-unit vectors without reshaping this
//! store.
//!
//! `SQLite` FTS5 is reached through raw SQL because Toasty 0.10 has no typed
//! virtual-table or `MATCH` API, the same boundary the Toasty compatibility
//! test documents. Raw SQL stays isolated to the FTS virtual table and its
//! rows; the authoritative `lexical_units` and `lexical_index_state` tables are
//! ordinary Toasty models.

use std::path::{Path, PathBuf};

use rift_core::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, ProjectPath, fault_label,
};
use serde::Serialize;
use toasty::db::Connection;
use toasty::migration::{MigrationFile, MigrationSet};
use toasty::stmt::{Type, Value};
use toasty::{Db, Executor};
use toasty_driver_sqlite::Sqlite;

/// Default maximum indexed lexical units per index.
const LEXICAL_UNITS_MAX_DEFAULT: u32 = 20_000;
/// Default maximum content bytes accepted for one lexical unit (1 MiB).
const LEXICAL_UNIT_BYTES_MAX_DEFAULT: u32 = 1_048_576;
/// Default maximum distinct query terms accepted per search.
const LEXICAL_QUERY_TERMS_MAX_DEFAULT: u32 = 32;
/// Default maximum search results returned per query.
const LEXICAL_MATCHES_MAX_DEFAULT: u32 = 1_000;
/// Default pooled `SQLite` connection slots.
const LEXICAL_POOL_SLOTS_DEFAULT: u32 = 4;
/// Default busy-wait budget, in milliseconds, `SQLite` grants a connection
/// before returning `SQLITE_BUSY`.
const LEXICAL_BUSY_TIMEOUT_MS_DEFAULT: u32 = 1_000;
/// Maximum UTF-8 bytes accepted for one unit's declaration name, matching
/// the wire's symbol-name maximum.
const UNIT_NAME_BYTES_MAX: usize = 4_096;

/// `bm25` column weight applied to the FTS `identity` column. `identity` is
/// `UNINDEXED` and never matches a term, but `bm25` still numbers every
/// declared column positionally, `identity` included, so this placeholder
/// keeps the `name` and `content` weights that follow aligned to their
/// columns.
const LEXICAL_IDENTITY_RANK_WEIGHT: f64 = 0.0;
/// `bm25` column weight applied to the FTS `name` column.
const LEXICAL_NAME_RANK_WEIGHT: f64 = 10.0;
/// `bm25` column weight applied to the FTS `content` column.
const LEXICAL_CONTENT_RANK_WEIGHT: f64 = 1.0;

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
];
pub(crate) const MIGRATIONS: MigrationSet = MigrationSet::new(MIGRATION_FILES);

/// Unit granularity the lexical tier indexes.
///
/// The semantic tier stores its vectors in the same database, addressed by the
/// digest of the text they were embedded from rather than by a unit identity:
/// a declaration that moves or is renamed keeps its text, so it keeps its
/// vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalUnitKind {
    /// A code symbol: one declaration extracted from source.
    Symbol,
    /// A non-source text file indexed as one whole unit.
    TextFile,
}

impl LexicalUnitKind {
    /// Returns the spelling stored in `lexical_units.kind`, derived from this
    /// type's serde form so the column value and the wire spelling cannot
    /// drift.
    fn stored_value(self) -> String {
        fault_label(&self)
    }

    /// Parses a stored spelling back into a unit kind.
    fn from_stored(value: &str) -> Option<Self> {
        [Self::Symbol, Self::TextFile]
            .into_iter()
            .find(|kind| kind.stored_value() == value)
    }
}

/// One immutable indexed unit: a code symbol or a whole text file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalUnit {
    identity: String,
    path: ProjectPath,
    kind: LexicalUnitKind,
    name: Option<String>,
    content: String,
}

impl LexicalUnit {
    /// Constructs one lexical unit.
    ///
    /// `identity` is the unit's stable key: the rift symbol id for a symbol
    /// unit, the project path for a text-file unit. `name` is the
    /// declaration name for a symbol unit or the file stem for a text-file
    /// unit, when known.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] with [`LexicalIndexViolation::IdentityEmpty`]
    /// when `identity` is empty.
    pub fn new(
        identity: impl Into<String>,
        path: ProjectPath,
        kind: LexicalUnitKind,
        name: Option<String>,
        content: impl Into<String>,
    ) -> Result<Self, LexicalIndexError> {
        let identity = identity.into();
        if identity.is_empty() {
            return Err(lexical_error(LexicalIndexViolation::IdentityEmpty));
        }
        Ok(Self {
            identity,
            path,
            kind,
            name,
            content: content.into(),
        })
    }

    /// Returns the unit's stable identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Returns the unit's project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns the unit's granularity.
    #[must_use]
    pub const fn kind(&self) -> LexicalUnitKind {
        self.kind
    }

    /// Returns the unit's declaration name or file stem, when known.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the unit's indexed text content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
}

/// One lexical search hit.
///
/// `bm25` is negative; lower is better. `rank` is only comparable within the
/// results of one `search` call.
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalMatch {
    identity: String,
    path: ProjectPath,
    kind: LexicalUnitKind,
    name: Option<String>,
    rank: f64,
}

impl LexicalMatch {
    /// Constructs one lexical match directly. Production code only ever builds these from
    /// a live [`LexicalSearchIndex::search`]; this constructor exists for callers that
    /// merge or resolve matches from a search result they already hold - most notably
    /// tests exercising that merge without a live database.
    #[must_use]
    pub fn new(
        identity: impl Into<String>,
        path: ProjectPath,
        kind: LexicalUnitKind,
        name: Option<String>,
        rank: f64,
    ) -> Self {
        Self {
            identity: identity.into(),
            path,
            kind,
            name,
            rank,
        }
    }

    /// Returns the matched unit's stable identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Returns the matched unit's project-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns the matched unit's granularity.
    #[must_use]
    pub const fn kind(&self) -> LexicalUnitKind {
        self.kind
    }

    /// Returns the matched unit's declaration name or file stem, when known.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns this hit's `bm25` rank, comparable only within one search.
    #[must_use]
    pub const fn rank(&self) -> f64 {
        self.rank
    }
}

/// Resource bounds for one [`LexicalSearchIndex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexicalIndexLimits {
    units_max: u32,
    unit_bytes_max: u32,
    query_terms_max: u32,
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
        query_terms_max: u32,
        matches_max: u32,
        pool_slots: u32,
        busy_timeout_ms: u32,
    ) -> Self {
        Self {
            units_max,
            unit_bytes_max,
            query_terms_max,
            matches_max,
            pool_slots,
            busy_timeout_ms,
        }
    }

    /// Returns maximum indexed units accepted per `replace_all`.
    #[must_use]
    pub const fn units_max(self) -> u32 {
        self.units_max
    }

    /// Returns maximum content bytes accepted for one unit.
    #[must_use]
    pub const fn unit_bytes_max(self) -> u32 {
        self.unit_bytes_max
    }

    /// Returns maximum distinct query terms accepted per search.
    #[must_use]
    pub const fn query_terms_max(self) -> u32 {
        self.query_terms_max
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
}

impl Default for LexicalIndexLimits {
    /// Defaults accept 20,000 units, 1 MiB per unit, 32 distinct query terms,
    /// 1,000 returned matches, 4 pooled connections, and a 1,000ms busy
    /// timeout.
    fn default() -> Self {
        Self::new(
            LEXICAL_UNITS_MAX_DEFAULT,
            LEXICAL_UNIT_BYTES_MAX_DEFAULT,
            LEXICAL_QUERY_TERMS_MAX_DEFAULT,
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
    /// One unit's `content` exceeded `unit_bytes_max` or its `name` exceeded
    /// `UNIT_NAME_BYTES_MAX`.
    UnitTooLarge,
    /// A search query's distinct term count exceeded `query_terms_max`.
    QueryTermLimit,
    /// A stored row's path failed [`ProjectPath`] validation.
    StoredPathInvalid,
    /// A stored row's kind failed to parse as a known [`LexicalUnitKind`].
    StoredKindInvalid,
    /// A unit was constructed with an empty identity.
    IdentityEmpty,
    /// A `replace_all` batch repeated one identity across units.
    DuplicateIdentity,
}

/// The bound and observed value behind a `limit_exceeded` violation, minted from the exact
/// numbers the caller already computed. The wire [`LimitEvidence`] and this fault's own
/// rendered `observed`/`maximum` context both derive from this one typed value, so they
/// cannot drift apart the way reparsing rendered text back into numbers could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LimitBreach {
    field: &'static str,
    observed: u64,
    maximum: u64,
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
            | LexicalIndexViolation::QueryTermLimit => ErrorName::Wire(ErrorCode::LimitExceeded),
            LexicalIndexViolation::StoredPathInvalid
            | LexicalIndexViolation::StoredKindInvalid
            | LexicalIndexViolation::DuplicateIdentity => ErrorName::Wire(ErrorCode::InternalError),
            LexicalIndexViolation::IdentityEmpty => ErrorName::Wire(ErrorCode::InvalidRequest),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.display().to_string()));
        }
        if let Some(breach) = self.limit {
            context.push(ErrorContext::new("observed", breach.observed.to_string()));
            context.push(ErrorContext::new("maximum", breach.maximum.to_string()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.limit.map(|breach| LimitEvidence {
            field: breach.field.to_owned(),
            limit: breach.maximum,
            required: breach.observed,
        })
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

fn lexical_error_caused_by(
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

/// Maps one Toasty operating failure onto [`LexicalIndexViolation::Storage`].
pub(crate) fn storage_error(source: toasty::Error) -> LexicalIndexError {
    lexical_error_caused_by(LexicalIndexViolation::Storage, None, source)
}

/// Builds an FTS5 `MATCH` expression from a free-text query.
///
/// Terms split on non-alphanumeric Unicode boundaries, empty segments drop,
/// and duplicates dedupe keeping first occurrence. FTS5's default `AND`
/// between bareword terms would require every term to match one row, so
/// terms join with `OR` instead: a multi-word query must not require every
/// word to be present. Each term is double-quoted so an FTS5 keyword used as
/// a term (`AND`, `OR`, `NOT`, `NEAR`) cannot be parsed as an operator.
///
/// Returns `None` when the query carries no terms.
///
/// The loop refuses the instant a distinct term count passes `terms_max`,
/// before considering any later part of the query: a query with far more
/// distinct terms than `terms_max` is never fully scanned.
///
/// # Errors
///
/// Returns [`LexicalIndexError`] with [`LexicalIndexViolation::QueryTermLimit`]
/// when the query's distinct term count exceeds `terms_max`.
fn match_expression(query: &str, terms_max: usize) -> Result<Option<String>, LexicalIndexError> {
    let mut seen = std::collections::HashSet::new();
    let mut terms: Vec<&str> = Vec::new();
    for candidate in query.split(|character: char| !character.is_alphanumeric()) {
        if candidate.is_empty() || !seen.insert(candidate) {
            continue;
        }
        terms.push(candidate);
        if terms.len() > terms_max {
            return Err(lexical_error_over_limit(
                LexicalIndexViolation::QueryTermLimit,
                None,
                "query_terms_max",
                limit_count(terms.len()),
                limit_count(terms_max),
            ));
        }
    }
    if terms.is_empty() {
        return Ok(None);
    }
    let expression = terms
        .iter()
        .map(|term| format!("\"{term}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    Ok(Some(expression))
}

/// Splits an identifier into words on case boundaries and non-alphanumeric
/// separators, preserving each word's original casing.
fn split_identifier_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lowercase = false;
    for character in name.chars() {
        if character.is_alphanumeric() {
            if previous_lowercase && character.is_uppercase() && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.push(character);
            previous_lowercase = character.is_lowercase();
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lowercase = false;
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Expands an identifier into its case-split words for FTS indexing.
///
/// FTS5's `unicode61` tokenizer has no `camelCase` folding, so this writes
/// case-split tokens at insert time instead: `SearchHit` becomes
/// `"SearchHit search hit"`, `parse_config_file` becomes
/// `"parse_config_file parse config file"`. Returns the input unchanged when
/// splitting adds no new word.
fn identifier_expansion(name: &str) -> String {
    let words = split_identifier_words(name);
    if words.len() == 1 && words[0] == name {
        return name.to_owned();
    }
    let expansion = words
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if expansion.is_empty() || expansion == name {
        return name.to_owned();
    }
    format!("{name} {expansion}")
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
    unit: &LexicalUnit,
    field: &'static str,
    observed: usize,
    maximum: usize,
) -> LexicalIndexError {
    lexical_error_over_limit(
        LexicalIndexViolation::UnitTooLarge,
        Some(Path::new(unit.path().as_str())),
        field,
        limit_count(observed),
        limit_count(maximum),
    )
    .with_context(ErrorContext::new("field", field))
}

/// Refuses a `replace_all` batch that violates a configured bound, before any
/// database work begins. Bounds apply to the batch size, each unit's
/// `content` (`unit_bytes_max`), and each unit's `name`
/// (`UNIT_NAME_BYTES_MAX`).
fn validate_lexical_batch(
    units: &[LexicalUnit],
    limits: LexicalIndexLimits,
) -> Result<(), LexicalIndexError> {
    validate_indexed_count(units.len(), limits)?;
    validate_lexical_units(units, limits)
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

/// Refuses a unit whose fields break their bounds, and a batch spelling one identity twice.
fn validate_lexical_units(
    units: &[LexicalUnit],
    limits: LexicalIndexLimits,
) -> Result<(), LexicalIndexError> {
    let mut identities_seen = std::collections::HashSet::with_capacity(units.len());
    for unit in units {
        if let Some(name) = unit.name()
            && name.len() > UNIT_NAME_BYTES_MAX
        {
            return Err(oversized_field_error(
                unit,
                "name",
                name.len(),
                UNIT_NAME_BYTES_MAX,
            ));
        }
        if unit.content().len() > bound_as_usize(limits.unit_bytes_max()) {
            return Err(oversized_field_error(
                unit,
                "content",
                unit.content().len(),
                bound_as_usize(limits.unit_bytes_max()),
            ));
        }
        if !identities_seen.insert(unit.identity()) {
            return Err(lexical_error(LexicalIndexViolation::DuplicateIdentity)
                .with_context(ErrorContext::new("identity", unit.identity().to_owned())));
        }
    }
    Ok(())
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_units"]
struct LexicalUnitRecord {
    #[key]
    identity: String,
    path: String,
    kind: String,
    name: Option<String>,
    byte_length: i64,
    content: String,
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_index_state"]
struct LexicalIndexStateRecord {
    #[key]
    id: i64,
    tree_revision: String,
}

/// Converts a content byte length already bounded by `unit_bytes_max` (a
/// `u32`) into the wire-width integer `lexical_units.byte_length` stores.
fn checked_byte_length(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or_else(|_| {
        unreachable!(
            "content byte length must fit i64 once bounded by unit_bytes_max: bytes={bytes}"
        )
    })
}

/// Verifies a single-row pragma result matches the value just requested.
fn require_pragma_row(rows: &[Value], expected: &[Value]) -> Result<(), LexicalIndexError> {
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
/// The FTS rows go first, while `lexical_units` still holds the identities that name them:
/// the virtual table carries no path column of its own.
async fn delete_path_units(
    executor: &mut dyn Executor,
    path: &ProjectPath,
) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(
        "DELETE FROM lexical_units_fts WHERE identity IN \
         (SELECT identity FROM lexical_units WHERE path = ?1)",
    )
    .bind(path.as_str().to_owned())
    .exec(executor)
    .await
    .map_err(storage_error)?;
    toasty::sql::statement("DELETE FROM lexical_units WHERE path = ?1")
        .bind(path.as_str().to_owned())
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// How many units the typed table holds right now.
async fn indexed_unit_count(executor: &mut dyn Executor) -> Result<usize, LexicalIndexError> {
    let rows = toasty::sql::query("SELECT count(*) FROM lexical_units")
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

async fn insert_lexical_unit(
    executor: &mut dyn Executor,
    unit: &LexicalUnit,
) -> Result<(), LexicalIndexError> {
    toasty::create!(LexicalUnitRecord {
        identity: unit.identity().to_owned(),
        path: unit.path().as_str().to_owned(),
        kind: unit.kind().stored_value(),
        name: unit.name().map(str::to_owned),
        byte_length: checked_byte_length(unit.content().len()),
        content: unit.content().to_owned(),
    })
    .exec(executor)
    .await
    .map_err(storage_error)?;

    let expanded_name = unit.name().map_or_else(String::new, identifier_expansion);
    toasty::sql::statement(
        "INSERT INTO lexical_units_fts(identity, name, content) VALUES (?1, ?2, ?3)",
    )
    .bind(unit.identity().to_owned())
    .bind(expanded_name)
    .bind(unit.content().to_owned())
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(())
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
        name,
        Value::F64(rank),
    ] = record.as_slice()
    else {
        unreachable!("lexical search row must match its declared column types: row={row:?}");
    };
    let path = ProjectPath::new(path.clone()).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredPathInvalid, None, source)
    })?;
    let kind = LexicalUnitKind::from_stored(kind).ok_or_else(|| {
        lexical_error(LexicalIndexViolation::StoredKindInvalid)
            .with_context(ErrorContext::new("kind", kind.clone()))
    })?;
    Ok(LexicalMatch {
        identity: identity.clone(),
        path,
        kind,
        name: decode_name_column(name),
        rank: *rank,
    })
}

/// Reads the nullable `name` column of a joined search row.
fn decode_name_column(value: &Value) -> Option<String> {
    match value {
        Value::String(name) => Some(name.clone()),
        _ => None,
    }
}

/// Builds the joined ranking query, embedding this module's own weight
/// constants directly since FTS5's `bm25` column-weight arguments are not
/// bind parameters.
fn lexical_search_sql() -> String {
    format!(
        "SELECT lexical_units_fts.identity, lexical_units.path, lexical_units.kind, \
         lexical_units.name, \
         bm25(lexical_units_fts, {LEXICAL_IDENTITY_RANK_WEIGHT}, {LEXICAL_NAME_RANK_WEIGHT}, \
         {LEXICAL_CONTENT_RANK_WEIGHT}) AS rank \
         FROM lexical_units_fts \
         JOIN lexical_units ON lexical_units.identity = lexical_units_fts.identity \
         WHERE lexical_units_fts MATCH ?1 \
         ORDER BY rank LIMIT ?2"
    )
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
    inserted: Vec<LexicalUnit>,
}

impl LexicalChange {
    /// Builds one change from the paths whose units go and the units that replace them.
    #[must_use]
    pub fn new(replaced: Vec<ProjectPath>, inserted: Vec<LexicalUnit>) -> Self {
        Self { replaced, inserted }
    }

    /// The paths whose stored units this change deletes before it inserts.
    #[must_use]
    pub fn replaced(&self) -> &[ProjectPath] {
        &self.replaced
    }

    /// The units this change inserts.
    #[must_use]
    pub fn inserted(&self) -> &[LexicalUnit] {
        &self.inserted
    }

    /// Whether this change writes nothing, so the stored set already answers for the tree
    /// that produced it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replaced.is_empty() && self.inserted.is_empty()
    }
}

/// `SQLite` FTS5-backed lexical search index.
#[derive(Debug)]
pub struct LexicalSearchIndex {
    database: Db,
    limits: LexicalIndexLimits,
}

impl LexicalSearchIndex {
    /// Opens (creating if absent) the lexical index at `database_path`.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the database cannot be opened or
    /// its schema migration fails.
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
            .models(toasty::models!(LexicalUnitRecord, LexicalIndexStateRecord))
            .max_pool_size(bound_as_usize(limits.pool_slots()));
        let database = builder
            .build(Sqlite::open(database_path))
            .await
            .map_err(|source| {
                lexical_error_caused_by(LexicalIndexViolation::Storage, Some(database_path), source)
            })?;
        let _migration_report = MIGRATIONS.apply(&database).await.map_err(|source| {
            lexical_error_caused_by(LexicalIndexViolation::Storage, Some(database_path), source)
        })?;
        Ok(Self { database, limits })
    }

    /// Acquires a pooled connection configured with this adapter's required
    /// pragmas. Foreign keys are not configured: this schema's tables carry
    /// no foreign-key relationships between them.
    async fn configured_connection(&self) -> Result<Connection, LexicalIndexError> {
        let mut connection = self.database.connection().await.map_err(storage_error)?;

        let journal_mode = toasty::sql::query("PRAGMA journal_mode = WAL")
            .column_types([Type::String])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        require_pragma_row(&journal_mode, &[Value::String("wal".to_owned())])?;

        let busy_timeout_ms = i64::from(self.limits.busy_timeout_ms());
        toasty::sql::query(format!("PRAGMA busy_timeout = {busy_timeout_ms}"))
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        let busy_timeout = toasty::sql::query("PRAGMA busy_timeout")
            .column_types([Type::I64])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        require_pragma_row(&busy_timeout, &[Value::I64(busy_timeout_ms)])?;

        Ok(connection)
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
        units: &[LexicalUnit],
        tree_revision: &str,
    ) -> Result<(), LexicalIndexError> {
        validate_lexical_batch(units, self.limits)?;

        let mut connection = self.configured_connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;

        toasty::sql::statement("DELETE FROM lexical_units_fts")
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;
        LexicalUnitRecord::all()
            .delete()
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;

        for unit in units {
            insert_lexical_unit(&mut transaction, unit).await?;
        }

        LexicalIndexStateRecord::upsert_by_id(LEXICAL_INDEX_STATE_ID)
            .tree_revision(tree_revision.to_owned())
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;

        transaction.commit().await.map_err(storage_error)
    }

    /// Applies one change set's units and stamps `tree_revision`, in one transaction.
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
        validate_lexical_units(change.inserted(), self.limits)?;

        let mut connection = self.configured_connection().await?;
        let mut transaction = connection.transaction().await.map_err(storage_error)?;

        for path in change.replaced() {
            delete_path_units(&mut transaction, path).await?;
        }
        let stored = indexed_unit_count(&mut transaction).await?;
        validate_indexed_count(stored.saturating_add(change.inserted().len()), self.limits)?;

        for unit in change.inserted() {
            insert_lexical_unit(&mut transaction, unit).await?;
        }

        LexicalIndexStateRecord::upsert_by_id(LEXICAL_INDEX_STATE_ID)
            .tree_revision(tree_revision.to_owned())
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;

        transaction.commit().await.map_err(storage_error)
    }

    /// Searches indexed units, best matches first.
    ///
    /// An empty or all-punctuation `query` returns an empty result. The
    /// effective result count is `limit` capped by `matches_max`.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when `query` carries more distinct terms
    /// than `query_terms_max`, when a stored row fails to decode, or on
    /// storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; the search issues one read-only
    /// query.
    pub async fn search(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<LexicalMatch>, LexicalIndexError> {
        let terms_max = bound_as_usize(self.limits.query_terms_max());
        let Some(expression) = match_expression(query, terms_max)? else {
            return Ok(Vec::new());
        };
        let effective_limit = i64::from(limit.min(self.limits.matches_max()));

        let mut connection = self.configured_connection().await?;
        let rows = toasty::sql::query(lexical_search_sql())
            .bind(expression)
            .bind(effective_limit)
            .column_types([
                Type::String,
                Type::String,
                Type::String,
                Type::String,
                Type::F64,
            ])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;

        rows.iter().map(decode_lexical_match).collect()
    }

    /// Returns one unit's indexed content by its identity, or `None` when no
    /// unit with that identity is indexed.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] on storage failure.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only lookup.
    pub async fn content(&self, identity: &str) -> Result<Option<String>, LexicalIndexError> {
        let mut connection = self.configured_connection().await?;
        let record = LexicalUnitRecord::filter_by_identity(identity)
            .first()
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(record.map(|record| record.content))
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
        let record = LexicalIndexStateRecord::filter_by_id(LEXICAL_INDEX_STATE_ID)
            .first()
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        Ok(record.map(|record| record.tree_revision))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LexicalIndexFault, LexicalIndexLimits, LexicalIndexStateRecord, LexicalIndexViolation,
        LexicalSearchIndex, LexicalUnit, LexicalUnitKind, LexicalUnitRecord, UNIT_NAME_BYTES_MAX,
        checked_byte_length, decode_lexical_match, identifier_expansion, lexical_error,
        lexical_error_caused_by, match_expression, require_pragma_row, validate_lexical_batch,
    };
    use rift_core::{ErrorCode, ErrorName, Fault, ProjectPath};
    use toasty::stmt::Value;

    #[test]
    fn test_lexical_unit_new_refuses_empty_identity() {
        let path = ProjectPath::new("docs/a.md").expect("fixture path must be valid");
        let error = LexicalUnit::new("", path, LexicalUnitKind::TextFile, None, "content")
            .expect_err("empty identity must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::IdentityEmpty
        );
    }

    #[test]
    fn test_lexical_index_limits_default_accepts_documented_pool_and_timeout() {
        let limits = LexicalIndexLimits::default();
        assert_eq!(limits.pool_slots(), 4);
        assert_eq!(limits.busy_timeout_ms(), 1_000);
    }

    fn symbol_unit_with_name(name: String) -> LexicalUnit {
        let path = ProjectPath::new("src/a.rs").expect("fixture path must be valid");
        LexicalUnit::new(
            "crate::a",
            path,
            LexicalUnitKind::Symbol,
            Some(name),
            "body",
        )
        .expect("fixture unit must construct")
    }

    #[test]
    fn test_validate_lexical_batch_name_at_exact_limit_passes() {
        let unit = symbol_unit_with_name("n".repeat(UNIT_NAME_BYTES_MAX));
        validate_lexical_batch(&[unit], LexicalIndexLimits::default())
            .expect("name at exact limit must not refuse");
    }

    #[test]
    fn test_validate_lexical_batch_name_one_over_limit_refuses_naming_field_and_path() {
        let unit = symbol_unit_with_name("n".repeat(UNIT_NAME_BYTES_MAX + 1));
        let error = validate_lexical_batch(&[unit], LexicalIndexLimits::default())
            .expect_err("name one over limit must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::UnitTooLarge
        );
        let context = error.context();
        let field = context
            .iter()
            .find(|entry| entry.key() == "field")
            .map(rift_core::ErrorContext::value);
        assert_eq!(field, Some("name"));
        assert_eq!(error.fault().path(), Some(std::path::Path::new("src/a.rs")));
    }

    #[test]
    fn test_validate_lexical_batch_content_over_limit_refuses_naming_content_field() {
        let path = ProjectPath::new("src/a.rs").expect("fixture path must be valid");
        let unit = LexicalUnit::new(
            "crate::a",
            path,
            LexicalUnitKind::Symbol,
            None,
            "way too much body content",
        )
        .expect("fixture unit must construct");
        let limits = LexicalIndexLimits::new(100, 8, 32, 100, 4, 1_000);
        let error = validate_lexical_batch(&[unit], limits)
            .expect_err("content one over limit must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::UnitTooLarge
        );
        let context = error.context();
        let field = context
            .iter()
            .find(|entry| entry.key() == "field")
            .map(rift_core::ErrorContext::value);
        assert_eq!(field, Some("content"));
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
        let error = lexical_error(LexicalIndexViolation::IdentityEmpty);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn test_match_expression_empty_query_returns_none() {
        let expression = match_expression("", 8).expect("empty query must not refuse");
        assert_eq!(expression, None);
    }

    #[test]
    fn test_match_expression_punctuation_only_returns_none() {
        let expression =
            match_expression("   ...--- ", 8).expect("punctuation-only query must not refuse");
        assert_eq!(expression, None);
    }

    #[test]
    fn test_match_expression_dedupes_preserving_first_occurrence() {
        let expression =
            match_expression("alpha beta alpha", 8).expect("query must be within terms_max");
        assert_eq!(expression, Some("\"alpha\" OR \"beta\"".to_owned()));
    }

    #[test]
    fn test_match_expression_exact_limit_is_accepted() {
        let expression =
            match_expression("one two three", 3).expect("exact-limit query must not refuse");
        assert_eq!(
            expression,
            Some("\"one\" OR \"two\" OR \"three\"".to_owned())
        );
    }

    #[test]
    fn test_match_expression_one_over_limit_is_refused() {
        let error = match_expression("one two three four", 3)
            .expect_err("one-over-limit query must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::QueryTermLimit
        );
    }

    #[test]
    fn test_match_expression_stops_counting_immediately_past_terms_max() {
        let error = match_expression("alpha beta gamma delta epsilon", 2)
            .expect_err("over-limit query must refuse");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::QueryTermLimit
        );
        let context = error.context();
        let observed = context
            .iter()
            .find(|entry| entry.key() == "observed")
            .map(rift_core::ErrorContext::value);
        let maximum = context
            .iter()
            .find(|entry| entry.key() == "maximum")
            .map(rift_core::ErrorContext::value);
        assert_eq!(
            observed,
            Some("3"),
            "loop must refuse on the first term past terms_max, not after scanning every term"
        );
        assert_eq!(maximum, Some("2"));
    }

    #[test]
    fn test_query_term_limit_exposes_typed_limit_evidence() {
        let error = match_expression("alpha beta gamma delta", 3)
            .expect_err("over-limit query must refuse");
        assert_eq!(
            error.fault().limit_evidence(),
            Some(rift_core::LimitEvidence {
                field: "query_terms_max".to_owned(),
                limit: 3,
                required: 4,
            }),
            "the wire evidence must derive from the same typed breach as the rendered context"
        );
    }

    #[test]
    fn test_unit_limit_exposes_typed_limit_evidence() {
        let units = vec![
            symbol_unit_with_name("a".to_owned()),
            symbol_unit_with_name("b".to_owned()),
        ];
        let limits = LexicalIndexLimits::new(1, 1_048_576, 32, 1_000, 4, 1_000);
        let error =
            validate_lexical_batch(&units, limits).expect_err("a batch over units_max must refuse");
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
    fn test_unit_too_large_exposes_typed_limit_evidence_and_field_context() {
        let unit = symbol_unit_with_name("n".repeat(UNIT_NAME_BYTES_MAX + 1));
        let error = validate_lexical_batch(&[unit], LexicalIndexLimits::default())
            .expect_err("an oversized name must refuse");
        assert_eq!(
            error.fault().limit_evidence(),
            Some(rift_core::LimitEvidence {
                field: "name".to_owned(),
                limit: UNIT_NAME_BYTES_MAX as u64,
                required: (UNIT_NAME_BYTES_MAX + 1) as u64,
            })
        );
        // The rendered `field` context (used by the human-readable message) still comes
        // from the same `oversized_field_error` call, not a second, drifting source.
        let context = error.context();
        let field = context
            .iter()
            .find(|entry| entry.key() == "field")
            .map(rift_core::ErrorContext::value);
        assert_eq!(field, Some("name"));
    }

    #[test]
    fn test_a_non_limit_violation_exposes_no_limit_evidence() {
        let error = lexical_error(LexicalIndexViolation::IdentityEmpty);
        assert_eq!(error.fault().limit_evidence(), None);
    }

    #[test]
    fn test_match_expression_quotes_every_term() {
        let expression = match_expression("hello", 8).expect("single-term query must not refuse");
        assert_eq!(expression, Some("\"hello\"".to_owned()));
    }

    #[test]
    fn test_match_expression_quotes_fts_keyword_used_as_term() {
        let expression = match_expression("AND OR NOT NEAR", 8)
            .expect("keyword-shaped terms must still be accepted");
        assert_eq!(
            expression,
            Some("\"AND\" OR \"OR\" OR \"NOT\" OR \"NEAR\"".to_owned())
        );
    }

    #[test]
    fn test_match_expression_splits_on_unicode_word_boundary() {
        let expression = match_expression("caf\u{e9},th\u{e9}", 8)
            .expect("unicode-letter query must not refuse");
        assert_eq!(expression, Some("\"caf\u{e9}\" OR \"th\u{e9}\"".to_owned()));
    }

    #[test]
    fn test_identifier_expansion_camel_case_appends_split_words() {
        assert_eq!(
            identifier_expansion("getUserName"),
            "getUserName get user name"
        );
    }

    #[test]
    fn test_identifier_expansion_pascal_case_appends_split_words() {
        assert_eq!(identifier_expansion("SearchHit"), "SearchHit search hit");
    }

    #[test]
    fn test_identifier_expansion_snake_case_appends_split_words() {
        assert_eq!(
            identifier_expansion("parse_config_file"),
            "parse_config_file parse config file"
        );
    }

    #[test]
    fn test_identifier_expansion_screaming_snake_case_appends_split_words() {
        assert_eq!(identifier_expansion("MAX_VALUE"), "MAX_VALUE max value");
    }

    #[test]
    fn test_identifier_expansion_single_word_is_unchanged() {
        assert_eq!(identifier_expansion("identifier"), "identifier");
    }

    #[test]
    fn test_identifier_expansion_digits_inside_word_stay_attached() {
        assert_eq!(
            identifier_expansion("utf8_decode"),
            "utf8_decode utf8 decode"
        );
    }

    #[test]
    fn test_identifier_expansion_unicode_case_boundary_appends_split_words() {
        assert_eq!(
            identifier_expansion("caf\u{e9}Menu"),
            "caf\u{e9}Menu caf\u{e9} menu"
        );
    }

    #[test]
    fn test_identifier_expansion_multi_word_already_lowercase_is_unchanged() {
        assert_eq!(identifier_expansion("foo bar"), "foo bar");
    }

    #[test]
    fn test_identifier_expansion_empty_string_is_unchanged() {
        assert_eq!(identifier_expansion(""), "");
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
    fn test_lexical_unit_record_debug_formats_declared_fields() {
        let record = LexicalUnitRecord {
            identity: "crate::a".to_owned(),
            path: "src/a.rs".to_owned(),
            kind: "symbol".to_owned(),
            name: Some("a".to_owned()),
            byte_length: 4,
            content: "body".to_owned(),
        };
        let formatted = format!("{record:?}");
        assert!(formatted.contains("crate::a"));
        assert!(formatted.contains("src/a.rs"));
    }

    #[test]
    fn test_lexical_index_state_record_debug_formats_declared_fields() {
        let record = LexicalIndexStateRecord {
            id: 1,
            tree_revision: "deadbeef".to_owned(),
        };
        let formatted = format!("{record:?}");
        assert!(formatted.contains("deadbeef"));
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
            (
                LexicalIndexViolation::QueryTermLimit,
                ErrorCode::LimitExceeded,
            ),
            (
                LexicalIndexViolation::StoredPathInvalid,
                ErrorCode::InternalError,
            ),
            (
                LexicalIndexViolation::StoredKindInvalid,
                ErrorCode::InternalError,
            ),
            (
                LexicalIndexViolation::IdentityEmpty,
                ErrorCode::InvalidRequest,
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
        let error = lexical_error(LexicalIndexViolation::IdentityEmpty);
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: violation identity_empty; \
             correct the reported field and resend the request"
        );
    }

    #[test]
    fn test_lexical_search_index_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LexicalSearchIndex>();
    }
}
