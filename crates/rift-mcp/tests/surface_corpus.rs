//! Request corpus and workspace shared by direct and proxy surface suites.

use std::fs;
use std::path::Path;

use crate::global_api::GlobalFixture;
use crate::hermetic_search;
use serde_json::{Value, json};

/// The engine used for reference resolution in corpus fixture.
const ENGINE: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

/// Sample validation corpus with various scenarios: one request per
/// advertised tool behavior worth proving.
pub(crate) fn corpus() -> Vec<(&'static str, Value)> {
    let mut requests = vec![
        ("get_symbol", json!({ "name": "beacon_one" })),
        ("search", json!({ "query": "beacon", "limit": 1 })),
        ("search", json!({ "query": "beacon", "include": [] })),
        // The fixture global API's collected package answers these: a package hit
        // carries `unit` in place of `path`.
        (
            "get_symbol",
            json!({ "name": "helper_beacon", "scope": "global" }),
        ),
        ("search", json!({ "query": "beacon", "scope": "all" })),
        // The fixture's committed baseline serves the timeline: one
        // `introduced` version from the walk's first commit.
        (
            "get_symbol",
            json!({ "name": "beacon_one", "include": ["history"] }),
        ),
        ("search", json!({ "query": "beacon" })),
        (
            "search",
            json!({ "query": "beacon", "include": ["source"] }),
        ),
        ("search", json!({ "query": "beacon", "include": ["score"] })),
        (
            "search",
            json!({
                "query": "beacon",
                "limit": 1,
                "paths": { "include": ["lib.rs"] }
            }),
        ),
        (
            "search",
            json!({ "query": "beacon", "limit": 1, "page_index": 100 }),
        ),
        (
            "search",
            json!({ "query": "beacon", "limit": 1, "page_index": 50 }),
        ),
        (
            "search",
            json!({ "query": "beacon", "paths": { "include": ["lib.rs"] } }),
        ),
        (
            "search",
            json!({
                "query": "phantom",
                "target": "symbol",
                "paths": { "force_include": ["hidden.rs"] }
            }),
        ),
        (
            "get_source",
            json!({ "unit": "rift://source/project/lib.rs" }),
        ),
        ("nodes", json!({ "path": "lib.rs", "position": 0 })),
        ("nodes", json!({ "path": "lib.rs", "position": 8 })),
        (
            "nodes",
            json!({ "path": "packages/@scope/name/package.json", "position": 4 }),
        ),
        (
            "nodes",
            json!({ "path": "notes for café.json", "position": 4 }),
        ),
    ];
    requests.extend(dependency_scope_search_corpus());
    requests.extend(package_argument_corpus());
    requests.extend(revision_read_corpus());
    requests.extend(change_search_corpus());
    requests.extend(commit_search_corpus());
    requests.extend(lexical_search_corpus());
    requests.extend(pattern_search_corpus());
    requests.extend(traversal_search_corpus());
    requests.extend([
        ("search", json!({"language":"sparql", "query":"ASK { ?symbol a :Symbol; :name 'beacon_one' }"})),
        ("search", json!({"language":"sparql", "query":"CONSTRUCT { ?symbol :name 'beacon_one' } WHERE { ?symbol a :Symbol; :name 'beacon_one' }"})),
        ("search", json!({"language":"sparql", "query":"SELECT (BNODE() AS ?blank) (<urn:fixture:term> AS ?iri) (1 AS ?number) ('hello'@en AS ?language) WHERE {}"})),
        ("search", json!({"language":"sparql", "query":format!("SELECT ?index {{ VALUES ?index {{ {} }} }} ORDER BY ?index", (0..1001).map(|index| index.to_string()).collect::<Vec<_>>().join(" "))})),
    ]);
    requests
        .into_iter()
        .map(|(name, request)| {
            if name != "search" || request["language"] == "sparql" {
                return (name, request);
            }
            let producer =
                serde_json::from_value(request).expect("static retrieval fixture parameters");
            let request = crate::workspace_client::query::retrieval(&producer)
                .expect("static retrieval query");
            (
                name,
                serde_json::to_value(request).expect("query parameters serialize"),
            )
        })
        .collect()
}

