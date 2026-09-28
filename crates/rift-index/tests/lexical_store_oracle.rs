//! A lexical store kept in step with a changing tree by incremental writes holds, row for
//! row, what a cold build of the same tree holds.
//!
//! The store writes only the paths a change names, so its rows after a sequence of edits,
//! additions, deletions, reverts, and chunk splits are the sum of many small writes. The
//! oracle is independent of that path: a fresh database populated from the final tree in
//! one whole replacement. After every step the two dumps must be equal, FTS5's own
//! integrity check must pass, the document frequencies a body match reads must agree, and
//! every row of a path the step did not name must keep its row id, which proves the write
//! left it alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rift_core::{LanguageFileSelections, ProjectPath, SourceVisibility, TextFileInclusion};
use rift_index::{
    DatabasePool, LastCapture, LexicalChange, LexicalIndexLimits, LexicalSearchIndex, LexicalStamp,
    PathChanges, RevisionScoped, WorkspaceDatabase, WorkspaceIndex, WorkspaceIndexLimits,
    capture_digests_with_languages,
};
use rift_ranking::{BodyTerms, FileRowFrequencies, ParsedQuery};
use tempfile::TempDir;
use toasty::Db;
use toasty::stmt::{Type, Value};
use toasty_driver_sqlite::Sqlite;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The chunk bound every index here derives its rows under: the smallest the
/// configuration accepts, so a file of a few kilobytes splits into several chunk rows.
const CHUNK_BYTES_MAX: u64 = 1_024;

/// Steps of the seeded edit sequence that follows the scripted one.
const SEEDED_STEPS: usize = 12;

/// The words every step's frequencies are compared for.
const PROBED_TERMS: &str = "beacon lantern harbor relay signal";

/// A generator whose sequence one seed fixes, so a failing run replays exactly.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

fn text_inclusion() -> TextFileInclusion {
    TextFileInclusion::new(vec!["**".to_owned()], CHUNK_BYTES_MAX)
}

fn index_of(root: &Path) -> TestResult<WorkspaceIndex> {
    Ok(WorkspaceIndex::build_with_languages(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &text_inclusion(),
        &LanguageFileSelections::default(),
    )?)
}

fn pool() -> DatabasePool {
    DatabasePool::new(2, 5_000)
}

/// One row of the typed table as the oracle compares it: every column a write sets.
const DUMP_SQL: &str = "SELECT identity, path, kind, digest, byte_length, byte_offset, name, \
    qualified_name, identifier_terms, signature, documentation, file_content \
    FROM lexical_documents ORDER BY identity";

async fn probe(path: &Path) -> TestResult<Db> {
    Ok(Db::builder().build(Sqlite::open(path)).await?)
}

/// Every stored row, in identity order.
async fn dump(path: &Path) -> TestResult<Vec<Value>> {
    let database = probe(path).await?;
    let mut connection = database.connection().await?;
    let rows = toasty::sql::query(DUMP_SQL)
        .column_types([
            Type::String,
            Type::String,
            Type::String,
            Type::String,
            Type::I64,
            Type::I64,
            Type::String,
            Type::String,
            Type::String,
            Type::String,
            Type::String,
            Type::String,
        ])
        .exec(&mut connection)
        .await?;
    Ok(rows)
}

/// Each stored row's id by identity, for the rows filed under a path outside `changed`.
async fn untouched_row_ids(
    path: &Path,
    changed: &[ProjectPath],
) -> TestResult<BTreeMap<String, i64>> {
    let database = probe(path).await?;
    let mut connection = database.connection().await?;
    let rows = toasty::sql::query("SELECT id, identity, path FROM lexical_documents")
        .column_types([Type::I64, Type::String, Type::String])
        .exec(&mut connection)
        .await?;
    let mut ids = BTreeMap::new();
    for row in rows {
        let Value::Record(record) = row else {
            return Err("a row reads back as a record".into());
        };
        let [
            Value::I64(id),
            Value::String(identity),
            Value::String(row_path),
        ] = record.as_slice()
        else {
            return Err(format!("unexpected row shape: {record:?}").into());
        };
        if !changed.iter().any(|path| path.as_str() == row_path) {
            ids.insert(identity.clone(), *id);
        }
    }
    Ok(ids)
}

/// FTS5's own check of the word index against the rows it reads.
async fn assert_index_matches_rows(path: &Path) -> TestResult {
    let database = probe(path).await?;
    let mut connection = database.connection().await?;
    toasty::sql::statement(
        "INSERT INTO lexical_documents_fts(lexical_documents_fts, rank) \
         VALUES('integrity-check', 1)",
    )
    .exec(&mut connection)
    .await?;
    Ok(())
}

async fn frequencies(
    store: &LexicalSearchIndex,
    tree_revision: &str,
) -> TestResult<FileRowFrequencies> {
    let terms = BodyTerms::of(&ParsedQuery::parse(PROBED_TERMS)?);
    match store.file_row_frequencies(tree_revision, &terms).await? {
        RevisionScoped::Matched(frequencies) => Ok(frequencies),
        other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
    }
}

/// The incremental store and its current tree.
struct Kept {
    root: PathBuf,
    database: PathBuf,
    store: LexicalSearchIndex,
    index: WorkspaceIndex,
}

