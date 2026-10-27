use super::*;
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use serde_json::json;
use std::time::{Duration, SystemTime};

struct Fixture {
    root: tempfile::TempDir,
    issuer: rift_server::LocalSymbolViews,
    selected: QueryFactSelection,
}

fn fixture() -> Fixture {
    fixture_with_file(None)
}

fn query_limits() -> rift_search::sparql::QueryLimits {
    rift_search::sparql::QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())
        .expect("accepted query bounds")
}

fn fixture_with_file(file: Option<(&str, &str)>) -> Fixture {
    fixture_with_requirements(file, None)
}

fn fixture_with_requirements(
    file: Option<(&str, &str)>,
    requirements: Option<FactRequirements>,
) -> Fixture {
    let root = tempfile::tempdir().expect("workspace");
    crate::server::hermetic_workspace(root.path(), "").expect("accepted workspace");
    if let Some((path, text)) = file {
        std::fs::write(root.path().join(path), text).expect("original source");
    }
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
    let mut selected = QueryFactSelection {
        root: original_root.to_str().expect("UTF-8 workspace").to_owned(),
        owner: SymbolOwner::Local,
        view,
        selection,
        requests: 2,
        graph: Some(rift_search::sparql::LOCAL_GRAPH.to_owned()),
        requirements: FactRequirements::default(),
        subjects: Vec::new(),
        facts: rift_search::query::QueryFacts::default(),
        services: rift_search::sparql::QueryServices::default(),
        warnings: Vec::new(),
        upstream: Vec::new(),
    };
    if file.is_some() {
        selected.requirements.required.extend([
            rift_search::query::FactRequirement::Domain,
            rift_search::query::FactRequirement::Description,
            rift_search::query::FactRequirement::Source,
        ]);
        if let Some(requirements) = requirements {
            selected.requirements = requirements;
        }
        let limits = rift_search::sparql::QueryLimits::try_from(
            &rift_protocol::query::QueryConfiguration::default(),
        )
        .expect("accepted query bounds");
        let mut budget =
            rift_server::QueryFactBudget::new(limits, &|| false).expect("original budget");
        let (facts, subjects) = input
            .query_facts_with_subjects(
                &selected.requirements,
                limits,
                &mut budget,
                &selected.selection,
            )
            .expect("original source projection");
        selected.facts = facts;
        selected.subjects = subjects;
    }
    Fixture {
        root,
        issuer,
        selected,
    }
}

fn encoded(selected: QueryFactSelection) -> serde_json::Value {
    serde_json::to_value(rift_server::QueryExchange::new(QueryFactResult::Facts(
        vec![selected],
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
        changed["payload"]["response"][0][field] = json!([]);
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

#[test]
fn owned_complete_empty_facts_differ_from_no_authoritative_owner() {
    let mut fixture = fixture();
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::Domain);
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::CompleteDomain);
    fixture
        .selected
        .validate_coverage(&fixture.selected.requirements)
        .expect("complete empty original domain");
    let value = encoded(fixture.selected);
    assert!(
        matches!(decode_response(CallToolResult::structured(value.clone())),
        Ok(QueryFactResult::Facts(selected)) if selected.len() == 1)
    );
    let mut missing = value;
    missing["payload"]["response"] = json!([]);
    assert!(decode_response(CallToolResult::structured(missing)).is_err());
}

#[test]
fn selected_fact_coverage_requires_original_request_and_present_subject_metadata() {
    let fixture = fixture();
    let expected = fixture.selected.requirements.clone();
    fixture
        .selected
        .validate_coverage(&expected)
        .expect("original request");
    let mut different = expected.clone();
    different.require(rift_search::query::FactRequirement::Domain);
    assert!(fixture.selected.validate_coverage(&different).is_err());
    let valid = encoded(fixture.selected);
    for field in ["graph", "requirements", "subjects"] {
        let mut missing = valid.clone();
        missing["payload"]["response"][0]
            .as_object_mut()
            .expect("selected body")
            .remove(field);
        assert!(
            decode_response(CallToolResult::structured(missing)).is_err(),
            "{field}"
        );
    }
    let mut invalid: QueryFactSelection =
        serde_json::from_value(valid["payload"]["response"][0].clone())
            .expect("original selected body");
    invalid.subjects = vec![0];
    assert!(invalid.validate_coverage(&expected).is_err());
    invalid.subjects.clear();
    invalid
        .requirements
        .require(rift_search::query::FactRequirement::Domain);
    invalid
        .requirements
        .require(rift_search::query::FactRequirement::Documentation);
    invalid
        .facts
        .unavailable_families
        .push(rift_search::query::QueryFactFamily::Documentation);
    assert!(invalid.validate_coverage(&invalid.requirements).is_err());
}

#[test]
fn selected_graph_requires_presence_but_accepts_explicit_null() {
    let fixture = fixture();
    let mut value = encoded(fixture.selected);
    value["payload"]["response"][0]["graph"] = json!(null);
    assert!(
        matches!(decode_response(CallToolResult::structured(value.clone())),
        Ok(QueryFactResult::Facts(selected)) if selected[0].graph.is_none())
    );
    value["payload"]["response"][0]
        .as_object_mut()
        .expect("selected body")
        .remove("graph");
    assert!(decode_response(CallToolResult::structured(value)).is_err());
}

#[test]
fn original_subject_selectors_include_anonymous_published_members()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture();
    std::fs::write(
        fixture.root.path().join("member.json"),
        "{\"beacon\":\"notes\"}\n",
    )?;
    let reads = rift_server::ReadService::build(
        fixture.root.path(),
        rift_index::WorkspaceIndexLimits::default(),
        &rift_core::SourceVisibility::default(),
        &rift_core::TextFileInclusion::default(),
        rift_protocol::configuration::HistoryConfiguration::default(),
    )?;
    let configuration = WorkspaceConfiguration::default();
    let input = reads.local_search_view_input(true)?;
    let selection = input.provider_selection(&configuration)?;
    let mut requirements = FactRequirements::default();
    requirements.require(rift_search::query::FactRequirement::Domain);
    requirements.require(rift_search::query::FactRequirement::Description);
    let limits = rift_search::query::QueryLimits::try_from(&configuration.search.query)?;
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (facts, subjects) =
        input.query_facts_with_subjects(&requirements, limits, &mut budget, &selection)?;
    assert_eq!(subjects, (0..facts.symbols.len()).collect::<Vec<_>>());
    let index = facts
        .symbols
        .iter()
        .position(|symbol| symbol.symbol.name == "beacon")
        .ok_or("original JSON member")?;
    assert!(facts.symbols[index].symbol.id.is_none());
    assert!(subjects.contains(&index));
    Ok(())
}

#[test]
fn flat_fact_exchange_roundtrips_normal_and_mixed_owner_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = fixture();
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::Domain);
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::CompleteDomain);
    let mut other: QueryFactSelection =
        serde_json::from_value(serde_json::to_value(&fixture.selected)?)?;
    let mut configuration = WorkspaceConfiguration::default();
    configuration.server.num_workers += 1;
    other.selection.configuration =
        rift_search::query::QueryProviderSelection::configuration_digest(&configuration)?;
    other.graph = None;
    other.requests = 0;
    assert_ne!(fixture.selected.selection, other.selection);
    let value = serde_json::to_value(rift_server::QueryExchange::new(QueryFactResult::Facts(
        vec![fixture.selected, other],
    )))?;
    assert_eq!(
        value["payload"]["response"]
            .as_array()
            .ok_or("flat fact owners")?
            .len(),
        2
    );
    assert!(value["payload"]["response"][1]["graph"].is_null());
    assert_eq!(value["payload"]["response"][0]["subjects"], json!([]));
    let QueryFactResult::Facts(selected) =
        decode_response(CallToolResult::structured(value.clone()))?
    else {
        return Err("validated fact bodies must roundtrip".into());
    };
    assert_eq!(selected.len(), 2);
    assert_eq!(
        selected[0].graph.as_deref(),
        Some(rift_search::sparql::LOCAL_GRAPH)
    );
    assert!(selected[1].graph.is_none());
    for body in &selected {
        body.validate_coverage(&body.requirements)?;
        assert!(body.subjects.is_empty());
    }
    let mut old = value;
    let old_body = old["payload"]["response"][0].clone();
    old["payload"]["response"] = old_body;
    assert!(decode_response(CallToolResult::structured(old)).is_err());
    Ok(())
}

fn capture_fact_body(
    issuer: &rift_server::LocalSymbolViews,
    root: &std::path::Path,
    selected: QueryFactSelection,
    answer: &rift_protocol::query::QueryAnswer,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<rift_server::CapturedQueryView, Box<dyn std::error::Error>> {
    let identities = super::super::identity::identities("local", &selected, budget)?;
    let limits = rift_search::sparql::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )
    .expect("accepted query bounds");
    let input = super::super::retained::Input::prepare(
        vec![("local".to_owned(), selected, identities)],
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        budget,
    )?;
    let response = input.capture(
        issuer,
        root,
        &SymbolOwner::Local,
        answer,
        Vec::new(),
        budget,
    )?;
    let rift_protocol::query::QueryReadResult::Results(response) = response else {
        return Err("retained query answer".into());
    };
    let view = response.view.ok_or("retained query view")?;
    Ok(issuer
        .query_view(&view.id, root, &SymbolOwner::Local)
        .map_err(|reason| format!("original query material: {reason:?}"))?)
}

