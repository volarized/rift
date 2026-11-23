use super::*;
use rift_protocol::read::{SourceUnitId, SourceUnitSpan, TextRange};

#[test]
fn only_readiness_refusals_skip_live_inventory_validation() -> Result<(), RiftError> {
    let analyzing = || {
        Err(rift_error::errors::lsp::engine_analyzing()
            .attempts(1_u64)
            .error())
    };
    let successful_empty = || {
        Ok(Some(rift_server::EngineInventory {
            calls: Vec::new(),
            references: Vec::new(),
            types: Vec::new(),
            heritage: Vec::new(),
            warnings: Vec::new(),
            packages: Vec::new(),
        }))
    };
    let observe = |answers: Vec<_>| -> Result<_, RiftError> {
        let mut read = InventoryRead::default();
        let mut coverage = EngineCoverage::default();
        for answer in answers {
            coverage.admit(&mut read, answer)?;
        }
        let captured_only = coverage.finish(&mut read);
        Ok((read, captured_only))
    };
    let (captured, captured_only) = observe(vec![Ok(None), analyzing(), analyzing(), Ok(None)])?;
    assert!(captured_only);
    assert!(captured.partial);
    assert!(matches!(
        captured.warnings.as_slice(),
        [ReadWarning::EngineAnalysisUnavailable { .. }]
    ));
    for other in [
        vec![],
        vec![Ok(None), Ok(None)],
        vec![successful_empty()],
        vec![analyzing(), successful_empty()],
        vec![successful_empty(), Ok(None), analyzing()],
        vec![analyzing(), Err(coverage_failure())],
    ] {
        assert!(!observe(other)?.1);
    }
    Ok(())
}

#[tokio::test]
async fn a_seeded_readiness_only_walk_keeps_its_accepted_capture()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use rift_search::sparql::GraphQuery;

    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='seeded_inventory'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
    )?;
    std::fs::write(
        root.join("lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\n",
    )?;
    crate::server::hermetic_workspace(
        root,
        "[global]\nenabled=false\n[languages.rust.lsp]\ncommand=['unlaunched-test-engine']\n",
    )?;
    let server = RiftMcp::build_settled(root, rift_index::WorkspaceIndexLimits::default()).await?;
    let published = Arc::clone(&server.published.read().await.current);
    let seed = published
        .reads
        .get_symbol(&serde_json::from_value(
            serde_json::json!({"name":"caller","scope":"local"}),
        )?)?
        .hits
        .into_iter()
        .find(|hit| hit.symbol.name == "caller")
        .and_then(|hit| hit.symbol.id)
        .ok_or("captured caller")?;
    // The graph reserve consumes the whole engine admission window, independently of the
    // test host's scheduling. Request-time publication acceptance has already run.
    let budget = Duration::from_secs(1);
    let deadline = RequestDeadline::starting(budget);
    let resolved = ResolvedWorkspace::current(published, budget);
    let limits = GraphLimits::default();
    let captures = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&captures);
    server.force_capture(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        Err(coverage_failure())
    });

    let query = GraphQuery::parse(
        &format!("SELECT ?t {{ <{}> :calls ?t }}", seed.as_str()),
        &limits,
    )?;
    let selected = query.adjacency_selection().ok_or("seeded calls")?;
    let acquired = server
        .acquire_adjacency(&resolved, &selected, limits, deadline)
        .await?
        .map_err(|failure| format!("{failure:?}"))?;
    assert!(
        acquired
            .local_edges
            .iter()
            .any(|(from, kind, _)| { from == &seed && *kind == SymbolRelation::Calls })
    );
    assert!(acquired.warnings.iter().any(|warning| matches!(
        warning, ReadWarning::EngineAnalysisUnavailable { detail, .. }
            if detail.contains("readiness_timeout")
    )));
    assert_eq!(captures.load(Ordering::Relaxed), 0);

    let query = GraphQuery::parse(
        &format!("SELECT ?t {{ <{}> :calls/:reads ?t }}", seed.as_str()),
        &limits,
    )?;
    let selected = query.adjacency_selection().ok_or("mixed local walk")?;
    let result = server
        .acquire_adjacency(&resolved, &selected, limits, deadline)
        .await;
    assert_eq!(
        result.err().as_ref().map(RiftError::slug),
        Some(coverage_failure().slug())
    );
    assert_eq!(captures.load(Ordering::Relaxed), 1);
    Ok(())
}

fn references() -> (SymbolId, Vec<QueryOccurrence>) {
    let target = SymbolId("rift://symbol/local/rust/test/target".to_owned());
    let caller = SymbolId("rift://symbol/local/rust/test/caller".to_owned());
    let occurrences = [10, 30]
        .into_iter()
        .map(|start| QueryOccurrence {
            target: target.clone(),
            enclosing: Some(caller.clone()),
            source: SourceUnitSpan {
                unit: SourceUnitId("rift://source/project/lib.rs".to_owned()),
                range: TextRange {
                    start,
                    end: start + 6,
                },
            },
            kind: EdgeKind::References,
        })
        .collect();
    (target, occurrences)
}

