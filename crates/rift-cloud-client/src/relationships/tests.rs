use super::{OperationFixture, operation_client};
use rift_protocol::relationship_read::{RelationshipSearchRequest, RelationshipSearchResult};
use serde_json::{Value, json};

const SEED: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/entry";
const TARGET: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/helper";

fn counted_client(
    client: crate::GlobalClient,
) -> (
    crate::GlobalClient,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed = std::sync::Arc::clone(&attempts);
    (
        client.with_request_admission(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }),
        attempts,
    )
}

fn request() -> RelationshipSearchRequest {
    serde_json::from_value(json!({"relationships":{"seeds":[SEED],"direction":"outgoing","facets":["calls"]},"packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]})).expect("strict request")
}

fn relationship_bounds() -> rift_protocol::relationship_read::RelationshipBounds {
    let configuration = rift_protocol::configuration::GlobalConfiguration::default();
    rift_protocol::relationship_read::RelationshipBounds {
        max_seeds: configuration.max_seeds,
        max_facets: configuration.max_facets,
        max_relationships: configuration.max_relationships,
        max_occurrences: configuration.max_occurrences,
        max_symbols: configuration.max_symbols,
        max_sources: configuration.max_sources,
    }
}

#[tokio::test]
async fn relationship_page_windows_intersect_local_and_advertised_group_counts() {
    for occurrences in [false, true] {
        for (local, remote, expected) in [(100, 1000, 100), (1000, 50, 50)] {
            assert_relationship_page_window(local, remote, expected, occurrences).await;
        }
    }
}

async fn assert_relationship_page_window(
    local: usize,
    remote: u64,
    expected: i64,
    occurrences: bool,
) {
    let mut bounds = relationship_bounds();
    let mut pages = vec![page(0, false, Some("1")), page(1, true, None)];
    if occurrences {
        bounds.max_occurrences = remote;
        for (index, page) in pages.iter_mut().enumerate() {
            let index = u64::try_from(index).expect("fixture page index");
            page["groups"][0]["occurrences"] =
                json!([located_projection(index, Some(index), index)]);
        }
    } else {
        bounds.max_relationships = remote;
    }
    let (server, _) = operation_client(OperationFixture::RelationshipBounds {
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
        configuration.max_relationships = local;
    }
    let client = crate::GlobalClient::new(configuration).expect("accepted count policy");
    let mut admissions = 0;
    let result = client
        .relationship_pages_with_admission(&request(), limits(), || {
            admissions += 1;
            Ok(())
        })
        .await
        .expect("complete selected pages");
    let RelationshipSearchResult::Page(result) = result else {
        panic!("raw pages");
    };
    assert_eq!(admissions, 2);
    assert_eq!(result.groups[0].relationships.len(), 2);
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
    assert_eq!(pages.len(), 2);
    for page in &pages {
        assert!(
            page.query
                .as_deref()
                .expect("page query")
                .split('&')
                .any(|field| field == format!("limit={expected}"))
        );
    }
    let first: Value = serde_json::from_slice(&pages[0].body).expect("first selected request");
    let second: Value = serde_json::from_slice(&pages[1].body).expect("continued selected request");
    assert_eq!(first["relationships"], second["relationships"]);
    assert_eq!(second["view"], "a".repeat(64));
    assert!(first["view"].is_null());
    assert_eq!(second["packages"], first["packages"]);
}

#[tokio::test]
async fn relationship_page_windows_preserve_independent_generic_limit_refusal() {
    let mut bounds = relationship_bounds();
    bounds.max_relationships = 1;
    bounds.max_occurrences = 1;
    let (server, client) = operation_client(OperationFixture::RelationshipBounds {
        pages: vec![],
        bounds,
        page_limit_min: crate::PAGE_LIMIT_MIN,
        page_limit_max: 2,
    })
    .await;
    assert_eq!(
        client
            .relationship_page(&request(), 3, None)
            .await
            .expect_err("generic advertised limit remains independent")
            .slug()
            .as_str(),
        "rift.client.request_invalid"
    );
    assert!(
        !server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .any(|request| request.path.ends_with("/search"))
    );
}

#[tokio::test]
async fn relationship_page_windows_keep_generic_minimum_above_remote_group_counts() {
    let mut bounds = relationship_bounds();
    bounds.max_relationships = 1;
    bounds.max_occurrences = 1;
    let (server, _) = operation_client(OperationFixture::RelationshipBounds {
        pages: vec![page(0, false, Some("1")), page(1, true, None)],
        bounds,
        page_limit_min: 10,
        page_limit_max: crate::PAGE_LIMIT_MAX,
    })
    .await;
    let mut configuration = server.config();
    configuration.max_relationships = 100;
    configuration.max_occurrences = 100;
    let client = crate::GlobalClient::new(configuration).expect("accepted count policy");
    let result = client
        .relationship_pages(&request(), limits())
        .await
        .expect("remote groups below the generic minimum remain valid");
    let RelationshipSearchResult::Page(result) = result else {
        panic!("raw pages");
    };
    assert_eq!(result.groups[0].relationships.len(), 2);
    assert_eq!(result.view.id.as_str(), "a".repeat(64));
    assert_eq!(result.view.expires_at, "2099-01-01T00:00:00Z");
    let requests = server.state.request_log.lock().await;
    let pages = requests
        .iter()
        .filter(|request| request.path.ends_with("/search"))
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 2);
    for page in pages {
        assert!(
            page.query
                .as_deref()
                .expect("page query")
                .split('&')
                .any(|field| field == "limit=10")
        );
    }
}

#[tokio::test]
async fn relationship_page_windows_preserve_local_group_refusal_above_generic_minimum() {
    for occurrences in [false, true] {
        let mut selected = page(0, true, None);
        if occurrences {
            selected["groups"][0]["occurrences"] = json!([
                located_projection(0, Some(0), 0),
                located_projection(0, Some(0), 1),
            ]);
        } else {
            let mut second = selected["groups"][0]["relationships"][0].clone();
            second["id"] = json!(format!("{:064x}", 1));
            selected["groups"][0]["relationships"]
                .as_array_mut()
                .expect("records")
                .push(second);
        }
        let (server, _) = operation_client(OperationFixture::RelationshipBounds {
            pages: vec![selected],
            bounds: relationship_bounds(),
            page_limit_min: 10,
            page_limit_max: crate::PAGE_LIMIT_MAX,
        })
        .await;
        let mut configuration = server.config();
        if occurrences {
            configuration.max_occurrences = 1;
        } else {
            configuration.max_relationships = 1;
        }
        let client = crate::GlobalClient::new(configuration).expect("accepted count policy");
        assert_eq!(
            client
                .relationship_pages(&request(), limits())
                .await
                .expect_err("local group count remains strict")
                .slug()
                .as_str(),
            "rift.client.response_invalid"
        );
        let requests = server.state.request_log.lock().await;
        let pages = requests
            .iter()
            .filter(|request| request.path.ends_with("/search"))
            .collect::<Vec<_>>();
        assert_eq!(pages.len(), 1);
        assert!(
            pages[0]
                .query
                .as_deref()
                .expect("page query")
                .split('&')
                .any(|field| field == "limit=10")
        );
    }
}

#[tokio::test]
async fn relationship_request_counts_use_configured_exact_bounds() {
    let (server, _) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, true, None)],
        advertised: true,
    })
    .await;
    let mut configuration = server.config();
    configuration.max_seeds = 1;
    configuration.max_facets = 1;
    let client = crate::GlobalClient::new(configuration).expect("configured counts");
    client
        .relationship_page(&request(), 10, None)
        .await
        .expect("exact subject and facet counts");
    let mut too_many = request();
    too_many
        .relationships
        .seeds
        .push(rift_protocol::read::SymbolId(TARGET.to_owned()));
    assert_eq!(
        client
            .relationship_page(&too_many, 10, None)
            .await
            .expect_err("one extra subject")
            .slug()
            .as_str(),
        "rift.client.request_invalid"
    );
    let mut too_many = request();
    too_many
        .relationships
        .facets
        .push(rift_protocol::read::RelationshipFacet::References);
    assert_eq!(
        client
            .relationship_page(&too_many, 10, None)
            .await
            .expect_err("one extra facet")
            .slug()
            .as_str(),
        "rift.client.request_invalid"
    );
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
async fn relationship_request_byte_refusal_precedes_continuation_copy_and_send() {
    let (server, _) = operation_client(OperationFixture::Relationships {
        pages: vec![],
        advertised: true,
    })
    .await;
    let mut configuration = server.config();
    configuration.max_seeds = usize::MAX;
    configuration.max_request = 128;
    let client = crate::GlobalClient::new(configuration).expect("configured counts and bytes");
    let mut selected = request();
    selected.relationships.seeds = (0..64)
        .map(|index| {
            rift_protocol::read::SymbolId(format!(
                "rift://symbol/cargo/crates.io/demo@1.0.0/rust/entry{index}"
            ))
        })
        .collect();
    let original = serde_json::to_value(&selected).expect("original request");
    let mut admissions = 0;
    let error = client
        .relationship_pages_with_admission(&selected, limits(), || {
            admissions += 1;
            Ok(())
        })
        .await
        .expect_err("request byte bound");
    assert_eq!(error.slug().as_str(), "rift.client.body_limit");
    assert_eq!(admissions, 1);
    assert_eq!(
        serde_json::to_value(&selected).expect("unchanged request"),
        original
    );
    assert!(
        !server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .any(|request| request.path.ends_with("/search"))
    );
}

