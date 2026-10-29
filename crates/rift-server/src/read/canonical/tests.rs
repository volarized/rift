use super::*;
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;

fn fixture(
    metadata: bool,
) -> (
    tempfile::TempDir,
    Arc<ReadService>,
    rift_protocol::read::SymbolId,
) {
    let directory = tempfile::tempdir().expect("workspace");
    if metadata {
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"beacon","version":"1.0.0","main":"index.js","types":"index.d.ts"}"#,
        )
        .expect("captured metadata");
    }
    std::fs::write(
        directory.path().join("index.js"),
        "/** Implementation. */\nexport function open(path) {}\n",
    )
    .expect("implementation");
    std::fs::write(directory.path().join("index.d.ts"),
        "/** First overload. */\nexport declare function open(path: string): void;\n/** Second overload. */\nexport declare function open(path: number): void;\n")
        .expect("original overloads");
    let reads = Arc::new(
        ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("captured index"),
    );
    let id = if metadata {
        let matched = reads
            .index
            .symbols("open", 1)
            .expect("physical declaration")[0];
        rift_protocol::read::SymbolId(
            reads
                .index
                .assembled_symbol(matched)
                .expect("assembled object")
                .identity()
                .expect("canonical identity")
                .as_str()
                .to_owned(),
        )
    } else {
        rift_protocol::read::SymbolId::parse("rift://symbol/local/javascript/beacon/open")
            .expect("valid exact identity")
    };
    (directory, reads, id)
}

fn capture(store: &LocalSymbolViews, reads: &Arc<ReadService>, now: SystemTime) -> CapturedView {
    store
        .capture_at(captured_input(reads), now)
        .expect("capture succeeds")
        .expect("admitted material")
}

fn captured_input(reads: &ReadService) -> LocalSymbolViewInput {
    reads
        .local_search_view_input(true)
        .expect("measured publication")
}

#[test]
fn force_included_search_retains_original_source_after_mutation_and_removal() {
    let (directory, _reads, _) = fixture(true);
    let original = "pub fn phantom_signal() {}\n";
    std::fs::write(directory.path().join("hidden.rs"), original).expect("ignored source");
    std::fs::write(directory.path().join(".gitignore"), "hidden.rs\n").expect("ignored path");
    let reads = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
    .expect("base publication");
    let path = rift_core::ProjectPath::new("hidden.rs").expect("project path");
    assert!(reads.index.text_file(&path).is_none());
    let params = serde_json::from_value(serde_json::json!({
        "query": "phantom", "scope": "local", "target": "symbol",
        "paths": {"force_include": ["hidden.rs"]}
    }))
    .expect("forced search request");
    let (inputs, material) = reads
        .search_candidate_view_input(
            &params,
            &crate::StoreAnswer::identifier_only(),
            &crate::EngineReferences::default(),
            true,
        )
        .expect("same selected candidates and source");
    let row = inputs
        .precise
        .iter()
        .flat_map(|input| &input.order)
        .find(|row| {
            row.hit
                .path
                .as_ref()
                .is_some_and(|path| path.0 == "hidden.rs")
        })
        .expect("forced declaration row");
    assert_eq!(material.material.index.source(&path), Some(original));
    let refused = local_view_input(
        Arc::clone(&material.material.index),
        material.material.metadata.to_vec(),
        true,
    );
    assert!(
        LocalSymbolViews::new(0, [25; 32])
            .capture(refused)
            .expect("bounded admission")
            .is_none()
    );
    let store = LocalSymbolViews::new(1 << 20, [24; 32]);
    let view = store.capture(material).expect("admission").expect("view");
    let held_row_source = || {
        let held = store
            .state
            .lock()
            .expect("retained material state")
            .views
            .get(view.id.as_str())
            .expect("forced row view remains held")
            .clone();
        let range = row.hit.range.as_ref().expect("forced row range");
        let start = usize::try_from(range.start).expect("row start");
        let end = usize::try_from(range.end).expect("row end");
        held.material
            .index
            .source(&path)
            .and_then(|source| source.get(start..end))
            .map(str::to_owned)
    };
    assert_eq!(
        held_row_source().as_deref(),
        Some("pub fn phantom_signal() {}")
    );
    std::fs::write(
        directory.path().join("hidden.rs"),
        "pub fn replacement() {}\n",
    )
    .expect("current source mutation");
    assert_eq!(
        held_row_source().as_deref(),
        Some("pub fn phantom_signal() {}")
    );
    std::fs::remove_file(directory.path().join("hidden.rs")).expect("current source removal");
    assert_eq!(
        held_row_source().as_deref(),
        Some("pub fn phantom_signal() {}")
    );
}

