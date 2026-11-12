//! Checkpoint and source admission controls.

#![cfg(feature = "runtime")]

use rift_collect::{
    Batch, ByteRange, Category, CollectionCache, CollectionInput, CollectionSource,
    CollectionStats, Collector, Coverage, Error, Evidence, Inventory, Observation, Producer,
    SourceAnchor,
};
use rift_core::{FileDigest, ProviderId, SourceUnitId};
use tempfile::tempdir;

fn unit(path: &str) -> SourceUnitId {
    SourceUnitId::parse(&format!("rift://source/project/{path}")).expect("fixture unit")
}

fn producer(revision: &[u8], context: &[u8]) -> Producer {
    Producer::new(
        ProviderId::new("fixture.collector").expect("fixture provider"),
        FileDigest::of(revision),
        FileDigest::of(context),
    )
}

fn inventory(dependency: Option<&str>, filename: &str) -> Inventory {
    let mut inventory = Inventory::default();
    let mut add = |path: &str, filename: &str, text: &str| {
        inventory
            .insert(
                CollectionSource::new(unit(path), filename.into(), text.into()).expect("source"),
            )
            .expect("distinct source");
    };
    add("entry.py", filename, "é; f(); f()");
    add("other.py", "other.py", "g()");
    if let Some(text) = dependency {
        add("dependency.py", "dependency.py", text);
    }
    inventory
}

#[derive(Default)]
enum OtherSource {
    #[default]
    Complete,
    Partial,
    Interrupted,
}

#[derive(Default)]
struct Fixture {
    requests: Vec<SourceUnitId>,
    other: OtherSource,
    invalid: bool,
    stale_target: bool,
}

impl Collector for Fixture {
    type Error = Error;

