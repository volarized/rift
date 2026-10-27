use super::super::VirtualDataset;
use crate::query::{
    QueryFacts, QueryLimits, QueryProviderContext, QueryProviderDeclaration, QueryProviderEndpoint,
    QueryProviderInput, QueryProviderRecord, QueryProviderSelection, QueryProviderSnapshot,
    QuerySource, QuerySymbol,
};
use oxigraph::model::Term;
use oxigraph::sparql::CancellationToken;
use rift_core::{SourceVisibility, TextFileInclusion, WorkspaceId};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::identity::SourceDigest;
use rift_protocol::read::SourceUnitId;
use rift_provider::NormalizedTarget;

const UNRELATED_SOURCE: &str = "unrelated = 1\n";

fn physical_facts() -> Result<QueryFacts, Box<dyn std::error::Error>> {
    physical_facts_with_source("int beacon(int value) { return value; }\n")
}

fn physical_facts_with_source(text: &str) -> Result<QueryFacts, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("beacon.c"), text)?;
    std::fs::write(directory.path().join("rift.toml"), UNRELATED_SOURCE)?;
    let index = WorkspaceIndex::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?;
    let view = index.symbol_view()?;
    for declaration in view.unestablished_declarations() {
        let (object, _, declaration) = declaration?;
        if object.facts().name() != "beacon" {
            continue;
        }
        let graph = view.normalized_graph();
        let key = object
            .assembled()
            .contributions()
            .first()
            .ok_or("original contribution")?;
        let record = graph
            .record_for(key.reference())
            .ok_or("original normalized record")?;
        let binding = graph
            .contribution(key)
            .and_then(|contribution| contribution.source())
            .ok_or("original declaration binding")?;
        let digest = SourceDigest::parse(
            &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes()),
        )?;
        let selection = QueryProviderSelection {
            root: WorkspaceId::new("file:///selected-provider")?,
            owner: view.owner().clone(),
            index_revision: graph.index_revision(),
            source_revision: graph.source_revision(),
            tree_revision: graph.tree_revision(),
            fingerprint: SourceDigest::parse(
                &data_encoding::HEXLOWER.encode(view.fingerprint().as_bytes()),
            )?,
            configuration: digest.clone(),
        };
        let symbol = object.to_protocol_symbol();
        assert!(symbol.id.is_none());
        return Ok(QueryFacts {
            symbols: vec![QuerySymbol {
                symbol,
                declarations: vec![declaration.clone()],
            }],
            sources: vec![QuerySource {
                unit: SourceUnitId(binding.unit().to_string()),
                digest: Some(digest.clone()),
                text: Some(text.to_owned()),
            }],
            providers: vec![QueryProviderSnapshot {
                context: QueryProviderContext {
                    root: selection.root.clone(),
                    owner: selection.owner.clone(),
                    index_revision: selection.index_revision,
                    source_revision: selection.source_revision,
                    tree_revision: selection.tree_revision,
                    fingerprint: selection.fingerprint.clone(),
                    selection,
                    source_digest: digest.clone(),
                    inputs: vec![QueryProviderInput {
                        uri: binding.unit().to_string(),
                        digest,
                    }],
                },
                records: vec![QueryProviderRecord {
                    symbol_index: 0,
                    identity: record.identity().cloned(),
                    contributions: record.contributions().to_vec(),
                    declarations: vec![QueryProviderDeclaration {
                        declaration_index: 0,
                        contribution: key.clone(),
                        binding: binding.clone(),
                    }],
                    container: None,
                }],
                source_indices: vec![0],
                references: Vec::new(),
                relationships: Vec::new(),
            }],
            ..QueryFacts::default()
        });
    }
    Err("captured physical declaration".into())
}

