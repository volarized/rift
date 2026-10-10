use super::*;
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;

fn fixture(metadata: bool) -> (tempfile::TempDir, Arc<ReadService>, SymbolId) {
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
        SymbolId(
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
        SymbolId::parse("rift://symbol/local/javascript/beacon/open").expect("valid exact identity")
    };
    (directory, reads, id)
}

fn request(id: SymbolId, view: CapturedViewId) -> GetSymbolParams {
    GetSymbolParams {
        id,
        include: vec![GetSymbolInclude::Source],
        rev: None,
        view: Some(view),
        declaration_limit: 1,
        declaration_cursor: None,
    }
}

fn capture(store: &LocalSymbolViews, reads: &Arc<ReadService>, now: SystemTime) -> CapturedView {
    store
        .capture_at(captured_input(reads), now)
        .expect("capture succeeds")
        .expect("admitted material")
}

fn captured_input(reads: &ReadService) -> LocalSymbolViewInput {
    reads
        .local_symbol_view_input(
            &GetSymbolParams {
                id: SymbolId::parse("rift://symbol/local/javascript/beacon/open")
                    .expect("fixture identity"),
                include: vec![GetSymbolInclude::Source],
                rev: None,
                view: None,
                declaration_limit: 1,
                declaration_cursor: None,
            },
            true,
        )
        .expect("measured publication")
}

#[test]
fn search_view_input_retains_nonempty_material_and_truthful_history_coverage() {
    let (_directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [8; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let input = reads
        .local_search_view_input(true)
        .expect("captured search material");
    assert!(input.ready);
    assert!(input.material.history.is_none());
    assert!(!input.material.index.index_documents().is_empty());
    let view = store
        .capture_at(input, now)
        .expect("search view admission")
        .expect("view");
    let mut params = request(id, view.id);
    params.include = vec![GetSymbolInclude::Source, GetSymbolInclude::Documentation];
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("same captured read material"),
        GetSymbolResult::Found { .. }
    ));
    params.include.push(GetSymbolInclude::History);
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("history coverage"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
}

#[test]
fn captured_nonempty_material_refuses_uncaptured_history_without_live_lookup() {
    let (directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [9; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = capture(&store, &reads, now);
    let material = Arc::clone(&store.state.lock().expect("state").views[view.id.as_str()].material);
    assert!(!material.index.index_documents().is_empty());
    assert!(material.index.retained_material_bytes() > material.metadata.len());
    std::fs::remove_file(directory.path().join("index.js")).expect("remove current source");
    let mut params = request(id, view.id);
    params.include = vec![GetSymbolInclude::Source, GetSymbolInclude::Documentation];
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("captured source and documentation"),
        GetSymbolResult::Found { .. }
    ));
    params.include.push(GetSymbolInclude::History);
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("uncaptured history"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
}

#[test]
fn exact_local_view_keeps_original_bindings_sources_and_continuations() {
    let (directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [1; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = capture(&store, &reads, now);
    let mut params = request(id.clone(), view.id.clone());
    let mut ranges = BTreeSet::new();
    let mut original = None;
    for page in 0..3 {
        let result = store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("exact read");
        assert!(result.is_valid_for(&id));
        let GetSymbolResult::Found {
            symbol,
            view: answered,
            declarations,
            declaration_cursor,
            ..
        } = result
        else {
            panic!("established canonical object must be found");
        };
        assert_eq!(answered, view, "paging preserves original expiry");
        assert_eq!(symbol.signatures.len(), 3);
        assert_eq!(symbol.documentation.len(), 3);
        assert_eq!(declarations.len(), 1);
        let declaration = &declarations[0];
        assert!(declaration.source_complete);
        assert!(declaration.node.is_some());
        assert!(
            declaration
                .source
                .as_ref()
                .is_some_and(|source| source.contains("open"))
        );
        assert_eq!(declaration.signature_indices.len(), 1);
        assert_eq!(declaration.documentation_indices.len(), 1);
        assert!(ranges.insert((
            declaration.path.clone().expect("physical path"),
            declaration.range.start
        )));
        if page == 0 {
            original = Some(params.clone());
        }
        params.declaration_cursor = declaration_cursor;
        assert_eq!(params.declaration_cursor.is_some(), page < 2);
    }
    std::fs::write(
        directory.path().join("index.js"),
        "export function replaced() {}\n",
    )
    .expect("live change");
    let original = original.expect("first request");
    let repeated = store
        .resolve_at(&original, reads.index.root(), reads.index.owner(), now)
        .expect("retained first page");
    let GetSymbolResult::Found { symbol, .. } = repeated else {
        panic!("live changes cannot replace retained authority");
    };
    assert_eq!(symbol.name, "open");
}

#[test]
fn local_views_refuse_expired_foreign_and_changed_cursor_context() {
    let (_directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [2; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = capture(&store, &reads, now);
    let mut params = request(id.clone(), view.id.clone());
    let first = store
        .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
        .expect("first page");
    let GetSymbolResult::Found {
        declaration_cursor, ..
    } = first
    else {
        panic!("found");
    };
    params.declaration_cursor = declaration_cursor;
    params.include.clear();
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("context refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::ViewContext,
            ..
        }
    ));
    let params = request(id.clone(), view.id.clone());
    let foreign = SymbolOwner::NamedLocal {
        name: "cloud".to_owned(),
    };
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), &foreign, now)
            .expect("owner refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::ViewContext,
            ..
        }
    ));
    let other_root = tempfile::tempdir().expect("foreign root");
    assert!(matches!(
        store
            .resolve_at(&params, other_root.path(), reads.index.owner(), now)
            .expect("root refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::ViewContext,
            ..
        }
    ));
    assert!(matches!(
        store
            .resolve_at(
                &params,
                reads.index.root(),
                reads.index.owner(),
                now + VIEW_LIFETIME
            )
            .expect("expiry refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::ViewExpired,
            ..
        }
    ));
}