    fn collect(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> impl std::future::Future<Output = Result<Batch, Error>> {
        std::future::ready(self.batch(input, category))
    }
}

impl Fixture {
    fn batch(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> Result<Batch, Error> {
        let source = input.selected();
        self.requests.push(source.unit().clone());
        if category != Category::Calls {
            return Ok(Batch {
                coverage: Coverage::Unsupported,
                observations: vec![],
            });
        }
        if source.unit() == &unit("other.py") {
            let coverage = match self.other {
                OtherSource::Complete => Coverage::Complete,
                OtherSource::Partial => Coverage::Partial,
                OtherSource::Interrupted => return Err(Error::Deadline),
            };
            return Ok(Batch {
                coverage,
                observations: vec![],
            });
        }
        let target = input
            .read(&unit("dependency.py"))?
            .map(|dependency| SourceAnchor {
                unit: dependency.unit().to_string(),
                source_sha256: if self.stale_target {
                    "0".repeat(64)
                } else {
                    dependency.sha256().into()
                },
                range: ByteRange {
                    start: 0,
                    end: dependency.text().len() as u64,
                },
            });
        let observations = [4, 9]
            .into_iter()
            .enumerate()
            .map(|(id, start)| Observation {
                id: id.to_string(),
                occurrence: ByteRange {
                    start: if self.invalid { 1 } else { start },
                    end: start + 3,
                },
                enclosing: None,
                evidence: if target.is_some() {
                    Evidence::Candidate
                } else {
                    Evidence::Unresolved
                },
                target: target.clone(),
            })
            .collect();
        Ok(Batch {
            coverage: Coverage::Complete,
            observations,
        })
    }
}

async fn collect(
    cache: &CollectionCache,
    producer: &Producer,
    collector: &mut Fixture,
    inventory: &Inventory,
    selection: &[SourceUnitId],
    category: Category,
) -> Result<(CollectionStats, Vec<Batch>), Error> {
    let mut result = Vec::new();
    let stats = cache
        .collect(
            producer,
            collector,
            inventory,
            selection,
            category,
            |_, batch| {
                result.push(batch.clone());
                Ok(())
            },
        )
        .await?;
    Ok((stats, result))
}

#[tokio::test]
async fn restart_reuses_exact_batches_and_recollects_changed_or_missing_dependencies() {
    let directory = tempdir().expect("fixture directory");
    let path = directory.path().join("facts.sqlite");
    let selected = [unit("entry.py"), unit("other.py")];
    let first_inventory = inventory(Some("def f(): pass"), "entry.py");
    let id = producer(b"one", b"context");
    let mut worker = Fixture::default();
    let cache = CollectionCache::open(&path).expect("open");
    let (cold, held) = collect(
        &cache,
        &id,
        &mut worker,
        &first_inventory,
        &selected,
        Category::Calls,
    )
    .await
    .expect("cold");
    assert_eq!(cold.collected, 2);
    assert_eq!(held[0].observations.len(), 2);
    assert_ne!(
        held[0].observations[0].occurrence,
        held[0].observations[1].occurrence
    );
    drop(cache);

    let cache = CollectionCache::open(&path).expect("restart");
    worker.requests.clear();
    let (warm, reused) = collect(
        &cache,
        &id,
        &mut worker,
        &first_inventory,
        &selected,
        Category::Calls,
    )
    .await
    .expect("warm");
    assert_eq!(warm.reused, 2);
    assert!(worker.requests.is_empty());
    assert_eq!(held, reused);

    let changed = inventory(Some("def f(): return 1"), "entry.py");
    let (delta, after_edit) = collect(
        &cache,
        &id,
        &mut worker,
        &changed,
        &selected,
        Category::Calls,
    )
    .await
    .expect("edit");
    assert_eq!((delta.reused, delta.collected), (1, 1));
    assert_eq!(worker.requests, [unit("entry.py")]);
    assert_ne!(held[0], after_edit[0]);
    let independent = CollectionCache::open(std::path::Path::new(":memory:")).expect("fresh cache");
    let (_, cold_changed) = collect(
        &independent,
        &id,
        &mut Fixture::default(),
        &changed,
        &selected,
        Category::Calls,
    )
    .await
    .expect("independent cold edit");
    assert_eq!(after_edit, cold_changed);

    worker.requests.clear();
    let removed = inventory(None, "entry.py");
    let (_, after_delete) = collect(
        &cache,
        &id,
        &mut worker,
        &removed,
        &selected[..1],
        Category::Calls,
    )
    .await
    .expect("delete");
    assert_eq!(after_delete.len(), 1);
    assert_eq!(
        after_delete[0].observations[0].evidence,
        Evidence::Unresolved
    );
    assert_eq!(worker.requests, [unit("entry.py")]);

    worker.requests.clear();
    let (restored, restored_output) = collect(
        &cache,
        &id,
        &mut worker,
        &first_inventory,
        &selected,
        Category::Calls,
    )
    .await
    .expect("original selection");
    assert_eq!(restored.reused, 2);
    assert!(worker.requests.is_empty());
    assert_eq!(held, restored_output);
}

#[tokio::test]
async fn adding_a_previously_absent_unit_invalidates_its_dependent() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py"), unit("other.py")];
    let id = producer(b"one", b"context");
    let mut worker = Fixture::default();
    let missing = inventory(None, "entry.py");
    collect(
        &cache,
        &id,
        &mut worker,
        &missing,
        &selected,
        Category::Calls,
    )
    .await
    .expect("missing");
    worker.requests.clear();
    let added = inventory(Some("new dependency"), "entry.py");
    let (stats, output) = collect(&cache, &id, &mut worker, &added, &selected, Category::Calls)
        .await
        .expect("added");
    assert_eq!((stats.reused, stats.collected), (1, 1));
    assert_eq!(worker.requests, [unit("entry.py")]);
    assert_eq!(output[0].observations[0].evidence, Evidence::Candidate);
}

#[tokio::test]
async fn interruption_and_partial_coverage_do_not_mark_work_complete() {
    let directory = tempdir().expect("fixture directory");
    let path = directory.path().join("facts.sqlite");
    let selected = [unit("entry.py"), unit("other.py")];
    let inputs = inventory(None, "entry.py");
    let id = producer(b"one", b"context");
    let mut worker = Fixture {
        other: OtherSource::Interrupted,
        ..Fixture::default()
    };
    let cache = CollectionCache::open(&path).expect("open");
    assert!(matches!(
        collect(
            &cache,
            &id,
            &mut worker,
            &inputs,
            &selected,
            Category::Calls
        )
        .await,
        Err(Error::Deadline)
    ));
    drop(cache);

    let cache = CollectionCache::open(&path).expect("restart");
    worker = Fixture {
        other: OtherSource::Partial,
        ..Fixture::default()
    };
    let (partial, output) = collect(
        &cache,
        &id,
        &mut worker,
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("partial");
    assert_eq!(
        (partial.reused, partial.collected, partial.partial),
        (1, 0, 1)
    );
    assert_eq!(output[1].coverage, Coverage::Partial);
    worker = Fixture::default();
    let (resumed, _) = collect(
        &cache,
        &id,
        &mut worker,
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("resume");
    assert_eq!(
        (resumed.reused, resumed.collected, resumed.partial),
        (1, 1, 0)
    );
    assert_eq!(worker.requests, [unit("other.py")]);
}

#[tokio::test]
async fn category_context_and_parser_filename_are_independent_inputs() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py")];
    let inputs = inventory(None, "entry.py");
    let id = producer(b"one", b"context");
    let mut worker = Fixture::default();
    collect(
        &cache,
        &id,
        &mut worker,
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("calls");
    let (different, batch) = collect(
        &cache,
        &id,
        &mut worker,
        &inputs,
        &selected,
        Category::Imports,
    )
    .await
    .expect("imports");
    assert_eq!(different.collected, 1);
    assert_eq!(batch[0].coverage, Coverage::Unsupported);
    for changed in [
        producer(b"two", b"context"),
        producer(b"one", b"another context"),
    ] {
        let (stats, _) = collect(
            &cache,
            &changed,
            &mut worker,
            &inputs,
            &selected,
            Category::Calls,
        )
        .await
        .expect("changed context");
        assert_eq!(stats.collected, 1);
    }
    let renamed = inventory(None, "alias.py");
    let (stats, _) = collect(
        &cache,
        &id,
        &mut worker,
        &renamed,
        &selected,
        Category::Calls,
    )
    .await
    .expect("renamed parser input");
    assert_eq!(stats.collected, 1);
}

#[tokio::test]
async fn refuses_a_split_utf8_boundary_before_checkpointing() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py")];
    let inputs = inventory(None, "entry.py");
    let id = producer(b"one", b"context");
    let mut invalid = Fixture {
        invalid: true,
        ..Fixture::default()
    };
    assert!(matches!(
        collect(
            &cache,
            &id,
            &mut invalid,
            &inputs,
            &selected,
            Category::Calls
        )
        .await,
        Err(Error::Admission(_))
    ));
    let (stats, _) = collect(
        &cache,
        &id,
        &mut Fixture::default(),
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("valid producer");
    assert_eq!(stats.collected, 1);
}

#[tokio::test]
async fn a_stale_target_digest_cannot_checkpoint_a_plausible_range() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py")];
    let inputs = inventory(Some("def f(): pass"), "entry.py");
    let id = producer(b"one", b"context");
    let mut invalid = Fixture {
        stale_target: true,
        ..Fixture::default()
    };
    let error = collect(
        &cache,
        &id,
        &mut invalid,
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect_err("stale target must be refused");
    assert!(error.to_string().contains("target digest does not match"));
    let (stats, _) = collect(
        &cache,
        &id,
        &mut Fixture::default(),
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("valid producer");
    assert_eq!(stats.collected, 1);
}

#[tokio::test]
async fn recollecting_an_old_dependency_promotes_it_into_the_reuse_window() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py")];
    let id = producer(b"one", b"context");
    let mut worker = Fixture::default();
    for index in 0..=16 {
        let dependency = format!("def f(): return {index}");
        let inputs = inventory(Some(&dependency), "entry.py");
        let (stats, _) = collect(
            &cache,
            &id,
            &mut worker,
            &inputs,
            &selected,
            Category::Calls,
        )
        .await
        .expect("new dependency");
        assert_eq!(stats.collected, 1);
    }
    let original = inventory(Some("def f(): return 0"), "entry.py");
    let (stats, _) = collect(
        &cache,
        &id,
        &mut worker,
        &original,
        &selected,
        Category::Calls,
    )
    .await
    .expect("recollect original");
    assert_eq!(stats.collected, 1);
    let (stats, _) = collect(
        &cache,
        &id,
        &mut worker,
        &original,
        &selected,
        Category::Calls,
    )
    .await
    .expect("reuse original");
    assert_eq!(stats.reused, 1);
}

#[test]
fn inventory_rejects_duplicate_and_oversized_sources() {
    let id = unit("entry.py");
    let mut sources = Inventory::default();
    let source = CollectionSource::new(id.clone(), "entry.py".into(), "f()".into()).expect("admit");
    assert_eq!(source.digest(), FileDigest::of(b"f()"));
    sources.insert(source).expect("insert");
    assert_eq!(sources.units().collect::<Vec<_>>(), [&id]);

    let duplicate =
        CollectionSource::new(id.clone(), "other.py".into(), "g()".into()).expect("admit");
    assert!(sources.insert(duplicate).is_err());
    assert!(CollectionSource::new(id.clone(), String::new(), "f()".into()).is_err());
    let too_long = " ".repeat(rift_collect::SOURCE_BYTES_MAX + 1);
    assert!(CollectionSource::new(id, "entry.py".into(), too_long.into()).is_err());
    assert!(CollectionInput::new(&sources, &unit("other.py")).is_err());
}

#[test]
fn evidence_and_coverage_must_match_admitted_occurrences() {
    let sources = inventory(None, "entry.py");
    let mut input = CollectionInput::new(&sources, &unit("entry.py")).expect("selection");
    let occurrence = ByteRange { start: 4, end: 7 };
    let syntax = Observation {
        id: "authored".into(),
        occurrence,
        enclosing: None,
        evidence: Evidence::Syntax,
        target: None,
    };
    let invalid = [
        (Coverage::Unsupported, vec![syntax.clone()]),
        (Coverage::Complete, vec![syntax.clone(); 2]),
        (
            Coverage::Complete,
            vec![syntax.clone(); rift_collect::OBSERVATIONS_MAX + 1],
        ),
        (
            Coverage::Complete,
            vec![Observation {
                enclosing: Some(ByteRange { start: 5, end: 7 }),
                ..syntax.clone()
            }],
        ),
        (
            Coverage::Complete,
            vec![Observation {
                evidence: Evidence::Resolution,
                ..syntax.clone()
            }],
        ),
        (
            Coverage::Complete,
            vec![Observation {
                evidence: Evidence::Candidate,
                target: Some(SourceAnchor {
                    unit: "rift://source/project/unknown.py".into(),
                    source_sha256: "0".repeat(64),
                    range: occurrence,
                }),
                ..syntax
            }],
        ),
    ];
    for (coverage, observations) in invalid {
        assert!(
            input
                .validate(&Batch {
                    coverage,
                    observations
                })
                .is_err()
        );
    }
}

#[tokio::test]
async fn invalid_selection_runs_no_collector() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let inputs = inventory(None, "entry.py");
    let id = producer(b"one", b"context");
    let mut worker = Fixture::default();
    let duplicate = [unit("entry.py"), unit("entry.py")];
    let missing = [unit("entry.py"), unit("missing.py")];
    for selection in [duplicate, missing] {
        assert!(
            collect(
                &cache,
                &id,
                &mut worker,
                &inputs,
                &selection,
                Category::Calls
            )
            .await
            .is_err()
        );
    }
    assert!(worker.requests.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_host_deadline_leaves_the_batch_owed() {
    let cache = CollectionCache::open(std::path::Path::new(":memory:")).expect("open");
    let selected = [unit("entry.py")];
    let inputs = inventory(None, "entry.py");
    let id = producer(b"one", b"context");
    let mut worker = rift_collect::StdioCollector::new(
        "sh".into(),
        vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
        std::time::Duration::from_millis(50),
    );
    let result = cache
        .collect(
            &id,
            &mut worker,
            &inputs,
            &selected,
            Category::Calls,
            |_, _| Ok(()),
        )
        .await;
    assert!(matches!(result, Err(Error::Deadline)), "{result:?}");
    worker.close().await.expect("failed child has closed");
    let (stats, _) = collect(
        &cache,
        &id,
        &mut Fixture::default(),
        &inputs,
        &selected,
        Category::Calls,
    )
    .await
    .expect("resume");
    assert_eq!(stats.collected, 1);
}
