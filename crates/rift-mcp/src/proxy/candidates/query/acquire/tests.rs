use super::*;
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::symbol_read::SymbolUnavailableReason;
use serde_json::json;
use std::time::{Duration, SystemTime};

struct Fixture {
    root: tempfile::TempDir,
    issuer: rift_server::LocalSymbolViews,
    selected: QueryFactSelection,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("workspace");
    crate::server::hermetic_workspace(root.path(), "").expect("accepted workspace");
    let reads = rift_server::ReadService::build(
        root.path(),
        rift_index::WorkspaceIndexLimits::default(),
        &rift_core::SourceVisibility::default(),
        &rift_core::TextFileInclusion::default(),
        rift_protocol::configuration::HistoryConfiguration::default(),
    )
    .expect("published workspace");
    let input = reads
        .local_search_view_input(true)
        .expect("original publication");
    let selection = input
        .provider_selection(&WorkspaceConfiguration::default())
        .expect("original provider selection");
    let original_root = input.root().to_path_buf();
    let issuer = rift_server::LocalSymbolViews::new(1 << 20, [53; 32]);
    let view = issuer
        .capture_query_with(
            rift_server::QueryViewInput::new(
                original_root.clone(),
                SymbolOwner::Local,
                SystemTime::now() + Duration::from_secs(300),
            )
            .expect("original selection"),
            |_| Ok(true),
        )
        .expect("issued view")
        .expect("retained view");
    let selected = QueryFactSelection {
        root: original_root.to_str().expect("UTF-8 workspace").to_owned(),
        owner: SymbolOwner::Local,
        view,
        selection,
        requests: 2,
        facts: rift_search::query::QueryFacts::default(),
        services: rift_search::sparql::QueryServices::default(),
        warnings: Vec::new(),
        upstream: Vec::new(),
    };
    Fixture {
        root,
        issuer,
        selected,
    }
}

fn encoded(selected: QueryFactSelection) -> serde_json::Value {
    serde_json::to_value(rift_server::QueryExchange::new(QueryFactResult::Facts(
        Box::new(selected),
    )))
    .expect("current exchange")
}

#[test]
fn fact_response_rejects_unknown_fields_and_incompatible_exchange() {
    let fixture = fixture();
    let valid = encoded(fixture.selected);
    assert!(matches!(
        decode_response(CallToolResult::structured(valid.clone())),
        Ok(QueryFactResult::Facts(_))
    ));
    for field in ["results", "unexpected", "supplied_facts"] {
        let mut changed = valid.clone();
        changed["payload"]["response"][field] = json!([]);
        let error = decode_response(CallToolResult::structured(changed))
            .err()
            .expect("unknown selected field");
        assert_eq!(error.slug(), errors::server::read_unsupported::SLUG);
    }
    for changed in [
        valid["payload"].clone(),
        {
            let mut changed = valid.clone();
            changed["version"] = json!(0);
            changed
        },
        {
            let mut changed = valid;
            changed["unexpected"] = json!(true);
            changed
        },
    ] {
        let error = decode_response(CallToolResult::structured(changed))
            .err()
            .expect("incompatible exchange");
        assert_eq!(error.slug(), errors::server::read_unsupported::SLUG);
    }
}

