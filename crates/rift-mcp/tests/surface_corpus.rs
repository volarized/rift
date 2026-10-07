//! Request corpus and workspace shared by direct and proxy surface suites.

use std::fs;
use std::path::Path;

use crate::global_api::{GlobalFixture, SymbolFixture};
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
        ("get_symbol", json!({ "name": "beacon", "limit": 1 })),
        ("get_symbol", json!({ "name": "beacon", "include": [] })),
        // The fixture global API's collected package answers these: a package hit
        // carries `unit` in place of `path`.
        (
            "get_symbol",
            json!({ "name": "helper_beacon", "scope": "global" }),
        ),
        ("get_symbol", json!({ "name": "beacon", "scope": "all" })),
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
            "get_symbol",
            json!({ "name": "beacon", "limit": 1, "page_index": 50 }),
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
    requests
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
            "get_symbol",
            json!({
                "name": "helper_beacon",
                "scope": "global",
                "packages": [{ "manager": "cargo", "name": "helper", "version": "0.1.0" }]
            }),
        ),
        (
            "search",
            json!({
                "query": "helper_beacon",
                "scope": "all",
                "packages": [{ "manager": "cargo", "name": "extra" }]
            }),
        ),
        (
            "search",
            json!({
                "query": "beacon",
                "scope": "global",
                "packages": [{ "manager": "cargo", "name": "demo" }]
            }),
        ),
        (
            "search",
            json!({
                "pattern": "helper_beacon",
                "scope": "all",
                "packages": [{ "manager": "cargo", "name": "demo" }]
            }),
        ),
    ]
}

/// `traversal` requests over the one real call edge the fixture already carries -
/// `traversal_caller.py`'s `calls_callee` calling `traversal_callee.py`'s `callee` - so
/// this corpus proves the params without perturbing any other corpus entry's fixture
/// source. Incoming walks start at the callee and outgoing walks at the caller. The walks
/// the server refuses are proven by `traversal_refusal_corpus` below; a refusal has no
/// structured content to validate against this corpus's output schema.
fn traversal_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({
                "traversal": { "seed": TRAVERSAL_CALLEE }
            }),
        ),
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "direction": "incoming",
                    "depth": 2
                }
            }),
        ),
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "to": TRAVERSAL_CALLER
                }
            }),
        ),
        // The engine lane resolves references alone, so `implements` beside them has no
        // lane: this answer carries the `relationship_coverage_missing` warning and
        // validates the schema arm serving it.
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "facets": ["implements", "references"]
                }
            }),
        ),
        // The caller's call hierarchy names `callee` in the project and `len` in the
        // standard library the local index does not analyze, so the answer carries the
        // `callees_dropped` warning and validates the schema arm serving it.
        (
            "search",
            json!({
                "traversal": { "seed": TRAVERSAL_CALLER, "direction": "outgoing" }
            }),
        ),
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLER,
                    "direction": "outgoing",
                    "depth": 2,
                    "to": TRAVERSAL_CALLEE
                }
            }),
        ),
        (
            "search",
            json!({
                "query": "callee",
                "traversal": {
                    "seed": TRAVERSAL_CALLER,
                    "direction": "outgoing",
                    "facets": ["calls"]
                }
            }),
        ),
        // Call hierarchy names calls alone, so `references` beside them has no lane on an
        // outgoing walk.
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLER,
                    "direction": "outgoing",
                    "facets": ["references", "calls"]
                }
            }),
        ),
    ]
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
        let global = GlobalFixture::start(SymbolFixture::Valid).await?;
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
             def calls_callee() -> int:\n    return callee() + len(\"callee\")\n",
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
             [[dependencies.packages]]\nmanager = \"cargo\"\nname = \"demo\"\nversion = \"1.0.0\"\n\n\
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