#[test]
fn selected_provider_resource_rejects_foreign_or_missing_associations()
-> Result<(), Box<dyn std::error::Error>> {
    let facts = physical_facts()?;
    let endpoint = QueryProviderEndpoint {
        provider: Some(0),
        target: facts.providers[0].records[0]
            .target()
            .ok_or("original target")?,
    };
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let dataset = VirtualDataset::new(facts, limits, CancellationToken::new())?;
    let resource = dataset.selected_resource(0, &endpoint)?;
    assert!(matches!(resource, Term::BlankNode(_)));
    assert!(dataset.selected_resource(1, &endpoint).is_err());
    assert!(
        dataset
            .selected_resource(
                0,
                &QueryProviderEndpoint {
                    provider: Some(1),
                    ..endpoint.clone()
                }
            )
            .is_err()
    );
    assert!(
        dataset
            .selected_resource(
                0,
                &QueryProviderEndpoint {
                    provider: Some(0),
                    target: NormalizedTarget::Symbol(rift_core::SymbolId::new("missing")?),
                }
            )
            .is_err()
    );
    assert!(
        dataset
            .selected_resource(
                0,
                &QueryProviderEndpoint {
                    provider: None,
                    target: NormalizedTarget::Symbol(rift_core::SymbolId::new("missing")?),
                }
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn original_provider_keys_ignore_projected_validation_rows()
-> Result<(), Box<dyn std::error::Error>> {
    let original = physical_facts()?;
    original.validate_providers()?;
    let mut projected = original.clone();
    let digest = source_digest(UNRELATED_SOURCE)?;
    let source = QuerySource {
        unit: SourceUnitId::parse("rift://source/project/rift.toml")?,
        digest: Some(digest.clone()),
        text: None,
    };
    projected.providers[0]
        .source_indices
        .push(projected.sources.len());
    projected.providers[0].context.source_digest = digest.clone();
    projected.providers[0]
        .context
        .inputs
        .push(QueryProviderInput {
            uri: source.unit.as_str().to_owned(),
            digest,
        });
    projected.sources.push(source);
    projected.validate_providers()?;
    let snapshot = &original.providers[0];
    let changed = &projected.providers[0];
    let record = &snapshot.records[0];
    let target = record.target().ok_or("original normalized target")?;
    assert_eq!(
        record.key(&snapshot.context)?,
        record.key(&changed.context)?
    );
    assert_eq!(
        snapshot.context.target_key(&target)?,
        changed.context.target_key(&target)?
    );
    assert_eq!(
        physical_resource(original)?,
        physical_resource(projected.clone())?
    );
    projected.providers[0].context.source_digest = source_digest("uncaptured source")?;
    assert!(projected.validate_providers().is_err());
    Ok(())
}

fn source_digest(text: &str) -> Result<SourceDigest, Box<dyn std::error::Error>> {
    Ok(SourceDigest::parse(&data_encoding::HEXLOWER.encode(
        rift_core::FileDigest::of(text.as_bytes()).as_bytes(),
    ))?)
}

fn physical_resource(facts: QueryFacts) -> Result<Term, Box<dyn std::error::Error>> {
    let target = facts.providers[0].records[0]
        .target()
        .ok_or("original target")?;
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let dataset = VirtualDataset::new(facts, limits, CancellationToken::new())?;
    Ok(dataset.selected_resource(
        0,
        &QueryProviderEndpoint {
            provider: Some(0),
            target,
        },
    )?)
}

#[test]
fn original_provider_keys_separate_owner_root_revisions_and_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let facts = physical_facts()?;
    let snapshot = &facts.providers[0];
    let original = &snapshot.context;
    let record = &snapshot.records[0];
    let target = record.target().ok_or("original target")?;
    let expected_record = record.key(original)?;
    let expected_target = original.target_key(&target)?;
    let mut variants = Vec::new();
    let mut changed = original.clone();
    changed.fingerprint = source_digest("different captured bytes")?;
    variants.push(("fingerprint", changed));
    let mut changed = original.clone();
    changed.root = WorkspaceId::new("file:///another-provider")?;
    variants.push(("root", changed));
    let mut changed = original.clone();
    changed.owner = rift_protocol::identity::SymbolIdentity::parse(
        "rift://symbol/stdlib/cpython@3.12.3/python/builtins/len",
    )?
    .owner()
    .clone();
    variants.push(("owner", changed));
    let mut changed = original.clone();
    changed.index_revision =
        rift_core::IndexRevision::new(original.index_revision.get().checked_add(1).unwrap_or(1))?;
    variants.push(("index_revision", changed));
    let mut changed = original.clone();
    changed.source_revision =
        rift_core::SourceRevision::new(original.source_revision.get().checked_add(1).unwrap_or(1))?;
    variants.push(("source_revision", changed));
    let mut changed = original.clone();
    changed.tree_revision =
        rift_core::TreeRevision::new(original.tree_revision.get().checked_add(1).unwrap_or(1))?;
    variants.push(("tree_revision", changed));
    let mut changed = original.clone();
    changed.selection.configuration = source_digest("different accepted configuration")?;
    variants.push(("configuration", changed));
    for (field, changed) in variants {
        assert_ne!(record.key(&changed)?, expected_record, "record {field}");
        assert_ne!(
            changed.target_key(&target)?,
            expected_target,
            "target {field}"
        );
    }
    Ok(())
}

#[test]
fn provider_context_decode_requires_original_fingerprint() -> Result<(), Box<dyn std::error::Error>>
{
    let facts = physical_facts()?;
    let mut value = serde_json::to_value(&facts.providers[0].context)?;
    value
        .as_object_mut()
        .ok_or("context fields")?
        .remove("fingerprint");
    assert!(serde_json::from_value::<QueryProviderContext>(value).is_err());
    Ok(())
}

#[test]
fn provider_target_keys_preserve_adjacent_u64_revisions() -> Result<(), Box<dyn std::error::Error>>
{
    let facts = physical_facts()?;
    let snapshot = &facts.providers[0];
    let record = &snapshot.records[0];
    let target = record.target().ok_or("original normalized target")?;
    let mut first = snapshot.context.clone();
    first.index_revision = rift_core::IndexRevision::new(9_007_199_254_740_992)?;
    let mut second = first.clone();
    second.index_revision = rift_core::IndexRevision::new(9_007_199_254_740_993)?;
    assert_ne!(first.target_key(&target)?, second.target_key(&target)?);
    assert_ne!(record.key(&first)?, record.key(&second)?);
    Ok(())
}

#[test]
fn accepted_configuration_digests_preserve_adjacent_large_counts() {
    let mut first = rift_protocol::configuration::WorkspaceConfiguration::default();
    let mut second = rift_protocol::configuration::WorkspaceConfiguration::default();
    first.global.max_symbols = 1_u64 << 53;
    second.global.max_symbols = first.global.max_symbols + 1;
    first.validate().expect("accepted positive global count");
    second.validate().expect("accepted adjacent global count");
    assert_ne!(
        serde_json::to_vec(&first).expect("first accepted configuration"),
        serde_json::to_vec(&second).expect("second accepted configuration")
    );
    assert_ne!(
        crate::query::QueryProviderSelection::configuration_digest(&first)
            .expect("first configuration digest"),
        crate::query::QueryProviderSelection::configuration_digest(&second)
            .expect("second configuration digest"),
    );
}

#[test]
fn accepted_configuration_digests_ignore_object_order_and_keep_array_order() {
    use crate::query::QueryProviderSelection;
    use rift_protocol::configuration::WorkspaceConfiguration;

    let first: WorkspaceConfiguration = serde_json::from_str(
        r#"{
        "lsp":{"probe":{"command":["unused-server"],"initialization_options":{
            "z":{"b":2,"a":1},"a":[{"z":1,"a":2},3]
        }}}
    }"#,
    )
    .expect("first accepted options");
    let second: WorkspaceConfiguration = serde_json::from_str(
        r#"{
        "lsp":{"probe":{"command":["unused-server"],"initialization_options":{
            "a":[{"a":2,"z":1},3],"z":{"a":1,"b":2}
        }}}
    }"#,
    )
    .expect("reordered accepted options");
    first.validate().expect("accepted first configuration");
    second.validate().expect("accepted reordered configuration");
    assert_eq!(first, second);
    assert_ne!(
        serde_json::to_vec(&first).expect("first source order"),
        serde_json::to_vec(&second).expect("second source order")
    );
    let digest = QueryProviderSelection::configuration_digest(&first).expect("first digest");
    assert_eq!(
        digest,
        QueryProviderSelection::configuration_digest(&second).expect("reordered digest")
    );
    let mut changed = second;
    changed
        .lsp
        .get_mut("probe")
        .expect("original process")
        .initialization_options = Some(serde_json::json!({
        "a":[3,{"a":2,"z":1}],"z":{"a":1,"b":2}
    }));
    changed.validate().expect("accepted changed array");
    assert_ne!(
        digest,
        QueryProviderSelection::configuration_digest(&changed).expect("changed digest")
    );
}

