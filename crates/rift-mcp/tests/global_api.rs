//! A fixture global API on a loopback port.
//!
//! It answers the capability, resolution, symbol, nodes, search, and pattern endpoints
//! `rift-cloud-client` reads, and records every request it receives. Its collection holds
//! one package, [`COLLECTED`], whose `src/lib.rs` declares [`DECLARATIONS`]; a symbol
//! request answers the declaration it names, a nodes request that declaration's syntax
//! nodes, a search request the declarations its
//! identifiers match, and a pattern request the matches of its pattern in that file, so
//! each answer passes the client's own validation. The
//! collected release also answers, as the nearest release, an exact `demo` entry at another
//! version and every `demo` requirement.
//!
//! A fixture started with [`FixtureOptions::python_collection`] also holds
//! [`PYTHON_STANDARD_LIBRARY`] and [`GREETING`], and names the declarations
//! [`NAMED_POSITIONS`] lists at a position in their files, as the declaration endpoint does
//! for the callees an outgoing walk meets.

#[path = "python_runtime.rs"]
pub(crate) mod python_runtime;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rift_ranking::IdentifierMatchClass;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

/// The package the fixture collection holds, as `manager`, `name`, and `version`.
pub(crate) const COLLECTED: (&str, &str, &str) = ("cargo", "demo", "1.0.0");

/// The source unit every collected declaration lives in.
pub(crate) const COLLECTED_UNIT: &str = "rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs";

/// The collected package's `src/lib.rs`.
const COLLECTED_SOURCE: &str = "pub fn helper_beacon() {}\npub fn beacon() {}\n";

/// The case-distinct declarations served by the identifier regression.
pub(crate) const CASE_SOURCE: &str = "pub fn SearchHit() {}\npub fn searchHit() {}\npub fn createProgram() {}\npub fn createprogram() {}\npub fn load_config() {}\npub fn Load_config() {}\npub fn Foo() {}\npub fn foo() {}\n";

/// The declarations in [`CASE_SOURCE`], in source order.
const CASE_DECLARATIONS: [&str; 8] = [
    "SearchHit",
    "searchHit",
    "createProgram",
    "createprogram",
    "load_config",
    "Load_config",
    "Foo",
    "foo",
];

/// The declarations [`COLLECTED_SOURCE`] carries, in source order.
pub(crate) const DECLARATIONS: [&str; 2] = ["helper_beacon", "beacon"];

/// The one `demo` requirement the collected release lies outside: the resolution answers
/// it with that release and a `requirement_unsatisfied` warning.
pub(crate) const UNSATISFIED_REQUIREMENT: &str = ">=2";

/// The `page_limit_max` the fixture's capabilities advertise unless a test sets another.
pub(crate) const PAGE_LIMIT_ADVERTISED: u64 = 200;

/// The `dependency_entries_max` the fixture's capabilities advertise unless a test sets
/// another: the client's own bound.
const DEPENDENCY_ENTRIES_ADVERTISED: u64 = 20_000;

/// The cursor a search page stopped at the response body bound names.
pub(crate) const BODY_BOUND_CURSOR: &str = "after-body-bound";

/// The query the fixture answers with `beacon` matched inside its body: a word no
/// declaration name holds, so the hit claims the `unknown` class and `file_content`.
pub(crate) const BODY_MATCH_QUERY: &str = "handshake";

/// The prose query the fixture answers only in the broad phase, in full-text order.
pub(crate) const BROAD_BODY_MATCH_QUERY: &str = "network handshake";

/// The Python standard library release a Python collection holds: every `stdlib/cpython`
/// entry resolves to it, as the nearest release when it names another.
pub(crate) const PYTHON_STANDARD_LIBRARY: (&str, &str, &str) =
    ("stdlib", "cpython", python_runtime::VERSION);

/// The `pypi` release a Python collection holds, whose `greeting/core.py` declares `greet`
/// on its first line.
pub(crate) const GREETING: (&str, &str, &str) = ("pypi", "greeting", "1.0.0");

pub(crate) const GREETING_SOURCE: &str =
    "def greet(name: str) -> str:\n    return name\n\n\ndef quiet() -> None:\n    return None\n";

