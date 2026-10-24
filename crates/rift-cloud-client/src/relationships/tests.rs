use super::{OperationFixture, operation_client};
use rift_protocol::relationship_read::{RelationshipSearchRequest, RelationshipSearchResult};
use serde_json::{Value, json};

const SEED: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/entry";
const TARGET: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/helper";

fn request() -> RelationshipSearchRequest {
    serde_json::from_value(json!({"relationships":{"seeds":[SEED],"direction":"outgoing","facets":["calls"]},"packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]})).expect("strict request")
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
    occurrence["groups"][0]["occurrences"] = json!([{"target":SEED,"source":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":0,"end":1}},"facet":"references"}]);
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
    assert!(result.groups[0].occurrences[0].enclosing.is_none());
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