#[tokio::test]
async fn relationship_group_counts_use_configured_exact_bounds() {
    for occurrences in [false, true] {
        let mut selected = page(0, true, None);
        if occurrences {
            selected["groups"][0]["occurrences"] = json!([
                located_projection(0, Some(0), 0),
                located_projection(0, Some(0), 1),
            ]);
        } else {
            let mut second = selected["groups"][0]["relationships"][0].clone();
            second["id"] = json!(format!("{:064x}", 1));
            selected["groups"][0]["relationships"]
                .as_array_mut()
                .expect("records")
                .push(second);
        }
        let (server, _) = operation_client(OperationFixture::Relationships {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        for maximum in [2, 1] {
            let mut configuration = server.config();
            if occurrences {
                configuration.max_occurrences = maximum;
            } else {
                configuration.max_relationships = maximum;
            }
            let client = crate::GlobalClient::new(configuration).expect("group count policy");
            let result = client.relationship_page(&request(), 10, None).await;
            if maximum == 2 {
                result.expect("exact configured group count");
            } else {
                assert_eq!(
                    result
                        .expect_err("one over configured group count")
                        .slug()
                        .as_str(),
                    "rift.client.response_invalid"
                );
            }
        }
    }
}

#[tokio::test]
async fn relationship_bounds_require_remote_policy_and_intersect_local_counts() {
    let (server, _) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, true, None)],
        advertised: true,
    })
    .await;
    let mut configuration = server.config();
    configuration.max_seeds = 2;
    configuration.max_facets = 2;
    configuration.max_relationships = 2;
    configuration.max_occurrences = 2;
    configuration.max_symbols = 2;
    configuration.max_sources = 2;
    let client = crate::GlobalClient::new(configuration).expect("local counts");
    let mut value: Value =
        serde_json::from_str(&super::capabilities_json()).expect("fixture metadata");
    value["supported_features"] = json!(["relationships"]);
    value["relationship_capabilities"] = json!([{
        "direction":"outgoing", "facets":["calls"], "bounds":{
            "max_seeds":1, "max_facets":1, "max_relationships":1,
            "max_occurrences":1, "max_symbols":1, "max_sources":1,
        }
    }]);
    let capabilities: crate::Capabilities =
        serde_json::from_value(value.clone()).expect("advertised counts");
    let bounds = client
        .relationship_bounds(
            &capabilities,
            rift_protocol::read::TraversalDirection::Outgoing,
            &[rift_protocol::read::RelationshipFacet::Calls],
        )
        .expect("intersection");
    assert_eq!(
        serde_json::to_value(bounds).expect("active counts"),
        json!({"max_seeds":1,"max_facets":1,"max_relationships":1,"max_occurrences":1,"max_symbols":1,"max_sources":1})
    );
    value["relationship_capabilities"][0]
        .as_object_mut()
        .expect("operation")
        .remove("bounds");
    let capabilities: crate::Capabilities =
        serde_json::from_value(value).expect("old ordinary metadata remains valid");
    assert!(crate::GlobalClient::relationship_capabilities(&capabilities).is_ok());
    let error = client
        .relationship_bounds(
            &capabilities,
            rift_protocol::read::TraversalDirection::Outgoing,
            &[rift_protocol::read::RelationshipFacet::Calls],
        )
        .expect_err("old metadata cannot establish raw count policy");
    assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
}

