//! The trigram index behind regex `pattern` search, and the file rows it does not hold yet.
//!
//! `lexical_documents_trigram` is an external-content FTS5 index holding every three
//! characters of the file rows' text under [`rift_ranking::TRIGRAM_TOKENIZER`], so a regex
//! pattern's prefilter selects the rows that could hold a match. It adds no copy of the
//! text. A lexical write files each new file row in `lexical_trigram_pending` instead of
//! indexing it, and [`index_batch`] indexes those rows later, in bounded transactions of
//! their own: a search waits for the write that stamps the tree it captured, and trigrams
//! indexed inside that write made every such search wait for them too.
//!
//! Every function here runs inside a transaction its caller in [`crate::lexical`] opened,
//! so the trigram index moves with the typed rows it reads. The statements are raw SQL for
//! the reason the lexical module states: Toasty has no typed virtual-table or `MATCH` API,
//! and the pending table is reached through set-based statements over the typed rows.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use rift_core::ProjectPath;
use rift_ranking::Prefilter;
use toasty::Executor;
use toasty::stmt::{Type, Value};

use crate::lexical::{
    LexicalIndexError, LexicalIndexViolation, bound_as_usize, file_row_count,
    lexical_error_caused_by, single_i64, storage_error, stored_file_range,
};

/// The rows the trigram index holds once it has caught up: every row that stores text. A
/// symbol row holds none, and `columnsize=0` is what lets the index skip it: under the
/// default, FTS5's `integrity-check` counts every content row and reports the table
/// malformed.
pub(crate) const TRIGRAM_ROWS: &str = "file_content IS NOT NULL";

/// The file rows the trigram index does hold: those no `lexical_trigram_pending` row names.
const TRIGRAM_INDEXED: &str = "NOT EXISTS (SELECT 1 FROM lexical_trigram_pending \
     WHERE lexical_trigram_pending.id = lexical_documents.id)";

/// Takes one path's rows out of the trigram index and the pending set, while the typed rows
/// still hold them.
///
/// The index gives up the entries of the rows it holds through the FTS5 `'delete'` command,
/// which "must match the values currently stored in the table"
/// (<https://www.sqlite.org/fts5.html>). It names those rows alone: a `'delete'` naming a
/// row the index never took leaves its `integrity-check` reporting the table malformed. The
/// rows it has not taken leave the pending set instead.
pub(crate) async fn delete_path(
    executor: &mut dyn Executor,
    path: &ProjectPath,
) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(format!(
        "INSERT INTO lexical_documents_trigram(lexical_documents_trigram, rowid, file_content) \
         SELECT 'delete', id, file_content FROM lexical_documents \
         WHERE path = ?1 AND {TRIGRAM_ROWS} AND {TRIGRAM_INDEXED}"
    ))
    .bind(path.as_str().to_owned())
    .exec(&mut *executor)
    .await
    .map_err(storage_error)?;
    toasty::sql::statement(
        "DELETE FROM lexical_trigram_pending \
         WHERE id IN (SELECT id FROM lexical_documents WHERE path = ?1)",
    )
    .bind(path.as_str().to_owned())
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// Empties the trigram index and the pending set.
///
/// `'delete-all'` clears an external-content index without reading a typed row: FTS5
/// offers it "only with external content and contentless tables".
pub(crate) async fn clear(executor: &mut dyn Executor) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(
        "INSERT INTO lexical_documents_trigram(lexical_documents_trigram) VALUES('delete-all')",
    )
    .exec(&mut *executor)
    .await
    .map_err(storage_error)?;
    toasty::sql::statement("DELETE FROM lexical_trigram_pending")
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Files every file row with an id above `last` as one the trigram index lacks: the rows a
/// write inserted after reading its last id.
pub(crate) async fn file_rows_above(
    executor: &mut dyn Executor,
    last: i64,
) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(format!(
        "INSERT INTO lexical_trigram_pending(id) \
         SELECT id FROM lexical_documents WHERE id > ?1 AND {TRIGRAM_ROWS}"
    ))
    .bind(last)
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// The files `prefilter` selects from the trigram index, in project-path order, and the
/// file rows the index lacks; see [`crate::LexicalSearchIndex::pattern_candidates`].
pub(crate) async fn candidates(
    executor: &mut dyn Executor,
    prefilter: &Prefilter,
    line_bound: bool,
    rows_max: u32,
) -> Result<PatternCandidates, LexicalIndexError> {
    let Some(selection) = selected_rows(&mut *executor, prefilter, line_bound, rows_max).await?
    else {
        return Ok(PatternCandidates::cut(rows_max));
    };
    let unindexed = unindexed_rows(executor, &selection, line_bound, rows_max).await?;
    Ok(PatternCandidates {
        candidates: grouped(selection.rows),
        truncated_at: None,
        unindexed,
    })
}