/// One declaration a Python collection names at a package position.
pub(crate) struct NamedPosition {
    /// The package, as `manager`, `name`, and `version`.
    pub(crate) package: (&'static str, &'static str, &'static str),
    /// The file below the package's root.
    pub(crate) path: &'static str,
    /// The line a position must fall on, or any line when absent.
    pub(crate) line: Option<i64>,
    /// The declaration's qualified name.
    pub(crate) qualified_name: &'static str,
    /// The declaration's kind, as the Python provider names it.
    pub(crate) kind: &'static str,
}

/// The declarations a Python collection names at package positions. Every other position
/// answers no declaration.
pub(crate) const NAMED_POSITIONS: [NamedPosition; 2] = [
    NamedPosition {
        package: PYTHON_STANDARD_LIBRARY,
        path: "builtins.pyi",
        line: None,
        qualified_name: "len",
        kind: "function",
    },
    NamedPosition {
        package: GREETING,
        path: "greeting/core.py",
        line: Some(0),
        qualified_name: "greet",
        kind: "function",
    },
];

/// Largest request body the fixture reads.
const REQUEST_BODY_BYTES_MAX: usize = 4_194_304;

/// Compiled-size bound the fixture parses a pattern under.
const PATTERN_COMPILED_BYTES_MAX: usize = 1_048_576;

pub(crate) struct GlobalFixture {
    pub(crate) endpoint: String,
    state: FixtureState,
    task: JoinHandle<()>,
}

/// What the symbol and search endpoints answer.
#[derive(Clone, Copy)]
pub(crate) enum SymbolFixture {
    /// Hits whose symbol identities name the unit they sit in.
    Valid,
    /// Declarations differing only in case, with their original source.
    CasePairs,
    /// Hits whose symbol identities name another file, which the client refuses.
    InvalidIdentity,
}

/// An answer the fixture holds before it sends it, and for how long.
#[derive(Clone, Copy)]
pub(crate) enum Hold {
    /// The resolution endpoint's answer.
    Resolution(Duration),
    /// The symbol, nodes, and search endpoints' answers.
    Read(Duration),
    /// The declarations endpoint's answer.
    Declarations(Duration),
}

/// How the fixture answers, beyond the collection it holds.
#[derive(Clone, Copy)]
pub(crate) struct FixtureOptions {
    /// What the symbol and search endpoints answer.
    pub(crate) symbol: SymbolFixture,
    /// The answer the fixture holds before it sends it, and for how long.
    pub(crate) hold: Option<Hold>,
    /// The `page_limit_max` the capabilities advertise.
    pub(crate) page_limit_max: u64,
    /// The `dependency_entries_max` the capabilities advertise.
    pub(crate) dependency_entries_max: u64,
    /// Whether the precise search page stops at the response body bound: it answers its
    /// declarations with a `result_truncated` warning and [`BODY_BOUND_CURSOR`].
    pub(crate) stopped_at_body_bound: bool,
    /// The features the capabilities leave out of `supported_features`. Without both
    /// documentation features they carry no `documentation_revision` either.
    pub(crate) withheld_features: &'static [&'static str],
    /// Whether the collection also holds [`PYTHON_STANDARD_LIBRARY`] and [`GREETING`].
    pub(crate) python_collection: bool,
    /// The status of the problem the resolution endpoint answers in place of a resolution.
    pub(crate) resolution_problem: Option<StatusCode>,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            symbol: SymbolFixture::Valid,
            hold: None,
            page_limit_max: PAGE_LIMIT_ADVERTISED,
            dependency_entries_max: DEPENDENCY_ENTRIES_ADVERTISED,
            stopped_at_body_bound: false,
            withheld_features: &[],
            python_collection: false,
            resolution_problem: None,
        }
    }
}

#[derive(Clone)]
struct FixtureState {
    options: FixtureOptions,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    view: Value,
}

impl FixtureState {
    fn response(&self, mut value: Value) -> Response {
        if let Some(view) = value.get_mut("view") {
            view.clone_from(&self.view);
        }
        if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
            for result in results {
                if let Some(view) = result.get_mut("view") {
                    view.clone_from(&self.view);
                }
            }
        }
        json_response(&value)
    }
}

impl FixtureState {
    /// Waits out the resolution hold, when the fixture holds resolutions.
    async fn hold_resolution(&self) {
        if let Some(Hold::Resolution(delay)) = self.options.hold {
            tokio::time::sleep(delay).await;
        }
    }

    /// Waits out the read hold, when the fixture holds symbol, nodes, and search answers.
    async fn hold_read(&self) {
        if let Some(Hold::Read(delay)) = self.options.hold {
            tokio::time::sleep(delay).await;
        }
    }

