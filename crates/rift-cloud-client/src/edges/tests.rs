use super::{OperationFixture, operation_client};
use rift_protocol::edge_read::{EdgeSearchRequest, EdgeSearchResult};
use serde_json::{Value, json};

const SEED: &str = "rift://symbol/cargo/crates.io/demo/rust/entry";
const TARGET: &str = "rift://symbol/cargo/crates.io/demo/rust/helper";

fn request() -> EdgeSearchRequest {
    serde_json::from_value(json!({"edges":{"seeds":[SEED],"direction":"outgoing","kinds":["calls"]},"packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]})).expect("strict request")
}

#[tokio::test]
async fn cached_edge_capability_does_not_retry_an_unsupported_operation() {
    let problem = json!({
        "type":"about:blank", "title":"Service unavailable", "status":503,
        "detail":"Service unavailable", "code":"rift.server.read_unsupported"
    });
    let (server, client) = operation_client(OperationFixture::EdgeRefusal {
        value: problem,
        problem: true,
    })
    .await;
    client
        .get_capabilities()
        .await
        .expect("cached supported capability");
    let error = client
        .edge_pages(&request(), limits())
        .await
        .expect_err("unsupported operation");
    assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    let requests = server.state.request_log.lock().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.ends_with("/search"))
            .count(),
        1
    );
}

#[tokio::test]
async fn missing_edge_coverage_keeps_its_typed_reason() {
    let value = json!({"outcome":"unavailable", "reason":"insufficient_coverage"});
    let (server, client) = operation_client(OperationFixture::EdgeRefusal {
        value: value.clone(),
        problem: false,
    })
    .await;
    let answer = client
        .edge_pages(&request(), limits())
        .await
        .expect("typed unavailable");
    assert_eq!(serde_json::to_value(answer).expect("typed result"), value);
    let requests = server.state.request_log.lock().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.ends_with("/search"))
            .count(),
        1
    );
}

fn page(start: u64, complete: bool, _next: Option<&str>) -> Value {
    json!({"groups":[{"seed":SEED,"complete":complete,"edges":[{"id":format!("{start:064x}"),"edge":{"from":SEED,"to":TARGET,"kind":"call","kinds":["calls"],"derivation":"resolution","occurrence":{"unit":"rift://source/cargo/crates.io/demo/src/lib.rs","range":{"start":start,"end":start+1}}}}]}],
        "symbols":[symbol(SEED,"entry"),symbol(TARGET,"helper")],
        "sources":[{"unit":"rift://source/cargo/crates.io/demo/src/lib.rs","digest":full_digest("...")}],
        "analyzer_revision":"analyzer-v1","corpus_revision":"corpus-v1","view":{"id":"a".repeat(64),"expires_at":"2099-01-01T00:00:00Z"}})
}

fn full_digest(text: &str) -> String {
    data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes())
}

fn symbol(id: &str, name: &str) -> Value {
    json!({"symbol":{"id":id,"name":name,"language":"rust","kind":"function","origin":{"source_kind":"synthetic"}},"declarations":[]})
}

fn limits() -> crate::edges::EdgeReadLimits {
    crate::edges::EdgeReadLimits {
        requests_max: 8,
        records_max: 100,
        bytes_max: 1_048_576,
    }
}

#[tokio::test]
async fn raw_edge_pages_preserve_parallel_records_and_original_selection() {
    let (server, client) = operation_client(OperationFixture::Edges {
        pages: vec![page(0, false, Some("1")), page(2, true, None)],
        advertised: true,
    })
    .await;
    let result = client
        .edge_pages(&request(), limits())
        .await
        .expect("complete adjacency");
    let EdgeSearchResult::Page(result) = result else {
        panic!("raw page");
    };
    assert_eq!(result.groups[0].edges.len(), 2);
    assert!(result.groups[0].complete);
    assert_eq!(result.view.expires_at, "2099-01-01T00:00:00Z");
    assert!(result.sources[0].text.is_none());
    assert_eq!(result.sources[0].digest.as_str(), full_digest("..."));
    let requests = server.state.request_log.lock().await;
    let pages = requests
        .iter()
        .filter(|request| request.path.ends_with("/search"))
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 1);
    assert!(
        requests
            .iter()
            .all(|request| request.api_version.as_deref() == Some("2"))
    );
    let first: Value = serde_json::from_slice(&pages[0].body).expect("request");
    assert!(first.get("query").is_none());
    assert!(first.get("traversal").is_none());
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/resolutions"))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/symbols") || request.path.ends_with("/source"))
    );
}