#[test]
fn fact_response_preserves_registered_unavailable_outcomes() {
    for reason in [
        SymbolUnavailableReason::IndexPreparing,
        SymbolUnavailableReason::ViewExpired,
        SymbolUnavailableReason::ViewContext,
        SymbolUnavailableReason::ReadFailed,
    ] {
        let unavailable = rift_protocol::search_read::SearchUnavailable::Unavailable {
            reason,
            view: None,
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(rift_server::QueryExchange::new(
            QueryFactResult::Unavailable(unavailable),
        ))
        .expect("registered unavailable outcome");
        let QueryFactResult::Unavailable(actual) =
            decode_response(CallToolResult::structured(value)).expect("compatible outcome")
        else {
            panic!("unavailable outcome");
        };
        let rift_protocol::search_read::SearchUnavailable::Unavailable { reason: actual, .. } =
            actual;
        assert_eq!(actual, reason);
    }
}

#[test]
fn fact_response_requires_accepted_root_owner_scope_expiry_and_request_allowance() {
    let fixture = fixture();
    let root = fixture.selected.root.clone();
    let owner = fixture.selected.owner.clone();
    let valid = serde_json::to_value(&fixture.selected).expect("selected facts");
    validate_selection(&fixture.selected, &root, &owner, 2, SystemTime::now())
        .expect("original accepted selection");
    for field in [
        "root",
        "owner",
        "selection_owner",
        "selection_root",
        "requests",
    ] {
        let mut changed = valid.clone();
        let foreign = SymbolOwner::NamedLocal {
            name: "other".into(),
        };
        match field {
            "root" => changed["root"] = json!(format!("{root}/other")),
            "owner" => changed["owner"] = serde_json::to_value(&foreign).expect("owner"),
            "selection_owner" => {
                changed["selection"]["owner"] = serde_json::to_value(&foreign).expect("owner");
            }
            "selection_root" => {
                let other = rift_server::query_provider_root(&fixture.root.path().join("other"))
                    .expect("workspace URI");
                changed["selection"]["root"] = json!(other.as_str());
            }
            "requests" => changed["requests"] = json!(3),
            _ => unreachable!(),
        }
        let changed = serde_json::from_value(changed).expect("well-formed foreign selection");
        assert!(
            validate_selection(&changed, &root, &owner, 2, SystemTime::now()).is_err(),
            "{field}"
        );
    }
    assert!(
        validate_selection(
            &fixture.selected,
            &root,
            &owner,
            2,
            crate::proxy::captured_expiry(&fixture.selected.view).expect("original expiry"),
        )
        .is_err()
    );
}

#[test]
fn retained_selection_keeps_distinct_configuration_under_the_same_root() {
    let mut fixture = fixture();
    let original_root = std::path::PathBuf::from(&fixture.selected.root);
    fixture
        .selected
        .facts
        .sources
        .push(rift_search::query::QuerySource {
            unit: rift_protocol::read::SourceUnitId::parse("rift://source/project/lib.rs")
                .expect("project source"),
            digest: None,
            text: Some("pub fn beacon() {}\n".to_owned()),
        });
    let mut other: QueryFactSelection =
        serde_json::from_value(serde_json::to_value(&fixture.selected).expect("selected facts"))
            .expect("same original publication");
    let mut configuration = WorkspaceConfiguration::default();
    configuration.server.num_workers += 1;
    other.selection.configuration =
        rift_search::query::QueryProviderSelection::configuration_digest(&configuration)
            .expect("different accepted configuration");
    assert_ne!(
        fixture.selected.selection.configuration,
        other.selection.configuration
    );
    assert_eq!(fixture.selected.selection.root, other.selection.root);
    assert_eq!(fixture.selected.owner, other.owner);
    let limits = rift_search::query::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )
    .expect("accepted query bounds");
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false).expect("query admission");
    let first = super::super::identity::identities("#same", &fixture.selected, &mut budget)
        .expect("first selected identities");
    let second = super::super::identity::identities("#same", &other, &mut budget)
        .expect("second selected identities");
    assert_ne!(first.blank_prefix, second.blank_prefix);
    assert_ne!(first.sources[0].1, second.sources[0].1);
    let expected = serde_json::to_value([&fixture.selected.selection, &other.selection])
        .expect("original selections");
    let input = super::super::retained::Input::prepare(
        vec![
            ("#same".to_owned(), fixture.selected, first),
            ("#same".to_owned(), other, second),
        ],
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)
            .expect("exchange bound")
            .material_bytes_max,
        &mut budget,
    )
    .expect("retained original selections");
    let captured = input
        .capture(
            &fixture.issuer,
            &original_root,
            &SymbolOwner::Local,
            &rift_protocol::query::QueryAnswer::Boolean { value: true },
            Vec::new(),
            &mut budget,
        )
        .expect("query retention");
    let rift_protocol::query::QueryReadResult::Results(captured) = captured else {
        panic!("retained query answer");
    };
    let view = captured.view.expect("retained query view");
    let material = fixture
        .issuer
        .query_view(&view.id, &original_root, &SymbolOwner::Local)
        .expect("original query material");
    let metadata = super::super::retained::metadata(&material).expect("provider metadata");
    assert_eq!(
        serde_json::to_value(
            metadata
                .providers
                .iter()
                .map(|provider| &provider.selection)
                .collect::<Vec<_>>()
        )
        .expect("retained provider selections"),
        expected
    );
    assert_ne!(metadata.sources[0].public, metadata.sources[1].public);
}