#[tokio::test]
async fn cached_relationship_capability_does_not_retry_an_unsupported_operation() {
    let problem = json!({
        "type":"about:blank", "title":"Service unavailable", "status":503,
        "detail":"Service unavailable", "code":"rift.server.read_unsupported"
    });
    let (server, client) = operation_client(OperationFixture::RelationshipRefusal {
        value: problem,
        problem: true,
    })
    .await;
    let (client, attempts) = counted_client(client);
    client
        .get_capabilities()
        .await
        .expect("cached supported capability");
    let error = client
        .relationship_pages(&request(), limits())
        .await
        .expect_err("unsupported operation");
    assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
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
async fn missing_relationship_coverage_keeps_its_typed_reason() {
    let value = json!({"outcome":"unavailable", "reason":"insufficient_coverage"});
    let (server, client) = operation_client(OperationFixture::RelationshipRefusal {
        value: value.clone(),
        problem: false,
    })
    .await;
    let (client, attempts) = counted_client(client);
    let answer = client
        .relationship_pages(&request(), limits())
        .await
        .expect("typed unavailable");
    assert_eq!(serde_json::to_value(answer).expect("typed result"), value);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
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
async fn unknown_problem_codes_and_non_problem_media_do_not_authorize_unsupported() {
    for (code, problem, expected) in [
        ("rift.server.other", true, "rift.client.remote_unavailable"),
        (
            "rift.server.read_unsupported",
            false,
            "rift.client.response_invalid",
        ),
    ] {
        let (server, client) = operation_client(OperationFixture::RelationshipRefusal {
            value: json!({"type":"about:blank", "title":"Service unavailable", "status":503,
                "detail":"Service unavailable", "code":code}),
            problem,
        })
        .await;
        let error = client
            .relationship_pages(&request(), limits())
            .await
            .expect_err("registered response boundary");
        assert_eq!(error.slug().as_str(), expected);
        let requests = server.state.request_log.lock().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path.ends_with("/search"))
                .count(),
            usize::try_from(server.config().attempts).expect("configured attempt bound"),
        );
    }
}

