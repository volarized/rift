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
                && edge.to().as_str().ends_with("/fixture/Task/run"))
    );
    assert!(
        index
            .edges()
            .edges()
            .all(|edge| !edge.from().as_str().contains("/jobs/Task")
                && !edge.to().as_str().contains("/jobs/Task")),
        "the implementation belongs to the imported type"
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

#[test]
fn test_rust_local_declared_returns_keep_stored_receivers_and_shadowing() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub struct Item { pub value: usize } impl Item { pub fn copy(&self) -> Self { Self { value: self.value } } pub fn read(&self) -> usize { self.value } pub fn use_copy(&self) -> usize { let copy = self.copy(); copy.read() } pub fn alias(&self) -> usize { let copy = self.copy(); let next = copy; next.read() } pub fn shadow(&self) { let copy = self.copy(); let copy = 0; copy.read(); } pub fn unknown(&self) { let copy = opaque(); copy.read(); } } fn opaque() -> impl Sized { 0 } pub fn make() -> Item { Item { value: 0 } } pub fn free() { let item = make(); item.read(); }").expect("source");
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
    for from in ["/Item/use_copy", "/Item/alias", "/fixture/free"] {
        for kind in [EdgeKind::Calls, EdgeKind::Reads] {
            assert!(
                index.edges().edges().any(|edge| edge.kind() == kind
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/Item/read")),
                "missing {kind:?} {from} -> Item.read"
            );
        }
    }
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && ["/Item/shadow", "/Item/unknown"]
                .iter()
                .any(|from| edge.from().as_str().ends_with(from))
            && edge.to().as_str().ends_with("/Item/read")
    }));
}

#[test]
fn test_rust_reference_and_slice_impls_keep_distinct_stored_identities() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    let source = "pub trait Count { fn read(&self) -> usize; } pub struct Item; impl<'a> Count for &'a Item { fn read(&self) -> usize { 1 } } impl Count for &mut Item { fn read(&self) -> usize { 2 } } impl Count for [u8] { fn read(&self) -> usize { 3 } } impl Count for &[u8] { fn read(&self) -> usize { 4 } }";
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    for record in view.normalized_graph().records() {
        assert!(record.identity().is_some(), "missing identity: {record:?}");
    }
    let edges = index.edges().edges().collect::<Vec<_>>();
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Implements
                && edge.to().as_str().ends_with("/fixture/Count"))
            .count(),
        4,
        "{edges:?}"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Overrides
                && edge.to().as_str().ends_with("/Count/read"))
            .count(),
        4,
        "{edges:?}"
    );
    assert!(!edges.iter().any(|edge| edge.kind() == EdgeKind::Implements
        && edge.from().as_str().ends_with("/fixture/Item")));
}

#[test]
fn test_rust_restricted_module_visibility_keeps_stored_imports() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "mod provider; mod consumer;").expect("root source");
    std::fs::write(root.join("src/provider.rs"), "pub(crate) mod nested { pub(crate) trait Count { fn read(&self); } } mod private { pub trait Hidden { fn read(&self); } }").expect("provider source");
    std::fs::write(root.join("src/consumer.rs"), "use crate::provider::nested::Count; use crate::provider::private::Hidden; fn root<T: Count>(item: &T) { item.read(); } fn denied<T: Hidden>(item: &T) { item.read(); }").expect("consumer source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for kind in [EdgeKind::Imports, EdgeKind::Calls, EdgeKind::Reads] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == kind
                    && edge.to().as_str().contains("/provider/nested/Count")),
            "missing {kind:?}: {:?}",
            index.edges().edges().collect::<Vec<_>>()
        );
    }
    assert!(!index.edges().edges().any(|edge| matches!(
        edge.kind(),
        EdgeKind::Imports | EdgeKind::Calls | EdgeKind::Reads
    )
        && edge.to().as_str().contains("/provider/private/Hidden")));
}

#[test]
fn test_rust_field_reads_keep_stored_calls_and_refuse_method_values() {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "struct Item { value: usize } impl Item { fn value(&self) -> usize { self.value } fn call(&self) { self.value(); } fn invalid(&self) { self.missing; } fn missing(&self) {} }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for from in ["/Item/value", "/Item/call"] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Reads
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/Item/value")),
            "missing read from {from}"
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with("/Item/call")
                && edge.to().as_str().ends_with("/Item/value"))
    );
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Reads
                && edge.from().as_str().ends_with("/Item/invalid"))
    );
}

#[test]
fn test_rust_macro_token_errors_keep_unaffected_stored_facts() {
    // #822: opaque macro matchers must not suppress unrelated source facts.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    let source = "#[macro_export] macro_rules! marker { (~) => {}; } pub struct Item { pub value: usize } impl Item { pub fn new() -> Self { Self { value: 1 } } pub fn read(&self) -> usize { self.value } } pub fn use_item() -> usize { let item = Item::new(); item.read() }";
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let view = index.symbol_view().expect("symbols");
    for record in view.normalized_graph().records() {
        assert!(record.identity().is_some(), "missing identity: {record:?}");
    }
    for (kind, from, to) in [
        (EdgeKind::Calls, "/fixture/use_item", "/Item/new"),
        (EdgeKind::Calls, "/fixture/use_item", "/Item/read"),
        (EdgeKind::Reads, "/Item/read", "/Item/value"),
    ] {
        assert!(
            index.edges().edges().any(|edge| edge.kind() == kind
                && edge.from().as_str().ends_with(from)
                && edge.to().as_str().ends_with(to)),
            "missing {kind:?} {from} -> {to}"
        );
    }
}

#[test]
fn test_rust_private_associated_calls_use_the_declaring_module() {
    // #824: an imported type does not change its helper's visibility scope.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub struct Item; mod methods; fn denied() { Item::hidden(1); } mod other { use crate::Item; fn denied() { Item::hidden(1); } }").expect("root source");
    std::fs::write(root.join("src/methods.rs"), "use crate::Item; impl Item { fn hidden<T>(_: T) -> Self { Item } pub fn run() -> Self { Item::hidden(1) } } mod child { use crate::Item; fn nested() { Item::hidden(1); } }").expect("module source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for from in ["/Item/run", "/child/nested"] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Calls
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/Item/hidden")),
            "missing helper call from {from}"
        );
    }
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with("/denied")
                && edge.to().as_str().ends_with("/Item/hidden"))
    );
}

#[test]
fn test_rust_reexport_imports_keep_the_written_module() {
    // #828: keep the authored module and terminal declaration as separate facts.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub struct Item; mod bridge; mod client; mod denied { use crate::closed::Item; } mod closed { use crate::Item; }").expect("root source");
    std::fs::write(root.join("src/bridge.rs"), "pub(crate) use crate::Item;")
        .expect("bridge source");
    std::fs::write(
        root.join("src/client.rs"),
        "use crate::bridge::Item; pub fn use_item(_: Item) {}",
    )
    .expect("client source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for (from, to) in [
        ("/fixture/bridge", "/fixture/Item"),
        ("/fixture/client", "/fixture/bridge"),
    ] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Imports
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with(to)),
            "missing {from} -> {to}"
        );
    }
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Imports
                && edge.from().as_str().ends_with("/fixture/denied"))
    );
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Aliases
                && edge.from().as_str().ends_with("/bridge/Item")
                && edge.to().as_str().ends_with("/fixture/Item")),
        "missing reexport alias"
    );
}