#[test]
fn retained_fact_bodies_keep_original_source_and_binding_bags_after_edit()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_protocol::query::{QueryAnswer, RdfTerm};
    let fixture = fixture_with_file(Some(("lib.rs", "pub fn held() -> u32 { 17 }\n")));
    let original = serde_json::to_value(&fixture.selected.facts)?;
    let original_requirements = fixture.selected.requirements.clone();
    let original_subjects = fixture.selected.subjects.clone();
    let row = std::collections::BTreeMap::from([(
        "symbol".to_owned(),
        RdfTerm::BlankNode {
            value: "retained_row".to_owned(),
        },
    )]);
    let answer = QueryAnswer::Bindings {
        variables: vec!["symbol".to_owned(), "optional".to_owned()],
        bindings: vec![row.clone(), row],
    };
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let captured = capture_fact_body(
        &fixture.issuer,
        fixture.root.path(),
        fixture.selected,
        &answer,
        &mut budget,
    )?;
    std::fs::write(fixture.root.path().join("lib.rs"), "pub fn changed() {}\n")?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    assert_eq!(bodies.len(), 1);
    assert_eq!(serde_json::to_value(&bodies[0].1.facts)?, original);
    assert_eq!(bodies[0].1.requirements, original_requirements);
    assert_eq!(bodies[0].1.subjects, original_subjects);
    assert_eq!(bodies[0].1.requests, 0);
    assert!(super::super::replay::covers_requested(
        &bodies,
        &original_requirements,
        &mut budget
    )?);
    let restored = metadata
        .answer
        .decode_bytes(captured.material().records(), None)?;
    assert_eq!(restored, answer);
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let statement = format!("SELECT ?symbol ?optional {{ SERVICE <{service}> {{}} }}");
    let evaluated = rift_search::sparql::PreparedQuery::parse(&statement, limits)?
        .evaluate_with_services(
            bodies[0].1.facts.clone(),
            limits,
            rift_search::sparql::QueryCancellation::default(),
            rift_search::sparql::QueryServices {
                bindings: vec![rift_search::sparql::RetainedResult {
                    service,
                    answer: restored,
                }],
                ..rift_search::sparql::QueryServices::default()
            },
        )?;
    assert_eq!(evaluated, answer);
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let repeated =
        format!("SELECT ?symbol ?optional {{ SERVICE <{service}> {{}} SERVICE <{service}> {{}} }}");
    let repeated = rift_search::sparql::PreparedQuery::parse(&repeated, limits)?
        .evaluate_with_services(
            bodies[0].1.facts.clone(),
            limits,
            rift_search::sparql::QueryCancellation::default(),
            rift_search::sparql::QueryServices {
                bindings: vec![rift_search::sparql::RetainedResult {
                    service,
                    answer: metadata
                        .answer
                        .decode_bytes(captured.material().records(), None)?,
                }],
                ..rift_search::sparql::QueryServices::default()
            },
        )?;
    let QueryAnswer::Bindings { bindings, .. } = repeated else {
        return Err("repeated retained bindings".into());
    };
    assert_eq!(bindings.len(), 4);
    assert!(bindings.iter().all(|row| !row.contains_key("optional")));
    assert!(bindings.windows(2).all(|rows| rows[0] == rows[1]));
    let mut missing = original_requirements;
    missing.require(rift_search::query::FactRequirement::CompleteDomain);
    missing
        .facets
        .push(rift_protocol::read::RelationshipFacet::References);
    assert!(!super::super::replay::covers_requested(
        &bodies,
        &missing,
        &mut budget
    )?);
    Ok(())
}

#[test]
fn retained_owned_complete_empty_body_keeps_its_certificate()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = fixture();
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::Domain);
    fixture
        .selected
        .requirements
        .require(rift_search::query::FactRequirement::CompleteDomain);
    fixture
        .selected
        .requirements
        .facets
        .push(rift_protocol::read::RelationshipFacet::References);
    let requirements = fixture.selected.requirements.clone();
    let limits = rift_search::sparql::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )
    .expect("accepted query bounds");
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let captured = capture_fact_body(
        &fixture.issuer,
        fixture.root.path(),
        fixture.selected,
        &rift_protocol::query::QueryAnswer::Boolean { value: false },
        &mut budget,
    )?;
    let (_, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].1.subjects.is_empty());
    assert!(bodies[0].1.facts.symbols.is_empty());
    assert!(super::super::replay::covers_requested(
        &bodies,
        &requirements,
        &mut budget
    )?);
    Ok(())
}

#[test]
fn selected_identity_prefix_preserves_adjacent_u64_revisions()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture_with_file(Some(("member.json", "{\"beacon\":\"notes\"}\n")));
    let original = serde_json::to_value(&fixture.selected)?;
    let limits = rift_search::sparql::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )
    .expect("accepted query bounds");
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    for field in ["index_revision", "source_revision", "tree_revision"] {
        let mut first = original.clone();
        let mut second = original.clone();
        first["selection"][field] = json!(1_u64 << 53);
        second["selection"][field] = json!((1_u64 << 53) + 1);
        let first: QueryFactSelection = serde_json::from_value(first)?;
        let second: QueryFactSelection = serde_json::from_value(second)?;
        assert_eq!(
            serde_json::to_value(&first.facts)?,
            serde_json::to_value(&second.facts)?
        );
        let first = super::super::identity::identities("local", &first, &mut budget)?;
        let second = super::super::identity::identities("local", &second, &mut budget)?;
        assert_ne!(first.blank_prefix, second.blank_prefix, "{field}");
    }
    Ok(())
}

fn original_public_source(
    body: &QueryFactSelection,
    identities: &rift_search::sparql::QueryIdentities,
    text: &str,
) -> Result<rift_protocol::read::SourceUnitId, Box<dyn std::error::Error>> {
    let unit = body
        .facts
        .sources
        .iter()
        .find(|source| source.text.as_deref() == Some(text))
        .ok_or("original selected source")?
        .unit
        .clone();
    let public = identities
        .sources
        .iter()
        .find(|(original, _)| original == &unit)
        .ok_or("original source dictionary row")?
        .1
        .clone();
    Ok(public)
}

#[test]
fn saved_service_dictionary_survives_domain_roles_and_owner_order()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = fixture_with_file(Some(("member.json", "{\"beacon\":\"notes\"}\n")));
    fixture.selected.graph = None;
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let captured = capture_fact_body(
        &fixture.issuer,
        fixture.root.path(),
        fixture.selected,
        &rift_protocol::query::QueryAnswer::Boolean { value: true },
        &mut budget,
    )?;
    let (_, mut bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let (_, body, identities) = bodies.pop().ok_or("original stored owner")?;
    let original = serde_json::to_value(&body)?;
    let original_identities = serde_json::to_value(&identities)?;
    let saved_prefix = identities.blank_prefix.clone();
    let text = "{\"beacon\":\"notes\"}\n";
    let public = original_public_source(&body, &identities, text)?;
    for reverse in [false, true] {
        let mut domain: QueryFactSelection = serde_json::from_value(original.clone())?;
        domain.graph = Some(rift_search::sparql::LOCAL_GRAPH.to_owned());
        let service: QueryFactSelection = serde_json::from_value(original.clone())?;
        let mut selections = vec![
            (
                "local".to_owned(),
                domain,
                rift_search::sparql::QueryServices::default(),
                None,
            ),
            (
                "local".to_owned(),
                service,
                rift_search::sparql::QueryServices::default(),
                Some(identities.clone()),
            ),
        ];
        if reverse {
            selections.reverse();
        }
        let (_input, facts, services, _) = super::super::evaluate::prepare_selected_services(
            selections,
            &[],
            limits,
            rift_protocol::query::QueryConfiguration::default(),
            rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
            &mut budget,
        )?;
        assert!(facts.symbols.is_empty());
        assert_eq!(services.datasets.len(), 2);
        for owner in &services.datasets {
            assert_eq!(owner.identities.blank_prefix, saved_prefix);
            assert_eq!(
                serde_json::to_value(&owner.identities)?,
                original_identities
            );
        }
        let graph = oxigraph::model::NamedNodeRef::new(rift_search::sparql::LOCAL_GRAPH)?;
        let statement = format!(
            "SELECT ?symbol ?again {{ GRAPH {graph} {{ ?symbol a :Symbol; :name 'beacon' }} ?again a :Symbol; :name 'beacon' FILTER(sameTerm(?symbol, ?again)) }}"
        );
        let answer = rift_search::sparql::PreparedQuery::parse(&statement, limits)?
            .evaluate_with_services(
                facts.clone(),
                limits,
                rift_search::sparql::QueryCancellation::default(),
                services.clone(),
            )?;
        let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } = answer else {
            return Err("same original physical symbol bindings".into());
        };
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0]["symbol"], bindings[0]["again"]);
        assert!(matches!(
            bindings[0]["symbol"],
            rift_protocol::query::RdfTerm::BlankNode { .. }
        ));
        let source = oxigraph::model::Literal::from(text);
        let statement = format!("ASK {{ <{}> :source {source} }}", public.as_str());
        assert_eq!(
            rift_search::sparql::PreparedQuery::parse(&statement, limits)?.evaluate_with_services(
                facts,
                limits,
                rift_search::sparql::QueryCancellation::default(),
                services,
            )?,
            rift_protocol::query::QueryAnswer::Boolean { value: true }
        );
    }
    Ok(())
}

fn locator_fixture(
    path: &str,
    text: &str,
    changed: bool,
) -> Result<(Fixture, rift_protocol::read::SearchParams), Box<dyn std::error::Error>> {
    let mut fixture = fixture_with_file(Some((path, text)));
    fixture.selected.graph = None;
    let (index, symbol) = fixture
        .selected
        .facts
        .symbols
        .iter()
        .enumerate()
        .find(|(_, symbol)| symbol.symbol.name == "beacon")
        .ok_or("original beacon")?;
    let declaration = symbol.declarations.first().ok_or("original declaration")?;
    let target = fixture
        .selected
        .facts
        .providers
        .iter()
        .enumerate()
        .find_map(|(provider, snapshot)| {
            snapshot
                .records
                .iter()
                .find(|record| record.symbol_index == index)
                .and_then(rift_search::query::QueryProviderRecord::target)
                .map(|target| {
                    (
                        0,
                        rift_search::query::QueryProviderEndpoint {
                            provider: Some(provider),
                            target,
                        },
                    )
                })
        });
    let change = changed.then(|| rift_protocol::read::SymbolChange {
        kind: rift_protocol::read::SymbolVersionKind::Introduced,
        base_path: None,
        head_path: Some(rift_protocol::read::ProjectPath(path.to_owned())),
    });
    let hit: rift_protocol::read::SearchHit = serde_json::from_value(json!({
        "view": fixture.selected.view,
        "hit": {"target":"symbol", "symbol":symbol.symbol},
        "path":path, "range":declaration.range, "line":declaration.line,
        "source":text, "change":change
    }))?;
    let request: rift_protocol::read::SearchParams = serde_json::from_value(json!({
        "query":"beacon", "target":"symbol", "include":["source"]
    }))?;
    fixture
        .selected
        .requirements
        .retrievals
        .push(request.clone());
    fixture
        .selected
        .services
        .retrievals
        .push(rift_search::sparql::QueryServiceData {
            request: request.clone(),
            result: rift_protocol::read::SearchResult {
                view: Some(fixture.selected.view.clone()),
                results: vec![hit],
                pagination: rift_protocol::read::Pagination {
                    page_index: 0,
                    total_pages: 1,
                },
                warnings: Vec::new(),
            },
            targets: vec![target],
        });
    Ok((fixture, request))
}