fn page(start: u64, complete: bool, next: Option<&str>) -> Value {
    json!({"groups":[{"seed":SEED,"complete":complete,"relationships":[{"id":format!("{start:064x}"),"relationship":{"from":SEED,"to":TARGET,"kind":"call","facets":["calls"],"derivation":"resolution","occurrence":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":start,"end":start+1}}}}]}],
        "symbols":[symbol(SEED,"entry"),symbol(TARGET,"helper")],
        "sources":[{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","digest":full_digest("...")}],
        "next_cursor":next,"analyzer_revision":"analyzer-v1","corpus_revision":"corpus-v1","view":{"id":"a".repeat(64),"expires_at":"2099-01-01T00:00:00Z"}})
}

fn full_digest(text: &str) -> String {
    data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes())
}

fn located_projection(identity: u64, relationship: Option<u64>, start: u64) -> Value {
    json!({"id":format!("{identity:064x}"), "relationship":relationship.map(|id|format!("{id:064x}")),
        "occurrence":{"target":TARGET,"enclosing":SEED,
        "source":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":start,"end":start+1}},"facet":"calls"}})
}

#[tokio::test]
async fn page_limit_counts_original_ids_and_preserves_every_physical_projection() {
    let mut selected = page(0, true, None);
    selected["groups"][0]["occurrences"] = json!([
        located_projection(0, Some(0), 0),
        located_projection(0, Some(0), 1)
    ]);
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![selected.clone()],
        advertised: true,
    })
    .await;
    let RelationshipSearchResult::Page(answer) = client
        .relationship_page(&request(), 1, None)
        .await
        .expect("one original record")
    else {
        panic!("page");
    };
    assert_eq!(answer.groups[0].occurrences.len(), 2);
    selected["groups"][0]["occurrences"][1]["id"] = json!(format!("{:064x}", 1));
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![selected.clone()],
        advertised: true,
    })
    .await;
    assert!(client.relationship_page(&request(), 1, None).await.is_err());
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![selected],
        advertised: true,
    })
    .await;
    assert!(client.relationship_page(&request(), 2, None).await.is_ok());
}

#[tokio::test]
async fn original_usage_replay_and_serialized_bytes_remain_bounded() {
    let mut first = page(0, false, Some("1"));
    first["groups"][0]["occurrences"] = json!([located_projection(0, Some(0), 0)]);
    let mut second = page(1, true, None);
    second["groups"][0]["relationships"] = json!([]);
    second["groups"][0]["occurrences"] = json!([located_projection(0, Some(0), 1)]);
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![first.clone(), second],
        advertised: true,
    })
    .await;
    assert!(
        client
            .relationship_pages(&request(), limits())
            .await
            .is_err()
    );
    first["groups"][0]["complete"] = json!(true);
    first["next_cursor"] = Value::Null;
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![first],
        advertised: true,
    })
    .await;
    let mut bounded = limits();
    bounded.bytes_max = 1;
    assert!(
        client
            .relationship_pages(&request(), bounded)
            .await
            .is_err()
    );
}