#[test]
fn captured_source_selection_refuses_different_root_owner_and_changed_file_identity() {
    let (directory, reads, _) = fixture(true);
    let input = reads.local_search_view_input(true).expect("base material");
    let (_foreign_directory, foreign, _) = fixture(true);
    assert!(input.with_sources(&foreign.index).is_err());
    let named = rift_index::WorkspaceIndex::build_with_owner(
        (
            directory.path(),
            SymbolOwner::NamedLocal {
                name: "cloud".to_owned(),
            },
        ),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        &rift_core::LanguageFileSelections::default(),
        None,
        &|| false,
    )
    .expect("same root with a different accepted owner");
    assert!(
        reads
            .local_search_view_input(true)
            .expect("original material")
            .with_sources(&named)
            .is_err()
    );
    std::fs::write(
        directory.path().join("index.js"),
        "export function changed() {}\n",
    )
    .expect("current source mutation");
    let changed = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
    .expect("changed publication");
    assert!(
        reads
            .local_search_view_input(true)
            .expect("original material")
            .with_sources(&changed.index)
            .is_err()
    );
}

#[test]
fn local_view_admission_refuses_material_overflow_without_retaining_authority() {
    let (_directory, reads, _id) = fixture(false);
    let store = LocalSymbolViews::new(usize::MAX, [5; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let mut input = captured_input(&reads);
    input.retained_bytes = usize::MAX;
    assert!(
        store
            .capture_at(input, now)
            .expect("overflow refusal")
            .is_none()
    );
    assert!(store.state.lock().expect("store").views.is_empty());
}

#[test]
fn local_view_response_refusal_keeps_only_prior_authority() {
    let (_directory, reads, _) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [13; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let first = capture(&store, &reads, now);
    let first_charge = store.state.lock().expect("store").material_bytes();
    let refused = store
        .capture_with_at(captured_input(&reads), now, |_| Ok(false))
        .expect("response admission refusal");
    assert!(refused.is_none());
    let failed = store.capture_with_at(captured_input(&reads), now, |_| {
        Err(invalid("view", "response exceeds its byte bound"))
    });
    assert!(failed.is_err());
    let state = store.state.lock().expect("store");
    assert_eq!(state.views.len(), 1);
    assert!(state.views.contains_key(first.id.as_str()));
    assert_eq!(state.material_bytes(), first_charge);
}

#[test]
fn local_view_response_admission_runs_only_after_material_admission() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let tiny = LocalSymbolViews::new(1, [14; 32]);
    assert!(
        tiny.capture_with_at(captured_input(&reads), now, |_| {
            panic!("material refusal must precede response admission")
        })
        .expect("material refusal")
        .is_none()
    );
    let store = LocalSymbolViews::new(1 << 20, [15; 32]);
    let mut response_view = None;
    let captured = store
        .capture_with_at(captured_input(&reads), now, |view| {
            response_view = Some(view.clone());
            Ok(true)
        })
        .expect("response admission")
        .expect("captured view");
    assert_eq!(response_view, Some(captured.clone()));
    assert!(
        store
            .state
            .lock()
            .expect("store")
            .views
            .contains_key(captured.id.as_str())
    );
}

#[test]
fn captured_view_keeps_original_expiry_and_refuses_changed_context() {
    let (_directory, reads, _) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [10; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = capture(&store, &reads, now);
    for elapsed in [0, 1, 899] {
        assert_eq!(
            store.captured_view_at(
                &view.id,
                reads.index.root(),
                reads.index.owner(),
                now + Duration::from_secs(elapsed),
            ),
            Ok(view.clone()),
        );
    }
    let foreign = SymbolOwner::NamedLocal {
        name: "cloud".to_owned(),
    };
    assert_eq!(
        store.captured_view_at(&view.id, reads.index.root(), &foreign, now),
        Err(SymbolUnavailableReason::ViewContext),
    );
    let other_root = tempfile::tempdir().expect("foreign root");
    assert_eq!(
        store.captured_view_at(&view.id, other_root.path(), reads.index.owner(), now),
        Err(SymbolUnavailableReason::ViewContext),
    );
    assert_eq!(
        store.captured_view_at(
            &view.id,
            reads.index.root(),
            reads.index.owner(),
            now + VIEW_LIFETIME,
        ),
        Err(SymbolUnavailableReason::ViewExpired),
    );
    assert!(
        store
            .captured_view("bad", reads.index.root(), reads.index.owner())
            .is_err()
    );
}

#[test]
fn captured_view_refuses_unknown_keys_and_preparing_indices() {
    let (_directory, reads, _) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [11; 32]);
    let foreign_store = LocalSymbolViews::new(1 << 20, [12; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let other = capture(&foreign_store, &reads, now);
    assert_eq!(
        store.captured_view_at(&other.id, reads.index.root(), reads.index.owner(), now),
        Err(SymbolUnavailableReason::ViewContext),
    );
    let input = reads
        .local_search_view_input(false)
        .expect("captured preparing material");
    let view = store
        .capture_at(input, now)
        .expect("admission")
        .expect("view");
    assert_eq!(
        store.captured_view_at(&view.id, reads.index.root(), reads.index.owner(), now),
        Err(SymbolUnavailableReason::IndexPreparing),
    );
}

#[ignore = "manual retained material measurement"]
fn distinct_text_generations() -> (tempfile::TempDir, Vec<rift_index::WorkspaceIndex>, String) {
    let directory = tempfile::tempdir().expect("workspace");
    let original = "original captured text\n".repeat(8192);
    for path in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(directory.path().join(path), &original).expect("original source");
    }
    let mut preparation = rift_index::WorkspaceIndexPreparation::new(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &TextFileInclusion::default(),
        &rift_core::LanguageFileSelections::default(),
    )
    .expect("preparation");
    assert_eq!(
        preparation
            .discover(&SourceVisibility::default(), &|| false)
            .expect("discovery"),
        3
    );
    let first = preparation
        .advance_to(1, &|| false)
        .expect("first generation");
    let second = preparation
        .advance_to_with_previous(2, Some(&first), &|| false)
        .expect("second generation");
    let third = preparation
        .advance_to_with_previous(3, Some(&second), &|| false)
        .expect("third generation");
    (directory, vec![first, second, third], original)
}

fn text_generation_input(index: &rift_index::WorkspaceIndex) -> LocalSymbolViewInput {
    let metadata = serde_json::to_vec(&CapturedMetadata {
        revision: rift_protocol::revision::Revision::tree(&"0".repeat(64)).expect("tree revision"),
        warnings: Vec::new(),
    })
    .expect("captured metadata");
    local_view_input(
        index.symbol_view().expect("captured generation"),
        metadata,
        true,
    )
}

#[test]
fn distinct_generation_admission_counts_shared_allocations_and_keeps_original_source() {
    let (directory, indexes, original) = distinct_text_generations();
    let now = SystemTime::now();
    let measured = LocalSymbolViews::new(usize::MAX, [38; 32]);
    for index in &indexes[..2] {
        measured
            .capture_at(text_generation_input(index), now)
            .expect("capture")
            .expect("view");
    }
    let (cached, shared) = {
        let state = measured.state.lock().expect("state");
        (
            state.material_bytes(),
            state.material_bytes_with_shared_indexes(true),
        )
    };
    assert!(
        cached > shared + original.len(),
        "original source allocation is shared between distinct generations"
    );
    let limited = LocalSymbolViews::new(shared, [39; 32]);
    let first = limited
        .capture_at(text_generation_input(&indexes[0]), now)
        .expect("first capture")
        .expect("first view");
    let second = limited
        .capture_at(text_generation_input(&indexes[1]), now)
        .expect("second capture")
        .expect("shared generation admission");
    {
        let state = limited.state.lock().expect("state");
        assert!(
            state.material_bytes() > shared,
            "cached charges alone would refuse"
        );
        assert_eq!(state.admitted_material_bytes(shared), shared);
        assert!(state.views.contains_key(first.id.as_str()));
        assert!(state.views.contains_key(second.id.as_str()));
    }
    assert!(
        limited
            .capture_at(text_generation_input(&indexes[2]), now)
            .expect("actual byte bound")
            .is_none()
    );
    std::fs::write(directory.path().join("a.txt"), "changed current bytes\n")
        .expect("later source");
    let path = rift_core::ProjectPath::new("a.txt").expect("text path");
    let held = limited
        .state
        .lock()
        .expect("state")
        .views
        .get(first.id.as_str())
        .expect("previously advertised view remains held")
        .clone();
    assert_eq!(held.view, first);
    assert_eq!(held.material.index.source(&path), Some(original.as_str()));
    let state = limited.state.lock().expect("state");
    assert_eq!(
        state.views.len(),
        2,
        "refused third generation cannot replace either held view"
    );
    assert!(state.views.contains_key(second.id.as_str()));
    println!("cached_material_bytes={cached} shared_material_bytes={shared}");
}

#[test]
#[ignore = "retained material measurement"]
fn captured_preparation_views_measure_shared_accounting_cost() {
    let root = std::path::PathBuf::from(
        std::env::var_os("RIFT_RETAINED_MATERIAL_ROOT").expect("measurement root"),
    );
    let mut preparation = rift_index::WorkspaceIndexPreparation::new(
        &root,
        WorkspaceIndexLimits::default(),
        &TextFileInclusion::default(),
        &rift_core::LanguageFileSelections::default(),
    )
    .expect("workspace preparation");
    let total = preparation
        .discover(&SourceVisibility::default(), &|| false)
        .expect("selected files");
    assert!(total > 0, "measurement requires selected files");
    let mut reads = ReadService::from_prepared_index(
        preparation.empty_snapshot().expect("empty snapshot"),
        None,
        Arc::new(rift_dependency::DependencyContext::default()),
        HistoryConfiguration::default(),
        rift_protocol::dependencies::DependenciesConfiguration::default(),
    );
    let budget = usize::try_from(8_u64 << 30).expect("existing byte bound");
    let store = LocalSymbolViews::new(budget, [23; 32]);
    let now = SystemTime::now();
    let mut held_source = None;
    let mut prepared = 0;
    let mut target = 0;
    for _ in 0..64 {
        if target != 0 {
            let index = reads
                .advance_workspace_preparation(&mut preparation, target, &|| false)
                .expect("preparation snapshot");
            reads = ReadService::from_prepared_index(
                index,
                None,
                Arc::new(rift_dependency::DependencyContext::default()),
                HistoryConfiguration::default(),
                rift_protocol::dependencies::DependenciesConfiguration::default(),
            );
            prepared = target;
        }
        let input = reads
            .local_search_view_input(prepared == total)
            .expect("captured preparation material");
        if held_source.is_none()
            && let Some(file) = reads
                .index
                .files()
                .find(|file| !file.source().is_empty() && file.source().len() <= 65_536)
        {
            let original = input
                .material
                .index
                .source(file.path())
                .expect("original captured source")
                .to_owned();
            held_source = Some((
                file.path().clone(),
                original,
                Arc::clone(&input.material),
                None,
            ));
        }
        let started = std::time::Instant::now();
        let admitted = store
            .capture_at(input, now)
            .expect("material admission")
            .expect("original byte bound admits shared material");
        let capture_elapsed = started.elapsed();
        if let Some((path, original, material, view)) = &mut held_source
            && view.is_none()
        {
            assert_measured_held_source(&store, &reads, path, original, material, &admitted);
            *view = Some(admitted);
        }
        if measured_shared_accounting(&store, budget, prepared, total, capture_elapsed) {
            let (path, original, material, view) = held_source.expect("observed held source");
            assert_measured_held_source(
                &store,
                &reads,
                &path,
                &original,
                &material,
                &view.expect("advertised view"),
            );
            return;
        }
        assert!(
            prepared < total,
            "completed preparation did not reach finite cached overflow"
        );
        target = target.saturating_add(512).min(total);
    }
    panic!("measurement did not reach finite cached overflow within 64 captures");
}

fn measured_shared_accounting(
    store: &LocalSymbolViews,
    budget: usize,
    prepared: usize,
    total: usize,
    capture_elapsed: Duration,
) -> bool {
    let state = store.state.lock().expect("store");
    let cached = state.material_bytes();
    assert_ne!(cached, usize::MAX, "finite cached material measurement");
    if cached <= budget {
        return false;
    }
    let started = std::time::Instant::now();
    let shared = state.admitted_material_bytes(budget);
    let shared_elapsed = started.elapsed();
    assert!(
        shared <= budget,
        "shared material remains within original byte bound"
    );
    println!(
        "cached_material_bytes={cached} shared_material_bytes={shared} budget_bytes={budget} view_count={} prepared={prepared} total={total} capture_elapsed={capture_elapsed:?} shared_elapsed={shared_elapsed:?}",
        state.views.len(),
    );
    true
}

fn assert_measured_held_source(
    store: &LocalSymbolViews,
    reads: &ReadService,
    path: &rift_core::ProjectPath,
    original: &str,
    material: &Arc<LocalSymbolMaterial>,
    view: &CapturedView,
) {
    let held = store
        .state
        .lock()
        .expect("retained material state")
        .views
        .get(view.id.as_str())
        .expect("retained original material")
        .clone();
    assert_eq!(held.view, *view);
    assert!(held.expires > SystemTime::now());
    assert_eq!(held.material.index.root(), reads.index.root());
    assert_eq!(held.material.index.owner(), reads.index.owner());
    assert!(Arc::ptr_eq(&held.material, material));
    assert_eq!(
        held.material
            .index
            .source(path)
            .expect("held original source"),
        original
    );
}