/// Resolves the corpus's original declarations through discovery before issuing exact reads.
pub(crate) async fn prepared_corpus(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Result<Vec<(&'static str, Value)>, Box<dyn std::error::Error>> {
    let mut requests = corpus();
    for (tool, request) in &mut requests {
        if *tool == "get_symbol" {
            let name = request["name"]
                .as_str()
                .ok_or("corpus declaration name")?
                .to_owned();
            let mut discovery =
                json!({"query": name, "target": "symbol", "include": ["source"], "limit": 1000});
            for field in ["scope", "packages", "rev"] {
                if let Some(value) = request.get(field) {
                    discovery[field] = value.clone();
                }
            }
            let id = discovered_id(client, &discovery, &name, None).await?;
            let object = request.as_object_mut().ok_or("corpus request object")?;
            for field in ["name", "scope", "packages"] {
                object.remove(field);
            }
            object.insert("id".to_owned(), json!(id));
        }
        if *tool == "search" {
            let query = request["query"]
                .as_str()
                .ok_or("query statement")?
                .to_owned();
            let mut expanded = query;
            for (placeholder, name, path) in [
                (TRAVERSAL_CALLEE, "callee", "traversal_callee.py"),
                (TRAVERSAL_CALLER, "calls_callee", "traversal_caller.py"),
            ] {
                if expanded.contains(placeholder) {
                    let identity =
                        crate::workspace_client::captured_seed(client, name, path).await?;
                    expanded = expanded.replace(placeholder, &identity);
                }
            }
            request["query"] = json!(expanded);
        }
    }
    Ok(requests)
}

async fn discovered_id(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    request: &Value,
    name: &str,
    path: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if request["scope"]
        .as_str()
        .is_none_or(|scope| scope == "local")
    {
        if let Some(path) = path.or((name == "beacon_one").then_some("lib.rs")) {
            return crate::workspace_client::captured_seed(client, name, path).await;
        }
    }
    let producer = serde_json::from_value(request.clone())?;
    let request = crate::workspace_client::query::retrieval(&producer)?;
    let request = serde_json::to_value(request)?;
    let result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("search")
                .with_arguments(request.as_object().ok_or("discovery object")?.clone()),
        )
        .await?;
    assert!(
        !result.is_error.unwrap_or(false),
        "discovery refused: {result:?}"
    );
    let answer = result.structured_content.ok_or("discovery content")?;
    let mut rows = crate::workspace_client::query::rows(&answer)?
        .iter()
        .filter(|row| {
            crate::workspace_client::query::literal(row, "name").ok() == Some(name)
                && path.is_none_or(|path| {
                    crate::workspace_client::query::literal(row, "path").ok() == Some(path)
                })
        });
    let row = rows
        .next()
        .ok_or_else(|| format!("original declaration {name}: {answer}"))?;
    assert!(
        rows.next().is_none(),
        "one original declaration {name}: {answer}"
    );
    assert!(
        !crate::workspace_client::query::literal(row, "source")?.is_empty(),
        "original source: {row}"
    );
    assert!(
        crate::workspace_client::query::literal(row, "start")?.parse::<u64>()?
            < crate::workspace_client::query::literal(row, "end")?.parse::<u64>()?,
        "original range: {row}"
    );
    let id = crate::workspace_client::query::iri(row, "symbol")?;
    let _ = rift_protocol::identity::SymbolIdentity::parse(id)?;
    Ok(id.to_owned())
}