fn symbol(id: &str, name: &str) -> Value {
    json!({"symbol":{"id":id,"name":name,"language":"rust","kind":"function","origin":{"source_kind":"synthetic"}},"declarations":[]})
}

fn limits() -> crate::relationships::RelationshipReadLimits {
    crate::relationships::RelationshipReadLimits {
        requests_max: 8,
        records_max: 100,
        bytes_max: 1_048_576,
    }
}

#[tokio::test]
async fn raw_relationship_pages_preserve_parallel_records_and_original_selection() {
    let (server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, false, Some("1")), page(2, true, None)],
        advertised: true,
    })
    .await;
    let result = client
        .relationship_pages(&request(), limits())
        .await
        .expect("complete adjacency");
    let RelationshipSearchResult::Page(result) = result else {
        panic!("raw page");
    };
    assert_eq!(result.groups[0].relationships.len(), 2);
    assert!(result.groups[0].complete);
    assert_eq!(result.view.expires_at, "2099-01-01T00:00:00Z");
    assert!(result.sources[0].text.is_none());
    assert_eq!(
        result.sources[0]
            .digest
            .as_ref()
            .expect("catalog digest")
            .as_str(),
        full_digest("...")
    );
    let requests = server.state.request_log.lock().await;
    let pages = requests
        .iter()
        .filter(|request| request.path.ends_with("/search"))
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 2);
    let first: Value = serde_json::from_slice(&pages[0].body).expect("request");
    let second: Value = serde_json::from_slice(&pages[1].body).expect("request");
    assert_eq!(first["relationships"], second["relationships"]);
    assert!(first.get("query").is_none());
    assert!(first.get("traversal").is_none());
    assert_eq!(second["view"], "a".repeat(64));
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
async fn request_admission_counts_http_pages_and_keeps_cached_capabilities_free() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let (server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, false, Some("1")), page(2, true, None)],
        advertised: true,
    })
    .await;
    let attempts = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&attempts);
    let client = client.with_request_admission(move || {
        let attempt = observed.fetch_add(1, Ordering::Relaxed);
        if attempt >= 2 {
            return Err(crate::failure::request_invalid(
                "query",
                "requests",
                "request bound reached",
            ));
        }
        Ok(())
    });
    client
        .get_capabilities()
        .await
        .expect("first capability HTTP request");
    client
        .get_capabilities()
        .await
        .expect("cached capability answer");
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(
        client
            .relationship_pages(&request(), limits())
            .await
            .is_err()
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 3);
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
async fn equal_payload_records_keep_distinct_full_identities_with_the_same_short_prefix() {
    let mut first = page(0, false, Some("1"));
    let mut second = page(0, true, None);
    first["groups"][0]["relationships"][0]["id"] = json!(format!("12345678{}", "a".repeat(56)));
    second["groups"][0]["relationships"][0]["id"] = json!(format!("12345678{}", "b".repeat(56)));
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![first, second],
        advertised: true,
    })
    .await;
    let RelationshipSearchResult::Page(answer) = client
        .relationship_pages(&request(), limits())
        .await
        .expect("distinct original records")
    else {
        panic!("page");
    };
    assert_eq!(answer.groups[0].relationships.len(), 2);
    assert_eq!(
        answer.groups[0].relationships[0].relationship,
        answer.groups[0].relationships[1].relationship
    );
    assert_ne!(
        answer.groups[0].relationships[0].id,
        answer.groups[0].relationships[1].id
    );
    assert_eq!(answer.symbols.len(), 2);
    assert_eq!(answer.sources.len(), 1);
}