    /// Waits out the declarations hold, when the fixture holds declaration answers.
    async fn hold_declarations(&self) {
        if let Some(Hold::Declarations(delay)) = self.options.hold {
            tokio::time::sleep(delay).await;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObservedRequest {
    pub(crate) telemetry_disabled: bool,
    pub(crate) uri: String,
    pub(crate) body: Option<Value>,
}

impl GlobalFixture {
    pub(crate) async fn start(symbol: SymbolFixture) -> Result<Self, std::io::Error> {
        Self::start_with(FixtureOptions {
            symbol,
            ..FixtureOptions::default()
        })
        .await
    }

    /// Starts the fixture, holding the answers `hold` names before it sends them.
    pub(crate) async fn start_holding(
        symbol: SymbolFixture,
        hold: Option<Hold>,
    ) -> Result<Self, std::io::Error> {
        Self::start_with(FixtureOptions {
            symbol,
            hold,
            ..FixtureOptions::default()
        })
        .await
    }

    pub(crate) async fn start_with(options: FixtureOptions) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let expires = std::time::SystemTime::now()
            .checked_add(Duration::from_mins(15))
            .expect("fixture view expiry fits the clock bound");
        let view = json!({
            "id": "a".repeat(64),
            "expires_at": jiff::Timestamp::try_from(expires)
                .expect("fixture view expiry fits the timestamp bound")
                .to_string(),
        });
        let state = FixtureState {
            options,
            requests: Arc::new(Mutex::new(Vec::new())),
            view,
        };
        let app = Router::new()
            .fallback(global_handler)
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            endpoint: format!("http://{address}/rift/rest"),
            state,
            task,
        })
    }

    pub(crate) async fn requests(&self) -> Vec<ObservedRequest> {
        self.state.requests.lock().await.clone()
    }
}

impl Drop for GlobalFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn global_handler(
    State(state): State<FixtureState>,
    request: axum::http::Request<Body>,
) -> Response {
    let telemetry_disabled = request
        .headers()
        .get("dnt")
        .is_some_and(|value| value == "1");
    let uri = request.uri().to_string();
    let path = request.uri().path().to_owned();
    let limit = request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|member| member.strip_prefix("limit="))
        })
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| {
            usize::try_from(PAGE_LIMIT_ADVERTISED).expect("advertised page bound fits usize")
        });
    let Ok(bytes) = to_bytes(request.into_body(), REQUEST_BODY_BYTES_MAX).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let body = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(body) => Some(body),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };
    state.requests.lock().await.push(ObservedRequest {
        telemetry_disabled,
        uri,
        body: body.clone(),
    });
    let body = body.unwrap_or(Value::Null);
    let options = state.options;
    if path.ends_with("/capabilities") {
        return json_response(&capabilities(&options));
    }
    if path.ends_with("/resolutions") {
        state.hold_resolution().await;
        if let Some(status) = options.resolution_problem {
            return problem_response(status);
        }
        return json_response(&resolution(&body, options.python_collection));
    }
    if path.ends_with("/declarations") {
        state.hold_declarations().await;
        return state.response(declarations(&body));
    }
    if path.ends_with("/search") {
        state.hold_read().await;
        let mut page = search_page(options.symbol, &body, options.stopped_at_body_bound);
        bound_case_page(&mut page, options.symbol, limit);
        return state.response(page);
    }
    if path.ends_with("/symbols") {
        state.hold_read().await;
        return state.response(symbols(options.symbol, &body));
    }
    if path.ends_with("/nodes") {
        state.hold_read().await;
        return symbol_nodes(&body).map_or_else(
            || problem_response(StatusCode::NOT_FOUND),
            |nodes| state.response(nodes),
        );
    }
    if path.ends_with("/patterns") {
        return pattern_page(&body).map_or_else(
            || StatusCode::BAD_REQUEST.into_response(),
            |page| state.response(page),
        );
    }
    StatusCode::NOT_FOUND.into_response()
}

fn json_response(value: &Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

/// An RFC 9457 problem answering `status`, as the global API sends one.
fn problem_response(status: StatusCode) -> Response {
    let problem = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or_default(),
        "status": status.as_u16(),
        "detail": "the fixture answers this status"
    });
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/problem+json")],
        problem.to_string(),
    )
        .into_response()
}

/// Applies the requested page bound to the case regression's already ranked declarations.
fn bound_case_page(page: &mut Value, fixture: SymbolFixture, limit: usize) {
    if matches!(fixture, SymbolFixture::CasePairs) {
        page["items"]
            .as_array_mut()
            .expect("a page carries items")
            .truncate(limit);
    }
}