fn default_owner(
    facts: QueryFacts,
) -> Result<crate::sparql::QueryDataset, Box<dyn std::error::Error>> {
    let selection = &facts.providers[0].context.selection;
    let bytes = serde_json::to_vec(selection)?;
    let prefix = SourceDigest::parse(
        &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(&bytes).as_bytes()),
    )?;
    let sources = facts
        .sources
        .iter()
        .map(|source| {
            Ok((
                source.unit.clone(),
                crate::query::qualify_source_unit(&source.unit, Some(&prefix))?,
            ))
        })
        .collect::<Result<Vec<_>, rift_error::RiftError>>()?;
    Ok(crate::sparql::QueryDataset {
        graph: None,
        facts,
        identities: crate::sparql::QueryIdentities {
            symbols: Vec::new(),
            sources,
            blank_prefix: prefix,
        },
        default: true,
    })
}

#[test]
fn routing_owner_reorder_and_filter_keep_original_physical_resource_and_source()
-> Result<(), Box<dyn std::error::Error>> {
    let selected = physical_facts_with_source("int beacon(int value) { return 0; }\n")?;
    let other = physical_facts_with_source("int beacon(int value) { return 1; }\n")?;
    assert_eq!(selected.symbols[0].symbol, other.symbols[0].symbol);
    let endpoint = QueryProviderEndpoint {
        provider: Some(0),
        target: selected.providers[0].records[0]
            .target()
            .ok_or("selected original target")?,
    };
    let selected = default_owner(selected)?;
    let other = default_owner(other)?;
    assert_ne!(selected.identities.sources, other.identities.sources);
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let dataset = |owners| {
        VirtualDataset::new(QueryFacts::default(), limits, CancellationToken::new())?
            .with_datasets(owners)
    };
    let original = dataset(vec![selected.clone(), other.clone()])?;
    let resource = original.selected_resource(1, &endpoint)?;
    assert!(matches!(resource, Term::BlankNode(_)));
    let reordered = dataset(vec![other.clone(), selected.clone()])?;
    assert_eq!(resource, reordered.selected_resource(2, &endpoint)?);
    if let Ok(unrelated) = reordered.selected_resource(1, &endpoint) {
        assert_ne!(resource, unrelated);
    }
    let filtered = dataset(vec![selected.clone()])?;
    assert_eq!(resource, filtered.selected_resource(1, &endpoint)?);
    let statement = "SELECT ?unit ?source { ?unit :source ?source } ORDER BY ?unit";
    let evaluate = |owners| {
        crate::sparql::PreparedQuery::parse(statement, limits)?.evaluate_with_services(
            QueryFacts::default(),
            limits,
            crate::sparql::QueryCancellation::default(),
            crate::sparql::QueryServices {
                datasets: owners,
                ..crate::sparql::QueryServices::default()
            },
        )
    };
    assert_eq!(
        evaluate(vec![selected.clone(), other.clone()])?,
        evaluate(vec![other, selected.clone()])?
    );
    let crate::sparql::QueryAnswer::Bindings { bindings, .. } = evaluate(vec![selected.clone()])?
    else {
        return Err("filtered original source bindings".into());
    };
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]["source"],
        rift_protocol::query::RdfTerm::Literal {
            value: "int beacon(int value) { return 0; }\n".to_owned(),
            datatype: "http://www.w3.org/2001/XMLSchema#string".to_owned(),
        }
    );
    assert_eq!(
        selected.identities.sources[0].1.as_str(),
        match &bindings[0]["unit"] {
            rift_protocol::query::RdfTerm::Iri { value } => value,
            _ => return Err("filtered original source unit".into()),
        }
    );
    assert!(filtered.selected_resource(2, &endpoint).is_err());
    Ok(())
}