/// Commit searches over the fixture's two commit messages. The history store fills in the
/// background, so an answer holds the commits it analyzed so far;
/// `a_commit_search_hit_validates_against_the_served_output_schema` waits for a hit.
fn commit_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({ "target": "commit", "query": "witness" })),
        (
            "search",
            json!({ "target": "commit", "query": "fixture change", "limit": 1 }),
        ),
    ]
}

/// Regex `pattern` searches: file and symbol hits verified from the trigram candidates,
/// the matched line as `source`, a `force_include` file verified whole, and the package
/// matches a `scope` past `local` adds.
fn pattern_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({ "pattern": "beacon" })),
        (
            "search",
            json!({ "pattern": r"fn beacon_\w+\(", "target": "file", "include": ["source"] }),
        ),
        (
            "search",
            json!({ "pattern": "(?i)BEACON_ONE", "target": "symbol" }),
        ),
        (
            "search",
            json!({
                "pattern": "phantom",
                "target": "file",
                "paths": { "force_include": ["hidden.rs"] }
            }),
        ),
        ("search", json!({ "pattern": "beacon", "scope": "all" })),
        (
            "search",
            json!({
                "pattern": r"fn \w*beacon",
                "scope": "global",
                "target": "all",
                "include": ["source"]
            }),
        ),
    ]
}

/// `scope` on `search` reaches the same collected package `get_symbol`'s scoped requests
/// reach: a package hit carries `unit` in place of `path`, `all` merges it with the
/// project hits, and a `file` target answers empty from the package side since a package
/// contributes declarations alone.
fn dependency_scope_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({ "query": "helper_beacon", "scope": "global" }),
        ),
        (
            "search",
            json!({ "query": "beacon", "scope": "all", "include": ["source"] }),
        ),
        (
            "search",
            json!({
                "query": "beacon",
                "scope": "global",
                "target": "file"
            }),
        ),
    ]
}

/// `packages` requests: one replacing the version of the fixture's path dependency
/// `helper`, which then answers `package_absent` in place of `package_unavailable`, one
/// adding `extra`, a package the context lacks, which answers
/// `package_requirement_absent`, one replacing the collected `demo` by the requirement
/// `>=0`, which the fixture global API resolves to its collected release, and a `pattern`
/// matched over that release beside the project.
fn package_argument_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({
                "query": "helper_beacon",
                "scope": "global",
                "packages": [{ "manager": "cargo", "registry": "crates.io", "name": "helper", "version": "0.1.0" }]
            }),
        ),
        (
            "search",
            json!({
                "query": "helper_beacon",
                "scope": "all",
                "packages": [{ "manager": "cargo", "registry": "crates.io", "name": "extra" }]
            }),
        ),
        (
            "search",
            json!({
                "query": "beacon",
                "scope": "global",
                "packages": [{ "manager": "cargo", "registry": "crates.io", "name": "demo" }]
            }),
        ),
        (
            "search",
            json!({
                "pattern": "helper_beacon",
                "scope": "all",
                "packages": [{ "manager": "cargo", "registry": "crates.io", "name": "demo" }]
            }),
        ),
    ]
}

/// Physical usages, incoming references and outgoing calls use standard graph patterns.
fn traversal_search_corpus() -> Vec<(&'static str, Value)> {
    [
        format!("SELECT ?usage ?unit ?start ?end {{ <{TRAVERSAL_CALLEE}> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end }} ORDER BY ?unit ?start"),
        format!("SELECT DISTINCT ?caller {{ ?caller :references <{TRAVERSAL_CALLEE}> }}"),
        format!("SELECT DISTINCT ?caller {{ ?caller :references+ <{TRAVERSAL_CALLEE}> }}"),
        format!("SELECT ?caller {{ ?caller :references <{TRAVERSAL_CALLEE}> FILTER(?caller = <{TRAVERSAL_CALLER}>) }}"),
        format!("SELECT ?callee {{ <{TRAVERSAL_CALLER}> :calls ?callee }}"),
        format!("SELECT ?callee {{ <{TRAVERSAL_CALLER}> :calls/:calls ?callee }}"),
        format!("SELECT ?callee {{ <{TRAVERSAL_CALLER}> :calls ?callee FILTER(?callee = <{TRAVERSAL_CALLEE}>) }}"),
        format!("SELECT ?callee {{ SERVICE :search {{ ?hit :query 'callee'; :symbol ?callee }} <{TRAVERSAL_CALLER}> :calls ?callee }}"),
    ].into_iter().map(|query| ("search", json!({"query":query, "language":"sparql"}))).collect()
}

