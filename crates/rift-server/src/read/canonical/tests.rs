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

// `QUERY_RETAINED_VIEWS_DEFAULT`; `retained_view_limits_read_the_accepted_configuration` pins it.
const VIEWS_DEFAULT: usize = 256;

/// A store paired with the bounds every capture in one test states.
struct BoundedViews {
    store: LocalSymbolViews,
    limits: RetainedViewLimits,
}

impl std::ops::Deref for BoundedViews {
    type Target = LocalSymbolViews;

    fn deref(&self) -> &LocalSymbolViews {
        &self.store
    }
}

impl BoundedViews {
    fn new(bytes_max: usize, service_key: [u8; 32]) -> Self {
        Self::with_limits(
            RetainedViewLimits::new(VIEWS_DEFAULT, bytes_max),
            service_key,
        )
    }

    fn with_limits(limits: RetainedViewLimits, service_key: [u8; 32]) -> Self {
        Self {
            store: LocalSymbolViews::new(service_key),
            limits,
        }
    }

    fn capture(&self, input: LocalSymbolViewInput) -> Result<Option<CapturedView>, RiftError> {
        self.store.capture(input, self.limits)
    }

    fn capture_at(
        &self,
        input: LocalSymbolViewInput,
        now: SystemTime,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.store.capture_at(input, self.limits, now)
    }

    fn capture_with_at(
        &self,
        input: LocalSymbolViewInput,
        now: SystemTime,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
    ) -> Result<Option<CapturedView>, RiftError> {
        self.store.capture_with_at(input, self.limits, now, admit)
    }
}

fn capture(store: &BoundedViews, reads: &Arc<ReadService>, now: SystemTime) -> CapturedView {
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
        BoundedViews::new(0, [25; 32])
            .capture(refused)
            .expect("bounded admission")
            .is_none()
    );
    let store = BoundedViews::new(1 << 20, [24; 32]);
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
    let store = BoundedViews::new(usize::MAX, [5; 32]);
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
    let store = BoundedViews::new(1 << 20, [13; 32]);
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
    let tiny = BoundedViews::new(1, [14; 32]);
    assert!(
        tiny.capture_with_at(captured_input(&reads), now, |_| {
            panic!("material refusal must precede response admission")
        })
        .expect("material refusal")
        .is_none()
    );
    let store = BoundedViews::new(1 << 20, [15; 32]);
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
    let store = BoundedViews::new(1 << 20, [10; 32]);
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
    let store = BoundedViews::new(1 << 20, [11; 32]);
    let foreign_store = BoundedViews::new(1 << 20, [12; 32]);
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
    let measured = BoundedViews::new(usize::MAX, [38; 32]);
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
    let limited = BoundedViews::new(shared, [39; 32]);
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
    assert!(
        limited
            .capture_at(text_generation_input(&indexes[2]), now)
            .expect("actual byte bound")
            .is_none(),
        "a third generation larger than the whole bound alone is refused"
    );
    let state = limited.state.lock().expect("state");
    assert_eq!(
        state.views.len(),
        2,
        "refusing an oversized generation releases neither held view"
    );
    assert!(state.views.contains_key(first.id.as_str()));
    assert!(state.views.contains_key(second.id.as_str()));
    drop(state);
    assert_eq!(held.material.index.source(&path), Some(original.as_str()));
    println!("cached_material_bytes={cached} shared_material_bytes={shared}");
}

/// Releases by measuring every remaining view again after each release: the rule the one
/// measure in use order must equal.
fn released_one_at_a_time(state: &mut State, kept: &str, bytes_max: usize) -> usize {
    let mut released = 0;
    while state.exceeds(bytes_max) && state.release_least_recent(Some(kept)) {
        released += 1;
    }
    released
}