/// Indexes the oldest rows the trigram index lacks, at most `rows_max` rows and `bytes_max`
/// bytes of their text, and answers what it did; see
/// [`crate::LexicalSearchIndex::index_trigrams`].
pub(crate) async fn index_batch(
    executor: &mut dyn Executor,
    rows_max: usize,
    bytes_max: u64,
) -> Result<TrigramBatch, LexicalIndexError> {
    let oldest = oldest_pending_rows(&mut *executor, rows_max.max(1)).await?;
    let Some((through, indexed)) = trigram_batch_end(&oldest, bytes_max) else {
        return Ok(TrigramBatch::default());
    };
    index_pending_through(&mut *executor, through).await?;
    let pending = pending_trigram_count(executor).await?;
    Ok(TrigramBatch { indexed, pending })
}

/// One file a regex pattern's prefilter selected, and the spans of it to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternCandidate {
    path: ProjectPath,
    spans: Vec<Range<u64>>,
}

impl PatternCandidate {
    /// The file the selected rows belong to.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// The bytes of the file each selected row holds, in file order, or none when the
    /// whole file is the candidate: a match holding a line feed may cross two rows.
    #[must_use]
    pub fn spans(&self) -> &[Range<u64>] {
        &self.spans
    }
}

/// The files one pattern's prefilter selected from the trigram index, or the bound the
/// selection stopped at, and the stored file rows the trigram index did not hold yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternCandidates {
    candidates: Vec<PatternCandidate>,
    truncated_at: Option<u32>,
    unindexed: Option<UnindexedRows>,
}

impl PatternCandidates {
    /// The selected files, in project-path order.
    #[must_use]
    pub fn candidates(&self) -> &[PatternCandidate] {
        &self.candidates
    }

    /// The row bound the selection stopped at, when the index held more rows than it let
    /// the selection read. The candidates are then a prefix of the selection, and a
    /// caller refuses rather than answer from them.
    #[must_use]
    pub const fn truncated_at(&self) -> Option<u32> {
        self.truncated_at
    }

    /// The stored file rows the trigram index lacked when the selection read it, or `None`
    /// when it held every one. The selection names no file whose text sits in those rows
    /// alone.
    #[must_use]
    pub const fn unindexed(&self) -> Option<&UnindexedRows> {
        self.unindexed.as_ref()
    }

    /// A selection cut at `rows_max`, which carries no candidate.
    const fn cut(rows_max: u32) -> Self {
        Self {
            candidates: Vec::new(),
            truncated_at: Some(rows_max),
            unindexed: None,
        }
    }
}

/// The stored file rows one pattern read found the trigram index lacking: the rows a
/// write stored since [`crate::LexicalSearchIndex::index_trigrams`] last caught up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnindexedRows {
    prepared: u64,
    total: u64,
    completed: Option<Vec<PatternCandidate>>,
}

impl UnindexedRows {
    /// File rows the trigram index holds: whole files, and the chunks of files split
    /// under `[search.text]`.
    #[must_use]
    pub const fn prepared(&self) -> u64 {
        self.prepared
    }