#[test]
fn test_rust_function_pointer_results_keep_declared_member_owners() {
    // #832: use declared field returns and subsequent method returns.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    let source = "pub struct Item { pub value: usize } impl Item { pub fn read(self) -> usize { self.value } pub fn copy(self) -> Item { self } } pub struct Table { pub make: fn() -> Item } pub struct Opaque { pub make: fn() -> Unknown } pub struct Other; impl Other { pub fn read(self) -> usize { 0 } } pub struct OtherTable { pub make: fn() -> Other } mod secrets { use crate::Item; pub struct SecretTable { make: fn() -> Item } } pub fn private(table: &secrets::SecretTable) -> usize { (table.make)().read() } pub fn run(table: &Table) -> usize { (table.make)().read() } pub fn chain(table: &Table) -> usize { (table.make)().copy().read() } pub fn local(table: &Table) -> usize { let item = (table.make)(); item.read() } pub fn unknown(table: &Opaque) { (table.make)().read(); } pub fn method_value(table: &Table) { table.make.read(); } pub fn other(table: &OtherTable) -> usize { (table.make)().read() } pub fn shadow(table: &Table) { let table = 0; (table.make)().read(); }";
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    for from in ["/fixture/run", "/fixture/chain", "/fixture/local"] {
        for kind in [EdgeKind::Calls, EdgeKind::Reads] {
            assert!(
                index.edges().edges().any(|edge| edge.kind() == kind
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/Item/read")),
                "missing {kind:?} {from} -> Item.read"
            );
        }
    }
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && [
                "/fixture/unknown",
                "/fixture/shadow",
                "/fixture/method_value",
                "/fixture/private",
                "/fixture/other",
            ]
            .iter()
            .any(|from| edge.from().as_str().ends_with(from))
            && edge.to().as_str().ends_with("/Item/read")
    }));
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with("/fixture/other")
                && edge.to().as_str().ends_with("/Other/read")),
        "other field keeps its own return owner"
    );
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Reads
                && edge.from().as_str().ends_with("/fixture/private")),
        "private fields stay refused outside their declaration module"
    );
}

#[test]
fn test_rust_local_traits_take_precedence_over_glob_imports() {
    // #833: local declarations hide wildcard imports in the type namespace.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub struct Item; pub use std::error::Error as Bound; mod inner { use super::*; pub trait Bound { fn run(&self); } impl Bound for Item { fn run(&self) {} } } mod other { pub trait Bound {} } mod ambiguous { use crate::inner::*; use crate::other::*; impl Bound for crate::Item {} } mod conflict { use crate::other::Bound; pub trait Bound {} impl Bound for crate::Item {} }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    let file = index
        .files()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .expect("source file");
    let declared = file
        .syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == "inner::Bound")
        .expect("local trait");
    assert_eq!(declared.container.as_deref(), Some("inner"));
    let source = std::fs::read_to_string(root.join("src/lib.rs")).expect("source");
    assert_eq!(
        &source[usize::try_from(declared.item_range.start).expect("start")
            ..usize::try_from(declared.item_range.end).expect("end")],
        "pub trait Bound { fn run(&self); }"
    );
    for (kind, from, to) in [
        (EdgeKind::Implements, "/fixture/Item", "/inner/Bound"),
        (EdgeKind::Overrides, "/Item/run", "/inner/Bound/run"),
    ] {
        assert!(
            index.edges().edges().any(|edge| edge.kind() == kind
                && edge.from().as_str().ends_with(from)
                && edge.to().as_str().ends_with(to)),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Implements)
            .count(),
        1
    );
}

#[test]
fn test_rust_tuple_impls_keep_structural_identities_and_calls() {
    // #835: tuple owners preserve order and do not become element impls.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    std::fs::write(root.join("src/lib.rs"), "pub trait Pair { fn total(&self) -> usize; } impl Pair for (usize, u8) { fn total(&self) -> usize { 1 } } impl Pair for (u8, usize) { fn total(&self) -> usize { 2 } } impl Pair for (usize,) { fn total(&self) -> usize { 3 } } impl Pair for () { fn total(&self) -> usize { 4 } } pub fn one(value: &(usize,)) -> usize { value.total() } pub fn unit(value: &()) -> usize { value.total() } pub fn use_pair(value: &(usize, u8)) -> usize { value.total() } pub fn reverse(value: &(u8, usize)) -> usize { value.total() } pub fn denied(value: usize) { value.total(); }").expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    for record in index
        .symbol_view()
        .expect("symbols")
        .normalized_graph()
        .records()
    {
        assert!(record.identity().is_some(), "missing identity: {record:?}");
    }
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Implements
                && edge.to().as_str().ends_with("/fixture/Pair"))
            .count(),
        4
    );
    assert_eq!(
        index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Overrides
                && edge.to().as_str().ends_with("/Pair/total"))
            .count(),
        4
    );
    for from in [
        "/fixture/use_pair",
        "/fixture/reverse",
        "/fixture/one",
        "/fixture/unit",
    ] {
        assert!(
            index
                .edges()
                .edges()
                .any(|edge| edge.kind() == EdgeKind::Calls
                    && edge.from().as_str().ends_with(from)
                    && edge.to().as_str().ends_with("/total")),
            "missing tuple call from {from}"
        );
    }
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.from().as_str().ends_with("/fixture/denied"))
    );
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Implements
            && ["/fixture/usize", "/fixture/u8"]
                .iter()
                .any(|suffix| edge.from().as_str().ends_with(suffix))
    }));
}

#[test]
fn test_rust_multiline_member_reads_keep_captured_receiver_ranges() {
    // #832: member reads use the same authored receiver as their calls.
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::create_dir(root.join("src")).expect("source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .expect("manifest");
    let source = "pub struct Item; pub struct Next; impl Item { pub fn next(self) -> Next { Next } } impl Next { pub fn read(self) -> usize { 1 } } pub struct Table { pub make: fn() -> Item } pub fn run(table: &Table) -> usize { (table.make)() /* first */\n .next() /* second */\n .read() } pub fn denied(table: &Table) { let table = 0; (table.make)() /* shadow */\n .next() /* shadow */\n .read(); }";
    std::fs::write(root.join("src/lib.rs"), source).expect("source");
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    assert!(
        index
            .symbol_view()
            .expect("symbols")
            .normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some())
    );
    let file = index
        .files()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .expect("source file");
    let facts = file.syntax().static_facts().expect("static facts");
    let text = |range: rift_syntax::ByteRange| {
        &source
            [usize::try_from(range.start).expect("start")..usize::try_from(range.end).expect("end")]
    };
    let call = facts.calls().iter().find(|call| matches!(call.callee, rift_syntax::StaticCallee::Member { member, .. } if text(member) == "read") && text(call.range).contains("/* second */")).expect("captured member call");
    let rift_syntax::StaticCallee::Member { receiver, .. } = call.callee else {
        unreachable!("member call");
    };
    assert_eq!(text(receiver), "(table.make)() /* first */\n .next()");
    for (target, read) in [
        ("/Item/next", "(table.make)() /* first */\n .next"),
        (
            "/Next/read",
            "(table.make)() /* first */\n .next() /* second */\n .read",
        ),
    ] {
        for kind in [EdgeKind::Calls, EdgeKind::Reads] {
            let edge = index
                .edges()
                .edges()
                .find(|edge| {
                    edge.kind() == kind
                        && edge.from().as_str().ends_with("/fixture/run")
                        && edge.to().as_str().ends_with(target)
                })
                .unwrap_or_else(|| panic!("missing {kind:?} -> {target}"));
            let expected = if kind == EdgeKind::Calls {
                format!("{read}()")
            } else {
                read.to_owned()
            };
            let range = edge.occurrence().range();
            assert_eq!(
                &source[usize::try_from(range.start()).expect("start")
                    ..usize::try_from(range.end()).expect("end")],
                expected
            );
        }
    }
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.from().as_str().ends_with("/fixture/denied")
                && matches!(edge.kind(), EdgeKind::Calls | EdgeKind::Reads))
    );
}

fn fixture_index(manifest: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceIndex) {
    let directory = tempfile::tempdir().expect("workspace");
    let root = directory.path();
    std::fs::write(root.join("Cargo.toml"), manifest).expect("manifest");
    for (path, source) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("source directory");
        std::fs::write(path, source).expect("source");
    }
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    (directory, index)
}

fn has_edge(index: &WorkspaceIndex, kind: EdgeKind, from: &str, to: &str) -> bool {
    index.edges().edges().any(|edge| {
        edge.kind() == kind
            && edge.from().as_str().ends_with(from)
            && edge.to().as_str().ends_with(to)
    })
}