fn check_locator_capture(
    fixture: &Fixture,
    body: QueryFactSelection,
    saved: Option<rift_search::sparql::QueryIdentities>,
    request: &rift_protocol::read::SearchParams,
    path: &str,
    text: &str,
) -> Result<(QueryFactSelection, rift_search::sparql::QueryIdentities), Box<dyn std::error::Error>>
{
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let original_services = serde_json::to_value(&body.services)?;
    let original_view = body.view.clone();
    let companion = body.services.clone();
    let (input, facts, services, warnings) = super::super::evaluate::prepare_selected_services(
        vec![("local".to_owned(), body, companion, saved)],
        std::slice::from_ref(request),
        limits,
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        &mut budget,
    )?;
    let hit = &services.retrievals[0].result.results[0];
    assert_eq!(hit.path.as_ref().map(|path| path.0.as_str()), Some(path));
    assert!(hit.unit.is_none());
    assert_eq!(hit.view.as_ref(), Some(&original_view));
    assert_eq!(hit.source.as_deref(), Some(text));
    let statement = "SELECT ?path ?source { SERVICE :search { ?hit :query 'beacon'; :target 'symbol'; :include '[\"source\"]'^^rdf:JSON; :path ?path; :source ?source } }";
    let answer = rift_search::sparql::PreparedQuery::parse(statement, limits)?
        .evaluate_with_services(
            facts,
            limits,
            rift_search::sparql::QueryCancellation::default(),
            services,
        )?;
    let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } = &answer else {
        return Err("original path bindings".into());
    };
    assert_eq!(bindings.len(), 1);
    let rift_protocol::query::RdfTerm::Literal { value, .. } = &bindings[0]["path"] else {
        return Err("original path literal".into());
    };
    assert_eq!(value, path);
    let response = input.capture(
        &fixture.issuer,
        fixture.root.path(),
        &SymbolOwner::Local,
        &answer,
        warnings,
        &mut budget,
    )?;
    let rift_protocol::query::QueryReadResult::Results(response) = response else {
        return Err("captured locator result".into());
    };
    let view = response.view.ok_or("captured locator view")?;
    assert_eq!(view.expires_at, original_view.expires_at);
    let captured = fixture
        .issuer
        .query_view(&view.id, fixture.root.path(), &SymbolOwner::Local)
        .map_err(|reason| format!("captured locator: {reason:?}"))?;
    let (metadata, mut bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let (_, body, identities) = bodies.pop().ok_or("original locator owner")?;
    assert_eq!(serde_json::to_value(&body.services)?, original_services);
    let original = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("project")?,
        rift_core::SourcePath::new(path)?,
    )?
    .to_string();
    let source = metadata
        .sources
        .iter()
        .find(|source| source.original.as_str() == original)
        .ok_or("original locator source")?;
    assert_eq!(
        rift_protocol::identity::parse_source_unit_identity(source.original.as_str())?.1,
        path
    );
    assert_eq!(metadata.providers[source.provider].view, original_view);
    let bytes = captured
        .material()
        .sources()
        .get(source.bytes.clone().ok_or("original source bytes")?)
        .ok_or("captured source range")?;
    assert_eq!(bytes, text.as_bytes());
    let public = source.public.clone();
    let request = serde_json::from_value(json!({"unit":public,"view":view.id}))?;
    let source = super::super::source::captured_source(request, bytes, &view, &mut budget)?;
    let rift_protocol::source_read::GetSourceResult::Found {
        source,
        source_complete,
        view: read_view,
        ..
    } = source
    else {
        return Err("original source readback".into());
    };
    assert!(source_complete);
    assert_eq!(read_view, view);
    assert_eq!(source.text, text);
    assert_eq!(source.span.unit, public);
    assert_eq!(source.span.range.start, 0);
    assert_eq!(source.span.range.end, u64::try_from(text.len())?);
    Ok((body, identities))
}

#[test]
fn fresh_and_saved_service_locators_keep_original_paths_views_and_source_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    for (path, text, changed) in [
        ("lib.rs", "pub fn beacon() {}\n", false),
        ("notes for café.json", "{\"beacon\":\"雪\"}\n", false),
        ("change.rs", "pub fn beacon() {}\n", true),
    ] {
        let (fixture, request) = locator_fixture(path, text, changed)?;
        let body: QueryFactSelection =
            serde_json::from_value(serde_json::to_value(&fixture.selected)?)?;
        let (body, identities) = check_locator_capture(&fixture, body, None, &request, path, text)?;
        std::fs::write(fixture.root.path().join(path), "changed after capture\n")?;
        check_locator_capture(&fixture, body, Some(identities), &request, path, text)?;
    }
    Ok(())
}

fn named_subject_capture(
    repeats: usize,
) -> Result<
    (
        tempfile::TempDir,
        rift_server::LocalSymbolViews,
        rift_server::CapturedQueryView,
    ),
    Box<dyn std::error::Error>,
> {
    named_subject_capture_with_source(repeats, "{\"beacon\":\"notes\"}\n")
}

fn named_subject_capture_with_source(
    repeats: usize,
    source: &str,
) -> Result<
    (
        tempfile::TempDir,
        rift_server::LocalSymbolViews,
        rift_server::CapturedQueryView,
    ),
    Box<dyn std::error::Error>,
> {
    let requirements = rift_search::sparql::QueryPlan::prepare(
        "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }".to_owned(),
        query_limits(),
    )?
    .requirements()
    .clone();
    assert_eq!(requirements.symbol_name.as_deref(), Some("beacon"));
    let fixture = fixture_with_requirements(Some(("member.json", source)), Some(requirements));
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let identities = super::super::identity::identities("local", &fixture.selected, &mut budget)?;
    let resources = rift_search::sparql::retained_subject_resources(
        &fixture.selected.facts,
        identities,
        &fixture.selected.subjects,
        limits,
        &rift_search::sparql::QueryCancellation::default(),
        &mut |bytes| budget.charge_material(bytes),
    )?;
    let original = resources
        .iter()
        .filter(|resource| resource.original)
        .collect::<Vec<_>>();
    assert_eq!(original.len(), usize::from(repeats != 0));
    let mut row = std::collections::BTreeMap::new();
    if let Some(original) = original.first() {
        assert!(
            fixture.selected.facts.symbols[original.symbol]
                .symbol
                .id
                .is_none()
        );
        assert!(matches!(
            original.resource,
            rift_protocol::query::RdfTerm::BlankNode { .. }
        ));
        row.insert("symbol".to_owned(), original.resource.clone());
    }
    let answer = rift_protocol::query::QueryAnswer::Bindings {
        variables: vec!["symbol".to_owned(), "optional".to_owned()],
        bindings: vec![row; repeats],
    };
    let original_root = fixture.root.path().canonicalize()?;
    assert_eq!(original_root.to_str(), Some(fixture.selected.root.as_str()));
    let captured = capture_fact_body(
        &fixture.issuer,
        &original_root,
        fixture.selected,
        &answer,
        &mut budget,
    )?;
    Ok((fixture.root, fixture.issuer, captured))
}

fn retained_name_plan(
    service: &str,
    pattern: &str,
) -> Result<rift_search::sparql::QueryPlan, rift_error::RiftError> {
    rift_search::sparql::QueryPlan::prepare(
        format!(
            "SELECT ?symbol ?name ?optional {{ {} }}",
            pattern.replace("SERVICE_BODY", &format!("SERVICE <{service}> {{}}"))
        ),
        query_limits(),
    )
}

#[test]
fn retained_original_subjects_allow_join_order_optional_and_binding_bags_after_edit()
-> Result<(), Box<dyn std::error::Error>> {
    let (root, _issuer, captured) = named_subject_capture(2)?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    assert!(bodies.iter().all(|(_, body, _)| body.requests == 0));
    let original = serde_json::to_value(&bodies[0].1)?;
    let services = super::super::replay::restore_bindings(
        &captured,
        &metadata,
        vec![(service.clone(), None)],
        &mut budget,
    )?;
    std::fs::write(root.path().join("member.json"), "{\"replacement\":true}\n")?;
    for (pattern, count) in [
        ("SERVICE_BODY ?symbol :name ?name", 2),
        ("?symbol :name ?name SERVICE_BODY", 2),
        ("SERVICE_BODY OPTIONAL { ?symbol :name ?name }", 2),
        (
            "SERVICE_BODY ?symbol :name ?name OPTIONAL { ?symbol :missing ?optional }",
            2,
        ),
        ("SERVICE_BODY SERVICE_BODY ?symbol :name ?name", 4),
    ] {
        let plan = retained_name_plan(&service, pattern)?;
        assert!(plan.retained_subject_selection().is_some(), "{pattern}");
        assert!(!super::super::replay::covers_requested(
            &bodies,
            plan.requirements(),
            &mut budget
        )?);
        assert!(super::super::replay::covers_retained_bindings(
            &bodies,
            plan.requirements(),
            &services,
            plan.retained_subject_selection(),
            limits,
            &mut budget
        )?);
        let selected: QueryFactSelection = serde_json::from_value(original.clone())?;
        let (_, facts, evaluation, _) = super::super::evaluate::prepare_selected_services(
            vec![(
                bodies[0].0.clone(),
                selected,
                services.clone(),
                Some(bodies[0].2.clone()),
            )],
            &[],
            limits,
            rift_protocol::query::QueryConfiguration::default(),
            rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
            &mut budget,
        )?;
        let answer = rift_search::sparql::PreparedQuery::parse(plan.statement(), limits)?
            .evaluate_with_services(
                facts,
                limits,
                rift_search::sparql::QueryCancellation::default(),
                evaluation,
            )?;
        let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } = answer else {
            return Err("retained joined bindings".into());
        };
        assert_eq!(bindings.len(), count, "{pattern}");
        assert!(bindings.iter().all(|row| row["name"]
            == rift_protocol::query::RdfTerm::Literal {
                value: "beacon".to_owned(),
                datatype: "http://www.w3.org/2001/XMLSchema#string".to_owned(),
            }));
        assert!(bindings.iter().all(|row| !row.contains_key("optional")));
        assert!(bindings.windows(2).all(|rows| rows[0] == rows[1]));
        assert_eq!(serde_json::to_value(&bodies[0].1)?, original);
    }
    Ok(())
}