    /// File rows the store holds.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// The selection merged with every row the index lacks, in project-path order, each of
    /// those rows a candidate the index could not rule out; `None` when those rows numbered
    /// more than the candidate bound left the selection room for.
    #[must_use]
    pub fn completed(&self) -> Option<&[PatternCandidate]> {
        self.completed.as_deref()
    }
}

/// What one prefilter selected: each selected row as its file and the bytes of the file
/// it holds, or its file alone when the whole file is the candidate, and how many index
/// rows the selection read.
struct Selection {
    rows: Vec<(ProjectPath, Option<Range<u64>>)>,
    rows_read: u32,
}

/// Groups rows by file, from rows in any order: each file's spans in file order, or no
/// span when any of its rows names the whole file.
fn grouped(
    rows: impl IntoIterator<Item = (ProjectPath, Option<Range<u64>>)>,
) -> Vec<PatternCandidate> {
    let mut files: BTreeMap<ProjectPath, Option<Vec<Range<u64>>>> = BTreeMap::new();
    for (path, span) in rows {
        let spans = files.entry(path).or_insert_with(|| Some(Vec::new()));
        match span {
            Some(span) => {
                if let Some(spans) = spans {
                    spans.push(span);
                }
            }
            None => *spans = None,
        }
    }
    files
        .into_iter()
        .map(|(path, spans)| {
            let mut spans = spans.unwrap_or_default();
            spans.sort_by_key(|span| (span.start, span.end));
            PatternCandidate { path, spans }
        })
        .collect()
}

/// The rows one bounded read answered, or `None` when it answered the row past
/// `rows_max`: every row of the read's shape is a file and the bytes of the file it holds.
fn within_rows(
    rows: &[Value],
    rows_max: u32,
) -> Result<Option<Vec<(ProjectPath, Range<u64>)>>, LexicalIndexError> {
    if rows.len() > bound_as_usize(rows_max) {
        return Ok(None);
    }
    rows.iter()
        .map(decode_trigram_row)
        .collect::<Result<_, _>>()
        .map(Some)
}

/// The column types of every read answering a file row as its path and the bytes of the
/// file it holds.
const FILE_ROW_COLUMNS: [Type; 3] = [Type::String, Type::I64, Type::I64];

/// The trigram rows `expression` selects, at most `rows_max` of them: each row's file and
/// the bytes of the file it holds. Reads one row past the bound, and answers `None` when
/// the index held that row. The read carries no order, so the bound cuts before any sort.
///
/// Only a row whose text is the slice of its file starting at `byte_offset` answers. A
/// notebook cell's row holds the cell's source rather than the file's bytes, records no
/// offset, and never selects its file: a caller verifies a notebook whole.
async fn trigram_rows(
    executor: &mut dyn Executor,
    expression: &str,
    rows_max: u32,
) -> Result<Option<Vec<(ProjectPath, Range<u64>)>>, LexicalIndexError> {
    let rows = toasty::sql::query(
        "SELECT lexical_documents.path, lexical_documents.byte_offset, \
         lexical_documents.byte_length \
         FROM lexical_documents_trigram \
         JOIN lexical_documents ON lexical_documents.id = lexical_documents_trigram.rowid \
         WHERE lexical_documents_trigram MATCH ?1 \
         AND lexical_documents.byte_offset IS NOT NULL LIMIT ?2",
    )
    .bind(expression.to_owned())
    .bind(i64::from(rows_max) + 1)
    .column_types(FILE_ROW_COLUMNS)
    .exec(executor)
    .await
    .map_err(storage_error)?;
    within_rows(&rows, rows_max)
}