#[tokio::test]
async fn complete_raw_pages_require_reached_symbol_and_physical_source_projections() {
    for omitted in ["symbols", "sources"] {
        let mut selected = page(0, true, None);
        selected[omitted] = json!([]);
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        assert!(
            client
                .relationship_pages(&request(), limits())
                .await
                .is_err()
        );
    }
    let mut selected = page(0, true, None);
    selected["sources"][0]["text"] = json!("é");
    selected["sources"][0]["digest"] = json!(full_digest("é"));
    let mut request = request();
    request.include = vec![rift_protocol::read::GetSymbolInclude::Source];
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![selected],
        advertised: true,
    })
    .await;
    assert!(
        client.relationship_pages(&request, limits()).await.is_err(),
        "byte range splits UTF-8"
    );
}

#[tokio::test]
async fn full_source_digest_is_required_and_short_prefix_agreement_never_proves_selected_bytes() {
    let original = "source-46399";
    let changed = "source-60140";
    let first_hash = full_digest(original);
    let second_hash = full_digest(changed);
    assert_eq!(&first_hash[..8], &second_hash[..8]);
    assert_ne!(first_hash, second_hash);
    let mut first = page(0, false, Some("1"));
    first["sources"][0]["digest"] = json!(first_hash);
    let mut second = page(2, true, None);
    second["sources"][0]["digest"] = json!(second_hash);
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![first, second],
        advertised: true,
    })
    .await;
    assert!(
        client
            .relationship_pages(&request(), limits())
            .await
            .is_err()
    );
    for digest in [Value::Null, json!("38be2be5"), json!(first_hash)] {
        let mut selected = page(0, true, None);
        selected["sources"][0]["text"] = json!(changed);
        selected["sources"][0]["digest"] = digest;
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        let mut selected = request();
        selected.include = vec![rift_protocol::read::GetSymbolInclude::Source];
        assert!(
            client
                .relationship_pages(&selected, limits())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn source_projection_keeps_exact_crlf_unicode_and_missing_final_newline() {
    for (original, changed) in [
        ("...\r\n", "...\n"),
        ("...", "...\n"),
        ("...é", "...e\u{301}"),
    ] {
        let mut selected = page(0, true, None);
        selected["sources"][0]["text"] = json!(original);
        selected["sources"][0]["digest"] = json!(full_digest(original));
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![selected.clone()],
            advertised: true,
        })
        .await;
        let mut request = request();
        request.include = vec![rift_protocol::read::GetSymbolInclude::Source];
        let RelationshipSearchResult::Page(answer) = client
            .relationship_pages(&request, limits())
            .await
            .expect("exact physical bytes")
        else {
            panic!("page")
        };
        assert_eq!(
            answer.sources[0]
                .text
                .as_deref()
                .expect("source bytes")
                .as_bytes(),
            original.as_bytes()
        );
        selected["sources"][0]["digest"] = json!(full_digest(changed));
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![selected],
            advertised: true,
        })
        .await;
        assert!(client.relationship_pages(&request, limits()).await.is_err());
    }
}