#[test]
fn retained_original_subjects_refuse_unbound_foreign_literal_and_missing_family()
-> Result<(), Box<dyn std::error::Error>> {
    let (_root, _issuer, captured) = named_subject_capture(1)?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let services = super::super::replay::restore_bindings(
        &captured,
        &metadata,
        vec![(service.clone(), None)],
        &mut budget,
    )?;
    let plan = retained_name_plan(&service, "SERVICE_BODY ?symbol :name ?name")?;
    for invalid in [
        None,
        Some(rift_protocol::query::RdfTerm::Iri {
            value: "urn:unrelated:subject".to_owned(),
        }),
        Some(rift_protocol::query::RdfTerm::BlankNode {
            value: "answer_generated".to_owned(),
        }),
        Some(rift_protocol::query::RdfTerm::Literal {
            value: "beacon".to_owned(),
            datatype: "http://www.w3.org/2001/XMLSchema#string".to_owned(),
        }),
    ] {
        let mut changed = services.clone();
        let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
            &mut changed.bindings[0].answer
        else {
            return Err("stored binding table".into());
        };
        let mut row = std::collections::BTreeMap::new();
        if let Some(invalid) = invalid {
            row.insert("symbol".to_owned(), invalid);
        }
        bindings.push(row);
        assert!(!super::super::replay::covers_retained_bindings(
            &bodies,
            plan.requirements(),
            &changed,
            plan.retained_subject_selection(),
            limits,
            &mut budget
        )?);
    }
    require_uncaptured_families(&bodies, &services, &plan, &mut budget)?;
    for pattern in [
        "OPTIONAL { SERVICE_BODY } ?symbol :name ?name",
        "SERVICE_BODY ?other :name ?name",
    ] {
        let unsafe_plan = retained_name_plan(&service, pattern)?;
        if pattern.starts_with("OPTIONAL") {
            assert!(unsafe_plan.retained_subject_selection().is_none());
        }
        assert!(!super::super::replay::covers_retained_bindings(
            &bodies,
            unsafe_plan.requirements(),
            &services,
            unsafe_plan.retained_subject_selection(),
            limits,
            &mut budget
        )?);
    }
    assert!(!super::super::replay::covers_retained_bindings(
        &bodies,
        plan.requirements(),
        &services,
        None,
        limits,
        &mut budget
    )?);
    Ok(())
}

fn require_uncaptured_families(
    bodies: &[(
        String,
        QueryFactSelection,
        rift_search::sparql::QueryIdentities,
    )],
    services: &rift_search::sparql::QueryServices,
    plan: &rift_search::sparql::QueryPlan,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let limits = query_limits();
    let mut missing = plan.requirements().clone();
    missing.require(rift_search::query::FactRequirement::CompleteDomain);
    missing
        .facets
        .push(rift_protocol::read::RelationshipFacet::References);
    assert!(!super::super::replay::covers_retained_bindings(
        bodies,
        &missing,
        services,
        plan.retained_subject_selection(),
        limits,
        budget
    )?);
    for family in [
        rift_search::query::FactRequirement::Source,
        rift_search::query::FactRequirement::History,
    ] {
        let mut missing = plan.requirements().clone();
        missing.require(family);
        assert!(!super::super::replay::covers_retained_bindings(
            bodies,
            &missing,
            services,
            plan.retained_subject_selection(),
            limits,
            budget,
        )?);
    }
    Ok(())
}

#[test]
fn retained_owned_empty_subject_table_keeps_family_refusals()
-> Result<(), Box<dyn std::error::Error>> {
    let (_root, _issuer, captured) =
        named_subject_capture_with_source(0, "{\"different\":true}\n")?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let services = super::super::replay::restore_bindings(
        &captured,
        &metadata,
        vec![(service.clone(), None)],
        &mut budget,
    )?;
    let plan = retained_name_plan(&service, "SERVICE_BODY ?symbol :name ?name")?;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].1.subjects.is_empty());
    assert!(bodies[0].1.facts.symbols.is_empty());
    assert!(super::super::replay::covers_retained_bindings(
        &bodies,
        plan.requirements(),
        &services,
        plan.retained_subject_selection(),
        limits,
        &mut budget
    )?);
    require_uncaptured_families(&bodies, &services, &plan, &mut budget)?;
    Ok(())
}

#[test]
fn retained_original_subjects_refuse_projected_enrichment_and_ambiguous_resources()
-> Result<(), Box<dyn std::error::Error>> {
    let (_root, _issuer, captured) = named_subject_capture(1)?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (metadata, mut bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let mut services = super::super::replay::restore_bindings(
        &captured,
        &metadata,
        vec![(service.clone(), None)],
        &mut budget,
    )?;
    let identities = bodies[0].2.clone();
    let selected = &mut bodies[0].1;
    selected
        .facts
        .symbols
        .push(selected.facts.symbols[0].clone());
    selected.validate_coverage(&selected.requirements)?;
    let resources = rift_search::sparql::retained_subject_resources(
        &selected.facts,
        identities,
        &selected.subjects,
        limits,
        &rift_search::sparql::QueryCancellation::default(),
        &mut |bytes| budget.charge_material(bytes),
    )?;
    let enriched = resources
        .iter()
        .find(|resource| !resource.original)
        .ok_or("projected enriched symbol")?;
    let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
        &mut services.bindings[0].answer
    else {
        return Err("stored bindings".into());
    };
    bindings[0].insert("symbol".to_owned(), enriched.resource.clone());
    let plan = retained_name_plan(&service, "SERVICE_BODY ?symbol :name ?name")?;
    assert!(!super::super::replay::covers_retained_bindings(
        &bodies,
        plan.requirements(),
        &services,
        plan.retained_subject_selection(),
        limits,
        &mut budget
    )?);
    // A repeated original directory cannot establish which original owner supplies authority.
    let duplicate: QueryFactSelection =
        serde_json::from_value(serde_json::to_value(&bodies[0].1)?)?;
    let duplicate_identities = bodies[0].2.clone();
    bodies.push((bodies[0].0.clone(), duplicate, duplicate_identities));
    let original = resources
        .iter()
        .find(|resource| resource.original)
        .ok_or("original resource")?;
    let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
        &mut services.bindings[0].answer
    else {
        return Err("stored bindings".into());
    };
    bindings[0].insert("symbol".to_owned(), original.resource.clone());
    assert!(!super::super::replay::covers_retained_bindings(
        &bodies,
        plan.requirements(),
        &services,
        plan.retained_subject_selection(),
        limits,
        &mut budget
    )?);
    Ok(())
}

#[tokio::test]
async fn retained_subject_query_inputs_keep_zero_provider_allowance_after_source_removal()
-> Result<(), Box<dyn std::error::Error>> {
    let (root, issuer, captured) = named_subject_capture(2)?;
    let metadata = super::super::retained::metadata(&captured)?;
    let original_view = metadata.providers[0].view.clone();
    let configuration = WorkspaceConfiguration::default();
    let registrations =
        crate::proxy::extra::Registrations::accept(root.path(), &configuration.mcp, &[], None)?;
    let mut proxy = super::super::RiftProxy::with_configuration(
        root.path(),
        crate::proxy::tests::test_identity(),
        crate::output::OutputPolicy::All,
        &configuration.server,
        registrations,
    );
    proxy.accepted_root = Some(std::sync::Arc::from(root.path().canonicalize()?.as_path()));
    proxy.query_views = std::sync::Arc::new(std::sync::OnceLock::new());
    assert!(proxy.query_views.set(issuer).is_ok());
    assert!(proxy.upstream.lock().await.connected.is_none());
    std::fs::remove_file(root.path().join("member.json"))?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str()
    );
    let plan = retained_name_plan(&service, "SERVICE_BODY ?symbol :name ?name")?;
    let limits = query_limits();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let budget = rift_server::QueryFactBudget::for_request(limits, deadline.into_std(), &|| false)?;
    let requests = rift_server::ProviderRequests::new(0);
    let Acquired::Facts(selected) = proxy
        .query_inputs(&plan, &budget, &requests, deadline)
        .await?
    else {
        return Err("original subject facts without current acquisition".into());
    };
    assert_eq!(requests.remaining(), 0);
    assert!(proxy.upstream.lock().await.connected.is_none());
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].1.requests, 0);
    assert_eq!(selected[0].1.view, original_view);
    assert_eq!(selected[0].1.subjects.len(), 1);
    let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
        &selected[0].2.bindings[0].answer
    else {
        return Err("unchanged saved binding bag".into());
    };
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0], bindings[1]);
    Ok(())
}

#[test]
fn retained_subject_rows_partition_original_owners_without_borrowed_family_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let (_first_root, _first_issuer, first) = named_subject_capture(1)?;
    let (_second_root, _second_issuer, second) =
        named_subject_capture_with_source(1, "{\"beacon\":\"other\"}\n")?;
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        first.view().id.as_str()
    );
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let (first_metadata, mut bodies) = super::super::replay::restore(&first, &mut budget)?;
    let (second_metadata, mut other) = super::super::replay::restore(&second, &mut budget)?;
    assert_ne!(bodies[0].1.selection.root, other[0].1.selection.root);
    let mut services = super::super::replay::restore_bindings(
        &first,
        &first_metadata,
        vec![(service.clone(), None)],
        &mut budget,
    )?;
    let second_answer = second_metadata
        .answer
        .decode_bytes(second.material().records(), None)?;
    let rift_protocol::query::QueryAnswer::Bindings {
        bindings: second_rows,
        ..
    } = second_answer
    else {
        return Err("second original table".into());
    };
    let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } =
        &mut services.bindings[0].answer
    else {
        return Err("first original table".into());
    };
    assert_ne!(bindings[0]["symbol"], second_rows[0]["symbol"]);
    bindings.extend(second_rows);
    bodies.append(&mut other);
    let plan = retained_name_plan(&service, "SERVICE_BODY ?symbol :name ?name")?;
    for reverse in [false, true] {
        if reverse {
            bodies.reverse();
        }
        assert!(super::super::replay::covers_retained_bindings(
            &bodies,
            plan.requirements(),
            &services,
            plan.retained_subject_selection(),
            limits,
            &mut budget
        )?);
    }
    let mut missing = plan.requirements().clone();
    missing.require(rift_search::query::FactRequirement::CompleteDomain);
    missing
        .facets
        .push(rift_protocol::read::RelationshipFacet::References);
    // One authoritative empty owner does not provide References for another original population.
    let mut empty = fixture();
    empty.selected.requirements = missing.clone();
    empty.selected.requirements.symbol_name = None;
    empty.selected.requirements.services.clear();
    empty
        .selected
        .validate_coverage(&empty.selected.requirements)?;
    let empty_capture = capture_fact_body(
        &empty.issuer,
        empty.root.path(),
        empty.selected,
        &rift_protocol::query::QueryAnswer::Bindings {
            variables: vec!["symbol".to_owned()],
            bindings: Vec::new(),
        },
        &mut budget,
    )?;
    let (_, mut complete_empty) = super::super::replay::restore(&empty_capture, &mut budget)?;
    bodies.append(&mut complete_empty);
    assert!(!super::super::replay::covers_retained_bindings(
        &bodies,
        &missing,
        &services,
        plan.retained_subject_selection(),
        limits,
        &mut budget
    )?);
    Ok(())
}

