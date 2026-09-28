//! A fixture global API on a loopback port.
//!
//! It answers the capability, resolution, symbol, search, and pattern endpoints
//! `rift-cloud-client` reads, and records every request it receives. Its collection holds
//! one package, [`COLLECTED`], whose `src/lib.rs` declares [`DECLARATIONS`]; a symbol
//! request answers the declaration it names, a search request the declarations its
//! identifiers match, and a pattern request the matches of its pattern in that file, so
//! each answer passes the client's own validation. The
//! collected release also answers, as the nearest release, an exact `demo` entry at another
//! version and every `demo` requirement.

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
pub(crate) const COLLECTED_UNIT: &str = "rift://source/cargo/demo@1.0.0/src/lib.rs";

/// The collected package's `src/lib.rs`.
const COLLECTED_SOURCE: &str = "pub fn helper_beacon() {}\npub fn beacon() {}\n";

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
    /// Hits whose symbol identities name another file, which the client refuses.
    InvalidIdentity,
}

/// An answer the fixture holds before it sends it, and for how long.
#[derive(Clone, Copy)]
pub(crate) enum Hold {
    /// The resolution endpoint's answer.
    Resolution(Duration),
    /// The symbol and search endpoints' answers.
    Read(Duration),
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
        }
    }
}

#[derive(Clone)]
struct FixtureState {
    options: FixtureOptions,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
}

impl FixtureState {
    /// Waits out the resolution hold, when the fixture holds resolutions.
    async fn hold_resolution(&self) {
        if let Some(Hold::Resolution(delay)) = self.options.hold {
            tokio::time::sleep(delay).await;
        }
    }