#[test]
fn test_rust_alternative_path_modules_keep_every_declaration_identity() {
    // Alternative `#[path]` files of one module keep searchable declarations; only the
    // admitted alternative contributes resolved facts.
    let manifest = "[package]\nname='fixture'\nversion='0.1.0'\n[features]\nalternate=[]\n";
    let library = "#[cfg(feature = \"alternate\")]\n#[path = \"other.rs\"]\nmod imp;\n#[cfg(not(feature = \"alternate\"))]\n#[path = \"default.rs\"]\nmod imp;\n#[cfg(not(feature = \"alternate\"))]\n#[path = \"first.rs\"]\nmod pick;\n#[cfg(feature = \"alternate\")]\n#[path = \"second.rs\"]\nmod pick;\npub mod item { pub struct Item; }\npub fn run() -> u8 { imp::read() + pick::read() }\n";
    let module =
        "pub struct Item { pub value: u8 }\npub fn read() -> u8 { Item { value: 1 }.value }\n";
    let (_directory, index) = fixture_index(
        manifest,
        &[
            ("src/lib.rs", library),
            ("src/other.rs", module),
            ("src/default.rs", module),
            ("src/first.rs", module),
            ("src/second.rs", module),
        ],
    );
    let view = index.symbol_view().expect("symbols");
    for record in view.normalized_graph().records() {
        assert!(record.identity().is_some(), "missing identity: {record:?}");
    }
    let language = view
        .files()
        .next()
        .expect("file")
        .syntax()
        .language()
        .clone();
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("coverage")
    );
    for file in [
        "src/other.rs",
        "src/default.rs",
        "src/first.rs",
        "src/second.rs",
    ] {
        let declared = view
            .files()
            .find(|candidate| candidate.path().as_str() == file)
            .expect("module file");
        let symbols = declared.syntax().symbols();
        assert!(!symbols.is_empty(), "{file}");
        assert!(
            (0..symbols.len())
                .all(|position| view.symbol_identity_at(declared.path(), position).is_some()),
            "{file} declarations lack identities"
        );
    }
    for (from, to) in [
        ("/fixture/run", "/imp/read"),
        ("/fixture/run", "/pick/read"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "missing {from} -> {to}"
        );
    }
    // Inactive alternatives contribute no resolved facts, and the unrelated `item::Item`
    // keeps its own identity.
    for edge in index.edges().edges() {
        let unit = edge.occurrence().unit().to_string();
        assert!(
            !unit.ends_with("other.rs") && !unit.ends_with("second.rs"),
            "fact from an inactive alternative: {edge:?}"
        );
        assert!(!edge.to().as_str().ends_with("/item/Item") || edge.kind() == EdgeKind::Contains);
    }
}

const PLAIN_MANIFEST: &str = "[package]\nname='fixture'\nversion='0.1.0'\n";

#[test]
fn test_rust_reference_receivers_reach_the_referent_members() {
    // A method of an impl for `&mut Reader` reaches `Reader`'s methods and fields
    // through its receiver. `both` follows the probing order: the implementation's `&self`
    // method (receiver `&&mut Reader`) comes before `Reader`'s `&self` method after a
    // dereference. `Other::step` and the unknown `missing` are never targets.
    let source = "pub trait Visit { fn visit(self) -> u8; fn both(&self) -> u8; }\npub struct Reader { pub count: u8 }\npub struct Other { pub count: u8 }\nimpl Reader { fn step(&mut self) -> u8 { self.count } fn both(&self) -> u8 { 0 } }\nimpl Other { fn step(&self) -> u8 { self.count } }\nimpl<'a> Visit for &'a mut Reader {\n    fn visit(self) -> u8 { self.step() + self.count + self.both() + self.missing() }\n    fn both(&self) -> u8 { 1 }\n}\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, to) in [
        (EdgeKind::Calls, "/Reader/step"),
        (EdgeKind::Reads, "/Reader/step"),
        (EdgeKind::Reads, "/Reader/count"),
        (EdgeKind::Calls, "/&mut%20Reader/both"),
    ] {
        assert!(
            has_edge(&index, kind, "/visit", to),
            "missing {kind:?} visit -> {to}"
        );
    }
    let visit_targets = index
        .edges()
        .edges()
        .filter(|edge| {
            edge.from().as_str().ends_with("/visit")
                && matches!(edge.kind(), EdgeKind::Calls | EdgeKind::Reads)
        })
        .map(|edge| edge.to().as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(
        visit_targets
            .iter()
            .all(|to| to.ends_with("/fixture/Reader/step")
                || to.ends_with("/fixture/Reader/count")
                || to.ends_with("/fixture/&mut%20Reader/both")),
        "{visit_targets:?}"
    );
}

#[test]
fn test_rust_associated_types_stay_outside_lexical_scope() {
    // `Error` in a method signature names the module item; only `Self::Error` names the
    // implementation's associated type.
    let source = "pub struct Error;\npub trait Source { type Error; fn read(&self) -> Result<u8, Self::Error>; fn other(&self) -> Self::Error; }\npub struct Item;\nimpl Source for Item {\n    type Error = Error;\n    fn read(&self) -> Result<u8, Error> { Err(Error) }\n    fn other(&self) -> Self::Error { Error }\n}\npub fn local() { struct Error; let _value: Error = Error; }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    let types = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::HasType)
        .map(|edge| {
            (
                edge.from().as_str().to_owned(),
                edge.to().as_str().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let read = types
        .iter()
        .filter(|(from, _)| from.ends_with("/read") && !from.contains("/Source/"))
        .map(|(_, to)| to.as_str())
        .collect::<Vec<_>>();
    assert!(
        read.iter().any(|to| to.ends_with("/fixture/Error")),
        "{types:?}"
    );
    assert!(
        !read
            .iter()
            .any(|to| to.ends_with("/Error") && !to.ends_with("/fixture/Error")),
        "{types:?}"
    );
    assert!(
        types
            .iter()
            .any(|(from, to)| from.ends_with("/local") && to.ends_with("/local/Error")),
        "{types:?}"
    );
}

#[test]
fn test_rust_inactive_alternative_modules_are_never_endpoints() {
    // `pub use self::imp::Write` names `std::io` under the default feature; the inactive
    // `#[path]` alternative is not a local endpoint for implementations or type uses.
    let manifest =
        "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['std']\nstd=[]\n";
    let io = "pub use self::imp::Write;\npub use self::local::Flush;\n#[cfg(not(feature = \"std\"))]\n#[path = \"core.rs\"]\nmod imp;\n#[cfg(feature = \"std\")]\nuse std::io as imp;\n#[cfg(feature = \"std\")]\nmod local;\n";
    let library = "pub mod io;\npub struct Sink;\nimpl io::Write for Sink { fn write(&mut self, _: &[u8]) -> std::io::Result<usize> { Ok(0) } fn flush(&mut self) -> std::io::Result<()> { Ok(()) } }\nimpl io::Flush for Sink { fn flush(&mut self) {} }\npub fn run(_writer: &mut dyn io::Write, _flush: &dyn io::Flush) {}\n";
    let (_directory, index) = fixture_index(
        manifest,
        &[
            ("src/lib.rs", library),
            ("src/io/mod.rs", io),
            (
                "src/io/core.rs",
                "pub trait Write { fn write(&mut self, buf: &[u8]) -> usize; fn flush(&mut self); }\n",
            ),
            (
                "src/io/local.rs",
                "pub trait Flush { fn flush(&mut self); }\n",
            ),
        ],
    );
    assert!(has_edge(
        &index,
        EdgeKind::Implements,
        "/fixture/Sink",
        "/local/Flush"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::HasType,
        "/fixture/run",
        "/local/Flush"
    ));
    for edge in index.edges().edges() {
        assert!(
            !edge.to().as_str().contains("/imp/") || edge.from().as_str().contains("/imp/"),
            "inactive alternative endpoint: {edge:?}"
        );
    }
}

#[test]
fn test_rust_enclosing_trait_implementation_selects_its_member() {
    // Inside `impl t::A for Error`, `Error::make` names A's member even though `t::B` also
    // gives `Error` a `make`; outside any implementation the two stay ambiguous.
    let source = "pub mod t { pub trait A { fn make(value: u8) -> Self; fn wrap(value: u8) -> Self; } pub trait B { fn make(value: u8) -> Self; } }\npub struct Error;\nimpl t::A for Error { fn make(_: u8) -> Self { Error } fn wrap(value: u8) -> Self { Error::make(value) } }\nimpl t::B for Error { fn make(_: u8) -> Self { Error } }\nmod outside { use super::t::B; pub fn build() -> super::Error { super::Error::make(1) } }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Error/wrap",
        "/Error/make"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls
            && edge.from().as_str().ends_with("/outside/build")
            && edge.to().as_str().ends_with("/make")
    }));
}