/// The capabilities the fixture advertises under `options`, every feature it serves but
/// the withheld ones.
fn capabilities(options: &FixtureOptions) -> Value {
    let withheld = options.withheld_features;
    let features: Vec<&str> = [
        "resolutions",
        "search",
        "symbols",
        "documentation_search",
        "symbol_documentation",
        "patterns",
        "declarations",
        "nodes",
    ]
    .into_iter()
    .filter(|feature| !withheld.contains(feature))
    .collect();
    let mut advertised = json!({
        "supported_package_managers": ["cargo"],
        "supported_features": features,
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd",
        "required_search_fields": [
            "name", "qualified_name", "documentation", "signature"
        ],
        "bounds": {
            "request_body_bytes_max": 4_194_304,
            "response_body_bytes_max": 33_554_432,
            "dependency_entries_max": options.dependency_entries_max,
            "query_bytes_max": 4096,
            "query_terms_max": 32,
            "query_term_bytes_max": 256,
            "identifiers_max": 16,
            "identifier_bytes_max": 4096,
            "packages_max": 20000,
            "page_limit_min": 1,
            "page_limit_max": options.page_limit_max,
            "page_limit_default": 20,
            "cursor_bytes_max": 4096,
            "candidate_pool_max": 1000,
            "warnings_max": 32,
            "source_bytes_max": 1_048_576
        }
    });
    let documented = ["documentation_search", "symbol_documentation"]
        .iter()
        .any(|feature| !withheld.contains(feature));
    if !documented && let Some(fields) = advertised.as_object_mut() {
        fields.remove("documentation_revision");
    }
    advertised
}

