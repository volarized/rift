use super::*;
use crate::query::{QueryLimits, QueryNodeSource};
use crate::sparql::{PreparedQuery, QueryCancellation};

fn limits() -> QueryLimits {
    QueryLimits {
        query_bytes_max: 16_384,
        algebra_nodes_max: 4096,
        quads_max: 10_000,
        rdf_bytes_max: 1_048_576,
        result_items_max: 1000,
        result_bytes_max: 1_048_576,
        input_bytes_max: None,
        output_bytes_max: None,
        material_bytes_max: None,
        heap_bytes_max: None,
        diagnostic_bytes_max: None,
    }
}

fn facts() -> QueryFacts {
    let source = "fn one() {}";
    QueryFacts {
        sources: vec![crate::query::QuerySource {
            unit: rift_protocol::read::SourceUnitId("rift://source/project/lib.rs".into()),
            digest: Some(
                rift_protocol::identity::SourceDigest::parse(
                    &data_encoding::HEXLOWER.encode(&Sha256::digest(source.as_bytes())),
                )
                .expect("full captured source digest"),
            ),
            text: Some(source.into()),
        }],
        nodes: vec![crate::query::test_node_source()],
        ..QueryFacts::default()
    }
}

#[test]
fn same_address_nodes_preserve_original_parent_chain_regions_and_source() {
    let statement = "ASK { ?root a :Node; :kind \"source_file\"; :id \"rift://node/rust/lib.rs@0-11#1c281acb\" . ?function a :Node; :kind \"function_item\"; :parent ?root; :id \"rift://node/rust/lib.rs@0-11#1c281acb\"; :regions ?region . ?region :role \"name\"; :range [ :start 3; :end 6 ] . ?name a :Node; :kind \"identifier\"; :parent ?function; :unit <rift://source/project/lib.rs> . FILTER(?root != ?function && ?function != ?name) }";
    let prepared = PreparedQuery::parse(statement, limits()).expect("ordinary query");
    assert_eq!(
        prepared
            .evaluate(facts(), limits(), QueryCancellation::default())
            .expect("complete original tree"),
        rift_protocol::query::QueryAnswer::Boolean { value: true }
    );
}

#[test]
fn captured_node_projection_is_deterministic_and_refuses_missing_or_invalid_source() {
    let prepared =
        PreparedQuery::parse("CONSTRUCT { ?s ?p ?o } { ?s ?p ?o }", limits()).expect("query");
    let first = prepared
        .evaluate(facts(), limits(), QueryCancellation::default())
        .expect("captured graph");
    let second = prepared
        .evaluate(facts(), limits(), QueryCancellation::default())
        .expect("same graph");
    let graph = |answer| {
        let rift_protocol::query::QueryAnswer::Graph { triples } = answer else {
            panic!("CONSTRUCT returns a graph");
        };
        triples
            .iter()
            .map(|triple| serde_json::to_string(triple).expect("RDF triple"))
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(graph(first), graph(second));
    for change in 0..3 {
        let mut changed = facts();
        match change {
            0 => changed.sources[0].digest = None,
            1 => changed.sources[0].text = Some("changed".into()),
            _ => {
                changed.sources[0].text = Some("fn ée() {}".into());
                changed.sources[0].digest = Some(
                    rift_protocol::identity::SourceDigest::parse(
                        &data_encoding::HEXLOWER.encode(&Sha256::digest("fn ée() {}".as_bytes())),
                    )
                    .expect("captured digest"),
                );
                changed.nodes[0].nodes[2].node.range.end = 4;
            }
        }
        assert!(
            prepared
                .evaluate(changed, limits(), QueryCancellation::default())
                .is_err()
        );
    }
    let mut limited = limits();
    limited.quads_max = 2;
    assert!(
        prepared
            .evaluate(facts(), limited, QueryCancellation::default())
            .is_err()
    );
    let malformed: Result<QueryNodeSource, _> =
        serde_json::from_value(serde_json::json!({"unit":"rift://source/project/lib.rs"}));
    assert!(malformed.is_err());
}
