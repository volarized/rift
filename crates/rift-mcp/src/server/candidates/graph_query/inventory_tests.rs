use std::sync::Arc;

use rift_protocol::query::{QueryAnswer, QueryParams, QueryReadResult, RdfTerm};
use rift_protocol::read::ReadWarning;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::SymbolUnavailableReason;

use super::RiftMcp;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn edge_inventories_keep_identified_pairs_beside_unestablished_captures() -> TestResult {
    let (_directory, server) = fixture().await?;
    let published = Arc::clone(&server.published.read().await.current);
    assert_unestablished_capture(&server).await?;
    let caller = super::tests::static_endpoint(&server, "caller").await?;
    let target = super::tests::static_endpoint(&server, "target").await?;
    for query in [
        "SELECT ?a ?b { ?a :calls ?b }".to_owned(),
        "SELECT ?a ?b { ?a :imports ?b }".to_owned(),
        "SELECT ?a ?b { ?a :extends ?b }".to_owned(),
        "SELECT ?a ?b { ?a :reads|:writes ?b }".to_owned(),
        "SELECT ?a ?b { ?a :hasType ?b }".to_owned(),
        "SELECT ?a ?b { ?a :references ?b }".to_owned(),
        format!("ASK {{ <{caller}> :calls <{target}> }}"),
        format!("SELECT ?b {{ <{caller}> :calls ?b }}"),
        format!("SELECT ?a {{ ?a :references <{target}> }}"),
    ] {
        let result = server
            .graph_query(QueryParams {
                query: query.clone(),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("{error:?}"))?
            .0
            .result;
        let QueryReadResult::Results(answer) = result else {
            return Err(format!("{query}: {result:?}").into());
        };
        assert!(
            matches!(&answer.answer,
            QueryAnswer::Bindings { bindings, .. } if !bindings.is_empty())
                || matches!(&answer.answer, QueryAnswer::Boolean { value: true }),
            "{query}: {answer:?}"
        );
        assert!(
            answer.warnings.iter().any(|warning| matches!(warning,
            ReadWarning::EdgeCoverageMissing { detail, .. }
                if detail.contains("identities: 1;"))),
            "{query}: {:?}",
            answer.warnings
        );
    }
    let input = published.reads.local_search_view_input(true)?;
    assert!(!input.semantic_seeds(100)?.is_empty());
    assert!(input.semantic_seeds(1).is_err(), "seed bounds still apply");
    let (seeds, coverage) = input.inventory_seeds()?;
    assert!(!seeds.is_empty());
    assert_eq!(coverage, rift_server::InventoryCoverage::Complete);
    Ok(())
}

/// An inventory that left a capture out answers its rows, but an absence or an aggregate over
/// it is a claim the left-out capture may contradict, so those refuse.
#[tokio::test]
async fn edge_inventories_refuse_absences_and_aggregates_beside_unestablished_captures()
-> TestResult {
    let (_directory, server) = fixture().await?;
    assert_unestablished_capture(&server).await?;
    let caller = super::tests::static_endpoint(&server, "caller").await?;
    let target = super::tests::static_endpoint(&server, "target").await?;
    for query in [
        format!("ASK {{ <{target}> :calls <{caller}> }}"),
        "ASK { ?a :calls ?a }".to_owned(),
        format!("SELECT ?b {{ <{target}> :calls ?b }}"),
        "SELECT (COUNT(?b) AS ?n) { ?a :calls ?b }".to_owned(),
        "SELECT (COUNT(?b) AS ?n) { ?a :references ?b }".to_owned(),
        "SELECT ?a (COUNT(?b) AS ?n) { ?a :imports ?b } GROUP BY ?a".to_owned(),
        format!(
            "ASK {{ <{caller}> :name 'caller' . FILTER NOT EXISTS {{ <{caller}> :calls ?b }} }}"
        ),
        format!("SELECT ?a {{ ?a :calls <{target}> . FILTER NOT EXISTS {{ ?a :imports ?b }} }}"),
        format!("SELECT ?a ?b {{ ?a :calls <{target}> OPTIONAL {{ ?a :extends ?b }} }}"),
    ] {
        let result = server
            .graph_query(QueryParams {
                query: query.clone(),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("{error:?}"))?
            .0
            .result;
        let QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
            reason, warnings, ..
        }) = &result
        else {
            return Err(format!("{query}: {result:?}").into());
        };
        assert_eq!(
            *reason,
            SymbolUnavailableReason::InsufficientCoverage,
            "{query}"
        );
        assert!(
            warnings.iter().any(|warning| matches!(warning,
            ReadWarning::EdgeCoverageMissing { detail, .. }
                if detail.contains("identities: 1;"))),
            "{query}: {warnings:?}"
        );
    }
    Ok(())
}

async fn assert_unestablished_capture(server: &RiftMcp) -> TestResult {
    let broken = server
        .graph_query(QueryParams {
            query: "SELECT ?s { ?s a :Symbol; :name 'Broken' }".to_owned(),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("{error:?}"))?
        .0
        .result;
    assert!(
        matches!(&broken, QueryReadResult::Results(answer)
        if matches!(&answer.answer, QueryAnswer::Bindings { bindings, .. }
            if bindings.len() == 1 && matches!(bindings[0].get("s"), Some(RdfTerm::BlankNode { .. })))),
        "the unreadable declaration has no identity: {broken:?}"
    );
    Ok(())
}

async fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
    let directory = tempfile::tempdir()?;
    crate::server::hermetic_workspace(directory.path(), "")?;
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"name":"beacon"}"#,
    )?;
    std::fs::write(
        directory.path().join("broken.ts"),
        // A declaration whose own value is unreadable keeps no identity.
        "export const Broken = { value: (globalThis as any)?.value ?? }\n",
    )?;
    std::fs::write(
        directory.path().join("helper.ts"),
        "export class Base { value: number = 1; }\nexport function target() {}\n",
    )?;
    std::fs::write(
        directory.path().join("index.ts"),
        "import { Base, target } from './helper';\n\
         export class Child extends Base {}\n\
         export function caller(input: Base) { target(); return input.value; }\n",
    )?;
    let root = directory.path().canonicalize()?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    Ok((directory, server))
}