impl Kept {
    /// Captures the tree, writes what moved since the last step, and proves the store
    /// against a cold build of the same tree.
    async fn step(&mut self, label: &str) -> TestResult {
        let (captured, _) = capture_digests_with_languages(
            &self.root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &text_inclusion(),
            &LanguageFileSelections::default(),
            &LastCapture::default(),
        )?;
        let changes = PathChanges::between(&self.index.digests(), &captured);
        let next = self.index.rebuilt(&changes)?;
        let replaced: Vec<ProjectPath> = changes.paths().cloned().collect();
        let recorded = replaced
            .iter()
            .filter_map(|path| Some((path.clone(), next.digest(path)?)))
            .collect();
        let change = LexicalChange::new(replaced.clone(), next.index_documents_for(&replaced))
            .with_recorded(recorded);
        let kept_before = untouched_row_ids(&self.database, &replaced).await?;
        let revision = next.tree_revision();
        self.store
            .apply(&change, &LexicalStamp::published(&revision, "derivation"))
            .await?;
        self.index = next;
        assert_index_matches_rows(&self.database).await?;
        let kept_after = untouched_row_ids(&self.database, &replaced).await?;
        assert_eq!(
            kept_before, kept_after,
            "{label}: rows of paths the step did not name keep their ids"
        );
        self.assert_equals_a_cold_build(label, &revision).await
    }

    async fn assert_equals_a_cold_build(&self, label: &str, revision: &str) -> TestResult {
        let cold = TempDir::new()?;
        let cold_path = cold.path().join("db");
        let cold_store = LexicalSearchIndex::attached(
            WorkspaceDatabase::open(&cold_path, pool()).await?,
            LexicalIndexLimits::default(),
        );
        cold_store
            .replace_all(&index_of(&self.root)?.index_documents(), revision)
            .await?;
        assert_index_matches_rows(&cold_path).await?;
        assert_eq!(
            dump(&self.database).await?,
            dump(&cold_path).await?,
            "{label}: the incremental store equals a cold build row for row"
        );
        assert_eq!(
            frequencies(&self.store, revision).await?,
            frequencies(&cold_store, revision).await?,
            "{label}: both stores count the same file rows"
        );
        Ok(())
    }

    fn write(&self, relative: &str, text: &str) -> TestResult {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    fn remove(&self, relative: &str) -> TestResult {
        std::fs::remove_file(self.root.join(relative))?;
        Ok(())
    }
}

const LIB: &str = "pub fn beacon() -> u32 {\n    let lantern = 1;\n    lantern + 1\n}\n\n\
                   pub struct Relay;\n\nimpl Relay {\n    pub fn signal(&self) {}\n}\n";

fn long_text(word: &str, lines: usize) -> String {
    format!("the {word} line holds a harbor\n").repeat(lines)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incremental_store_equals_a_cold_build_of_the_same_tree() -> TestResult {
    let tree = TempDir::new()?;
    let root = tree.path().to_path_buf();
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(root.join("src/lib.rs"), LIB)?;
    std::fs::write(root.join("src/util.rs"), "pub fn harbor() {}\n")?;
    std::fs::write(root.join("notes.txt"), "a short relay note\n")?;
    std::fs::write(root.join("big.txt"), long_text("beacon", 120))?;
    std::fs::write(root.join("Cargo.lock"), "version = 4\n")?;
    let database_directory = TempDir::new()?;
    let database = database_directory.path().join("db");
    let store = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database, pool()).await?,
        LexicalIndexLimits::default(),
    );
    let index = index_of(&root)?;
    store
        .replace_all(&index.index_documents(), &index.tree_revision())
        .await?;
    let mut kept = Kept {
        root,
        database,
        store,
        index,
    };
    kept.assert_equals_a_cold_build("initial", &kept.index.tree_revision())
        .await?;

    kept.write("src/lib.rs", &LIB.replace("lantern + 1", "lantern + 2"))?;
    kept.step("edit a body").await?;
    kept.write("src/new.rs", "pub fn lantern() {}\n")?;
    kept.step("add a file").await?;
    kept.remove("src/util.rs")?;
    kept.step("delete a file").await?;
    kept.write("src/lib.rs", LIB)?;
    kept.step("revert the edit").await?;
    kept.write("notes.txt", &long_text("relay", 90))?;
    kept.step("grow a file past the chunk bound").await?;
    kept.write("big.txt", "now a short beacon\n")?;
    kept.step("shrink a chunked file").await?;
    kept.write(
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"beacon\"\n",
    )?;
    kept.step("edit a lockfile search leaves out").await?;

    let names = [
        "src/lib.rs",
        "src/new.rs",
        "notes.txt",
        "big.txt",
        "docs/guide.md",
    ];
    let words = ["beacon", "lantern", "harbor", "relay", "signal"];
    let mut random = SplitMix64(0x5eed_0f0e_c0de);
    for step in 0..SEEDED_STEPS {
        let name = names[random.below(names.len())];
        let word = words[random.below(words.len())];
        let lines = random.below(80);
        let label = format!("seeded step {step}: {name} as {lines} {word} lines");
        if lines == 0 && kept.root.join(name).exists() {
            kept.remove(name)?;
        } else {
            kept.write(name, &long_text(word, lines.max(1)))?;
        }
        kept.step(&label).await?;
    }
    Ok(())
}
