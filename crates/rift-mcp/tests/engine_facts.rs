//! Facts a language engine adds to static analysis, per language and per edge kind.
//!
//! Each fixture holds sites whose target static analysis cannot name from the written
//! source alone: a receiver typed by a list element, a callback parameter typed by the
//! array it runs over, an inferred return or object type, a closure parameter typed
//! through an iterator, an implementation selected by the expected type. The suite serves
//! every fixture twice, without and with an engine, and states one rule in numbers: calls,
//! references and type uses hold more facts with the engine wherever the fixture holds such
//! a site, no static fact leaves the answer, heritage and structural edges stay equal, and
//! every fact the engine adds names that engine as its provider.
//!
//! Python runs the embedded `ty` engine. TypeScript and Rust replay answers recorded from
//! typescript-language-server and rust-analyzer for these exact fixture bytes
//! (`fixtures/engine_facts/*/recorded.sh`), so the default tier installs neither program. A
//! request the recording does not hold answers `null`. The `live_` cases drive the real
//! servers under `RIFT_ENGINE_LIVE` and check the same rule without the recorded counts.

mod hermetic_search;
#[path = "../../rift-lsp/tests/live_engine_gate.rs"]
mod live_engine_gate;
#[cfg(unix)]
#[path = "../../rift-lsp/tests/typescript_install.rs"]
mod typescript_install;
#[allow(
    dead_code,
    reason = "this suite serves plain and prepared roots and reads through SPARQL alone"
)]
mod workspace_client;

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::fmt::Write as _;

use rift_protocol::query::QueryParams;
use serde_json::Value;
use workspace_client::{TestResult, query, query_after_population, served_workspace};

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;
type Pair = (String, String);

/// Edge kinds an engine can add to: the SPARQL predicate and the `:kinds` value its edge
/// records carry.
const ENGINE_KINDS: [(&str, &str); 5] = [
    ("calls", "calls"),
    ("references", "references"),
    ("hasType", "has_type"),
    ("extends", "extends"),
    ("implements", "implements"),
];

/// Edge kinds static analysis alone establishes: an engine neither adds nor removes one.
const STRUCTURAL_KINDS: [&str; 5] = ["contains", "imports", "reads", "writes", "overrides"];

/// Reads one live answer may take while its engine loads the fixture, one second apart:
/// below the 60 seconds a test has, so a slow load fails with its own message.
const LIVE_ATTEMPTS: usize = 45;

const PYTHON_FILES: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        include_str!("fixtures/engine_facts/python/pyproject.toml"),
    ),
    (
        "shapes.py",
        include_str!("fixtures/engine_facts/python/shapes.py"),
    ),
    (
        "main.py",
        include_str!("fixtures/engine_facts/python/main.py"),
    ),
];
const PYTHON_OWNER: &str = "rift://symbol/local/python/";
const PYTHON_EMBEDDED: &str = "[languages.python.lsp]\nembedded = \"ty\"\n";
const PYTHON_LIVE: &str =
    "[languages.python.lsp]\ncommand = [\"uvx\", \"ty@0.0.103\", \"server\"]\n";

#[cfg(unix)]
const TYPESCRIPT_SOURCES: &[(&str, &str)] = &[
    (
        "tsconfig.json",
        include_str!("fixtures/engine_facts/typescript/tsconfig.json"),
    ),
    (
        "src/shapes.ts",
        include_str!("fixtures/engine_facts/typescript/src/shapes.ts"),
    ),
    (
        "src/main.ts",
        include_str!("fixtures/engine_facts/typescript/src/main.ts"),
    ),
];
#[cfg(unix)]
const TYPESCRIPT_PACKAGE: (&str, &str) = (
    "package.json",
    include_str!("fixtures/engine_facts/typescript/package.json"),
);
#[cfg(unix)]
const TYPESCRIPT_OWNER: &str = "rift://symbol/local/typescript/.%2Fsrc%2F";
/// The live server's packages are installed into the workspace and left out of the index.
#[cfg(unix)]
const TYPESCRIPT_LIVE_SOURCES: &str = "[source]\nexclude = [\"node_modules/**\"]\n";
#[cfg(unix)]
const TYPESCRIPT_RECORDED: &str = include_str!("fixtures/engine_facts/typescript/recorded.sh");