#[test]
fn one_shared_measure_releases_what_measuring_after_each_release_does() {
    let (_directory, indexes, _) = distinct_text_generations();
    let now = SystemTime::now();
    let held = |service_key| {
        let store = BoundedViews::new(usize::MAX, service_key);
        let views = indexes
            .iter()
            .map(|index| {
                store
                    .capture_at(text_generation_input(index), now)
                    .expect("capture")
                    .expect("view")
            })
            .collect::<Vec<_>>();
        (store, views)
    };
    let (measured, views) = held([51; 32]);
    let kept = views.last().expect("newest view").id.as_str().to_owned();
    // The shared charge of the newest view alone, of the two newest, and of all three.
    let mut bounds = Vec::new();
    {
        let mut state = measured.state.lock().expect("state");
        bounds.push(state.material_bytes());
        loop {
            bounds.push(state.material_bytes_with_shared_indexes(true));
            if !state.release_least_recent(Some(&kept)) {
                break;
            }
        }
    }
    assert_eq!(bounds.len(), 4);
    let mut released_some = false;
    let mut kept_shared = false;
    for bound in bounds {
        // Bounds on both sides of each charge, closer than one released key's bytes.
        for bytes_max in (bound.saturating_sub(96)..=bound.saturating_add(96)).step_by(8) {
            let (expected, expected_views) = held([52; 32]);
            let (actual, actual_views) = held([52; 32]);
            assert_eq!(expected_views, actual_views);
            let kept = actual_views.last().expect("newest view").id.as_str();
            let mut expected = expected.state.lock().expect("state");
            let mut actual = actual.state.lock().expect("state");
            let expected_released = released_one_at_a_time(&mut expected, kept, bytes_max);
            let (mut released, settled) = actual.release_measured(kept, bytes_max);
            if !settled {
                released += released_one_at_a_time(&mut actual, kept, bytes_max);
            }
            assert_eq!(released, expected_released, "bytes_max={bytes_max}");
            assert_eq!(
                actual.views.keys().collect::<Vec<_>>(),
                expected.views.keys().collect::<Vec<_>>(),
                "bytes_max={bytes_max}"
            );
            assert_eq!(actual.expired, expected.expired, "bytes_max={bytes_max}");
            released_some |= released > 0;
            kept_shared |= released == 0 && actual.material_bytes() > bytes_max;
        }
    }
    assert!(
        released_some,
        "a bound below the shared charge releases views"
    );
    assert!(
        kept_shared,
        "a bound the shared charge fits keeps every view"
    );
}