#[test]
fn test_rust_struct_expressions_name_body_local_structs_first() {
    let source = "pub struct Adapter { pub value: u8 }\npub fn collect() -> u8 { struct Adapter { value: u8 } let adapter = Adapter { value: 1 }; adapter.value }\npub fn module_level() -> u8 { Adapter { value: 2 }.value }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/collect",
        "/collect/Adapter"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/module_level",
        "/fixture/Adapter"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/collect",
        "/fixture/Adapter"
    ));
}

#[test]
fn test_rust_trait_path_calls_select_the_argument_implementation() {
    // `fmt::Display::fmt(&self.inner, f)` names the Display implementation of the first
    // argument's type; a trait the argument type does not implement, or an unknown
    // argument, stays unresolved.
    let source = "use std::fmt;\npub struct Inner;\npub struct Reader { pub inner: Inner }\nimpl fmt::Display for Inner { fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) } }\nimpl fmt::Debug for Inner { fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result { Ok(()) } }\nimpl fmt::Display for Reader { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.inner, f) } }\nimpl Reader {\n    pub fn debug(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Debug::fmt(&(self.inner), f) }\n    pub fn pointer(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Pointer::fmt(&self.inner, f) }\n    pub fn unknown(&self, value: u8, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&value, f) }\n}\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Reader/fmt",
        "/Inner/fmt"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/Reader/debug",
        "/Inner/fmt"
    ));
    // No local implementation answers them; the trait method itself is a standard-library
    // target (D23), spelled through `use std::fmt`.
    for (from, spelled) in [
        (
            "/Reader/pointer",
            "rift://symbol/stdlib/rust/rust/std/fmt/Pointer/fmt",
        ),
        (
            "/Reader/unknown",
            "rift://symbol/stdlib/rust/rust/std/fmt/Display/fmt",
        ),
    ] {
        let calls = index
            .edges()
            .edges()
            .filter(|edge| edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with(from))
            .map(|edge| edge.to().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(calls, [spelled], "{from}");
    }
}

#[test]
fn test_rust_reference_receivers_follow_the_probing_order() {
    // In `impl Source for &Value`, `self.read()` (self: &Value) selects the reference
    // implementation's by-value `read` before `Value`'s by-value `read`; `self.peek()` finds
    // `Value`'s `&self` method first.
    let source = "pub trait Source { fn read(self) -> u8; fn copy(self) -> u8; }\npub struct Value;\nimpl Value { pub fn peek(&self) -> u8 { 0 } }\nimpl Source for Value { fn read(self) -> u8 { 1 } fn copy(self) -> u8 { self.read() } }\nimpl<'a> Source for &'a Value { fn read(self) -> u8 { self.peek() } fn copy(self) -> u8 { self.read() } }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    let calls = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::Calls)
        .map(|edge| {
            (
                edge.from().as_str().to_owned(),
                edge.to().as_str().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let reference_copy = calls
        .iter()
        .filter(|(from, _)| from.ends_with("/copy") && from.contains('&'))
        .map(|(_, to)| to.as_str())
        .collect::<Vec<_>>();
    assert_eq!(reference_copy.len(), 1, "{calls:?}");
    assert!(
        reference_copy[0].contains('&') && reference_copy[0].ends_with("/read"),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|(from, to)| from.ends_with("/read")
            && from.contains('&')
            && to.ends_with("/Value/peek")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|(from, to)| from.ends_with("/Value/copy") && to.ends_with("/Value/read")),
        "{calls:?}"
    );
}

#[test]
fn test_rust_reference_receivers_see_implementations_in_other_modules() {
    // `self: &Map` in a module other than the one implementing `Walk for &Map` still finds
    // that implementation's by-value `walk` before `Map`'s by-value `walk`.
    let source = "pub trait Walk { fn walk(self) -> u8; }\npub struct Map;\nimpl Walk for Map { fn walk(self) -> u8 { 1 } }\nimpl<'a> Walk for &'a Map { fn walk(self) -> u8 { 2 } }\npub mod de {\n    pub trait Read { fn read(self) -> u8; }\n    impl<'a> Read for &'a super::Map { fn read(self) -> u8 { use super::Walk; self.walk() } }\n}\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    let targets = index
        .edges()
        .edges()
        .filter(|edge| edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("/read"))
        .map(|edge| edge.to().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].ends_with("/fixture/&Map/walk"), "{targets:?}");
}

#[test]
fn test_rust_closure_parameter_patterns_bind_their_names() {
    // `|Item { value, .. }|` binds `value` in the closure; the struct expression inside
    // still names `Item`. A pattern name shadows a module function of that name, and a
    // tuple struct pattern does not bind its own type name.
    let source = "pub struct Item { pub value: u8, pub key: u8 }\npub struct Wrapper(pub u8);\npub fn helper() -> u8 { 1 }\npub fn value() -> u8 { 2 }\npub fn convert(items: Vec<Item>) -> Vec<Item> { items.into_iter().map(|Item { value, .. }| Item { value, key: 0 }).collect() }\npub fn shadow(pairs: Vec<(fn() -> u8, u8)>) -> Vec<u8> { pairs.into_iter().map(|(helper, _)| helper()).collect() }\npub fn shorthand(items: Vec<Item>) -> u8 { let Item { value, .. } = items[0]; value; 0 }\npub fn wrap(items: Vec<Wrapper>) -> Vec<Wrapper> { items.into_iter().map(|Wrapper(inner)| Wrapper(inner)).collect() }\npub fn mutable(items: Vec<u8>) -> Vec<u8> { items.into_iter().map(|mut item| { item += helper(); item }).collect() }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (from, to) in [
        ("/fixture/convert", "/fixture/Item"),
        ("/fixture/wrap", "/fixture/Wrapper"),
        ("/fixture/mutable", "/fixture/helper"),
    ] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, to),
            "missing {from} -> {to}"
        );
    }
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/shadow",
        "/fixture/helper"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Reads,
        "/fixture/shorthand",
        "/fixture/value"
    ));
}

#[test]
fn test_rust_tuple_struct_constructors_are_calls() {
    // `Wrapper(1)` calls the tuple struct's constructor; a body-local tuple struct shadows the
    // module one, and a local value of that name is not a constructor.
    let source = "pub struct Wrapper(pub u8);\npub fn build() -> Wrapper { Wrapper(1) }\npub fn local() -> u8 { struct Wrapper(u8); let inner = Wrapper(2); inner.0 }\npub fn value(make: fn(u8) -> u8) -> u8 { let Wrapper = make; Wrapper(3) }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/build",
        "/fixture/Wrapper"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/local",
        "/local/Wrapper"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/local",
        "/fixture/Wrapper"
    ));
    assert!(!index.edges().edges().any(|edge| {
        edge.kind() == EdgeKind::Calls && edge.from().as_str().ends_with("/fixture/value")
    }));
}

/// The conditional item chain macro as the cfg-if crate defines it, copied into a crate.
const CONDITIONAL_CHAIN_MACRO: &str = "// Copied from the cfg-if crate.\n#[allow(unused_macro_rules)]\nmacro_rules! cfg_if {\n    ($(\n        if #[cfg($($meta:meta),*)] { $($it:item)* }\n    ) else * else {\n        $($it2:item)*\n    }) => {\n        cfg_if! {\n            @__items\n            () ;\n            $( ( ($($meta),*) ($($it)*) ), )*\n            ( () ($($it2)*) ),\n        }\n    };\n    (\n        if #[cfg($($i_met:meta),*)] { $($i_it:item)* }\n        $(\n            else if #[cfg($($e_met:meta),*)] { $($e_it:item)* }\n        )*\n    ) => {\n        cfg_if! {\n            @__items\n            () ;\n            ( ($($i_met),*) ($($i_it)*) ),\n            $( ( ($($e_met),*) ($($e_it)*) ), )*\n            ( () () ),\n        }\n    };\n    (@__items ($($not:meta,)*) ; ) => {};\n    (@__items ($($not:meta,)*) ; ( ($($m:meta),*) ($($it:item)*) ), $($rest:tt)*) => {\n        cfg_if! { @__apply cfg(all($($m,)* not(any($($not),*)))), $($it)* }\n        cfg_if! { @__items ($($not,)* $($m,)*) ; $($rest)* }\n    };\n    (@__apply $m:meta, $($it:item)*) => {\n        $(#[$m] $it)*\n    };\n}\n";