/// The resolution of `requested`. The collected release answers an exact `demo` entry at
/// its own version as available, and one at another version, or a `demo` requirement, as
/// the nearest release, warning `requirement_unsatisfied` for [`UNSATISFIED_REQUIREMENT`].
/// With `python_collection`, [`PYTHON_STANDARD_LIBRARY`] and [`GREETING`] answer their
/// entries the same way. Every other exact entry and requirement is missing, so each entry
/// the request carries, the standard library entries included, is accounted for once.
fn resolution(requested: &Value, python_collection: bool) -> Value {
    let held: &[(&str, &str, &str)] = if python_collection {
        &[COLLECTED, PYTHON_STANDARD_LIBRARY, GREETING]
    } else {
        &[COLLECTED]
    };
    let (mut available, mut resolved, mut missing_exact, mut missing_requirements) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut warnings = Vec::new();
    for entry in requested["entries"].as_array().into_iter().flatten() {
        let manager = entry["manager"].as_str().unwrap_or_default();
        let name = entry["name"].as_str().unwrap_or_default();
        let release = held
            .iter()
            .find(|(held_manager, held_name, _)| (*held_manager, *held_name) == (manager, name));
        match (&entry["version"], release) {
            (Value::String(version), Some(release)) if version == release.2 => {
                available.push(package_value(*release));
            }
            (Value::String(version), None) => missing_exact.push(json!({
                "manager": manager, "registry": entry["registry"], "name": name, "version": version
            })),
            (_, None) => missing_requirements.push(entry.clone()),
            (_, Some(release)) => {
                resolved.push(json!({"entry": entry, "package": package_value(*release)}));
                if entry["requirement"] == UNSATISFIED_REQUIREMENT {
                    warnings.push(json!({
                        "code": "requirement_unsatisfied",
                        "detail": format!(
                            "{manager}/{name} {UNSATISFIED_REQUIREMENT} answered by {}",
                            release.2
                        )
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "available_exact": available,
        "resolved_requirements": resolved,
        "missing_exact": missing_exact,
        "missing_requirements": missing_requirements
    });
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
    }
    body
}

fn package_value((manager, name, version): (&str, &str, &str)) -> Value {
    let registry = match manager {
        "cargo" => Some("crates.io"),
        "pypi" => Some("pypi.org"),
        _ => None,
    };
    json!({"manager": manager, "registry": registry, "name": name, "version": version})
}

fn named_owner(named: &NamedPosition) -> rift_protocol::identity::SymbolOwner {
    use rift_protocol::identity::SymbolOwner;
    let (manager, name, version) = named.package;
    if manager == "stdlib" {
        SymbolOwner::Runtime {
            runtime: name.to_owned(),
            version: version.to_owned(),
        }
    } else {
        SymbolOwner::Package {
            manager: manager.to_owned(),
            registry: "pypi.org".to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }
}

pub(crate) fn named_unit(named: &NamedPosition) -> String {
    rift_core::SourceUnitId::for_owner(named_owner(named), named.path)
        .expect("fixture released owner and original source path")
        .to_string()
}

pub(crate) fn named_identity(named: &NamedPosition) -> String {
    use rift_protocol::identity::SymbolIdentity;
    let owner = named_owner(named);
    let mut logical = named
        .path
        .trim_end_matches(".py")
        .trim_end_matches(".pyi")
        .split('/')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    logical.push(named.qualified_name.to_owned());
    SymbolIdentity::new(
        owner,
        rift_protocol::read::Language::from_identity_segment("python")
            .expect("fixture language is supported"),
        logical,
    )
    .expect("fixture logical declaration")
    .wire_identity()
}

/// The declaration answer for `request`: each position [`NAMED_POSITIONS`] lists names its
/// declaration, by the identity package analysis mints, and its kind; every other position
/// names neither.
fn declarations(request: &Value) -> Value {
    let results: Vec<Value> = request["positions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|position| {
            let named = NAMED_POSITIONS.iter().find(|named| {
                position["unit"] == named_unit(named)
                    && named.line.is_none_or(|line| position["line"] == line)
            });
            let view = json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "expires_at": "2099-10-10T10:05:00Z"});
            match named {
                Some(named) => {
                    json!({
                        "outcome": "found",
                        "position": position,
                        "id": named_identity(named),
                        "kind": named.kind,
                        "view": view
                    })
                }
                None => json!({"outcome": "missing", "position": position, "view": view}),
            }
        })
        .collect();
    json!({"results": results})
}

/// One page of `items` and `warnings`, with the revision fields every page carries and no
/// cursor.
fn page(items: &[Value], warnings: &Value) -> Value {
    json!({
        "items": items,
        "warnings": warnings,
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd",
        "view": {
            "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "expires_at": "2099-10-10T10:05:00Z"
        }
    })
}

/// The exact result for the canonical object named by `request`, retaining its original
/// location and the collection's captured view.
fn symbols(fixture: SymbolFixture, request: &Value) -> Value {
    let id = request["id"].as_str().unwrap_or_default();
    let declaration = fixture_declarations(fixture)
        .iter()
        .copied()
        .find(|name| collected_identity(fixture, name) == id);
    let view = json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "expires_at": "2099-10-10T10:05:00Z"});
    if id == named_identity(&NAMED_POSITIONS[0]) {
        return runtime_symbol(&view);
    }
    if id == named_identity(&NAMED_POSITIONS[1]) {
        return greeting_symbol(&view);
    }
    let Some(declaration) = declaration else {
        return json!({"outcome":"missing", "symbol_not_found":{"id":id,"alternatives":[]},"view":view,"revision":release_revision(id)});
    };
    let hit = collected_hit(fixture, declaration, false);
    let binding = json!({"origin":hit["symbol"]["origin"],"unit":hit["unit"],"range":hit["range"],"line":hit["line"],"signature_indices":[],"type_indices":[],"documentation_indices":[]});
    json!({"outcome":"found", "symbol":hit["symbol"], "declarations":[binding],"view":view,"revision":release_revision(id)})
}

/// The release revision the global API answers a package or runtime symbol `id` at.
fn release_revision(id: &str) -> Value {
    let revision = rift_protocol::identity::SymbolIdentity::parse(id)
        .and_then(|identity| rift_protocol::revision::Revision::release(identity.owner()));
    json!(
        revision
            .map(|revision| revision.to_string())
            .unwrap_or_default()
    )
}

/// The nodes of the collected declaration `request` names, as a symbol read of
/// [`COLLECTED_SOURCE`] lists them: the item node, then its visibility, name, parameters,
/// and body, in the fixture's view when the request names one. `None` for any other symbol.
fn symbol_nodes(request: &Value) -> Option<Value> {
    let id = request["symbol"].as_str()?;
    if id == named_identity(&NAMED_POSITIONS[0]) {
        return Some(runtime_nodes(request));
    }
    let name = DECLARATIONS
        .into_iter()
        .find(|name| collected_identity(SymbolFixture::Valid, name) == id)?;
    let item = format!("pub fn {name}() {{}}");
    let start = COLLECTED_SOURCE.find(&item)?;
    let parts = [
        ("function_item", 0, item.as_str()),
        ("visibility_modifier", 0, "pub"),
        ("identifier", "pub fn ".len(), name),
        ("parameters", "pub fn ".len() + name.len(), "()"),
        ("block", item.len() - "{}".len(), "{}"),
    ];
    let language = rift_protocol::read::Language::from_identity_segment("rust").ok()?;
    let nodes = parts
        .into_iter()
        .map(|(kind, offset, source)| {
            let node_start = start + offset;
            let id = rift_protocol::read::NodeId::for_content(&language, kind, source.as_bytes());
            let mut node = json!({
                "id": id.0, "unit": COLLECTED_UNIT, "language": "rust", "kind": kind,
                "range": {"start": node_start, "end": node_start + source.len()},
                "source": source,
            });
            if kind == "function_item" {
                node["of"] = json!(collected_identity(SymbolFixture::Valid, name));
                node["facets"] = json!(["declaration", "definition"]);
            }
            node
        })
        .collect::<Vec<_>>();
    let mut answer = json!({"nodes": nodes, "revision": release_revision(id)});
    if request.get("view").is_some() {
        answer["view"] = Value::Null;
    }
    Some(answer)
}

