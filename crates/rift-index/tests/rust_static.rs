//! Stored Rust edges from captured source without a language engine.

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

#[test]
fn test_rust_static_edges_keep_declared_types_and_members() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "mod jobs; use crate::jobs::{Task, leaf as run}; trait Run { fn execute(&self); } fn root(task: &Task) { run(); Task::new(); task.execute(); }").expect("root source");
    std::fs::write(root.join("src/jobs.rs"), "use crate::Run; pub struct Value; impl Value { fn open(&self) {} } pub struct Task { value: Value } pub fn leaf() {} impl Task { pub fn new() -> Self { Self { value: Value } } fn read(&mut self, value: Value) { self.value = value; let value = &self.value; self.value.open(); } } impl Run for Task { fn execute(&self) {} }").expect("module source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    let mut kinds = std::collections::BTreeSet::new();
    for record in view.normalized_graph().records() {
        if let Some(id) = record.identity() {
            kinds.extend(
                index
                    .edges()
                    .outgoing(id)
                    .iter()
                    .map(rift_index::Edge::kind),
            );
        }
    }
    for kind in [
        EdgeKind::Calls,
        EdgeKind::Contains,
        EdgeKind::Implements,
        EdgeKind::Overrides,
        EdgeKind::Imports,
        EdgeKind::HasType,
        EdgeKind::Reads,
        EdgeKind::Writes,
    ] {
        assert!(kinds.contains(&kind), "missing {kind:?}: {kinds:?}");
    }
    assert!(
        view.normalized_graph()
            .records()
            .iter()
            .filter_map(|record| record.identity())
            .flat_map(|id| index.edges().outgoing(id))
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with("/Task/read")
                && edge.to().as_str().ends_with("/Value/open"))
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

#[test]
fn test_rust_impl_only_module_keeps_imported_calls_and_type_members() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(
        root.join("src/lib.rs"),
        "mod jobs; pub struct Task; pub fn leaf() {}",
    )
    .expect("root source");
    std::fs::write(
        root.join("src/jobs.rs"),
        "use crate::{Task, leaf}; impl Task { fn run(&self) { leaf(); } }",
    )
    .expect("module source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(index.edges().edges().any(
        |edge| edge.kind() == EdgeKind::Calls && edge.to().as_str().ends_with("/fixture/leaf")
    ));
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Contains
                && edge.from().as_str().ends_with("/fixture/Task")
                && edge.to().as_str().ends_with("/Task/run"))
    );
}

#[test]
fn test_rust_binding_does_not_shadow_its_own_initializer() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "fn leaf() -> (u8, u8) { (0, 0) } fn run() { let (leaf, other) = leaf(); } fn plain() { let leaf = leaf(); }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls
                && edge.to().as_str().ends_with("/fixture/leaf"))
            .count(),
        2
    );
}

#[test]
fn test_rust_assignment_keeps_the_declared_receiver_type() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "struct Task; impl Task { fn open(&self) {} } fn root(mut value: Task) { value = Task; value.open(); }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(
                |edge| edge.kind() == EdgeKind::Calls && edge.to().as_str().ends_with("/Task/open")
            )
            .count(),
        1
    );
}

#[test]
fn test_rust_local_type_keeps_its_members_and_nested_callables() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(
        root.join("src/lib.rs"),
        "fn root() { struct Task; trait Run { fn open(&self); } impl Task { fn open(&self) { let next = || 1; next(); } } impl Run for Task { fn open(&self) {} } }",
    )
    .expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    for (from, to) in [("/root/Task", "/Task/open"), ("/Task/open", "/open/next")] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Contains
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with(to)),
            "missing {from} -> {to}"
        );
    }
    assert!(
        index.edges().edges().any(
            |edge| edge.kind() == EdgeKind::Calls && edge.to().as_str().ends_with("/open/next")
        )
    );
}

#[test]
fn test_rust_associated_type_use_keeps_its_declared_member() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(
        root.join("src/lib.rs"),
        "mod jobs; struct Task; trait Run { type Output; fn run(&self) -> Self::Output; }",
    )
    .expect("source");
    std::fs::write(root.join("src/jobs.rs"), "use crate::{Task, Run}; impl Run for Task { type Output = u8; fn run(&self) -> Self::Output { 0 } }").expect("module source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    for target in ["/Task/Output", "/Run/Output"] {
        assert!(
            index.edges().edges().any(
                |edge| edge.kind() == EdgeKind::HasType && edge.to().as_str().ends_with(target)
            ),
            "missing {target}"
        );
    }
}

#[test]
fn test_rust_trait_defaults_and_bounded_receivers_keep_stored_targets() {
    let source = "pub trait Count { fn remaining(&self) -> usize; fn get(&self) -> usize { self.remaining() } } pub struct Item; impl Count for Item { fn remaining(&self) -> usize { 1 } } pub fn generic<T: Count>(item: &T) -> usize { item.remaining() } pub fn bounded<T>(item: &T) -> usize where T: Count { item.remaining() } pub fn shadow<T: Count>(item: &T) { let item = 1; item.remaining(); } trait Other { fn remaining(&self); } fn ambiguous<T: Count + Other>(item: &T) { item.remaining(); }";
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    let records = view.normalized_graph().records();
    assert!(
        records.iter().all(|record| record.identity().is_some()),
        "every captured declaration has an identity"
    );
    for from in ["/Count/get", "/fixture/generic", "/fixture/bounded"] {
        for kind in [EdgeKind::Calls, EdgeKind::Reads] {
            assert!(
                index.edges().edges().any(|edge| edge.kind() == kind
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/Count/remaining")),
                "missing {kind:?} {from} -> Count.remaining"
            );
        }
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Implements
                && edge.from().as_str().ends_with("/fixture/Item")
                && edge.to().as_str().ends_with("/fixture/Count"))
    );
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && ["/fixture/shadow", "/fixture/ambiguous"]
                .iter()
                .any(|from| edge.from().as_str().ends_with(from))
    }));
}

