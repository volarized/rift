use super::super::VirtualDataset;
use crate::query::{
    QueryFacts, QueryLimits, QueryProviderContext, QueryProviderDeclaration, QueryProviderEndpoint,
    QueryProviderRecord, QueryProviderSelection, QueryProviderSnapshot, QuerySource, QuerySymbol,
};
use oxigraph::model::Term;
use oxigraph::sparql::CancellationToken;
use rift_core::{SourceVisibility, TextFileInclusion, WorkspaceId};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::identity::SourceDigest;
use rift_protocol::read::SourceUnitId;
use rift_provider::NormalizedTarget;

fn physical_facts() -> Result<QueryFacts, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let text = "int beacon(int value) { return value; }\n";
    std::fs::write(directory.path().join("beacon.c"), text)?;
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
            fingerprint: digest.clone(),
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
                    selection,
                    source_digest: digest,
                    inputs: Vec::new(),
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