/// The byte range of the runtime `len` declaration in the embedded standard library stub.
fn runtime_declaration() -> (usize, usize) {
    let builtins = python_runtime::builtins();
    let start = builtins
        .find("def len(")
        .expect("embedded stub declares len");
    let end = builtins[start..]
        .find('\n')
        .map_or(builtins.len(), |offset| start + offset);
    (start, end)
}

/// The item node of the runtime `len` declaration, carrying the embedded standard library
/// stub's own bytes at its range, in the view the request names.
fn runtime_nodes(request: &Value) -> Value {
    let builtins = python_runtime::builtins();
    let (start, end) = runtime_declaration();
    let source = &builtins[start..end];
    let language = rift_protocol::read::Language::from_identity_segment("python")
        .expect("python identity segment");
    let id = rift_protocol::read::NodeId::for_content(
        &language,
        "function_definition",
        source.as_bytes(),
    );
    let mut answer = json!({"nodes": [{
        "id": id.0, "of": named_identity(&NAMED_POSITIONS[0]),
        "unit": named_unit(&NAMED_POSITIONS[0]), "language": "python",
        "kind": "function_definition", "facets": ["declaration", "definition"],
        "range": {"start": start, "end": end}, "source": source,
    }], "revision": release_revision(&named_identity(&NAMED_POSITIONS[0]))});
    if let Some(view) = request.get("view") {
        answer["view"] = json!({"id": view, "expires_at": "2099-10-10T10:05:00Z"});
    }
    answer
}

/// The runtime `len` declaration at its range in the embedded standard library stub.
fn runtime_symbol(view: &Value) -> Value {
    let builtins = python_runtime::builtins();
    let (start, end) = runtime_declaration();
    let line = builtins[..start].matches('\n').count() + 1;
    let origin = json!({
        "location":"stdlib", "source_kind":"authored",
        "runtime":{"runtime":PYTHON_STANDARD_LIBRARY.1, "version":PYTHON_STANDARD_LIBRARY.2}
    });
    let declaration = json!({
        "origin":origin, "unit":named_unit(&NAMED_POSITIONS[0]),
        "range":{"start":start,"end":end}, "line":line,
        "signature_indices":[], "type_indices":[], "documentation_indices":[]
    });
    json!({
        "outcome":"found", "view":view,
        "revision":release_revision(&named_identity(&NAMED_POSITIONS[0])),
        "symbol":{
            "id":named_identity(&NAMED_POSITIONS[0]), "language":"python",
            "name":"len", "kind":"function", "origin":origin
        },
        "declarations":[declaration]
    })
}

fn greeting_symbol(view: &Value) -> Value {
    let source = GREETING_SOURCE
        .split("\n\n")
        .next()
        .expect("greet declaration");
    let origin = json!({
        "location":"dependency", "package":package_value(GREETING),
        "source_kind":"authored"
    });
    let declaration = json!({
        "origin":origin, "unit":named_unit(&NAMED_POSITIONS[1]),
        "range":{"start":0,"end":source.len()}, "line":1,
        "signature_indices":[], "type_indices":[], "documentation_indices":[]
    });
    json!({
        "outcome":"found", "view":view,
        "revision":release_revision(&named_identity(&NAMED_POSITIONS[1])),
        "symbol":{
            "id":named_identity(&NAMED_POSITIONS[1]), "language":"python",
            "name":"greet", "kind":"function", "origin":origin
        },
        "declarations":[declaration]
    })
}