/// The file rows the trigram index lacks, at most `rows_max` of them, as [`trigram_rows`]
/// answers the rows it selects: a notebook cell's row stays out, since a caller verifies a
/// notebook whole whatever the index holds.
async fn pending_rows(
    executor: &mut dyn Executor,
    rows_max: u32,
) -> Result<Option<Vec<(ProjectPath, Range<u64>)>>, LexicalIndexError> {
    let rows = toasty::sql::query(
        "SELECT lexical_documents.path, lexical_documents.byte_offset, \
         lexical_documents.byte_length \
         FROM lexical_trigram_pending \
         JOIN lexical_documents ON lexical_documents.id = lexical_trigram_pending.id \
         WHERE lexical_documents.byte_offset IS NOT NULL LIMIT ?1",
    )
    .bind(i64::from(rows_max) + 1)
    .column_types(FILE_ROW_COLUMNS)
    .exec(executor)
    .await
    .map_err(storage_error)?;
    within_rows(&rows, rows_max)
}

/// One trigram row as its file and the bytes of the file it holds.
fn decode_trigram_row(row: &Value) -> Result<(ProjectPath, Range<u64>), LexicalIndexError> {
    let Value::Record(record) = row else {
        unreachable!("trigram row must be a record: row={row:?}");
    };
    let [Value::String(path), Value::I64(offset), Value::I64(length)] = record.as_slice() else {
        unreachable!("trigram row must match its declared column types: row={row:?}");
    };
    let path = ProjectPath::new(path.clone()).map_err(|source| {
        lexical_error_caused_by(LexicalIndexViolation::StoredPathInvalid, None, source)
    })?;
    Ok((path, stored_file_range(*offset, *length)?))
}

/// The rows `prefilter` selects, at most `rows_max` of them, or `None` past the bound.
///
/// A `line_bound` pattern matches inside one line, and a line sits inside one row, since a
/// chunk packs whole lines: one `MATCH` of the whole formula selects the rows, each with
/// its span. A formula nested past [`rift_ranking::ROW_EXPRESSION_DEPTH_MAX`], and any
/// pattern that may cross a line, runs one `MATCH` per literal and combines their files
/// instead, each file a whole candidate.
async fn selected_rows(
    executor: &mut dyn Executor,
    prefilter: &Prefilter,
    line_bound: bool,
    rows_max: u32,
) -> Result<Option<Selection>, LexicalIndexError> {
    let Some(expression) = prefilter.row_expression().filter(|_| line_bound) else {
        return literal_rows(executor, prefilter, rows_max).await;
    };
    let Some(rows) = trigram_rows(executor, &expression, rows_max).await? else {
        return Ok(None);
    };
    Ok(Some(Selection {
        rows_read: u32::try_from(rows.len()).unwrap_or(rows_max),
        rows: rows
            .into_iter()
            .map(|(path, span)| (path, Some(span)))
            .collect(),
    }))
}

/// The files `prefilter` selects when its members may sit in different rows of one file:
/// one `MATCH` per literal, the literals' files combined as the formula combines them.
/// Every literal's read counts against one `rows_max` budget.
async fn literal_rows(
    executor: &mut dyn Executor,
    prefilter: &Prefilter,
    rows_max: u32,
) -> Result<Option<Selection>, LexicalIndexError> {
    let mut remaining = rows_max;
    let mut holding: BTreeMap<&BTreeSet<String>, BTreeSet<ProjectPath>> = BTreeMap::new();
    for literal in prefilter.literals() {
        let expression = Prefilter::literal_expression(literal);
        let Some(rows) = trigram_rows(&mut *executor, &expression, remaining).await? else {
            return Ok(None);
        };
        remaining -= u32::try_from(rows.len()).unwrap_or(remaining);
        holding.insert(literal, rows.into_iter().map(|(path, _)| path).collect());
    }
    let files: BTreeSet<&ProjectPath> = holding.values().flatten().collect();
    let rows = files
        .into_iter()
        .filter(|path| {
            prefilter.accepts_literals(&|literal| {
                holding
                    .get(literal)
                    .is_some_and(|paths| paths.contains(*path))
            })
        })
        .map(|path| (path.clone(), None))
        .collect();
    Ok(Some(Selection {
        rows,
        rows_read: rows_max - remaining,
    }))
}