#[test]
fn test_rust_modules_declared_by_a_conditional_chain_keep_identities() {
    // `mod` items in the branches of a local conditional item chain declare modules: every
    // file keeps its identities, and only the branch the configuration selects contributes
    // resolved facts. An unknown condition admits no branch after it; an invocation of a
    // macro that is not defined locally declares nothing.
    let manifest = "[package]\nname='fixture'\nversion='0.1.0'\n[features]\nalt=[]\n";
    let library =
        "#[macro_use]\nmod macros;\nmod group;\nmod chain;\nmod other;\npub fn run() -> u8 { 0 }\n";
    let group = "cfg_if! {\n    if #[cfg(feature = \"alt\")] {\n        mod alt;\n        use alt as imp;\n    } else {\n        mod base;\n        use base as imp;\n    }\n}\n";
    let chain = "cfg_if! {\n    if #[cfg(custom)] {\n        mod first;\n    } else if #[cfg(feature = \"alt\")] {\n        mod second;\n    } else {\n        mod third;\n    }\n}\n";
    let other = "undefined! {\n    if #[cfg(feature = \"alt\")] {\n        mod absent;\n    }\n}\npub fn plain() -> u8 { 1 }\n";
    let module =
        "pub struct Item { pub value: u8 }\npub fn read() -> u8 { Item { value: 1 }.value }\n";
    let files = [
        ("src/lib.rs", library),
        ("src/macros.rs", CONDITIONAL_CHAIN_MACRO),
        ("src/group/mod.rs", group),
        ("src/group/alt.rs", module),
        ("src/group/base.rs", module),
        ("src/chain/mod.rs", chain),
        ("src/chain/first.rs", module),
        ("src/chain/second.rs", module),
        ("src/chain/third.rs", module),
        ("src/other.rs", other),
    ];
    let (_directory, index) = fixture_index(manifest, &files);
    let view = index.symbol_view().expect("symbols");
    for record in view.normalized_graph().records() {
        assert!(record.identity().is_some(), "missing identity: {record:?}");
    }
    let language = view
        .files()
        .next()
        .expect("file")
        .syntax()
        .language()
        .clone();
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("coverage")
    );
    for (file, _) in files
        .iter()
        .filter(|(path, _)| path.contains("/group/") || path.contains("/chain/"))
    {
        let declared = view
            .files()
            .find(|candidate| candidate.path().as_str() == *file)
            .expect("module file");
        let symbols = declared.syntax().symbols();
        assert!(
            (0..symbols.len())
                .all(|position| view.symbol_identity_at(declared.path(), position).is_some()),
            "{file} declarations lack identities"
        );
    }
    assert!(has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/group/base/read",
        "/fixture/group/base/Item"
    ));
    for edge in index.edges().edges() {
        let unit = edge.occurrence().unit().to_string();
        assert!(
            !unit.ends_with("group/alt.rs") && !unit.contains("src/chain/"),
            "fact from a branch the configuration does not select: {edge:?}"
        );
        assert!(
            !edge.to().as_str().contains("/group/alt/") && !edge.to().as_str().contains("/chain/"),
            "endpoint in a branch the configuration does not select: {edge:?}"
        );
    }
}

#[test]
fn test_rust_reexports_of_crate_visible_items_keep_their_scope() {
    // A re-export of a `pub(crate)` or `pub(super)` item, at a visibility the item allows,
    // is followed wherever the re-exported name is visible, through nested use trees too.
    // A path from outside the re-export's own visibility stays unresolved.
    let control = "mod inner;\nmod tag;\npub(crate) use self::{inner::Helper, tag::{Open, Tag}};\npub(super) use self::inner::Shared;\npub(in crate::control) use self::inner::Local;\npub(crate) mod user;\n";
    let tag = "pub(crate) struct Tag(pub(super) u8);\nimpl Tag {\n    pub(crate) fn full(hash: u64) -> Tag { Tag(hash as u8) }\n    pub(crate) fn special(self) -> bool { self.0 == 0 }\n}\npub struct Open(pub u8);\nimpl Open {\n    pub fn full(hash: u64) -> Open { Open(hash as u8) }\n    pub fn special(self) -> bool { self.0 == 0 }\n}\n";
    let inner = "pub(crate) struct Helper;\nimpl Helper { pub(crate) fn make() -> u8 { 1 } }\npub(crate) struct Shared;\nimpl Shared { pub(crate) fn make() -> u8 { 2 } }\npub(super) struct Local;\nimpl Local { pub(super) fn make() -> u8 { 3 } }\n";
    let user = "use super::Local;\npub(crate) fn local() -> u8 { Local::make() }\n";
    let consumer = "use crate::control::{Helper, Open, Shared, Tag};\npub(crate) fn crate_visible(hash: u64, old: Tag) -> bool { Tag::full(hash); old.special() }\npub fn public(hash: u64, old: Open) -> bool { Open::full(hash); old.special() }\npub(crate) fn nested() -> u8 { Helper::make() + Shared::make() + crate::control::user::local() }\npub(crate) fn refused() -> u8 { crate::control::Local::make() }\n";
    let (_directory, index) = fixture_index(
        PLAIN_MANIFEST,
        &[
            ("src/lib.rs", "mod consumer;\nmod control;\n"),
            ("src/control/mod.rs", control),
            ("src/control/tag.rs", tag),
            ("src/control/inner.rs", inner),
            ("src/control/user.rs", user),
            ("src/consumer.rs", consumer),
        ],
    );
    for (kind, from, to) in [
        (
            EdgeKind::Aliases,
            "/fixture/control/Tag",
            "/control/tag/Tag",
        ),
        (
            EdgeKind::Aliases,
            "/fixture/control/Helper",
            "/control/inner/Helper",
        ),
        (
            EdgeKind::Aliases,
            "/fixture/control/Shared",
            "/control/inner/Shared",
        ),
        (
            EdgeKind::Calls,
            "/consumer/crate_visible",
            "/control/tag/Tag/full",
        ),
        (
            EdgeKind::Calls,
            "/consumer/crate_visible",
            "/control/tag/Tag/special",
        ),
        (
            EdgeKind::Reads,
            "/consumer/crate_visible",
            "/control/tag/Tag/special",
        ),
        (
            EdgeKind::Calls,
            "/consumer/public",
            "/control/tag/Open/full",
        ),
        (
            EdgeKind::Calls,
            "/consumer/nested",
            "/control/inner/Helper/make",
        ),
        (
            EdgeKind::Calls,
            "/consumer/nested",
            "/control/inner/Shared/make",
        ),
        (
            EdgeKind::Calls,
            "/control/user/local",
            "/control/inner/Local/make",
        ),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(
        index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Imports
                && edge.from().as_str().contains("consumer")
                && edge.to().as_str().ends_with("/control/tag/Tag")),
        "missing import of the re-exported Tag"
    );
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/consumer/refused",
        "/control/inner/Local/make"
    ));
}

#[test]
fn test_rust_match_arm_guards_keep_calls_and_reads() {
    // Calls, method calls and field reads in a match arm guard belong to the enclosing
    // function, as in the arm body. A name the arm pattern binds shadows an outer function
    // of that name in the guard.
    let source = "pub enum Kind { Special, Plain }\nimpl Kind { pub fn is_special(&self) -> bool { matches!(self, Kind::Special) } }\npub fn alpha(c: char) -> bool { c.is_ascii_alphabetic() }\npub fn shadow(_: u8) -> bool { false }\npub struct Input { pub pos: usize }\nimpl Input {\n    pub fn guard_call(&self, kind: Kind, c: char) -> bool {\n        match c {\n            'a' if alpha(c) => true,\n            '\\\\' if kind.is_special() && self.pos > 0 => true,\n            _ => false,\n        }\n    }\n    pub fn guard_shadow(&self, value: Option<fn(u8) -> bool>) -> bool {\n        match value {\n            Some(shadow) if shadow(1) => true,\n            _ => false,\n        }\n    }\n    pub fn body_call(&self, kind: Kind, c: char) -> bool {\n        match c {\n            'a' => alpha(c),\n            _ => kind.is_special(),\n        }\n    }\n}\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/Input/guard_call", "/fixture/alpha"),
        (EdgeKind::Calls, "/Input/guard_call", "/Kind/is_special"),
        (EdgeKind::Reads, "/Input/guard_call", "/Kind/is_special"),
        (EdgeKind::Reads, "/Input/guard_call", "/Input/pos"),
        (EdgeKind::Calls, "/Input/body_call", "/fixture/alpha"),
        (EdgeKind::Calls, "/Input/body_call", "/Kind/is_special"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/Input/guard_shadow",
        "/fixture/shadow"
    ));
}

