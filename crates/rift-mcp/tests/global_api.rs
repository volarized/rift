//! A fixture global API on a loopback port.
//!
//! It answers the capability, resolution, symbol, and search endpoints
//! `rift-cloud-client` reads, and records every request it receives. Its collection holds
//! one package, [`COLLECTED`], whose `src/lib.rs` declares [`DECLARATIONS`]; a symbol
//! request answers the declaration it names and a search request the declarations its
//! identifiers match, so each answer passes the client's own match class validation.

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

/// Largest request body the fixture reads.
const REQUEST_BODY_BYTES_MAX: usize = 4_194_304;

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

#[derive(Clone)]
struct FixtureState {
    symbol: SymbolFixture,
    hold: Option<Hold>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
}

impl FixtureState {
    /// Waits out the resolution hold, when the fixture holds resolutions.
    async fn hold_resolution(&self) {
        if let Some(Hold::Resolution(delay)) = self.hold {
            tokio::time::sleep(delay).await;
        }
    }

    /// Waits out the read hold, when the fixture holds symbol and search pages.
    async fn hold_read(&self) {
        if let Some(Hold::Read(delay)) = self.hold {
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
    pub(crate) async fn start(symbol_fixture: SymbolFixture) -> Result<Self, std::io::Error> {
        Self::start_holding(symbol_fixture, None).await
    }

    /// Starts the fixture, holding the answers `hold` names before it sends them.
    pub(crate) async fn start_holding(
        symbol_fixture: SymbolFixture,
        hold: Option<Hold>,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = FixtureState {
            symbol: symbol_fixture,
            hold,
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
    if path.ends_with("/capabilities") {
        return json_response(&capabilities());
    }
    if path.ends_with("/resolutions") {
        state.hold_resolution().await;
        return json_response(&resolution(&body));
    }
    if path.ends_with("/search") {
        state.hold_read().await;
        return json_response(&search_page(state.symbol, &body));
    }
    if path.ends_with("/symbols") {
        state.hold_read().await;
        return json_response(&symbols(state.symbol, &body));
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

fn capabilities() -> Value {
    json!({
        "supported_package_managers": ["cargo"],
        "supported_features": ["resolutions", "search", "symbols", "documentation_search", "symbol_documentation"],
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
            "dependency_entries_max": 20000,
            "query_bytes_max": 4096,
            "query_terms_max": 32,
            "query_term_bytes_max": 256,
            "identifiers_max": 16,
            "identifier_bytes_max": 4096,
            "packages_max": 20000,
            "page_limit_min": 1,
            "page_limit_max": 200,
            "page_limit_default": 20,
            "cursor_bytes_max": 4096,
            "candidate_pool_max": 1000,
            "warnings_max": 32,
            "source_bytes_max": 1_048_576
        }
    })
}

/// The resolution of `requested`: the collected release is available, every other exact
/// entry is missing, a requirement naming the collected package resolves to its one
/// release, and every other requirement is missing, so each entry the request carries,
/// the standard library entries included, is accounted for once.
fn resolution(requested: &Value) -> Value {
    let (mut available, mut resolved, mut missing_exact, mut missing_requirements) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let collected = json!({"manager": COLLECTED.0, "name": COLLECTED.1, "version": COLLECTED.2});
    for entry in requested["entries"].as_array().into_iter().flatten() {
        let identity = (
            entry["manager"].as_str().unwrap_or_default(),
            entry["name"].as_str().unwrap_or_default(),
            entry["version"].as_str().unwrap_or_default(),
        );
        let names_collected = (identity.0, identity.1) == (COLLECTED.0, COLLECTED.1);
        match (&entry["version"], &entry["requirement"]) {
            (Value::String(_), _) if identity == COLLECTED => available.push(collected.clone()),
            (Value::String(_), _) => missing_exact.push(json!({
                "manager": identity.0, "name": identity.1, "version": identity.2
            })),
            _ if names_collected => resolved.push(json!({
                "entry": entry, "package": collected
            })),
            _ => missing_requirements.push(entry.clone()),
        }
    }
    json!({
        "available_exact": available,
        "resolved_requirements": resolved,
        "missing_exact": missing_exact,
        "missing_requirements": missing_requirements
    })
}

/// One page of `items` and `warnings`, with the revision fields every page carries.
fn page(items: &[Value], warnings: &Value) -> Value {
    json!({
        "items": items,
        "next_cursor": null,
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
/// page warning; the broad phase answers no further declaration.
fn search_page(fixture: SymbolFixture, request: &Value) -> Value {
    if request["phase"] != "precise" {
        return page(&[], &json!([]));
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
    page(
        &items,
        &json!([{
            "code": "query_narrowed",
            "detail": "query exceeded the active term bound"
        }]),
    )
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
    let text = format!("pub fn {declaration}() {{}}");
    let start = COLLECTED_SOURCE
        .find(&format!("pub fn {declaration}("))
        .expect("every declaration sits in the collected source");
    let line = COLLECTED_SOURCE[..start].matches('\n').count() + 1;
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
        "range": {"start": start, "end": start + text.len()},
        "line": line
    });
    if with_source {
        hit["source"] = json!(text);
    }
    hit
}
