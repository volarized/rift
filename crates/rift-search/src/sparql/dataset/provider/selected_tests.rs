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
    let directory = tempfile::tempdir()?;
    let text = "int beacon(int value) { return value; }\n";
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
    let resource = dataset.selected_resource(crate::sparql::LOCAL_GRAPH, &endpoint)?;
    assert!(matches!(resource, Term::BlankNode(_)));
    assert!(
        dataset
            .selected_resource("urn:rift:graph:foreign", &endpoint)
            .is_err()
    );
    assert!(
        dataset
            .selected_resource(
                crate::sparql::LOCAL_GRAPH,
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
                crate::sparql::LOCAL_GRAPH,
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
                crate::sparql::LOCAL_GRAPH,
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
        crate::sparql::LOCAL_GRAPH,
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