#[test]
fn admissions_release_only_what_the_byte_bound_requires_and_keep_held_sources() {
    let (directory, indexes, original) = distinct_text_generations();
    let now = SystemTime::now();
    let reads = ReadService::from_prepared_index(
        rift_index::WorkspaceIndex::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &rift_core::LanguageFileSelections::default(),
        )
        .expect("workspace index"),
        None,
        Arc::new(rift_dependency::DependencyContext::default()),
        HistoryConfiguration::default(),
        rift_protocol::dependencies::DependenciesConfiguration::default(),
    );
    let path = rift_core::ProjectPath::new("a.txt").expect("text path");
    // The cached charge of the three generations, then the shared charge of all three,
    // of the two newest, and of the newest alone.
    let mut bounds = Vec::new();
    {
        let measured = BoundedViews::new(usize::MAX, [53; 32]);
        let mut kept = String::new();
        for index in &indexes {
            let view = measured
                .capture_at(text_generation_input(index), now)
                .expect("capture")
                .expect("view");
            kept = view.id.as_str().to_owned();
        }
        let mut state = measured.state.lock().expect("state");
        bounds.push(state.material_bytes());
        loop {
            bounds.push(state.material_bytes_with_shared_indexes(true));
            if !state.release_least_recent(Some(&kept)) {
                break;
            }
        }
    }
    assert_eq!(bounds.len(), 4);
    let mut released_some = false;
    let mut kept_shared = false;
    for bound in bounds {
        for budget in (bound.saturating_sub(96)..=bound.saturating_add(96)).step_by(8) {
            let store = BoundedViews::new(budget, [54; 32]);
            let mut held_sources = Vec::new();
            for index in &indexes {
                let input = text_generation_input(index);
                let material = Arc::clone(&input.material);
                let before = HeldBefore::of(&store);
                let Some(admitted) = store.capture_at(input, now).expect("capture") else {
                    break;
                };
                released_some |= before.assert_releases_required(&store, &admitted, budget) > 0;
                held_sources.push((path.clone(), original.clone(), material, admitted));
                assert_retained_held_sources(&store, &reads, &held_sources);
            }
            let state = store.state.lock().expect("state");
            kept_shared |= state.views.len() == indexes.len() && state.material_bytes() > budget;
        }
    }
    assert!(
        released_some,
        "a bound below the shared charge releases views"
    );
    assert!(
        kept_shared,
        "a bound the shared charge fits keeps every view"
    );
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
    let store = BoundedViews::new(budget, [23; 32]);
    let now = SystemTime::now();
    let mut held_sources = Vec::new();
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
        let held_source = reads
            .index
            .files()
            .find(|file| !file.source().is_empty() && file.source().len() <= 65_536)
            .map(|file| {
                let original = input
                    .material
                    .index
                    .source(file.path())
                    .expect("original captured source")
                    .to_owned();
                (file.path().clone(), original, Arc::clone(&input.material))
            });
        let before = HeldBefore::of(&store);
        let started = std::time::Instant::now();
        let admitted = store
            .capture_at(input, now)
            .expect("material admission")
            .expect("original byte bound admits shared material");
        let capture_elapsed = started.elapsed();
        let released = before.assert_releases_required(&store, &admitted, budget);
        if released > 0 {
            println!("released_views={released} prepared={prepared}");
        }
        if let Some((path, original, material)) = held_source {
            held_sources.push((path, original, material, admitted));
        }
        assert_retained_held_sources(&store, &reads, &held_sources);
        if measured_shared_accounting(&store, budget, prepared, total, capture_elapsed) {
            assert!(!held_sources.is_empty(), "observed held source");
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
    store: &BoundedViews,
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

/// The views and released keys a store held before one admission.
struct HeldBefore(State);

impl HeldBefore {
    fn of(store: &BoundedViews) -> Self {
        let state = store.state.lock().expect("retained material state");
        let mut before = State::default();
        before.views.clone_from(&state.views);
        before.expired.clone_from(&state.expired);
        Self(before)
    }

    /// Checks the views the admission of `admitted` released, answering their count.
    ///
    /// The admitted view stays. Every released view was used before every view left.
    /// The byte bound required the last release: the views left, the released view used
    /// last, and the keys of the views released before it pass `budget` under the
    /// measure admission takes.
    fn assert_releases_required(
        self,
        store: &BoundedViews,
        admitted: &CapturedView,
        budget: usize,
    ) -> usize {
        let state = store.state.lock().expect("retained material state");
        assert!(state.views.contains_key(admitted.id.as_str()));
        let (kept, released): (Vec<_>, Vec<_>) = self
            .0
            .views
            .iter()
            .partition(|(key, _)| state.views.contains_key(key.as_str()));
        let Some((last_key, last)) = released.iter().max_by_key(|(_, held)| held.used) else {
            return 0;
        };
        assert!(
            kept.iter().all(|(_, held)| held.used > last.used),
            "a released view was used after a view left"
        );
        let mut wider = State::default();
        wider.views.clone_from(&state.views);
        wider.views.insert((*last_key).clone(), (*last).clone());
        wider.expired.clone_from(&self.0.expired);
        wider.expired.extend(
            released
                .iter()
                .filter(|(key, _)| key != last_key)
                .map(|(key, _)| (*key).clone()),
        );
        assert!(
            wider.exceeds(budget),
            "a released view fits the byte bound with the views left"
        );
        released.len()
    }
}

/// Every admitted view the store still holds keeps its original material.
fn assert_retained_held_sources(
    store: &BoundedViews,
    reads: &ReadService,
    admitted: &[HeldSourceView],
) {
    for (path, original, material, view) in admitted {
        let retained = store
            .state
            .lock()
            .expect("retained material state")
            .views
            .contains_key(view.id.as_str());
        if retained {
            assert_measured_held_source(store, reads, path, original, material, view);
        }
    }
}

/// One admitted view with a source it captured.
type HeldSourceView = (
    rift_core::ProjectPath,
    String,
    Arc<LocalSymbolMaterial>,
    CapturedView,
);

fn assert_measured_held_source(
    store: &BoundedViews,
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

#[test]
fn query_material_keeps_original_expiry_and_releases_at_the_provider_bound() {
    let (_directory, reads, _) = fixture(true);
    let store = BoundedViews::new(usize::MAX, [42; 32]);
    let now = SystemTime::now();
    let expires = now + Duration::from_secs(30);
    let input = captured_input(&reads)
        .with_query([1; 32], b"answer".to_vec(), Some(expires))
        .with_sources(&reads.index)
        .expect("sources")
        .with_warnings(vec![rift_protocol::read::ReadWarning::GlobalAccessDisabled])
        .expect("warnings");
    let view = store
        .capture_at(input, now)
        .expect("capture")
        .expect("view");
    let (held, bytes) = store.query_at(&[1; 32], now).expect("query");
    assert_eq!(held, view);
    assert_eq!(&*bytes, b"answer");
    assert_eq!(
        view.expires_at,
        chrono::DateTime::<chrono::Utc>::from(expires).to_rfc3339()
    );
    assert!(store.query_at(&[2; 32], now).is_none());
    assert!(
        store
            .query_at(&[1; 32], expires - Duration::from_secs(1))
            .is_some()
    );
    assert!(store.query_at(&[1; 32], expires).is_none());
    assert!(store.state.lock().expect("state").views.is_empty());
}

#[test]
fn query_material_uses_the_existing_byte_and_view_bounds() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::now();
    let measured = BoundedViews::new(usize::MAX, [43; 32]);
    capture(&measured, &reads, now);
    let budget = measured.state.lock().expect("state").material_bytes();
    let limited = BoundedViews::new(budget, [44; 32]);
    capture(&limited, &reads, now);
    let input = captured_input(&reads).with_query([1; 32], vec![0; 1024], None);
    assert!(
        limited
            .capture_at(input, now)
            .expect("byte bound")
            .is_none()
    );
    assert!(limited.query_at(&[1; 32], now).is_none());
    assert_eq!(limited.state.lock().expect("state").views.len(), 1);
    for ordinal in 0..VIEWS_DEFAULT {
        let mut key = [0; 32];
        key[..8].copy_from_slice(&(ordinal as u64).to_be_bytes());
        let input = captured_input(&reads).with_query(key, vec![0], None);
        let admitted = measured.capture_at(input, now).expect("capture");
        assert!(admitted.is_some(), "a full store releases its oldest view");
    }
    assert_eq!(
        measured.state.lock().expect("state").views.len(),
        VIEWS_DEFAULT
    );
    assert!(
        measured
            .query_at(&[0; 32], now + VIEW_LIFETIME - Duration::from_secs(1))
            .is_some()
    );
    assert!(measured.query_at(&[0; 32], now + VIEW_LIFETIME).is_none());
    assert!(measured.state.lock().expect("state").views.is_empty());
}

fn query_input(reads: &ReadService, ordinal: u8, bytes: usize) -> LocalSymbolViewInput {
    captured_input(reads).with_query([ordinal; 32], vec![ordinal; bytes], None)
}

#[test]
fn full_store_releases_least_recently_used_view_and_admits_the_next() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let store = BoundedViews::with_limits(RetainedViewLimits::new(3, usize::MAX), [45; 32]);
    let views: Vec<_> = (1..=3)
        .map(|ordinal| {
            store
                .capture_at(query_input(&reads, ordinal, 1), now)
                .expect("capture")
                .expect("view below the bound")
        })
        .collect();
    let (root, owner) = (reads.index.root(), reads.index.owner());
    assert_eq!(
        store.captured_view_at(&views[0].id, root, owner, now),
        Ok(views[0].clone()),
        "reading the oldest view makes it the most recently used"
    );
    let fourth = store
        .capture_at(query_input(&reads, 4, 1), now)
        .expect("capture")
        .expect("a full store admits the next view");
    let state = store.state.lock().expect("store");
    assert_eq!(state.views.len(), 3);
    assert!(state.views.contains_key(views[0].id.as_str()));
    assert!(!state.views.contains_key(views[1].id.as_str()));
    assert!(state.views.contains_key(views[2].id.as_str()));
    assert!(state.views.contains_key(fourth.id.as_str()));
    drop(state);
    assert_eq!(
        store.captured_view_at(&views[1].id, root, owner, now),
        Err(SymbolUnavailableReason::ViewExpired),
        "a released view reads as expired, not as another context"
    );
    assert!(
        store.query_at(&[2; 32], now).is_none(),
        "a released view's answer is gone, so its next page evaluates again"
    );
    assert!(store.query_at(&[3; 32], now).is_some());
    store
        .capture_at(query_input(&reads, 5, 1), now)
        .expect("capture")
        .expect("next view");
    assert!(
        store.query_at(&[3; 32], now).is_some(),
        "a query read keeps its view in use"
    );
    assert!(
        store.query_at(&[1; 32], now).is_none(),
        "the view read least recently is released next"
    );
}