/// The declaration every walk in this corpus starts at.
pub(crate) const TRAVERSAL_CALLEE: &str = "rift://symbol/python/traversal_callee.py/callee";
/// The one declaration referencing it, and the seed of every outgoing walk.
pub(crate) const TRAVERSAL_CALLER: &str = "rift://symbol/python/traversal_caller.py/calls_callee";

/// Search requests only the lexical search-index tier can fully answer: a multi-word
/// prose query merging in hits identifier search alone would not surface, and a query
/// that only `notes.txt` answers, since identifier search never reaches its content.
fn lexical_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({ "query": "beacon two three" })),
        ("search", json!({ "query": "rotating legacy sensor unit" })),
    ]
}

/// Revision-addressed requests, one per read tool: each answers from the
/// fixture's committed baseline.
fn revision_read_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("get_symbol", json!({ "name": "beacon_one", "rev": "main" })),
        // An ancestry suffix names the fixture's baseline, one commit below `HEAD`.
        (
            "get_symbol",
            json!({ "name": "beacon_one", "rev": "HEAD^" }),
        ),
        ("search", json!({ "query": "beacon", "rev": "main" })),
        (
            "nodes",
            json!({ "path": "lib.rs", "position": 0, "rev": "main" }),
        ),
    ]
}

/// A comparison of the fixture's two committed revisions, then of `baseline` against the
/// working tree, which holds `HEAD`'s bytes: the `baseline` tag holds everything before
/// `change_witness.rs` arrived, so each answer carries the two `introduced` hits that file
/// brought and validates the `change` arm of the served output schema against a real
/// payload, and the second validates the working-tree `head` against the input schema.
///
fn change_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({
                "change": { "base": "baseline", "head": "HEAD" },
                "include": ["source"]
            }),
        ),
        (
            "search",
            json!({
                "change": { "base": "baseline", "head": { "kind": "working_tree" } },
                "include": ["source"]
            }),
        ),
        (
            "search",
            json!({ "change": { "base": "HEAD~1", "head": "HEAD" } }),
        ),
    ]
}

/// A v4 lockfile naming fixture and its path dependency.
const LOCK_WITH_HELPER: &str = "\
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = \"fixture\"
version = \"0.1.0\"
dependencies = [
 \"helper\",
]

[[package]]
name = \"helper\"
version = \"0.1.0\"
";

/// The workspace and global API used by the surface corpus.
pub(crate) struct SurfaceFixture {
    workspace: tempfile::TempDir,
    _global: GlobalFixture,
}