fn evaluate_owners(
    statement: &str,
    owners: Vec<crate::sparql::QueryDataset>,
) -> Result<crate::sparql::QueryAnswer, Box<dyn std::error::Error>> {
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    Ok(
        crate::sparql::PreparedQuery::parse(statement, limits)?.evaluate_with_services(
            QueryFacts::default(),
            limits,
            crate::sparql::QueryCancellation::default(),
            crate::sparql::QueryServices {
                datasets: owners,
                ..crate::sparql::QueryServices::default()
            },
        )?,
    )
}

#[test]
fn domain_and_service_roles_share_original_resources_and_default_triples()
-> Result<(), Box<dyn std::error::Error>> {
    let mut domain = default_owner(physical_facts()?)?;
    domain.graph = Some(crate::sparql::LOCAL_GRAPH.to_owned());
    domain.identities.sources = domain
        .facts
        .sources
        .iter()
        .map(|source| (source.unit.clone(), source.unit.clone()))
        .collect();
    let mut service = domain.clone();
    service.graph = None;
    let endpoint = QueryProviderEndpoint {
        provider: Some(0),
        target: domain.facts.providers[0].records[0]
            .target()
            .ok_or("original physical target")?,
    };
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let dataset = VirtualDataset::new(QueryFacts::default(), limits, CancellationToken::new())?
        .with_datasets(vec![domain.clone(), service.clone()])?;
    let resource = dataset.selected_resource(1, &endpoint)?;
    assert!(matches!(resource, Term::BlankNode(_)));
    assert_eq!(resource, dataset.selected_resource(2, &endpoint)?);
    let all = "SELECT ?subject ?predicate ?object { ?subject ?predicate ?object } ORDER BY ?subject ?predicate ?object";
    let original = evaluate_owners(all, vec![domain.clone()])?;
    assert_eq!(
        original,
        evaluate_owners(all, vec![domain.clone(), service.clone()])?
    );
    assert_eq!(
        original,
        evaluate_owners(all, vec![service.clone(), domain.clone()])?
    );
    let source = domain.facts.sources[0].unit.as_str();
    let text = oxigraph::model::Literal::from("int beacon(int value) { return value; }\n");
    let read = format!("ASK {{ <{source}> :source {text} }}");
    assert_eq!(
        evaluate_owners(&read, vec![domain.clone(), service.clone()])?,
        crate::sparql::QueryAnswer::Boolean { value: true }
    );
    let graph = "SELECT ?graph { GRAPH ?graph {} }";
    let crate::sparql::QueryAnswer::Bindings { bindings, .. } =
        evaluate_owners(graph, vec![service, domain.clone()])?
    else {
        return Err("named graph bindings".into());
    };
    assert_eq!(
        bindings,
        vec![std::collections::BTreeMap::from([(
            "graph".to_owned(),
            rift_protocol::query::RdfTerm::Iri {
                value: crate::sparql::LOCAL_GRAPH.to_owned()
            }
        )])]
    );
    assert!(evaluate_owners(all, vec![domain.clone(), domain]).is_err());
    Ok(())
}