#[test]
fn local_view_admission_charges_shared_generations_once_and_keeps_active_authority() {
    let (_directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [3; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let first = capture(&store, &reads, now);
    let first_charge = store.state.lock().expect("store").material_bytes();
    let second = capture(&store, &reads, now);
    let second_charge = store.state.lock().expect("store").material_bytes();
    assert!(
        second_charge - first_charge < first_charge / 2,
        "same immutable generation must not be charged twice"
    );
    let mut unmeasured = captured_input(&reads);
    unmeasured.retained_bytes = 0;
    let refused = store
        .capture_at(unmeasured, now)
        .expect("unmeasured admission refuses");
    assert!(refused.is_none());
    assert!(matches!(
        store
            .resolve_at(
                &request(id, first.id),
                reads.index.root(),
                reads.index.owner(),
                now
            )
            .expect("active authority"),
        GetSymbolResult::Found { .. }
    ));
    assert!(
        store
            .state
            .lock()
            .expect("store")
            .views
            .contains_key(second.id.as_str())
    );
    let tiny = LocalSymbolViews::new(first_charge - 1, [4; 32]);
    assert!(
        tiny.capture_at(captured_input(&reads), now)
            .expect("byte bound refusal")
            .is_none()
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
fn canonical_missing_requires_complete_identity_coverage() {
    for metadata in [true, false] {
        let (_directory, reads, _) = fixture(metadata);
        let store = LocalSymbolViews::new(1 << 20, [5; 32]);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let view = capture(&store, &reads, now);
        let id = SymbolId::parse("rift://symbol/local/javascript/beacon/missing")
            .expect("canonical miss");
        let result = store
            .resolve_at(
                &request(id, view.id),
                reads.index.root(),
                reads.index.owner(),
                now,
            )
            .expect("exact selection");
        if metadata {
            assert!(matches!(result, GetSymbolResult::Missing { .. }));
        } else {
            assert!(matches!(
                result,
                GetSymbolResult::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    ..
                }
            ));
        }
    }
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