    /// Waits out the read hold, when the fixture holds symbol and search pages.
    async fn hold_read(&self) {
        if let Some(Hold::Read(delay)) = self.options.hold {
            tokio::time::sleep(delay).await;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObservedRequest {
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
        let state = FixtureState {
            options,
            requests: Arc::new(Mutex::new(Vec::new())),
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
    let uri = request.uri().to_string();
    let path = request.uri().path().to_owned();
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
        return json_response(&resolution(&body));
    }
    if path.ends_with("/search") {
        state.hold_read().await;
        let page = search_page(options.symbol, &body, options.stopped_at_body_bound);
        return json_response(&page);
    }
    if path.ends_with("/symbols") {
        state.hold_read().await;
        return json_response(&symbols(options.symbol, &body));
    }
    if path.ends_with("/patterns") {
        return pattern_page(&body).map_or_else(
            || StatusCode::BAD_REQUEST.into_response(),
            |page| json_response(&page),
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

fn collected_package() -> Value {
    json!({"manager": COLLECTED.0, "name": COLLECTED.1, "version": COLLECTED.2})
}

/// The resolution of `requested`. The collected release answers an exact `demo` entry at
/// its own version as available, and one at another version, or a `demo` requirement, as
/// the nearest release, warning `requirement_unsatisfied` for [`UNSATISFIED_REQUIREMENT`].
/// Every other exact entry and requirement is missing, so each entry the request carries,
/// the standard library entries included, is accounted for once.
fn resolution(requested: &Value) -> Value {
    let (mut available, mut resolved, mut missing_exact, mut missing_requirements) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut warnings = Vec::new();
    for entry in requested["entries"].as_array().into_iter().flatten() {
        let manager = entry["manager"].as_str().unwrap_or_default();
        let name = entry["name"].as_str().unwrap_or_default();
        let collected = (manager, name) == (COLLECTED.0, COLLECTED.1);
        match (&entry["version"], collected) {
            (Value::String(version), true) if version == COLLECTED.2 => {
                available.push(collected_package());
            }
            (Value::String(version), false) => missing_exact.push(json!({
                "manager": manager, "name": name, "version": version
            })),
            (_, false) => missing_requirements.push(entry.clone()),
            (_, true) => {
                resolved.push(json!({"entry": entry, "package": collected_package()}));
                if entry["requirement"] == UNSATISFIED_REQUIREMENT {
                    warnings.push(json!({
                        "code": "requirement_unsatisfied",
                        "detail": format!(
                            "{manager}/{name} {UNSATISFIED_REQUIREMENT} answered by {}",
                            COLLECTED.2
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

/// One page of `items` and `warnings`, with the revision fields every page carries and no
/// cursor.
fn page(items: &[Value], warnings: &Value) -> Value {
    json!({
        "items": items,
        "warnings": warnings,
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd"
    })
}

/// The symbol page for `request`: each collected declaration its `name` matches, at the
/// class that match reaches, with a `source_truncated` page warning.
fn symbols(fixture: SymbolFixture, request: &Value) -> Value {
    let name = request["name"].as_str().unwrap_or_default().to_lowercase();
    let with_source = includes_source(request);
    let items: Vec<Value> = matched_declarations(&[name])
        .map(|(declaration, class)| {
            let mut hit = collected_hit(fixture, declaration, with_source);
            hit["match_class"] = json!(class_wire(class).0);
            hit
        })
        .collect();
    page(
        &items,
        &json!([{
            "code": "source_truncated",
            "detail": "source exceeded the active bound"
        }]),
    )
}

/// The search page for `request`: in the precise phase, each collected declaration one of
/// its identifiers matches, at the best class any of them reaches, with a `query_narrowed`
/// page warning; the broad phase answers no further declaration. [`BODY_MATCH_QUERY`]
/// answers `beacon` as a body match. A page stopped at the response body bound adds
/// `result_truncated` and [`BODY_BOUND_CURSOR`].
fn search_page(fixture: SymbolFixture, request: &Value, stopped_at_body_bound: bool) -> Value {
    if request["phase"] != "precise" {
        return page(&[], &json!([]));
    }
    if request["query"] == BODY_MATCH_QUERY {
        let mut hit = collected_hit(fixture, "beacon", includes_source(request));
        hit["target"] = json!("symbol");
        hit["match_class"] = json!("unknown");
        hit["contributing_fields"] = json!(["file_content"]);
        return page(&[hit], &json!([]));
    }
    let identifiers: Vec<String> = request["identifiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .collect();
    let with_source = includes_source(request);
    let items: Vec<Value> = matched_declarations(&identifiers)
        .map(|(declaration, class)| {
            let (match_class, field) = class_wire(class);
            let mut hit = collected_hit(fixture, declaration, with_source);
            hit["target"] = json!("symbol");
            hit["match_class"] = json!(match_class);
            hit["contributing_fields"] = json!([field]);
            hit
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
        "package": collected_package(),
        "unit": COLLECTED_UNIT,
        "range": {"start": matched.start, "end": matched.end},
        "line": COLLECTED_SOURCE[..matched.start].matches('\n').count() + 1,
        "size": COLLECTED_SOURCE.len()
    });
    if with_source {
        hit["source"] = json!(&COLLECTED_SOURCE[line_start..line_end]);
    }
    let holding = DECLARATIONS.into_iter().find(|declaration| {
        let range = declaration_range(declaration);
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
fn declaration_range(declaration: &str) -> std::ops::Range<usize> {
    let start = COLLECTED_SOURCE
        .find(&format!("pub fn {declaration}("))
        .expect("every declaration sits in the collected source");
    start..start + format!("pub fn {declaration}() {{}}").len()
}

/// Each collected declaration one of the lowercase `candidates` matches, at the best
/// class any of them reaches: the class the client computes again to validate the hit.
fn matched_declarations(
    candidates: &[String],
) -> impl Iterator<Item = (&'static str, IdentifierMatchClass)> + '_ {
    DECLARATIONS.into_iter().filter_map(|declaration| {
        candidates
            .iter()
            .filter_map(|candidate| rift_ranking::match_class(candidate, declaration, declaration))
            .min()
            .map(|class| (declaration, class))
    })
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

/// One collected declaration as a page item, located in [`COLLECTED_SOURCE`], before
/// its match class. Its symbol identity is the one package analysis mints over the unit,
/// `rift://symbol/rust/cargo/demo@1.0.0/<path>/<qualified name>`.
fn collected_hit(fixture: SymbolFixture, declaration: &str, with_source: bool) -> Value {
    let file = match fixture {
        SymbolFixture::Valid => "src/lib.rs",
        SymbolFixture::InvalidIdentity => "other.rs",
    };
    let range = declaration_range(declaration);
    let line = COLLECTED_SOURCE[..range.start].matches('\n').count() + 1;
    let (manager, name, version) = COLLECTED;
    let package = json!({"manager": manager, "name": name, "version": version});
    let mut hit = json!({
        "package": package,
        "symbol": {
            "id": format!("rift://symbol/rust/{manager}/{name}@{version}/{file}/{declaration}"),
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
        hit["source"] = json!(&COLLECTED_SOURCE[range]);
    }
    hit
}