#[tokio::test]
async fn complete_raw_pages_require_reached_symbol_and_physical_source_projections() {
    for omitted in ["symbols", "sources"] {
        let mut selected = page(0, true, None);
        selected[omitted] = json!([]);
        let (_server, client) = operation_client(OperationFixture::Edges {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        assert!(client.edge_pages(&request(), limits()).await.is_err());
    }
    let mut selected = page(0, true, None);
    selected["sources"][0]["text"] = json!("é");
    selected["sources"][0]["digest"] = json!(full_digest("é"));
    let mut request = request();
    request.include.source = true;
    let (_server, client) = operation_client(OperationFixture::Edges {
        pages: vec![selected],
        advertised: true,
    })
    .await;
    assert!(
        client.edge_pages(&request, limits()).await.is_err(),
        "byte range splits UTF-8"
    );
}

#[tokio::test]
async fn raw_edge_coverage_warning_admits_only_retained_witnesses() {
    let warn = json!([{"code":"edge_coverage_missing","detail":"Some adjacency is missing."}]);
    let mut partial = page(0, false, None);
    partial["warnings"] = warn.clone();
    let (_server, client) = operation_client(OperationFixture::Edges {
        pages: vec![partial.clone()],
        advertised: true,
    })
    .await;
    let result = client
        .edge_pages(&request(), limits())
        .await
        .expect("witness");
    let EdgeSearchResult::Page(result) = result else {
        panic!("partial edge page");
    };
    assert_eq!(result.groups[0].edges.len(), 1);
    assert!(!result.groups[0].complete);
    assert_eq!(
        serde_json::to_value(result.warnings).expect("warnings"),
        warn
    );

    partial["groups"][0]["edges"] = json!([]);
    let (_server, client) = operation_client(OperationFixture::Edges {
        pages: vec![partial],
        advertised: true,
    })
    .await;
    assert!(client.edge_pages(&request(), limits()).await.is_err());
}

#[tokio::test]
async fn raw_edge_failure_never_exposes_a_partial_batch() {
    let mut changed_view = page(2, true, None);
    changed_view["view"]["expires_at"] = json!("2000-01-01T00:00:00Z");
    let mut omitted = page(2, true, None);
    omitted["groups"] = json!([]);
    let mut incomplete = page(2, true, None);
    incomplete["groups"][0]["complete"] = json!(false);
    let mut wrong_subject = page(2, true, None);
    wrong_subject["groups"][0]["edges"][0]["edge"]["from"] = json!(TARGET);
    let mut changed_revision = page(2, true, None);
    changed_revision["corpus_revision"] = json!("");
    let mut duplicate = page(0, true, None);
    let record = duplicate["groups"][0]["edges"][0].clone();
    duplicate["groups"][0]["edges"]
        .as_array_mut()
        .expect("records")
        .push(record);
    let mut partial_warning = page(2, true, None);
    partial_warning["warnings"] = json!([{
        "code": "global_page_warning",
        "warning_code": "capability_unavailable"
    }]);
    for second in [
        changed_view,
        omitted,
        incomplete,
        wrong_subject,
        changed_revision,
        duplicate,
        partial_warning,
    ] {
        let (_server, client) = operation_client(OperationFixture::Edges {
            pages: vec![second],
            advertised: true,
        })
        .await;
        assert!(client.edge_pages(&request(), limits()).await.is_err());
    }
}

#[tokio::test]
async fn raw_edge_bound_and_capability_refusals_are_explicit() {
    let (server, client) = operation_client(OperationFixture::Edges {
        pages: vec![page(0, true, None)],
        advertised: false,
    })
    .await;
    assert!(client.edge_pages(&request(), limits()).await.is_err());
    assert!(
        !server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .any(|request| request.path.ends_with("/search"))
    );
    for bound in [
        crate::edges::EdgeReadLimits {
            requests_max: 0,
            ..limits()
        },
        crate::edges::EdgeReadLimits {
            records_max: 0,
            ..limits()
        },
        crate::edges::EdgeReadLimits {
            bytes_max: 1,
            ..limits()
        },
    ] {
        let (_server, client) = operation_client(OperationFixture::Edges {
            pages: vec![page(0, true, None)],
            advertised: true,
        })
        .await;
        assert!(client.edge_pages(&request(), bound).await.is_err());
    }
    let mut invalid = request();
    invalid.packages.clear();
    let (server, client) = operation_client(OperationFixture::Edges {
        pages: vec![],
        advertised: true,
    })
    .await;
    assert!(client.edge_pages(&invalid, limits()).await.is_err());
    assert!(server.state.request_log.lock().await.is_empty());
}

#[tokio::test]
async fn complete_empty_subject_is_authoritative_and_file_level_occurrence_survives() {
    let mut empty = page(0, true, None);
    empty["groups"][0]["edges"] = json!([]);
    let (_server, client) = operation_client(OperationFixture::Edges {
        pages: vec![empty],
        advertised: true,
    })
    .await;
    let EdgeSearchResult::Page(result) = client
        .edge_pages(&request(), limits())
        .await
        .expect("complete empty")
    else {
        panic!("page");
    };
    assert!(result.groups[0].complete);
    assert!(result.groups[0].edges.is_empty());
    let mut occurrence = page(0, true, None);
    occurrence["groups"][0]["edges"] = json!([]);
    occurrence["groups"][0]["occurrences"] = json!([{"id":"1".repeat(64),"occurrence":{"target":SEED,"source":{"unit":"rift://source/cargo/crates.io/demo/src/lib.rs","range":{"start":0,"end":1}},"kind":"references"}}]);
    let mut incoming = request();
    incoming.edges.direction = rift_protocol::read::TraversalDirection::Incoming;
    incoming.edges.kinds = vec![rift_protocol::read::EdgeKind::References];
    let (_server, client) = operation_client(OperationFixture::Edges {
        pages: vec![occurrence],
        advertised: true,
    })
    .await;
    let EdgeSearchResult::Page(result) = client
        .edge_pages(&incoming, limits())
        .await
        .expect("file-level occurrence")
    else {
        panic!("page");
    };
    assert_eq!(result.groups[0].occurrences.len(), 1);
    assert!(
        result.groups[0].occurrences[0]
            .occurrence
            .enclosing
            .is_none()
    );
}

fn edge_bounds() -> rift_protocol::edge_read::EdgeBounds {
    let configuration = rift_protocol::configuration::GlobalConfiguration::default();
    rift_protocol::edge_read::EdgeBounds {
        max_seeds: configuration.max_seeds,
        max_kinds: configuration.max_kinds,
        max_edges: configuration.max_edges,
        max_occurrences: configuration.max_occurrences,
        max_symbols: configuration.max_symbols,
        max_sources: configuration.max_sources,
    }
}

#[tokio::test]
async fn edge_page_windows_intersect_local_and_advertised_group_counts() {
    for occurrences in [false, true] {
        for (local, remote, expected) in [(100, 1000, 100), (1000, 50, 50)] {
            assert_edge_page_window(local, remote, expected, occurrences).await;
        }
    }
}

async fn assert_edge_page_window(local: usize, remote: u64, expected: i64, occurrences: bool) {
    let mut bounds = edge_bounds();
    let mut pages = vec![page(0, false, Some("1")), page(1, true, None)];
    if occurrences {
        bounds.max_occurrences = remote;
        for (index, page) in pages.iter_mut().enumerate() {
            let index = u64::try_from(index).expect("fixture page index");
            page["groups"][0]["occurrences"] =
                json!([located_projection(index, Some(index), index)]);
        }
    } else {
        bounds.max_edges = remote;
    }
    let (server, _) = operation_client(OperationFixture::EdgeBounds {
        pages,
        bounds,
        page_limit_min: crate::PAGE_LIMIT_MIN,
        page_limit_max: crate::PAGE_LIMIT_MAX,
    })
    .await;
    let mut configuration = server.config();
    if occurrences {
        configuration.max_occurrences = local;
    } else {
        configuration.max_edges = local;
    }
    let client = crate::GlobalClient::new(configuration).expect("accepted count policy");
    let mut admissions = 0;
    let result = client
        .edge_pages_with_admission(&request(), limits(), || {
            admissions += 1;
            Ok(())
        })
        .await
        .expect("complete selected pages");
    let EdgeSearchResult::Page(result) = result else {
        panic!("raw pages");
    };
    assert_eq!(admissions, 1);
    assert_eq!(result.groups[0].edges.len(), 2);
    assert_eq!(
        result.groups[0].occurrences.len(),
        usize::from(occurrences) * 2
    );
    assert_eq!(result.view.id.as_str(), "a".repeat(64));
    assert_eq!(result.view.expires_at, "2099-01-01T00:00:00Z");
    let requests = server.state.request_log.lock().await;
    let pages = requests
        .iter()
        .filter(|request| request.path.ends_with("/search"))
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 1);
    for page in &pages {
        assert!(
            page.query
                .as_deref()
                .expect("page query")
                .split('&')
                .any(|field| field == format!("max_results={expected}"))
        );
    }
    let first: Value = serde_json::from_slice(&pages[0].body).expect("selected request");
    assert_eq!(
        first["edges"],
        serde_json::to_value(request().edges).expect("selection")
    );
    assert!(first["view"].is_null());
}

fn located_projection(identity: u64, edge: Option<u64>, start: u64) -> Value {
    json!({"id":format!("{identity:064x}"), "edge":edge.map(|id|format!("{id:064x}")),
        "occurrence":{"target":TARGET,"enclosing":SEED,
        "source":{"unit":"rift://source/cargo/crates.io/demo/src/lib.rs","range":{"start":start,"end":start+1}},"kind":"calls"}})
}

#[tokio::test]
async fn merged_edge_providers_are_requested_and_default_edges_omit_them() {
    for included in [false, true] {
        let mut selected = page(0, true, None);
        if included {
            selected["groups"][0]["edges"][0]["edge"]["providers"] =
                json!(["engine", "lsp", "static"]);
        }
        let (server, client) = operation_client(OperationFixture::Edges {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        let mut request = request();
        request.include.providers = included;
        let result = client
            .edge_pages(&request, limits())
            .await
            .expect("merged edge page");
        let EdgeSearchResult::Page(result) = result else {
            panic!("raw page")
        };
        assert_eq!(result.groups[0].edges.len(), 1);
        let edge = serde_json::to_value(&result.groups[0].edges[0].edge).expect("wire edge");
        assert!(edge.get("provider").is_none());
        assert_eq!(
            edge.get("providers"),
            included
                .then(|| json!(["engine", "lsp", "static"]))
                .as_ref()
        );
        let requests = server.state.request_log.lock().await;
        let sent = requests
            .iter()
            .find(|request| request.path.ends_with("/search"))
            .expect("search request");
        let body: Value = serde_json::from_slice(&sent.body).expect("request body");
        assert_eq!(
            body.pointer("/include/providers"),
            included.then_some(&Value::Bool(true))
        );
    }
}

#[tokio::test]
async fn raw_edge_pages_refuse_unmerged_edges_and_singular_wire_provider() {
    for singular in [false, true] {
        let mut selected = page(0, true, None);
        if singular {
            selected["groups"][0]["edges"][0]["edge"]["provider"] = json!("static");
        } else {
            let mut duplicate = selected["groups"][0]["edges"][0].clone();
            duplicate["id"] = json!("b".repeat(64));
            selected["groups"][0]["edges"]
                .as_array_mut()
                .expect("edges")
                .push(duplicate);
        }
        let (_server, client) = operation_client(OperationFixture::Edges {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        assert!(client.edge_pages(&request(), limits()).await.is_err());
    }
}