#[test]
fn test_rust_default_features_keep_active_facts_and_refuse_unknown_conditions() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['standard']\nstandard=['std']\nstd=[]\noff=[]\n").expect("manifest");
    let source = "#[cfg(feature=\"std\")]\npub fn leaf() -> u8 { 1 }\n#[cfg(not(feature=\"std\"))]\npub fn leaf() -> u8 { 2 }\npub fn useit() -> u8 { leaf() }\npub use crate::leaf as selected; fn alias_call() { selected(); }\n#[cfg(feature=\"off\")] fn inactive() { leaf(); }\n#[cfg(unknown)] fn mystery() {}\nfn unknown_call() { mystery(); }\n#[cfg(feature=\"std\")] pub mod active;\nuse crate::active::child; fn module_call() { child(); } #[cfg(feature=\"off\")] mod inactive_module; #[cfg(unknown)] mod uncertain_module;";
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    std::fs::write(root.join("src/active.rs"), "pub fn child() {}").expect("module source");
    for file in ["inactive_module.rs", "uncertain_module.rs"] {
        std::fs::write(
            root.join("src").join(file),
            "fn leaf() {} fn root() { leaf(); }",
        )
        .expect("conditional module source");
    }
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    assert!(
        view.normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for (from, to, kind) in [
        ("/fixture/useit", "/fixture/leaf", EdgeKind::Calls),
        ("/fixture/module_call", "/active/child", EdgeKind::Calls),
    ] {
        assert!(
            index.edges().edges().any(|edge| edge.kind() == kind
                && edge.from().as_str().ends_with(from)
                && edge.to().as_str().ends_with(to)),
            "missing {from} -> {to}"
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Imports
                && edge.to().as_str().ends_with("/active/child"))
    );
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && ["/fixture/inactive", "/fixture/unknown_call"]
                .iter()
                .any(|from| edge.from().as_str().ends_with(from))
    }));
    assert!(!index.edges().edges().any(|edge| {
        ["/inactive_module", "/uncertain_module"]
            .iter()
            .any(|part| edge.from().as_str().contains(part))
    }));
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Aliases
                && edge.to().as_str().ends_with("/fixture/leaf"))
    );
}

#[test]
fn test_rust_unknown_method_alternatives_do_not_select_an_active_decoy() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "struct Item; impl Item { fn method() {} } #[cfg(unknown)] impl Item { fn method() {} } fn root() { Item::method(); }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls)
    );
}

#[test]
fn test_rust_reexport_chains_keep_terminal_trait_facts() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir_all(root.join("src/inner")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub mod inner; pub use crate::inner::Count; pub struct Item; impl Count for Item { fn read(&self) -> usize { 1 } } pub fn generic<T: Count>(item: &T) -> usize { item.read() }").expect("root source");
    std::fs::write(
        root.join("src/inner/mod.rs"),
        "mod traits; pub use self::traits::Count;",
    )
    .expect("module source");
    std::fs::write(
        root.join("src/inner/traits.rs"),
        "pub trait Count { fn read(&self) -> usize; }",
    )
    .expect("trait source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    assert!(
        view.normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for kind in [EdgeKind::Implements, EdgeKind::Imports, EdgeKind::Aliases] {
        assert!(
            index.edges().edges().any(|edge| edge.kind() == kind
                && edge.to().as_str().ends_with("/fixture/inner/traits/Count")),
            "missing {kind:?} to the authored trait: {:?}",
            index
                .edges()
                .edges()
                .map(|edge| (edge.kind(), edge.from().as_str(), edge.to().as_str()))
                .collect::<Vec<_>>()
        );
    }
    for kind in [EdgeKind::Calls, EdgeKind::Reads, EdgeKind::Overrides] {
        assert!(
            index.edges().edges().any(|edge| edge.kind() == kind
                && edge
                    .to()
                    .as_str()
                    .ends_with("/fixture/inner/traits/Count/read")),
            "missing {kind:?} to the authored member"
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Aliases
                && edge.to().as_str().ends_with("/fixture/inner/traits/Count"))
            .count()
            >= 2
    );
}

#[test]
fn test_rust_reexport_cycles_and_ambiguous_entries_stay_unresolved() {
    for modules in [
        [
            ("entry.rs", "pub use crate::other::Count;"),
            ("other.rs", "pub use crate::entry::Count;"),
        ],
        [
            (
                "entry.rs",
                "pub use crate::one::Count; pub use crate::other::Count;",
            ),
            ("other.rs", "pub trait Count {}"),
        ],
    ] {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        std::fs::create_dir(root.join("src")).expect("source directory");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .expect("manifest");
        std::fs::write(root.join("src/lib.rs"), "pub mod entry; pub mod other; pub mod one { pub trait Count {} } pub use crate::entry::Count; pub struct Item; impl Count for Item {}").expect("root source");
        for (path, source) in modules {
            std::fs::write(root.join("src").join(path), source).expect("module source");
        }
        let index = WorkspaceIndex::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("index");
        assert!(
            !index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Implements)
        );
        assert!(
            !index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Aliases
                    && edge.from().as_str().ends_with("/fixture/Count"))
        );
    }
}