#[test]
fn byte_bound_releases_old_views_and_refuses_only_an_oversized_view() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let measured = BoundedViews::new(usize::MAX, [46; 32]);
    for ordinal in 1..=2 {
        measured
            .capture_at(query_input(&reads, ordinal, 4096), now)
            .expect("capture")
            .expect("unbounded view");
    }
    // Slack below one view's query bytes leaves room for released keys, not a third view.
    let budget = measured.state.lock().expect("store").material_bytes() + 2048;
    let store = BoundedViews::new(budget, [47; 32]);
    let first = store
        .capture_at(query_input(&reads, 1, 4096), now)
        .expect("capture")
        .expect("first view");
    let second = store
        .capture_at(query_input(&reads, 2, 4096), now)
        .expect("capture")
        .expect("second view fits the measured budget");
    let third = store
        .capture_at(query_input(&reads, 3, 4096), now)
        .expect("capture")
        .expect("the byte bound releases the oldest view");
    {
        let state = store.state.lock().expect("store");
        assert!(!state.views.contains_key(first.id.as_str()));
        assert!(state.views.contains_key(second.id.as_str()));
        assert!(state.views.contains_key(third.id.as_str()));
        assert!(state.admitted_material_bytes(budget) <= budget);
    }
    assert!(
        store
            .capture_at(query_input(&reads, 4, budget), now)
            .expect("oversized refusal")
            .is_none(),
        "a view larger than the whole bound is refused"
    );
    let state = store.state.lock().expect("store");
    assert_eq!(
        state.views.len(),
        2,
        "refusing an oversized view releases nothing"
    );
    assert!(state.views.contains_key(second.id.as_str()));
    assert!(state.views.contains_key(third.id.as_str()));
}