#[tokio::test]
async fn raw_relationship_failure_never_exposes_a_partial_batch() {
    let original = page(0, false, Some("1"));
    let mut changed_view = page(2, true, None);
    changed_view["view"]["id"] = json!("b".repeat(64));
    let mut omitted = page(2, true, None);
    omitted["groups"] = json!([]);
    let mut incomplete = page(2, true, None);
    incomplete["groups"][0]["complete"] = json!(false);
    let mut wrong_subject = page(2, true, None);
    wrong_subject["groups"][0]["relationships"][0]["relationship"]["from"] = json!(TARGET);
    let mut changed_revision = page(2, true, None);
    changed_revision["corpus_revision"] = json!("corpus-v2");
    let duplicate = page(0, true, None);
    for second in [
        changed_view,
        omitted,
        incomplete,
        wrong_subject,
        changed_revision,
        duplicate,
    ] {
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![original.clone(), second],
            advertised: true,
        })
        .await;
        assert!(
            client
                .relationship_pages(&request(), limits())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn raw_relationship_bound_and_capability_refusals_are_explicit() {
    let (server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, true, None)],
        advertised: false,
    })
    .await;
    assert!(
        client
            .relationship_pages(&request(), limits())
            .await
            .is_err()
    );
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
        crate::relationships::RelationshipReadLimits {
            requests_max: 0,
            ..limits()
        },
        crate::relationships::RelationshipReadLimits {
            records_max: 0,
            ..limits()
        },
        crate::relationships::RelationshipReadLimits {
            bytes_max: 1,
            ..limits()
        },
    ] {
        let (_server, client) = operation_client(OperationFixture::Relationships {
            pages: vec![page(0, true, None)],
            advertised: true,
        })
        .await;
        assert!(client.relationship_pages(&request(), bound).await.is_err());
    }
    let mut invalid = request();
    invalid.packages.clear();
    let (server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![],
        advertised: true,
    })
    .await;
    assert!(client.relationship_pages(&invalid, limits()).await.is_err());
    assert!(server.state.request_log.lock().await.is_empty());
}

#[tokio::test]
async fn configured_catalog_counts_admit_exact_pages_and_refuse_one_above() {
    for (symbols, sources, supplied_sources, accepted) in [
        (2, 1, 1, true),
        (1, 1, 1, false),
        (2, 1, 2, false),
        (2, 2, 2, true),
    ] {
        let mut answer = page(0, true, None);
        if supplied_sources == 2 {
            let mut source = answer["sources"][0].clone();
            source["unit"] = json!("rift://source/cargo/crates.io/demo@1.0.0/src/other.rs");
            answer["sources"]
                .as_array_mut()
                .expect("source catalog")
                .push(source);
        }
        let (_server, base) = operation_client(OperationFixture::Relationships {
            pages: vec![answer],
            advertised: true,
        })
        .await;
        let mut configuration = base.inner.config.clone();
        configuration.max_symbols = symbols;
        configuration.max_sources = sources;
        let client = crate::GlobalClient::new_with_token(configuration, None)
            .expect("accepted catalog counts");
        let result = client.relationship_page(&request(), 200, None).await;
        assert_eq!(
            matches!(result, Ok(RelationshipSearchResult::Page(_))),
            accepted,
            "configured symbols {symbols}, sources {sources}, supplied sources {supplied_sources}: {result:?}",
        );
    }
}

#[test]
fn protocol_catalog_counts_reach_client_configuration_and_refuse_zero() {
    let mut source = rift_protocol::configuration::GlobalConfiguration {
        max_symbols: 2,
        max_sources: 3,
        ..Default::default()
    };
    let configured = crate::Config::try_from(&source).expect("accepted protocol catalog counts");
    assert_eq!(configured.max_symbols, 2);
    assert_eq!(configured.max_sources, 3);
    source.max_symbols = 0;
    assert!(crate::Config::try_from(&source).is_err());
    source.max_symbols = 2;
    source.max_sources = 0;
    assert!(crate::Config::try_from(&source).is_err());
}

#[tokio::test]
async fn catalog_counts_apply_per_page_while_existing_aggregate_budgets_remain() {
    let first = page(0, false, Some("1"));
    let mut second = page(1, true, None);
    let other = format!("{TARGET}_second");
    second["groups"][0]["relationships"][0]["relationship"]["to"] = json!(other);
    second["symbols"][1] = symbol(&other, "other");
    second["sources"][0]["unit"] = json!("rift://source/cargo/crates.io/demo@1.0.0/src/other.rs");
    let (_server, base) = operation_client(OperationFixture::Relationships {
        pages: vec![first, second],
        advertised: true,
    })
    .await;
    let mut configuration = base.inner.config.clone();
    configuration.max_symbols = 2;
    configuration.max_sources = 1;
    let client =
        crate::GlobalClient::new_with_token(configuration, None).expect("accepted per-page counts");
    let RelationshipSearchResult::Page(answer) = client
        .relationship_pages(&request(), limits())
        .await
        .expect("each page is within the accepted count and aggregate byte bounds")
    else {
        panic!("complete captured relationship pages");
    };
    assert_eq!(answer.symbols.len(), 3);
    assert_eq!(answer.sources.len(), 2);
}