/// The file rows the trigram index lacks, or `None` when it holds every one.
///
/// The rows are listed while they fit the room `rows_max` leaves past the rows the
/// selection read, and each joins the selection as a candidate the index could not rule
/// out: its span for a `line_bound` pattern, its whole file for one that may cross a line.
/// A caller that verifies them answers every match the stored text holds.
async fn unindexed_rows(
    executor: &mut dyn Executor,
    selection: &Selection,
    line_bound: bool,
    rows_max: u32,
) -> Result<Option<UnindexedRows>, LexicalIndexError> {
    let pending = pending_trigram_count(&mut *executor).await?;
    if pending == 0 {
        return Ok(None);
    }
    let total = file_row_count(&mut *executor).await?;
    let room = rows_max.saturating_sub(selection.rows_read);
    let completed = pending_rows(executor, room).await?.map(|listed| {
        let pending = listed
            .into_iter()
            .map(|(path, span)| (path, line_bound.then_some(span)));
        grouped(selection.rows.iter().cloned().chain(pending))
    });
    Ok(Some(UnindexedRows {
        prepared: total.saturating_sub(pending),
        total,
        completed,
    }))
}

/// How many file rows the trigram index lacks right now.
async fn pending_trigram_count(executor: &mut dyn Executor) -> Result<u64, LexicalIndexError> {
    let counted = single_i64(
        executor,
        "SELECT count(*) FROM lexical_trigram_pending",
        "pending trigram rows",
    )
    .await?;
    Ok(u64::try_from(counted).unwrap_or(0))
}

/// What one [`crate::LexicalSearchIndex::index_trigrams`] transaction did. The default is a
/// transaction that found no row lacking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrigramBatch {
    indexed: u64,
    pending: u64,
}

impl TrigramBatch {
    /// Rows this transaction took off the pending set, each one now in the trigram index.
    #[must_use]
    pub const fn indexed(self) -> u64 {
        self.indexed
    }

    /// File rows the trigram index still lacks after it.
    #[must_use]
    pub const fn pending(self) -> u64 {
        self.pending
    }
}

/// The last id and the number of the rows one transaction indexes out of `oldest`, the
/// lacking rows in id order with the bytes of text each holds: every row while their text
/// stays within `bytes_max`, and the first row whatever its size, so a row past the bound
/// takes a transaction of its own. `None` when no row is lacking.
fn trigram_batch_end(oldest: &[(i64, u64)], bytes_max: u64) -> Option<(i64, u64)> {
    let (&(first, first_bytes), rest) = oldest.split_first()?;
    let mut last = first;
    let mut count = 1_u64;
    let mut bytes = first_bytes;
    for &(id, row_bytes) in rest {
        bytes = bytes.saturating_add(row_bytes);
        if bytes > bytes_max {
            break;
        }
        last = id;
        count += 1;
    }
    Some((last, count))
}

/// The oldest `rows_max` rows the trigram index lacks, in id order, with the bytes of text
/// each holds. A pending id whose typed row is gone reads as holding none, so the batch
/// that reaches it clears it rather than stopping short of it.
async fn oldest_pending_rows(
    executor: &mut dyn Executor,
    rows_max: usize,
) -> Result<Vec<(i64, u64)>, LexicalIndexError> {
    let rows = toasty::sql::query(
        "SELECT lexical_trigram_pending.id, coalesce(lexical_documents.byte_length, 0) \
         FROM lexical_trigram_pending \
         LEFT JOIN lexical_documents ON lexical_documents.id = lexical_trigram_pending.id \
         ORDER BY lexical_trigram_pending.id LIMIT ?1",
    )
    .bind(i64::try_from(rows_max).unwrap_or(i64::MAX))
    .column_types([Type::I64, Type::I64])
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(rows.iter().map(decode_pending_size).collect())
}