#[test]
fn test_rust_paths_with_generic_arguments_name_the_plain_path() {
    // `Holder::<A>::new()` names the same declaration as `Holder::new()`; a local it
    // initializes keeps that type for later member calls. A qualified `<Holder<A>>::new()`
    // and a path from the crate root select the same targets.
    let source = "pub trait Array { fn size() -> usize; }\npub struct Holder<A: Array> { pub value: Option<A> }\nimpl<A: Array> Holder<A> {\n    pub const EMPTY: usize = 0;\n    pub fn new() -> Self { Holder { value: None } }\n    pub fn grow(&mut self) -> usize { 0 }\n    pub fn fresh() -> usize {\n        let mut v = Holder::<A>::new();\n        v.grow() + Holder::<A>::EMPTY\n    }\n    pub fn qualified() -> usize {\n        let mut v = <Holder<A>>::new();\n        v.grow()\n    }\n    pub fn plain() -> usize {\n        let mut v = Self::new();\n        v.grow()\n    }\n}\npub mod inner { pub struct Other; impl Other { pub fn new() -> Self { Other } } }\npub fn nested<A: Array>() -> usize { let _ = inner::Other::new(); let mut v = crate::Holder::<A>::new(); v.grow() }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/Holder/fresh", "/Holder/new"),
        (EdgeKind::Calls, "/Holder/fresh", "/Holder/grow"),
        (EdgeKind::Reads, "/Holder/fresh", "/Holder/grow"),
        (EdgeKind::Calls, "/Holder/qualified", "/Holder/new"),
        (EdgeKind::Calls, "/Holder/qualified", "/Holder/grow"),
        (EdgeKind::Calls, "/Holder/plain", "/Holder/new"),
        (EdgeKind::Calls, "/Holder/plain", "/Holder/grow"),
        (EdgeKind::Calls, "/fixture/nested", "/Holder/new"),
        (EdgeKind::Calls, "/fixture/nested", "/Holder/grow"),
        (EdgeKind::Calls, "/fixture/nested", "/inner/Other/new"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(!has_edge(
        &index,
        EdgeKind::Calls,
        "/fixture/nested",
        "/fixture/Holder/new/inner"
    ));
}

#[test]
fn test_rust_derived_clones_keep_the_receiver_type() {
    // `value.clone()` returns the receiver's type (`T` when it implements `Clone`, else the
    // reference `&T`), so a local it initializes keeps the type's members. A local `clone`
    // method takes precedence with its declared return. A struct expression is a value of
    // its struct.
    let source = "#[derive(Clone, Debug)]\npub struct Input { pub pos: usize }\nimpl Input {\n    pub fn step(&mut self) -> Option<char> { self.pos += 1; None }\n    pub fn cloned(&self) -> Option<char> { let mut copy = self.clone(); copy.step() }\n    pub fn argument(input: &Input) -> Option<char> { let mut copy = input.clone(); copy.step() }\n}\npub struct Plain { pub pos: usize }\nimpl Plain {\n    pub fn step(&self) -> usize { self.pos }\n    pub fn cloned(&self) -> usize { let mut copy = self.clone(); copy.step() }\n}\npub struct Other;\npub struct Custom;\nimpl Custom { pub fn clone(&self) -> Other { Other } }\nimpl Other { pub fn step(&self) -> usize { 0 } }\npub fn custom(value: &Custom) -> usize { let copy = value.clone(); copy.step() }\npub fn built() -> usize { Input { pos: 1 }.pos + Plain { pos: 2 }.step() }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/Input/cloned", "/Input/step"),
        (EdgeKind::Reads, "/Input/cloned", "/Input/step"),
        (EdgeKind::Calls, "/Input/argument", "/Input/step"),
        (EdgeKind::Calls, "/Plain/cloned", "/Plain/step"),
        (EdgeKind::Calls, "/fixture/custom", "/Other/step"),
        (EdgeKind::Reads, "/fixture/built", "/Input/pos"),
        (EdgeKind::Calls, "/fixture/built", "/Plain/step"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(!index.edges().edges().any(|edge| {
        edge.from().as_str().ends_with("/fixture/custom")
            && edge.kind() == EdgeKind::Calls
            && edge.to().as_str().ends_with("/step")
            && !edge.to().as_str().ends_with("/Other/step")
    }));
}

#[test]
fn test_rust_typed_self_receivers_reach_the_implementation_type() {
    // `self: Box<Self>` and `self: Pin<&mut Self>` receivers are the implementation type's
    // values; a receiver typed by another type stays unresolved.
    let source = "use std::pin::Pin;\npub struct Slice { pub len: usize }\npub struct Other;\nimpl Other { pub fn into_entries(self: Box<Self>) -> usize { 0 } }\nimpl Slice {\n    pub fn into_entries(self: Box<Self>) -> usize { self.len }\n    pub fn into_keys(self: Box<Self>) -> usize { self.into_entries() }\n    pub fn pinned(self: Pin<&mut Self>) -> usize { self.len }\n    pub fn shared(&self) -> usize { self.len }\n}\npub struct Wrapper(pub Slice);\nimpl Slice { pub fn wrapped(self: Wrapper) -> usize { self.len } }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/Slice/into_keys", "/Slice/into_entries"),
        (EdgeKind::Reads, "/Slice/into_keys", "/Slice/into_entries"),
        (EdgeKind::Reads, "/Slice/into_entries", "/Slice/len"),
        (EdgeKind::Reads, "/Slice/pinned", "/Slice/len"),
        (EdgeKind::Reads, "/Slice/shared", "/Slice/len"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(!index.edges().edges().any(|edge| {
        matches!(edge.kind(), EdgeKind::Calls | EdgeKind::Reads)
            && (edge.to().as_str().ends_with("/Other/into_entries")
                || (edge.from().as_str().ends_with("/Slice/wrapped")
                    && edge.to().as_str().ends_with("/Slice/len")))
    }));
}

#[test]
fn test_rust_boxed_and_pinned_self_do_not_flatten_reference_impls() {
    // Rust selects the wrapper's Pick before the implementation reference's Pick.
    // Static lookup cannot distinguish the wrapper's generic instantiation at this site.
    let source = r"use std::pin::Pin;

pub struct Value;

pub trait Pick { fn pick(self) -> u8; }

impl Pick for &Value {
    fn pick(self) -> u8 { 3 }
}

impl Pick for Box<&Value> {
    fn pick(self) -> u8 { 103 }
}

impl Pick for Pin<&mut &Value> {
    fn pick(self) -> u8 { 104 }
}

pub trait Through {
    fn through(self: Box<Self>) -> u8;
    fn through_pin(self: Pin<&mut Self>) -> u8;
    fn through_reference(self: &Self) -> u8;
}

impl Through for &Value {
    fn through(self: Box<Self>) -> u8 { self.pick() }
    fn through_pin(self: Pin<&mut Self>) -> u8 { self.pick() }
    fn through_reference(self: &Self) -> u8 { self.pick() }
}
";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    let file = index
        .files()
        .find(|file| file.path().as_str() == "src/lib.rs")
        .expect("source file");
    let facts = file.syntax().static_facts().expect("authored facts");
    let starts: Vec<_> = source
        .match_indices("self.pick()")
        .map(|(start, _)| u64::try_from(start).expect("range start"))
        .collect();
    let [boxed, pinned, explicit_reference] = starts.as_slice() else {
        panic!("three physical receiver calls");
    };
    for start in [boxed, pinned, explicit_reference] {
        assert!(
            facts
                .calls()
                .iter()
                .any(|call| call.range.start == *start && call.range.end == start + 11)
        );
        assert!(
            facts
                .references()
                .iter()
                .any(|access| access.target.start == *start
                    && access.target.end == start + 9
                    && access.role == rift_core::ReferenceRole::Read)
        );
    }
    for kind in [EdgeKind::Calls, EdgeKind::Reads] {
        assert!(!index.edges().edges().any(|edge| edge.kind() == kind
            && [*boxed, *pinned].contains(&edge.occurrence().range().start())));
        assert!(index.edges().edges().any(|edge| edge.kind() == kind
            && edge.occurrence().range().start() == *explicit_reference
            && edge.from().as_str().ends_with("/&Value/through_reference")
            && edge.to().as_str().ends_with("/&Value/pick")));
    }
}