const RUST_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("fixtures/engine_facts/rust/Cargo.toml"),
    ),
    (
        "src/lib.rs",
        include_str!("fixtures/engine_facts/rust/src/lib.rs"),
    ),
];
const RUST_OWNER: &str = "rift://symbol/local/rust/fixture/";
#[cfg(unix)]
const RUST_RECORDED: &str = include_str!("fixtures/engine_facts/rust/recorded.sh");
const RUST_LIVE: &str =
    "[languages.rust.lsp]\ncommand = [\"rustup\", \"run\", \"1.98\", \"rust-analyzer\"]\n";

/// The facts one served workspace answers: per engine-capable kind each pair with the
/// providers its edge records name, and per structural kind its pairs.
#[derive(Debug, Default)]
struct Facts {
    edges: BTreeMap<&'static str, BTreeMap<Pair, BTreeSet<String>>>,
    structural: BTreeMap<&'static str, BTreeSet<Pair>>,
}

impl Facts {
    fn pairs(&self, kind: &str) -> BTreeSet<&Pair> {
        self.edges
            .get(kind)
            .map(|pairs| pairs.keys().collect())
            .unwrap_or_default()
    }

    fn providers(&self, kind: &str, from: &str, to: &str) -> Vec<&str> {
        self.edges
            .get(kind)
            .and_then(|pairs| pairs.get(&(from.to_owned(), to.to_owned())))
            .map(|providers| providers.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

async fn select(client: &Client, text: String) -> TestResult<Value> {
    query_after_population(
        client,
        &QueryParams {
            query: text,
            max_results: 1000,
            ..Default::default()
        },
    )
    .await
}

fn endpoint(row: &Value, field: &str, owner: &str) -> TestResult<Option<String>> {
    if row[field]["type"] != "iri" {
        return Ok(None);
    }
    let identity = query::iri(row, field)?;
    Ok(Some(
        identity.strip_prefix(owner).unwrap_or(identity).to_owned(),
    ))
}

/// Reads every engine-capable and structural kind through the whole inventory.
///
/// Each engine-capable kind is read twice: once as plain pairs, the facts an agent reads,
/// and once through its edge records, which name the provider of each fact.
async fn facts(client: &Client, owner: &str) -> TestResult<Facts> {
    let mut read = Facts::default();
    for (predicate, kind) in ENGINE_KINDS {
        let plain = select(
            client,
            format!("SELECT ?s ?t WHERE {{ ?s :{predicate} ?t }}"),
        )
        .await?;
        let mut pairs = BTreeMap::new();
        for row in query::rows(&plain)? {
            if let (Some(from), Some(to)) = (endpoint(row, "s", owner)?, endpoint(row, "t", owner)?)
            {
                pairs.insert((from, to), BTreeSet::new());
            }
        }
        let records = select(
            client,
            format!(
                "SELECT DISTINCT ?s ?t ?p WHERE {{ ?s :{predicate} ?t . ?e a :Edge; :from ?s; \
                 :to ?t; :kinds :{kind}; :provider ?p }}"
            ),
        )
        .await?;
        for row in query::rows(&records)? {
            if let (Some(from), Some(to)) = (endpoint(row, "s", owner)?, endpoint(row, "t", owner)?)
            {
                let provider = query::literal(row, "p")?.to_owned();
                pairs
                    .get_mut(&(from.clone(), to.clone()))
                    .ok_or_else(|| {
                        format!("{predicate}: an edge record {from} -> {to} has no plain fact")
                    })?
                    .insert(provider);
            }
        }
        read.edges.insert(predicate, pairs);
    }
    for predicate in STRUCTURAL_KINDS {
        let plain = select(
            client,
            format!("SELECT ?s ?t WHERE {{ ?s :{predicate} ?t }}"),
        )
        .await?;
        let mut pairs = BTreeSet::new();
        for row in query::rows(&plain)? {
            if let (Some(from), Some(to)) = (endpoint(row, "s", owner)?, endpoint(row, "t", owner)?)
            {
                pairs.insert((from, to));
            }
        }
        read.structural.insert(predicate, pairs);
    }
    Ok(read)
}

/// Serves `files` and reads their facts, with the engine `configuration` names when given.
async fn served_facts(
    files: &[(&str, &str)],
    configuration: Option<String>,
    owner: &str,
) -> TestResult<Facts> {
    let (_directory, client, server_task) = served_workspace(files, configuration).await?;
    let read = facts(&client, owner).await;
    client.cancel().await?;
    server_task.await?;
    read
}

/// Waits until a live engine answers the call inventory, then reads the facts.
async fn live_facts(client: &Client, owner: &str) -> TestResult<Facts> {
    for _attempt in 0..LIVE_ATTEMPTS {
        let answer = select(client, "SELECT ?s ?t WHERE { ?s :calls ?t }".to_owned()).await?;
        let loading = answer["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|warning| {
                warning["code"] == "engine_analysis_unavailable"
                    && warning["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("analyzing"))
            });
        if !loading {
            return facts(client, owner).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Err("the live engine was still analyzing after every attempt".into())
}

/// Checks the rule every engine answer keeps against the static answer of the same
/// sources, and returns `(kind, static facts, engine facts)` per engine-capable kind.
///
/// - No static fact leaves the answer.
/// - Each fact the engine adds names `provider` alone.
/// - Structural kinds are equal.
fn assert_engine_rule(
    static_facts: &Facts,
    engine: &Facts,
    provider: &str,
) -> Vec<(&'static str, usize, usize)> {
    let mut counts = Vec::new();
    for (predicate, _) in ENGINE_KINDS {
        let held = static_facts.pairs(predicate);
        let answered = engine.pairs(predicate);
        let lost: Vec<_> = held.difference(&answered).collect();
        assert!(
            lost.is_empty(),
            "{predicate}: the engine answer lost static facts {lost:?}"
        );
        for pair in answered.difference(&held) {
            assert_eq!(
                engine.providers(predicate, &pair.0, &pair.1),
                [provider],
                "{predicate}: {pair:?} is absent from the static answer, so the engine alone \
                 provides it"
            );
        }
        counts.push((predicate, held.len(), answered.len()));
    }
    for predicate in STRUCTURAL_KINDS {
        assert_eq!(
            static_facts.structural.get(predicate),
            engine.structural.get(predicate),
            "{predicate}: an engine neither adds nor removes a structural edge"
        );
    }
    counts
}

/// Requires more engine facts than static facts for each of `kinds`.
fn assert_more(counts: &[(&'static str, usize, usize)], kinds: &[&str]) {
    for (predicate, held, answered) in counts {
        if kinds.contains(predicate) {
            assert!(
                answered > held,
                "{predicate}: the engine answer holds {answered} facts, the static answer {held}"
            );
        }
    }
}

/// The embedded Python engine types the element of a list literal and the receiver a list
/// method returns, so two calls of `area` join the static calls; every static call stays.
/// Unannotated `make` and `registry` return values stay untyped for the engine too, so
/// `total` and `lookup` gain no call of `area`.
#[tokio::test]
async fn python_embedded_engine_adds_facts_to_the_static_answer() -> TestResult {
    let static_facts = served_facts(PYTHON_FILES, None, PYTHON_OWNER).await?;
    let engine = served_facts(PYTHON_FILES, Some(PYTHON_EMBEDDED.to_owned()), PYTHON_OWNER).await?;
    let counts = assert_engine_rule(&static_facts, &engine, "engine");
    assert_eq!(
        counts,
        [
            ("calls", 9, 14),
            ("references", 21, 29),
            ("hasType", 10, 10),
            ("extends", 2, 2),
            ("implements", 0, 0),
        ]
    );
    assert_more(&counts, &["calls", "references"]);
    for (from, to) in [
        ("main/each", "shapes/Square/area"),
        ("main/chained", "shapes/Circle/area"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["engine"]);
        assert_eq!(engine.providers("references", from, to), ["engine"]);
    }
    assert_eq!(
        engine.providers("calls", "main/each", "shapes/Square"),
        ["engine", "static"],
        "a call both analyses establish names both"
    );
    for class in ["shapes/Square", "shapes/Circle"] {
        assert_eq!(
            engine.providers("extends", class, "shapes/Shape"),
            ["engine", "static"]
        );
        assert_eq!(
            engine.providers("hasType", class, "shapes/Shape"),
            ["engine", "static"]
        );
    }
    Ok(())
}

/// The recorded TypeScript server types an inferred return value, an object literal
/// member, a loop variable and an array callback parameter: four calls of `area`, and the
/// inferred types of five bindings, join the static answer.
#[cfg(unix)]
#[tokio::test]
async fn typescript_recorded_server_adds_facts_to_the_static_answer() -> TestResult {
    let mut files = TYPESCRIPT_SOURCES.to_vec();
    files.push(TYPESCRIPT_PACKAGE);
    let engine_directory = tempfile::tempdir()?;
    let configuration = recorded_engine(
        engine_directory.path(),
        TYPESCRIPT_RECORDED,
        &["typescript"],
    )?;
    let static_facts = served_facts(&files, None, TYPESCRIPT_OWNER).await?;
    let engine = served_facts(&files, Some(configuration), TYPESCRIPT_OWNER).await?;
    let counts = assert_engine_rule(&static_facts, &engine, "lsp");
    assert_eq!(
        counts,
        [
            ("calls", 6, 13),
            ("references", 18, 23),
            ("hasType", 2, 7),
            ("extends", 0, 0),
            ("implements", 2, 2),
        ]
    );
    assert_more(&counts, &["calls", "references", "hasType"]);
    for (from, to) in [
        ("main.ts/total", "shapes.ts/Square/area"),
        ("main.ts/each", "shapes.ts/Square/area"),
        ("main.ts/lookup", "shapes.ts/Circle/area"),
        ("main.ts/mapped", "shapes.ts/Circle/area"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["lsp"]);
        assert_eq!(
            engine.providers("references", from, to),
            ["lsp"],
            "a reference from another file is resolved in its opened document"
        );
    }
    for (from, to) in [
        ("main.ts/fallback", "shapes.ts/Square"),
        ("main.ts/total/shape", "shapes.ts/Square"),
        ("main.ts/each/shape", "shapes.ts/Square"),
        ("shapes.ts/make", "shapes.ts/Square"),
        ("shapes.ts/registry/circle", "shapes.ts/Circle"),
    ] {
        assert_eq!(engine.providers("hasType", from, to), ["lsp"]);
    }
    for class in ["shapes.ts/Square", "shapes.ts/Circle"] {
        assert_eq!(
            engine.providers("implements", class, "shapes.ts/Shape"),
            ["lsp", "static"]
        );
    }
    Ok(())
}

/// The recorded Rust server types a closure parameter through `Iterator::map` and
/// `Option::map_or`, and selects `Square`'s `Default` implementation from the declared type
/// of the binding: three calls join the static answer. Every item type is written in Rust,
/// so type uses stay equal.
#[cfg(unix)]
#[tokio::test]
async fn rust_recorded_server_adds_facts_to_the_static_answer() -> TestResult {
    let engine_directory = tempfile::tempdir()?;
    let configuration = recorded_engine(engine_directory.path(), RUST_RECORDED, &["rust"])?;
    let static_facts = served_facts(RUST_FILES, None, RUST_OWNER).await?;
    let engine = served_facts(RUST_FILES, Some(configuration), RUST_OWNER).await?;
    let counts = assert_engine_rule(&static_facts, &engine, "lsp");
    assert_eq!(
        counts,
        [
            ("calls", 6, 9),
            ("references", 16, 19),
            ("hasType", 9, 9),
            ("extends", 0, 0),
            ("implements", 3, 3),
        ]
    );
    assert_more(&counts, &["calls", "references"]);
    let reads = static_facts.structural.get("reads").expect("static reads");
    for source in ["total", "Square/default"] {
        assert!(
            reads
                .iter()
                .any(|(from, to)| from == source && to == "Square"),
            "{source} reads the written Square token"
        );
    }
    for (from, to) in [
        ("each", "Circle/area"),
        ("first", "Circle/area"),
        ("inferred", "Square/default"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["lsp"]);
        assert_eq!(engine.providers("references", from, to), ["lsp"]);
    }
    assert_eq!(
        engine.providers("calls", "total", "Square/area"),
        ["lsp", "static"],
        "a call both analyses establish names both"
    );
    for implementor in ["Square", "Circle"] {
        assert_eq!(
            engine.providers("implements", implementor, "Shape"),
            ["lsp", "static"],
            "the server names the implementing type of the impl header"
        );
    }
    Ok(())
}

/// Writes the recorded answers into `directory` and names them as the engine of each of
/// `languages`.
#[cfg(unix)]
fn recorded_engine(
    directory: &std::path::Path,
    script: &str,
    languages: &[&str],
) -> TestResult<String> {
    let path = directory.join("recorded.sh");
    std::fs::write(&path, script)?;
    let command = serde_json::to_string(&path)?;
    let mut configuration = String::new();
    for language in languages {
        writeln!(
            configuration,
            "[languages.{language}.lsp]\ncommand = [\"sh\", {command}]"
        )?;
    }
    Ok(configuration)
}

/// `ty server` adds the same calls of `area` the embedded engine adds, as `lsp` facts.
#[tokio::test]
async fn live_ty_server_adds_facts_to_the_static_answer() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let static_facts = served_facts(PYTHON_FILES, None, PYTHON_OWNER).await?;
    let (_directory, client, server_task) =
        served_workspace(PYTHON_FILES, Some(PYTHON_LIVE.to_owned())).await?;
    let engine = live_facts(&client, PYTHON_OWNER).await?;
    client.cancel().await?;
    server_task.await?;
    let counts = assert_engine_rule(&static_facts, &engine, "lsp");
    assert_more(&counts, &["calls", "references"]);
    for (from, to) in [
        ("main/each", "shapes/Square/area"),
        ("main/chained", "shapes/Circle/area"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["lsp"]);
    }
    Ok(())
}

/// typescript-language-server adds the calls and inferred types the recording holds.
#[cfg(unix)]
#[tokio::test]
async fn live_typescript_language_server_adds_facts_to_the_static_answer() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let mut files = typescript_install::typescript_package_files().to_vec();
    files.extend_from_slice(TYPESCRIPT_SOURCES);
    let static_facts = served_facts(
        &files,
        Some(TYPESCRIPT_LIVE_SOURCES.to_owned()),
        TYPESCRIPT_OWNER,
    )
    .await?;
    let configuration = format!(
        "{TYPESCRIPT_LIVE_SOURCES}\n[languages.typescript.lsp]\ncommand = [\"{}\", \"--stdio\"]\n\
         initialization_options = {{ tsserver = {{ useSyntaxServer = \"never\" }} }}\n",
        typescript_install::LANGUAGE_SERVER_PROGRAM
    );
    let (_directory, client, server_task) = workspace_client::served_prepared_workspace(
        &files,
        Some(configuration),
        typescript_install::install_typescript_engine,
    )
    .await?;
    let engine = live_facts(&client, TYPESCRIPT_OWNER).await?;
    client.cancel().await?;
    server_task.await?;
    let counts = assert_engine_rule(&static_facts, &engine, "lsp");
    assert_more(&counts, &["calls", "references", "hasType"]);
    for (from, to) in [
        ("main.ts/total", "shapes.ts/Square/area"),
        ("main.ts/lookup", "shapes.ts/Circle/area"),
        ("main.ts/mapped", "shapes.ts/Circle/area"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["lsp"]);
    }
    Ok(())
}

/// rust-analyzer adds the closure-parameter calls and the selected `Default` call.
#[tokio::test]
async fn live_rust_analyzer_adds_facts_to_the_static_answer() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let static_facts = served_facts(RUST_FILES, None, RUST_OWNER).await?;
    let (_directory, client, server_task) =
        served_workspace(RUST_FILES, Some(RUST_LIVE.to_owned())).await?;
    let engine = live_facts(&client, RUST_OWNER).await?;
    client.cancel().await?;
    server_task.await?;
    let counts = assert_engine_rule(&static_facts, &engine, "lsp");
    assert_more(&counts, &["calls", "references"]);
    for (from, to) in [
        ("each", "Circle/area"),
        ("first", "Circle/area"),
        ("inferred", "Square/default"),
    ] {
        assert_eq!(engine.providers("calls", from, to), ["lsp"]);
    }
    Ok(())
}