/// One pending row as its id and the bytes of text it holds.
fn decode_pending_size(row: &Value) -> (i64, u64) {
    let Value::Record(record) = row else {
        unreachable!("pending row must be a record: row={row:?}");
    };
    let [Value::I64(id), Value::I64(bytes)] = record.as_slice() else {
        unreachable!("pending row must match its declared column types: row={row:?}");
    };
    (*id, u64::try_from(*bytes).unwrap_or(0))
}

/// Indexes every file row the trigram index lacks up to id `through`, and takes those rows
/// off the pending set.
async fn index_pending_through(
    executor: &mut dyn Executor,
    through: i64,
) -> Result<(), LexicalIndexError> {
    toasty::sql::statement(format!(
        "INSERT INTO lexical_documents_trigram(rowid, file_content) \
         SELECT lexical_documents.id, lexical_documents.file_content \
         FROM lexical_trigram_pending \
         JOIN lexical_documents ON lexical_documents.id = lexical_trigram_pending.id \
         WHERE lexical_trigram_pending.id <= ?1 AND {TRIGRAM_ROWS}"
    ))
    .bind(through)
    .exec(&mut *executor)
    .await
    .map_err(storage_error)?;
    toasty::sql::statement("DELETE FROM lexical_trigram_pending WHERE id <= ?1")
        .bind(through)
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TRIGRAM_ROWS, grouped, trigram_batch_end};
    use rift_core::ProjectPath;

    /// A trigram batch takes rows in id order while their text fits the byte bound, and the
    /// first row whatever its size, so no row is ever left behind.
    #[test]
    fn test_a_trigram_batch_ends_at_the_byte_bound_and_always_takes_one_row() {
        assert_eq!(trigram_batch_end(&[], 100), None);
        let rows = [(3, 40), (5, 40), (9, 20), (12, 1)];
        assert_eq!(
            trigram_batch_end(&rows, 100),
            Some((9, 3)),
            "exactly at the bound"
        );
        assert_eq!(trigram_batch_end(&rows, 99), Some((5, 2)), "one byte short");
        assert_eq!(
            trigram_batch_end(&rows, 101),
            Some((12, 4)),
            "every row fits"
        );
        assert_eq!(
            trigram_batch_end(&[(7, 500), (8, 1)], 100),
            Some((7, 1)),
            "a row past the bound goes alone"
        );
        assert_eq!(
            trigram_batch_end(&[(1, u64::MAX), (2, 1)], u64::MAX - 1),
            Some((1, 1)),
            "the byte sum saturates rather than wrapping past the bound"
        );
    }

    /// Rows group by file in project-path order, a file's spans sorted in file order, and a
    /// row naming the whole file makes the whole file the candidate whatever else names it.
    #[test]
    fn test_rows_group_by_file_and_a_whole_file_row_wins() {
        let path = |value: &str| ProjectPath::new(value).expect("fixture path must be valid");
        let candidates = grouped([
            (path("b.txt"), Some(40..60)),
            (path("a.txt"), Some(10..20)),
            (path("b.txt"), Some(0..40)),
            (path("c.txt"), Some(0..5)),
            (path("c.txt"), None),
            (path("c.txt"), Some(5..9)),
            (path("d.txt"), None),
        ]);
        let shape: Vec<(&str, Vec<std::ops::Range<u64>>)> = candidates
            .iter()
            .map(|candidate| (candidate.path().as_str(), candidate.spans().to_vec()))
            .collect();
        assert_eq!(
            shape,
            [
                ("a.txt", vec![10..20]),
                ("b.txt", vec![0..40, 40..60]),
                ("c.txt", Vec::new()),
                ("d.txt", Vec::new()),
            ]
        );
    }

    /// The trigram index and the pending set hold file rows alone: the rows that store text.
    #[test]
    fn test_the_trigram_rows_are_the_rows_storing_text() {
        assert_eq!(TRIGRAM_ROWS, "file_content IS NOT NULL");
    }
}