#[test]
fn owned_empty_and_legacy_empty_graphs_keep_distinct_dispositions()
-> Result<(), Box<dyn std::error::Error>> {
    let mut owner = default_owner(physical_facts()?)?;
    owner.facts = QueryFacts::default();
    owner.graph = Some(crate::sparql::LOCAL_GRAPH.to_owned());
    let statement = "SELECT ?graph { GRAPH ?graph {} }";
    let named = evaluate_owners(statement, vec![owner.clone()])?;
    let legacy = evaluate_owners(statement, Vec::new())?;
    assert_eq!(named, legacy);
    let crate::sparql::QueryAnswer::Bindings { bindings, .. } = &named else {
        return Err("owned empty graph bindings".into());
    };
    assert_eq!(bindings.len(), 1);
    owner.graph = None;
    let crate::sparql::QueryAnswer::Bindings { bindings, .. } =
        evaluate_owners(statement, vec![owner])?
    else {
        return Err("unnamed empty graph bindings".into());
    };
    assert!(bindings.is_empty());
    Ok(())
}

#[test]
fn retained_subject_resources_match_original_physical_profile_projection()
-> Result<(), Box<dyn std::error::Error>> {
    let body = default_owner(physical_facts()?)?;
    let endpoint = QueryProviderEndpoint {
        provider: Some(0),
        target: body.facts.providers[0].records[0]
            .target()
            .ok_or("original target")?,
    };
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let selected = VirtualDataset::new(QueryFacts::default(), limits, CancellationToken::new())?
        .with_datasets(vec![body.clone()])?;
    let expected = crate::sparql::result::term(selected.selected_resource(1, &endpoint)?);
    let mut admitted = 0usize;
    let resources = crate::sparql::retained_subject_resources(
        &body.facts,
        body.identities,
        &[0],
        limits,
        &crate::sparql::QueryCancellation::default(),
        &mut |bytes| {
            admitted = admitted
                .checked_add(bytes)
                .ok_or_else(|| crate::sparql::failure::bound("material_bytes_max"))?;
            Ok(())
        },
    )?;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].resource, expected);
    assert_eq!(resources[0].symbol, 0);
    assert!(resources[0].original);
    assert!(admitted > size_of::<crate::sparql::QuerySubjectResource>());
    Ok(())
}