fn captured_source_fixture()
-> Result<(tempfile::TempDir, rift_server::CapturedQueryView), Box<dyn std::error::Error>> {
    let mut fixture = fixture_with_file(Some(("lib.rs", "pub fn original() {}\n")));
    fixture.selected.graph = None;
    let mut budget = rift_server::QueryFactBudget::new(query_limits(), &|| false)?;
    let answer = rift_protocol::query::QueryAnswer::Bindings {
        variables: Vec::new(),
        bindings: Vec::new(),
    };
    let root = fixture.root.path().canonicalize()?;
    let captured = capture_fact_body(
        &fixture.issuer,
        &root,
        fixture.selected,
        &answer,
        &mut budget,
    )?;
    Ok((fixture.root, captured))
}

#[test]
fn saved_public_source_keeps_original_bytes_and_provider_view()
-> Result<(), Box<dyn std::error::Error>> {
    let (root, captured) = captured_source_fixture()?;
    let metadata = super::super::retained::metadata(&captured)?;
    let original_unit = rift_protocol::read::SourceUnitId::parse("rift://source/project/lib.rs")?;
    let mut original_sources = metadata
        .sources
        .iter()
        .filter(|source| source.original == original_unit);
    let saved = original_sources
        .next()
        .ok_or("original lib.rs source metadata")?;
    assert!(original_sources.next().is_none());
    assert_ne!(saved.public, saved.original);
    std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n")?;
    let mut budget = rift_server::QueryFactBudget::new(query_limits(), &|| false)?;
    assert!(super::super::source::selected_source(&metadata, &saved.original).is_err());
    let unit = &saved.public;
    let selected = super::super::source::selected_source(&metadata, unit)?;
    assert!(std::ptr::eq(saved, selected));
    let bytes = captured
        .material()
        .sources()
        .get(selected.bytes.clone().ok_or("original source bytes")?)
        .ok_or("original source range")?;
    let params = rift_protocol::source_read::GetSourceParams {
        unit: unit.clone(),
        range: None,
        view: Some(captured.view().id.clone()),
        rev: None,
    };
    let response =
        super::super::source::captured_source(params.clone(), bytes, captured.view(), &mut budget)?;
    assert!(response.is_valid_for(&params));
    let upstream = rift_protocol::source_read::GetSourceParams {
        unit: saved.original.clone(),
        range: None,
        view: Some(metadata.providers[saved.provider].view.id.clone()),
        rev: None,
    };
    let forwarded = super::super::source::captured_source(
        upstream.clone(),
        bytes,
        &metadata.providers[saved.provider].view,
        &mut budget,
    )?;
    let qualified = super::super::source::qualify_source(
        forwarded,
        &upstream,
        "local",
        captured.view().clone(),
        Some(unit.clone()),
    )?;
    assert!(qualified.is_valid_for(&params));
    let rift_protocol::source_read::GetSourceResult::Found { source, view, .. } = response else {
        return Err("original source readback".into());
    };
    assert_eq!(source.span.unit, *unit);
    assert_eq!(source.text, "pub fn original() {}\n");
    assert_eq!(view, *captured.view());
    Ok(())
}

#[test]
fn saved_public_source_refuses_duplicate_public_units() -> Result<(), Box<dyn std::error::Error>> {
    let (_root, captured) = captured_source_fixture()?;
    let mut metadata = super::super::retained::metadata(&captured)?;
    let saved = metadata.sources.first().ok_or("original source metadata")?;
    let original = saved.original.clone();
    let public = saved.public.clone();
    let duplicate = super::super::retained::Source {
        provider: saved.provider,
        original: original.clone(),
        public: public.clone(),
        digest: saved.digest.clone(),
        bytes: saved.bytes.clone(),
    };
    metadata.sources.push(duplicate);
    assert!(super::super::source::selected_source(&metadata, &original).is_err());
    assert!(super::super::source::selected_source(&metadata, &public).is_err());
    Ok(())
}

#[test]
fn saved_public_sources_keep_distinct_provider_bytes_when_original_units_match()
-> Result<(), Box<dyn std::error::Error>> {
    let main = fixture_with_file(Some(("lib.rs", "pub fn main_original() {}\n")));
    let docs = fixture_with_file(Some(("lib.rs", "pub fn docs_original() {}\n")));
    let main_root = main.root.path().canonicalize()?;
    let docs_root = docs.root.path().canonicalize()?;
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let main_ids = super::super::identity::identities("local", &main.selected, &mut budget)?;
    let docs_ids = super::super::identity::identities("docs", &docs.selected, &mut budget)?;
    let main_public =
        original_public_source(&main.selected, &main_ids, "pub fn main_original() {}\n")?;
    let docs_public =
        original_public_source(&docs.selected, &docs_ids, "pub fn docs_original() {}\n")?;
    let input = super::super::retained::Input::prepare(
        vec![
            ("local".to_owned(), main.selected, main_ids),
            ("docs".to_owned(), docs.selected, docs_ids),
        ],
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        &mut budget,
    )?;
    let response = input.capture(
        &main.issuer,
        &main_root,
        &SymbolOwner::Local,
        &rift_protocol::query::QueryAnswer::Boolean { value: true },
        Vec::new(),
        &mut budget,
    )?;
    let rift_protocol::query::QueryReadResult::Results(response) = response else {
        return Err("retained query answer".into());
    };
    let view = response.view.ok_or("retained query view")?;
    let captured = main
        .issuer
        .query_view(&view.id, &main_root, &SymbolOwner::Local)
        .map_err(|reason| format!("original query material: {reason:?}"))?;
    let metadata = super::super::retained::metadata(&captured)?;
    let main_source = metadata
        .sources
        .iter()
        .find(|source| source.public == main_public)
        .ok_or("main original source metadata")?;
    let docs_source = metadata
        .sources
        .iter()
        .find(|source| source.public == docs_public)
        .ok_or("docs original source metadata")?;
    assert_eq!(main_source.original, docs_source.original);
    assert_eq!(main_source.public, main_source.original);
    assert_ne!(main_source.public, docs_source.public);
    assert_ne!(main_source.provider, docs_source.provider);
    assert_eq!(
        metadata.providers[main_source.provider].root,
        main_root.to_str().ok_or("main root")?
    );
    assert_eq!(
        metadata.providers[docs_source.provider].root,
        docs_root.to_str().ok_or("docs root")?
    );
    std::fs::write(main_root.join("lib.rs"), "pub fn main_replacement() {}\n")?;
    std::fs::write(docs_root.join("lib.rs"), "pub fn docs_replacement() {}\n")?;
    for (source, expected) in [
        (main_source, "pub fn main_original() {}\n"),
        (docs_source, "pub fn docs_original() {}\n"),
    ] {
        let selected = super::super::source::selected_source(&metadata, &source.public)?;
        assert!(std::ptr::eq(source, selected));
        let bytes = captured
            .material()
            .sources()
            .get(selected.bytes.clone().ok_or("original source bytes")?)
            .ok_or("original source range")?;
        assert_eq!(std::str::from_utf8(bytes)?, expected);
    }
    let unknown = rift_protocol::read::SourceUnitId::parse("rift://source/project/unknown.rs")?;
    assert!(super::super::source::selected_source(&metadata, &unknown).is_err());
    Ok(())
}

#[test]
fn prepared_source_service_reaches_evaluation_with_original_public_parent_expiry()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_protocol::read::SourceUnitId;
    use rift_protocol::source_read::GetSourceParams;
    let original = "// λ only\n";
    let mut fixture = fixture_with_file(Some(("lib.rs", original)));
    let root = fixture.root.path().canonicalize()?;
    let provider_view = fixture.selected.view.clone();
    let parent = source_parent_view(&fixture, &root, &provider_view)?;
    let unit = SourceUnitId::parse("rift://source/project/lib.rs")?;
    let request = GetSourceParams {
        unit: unit.clone(),
        range: None,
        view: Some(parent.id.clone()),
        rev: None,
    };
    let raw_request = GetSourceParams {
        view: Some(provider_view.id.clone()),
        ..request.clone()
    };
    let (result, raw_result) = prepared_source_results(
        &unit,
        original,
        &parent,
        &provider_view,
        &request,
        &raw_request,
    )?;
    fixture
        .selected
        .services
        .sources
        .push((raw_request.clone(), raw_result));
    fixture.selected.requirements.source_reads.push(raw_request);
    let certificate = serde_json::to_value(&fixture.selected.services.sources)?;
    fixture.selected.graph = None;
    fixture.selected.facts = rift_search::sparql::QueryFacts::default();
    fixture.selected.subjects.clear();
    fixture.selected.requirements.required.clear();
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let mut identities =
        super::super::identity::identities("local", &fixture.selected, &mut budget)?;
    assert_eq!(identities.sources.len(), 1);
    identities.sources[0].1 = unit.clone();
    let ready = rift_search::sparql::QueryServices {
        sources: vec![(request, result)],
        ..Default::default()
    };
    let (input, facts, services, warnings) = super::super::evaluate::prepare_selected_services(
        vec![(
            "local".to_owned(),
            fixture.selected,
            ready,
            Some(identities),
        )],
        &[],
        limits,
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        &mut budget,
    )?;
    assert_eq!(
        services.sources.len(),
        1,
        "the producer's typed source association must reach evaluation"
    );
    assert_eq!(
        input.original_context()?.0,
        root.to_str().ok_or("original root")?
    );
    assert_eq!(input.original_context()?.1, &SymbolOwner::Local);
    let answer =
        evaluate_prepared_source_service(facts, services, &unit, &parent, original, limits)?;
    let captured = input.capture(
        &fixture.issuer,
        &root,
        &SymbolOwner::Local,
        &answer,
        warnings,
        &mut budget,
    )?;
    let rift_protocol::query::QueryReadResult::Results(captured) = captured else {
        return Err("captured source answer".into());
    };
    let view = captured.view.ok_or("source query view")?;
    assert!(crate::proxy::captured_expiry(&view)? <= crate::proxy::captured_expiry(&parent)?);
    let captured = fixture
        .issuer
        .query_view(&view.id, &root, &SymbolOwner::Local)
        .map_err(|reason| format!("original source query: {reason:?}"))?;
    assert_captured_source_service(
        &captured,
        &unit,
        &view,
        &provider_view,
        &certificate,
        original,
        &mut budget,
    )?;
    Ok(())
}