#[test]
fn reference_pairs_charge_each_caller_once() {
    let (target, occurrences) = references();
    let mut read = InventoryRead {
        records: GraphLimits::default().triples_max() - 1,
        ..InventoryRead::default()
    };
    read.admit_references(&target, occurrences)
        .expect("two occurrences of one pair fit one remaining record");
    assert_eq!(read.local_edges.len(), 1);
    assert_eq!(read.local.len(), 1);
    assert_eq!(read.records, GraphLimits::default().triples_max());
}

#[test]
fn occurrence_reads_keep_each_reference_range() {
    let (target, occurrences) = references();
    let mut read = InventoryRead::default();
    read.admit_occurrences(&target, occurrences)
        .expect("physical references");
    let starts: Vec<_> = read
        .local
        .iter()
        .map(|occurrence| occurrence.source.range.start)
        .collect();
    assert_eq!(starts, vec![10, 30]);
    assert_eq!(read.local_edges.len(), 1);
    assert_eq!(read.records, 2);
}

#[cfg(unix)]
#[tokio::test]
async fn package_calls_spend_inventory_records_before_naming()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::server::candidates::graph_query::tests::dependency_inventory::recorded_rust_workspace;
    let directory = tempfile::tempdir()?;
    let cache = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    recorded_rust_workspace(&root, cache.path(), false)?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    let deadline = server.request_deadline().await;
    let resolved = server
        .published_workspace(crate::server::wire::ErrorPhase::Read, deadline)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let engines = server.engine_pool_for(&resolved.published).await;
    let reads = &resolved.published.reads;
    let roots = rift_server::CalleeRoots::read(reads, &engines);
    let seed = SymbolId("rift://symbol/local/rust/fixture/encode".to_owned());
    let mut calls = Box::pin(rift_server::graph_calls(
        reads,
        &engines,
        &roots,
        &seed,
        TraversalDirection::Outgoing,
        deadline.at(),
    ))
    .await?;
    assert_eq!(calls.packages.len(), 1);
    assert_eq!(calls.packages[0].1.len(), 1);
    calls.local.clear();
    calls.edges.clear();
    let maximum = GraphLimits::default().triples_max();
    let mut read = InventoryRead {
        records: maximum - 2,
        ..Default::default()
    };
    read.admit_calls(&seed, calls.clone(), true)?;
    assert_eq!(read.records, maximum);
    assert!(read.admit_calls(&seed, calls.clone(), false).is_err());
    assert_eq!(read.records, maximum);
    let (callee, occurrences) = calls.packages.pop().ok_or("package target")?;
    calls.name_package_call(
        &callee,
        SymbolId(
            "rift://symbol/cargo/crates.io/serde_json/rust/serde_json/ser/to_string".to_owned(),
        ),
        occurrences,
    );
    assert_eq!(
        calls.record_count(),
        2,
        "naming keeps the same target and occurrence charge"
    );
    engines.shutdown().await;
    Ok(())
}

#[test]
fn repeated_reference_provider_records_cannot_exceed_the_inventory_bound() {
    use rift_protocol::index::StoredEdge;
    use rift_protocol::read::{EdgeDerivation, EdgeProvider, ExactKind, Extensions};
    let (target, occurrences) = references();
    let edges = occurrences
        .iter()
        .map(|occurrence| StoredEdge {
            from: occurrence.enclosing.clone().expect("caller"),
            to: target.clone(),
            kind: ExactKind("references".to_owned()),
            kinds: vec![EdgeKind::References],
            occurrence: Some(occurrence.source.clone()),
            provider: EdgeProvider::Lsp,
            derivation: EdgeDerivation::Resolution,
            confidence: None,
            producer: None,
            extensions: Extensions::default(),
        })
        .collect();
    let mut read = InventoryRead {
        records: GraphLimits::default().triples_max() - 1,
        ..InventoryRead::default()
    };
    let answer = rift_server::EngineInventory {
        calls: Vec::new(),
        references: vec![(
            target,
            rift_server::GraphReferences {
                occurrences,
                edges,
                complete: true,
                edge_warnings: Vec::new(),
            },
        )],
        types: Vec::new(),
        heritage: Vec::new(),
        warnings: Vec::new(),
        packages: Vec::new(),
    };
    let result = EngineCoverage::default().admit(&mut read, Ok(Some(answer)));
    assert!(
        result.is_err(),
        "two retained provider records cannot fit one remaining record"
    );
    assert!(
        read.edge_facts.is_empty(),
        "reject before retaining the over-budget records"
    );
}