#[test]
fn retained_subject_resources_preserve_nonselected_and_equal_resources()
-> Result<(), Box<dyn std::error::Error>> {
    let mut body = default_owner(physical_facts()?)?;
    let canonical = rift_protocol::read::SymbolId::from_identity(
        &rift_protocol::identity::SymbolIdentity::parse("rift://symbol/local/rust/fixture/beacon")?,
    );
    body.facts.symbols[0].symbol.id = Some(canonical);
    body.facts.symbols.push(body.facts.symbols[0].clone());
    body.facts.providers.clear();
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let resources = crate::sparql::retained_subject_resources(
        &body.facts,
        body.identities,
        &[0],
        limits,
        &crate::sparql::QueryCancellation::default(),
        &mut |_| Ok(()),
    )?;
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0].resource, resources[1].resource);
    assert!(resources[0].original);
    assert!(!resources[1].original);
    assert_eq!((resources[0].symbol, resources[1].symbol), (0, 1));
    Ok(())
}

#[test]
fn retained_subject_resources_refuse_before_output_copy_and_keep_cancellation()
-> Result<(), Box<dyn std::error::Error>> {
    let body = default_owner(physical_facts()?)?;
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let cancel = crate::sparql::QueryCancellation::default();
    let refused = crate::sparql::retained_subject_resources(
        &body.facts,
        body.identities.clone(),
        &[0],
        limits,
        &cancel,
        &mut |_| Err(crate::sparql::failure::bound("material_bytes_max")),
    );
    assert!(refused.is_err());
    let missing = crate::sparql::retained_subject_resources(
        &body.facts,
        body.identities.clone(),
        &[body.facts.symbols.len()],
        limits,
        &cancel,
        &mut |_| Ok(()),
    );
    assert!(missing.is_err());
    cancel.cancel();
    assert!(
        crate::sparql::retained_subject_resources(
            &body.facts,
            body.identities,
            &[0],
            limits,
            &cancel,
            &mut |_| Ok(()),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn retained_subject_resources_keep_payload_only_anonymous_originals()
-> Result<(), Box<dyn std::error::Error>> {
    let mut body = default_owner(physical_facts()?)?;
    body.facts.providers.clear();
    assert!(body.facts.symbols[0].symbol.id.is_none());
    let answer = evaluate_owners(
        "SELECT ?symbol { ?symbol :name 'beacon' }",
        vec![body.clone()],
    )?;
    let crate::sparql::QueryAnswer::Bindings { bindings, .. } = answer else {
        return Err("original anonymous binding".into());
    };
    assert_eq!(bindings.len(), 1);
    let expected = bindings[0]
        .get("symbol")
        .ok_or("original anonymous resource")?;
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let resources = crate::sparql::retained_subject_resources(
        &body.facts,
        body.identities,
        &[0],
        limits,
        &crate::sparql::QueryCancellation::default(),
        &mut |_| Ok(()),
    )?;
    assert_eq!(&resources[0].resource, expected);
    assert!(resources[0].original);
    Ok(())
}

#[test]
fn streamed_provider_resources_match_encoded_keys_and_refuse_before_copy()
-> Result<(), Box<dyn std::error::Error>> {
    let mut facts = physical_facts_with_source("int beacon(int value) { return value; }\n")?;
    let index = facts.providers[0].records[0].symbol_index;
    facts.symbols[index].symbol.name = "beacon\"雪\n".to_owned();
    facts.providers[0].context.index_revision =
        rift_core::IndexRevision::new(9_007_199_254_740_993)?;
    facts.providers[0].context.tree_revision = rift_core::TreeRevision::new(u64::MAX)?;
    let symbol = &facts.symbols[index];
    assert!(symbol.symbol.id.is_none());
    let payload = serde_json::to_vec(symbol)?;
    assert_eq!(
        super::ProviderIndex::payload_resource(symbol, &mut |_| Ok(()))?,
        super::super::blank(&payload)?,
    );
    let snapshot = &facts.providers[0];
    let record = &snapshot.records[0];
    let encoded = record.key(&snapshot.context)?;
    assert_eq!(
        super::ProviderIndex::record_resource(symbol, snapshot, record, &mut |_| Ok(()))?,
        super::super::blank(&encoded)?,
    );
    let mut admitted = Vec::new();
    let refused = super::ProviderIndex::payload_resource(symbol, &mut |bytes| {
        admitted.push(bytes);
        if bytes == 0 {
            Ok(())
        } else {
            Err(crate::sparql::failure::bound("material_bytes_max"))
        }
    });
    assert!(refused.is_err());
    assert_eq!(admitted, [0, size_of::<[u8; 32]>() * 2]);
    Ok(())
}

#[test]
fn admitted_symbol_projection_matches_full_provider_index_without_edge_maps()
-> Result<(), Box<dyn std::error::Error>> {
    let facts = physical_facts_with_source("int beacon(int value) { return value; }\n")?;
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let token = CancellationToken::new();
    let full = super::ProviderIndex::new(&facts, limits, &token)?;
    let mut charged = 0usize;
    let selected =
        super::ProviderIndex::symbol_resources_admitted(&facts, limits, &token, &mut |bytes| {
            charged = charged
                .checked_add(bytes)
                .ok_or_else(|| crate::sparql::failure::bound("material_bytes_max"))?;
            Ok(())
        })?;
    assert_eq!(selected, full.symbols);
    assert!(charged >= facts.symbols.len() * size_of::<Term>());
    let mut checks = 0usize;
    let refused =
        super::ProviderIndex::symbol_resources_admitted(&facts, limits, &token, &mut |bytes| {
            checks += 1;
            if bytes == 0 {
                Ok(())
            } else {
                Err(crate::sparql::failure::bound("material_bytes_max"))
            }
        });
    assert!(refused.is_err());
    assert!(checks > 1);
    let mut checks = 0usize;
    let cancelled =
        super::ProviderIndex::symbol_resources_admitted(&facts, limits, &token, &mut |_| {
            checks += 1;
            Err(crate::sparql::failure::cancelled())
        });
    assert!(cancelled.is_err());
    assert_eq!(checks, 1);
    Ok(())
}

#[test]
fn retained_subject_callback_observes_outer_cancellation_before_any_copy()
-> Result<(), Box<dyn std::error::Error>> {
    let selected = default_owner(physical_facts()?)?;
    let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
    let mut checks = 0usize;
    let refusal = crate::sparql::retained_subject_resources(
        &selected.facts,
        selected.identities,
        &[0],
        limits,
        &crate::sparql::QueryCancellation::default(),
        &mut |_| {
            checks += 1;
            Err(crate::sparql::failure::cancelled())
        },
    );
    let Err(error) = refusal else {
        return Err("outer cancellation must refuse before resource copies".into());
    };
    assert_eq!(error.slug(), crate::sparql::failure::cancelled().slug());
    assert_eq!(checks, 1);
    Ok(())
}