#[test]
fn lowered_bounds_release_views_down_to_the_new_count() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let store = LocalSymbolViews::new([48; 32]);
    let wide = RetainedViewLimits::new(4, usize::MAX);
    for ordinal in 1..=4 {
        store
            .capture_at(query_input(&reads, ordinal, 1), wide, now)
            .expect("capture")
            .expect("view");
    }
    let narrow = RetainedViewLimits::new(2, usize::MAX);
    store
        .capture_at(query_input(&reads, 5, 1), narrow, now)
        .expect("capture")
        .expect("view under the lowered bound");
    let state = store.state.lock().expect("store");
    assert_eq!(state.views.len(), 2);
    drop(state);
    for released in 1..=3 {
        assert!(store.query_at(&[released; 32], now).is_none());
    }
    assert!(store.query_at(&[4; 32], now).is_some());
    assert!(store.query_at(&[5; 32], now).is_some());
}

#[test]
fn released_keys_stay_bounded() {
    let (_directory, reads, _) = fixture(true);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let store = LocalSymbolViews::new([49; 32]);
    let single = RetainedViewLimits::new(1, usize::MAX);
    for _ in 0..=RELEASED_KEYS_MAX {
        store
            .capture_at(captured_input(&reads), single, now)
            .expect("capture")
            .expect("view");
    }
    let state = store.state.lock().expect("store");
    assert_eq!(state.views.len(), 1);
    assert_eq!(state.expired.len(), RELEASED_KEYS_MAX);
}

#[test]
fn retained_view_limits_read_the_accepted_configuration() {
    let defaults = RetainedViewLimits::from(&rift_protocol::query::QueryConfiguration::default());
    assert_eq!(defaults.views_max(), VIEWS_DEFAULT);
    assert_eq!(
        u64::try_from(RELEASED_KEYS_MAX),
        Ok(rift_protocol::query::QUERY_RETAINED_VIEWS_MAX)
    );
    assert_eq!(
        u64::try_from(defaults.bytes_max()).expect("default fits"),
        rift_protocol::query::QUERY_RETAINED_BYTES_DEFAULT
    );
    let configured = rift_protocol::query::QueryConfiguration {
        retained_views: 8,
        retained_size: rift_protocol::configuration::ByteSize::from_bytes(64 << 20),
        ..Default::default()
    };
    assert_eq!(
        RetainedViewLimits::from(&configured),
        RetainedViewLimits::new(8, 64 << 20)
    );
}

#[test]
#[should_panic(expected = "a retained view store must admit at least one view")]
fn retained_view_limits_refuse_an_empty_store() {
    let _ = RetainedViewLimits::new(0, usize::MAX);
}
