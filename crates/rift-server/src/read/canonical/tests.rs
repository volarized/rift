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
        include: vec![GetSymbolInclude::Documentation],
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
                include: Vec::new(),
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
    params.include = vec![GetSymbolInclude::Documentation];
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
        None,
        None,
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
    params.include = vec![GetSymbolInclude::Documentation];
    assert!(matches!(
        store
            .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
            .expect("captured documentation"),
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
fn exact_local_view_keeps_original_bindings_and_continuations() {
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

#[test]
fn captured_nodes_keep_canonical_binding_and_file_source_after_live_source_changes() {
    let (directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [13; 32]);
    let view = store
        .capture(captured_input(&reads))
        .expect("admission")
        .expect("view");
    let node_request = rift_protocol::read::NodesParams {
        view: Some(view.id.clone()),
        ..rift_protocol::read::NodesParams::at(
            rift_protocol::read::ProjectPath("index.js".to_owned()),
            40,
        )
    };
    let nodes = store
        .nodes(
            &node_request,
            &view.id,
            reads.index.root(),
            reads.index.owner(),
        )
        .expect("node read")
        .expect("captured nodes");
    assert_eq!(nodes.1, view);
    assert_eq!(nodes.0.view.as_ref(), Some(&view));
    assert_eq!(nodes.0.symbol.as_ref(), Some(&id));
    let item = nodes
        .0
        .nodes
        .first()
        .expect("the captured function holds nodes");
    assert_eq!(item.index, 0);
    assert!(
        reads
            .index
            .file(&rift_core::ProjectPath::new("index.js").expect("path"))
            .expect("source")
            .source()
            .contains(item.source.as_str()),
        "the item node carries captured bytes"
    );
    std::fs::write(
        directory.path().join("index.js"),
        "export function changed() {}\n",
    )
    .expect("current source change");
    assert_eq!(
        store
            .nodes(
                &node_request,
                &view.id,
                reads.index.root(),
                reads.index.owner()
            )
            .expect("retained nodes")
            .expect("view"),
        nodes
    );
}

#[test]
fn captured_nodes_refuse_foreign_selection_and_invalid_positions() {
    let (_directory, reads, _) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [14; 32]);
    let view = store
        .capture(captured_input(&reads))
        .expect("admission")
        .expect("view");
    let nodes = rift_protocol::read::NodesParams {
        view: Some(view.id.clone()),
        ..rift_protocol::read::NodesParams::at(
            rift_protocol::read::ProjectPath("index.js".to_owned()),
            u64::MAX,
        )
    };
    assert!(
        store
            .nodes(&nodes, &view.id, reads.index.root(), reads.index.owner())
            .is_err()
    );
    let other_view = store
        .capture(captured_input(&reads))
        .expect("second admission")
        .expect("second view");
    let mut other_nodes = nodes.clone();
    other_nodes.view = Some(other_view.id);
    assert!(matches!(
        store
            .nodes(
                &other_nodes,
                &view.id,
                reads.index.root(),
                reads.index.owner()
            )
            .expect("request view context refusal"),
        Err(SymbolUnavailableReason::ViewContext)
    ));
    assert!(matches!(
        store
            .nodes(&nodes, &view.id, Path::new("/foreign"), reads.index.owner())
            .expect("context refusal"),
        Err(SymbolUnavailableReason::ViewContext)
    ));
}
#[test]
fn captured_input_getters_keep_exact_root_owner_and_material_charge() {
    let (_directory, reads, id) = fixture(true);
    let input = reads
        .local_search_view_input(true)
        .expect("immutable input");
    let charge = input.retained_bytes;
    assert_eq!(input.root(), reads.index.root());
    assert_eq!(input.owner(), reads.index.owner());
    assert_eq!(input.owner(), &SymbolOwner::Local);
    let root = input.root().to_owned();
    let owner = input.owner().clone();
    for _ in 0..256 {
        assert_eq!(input.root(), root);
        assert_eq!(input.owner(), &owner);
        assert_eq!(input.retained_bytes, charge);
    }
    let store = LocalSymbolViews::new(1 << 20, [18; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = store
        .capture_at(input, now)
        .expect("capture")
        .expect("admitted");
    assert!(matches!(
        store
            .resolve_at(&request(id, view.id), &root, &owner, now)
            .expect("same captured authority"),
        GetSymbolResult::Found { .. }
    ));
}

#[test]
fn captured_nodes_keep_inline_framework_after_current_source_changes() {
    let directory = tempfile::tempdir().expect("workspace");
    let original = "import {Component} from '@angular/core'; @Component({template:`<p>{{ title }}</p>`}) export class Beacon {}";
    std::fs::write(directory.path().join("component.ts"), original).expect("captured source");
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"name":"beacon","version":"1.0.0","main":"component.ts"}"#,
    )
    .expect("captured metadata");
    let reads = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
    .expect("captured index");
    let input = reads
        .local_search_view_input(true)
        .expect("immutable input");
    let store = LocalSymbolViews::new(1 << 20, [19; 32]);
    let view = store.capture(input).expect("capture").expect("admission");
    let request = rift_protocol::read::NodesParams {
        view: Some(view.id.clone()),
        ..rift_protocol::read::NodesParams::at(
            rift_protocol::read::ProjectPath("component.ts".to_owned()),
            u64::try_from(original.find("title").expect("template position")).expect("position"),
        )
    };
    let captured = store
        .nodes(&request, &view.id, reads.index.root(), reads.index.owner())
        .expect("captured node read")
        .expect("captured context");
    assert!(
        captured
            .0
            .nodes
            .iter()
            .any(|node| node.kind.0 == "interpolation")
    );
    assert_eq!(captured.0.view.as_ref(), Some(&view));
    assert!(
        captured
            .0
            .nodes
            .iter()
            .any(|node| node.source.contains("title"))
    );
    std::fs::write(
        directory.path().join("component.ts"),
        "export class Changed {}",
    )
    .expect("current source change");
    std::fs::write(directory.path().join("package.json"), "{}").expect("current metadata change");
    assert_eq!(
        store
            .nodes(&request, &view.id, reads.index.root(), reads.index.owner())
            .expect("retained node read")
            .expect("retained context"),
        captured
    );
}

#[test]
fn captured_traversal_refuses_missing_seed_before_admission() {
    let (_directory, reads, _) = fixture(true);
    let traversal = serde_json::from_value(serde_json::json!({"direction":"incoming"}))
        .expect("missing seed request");
    let error = reads
        .local_traversal_view_input(&traversal, crate::EngineReferences::default(), true)
        .expect_err("missing seed refuses before capture");
    assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
    let store = LocalSymbolViews::new(1 << 20, [20; 32]);
    let view = store
        .capture(captured_input(&reads))
        .expect("capture")
        .expect("view");
    let error = store
        .traversal(
            &traversal,
            &view.id,
            reads.index.root(),
            reads.index.owner(),
        )
        .expect_err("missing seed cannot read retained graph");
    assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
}

#[test]
fn captured_traversal_keeps_missing_engine_rows_unavailable() {
    let (_directory, reads, id) = fixture(true);
    let traversal = serde_json::from_value(serde_json::json!({"seed":id,"direction":"incoming"}))
        .expect("canonical traversal");
    let input = reads
        .local_traversal_view_input(&traversal, crate::EngineReferences::default(), true)
        .expect("captured syntax without engine authority");
    let store = LocalSymbolViews::new(1 << 20, [21; 32]);
    let view = store.capture(input).expect("capture").expect("view");
    assert!(matches!(
        store
            .traversal(
                &traversal,
                &view.id,
                reads.index.root(),
                reads.index.owner()
            )
            .expect("truthful engine coverage"),
        Err(SymbolUnavailableReason::InsufficientCoverage)
    ));
    assert!(matches!(
        store
            .traversal(
                &traversal,
                &view.id,
                Path::new("/foreign"),
                reads.index.owner()
            )
            .expect("context refusal"),
        Err(SymbolUnavailableReason::ViewContext)
    ));
}

#[test]
fn captured_input_warnings_remain_measured_after_current_source_changes() {
    let (directory, reads, id) = fixture(true);
    let warning = rift_protocol::read::ReadWarning::StaleIndex {
        index_tree_revision: rift_protocol::read::Digest("12345678".into()),
        captured_tree_revision: rift_protocol::read::Digest("87654321".into()),
        detail: "captured tree changed after publication".into(),
    };
    let input = reads
        .local_search_view_input(true)
        .expect("immutable input");
    let charge = input.retained_bytes;
    let input = input
        .with_warnings(vec![warning.clone()])
        .expect("measured warning");
    assert!(input.retained_bytes > charge);
    let metadata: CapturedMetadata =
        serde_json::from_slice(&input.material.metadata).expect("captured metadata");
    assert_eq!(metadata.warnings.last(), Some(&warning));
    assert_eq!(
        input.warnings().expect("captured warnings"),
        metadata.warnings
    );
    let root = input.root().to_owned();
    let owner = input.owner().clone();
    let store = LocalSymbolViews::new(1 << 20, [24; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = store
        .capture_at(input, now)
        .expect("capture")
        .expect("admitted");
    let selected = request(id, view.id.clone());
    let initial = store
        .resolve_at(&selected, &root, &owner, now)
        .expect("initial read");
    std::fs::write(
        directory.path().join("index.js"),
        "export function changed() {}\n",
    )
    .expect("later source");
    let later = store
        .resolve_at(&selected, &root, &owner, now)
        .expect("held read");
    let GetSymbolResult::Found {
        warnings: initial_warnings,
        view: initial_view,
        ..
    } = initial
    else {
        panic!("initial object")
    };
    let GetSymbolResult::Found {
        warnings: later_warnings,
        view: later_view,
        ..
    } = later
    else {
        panic!("held object")
    };
    assert_eq!(initial_warnings, later_warnings);
    assert_eq!(
        initial_warnings, metadata.warnings,
        "the overloads and the implementation spell one qualified name, so nothing disagrees"
    );
    assert!(initial_warnings.contains(&warning));
    assert_eq!(initial_view, later_view);
    assert_eq!(later_view, view);
}

#[test]
fn captured_input_warning_extension_preserves_preparing_refusal() {
    let (_directory, reads, id) = fixture(true);
    let input = reads
        .local_search_view_input(false)
        .expect("preparing input")
        .with_warnings(vec![rift_protocol::read::ReadWarning::GlobalAccessDisabled])
        .expect("warning");
    assert!(!input.ready);
    let root = input.root().to_owned();
    let owner = input.owner().clone();
    let store = LocalSymbolViews::new(1 << 20, [25; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = store
        .capture_at(input, now)
        .expect("capture")
        .expect("admitted");
    assert!(matches!(
        store
            .resolve_at(&request(id, view.id), &root, &owner, now)
            .expect("preparing refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::IndexPreparing,
            ..
        }
    ));
}

fn captured_documentation_hits(
    reads: &ReadService,
    view: &CapturedView,
) -> Vec<rift_protocol::read::SearchHit> {
    use rift_protocol::documentation::{DOCUMENTATION_EXCERPT_BYTES_MAX, DocumentationHit};
    use rift_protocol::read::{SearchHit, SearchHitTarget};
    let collection = reads.index.documentation();
    collection
        .index()
        .blocks
        .iter()
        .filter(|block| {
            block.range.end - block.range.start > u64::from(DOCUMENTATION_EXCERPT_BYTES_MAX)
        })
        .map(|block| SearchHit {
            view: Some(view.clone()),
            hit: SearchHitTarget::Documentation {
                documentation: Box::new(DocumentationHit {
                    block: block.clone(),
                    source: collection
                        .source(&block.source)
                        .expect("captured content owner")
                        .clone(),
                    documentation_revision: collection.index().documentation_revision.clone(),
                }),
            },
            score: None,
            matched_by: Vec::new(),
            source: None,
            range: Some(block.range.clone()),
            line: Some(block.line),
            path: match &block.source.source {
                rift_protocol::documentation::DocumentationSourceIdentity::Project { path } => {
                    Some(path.clone())
                }
                rift_protocol::documentation::DocumentationSourceIdentity::Package { .. } => None,
            },
            unit: match &block.source.source {
                rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } => {
                    Some(unit.clone())
                }
                rift_protocol::documentation::DocumentationSourceIdentity::Project { .. } => None,
            },
            traversal_path: None,
            distance: None,
            change: None,
        })
        .collect()
}

#[test]
fn captured_documentation_pages_reset_excerpt_budget_and_keep_original_utf8_sources() {
    use rift_protocol::documentation::{DOCUMENTATION_EXCERPT_BYTES_MAX, DocumentationWarningKind};
    let directory = tempfile::tempdir().expect("workspace");
    let budget = DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
    let original = format!("beacon {}\n", "é".repeat(budget));
    for name in ["first.md", "second.md"] {
        std::fs::write(directory.path().join(name), &original).expect("captured documentation");
    }
    let reads = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
    .expect("captured index");
    let store = LocalSymbolViews::new(64 << 20, [37; 32]);
    let view = store
        .capture(
            reads
                .local_search_view_input(true)
                .expect("captured material"),
        )
        .expect("material admission")
        .expect("captured view");
    let hits = captured_documentation_hits(&reads, &view);
    assert_eq!(hits.len(), 2);
    for name in ["first.md", "second.md"] {
        std::fs::write(directory.path().join(name), "changed current bytes\n")
            .expect("source change");
    }
    let mut combined = hits.clone();
    let warnings = store
        .populate_sources(
            &mut combined,
            &view.id,
            reads.index.root(),
            reads.index.owner(),
        )
        .expect("held page")
        .expect("original context");
    assert_eq!(
        combined[0].source.as_ref().expect("first excerpt").len(),
        budget - 1
    );
    assert!(
        combined[1]
            .source
            .as_ref()
            .is_none_or(|source| source.len() <= 1)
    );
    assert!(!warnings.is_empty());
    for hit in hits {
        let mut page = vec![hit];
        let warnings = store
            .populate_sources(&mut page, &view.id, reads.index.root(), reads.index.owner())
            .expect("held next page")
            .expect("original context");
        let source = page[0]
            .source
            .as_ref()
            .expect("each page receives its own excerpt budget");
        assert_eq!(source.as_bytes(), &original.as_bytes()[..budget - 1]);
        assert_eq!(page[0].view.as_ref(), Some(&view));
        assert!(warnings.iter().any(|warning| matches!(warning,
            rift_protocol::read::ReadWarning::Documentation { warning }
            if warning.kind == DocumentationWarningKind::LimitExceeded && warning.count == 1)));
    }
}

#[test]
#[ignore = "manual retained material measurement"]
fn captured_local_view_material_measures_existing_workspace() {
    let root = std::env::var_os("RIFT_RETAINED_MATERIAL_ROOT").expect("measurement root");
    let root = std::path::PathBuf::from(root);
    let reads = ReadService::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
    .expect("captured workspace");
    let first_input = reads
        .local_search_view_input(true)
        .expect("immutable measured input");
    let immutable = Arc::clone(&first_input.material.index);
    let immutable_bytes = immutable.retained_material_bytes();
    let factory_bytes = first_input.retained_bytes;
    assert_ne!(immutable_bytes, usize::MAX);
    assert_ne!(factory_bytes, usize::MAX);
    let budget = usize::try_from(8_u64 << 30).expect("measurement budget");
    let store = LocalSymbolViews::new(budget, [23; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let first = store
        .capture_at(first_input, now)
        .expect("first capture")
        .expect("ordinary workspace admission");
    let first_state = store.state.lock().expect("store").material_bytes();
    let second_input = reads
        .local_search_view_input(true)
        .expect("same publication input");
    assert!(Arc::ptr_eq(&immutable, &second_input.material.index));
    let second = store
        .capture_at(second_input, now)
        .expect("second capture")
        .expect("shared generation admission");
    let second_state = {
        let state = store.state.lock().expect("store");
        assert_eq!(state.views.len(), 2);
        assert!(state.views.contains_key(first.id.as_str()));
        assert!(state.views.contains_key(second.id.as_str()));
        state.material_bytes()
    };
    assert!(second_state <= budget);
    assert!(second_state - first_state < immutable_bytes);
    for record in immutable.normalized_graph().records() {
        let Some(identity) = record.identity() else {
            continue;
        };
        let (_, count) = immutable
            .declarations_by_identity_page(identity, 0, 1)
            .expect("captured binding page");
        if count < 2 {
            continue;
        }
        let params = request(
            SymbolId::parse(identity.as_str()).expect("captured identity"),
            first.id.clone(),
        );
        if matches!(
            store
                .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
                .expect("captured declaration page"),
            GetSymbolResult::Found {
                declaration_cursor: Some(_),
                ..
            }
        ) {
            break;
        }
    }
    let state = store.state.lock().expect("store");
    let cursor_state = state.material_bytes();
    assert!(cursor_state <= budget);
    println!(
        "retained_material_bytes={immutable_bytes} factory_bytes={factory_bytes} first_state_bytes={first_state} second_state_bytes={second_state} cursor_state_bytes={cursor_state} cursor_count={} budget_bytes={budget} root={}",
        state.cursors.len(),
        root.display(),
    );
}

#[test]
fn local_view_continuation_charges_owned_context_and_refuses_byte_overflow() {
    let (_directory, reads, id) = fixture(true);
    let store = LocalSymbolViews::new(1 << 20, [27; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let view = capture(&store, &reads, now);
    let before = store.state.lock().expect("state").material_bytes();
    let params = request(id.clone(), view.id.clone());
    let result = store
        .resolve_at(&params, reads.index.root(), reads.index.owner(), now)
        .expect("real declaration continuation");
    let GetSymbolResult::Found {
        declaration_cursor: Some(key),
        view: answered,
        ..
    } = result
    else {
        panic!("three original declarations require a continuation");
    };
    assert_eq!(answered, view);
    {
        let state = store.state.lock().expect("state");
        assert_eq!(state.cursors.len(), 1);
        let cursor = state.cursors.get(&key).expect("retained continuation");
        assert_eq!(cursor.id, id);
        assert_eq!(cursor.view, view.id);
        assert_eq!(cursor.include, params.include);
        assert!(state.material_bytes() > before);
        assert!(state.material_bytes() - before >= cursor.retained_bytes());
        assert!(state.material_bytes() <= store.material_bytes_max);
    }
    let limited = LocalSymbolViews::new(before, [28; 32]);
    let limited_view = capture(&limited, &reads, now);
    let limited_params = request(id, limited_view.id.clone());
    assert!(matches!(
        limited
            .resolve_at(
                &limited_params,
                reads.index.root(),
                reads.index.owner(),
                now
            )
            .expect("charged continuation refusal"),
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
    let state = limited.state.lock().expect("state");
    assert!(state.cursors.is_empty());
    assert!(state.views.contains_key(limited_view.id.as_str()));
    assert!(state.material_bytes() <= before);
}

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
        revision: None,
        warnings: Vec::new(),
    })
    .expect("captured metadata");
    local_view_input(
        index.symbol_view().expect("captured generation"),
        metadata,
        None,
        None,
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
fn shared_generation_accounting_preserves_slot_and_overflow_refusals() {
    let (_directory, reads, id) = fixture(true);
    let budget = usize::try_from(8_u64 << 30).expect("existing byte bound");
    let store = LocalSymbolViews::new(budget, [40; 32]);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let first = capture(&store, &reads, now);
    for _ in 1..VIEW_SLOTS {
        capture(&store, &reads, now);
    }
    assert!(
        store
            .capture_at(captured_input(&reads), now)
            .expect("slot refusal")
            .is_none()
    );
    assert!(matches!(
        store
            .resolve_at(
                &request(id, first.id),
                reads.index.root(),
                reads.index.owner(),
                now
            )
            .expect("original held view"),
        GetSymbolResult::Found { .. }
    ));
    let state = store.state.lock().expect("state");
    assert_eq!(state.views.len(), VIEW_SLOTS);
    assert!(state.admitted_material_bytes(budget) <= budget);
    drop(state);
    let overflow = LocalSymbolViews::new(budget, [41; 32]);
    let mut input = captured_input(&reads);
    input.retained_bytes = usize::MAX;
    assert!(
        overflow
            .capture_at(input, now)
            .expect("MAX refusal")
            .is_none()
    );
    assert!(overflow.state.lock().expect("state").views.is_empty());
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

/// A captured view over a workspace holding one JavaScript function and one text file, and
/// the function's identity.
fn text_and_symbol_view() -> (
    tempfile::TempDir,
    Arc<ReadService>,
    LocalSymbolViews,
    CapturedView,
    SymbolId,
) {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"name":"beacon","version":"1.0.0","main":"index.js"}"#,
    )
    .expect("captured metadata");
    std::fs::write(
        directory.path().join("index.js"),
        "export function open(path) {\n  return path;\n}\n",
    )
    .expect("implementation");
    std::fs::write(directory.path().join("notes.txt"), "beacon notes\n").expect("text");
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
    let matched = reads.index.symbols("open", 1).expect("declaration")[0];
    let id = SymbolId(
        reads
            .index
            .assembled_symbol(matched)
            .expect("assembled object")
            .identity()
            .expect("canonical identity")
            .as_str()
            .to_owned(),
    );
    let store = LocalSymbolViews::new(1 << 20, [17; 32]);
    let view = store
        .capture(captured_input(&reads))
        .expect("admission")
        .expect("view");
    (directory, reads, store, view, id)
}

/// The captured nodes `request` reads, or the reason the view refuses them.
fn captured_read(
    reads: &ReadService,
    store: &LocalSymbolViews,
    view: &CapturedView,
    request: rift_protocol::read::NodesParams,
) -> Result<rift_protocol::read::NodesResult, SymbolUnavailableReason> {
    let request = rift_protocol::read::NodesParams {
        view: Some(view.id.clone()),
        ..request
    };
    store
        .nodes(&request, &view.id, reads.index.root(), reads.index.owner())
        .expect("node read")
        .map(|(nodes, _)| nodes)
}

#[test]
fn captured_nodes_list_a_symbol_and_the_file_node_of_a_text_file() {
    let (_directory, reads, store, view, id) = text_and_symbol_view();

    let symbol = captured_read(
        &reads,
        &store,
        &view,
        rift_protocol::read::NodesParams::of(id.clone()),
    )
    .expect("captured symbol nodes");
    let text = captured_read(
        &reads,
        &store,
        &view,
        rift_protocol::read::NodesParams::at(
            rift_protocol::read::ProjectPath("notes.txt".to_owned()),
            3,
        ),
    )
    .expect("captured text nodes");

    assert_eq!(symbol.symbol.as_ref(), Some(&id));
    assert_eq!(symbol.nodes[0].index, 0);
    assert!(symbol.nodes[0].source.contains("function open(path)"));
    assert!(
        symbol
            .nodes
            .iter()
            .any(|node| node.source == "return path;")
    );
    assert_eq!(symbol.node_cursor, None);
    assert_eq!(text.nodes.len(), 1);
    assert_eq!(text.nodes[0].kind.0, "file");
    assert_eq!(text.nodes[0].source, "beacon notes\n");
}

#[test]
fn captured_nodes_refuse_an_absent_symbol_and_an_absent_file_as_uncovered() {
    let (_directory, reads, store, view, _id) = text_and_symbol_view();
    let absent =
        rift_protocol::read::SymbolId("rift://symbol/local/javascript/beacon/absent".to_owned());
    let missing = rift_protocol::read::NodesParams::at(
        rift_protocol::read::ProjectPath("absent.txt".to_owned()),
        0,
    );

    for request in [rift_protocol::read::NodesParams::of(absent), missing] {
        assert_eq!(
            captured_read(&reads, &store, &view, request).err(),
            Some(SymbolUnavailableReason::InsufficientCoverage)
        );
    }
}