#[test]
fn test_rust_paths_through_private_imports_of_a_parent_module() {
    // `use super::Bucket` names what the parent module imported as `Bucket`; the import is
    // private to the parent and its descendants, so a path from elsewhere stays unresolved.
    // A cycle of private imports resolves nothing.
    let source = "mod inner { pub struct Bucket { pub value: u8 } }\npub mod map {\n    use crate::inner::Bucket;\n    pub mod iter {\n        use super::Bucket;\n        pub fn new() -> u8 { Bucket { value: 1 }.value }\n    }\n}\npub mod other {\n    pub fn outside() -> u8 { crate::map::Bucket { value: 2 }.value }\n}\npub mod first { use crate::second::Loop; pub fn run() -> u8 { Loop { value: 3 }.value } }\npub mod second { use crate::first::Loop; }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/map/iter/new", "/inner/Bucket"),
        (EdgeKind::Reads, "/map/iter/new", "/inner/Bucket/value"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    assert!(!index.edges().edges().any(|edge| {
        (edge.from().as_str().ends_with("/other/outside")
            || edge.from().as_str().ends_with("/first/run"))
            && matches!(edge.kind(), EdgeKind::Calls | EdgeKind::Reads)
    }));
}

#[test]
fn test_rust_type_parameter_paths_follow_their_bounds() {
    // `A::size()` names the associated function of the trait bounding `A`, inline or in a
    // where clause, never a same-name function of another trait.
    let source = "pub trait Array { fn size() -> usize; }\npub trait Other { fn size() -> usize; }\npub struct Holder<A: Array> { pub value: Option<A> }\nimpl<A: Array> Holder<A> {\n    pub fn capacity() -> usize { A::size() }\n}\npub fn generic<T: Array>() -> usize { T::size() }\npub fn bounded<T>() -> usize where T: Array { T::size() }\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for from in ["/Holder/capacity", "/fixture/generic", "/fixture/bounded"] {
        assert!(
            has_edge(&index, EdgeKind::Calls, from, "/Array/size"),
            "missing {from} -> Array.size"
        );
    }
    assert!(
        !index
            .edges()
            .edges()
            .any(|edge| edge.kind() == EdgeKind::Calls
                && edge.to().as_str().ends_with("/Other/size"))
    );
}

#[test]
fn test_rust_closure_parameters_and_guards_typed_by_a_generic_callee() {
    // A closure passed to a local generic function takes its parameter type from the
    // callee's `FnMut(&mut T)` bound, with `T` bound by the callee parameter declared `T`;
    // a tuple pattern takes the element of a tuple argument. A closure without a bound for
    // it, or whose `T` is not bound by a known argument, stays unresolved.
    let source = "use core::ops::{Deref, DerefMut};\npub struct Guard<T, F: FnMut(&mut T)> { value: T, dropfn: F }\nimpl<T, F: FnMut(&mut T)> Deref for Guard<T, F> { type Target = T; fn deref(&self) -> &T { &self.value } }\nimpl<T, F: FnMut(&mut T)> DerefMut for Guard<T, F> { fn deref_mut(&mut self) -> &mut T { &mut self.value } }\nimpl<T, F: FnMut(&mut T)> Drop for Guard<T, F> { fn drop(&mut self) { (self.dropfn)(&mut self.value) } }\npub fn guard<T, F: FnMut(&mut T)>(value: T, dropfn: F) -> Guard<T, F> { Guard { value, dropfn } }\npub fn unbound<T, F>(value: T, f: F) -> T { let _ = f; value }\npub struct Table { pub items: usize }\nimpl Table {\n    pub fn reset(&mut self) { self.items = 0; }\n    pub fn fill(&mut self) { self.items = 1; }\n    pub fn clear(&mut self) {\n        let mut this = guard(self, |this| this.reset());\n        this.fill();\n    }\n    pub fn tuple(&mut self) {\n        let _guard = guard((0, self), |(index, this)| { *index += 1; this.reset() });\n    }\n    pub fn loose(&mut self) {\n        let _ = unbound(self, |this: &mut Table| this.fill());\n        let _ = unbound(0, |this| this);\n        let _ = unbound(self, |this| this.reset());\n    }\n    pub fn direct(&mut self) { let this: &mut Table = self; this.reset(); }\n}\n";
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, &[("src/lib.rs", source)]);
    for (kind, from, to) in [
        (EdgeKind::Calls, "/Table/clear", "/Table/reset"),
        (EdgeKind::Reads, "/Table/clear", "/Table/reset"),
        (EdgeKind::Calls, "/Table/tuple", "/Table/reset"),
        (EdgeKind::Calls, "/Table/direct", "/Table/reset"),
    ] {
        assert!(
            has_edge(&index, kind, from, to),
            "missing {kind:?} {from} -> {to}"
        );
    }
    // `unbound` declares no `Fn*` bound for its closure parameter, and `0` has no owner.
    assert!(!index.edges().edges().any(|edge| {
        edge.from().as_str().ends_with("/Table/loose")
            && edge.kind() == EdgeKind::Calls
            && edge.to().as_str().ends_with("/Table/reset")
    }));
}

#[test]
fn test_rust_optional_dependencies_are_implicit_features() {
    // An optional dependency is a feature of its name unless a feature names it with
    // `dep:`. Default features enabling it select the items under it; an optional
    // dependency only `dep:` names declares no feature, so its condition stays unknown.
    let manifest = "[package]\nname='fixture'\nversion='0.1.0'\n[dependencies]\nextra = { version = '1', optional = true }\nrenamed = { package = 'other', version = '1', optional = true }\nexplicit = { version = '1', optional = true }\nrequired = '1'\n[target.'cfg(unix)'.dependencies]\nplatform = { version = '1', optional = true }\n[features]\ndefault = ['extra', 'platform']\nuses_dep = ['dep:explicit']\n";
    let source = "#[cfg(feature = \"extra\")]\npub fn enabled() -> u8 { 1 }\n#[cfg(not(feature = \"extra\"))]\npub fn enabled() -> u8 { 2 }\n#[cfg(feature = \"platform\")]\npub fn targeted() -> u8 { 3 }\n#[cfg(feature = \"renamed\")]\npub fn renamed() -> u8 { 4 }\n#[cfg(not(feature = \"renamed\"))]\npub fn renamed() -> u8 { 5 }\n#[cfg(feature = \"explicit\")]\npub fn hidden() -> u8 { 6 }\n#[cfg(feature = \"required\")]\npub fn plain() -> u8 { 7 }\npub fn run() -> u8 { enabled() + targeted() + renamed() + hidden() + plain() }\n";
    let (_directory, index) = fixture_index(manifest, &[("src/lib.rs", source)]);
    for to in ["/fixture/enabled", "/fixture/targeted", "/fixture/renamed"] {
        assert!(
            has_edge(&index, EdgeKind::Calls, "/fixture/run", to),
            "missing run -> {to}"
        );
    }
    for to in ["/fixture/hidden", "/fixture/plain"] {
        assert!(
            !has_edge(&index, EdgeKind::Calls, "/fixture/run", to),
            "unknown condition resolved: run -> {to}"
        );
    }
}

