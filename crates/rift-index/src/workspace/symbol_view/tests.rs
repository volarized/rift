use std::{collections::BTreeMap, fs, sync::Arc};

use get_size2::{GetSize, StandardTracker};

use super::super::{
    SourceVisibility, SymbolDeclarations, TextFileInclusion, WorkspaceIndex, WorkspaceIndexLimits,
};

#[test]
fn captured_symbol_material_keeps_source_graph_ranking_and_binding_facts() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
    )
    .expect("captured metadata");
    fs::write(
        root.path().join("lib.rs"),
        "/// Original.\npub fn kept() {}\n",
    )
    .expect("captured source");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let matched = index.symbols("kept", 1).expect("physical declaration")[0];
    let object = index.assembled_symbol(matched).expect("canonical facts");
    let identity = object.identity().expect("established identity").clone();
    let view = index.symbol_view().expect("captured material");
    assert!(std::ptr::eq(
        view.normalized_graph(),
        index.normalized_graph()
    ));
    assert!(std::ptr::eq(view.relationships(), index.relationships()));
    assert_eq!(view.index_documents().len(), index.index_documents().len());
    assert_eq!(
        view.object_by_identity(&identity)
            .expect("object")
            .expect("found")
            .facts(),
        object.facts()
    );
    let (bindings, total) = view
        .symbols_by_identity_page(&identity, 0, 1)
        .expect("page");
    assert_eq!(total, 1);
    assert_eq!(bindings[0].symbol.item_range, matched.symbol.item_range);
    let path = matched.file.path().clone();
    assert!(view.retained_material_bytes() > matched.file.source().len());
    assert_ne!(view.retained_material_bytes(), usize::MAX);
    drop(index);
    fs::remove_file(root.path().join("lib.rs")).expect("remove live source");
    assert_eq!(
        view.file(&path).expect("retained source").source(),
        "/// Original.\npub fn kept() {}\n"
    );
    assert!(
        view.object_by_identity(&identity)
            .expect("retained object")
            .is_some()
    );
}

#[test]
fn captured_symbol_material_charges_shared_generation_once() {
    let root = tempfile::tempdir().expect("workspace");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("empty index");
    let before = super::retained_size::MEASUREMENTS.get();
    let view = index.symbol_view().expect("captured material");
    assert_eq!(super::retained_size::MEASUREMENTS.get(), before + 1);
    let charge = view.retained_material_bytes();
    for _ in 0..256 {
        assert_eq!(view.retained_material_bytes(), charge);
        assert!(Arc::ptr_eq(
            &view,
            &index.symbol_view().expect("same captured material")
        ));
    }
    assert_eq!(super::retained_size::MEASUREMENTS.get(), before + 1);
    assert!(Arc::ptr_eq(
        &view,
        &index.symbol_view().expect("same publication")
    ));
    let (one, _) = view.get_heap_size_with_tracker(StandardTracker::default());
    let pair = (Arc::clone(&view), Arc::clone(&view));
    let (two, _) = pair.get_heap_size_with_tracker(StandardTracker::default());
    assert!(one > 0);
    assert_eq!(one, two);
    assert_ne!(view.retained_material_bytes(), usize::MAX);
}

#[test]
#[ignore = "manual retained material measurement"]
fn captured_symbol_material_measures_existing_workspace() {
    let root = std::env::var_os("RIFT_RETAINED_MATERIAL_ROOT").expect("measurement root");
    let root = std::path::PathBuf::from(root);
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("captured workspace");
    let view = index.symbol_view().expect("captured material");
    let bytes = view.retained_material_bytes();
    assert_ne!(
        bytes,
        usize::MAX,
        "ordinary workspace material must be measured"
    );
    println!("retained_material_bytes={bytes} root={}", root.display());
}

#[test]
fn captured_symbol_material_refuses_unmeasured_portable_facts_without_overflow() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
    )
    .expect("captured metadata");
    fs::write(root.path().join("lib.rs"), "pub fn kept() {}\n").expect("source");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let matched = index.symbols("kept", 1).expect("physical declaration")[0];
    let mut object = index.assembled_symbol(matched).expect("canonical facts");
    let identity = object.identity().expect("established identity").clone();
    let mut signatures = object.facts.signatures_slice().to_vec();
    signatures[0].extensions = rift_protocol::read::Extensions(BTreeMap::from([(
        rift_protocol::read::ExtensionKey("example.org.fact".to_owned()),
        rift_protocol::read::ExtensionValue {
            version: 1,
            data: serde_json::json!({"held": "value"}),
        },
    )]));
    object.facts = object.facts.clone().signatures(signatures);
    let mut view = index.build_symbol_view().expect("immutable view");
    view.declarations = Arc::new(SymbolDeclarations {
        assembled: BTreeMap::from([(identity, Ok(object))]),
        ..SymbolDeclarations::default()
    });
    assert_eq!(super::retained_size::measure(&view), usize::MAX);
    assert_eq!(
        view.declarations.assembled.len(),
        1,
        "refusal retains object facts"
    );
    assert!(!view.normalized_graph().records().is_empty());
}