#[tokio::test]
async fn relationship_capability_direction_and_every_required_facet_are_explicit() {
    let (server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![page(0, true, None)],
        advertised: true,
    })
    .await;
    let capabilities = client.get_capabilities().await.expect("capabilities");
    let mut old = capabilities.clone();
    old.relationship_capabilities = None;
    assert!(
        crate::GlobalClient::relationship_capabilities(&old)
            .expect("older metadata")
            .is_empty()
    );
    let mut duplicate = capabilities.clone();
    duplicate.relationship_capabilities = Some(
        serde_json::from_value(json!([
            {"direction":"incoming","facets":["references","references"]}
        ]))
        .expect("generated capability fields"),
    );
    assert!(crate::GlobalClient::relationship_capabilities(&duplicate).is_err());
    let mut selected = request();
    selected
        .relationships
        .facets
        .push(rift_protocol::read::RelationshipFacet::Implements);
    assert!(
        client
            .relationship_pages(&selected, limits())
            .await
            .is_err()
    );
    selected.relationships.facets = vec![rift_protocol::read::RelationshipFacet::Calls];
    selected.relationships.direction = rift_protocol::read::TraversalDirection::Incoming;
    assert!(
        client
            .relationship_pages(&selected, limits())
            .await
            .is_err()
    );
    assert!(
        !server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .any(|request| request.path.ends_with("/search"))
    );
}

#[tokio::test]
async fn complete_empty_subject_is_authoritative_and_file_level_occurrence_survives() {
    let mut empty = page(0, true, None);
    empty["groups"][0]["relationships"] = json!([]);
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![empty],
        advertised: true,
    })
    .await;
    let RelationshipSearchResult::Page(result) = client
        .relationship_pages(&request(), limits())
        .await
        .expect("complete empty")
    else {
        panic!("page");
    };
    assert!(result.groups[0].complete);
    assert!(result.groups[0].relationships.is_empty());
    let mut occurrence = page(0, true, None);
    occurrence["groups"][0]["relationships"] = json!([]);
    occurrence["groups"][0]["occurrences"] = json!([{"id":"1".repeat(64),"occurrence":{"target":SEED,"source":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":0,"end":1}},"facet":"references"}}]);
    let mut incoming = request();
    incoming.relationships.direction = rift_protocol::read::TraversalDirection::Incoming;
    incoming.relationships.facets = vec![rift_protocol::read::RelationshipFacet::References];
    let (_server, client) = operation_client(OperationFixture::Relationships {
        pages: vec![occurrence],
        advertised: true,
    })
    .await;
    let RelationshipSearchResult::Page(result) = client
        .relationship_pages(&incoming, limits())
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

#[test]
fn shared_contract_sync_is_idempotent_and_preserves_authored_operations() {
    let directory = tempfile::tempdir().expect("contract directory");
    let path = directory.path().join("global-api.openapi.json");
    let original = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/public/global-api.openapi.json");
    let before: Value =
        serde_json::from_slice(&std::fs::read(&original).expect("authored contract"))
            .expect("contract JSON");
    std::fs::copy(original, &path).expect("copy contract");
    crate::contract::synchronize(&path).expect("sync shared schemas");
    let first = std::fs::read(&path).expect("first contract");
    crate::contract::synchronize(&path).expect("repeat shared sync");
    assert_eq!(std::fs::read(&path).expect("second contract"), first);
    let after: Value = serde_json::from_slice(&first).expect("synchronized JSON");
    for (endpoint, operation) in before["paths"].as_object().expect("authored operations") {
        if endpoint != "/v1/search" {
            assert_eq!(&after["paths"][endpoint], operation);
        }
    }
    assert_eq!(
        after["paths"]["/v1/search"]["post"]["requestBody"],
        before["paths"]["/v1/search"]["post"]["requestBody"]
    );
    assert_eq!(
        after["paths"]["/v1/search"]["post"]["responses"]["200"],
        before["paths"]["/v1/search"]["post"]["responses"]["200"]
    );
    crate::contract::validate(&path).expect("shared Rust schemas match the contract");
}