/// The search page for `request`: in the precise phase, each collected declaration one of
/// its identifiers matches, at the best class any of them reaches, with a `query_narrowed`
/// page warning; the broad phase answers no further declaration unless the query is
/// [`BROAD_BODY_MATCH_QUERY`]. That query answers every declaration as a body match in
/// source order. [`BODY_MATCH_QUERY`] answers `beacon` as a body match. A page stopped
/// at the response body bound adds `result_truncated` and [`BODY_BOUND_CURSOR`].
fn search_page(fixture: SymbolFixture, request: &Value, stopped_at_body_bound: bool) -> Value {
    if request["query"] == BROAD_BODY_MATCH_QUERY {
        let items = if request["phase"] == "broad" {
            DECLARATIONS
                .into_iter()
                .map(|declaration| body_match_hit(fixture, declaration, request))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        return page(&items, &json!([]));
    }
    if request["phase"] != "precise" {
        return page(&[], &json!([]));
    }
    if request["query"] == BODY_MATCH_QUERY {
        return page(&[body_match_hit(fixture, "beacon", request)], &json!([]));
    }
    let identifiers: Vec<String> = request["identifiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let with_source = includes_source(request);
    let items: Vec<Value> = matched_declarations(fixture, &identifiers)
        .map(|(declaration, class)| {
            let (_, field) = class_wire(class);
            canonical_search_hit(fixture, declaration, with_source, field)
        })
        .collect();
    let narrowed = json!({
        "code": "query_narrowed",
        "detail": "query exceeded the active term bound"
    });
    if !stopped_at_body_bound {
        return page(&items, &json!([narrowed]));
    }
    let truncated = json!({
        "code": "result_truncated",
        "detail": "the page stopped at response_body_bytes_max"
    });
    let mut stopped = page(&items, &json!([narrowed, truncated]));
    stopped["next_cursor"] = json!(BODY_BOUND_CURSOR);
    stopped
}

/// One body match retaining the declaration's original identity and source.
fn body_match_hit(fixture: SymbolFixture, declaration: &str, request: &Value) -> Value {
    canonical_search_hit(fixture, declaration, includes_source(request), "content")
}

/// Projects the original package facts through the shared canonical search result.
fn canonical_search_hit(
    fixture: SymbolFixture,
    declaration: &str,
    with_source: bool,
    field: &str,
) -> Value {
    let original = collected_hit(fixture, declaration, with_source);
    let matched_by = if field == "qualified_name" {
        "name"
    } else {
        field
    };
    let mut hit = json!({
        "hit": {"target": "symbol", "symbol": original["symbol"]},
        "unit": original["unit"],
        "range": original["range"],
        "line": original["line"],
        "matched_by": [matched_by]
    });
    if let Some(source) = original.get("source") {
        hit["source"] = source.clone();
    }
    hit
}

/// The pattern page for `request`: each match of its pattern in the collected source, in
/// offset order, with the declaration holding it and, when the request includes `source`,
/// the matched line and the declaration's source. `None` for a pattern that does not parse.
fn pattern_page(request: &Value) -> Option<Value> {
    let pattern = request["pattern"].as_str().unwrap_or_default();
    let pattern = rift_ranking::Pattern::parse(pattern, PATTERN_COMPILED_BYTES_MAX).ok()?;
    let with_source = includes_source(request);
    let items: Vec<Value> = pattern
        .matches(COLLECTED_SOURCE, 0..COLLECTED_SOURCE.len())
        .map(|matched| pattern_hit(&matched, with_source))
        .collect();
    let mut answer = page(&items, &json!([]));
    answer.as_object_mut()?.remove("documentation_revision");
    Some(answer)
}

/// One match of [`COLLECTED_SOURCE`] as a pattern page item.
fn pattern_hit(matched: &std::ops::Range<usize>, with_source: bool) -> Value {
    let line_start = COLLECTED_SOURCE[..matched.start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let line_end = COLLECTED_SOURCE[matched.start..]
        .find('\n')
        .map_or(COLLECTED_SOURCE.len(), |newline| matched.start + newline);
    let mut hit = json!({
        "package": package_value(COLLECTED),
        "unit": COLLECTED_UNIT,
        "range": {"start": matched.start, "end": matched.end},
        "line": COLLECTED_SOURCE[..matched.start].matches('\n').count() + 1,
        "size": COLLECTED_SOURCE.len()
    });
    if with_source {
        hit["source"] = json!(&COLLECTED_SOURCE[line_start..line_end]);
    }
    let holding = DECLARATIONS.into_iter().find(|declaration| {
        let range = declaration_range(COLLECTED_SOURCE, declaration);
        range.start <= matched.start && matched.end <= range.end
    });
    if let Some(declaration) = holding {
        let located = collected_hit(SymbolFixture::Valid, declaration, with_source);
        let mut held = json!({
            "symbol": located["symbol"],
            "range": located["range"],
            "line": located["line"]
        });
        if with_source {
            held["source"] = located["source"].clone();
        }
        hit["declaration"] = held;
    }
    hit
}

/// The bytes of [`COLLECTED_SOURCE`] one collected declaration spans.
fn declaration_range(source: &str, declaration: &str) -> std::ops::Range<usize> {
    let start = source
        .find(&format!("pub fn {declaration}("))
        .expect("every declaration sits in the collected source");
    start..start + format!("pub fn {declaration}() {{}}").len()
}

/// Original declarations carried by the selected collection.
fn fixture_declarations(fixture: SymbolFixture) -> &'static [&'static str] {
    match fixture {
        SymbolFixture::CasePairs => &CASE_DECLARATIONS,
        SymbolFixture::Valid | SymbolFixture::InvalidIdentity => &DECLARATIONS,
    }
}

/// Collected declarations ordered by their best original or normalized identifier match.
fn matched_declarations(
    fixture: SymbolFixture,
    candidates: &[String],
) -> impl Iterator<Item = (&'static str, IdentifierMatchClass)> {
    let declarations = fixture_declarations(fixture);
    let mut matched: Vec<_> = declarations
        .iter()
        .filter_map(|declaration| {
            candidates
                .iter()
                .enumerate()
                .filter_map(|(position, candidate)| {
                    rift_ranking::identifier_match(candidate, declaration, declaration)
                        .map(|rank| (rank, position))
                })
                .min()
                .map(|rank| (*declaration, rank))
        })
        .collect();
    matched.sort_by_key(|(declaration, rank)| (*rank, *declaration));
    matched
        .into_iter()
        .map(|(declaration, (rank, _))| (declaration, rank.class()))
}

fn includes_source(request: &Value) -> bool {
    request["include"]
        .as_array()
        .is_some_and(|fields| fields.iter().any(|field| field == "source"))
}

/// The wire spelling of `class`, and the field it was proved against.
const fn class_wire(class: IdentifierMatchClass) -> (&'static str, &'static str) {
    match class {
        IdentifierMatchClass::QualifiedExact => ("qualified_exact", "qualified_name"),
        IdentifierMatchClass::NameExact => ("name_exact", "name"),
        IdentifierMatchClass::NamePrefix => ("name_prefix", "name"),
        IdentifierMatchClass::Substring => ("substring", "qualified_name"),
    }
}

pub(crate) fn collected_identity(fixture: SymbolFixture, declaration: &str) -> String {
    let (manager, name, version) = COLLECTED;
    let root = if matches!(fixture, SymbolFixture::InvalidIdentity) {
        "other"
    } else {
        name
    };
    rift_protocol::identity::SymbolIdentity::new(
        rift_protocol::identity::SymbolOwner::Package {
            manager: manager.to_owned(),
            registry: "crates.io".to_owned(),
            name: if matches!(fixture, SymbolFixture::InvalidIdentity) {
                "other".to_owned()
            } else {
                name.to_owned()
            },
            version: version.to_owned(),
        },
        rift_protocol::read::Language::from_identity_segment("rust")
            .expect("fixture language is supported"),
        vec![root.to_owned(), declaration.to_owned()],
    )
    .expect("fixture package and literal public declaration are canonical")
    .wire_identity()
}

/// One original declaration with its canonical package identity and physical source unit.
fn collected_hit(fixture: SymbolFixture, declaration: &str, with_source: bool) -> Value {
    let source = match fixture {
        SymbolFixture::CasePairs => CASE_SOURCE,
        SymbolFixture::Valid | SymbolFixture::InvalidIdentity => COLLECTED_SOURCE,
    };
    let range = declaration_range(source, declaration);
    let line = source[..range.start].matches('\n').count() + 1;
    let (manager, name, version) = COLLECTED;
    let package = package_value((manager, name, version));
    let mut hit = json!({
        "package": package,
        "symbol": {
            "id": collected_identity(fixture, declaration),
            "kind": "function",
            "language": "rust",
            "name": declaration,
            "origin": {
                "location": "dependency",
                "package": package,
                "source_kind": "authored"
            }
        },
        "unit": COLLECTED_UNIT,
        "range": {"start": range.start, "end": range.end},
        "line": line
    });
    if with_source {
        hit["source"] = json!(&source[range]);
    }
    hit
}