impl SurfaceFixture {
    /// Builds fixture files, Git history, and global API for each corpus request.
    pub(crate) async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let global = GlobalFixture::start_with(crate::global_api::FixtureOptions {
            query_narrowed: false,
            ..crate::global_api::FixtureOptions::default()
        })
        .await?;
        let directory = tempfile::tempdir()?;
        // The manifest names the helper by a path outside the workspace, which no index
        // serves, so every package-scoped answer names it `package_unavailable`.
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"lib.rs\"\n\n[dependencies]\nhelper = { path = \"../helper\" }\n",
        )?;
        fs::write(directory.path().join("Cargo.lock"), LOCK_WITH_HELPER)?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon_one() {}\npub fn beacon_two() {}\npub fn beacon_three() {}\n",
        )?;
        // Gitignored, so a plain search never reaches it; `paths.force_include` is the only way
        // in, proving that arm of the surface end to end.
        fs::write(directory.path().join(".gitignore"), "hidden.rs\n")?;
        fs::write(
            directory.path().join("hidden.rs"),
            "pub fn phantom_signal() {}\n",
        )?;
        // Included into the lexical search-index tier by the default `[search.text]`
        // extensions, so the corpus can prove a text-file search hit end to end.
        fs::write(
            directory.path().join("notes.txt"),
            "Beacon telemetry guidance covers rotating every legacy sensor unit safely.\n",
        )?;
        // The one real call edge the traversal corpus walks. A configured language engine
        // resolves the references a walk follows, and this fixture selects the embedded `ty`
        // engine, so the pair is Python.
        fs::write(
            directory.path().join("traversal_callee.py"),
            "def callee() -> int:\n    return 1\n",
        )?;
        fs::write(
            directory.path().join("traversal_caller.py"),
            "from traversal_callee import callee\n\n\n\
             def calls_callee() -> int:\n    return callee()\n",
        )?;
        fs::write(
            directory.path().join("pyproject.toml"),
            "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['traversal_callee','traversal_caller']\n",
        )?;
        // No syntax provider claims it; `nodes` names the missing extension.
        fs::write(directory.path().join("justfile"), "default:\n    echo hi\n")?;
        // An npm scoped package directory, so the corpus mints an identity whose path holds the
        // `@` RFC 3986 keeps literal. The served `NodeId` pattern left `@` out and refused the
        // identity the server had just minted.
        fs::create_dir_all(directory.path().join("packages/@scope/name"))?;
        fs::write(
            directory.path().join("packages/@scope/name/package.json"),
            "{\n  \"name\": \"@scope/name\"\n}\n",
        )?;
        // A path carrying bytes `encode_path` escapes rather than keeps, so the corpus validates a
        // percent-encoded identity against the served pattern beside the literal one above.
        fs::write(
            directory.path().join("notes for café.json"),
            "{\n  \"beacon\": \"escaped\"\n}\n",
        )?;
        // A committed baseline, so the corpus can prove revision-addressed reads:
        // `hidden.rs` stays gitignored and uncommitted, everything else lands in
        // the fixture's one commit on `main`.
        //
        // The package list names the collected release, so the fixture global API serves
        // the package hits every `global` and `all` request in the corpus reaches.
        //
        // A commit hit exists only once the background fill has analyzed and written the
        // fixture's commits. At the default share of one core the history task rests three
        // times as long as each commit parsed, so the history store fills at the whole core:
        // the corpus proves the served schemas, and the rest only stretches its wait.
        let configuration = format!(
            "{}\n[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\n\
             [[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"demo\"\nversion = \"1.0.0\"\n\n\
             [providers.history]\ncpu_share = 1.0\n\n\
             {ENGINE}",
            hermetic_search::HERMETIC_TABLES,
            global.endpoint
        );
        fs::write(directory.path().join("rift.toml"), configuration)?;
        rift_history::fixture::init(directory.path());
        rift_history::fixture::commit_all(directory.path(), "fixture baseline");
        // A second commit, tagged apart from it, so a `change` request compares two
        // committed revisions that really differ.
        rift_history::fixture::git(directory.path(), &["tag", "baseline"]);
        // Both the declaration and its caller arrive in this commit and live in one file, so
        // the comparison answers two `introduced` hits.
        fs::write(
            directory.path().join("change_witness.rs"),
            "pub fn change_witness() {}\npub fn calls_change_witness() {\n    change_witness();\n}\n",
        )?;
        rift_history::fixture::commit_all(directory.path(), "introduce the change witness");
        Ok(Self {
            workspace: directory,
            _global: global,
        })
    }

    /// Returns workspace root used by direct and proxy clients.
    pub(crate) fn root(&self) -> &Path {
        self.workspace.path()
    }
}