fn source_parent_view(
    fixture: &Fixture,
    root: &std::path::Path,
    provider_view: &CapturedView,
) -> Result<CapturedView, Box<dyn std::error::Error>> {
    let provider_expiry = crate::proxy::captured_expiry(provider_view)?;
    let parent = fixture
        .issuer
        .capture_query_with(
            rift_server::QueryViewInput::new(
                root.to_path_buf(),
                SymbolOwner::Local,
                provider_expiry - Duration::from_secs(1),
            )?,
            |_| Ok(true),
        )?
        .ok_or("shorter public parent")?;
    assert!(crate::proxy::captured_expiry(&parent)? < provider_expiry);
    Ok(parent)
}

fn prepared_source_results(
    unit: &rift_protocol::read::SourceUnitId,
    original: &str,
    parent: &CapturedView,
    provider_view: &CapturedView,
    request: &rift_protocol::source_read::GetSourceParams,
    raw_request: &rift_protocol::source_read::GetSourceParams,
) -> Result<
    (
        rift_protocol::source_read::GetSourceResult,
        rift_protocol::source_read::GetSourceResult,
    ),
    Box<dyn std::error::Error>,
> {
    use rift_protocol::read::{SourceExcerpt, SourceUnitSpan, TextRange};
    use rift_protocol::source_read::GetSourceResult;
    let result = GetSourceResult::Found {
        source: SourceExcerpt {
            span: SourceUnitSpan {
                unit: unit.clone(),
                range: TextRange {
                    start: 0,
                    end: u64::try_from(original.len())?,
                },
            },
            text: original.to_owned(),
        },
        source_complete: true,
        view: parent.clone(),
        warnings: Vec::new(),
    };
    assert!(result.is_valid_for(request));
    let mut raw_result = result.clone();
    let GetSourceResult::Found { view, .. } = &mut raw_result else {
        return Err("original found source".into());
    };
    *view = provider_view.clone();
    assert!(raw_result.is_valid_for(raw_request));
    Ok((result, raw_result))
}

fn evaluate_prepared_source_service(
    facts: rift_search::sparql::QueryFacts,
    services: rift_search::sparql::QueryServices,
    unit: &rift_protocol::read::SourceUnitId,
    parent: &CapturedView,
    original: &str,
    limits: rift_search::sparql::QueryLimits,
) -> Result<rift_protocol::query::QueryAnswer, Box<dyn std::error::Error>> {
    let selected_view = oxigraph::model::Literal::from(parent.id.as_str());
    let query = rift_search::sparql::PreparedQuery::parse(
        &format!(
            "SELECT ?outcome ?text ?unit ?source_view ?expiry {{ SERVICE :source {{ ?read :unit <{unit}>; :view {selected_view}; :outcome ?outcome; :source [ :text ?text; :span [ :unit ?unit ] ]; :view [ :id ?source_view; :expires_at ?expiry ] }} }}",
            unit = unit.as_str(),
        ),
        limits,
    )?;
    let answer = query.evaluate_with_services(
        facts,
        limits,
        rift_search::sparql::QueryCancellation::default(),
        services,
    )?;
    let value = serde_json::to_value(&answer)?;
    let rows = value["bindings"].as_array().ok_or("source bindings")?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["outcome"]["value"], "found");
    assert_eq!(rows[0]["text"]["value"], original);
    assert_eq!(rows[0]["unit"]["value"], unit.as_str());
    assert_eq!(rows[0]["source_view"]["value"], parent.id.as_str());
    assert_eq!(rows[0]["expiry"]["value"], parent.expires_at);
    Ok(answer)
}

fn assert_captured_source_service(
    captured: &rift_server::CapturedQueryView,
    unit: &rift_protocol::read::SourceUnitId,
    view: &CapturedView,
    provider_view: &CapturedView,
    certificate: &serde_json::Value,
    original: &str,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
    let metadata = super::super::retained::metadata(captured)?;
    let saved = super::super::source::selected_source(&metadata, unit)?;
    assert_eq!(&saved.original, unit);
    let bytes = captured
        .material()
        .sources()
        .get(saved.bytes.clone().ok_or("whole source promotion")?)
        .ok_or("whole source buffer")?;
    assert_eq!(bytes, original.as_bytes());
    let own_request = GetSourceParams {
        unit: unit.clone(),
        range: None,
        view: Some(view.id.clone()),
        rev: None,
    };
    let own_result =
        super::super::source::captured_source(own_request.clone(), bytes, view, budget)?;
    assert!(own_result.is_valid_for(&own_request));
    let GetSourceResult::Found {
        source,
        view: returned,
        source_complete,
        ..
    } = own_result
    else {
        return Err("own outer view source result".into());
    };
    assert_eq!(source.text, original);
    assert_eq!(&source.span.unit, unit);
    assert_eq!(&returned, view);
    assert!(source_complete);
    let (_, bodies) = super::super::replay::restore(captured, budget)?;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        &bodies[0].1.view, provider_view,
        "raw provider view and expiry must stay unchanged"
    );
    assert_eq!(
        &serde_json::to_value(&bodies[0].1.services.sources)?,
        certificate,
        "raw source certificate stays unchanged"
    );
    Ok(())
}

#[test]
fn raw_source_certificate_keeps_request_order_and_refuses_subset_coverage()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
    let mut fixture = fixture();
    for path in ["a.rs", "b.rs"] {
        let request = GetSourceParams {
            unit: rift_protocol::read::SourceUnitId::parse(&format!(
                "rift://source/project/{path}"
            ))?,
            range: None,
            view: Some(fixture.selected.view.id.clone()),
            rev: None,
        };
        let result = GetSourceResult::Missing {
            unit: request.unit.clone(),
            view: fixture.selected.view.clone(),
            warnings: Vec::new(),
        };
        assert!(result.is_valid_for(&request));
        fixture
            .selected
            .requirements
            .source_reads
            .push(request.clone());
        fixture.selected.services.sources.push((request, result));
    }
    let requirements = fixture.selected.requirements.clone();
    fixture.selected.validate_coverage(&requirements)?;
    assert!(fixture.selected.covers(&requirements));
    let mut subset = requirements.clone();
    subset.source_reads.pop();
    assert!(!fixture.selected.covers(&subset));
    subset.source_reads.clear();
    assert!(fixture.selected.covers(&subset));
    let mut reordered = requirements.clone();
    reordered.source_reads.reverse();
    assert!(!fixture.selected.covers(&reordered));
    fixture.selected.services.sources.reverse();
    assert!(fixture.selected.validate_coverage(&requirements).is_err());
    assert!(!fixture.selected.covers(&requirements));
    Ok(())
}

#[test]
fn retained_source_directory_merges_only_the_same_original_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture_with_file(Some(("lib.rs", "pub fn original() {}\n")));
    let encoded = serde_json::to_vec(&fixture.selected)?;
    let limits = query_limits();
    for change in [
        "none",
        "root",
        "owner",
        "view",
        "bytes",
        "unknown_selection",
    ] {
        let mut first: QueryFactSelection = serde_json::from_slice(&encoded)?;
        let mut second: QueryFactSelection = serde_json::from_slice(&encoded)?;
        second.graph = None;
        let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
        let identities = super::super::identity::identities("local", &first, &mut budget)?;
        let second_identities = identities.clone();
        let public = identities
            .sources
            .first()
            .ok_or("source identity")?
            .1
            .clone();
        change_source_authority(change, &mut first, &mut second, &fixture)?;
        let input = super::super::retained::Input::prepare(
            vec![
                ("local".to_owned(), first, identities),
                ("local".to_owned(), second, second_identities),
            ],
            rift_protocol::query::QueryConfiguration::default(),
            rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
            &mut budget,
        );
        if !matches!(change, "none" | "view") {
            assert!(input.is_err(), "different {change} authority must refuse");
            continue;
        }
        let root = fixture.root.path().canonicalize()?;
        let response = input?.capture(
            &fixture.issuer,
            &root,
            &SymbolOwner::Local,
            &rift_protocol::query::QueryAnswer::Boolean { value: true },
            Vec::new(),
            &mut budget,
        )?;
        let rift_protocol::query::QueryReadResult::Results(response) = response else {
            return Err("same authority capture".into());
        };
        let view = response.view.ok_or("same authority view")?;
        let captured = fixture
            .issuer
            .query_view(&view.id, &root, &SymbolOwner::Local)
            .map_err(|reason| format!("same authority query: {reason:?}"))?;
        let metadata = super::super::retained::metadata(&captured)?;
        assert_eq!(
            metadata
                .sources
                .iter()
                .filter(|source| source.public == public)
                .count(),
            1
        );
        let (_, bodies) = super::super::replay::restore(&captured, &mut budget)?;
        assert_eq!(bodies.len(), 2);
        for (_, body, _) in bodies {
            assert!(
                body.facts
                    .sources
                    .iter()
                    .any(|source| source.text.as_deref() == Some("pub fn original() {}\n"))
            );
        }
    }
    Ok(())
}