#[test]
fn test_rust_reexports_of_dependency_items_keep_the_written_module() {
    // A `use` path whose first segment names no item of its module starts at a dependency
    // crate. A re-export chain ending at such a path keeps the written local module as the
    // import; a name the module does not bind imports nothing, and a local relative path
    // still names the local item.
    let alloc = "pub(crate) use self::inner::{Allocator, Thing};\nmod inner {\n    mod local { pub struct Thing; }\n    pub(crate) use extra::alloc::Allocator;\n    pub(crate) use local::Thing;\n}\n";
    let (_directory, index) = fixture_index(
        PLAIN_MANIFEST,
        &[
            (
                "src/lib.rs",
                "mod alloc;\nmod map;\nmod other;\nmod local;\n",
            ),
            ("src/alloc.rs", alloc),
            (
                "src/map.rs",
                "use crate::alloc::Allocator;\npub fn run() {}\n",
            ),
            (
                "src/other.rs",
                "use crate::alloc::Missing;\npub fn run() {}\n",
            ),
            (
                "src/local.rs",
                "use crate::alloc::Thing;\npub fn run(_: Thing) {}\n",
            ),
        ],
    );
    assert!(has_edge(
        &index,
        EdgeKind::Imports,
        "/fixture/map",
        "/fixture/alloc"
    ));
    assert!(!has_edge(
        &index,
        EdgeKind::Imports,
        "/fixture/other",
        "/fixture/alloc"
    ));
    assert!(has_edge(
        &index,
        EdgeKind::Imports,
        "/fixture/local",
        "/alloc/inner/local/Thing"
    ));
}

#[test]
fn test_rust_target_conditions_store_facts_of_the_host_target() {
    // #859: a local workspace decides target predicates with the host target.
    let (_directory, index) = fixture_index(
        "[package]\nname='fixture'\nversion='0.1.0'\n[features]\ndefault=['std']\nstd=[]\n",
        &[(
            "src/lib.rs",
            "pub struct Item { pub value: usize }\npub fn leaf() -> usize { 1 }\nimpl Item {\n    #[cfg(feature = \"std\")]\n    pub fn by_feature(&self) -> usize { leaf() + self.value }\n    #[cfg(unix)]\n    pub fn by_unix(&self) -> usize { leaf() + self.value }\n    #[cfg(any(unix, windows, target_os = \"wasi\"))]\n    pub fn by_any_target(&self) -> usize { leaf() + self.value }\n    #[cfg(all(feature = \"std\", any(unix, windows)))]\n    pub fn by_feature_and_target(&self) -> usize { leaf() + self.value }\n    #[cfg(not(target_os = \"emscripten\"))]\n    pub fn by_not_target(&self) -> usize { leaf() + self.value }\n    #[cfg(windows)]\n    pub fn by_windows(&self) -> usize { leaf() + self.value }\n    #[cfg(all(unix, custom))]\n    pub fn by_unknown(&self) -> usize { leaf() + self.value }\n    pub fn plain(&self) -> usize { leaf() + self.value }\n}\n#[cfg(unix)]\npub fn native() -> usize { 1 }\n#[cfg(windows)]\npub fn native() -> usize { 2 }\npub fn caller() -> usize { native() }\n",
        )],
    );
    for (method, active) in [
        ("by_feature", true),
        ("by_unix", cfg!(unix)),
        ("by_any_target", true),
        ("by_feature_and_target", true),
        ("by_not_target", true),
        ("by_windows", cfg!(windows)),
        ("by_unknown", false),
        ("plain", true),
    ] {
        let from = format!("/Item/{method}");
        for (kind, from, to) in [
            (EdgeKind::Contains, "/fixture/Item", from.as_str()),
            (EdgeKind::Calls, from.as_str(), "/fixture/leaf"),
            (EdgeKind::Reads, from.as_str(), "/Item/value"),
        ] {
            assert_eq!(
                has_edge(&index, kind, from, to),
                active,
                "{kind:?} {from} -> {to}"
            );
        }
    }
    assert!(
        has_edge(
            &index,
            EdgeKind::Calls,
            "/fixture/caller",
            "/fixture/native"
        ),
        "the inactive alternative leaves the active declaration as the one target"
    );
    let view = index.symbol_view().expect("symbols");
    assert!(
        view.normalized_graph()
            .records()
            .iter()
            .all(|record| record.identity().is_some()),
        "every declaration keeps its identity"
    );
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
}

/// Every stored edge of one fixture without its occurrence.
fn edge_rows(files: &[(&str, &str)]) -> std::collections::BTreeSet<(String, String, String)> {
    let (_directory, index) = fixture_index(PLAIN_MANIFEST, files);
    assert!(index.edges().is_complete());
    index
        .edges()
        .edges()
        .map(|edge| {
            (
                format!("{:?}", edge.kind()),
                edge.from().as_str().to_owned(),
                edge.to().as_str().to_owned(),
            )
        })
        .collect()
}

#[test]
fn test_rust_leading_whitespace_keeps_every_row_of_a_reexporting_file() {
    // #982: the parsed file starts at its first token and its `use` items apply to the file.
    const OTHER: &str = "pub struct Item; impl Item { pub fn read(&self) -> u8 { 1 } } pub fn leaf() {} pub trait Tr { fn t(&self); }";
    const FACADE: &str = "pub use crate::other::{leaf, Item, Tr};\npub use crate::other as renamed;\nuse crate::other::leaf as private;\nmod child;\n";
    const USER: &str = "use crate::facade::{leaf, Item, Tr};\npub fn run(x: &Item) -> u8 { leaf(); x.read() }\npub fn path() { crate::facade::leaf(); crate::facade::renamed::leaf(); }\npub struct H;\nimpl Tr for H { fn t(&self) {} }\npub fn ty(x: crate::facade::Item) -> u8 { x.read() }\n";
    const CHILD: &str = "pub fn inherited() { super::private() }\n";
    let heads = ["", "\n", "   ", "\r\n", "\n// c\n", "// c\n"];
    let module = heads.map(|head| {
        let facade = format!("{head}{FACADE}");
        edge_rows(&[
            ("src/lib.rs", "pub mod other; pub mod facade; pub mod user;"),
            ("src/other.rs", OTHER),
            ("src/facade.rs", facade.as_str()),
            ("src/facade/child.rs", CHILD),
            ("src/user.rs", USER),
        ])
    });
    let root = heads.map(|head| {
        let library =
            format!("{head}mod other; pub use other::{{leaf, Item, Tr}}; pub mod user;\n");
        edge_rows(&[
            ("src/lib.rs", library.as_str()),
            ("src/other.rs", OTHER),
            (
                "src/user.rs",
                "pub fn run(x: &crate::Item) -> u8 { crate::leaf(); x.read() }\n",
            ),
        ])
    });
    for rows in [module, root] {
        assert!(
            rows[0]
                .iter()
                .any(|(kind, _, to)| kind == "Calls" && to.ends_with("/other/leaf"))
        );
        for (head, found) in heads.iter().zip(&rows) {
            assert_eq!(found, &rows[0], "{head:?}");
        }
    }
}

#[test]
fn test_rust_imports_of_a_reexporting_module_start_at_the_module() {
    // #983: the module of a file is the one its placement states, not the common parent of
    // the names it declares.
    for (library, module) in [
        ("mod other; use other::leaf;", "/fixture"),
        ("mod other; pub use other::leaf;", "/fixture"),
        ("mod other; pub use other::{leaf, Item};", "/fixture"),
        (
            "mod other; pub use other::leaf; pub fn own() {}",
            "/fixture",
        ),
        (
            "mod other; pub use std::fmt::Debug; pub use other::leaf;",
            "/fixture",
        ),
        (
            "pub mod inner { pub fn leaf() {} pub mod deep { pub use super::leaf; } }",
            "/inner/deep",
        ),
        ("pub mod other; pub mod user;", "/fixture/user"),
        ("pub mod other; pub mod implement;", "/fixture/implement"),
    ] {
        let rows = edge_rows(&[
            ("src/lib.rs", library),
            ("src/other.rs", "pub struct Item; pub fn leaf() {}"),
            ("src/user.rs", "pub use crate::other::leaf;"),
            (
                "src/implement.rs",
                "use crate::other::Item; impl Item { pub fn read(&self) {} }",
            ),
        ]);
        let importers = rows
            .iter()
            .filter(|(kind, _, _)| kind == "Imports")
            .map(|(_, from, _)| from.as_str())
            .collect::<Vec<_>>();
        assert!(!importers.is_empty(), "{library}");
        for importer in importers {
            assert!(importer.ends_with(module), "{library}: {importer}");
        }
    }
}
