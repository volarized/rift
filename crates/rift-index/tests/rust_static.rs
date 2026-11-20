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