fn change_source_authority(
    change: &str,
    first: &mut QueryFactSelection,
    second: &mut QueryFactSelection,
    fixture: &Fixture,
) -> Result<(), Box<dyn std::error::Error>> {
    match change {
        "none" => {}
        "root" => second.root.push_str("/other"),
        "owner" => {
            second.owner = SymbolOwner::NamedLocal {
                name: "other".into(),
            }
        }
        "view" => {
            second.view = fixture
                .issuer
                .capture_query_with(
                    rift_server::QueryViewInput::new(
                        fixture.root.path().canonicalize()?,
                        SymbolOwner::Local,
                        SystemTime::now() + Duration::from_secs(300),
                    )?,
                    |_| Ok(true),
                )?
                .ok_or("same owner independent view")?;
            assert_ne!(second.view.id, first.view.id);
        }
        "unknown_selection" => {
            for body in [&mut *first, &mut *second] {
                for source in &mut body.facts.sources {
                    source.digest = None;
                    source.text = None;
                }
            }
            second.selection.fingerprint =
                rift_search::query::source_text_digest("another immutable source selection")?;
            assert_eq!(first.root, second.root);
            assert_eq!(first.owner, second.owner);
            assert_ne!(first.selection.fingerprint, second.selection.fingerprint);
            assert!(
                first
                    .facts
                    .sources
                    .iter()
                    .chain(&second.facts.sources)
                    .all(|source| source.digest.is_none() && source.text.is_none())
            );
        }
        "bytes" => {
            let source = second
                .facts
                .sources
                .iter_mut()
                .find(|source| source.text.is_some())
                .ok_or("full original source")?;
            source.text = Some("pub fn replacement() {}\n".to_owned());
            source.digest = None;
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[test]
fn held_node_units_use_the_complete_source_directory_to_select_providers()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_search::query::FactRequirement;
    let unit = rift_protocol::read::SourceUnitId::parse("rift://source/project/lib.rs")?;
    let selected = FactRequirements {
        required: [FactRequirement::Nodes, FactRequirement::Source].into(),
        source_units: Some(vec![unit]),
        ..Default::default()
    };
    assert!(selected.selects_node_sources());
    let main =
        fixture_with_requirements(Some(("lib.rs", "pub fn original() {}\n")), Some(selected));
    let mut docs = fixture_with_file(Some(("lib.rs", "pub fn documentation() {}\n")));
    docs.selected
        .facts
        .unavailable_families
        .push(rift_search::query::QueryFactFamily::Nodes);
    assert!(main.selected.requirements.requires(FactRequirement::Nodes));
    assert!(!docs.selected.requirements.requires(FactRequirement::Nodes));
    let root = main.root.path().canonicalize()?;
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let main_ids = super::super::identity::identities("local", &main.selected, &mut budget)?;
    let docs_ids = super::super::identity::identities("docs", &docs.selected, &mut budget)?;
    let main_public = original_public_source(&main.selected, &main_ids, "pub fn original() {}\n")?;
    let docs_public =
        original_public_source(&docs.selected, &docs_ids, "pub fn documentation() {}\n")?;
    let input = super::super::retained::Input::prepare(
        vec![
            ("local".to_owned(), main.selected, main_ids),
            ("docs".to_owned(), docs.selected, docs_ids),
        ],
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        &mut budget,
    )?;
    let response = input.capture(
        &main.issuer,
        &root,
        &SymbolOwner::Local,
        &rift_protocol::query::QueryAnswer::Boolean { value: true },
        Vec::new(),
        &mut budget,
    )?;
    let rift_protocol::query::QueryReadResult::Results(response) = response else {
        return Err("source directory capture".into());
    };
    let view = response.view.ok_or("source directory view")?;
    let captured = main
        .issuer
        .query_view(&view.id, &root, &SymbolOwner::Local)
        .map_err(|reason| format!("source directory query: {reason:?}"))?;
    assert_selected_node_directory(&captured, main_public, docs_public, limits, &mut budget)
}

fn assert_selected_node_directory(
    captured: &rift_server::CapturedQueryView,
    main_public: rift_protocol::read::SourceUnitId,
    docs_public: rift_protocol::read::SourceUnitId,
    limits: rift_search::sparql::QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rift_search::query::FactRequirement;
    let (metadata, bodies) = super::super::replay::restore(captured, budget)?;
    let mut requested = FactRequirements {
        required: [FactRequirement::Nodes, FactRequirement::Source].into(),
        source_units: Some(vec![main_public]),
        ..Default::default()
    };
    let mask = super::super::replay::selected_node_providers(&metadata, &requested, budget)?
        .ok_or("proven selected provider")?;
    assert_eq!(mask, [true, false]);
    let services = rift_search::sparql::QueryServices::default();
    assert!(super::super::replay::covers_retained_source_bindings(
        &bodies,
        &requested,
        &services,
        None,
        limits,
        Some(&mask),
        budget
    )?);
    assert!(!super::super::replay::covers_retained_bindings(
        &bodies, &requested, &services, None, limits, budget
    )?);
    let raw = serde_json::to_value(&bodies[1].1)?;
    let mut excluded = rift_search::sparql::QueryServices::default();
    super::super::replay::include_empty_node_domain(
        &mut excluded,
        &bodies[1].1,
        &bodies[1].2,
        budget,
    )?;
    assert_eq!(excluded.datasets.len(), 1);
    assert!(excluded.datasets[0].facts.symbols.is_empty());
    assert!(excluded.datasets[0].facts.nodes.is_empty());
    assert_eq!(serde_json::to_value(&bodies[1].1)?, raw);
    assert!(super::super::replay::covers_requested(
        &bodies[..1],
        &requested,
        budget
    )?);
    let mut unknown = requested.clone();
    unknown.source_units.as_mut().ok_or("selected unit")?.push(
        rift_protocol::read::SourceUnitId::parse("rift://source/project/unknown.rs")?,
    );
    assert!(!super::super::replay::covers_requested(
        &bodies[..1],
        &unknown,
        budget
    )?);
    assert!(!super::super::replay::covers_retained_source_bindings(
        &bodies[..1],
        &unknown,
        &services,
        None,
        limits,
        None,
        budget,
    )?);
    requested.source_units = Some(vec![docs_public]);
    let mask = super::super::replay::selected_node_providers(&metadata, &requested, budget)?
        .ok_or("selected unavailable provider")?;
    assert_eq!(mask, [false, true]);
    assert!(requested.validate_facts(&bodies[1].1.facts).is_err());
    assert!(!super::super::replay::covers_retained_source_bindings(
        &bodies,
        &requested,
        &services,
        None,
        limits,
        Some(&mask),
        budget
    )?);
    requested.source_units = Some(vec![rift_protocol::read::SourceUnitId::parse(
        "rift://source/project/unknown.rs",
    )?]);
    assert!(super::super::replay::selected_node_providers(&metadata, &requested, budget).is_err());
    requested.source_units = None;
    assert!(
        super::super::replay::selected_node_providers(&metadata, &requested, budget)?.is_none()
    );
    Ok(())
}

fn assert_narrow_symbol_source_records(
    selected: &QueryFactSelection,
) -> Result<(), Box<dyn std::error::Error>> {
    use rift_search::query::FactRequirement;
    assert!(!selected.requirements.requires(FactRequirement::Source));
    let [original_source] = selected.facts.sources.as_slice() else {
        return Err("one original declaration source record".into());
    };
    assert_eq!(
        original_source.unit.as_str(),
        "rift://source/project/lib.rs"
    );
    assert_eq!(
        original_source.digest.as_ref(),
        Some(&rift_search::query::source_text_digest(
            "pub fn beacon() {}\n"
        )?)
    );
    assert!(original_source.text.is_none());
    Ok(())
}

#[test]
fn narrow_symbol_source_directory_allows_only_selected_recorded_node_reacquisition()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_search::query::FactRequirement;
    let requirements = rift_search::sparql::QueryPlan::prepare(
        "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }".to_owned(),
        query_limits(),
    )?
    .requirements()
    .clone();
    assert_eq!(requirements.symbol_name.as_deref(), Some("beacon"));
    let fixture =
        fixture_with_requirements(Some(("lib.rs", "pub fn beacon() {}\n")), Some(requirements));
    assert_narrow_symbol_source_records(&fixture.selected)?;
    assert!(!fixture.selected.facts.symbols.is_empty());
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let root = fixture.root.path().canonicalize()?;
    let captured = capture_fact_body(
        &fixture.issuer,
        &root,
        fixture.selected,
        &rift_protocol::query::QueryAnswer::Boolean { value: true },
        &mut budget,
    )?;
    std::fs::remove_file(root.join("lib.rs"))?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let saved = metadata
        .sources
        .first()
        .ok_or("retained Local declaration source authority")?;
    assert!(saved.bytes.is_none());
    let requested = FactRequirements {
        required: [FactRequirement::Nodes, FactRequirement::Source].into(),
        source_units: Some(vec![saved.public.clone()]),
        ..Default::default()
    };
    let selected =
        super::super::replay::selected_node_providers(&metadata, &requested, &mut budget)?
            .ok_or("exact recorded provider")?;
    assert_eq!(selected, [true]);
    let (_, body, identities) = &bodies[0];
    let original = serde_json::to_value(body)?;
    assert!(
        super::super::replay::source_requirements(
            body,
            identities,
            &requested,
            false,
            &mut budget
        )?
        .is_none()
    );
    let acquired = super::super::replay::source_requirements(
        body,
        identities,
        &requested,
        selected[0],
        &mut budget,
    )?
    .ok_or("closed Node scope on original provider")?;
    assert!(acquired.selects_node_sources());
    assert_eq!(
        acquired.source_units.as_deref(),
        Some(std::slice::from_ref(&saved.original))
    );
    assert_eq!(acquired.symbol_name, None);
    assert_eq!(
        serde_json::to_value(body)?,
        original,
        "raw name filter and certificate stay unchanged"
    );
    let mut unknown = requested.clone();
    unknown.source_units = Some(vec![rift_protocol::read::SourceUnitId::parse(
        "rift://source/project/unknown.rs",
    )?]);
    assert!(
        super::super::replay::selected_node_providers(&metadata, &unknown, &mut budget).is_err()
    );
    assert!(
        super::super::replay::source_requirements(body, identities, &unknown, true, &mut budget)
            .is_err()
    );
    let mut generic = requested;
    generic.required.insert(FactRequirement::Domain);
    assert!(
        super::super::replay::source_requirements(body, identities, &generic, true, &mut budget)
            .is_err()
    );
    assert!(
        super::super::replay::source_requirements(body, identities, &generic, false, &mut budget)?
            .is_none()
    );
    Ok(())
}

const RECORDED_DEPENDENCY_SYMBOL: &str =
    "rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/beacon";

fn capture_missing_symbol_selection(
    proxy: &super::super::RiftProxy,
    fixture: &Fixture,
    backend: &str,
) -> Result<rift_protocol::symbol_read::CapturedView, Box<dyn std::error::Error>> {
    let mut selected: QueryFactSelection =
        serde_json::from_value(serde_json::to_value(&fixture.selected)?)?;
    let symbol = selected
        .facts
        .symbols
        .iter_mut()
        .find(|row| row.symbol.name == "beacon")
        .ok_or("original beacon record")?;
    symbol.symbol.id = Some(rift_protocol::read::SymbolId::parse(
        RECORDED_DEPENDENCY_SYMBOL,
    )?);
    selected.view.id = rift_protocol::symbol_read::CapturedViewId::parse("claims.remote")?;
    let original_view = selected.view.clone();
    let limits = query_limits();
    let mut budget = rift_server::QueryFactBudget::new(limits, &|| false)?;
    let identities = super::super::identity::identities(backend, &selected, &mut budget)?;
    let input = super::super::retained::Input::prepare(
        vec![(backend.to_owned(), selected, identities)],
        rift_protocol::query::QueryConfiguration::default(),
        rift_server::QueryProcessLimits::from_query(limits)?.material_bytes_max,
        &mut budget,
    )?;
    let answer = rift_protocol::query::QueryAnswer::Bindings {
        variables: vec!["symbol".to_owned()],
        bindings: vec![std::collections::BTreeMap::from([(
            "symbol".to_owned(),
            rift_protocol::query::RdfTerm::Iri {
                value: RECORDED_DEPENDENCY_SYMBOL.to_owned(),
            },
        )])],
    };
    let issuer = proxy.query_views.get().ok_or("query material owner")?;
    let root = proxy.accepted_root.as_deref().ok_or("accepted root")?;
    let response = input.capture(issuer, root, &proxy.owner, &answer, Vec::new(), &mut budget)?;
    let rift_protocol::query::QueryReadResult::Results(response) = response else {
        return Err("retained symbol selection".into());
    };
    assert_eq!(response.answer, answer);
    let view = response.view.ok_or("public query view")?;
    assert_ne!(view.id, original_view.id);
    assert!(
        crate::proxy::captured_expiry(&view)? <= crate::proxy::captured_expiry(&original_view)?
    );
    let captured = issuer
        .query_view(&view.id, root, &proxy.owner)
        .map_err(|reason| format!("original query material: {reason:?}"))?;
    let metadata = super::super::retained::metadata(&captured)?;
    assert_eq!(metadata.providers.len(), 1);
    assert_eq!(metadata.providers[0].backend, backend);
    assert_eq!(metadata.providers[0].root, fixture.selected.root);
    assert_eq!(metadata.providers[0].owner, SymbolOwner::Local);
    assert_eq!(metadata.providers[0].view, original_view);
    assert_eq!(
        serde_json::to_value(&metadata.providers[0].selection)?,
        serde_json::to_value(&fixture.selected.selection)?
    );
    Ok(view)
}

async fn answer_recorded_missing_symbol(
    backend: &super::super::RiftProxy,
    project: &'static str,
    raw_view: rift_protocol::symbol_read::CapturedView,
    returned_view: Option<rift_protocol::symbol_read::CapturedView>,
    returned_id: Option<&'static str>,
) -> tokio::task::JoinHandle<std::io::Result<usize>> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let (running, upstream) = crate::proxy::tests::direct_upstream();
    {
        let mut slot = backend.upstream.lock().await;
        slot.connected = Some(crate::proxy::Upstream {
            running,
            generation: 0,
        });
        slot.generation_next = 1;
    }
    let returned_view = returned_view.unwrap_or_else(|| raw_view.clone());
    let returned_id = returned_id.unwrap_or(RECORDED_DEPENDENCY_SYMBOL);
    tokio::spawn(async move {
        let (reading, mut writing) = tokio::io::split(upstream);
        let mut lines = tokio::io::BufReader::new(reading).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let params = &request["params"];
        assert_eq!(params["name"], "get_symbol");
        assert_eq!(params["arguments"]["id"], RECORDED_DEPENDENCY_SYMBOL);
        assert_eq!(params["arguments"]["view"], raw_view.id.as_str());
        assert!(params["arguments"].get("rev").is_none());
        let response = serde_json::json!({
            "jsonrpc":"2.0", "id":request["id"], "result":{
                "content":[], "isError":true,
                "structuredContent":{
                    "outcome":"missing", "symbol_not_found":{
                        "id":returned_id,
                        "alternatives":[], "detail":project
                    }, "view":returned_view
                }
            }
        });
        writing
            .write_all(format!("{response}\n").as_bytes())
            .await?;
        Ok(1)
    })
}

#[tokio::test]
async fn query_material_missing_symbols_keep_project_views_for_one_remote_token()
-> Result<(), Box<dyn std::error::Error>> {
    let main = fixture_with_file(Some(("lib.rs", "pub fn beacon() {}\n")));
    let cloud = fixture_with_file(Some(("lib.rs", "pub fn beacon() {}\n")));
    assert_ne!(main.selected.root, cloud.selected.root);
    assert_ne!(main.selected.selection.root, cloud.selected.selection.root);
    let proxy = crate::proxy::tests::named_proxy(main.root.path(), cloud.root.path())?;
    let first = capture_missing_symbol_selection(&proxy, &main, "local")?;
    let second = capture_missing_symbol_selection(&proxy, &cloud, "cloud")?;
    assert_ne!(first.id, second.id);
    let remote = |fixture: &Fixture| -> Result<_, rift_error::RiftError> {
        Ok(rift_protocol::symbol_read::CapturedView {
            id: rift_protocol::symbol_read::CapturedViewId::parse("claims.remote")?,
            expires_at: fixture.selected.view.expires_at.clone(),
        })
    };
    let main_answer =
        answer_recorded_missing_symbol(&proxy, "main", remote(&main)?, None, None).await;
    let cloud_answer = answer_recorded_missing_symbol(
        &proxy.backends["cloud"],
        "cloud",
        remote(&cloud)?,
        None,
        None,
    )
    .await;
    for (expected, view) in [("main", &first), ("cloud", &second)] {
        let request = rmcp::model::CallToolRequestParams::new("get_symbol").with_arguments(
            serde_json::from_value(serde_json::json!({
                "id":RECORDED_DEPENDENCY_SYMBOL, "view":view.id
            }))?,
        );
        let rmcp::model::CallToolResponse::Complete(result) = proxy
            .query_material_read(&request)
            .await?
            .ok_or("original query symbol directory")??
        else {
            return Err("complete missing symbol response".into());
        };
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.ok_or("typed Missing result")?;
        assert_eq!(body["symbol_not_found"]["detail"], expected);
        let typed: rift_protocol::symbol_read::GetSymbolResult = serde_json::from_value(body)?;
        assert!(
            matches!(typed, rift_protocol::symbol_read::GetSymbolResult::Missing {
            view: selected, symbol_not_found, ..
        } if selected == *view && symbol_not_found.id.as_str() == RECORDED_DEPENDENCY_SYMBOL)
        );
    }
    assert_eq!(main_answer.await??, 1);
    assert_eq!(cloud_answer.await??, 1);
    assert_eq!(proxy.upstream.lock().await.generation_next, 1);
    assert_eq!(
        proxy.backends["cloud"]
            .upstream
            .lock()
            .await
            .generation_next,
        1
    );
    Ok(())
}

#[tokio::test]
async fn query_material_missing_symbols_refuse_changed_original_view_or_identity()
-> Result<(), Box<dyn std::error::Error>> {
    for field in ["view", "expiry", "symbol"] {
        assert_mismatched_missing_response(field).await?;
    }
    Ok(())
}

async fn assert_mismatched_missing_response(field: &str) -> Result<(), Box<dyn std::error::Error>> {
    let main = fixture_with_file(Some(("lib.rs", "pub fn beacon() {}\n")));
    let cloud = fixture_with_file(Some(("lib.rs", "pub fn beacon() {}\n")));
    let proxy = crate::proxy::tests::named_proxy(main.root.path(), cloud.root.path())?;
    let public = capture_missing_symbol_selection(&proxy, &cloud, "cloud")?;
    let mut original = cloud.selected.view.clone();
    original.id = rift_protocol::symbol_read::CapturedViewId::parse("claims.remote")?;
    let mut returned = original.clone();
    let mut returned_id = None;
    match field {
        "view" => {
            returned.id = rift_protocol::symbol_read::CapturedViewId::parse("claims.foreign")?;
        }
        "expiry" => {
            returned.expires_at = jiff::Timestamp::now()
                .checked_add(std::time::Duration::from_secs(600))?
                .to_string();
        }
        "symbol" => {
            returned_id = Some("rift://symbol/cargo/crates.io/other@1.0.0/rust/other/beacon");
        }
        _ => unreachable!(),
    }
    let answering = answer_recorded_missing_symbol(
        &proxy.backends["cloud"],
        "cloud",
        original,
        Some(returned),
        returned_id,
    )
    .await;
    let request = rmcp::model::CallToolRequestParams::new("get_symbol").with_arguments(
        serde_json::from_value(serde_json::json!({
            "id":RECORDED_DEPENDENCY_SYMBOL, "view":public.id
        }))?,
    );
    let error = proxy
        .query_material_read(&request)
        .await
        .expect_err("a different original symbol or full view cannot be qualified");
    assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
    assert_eq!(answering.await??, 1);
    assert_eq!(
        proxy.backends["cloud"]
            .upstream
            .lock()
            .await
            .generation_next,
        1
    );
    assert_eq!(proxy.upstream.lock().await.generation_next, 0);
    Ok(())
}

#[test]
fn held_source_reads_do_not_require_symbol_facts_from_a_narrow_query()
-> Result<(), Box<dyn std::error::Error>> {
    use rift_search::query::FactRequirement;
    let plan = rift_search::sparql::QueryPlan::prepare(
        "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }".to_owned(),
        query_limits(),
    )?;
    let fixture = fixture_with_requirements(
        Some(("lib.rs", "pub fn beacon() {}\n")),
        Some(plan.requirements().clone()),
    );
    assert_eq!(
        fixture.selected.requirements.symbol_name.as_deref(),
        Some("beacon")
    );
    let mut budget = rift_server::QueryFactBudget::new(query_limits(), &|| false)?;
    let root = fixture.root.path().canonicalize()?;
    let captured = capture_fact_body(
        &fixture.issuer,
        &root,
        fixture.selected,
        &rift_protocol::query::QueryAnswer::Boolean { value: true },
        &mut budget,
    )?;
    let (metadata, bodies) = super::super::replay::restore(&captured, &mut budget)?;
    let source = metadata
        .sources
        .iter()
        .find(|source| source.original.as_str() == "rift://source/project/lib.rs")
        .ok_or("original library source authority")?;
    let view = oxigraph::model::Literal::from(captured.view().id.as_str());
    let statement = format!(
        "SELECT ?text {{ SERVICE <{}{}> {{}} SERVICE :source {{ ?read :unit <{}>; :view {view}; :outcome 'found'; :source [ :text ?text ] }} }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        captured.view().id.as_str(),
        source.public.as_str(),
    );
    let plan = rift_search::sparql::QueryPlan::prepare(statement, query_limits())?;
    let requested = plan.requirements();
    assert!(requested.required.is_empty());
    assert_eq!(requested.source_reads.len(), 1);
    let original = serde_json::to_value(&bodies)?;
    assert!(super::super::replay::covers_requested(
        &bodies,
        requested,
        &mut budget
    )?);
    let mut domain = requested.clone();
    domain.require(FactRequirement::Domain);
    assert!(!super::super::replay::covers_requested(
        &bodies,
        &domain,
        &mut budget
    )?);
    let mut other_service = requested.clone();
    other_service
        .services
        .push(rift_search::sparql::SEARCH_SERVICE.to_owned());
    assert!(!super::super::replay::covers_requested(
        &bodies,
        &other_service,
        &mut budget
    )?);
    assert_eq!(serde_json::to_value(&bodies)?, original);
    Ok(())
}
