use rift_protocol::{identity::SymbolOwner, read::Language};
use rift_syntax::{SyntaxDocument, SyntaxLimits, SyntaxSource, registry};

use super::{ReferenceRole, StaticOccurrence, StaticResolution, StaticSource, resolve};
use crate::analyzer::namespace::{SelectedFile, prepare_context};
use crate::semantic::PlacedFacts;
use crate::{ExactPackageLimits, NamespaceInput, PackageSource, WorkspaceSemantics};
use rift_core::ProjectPath;
use rift_provider::NormalizedTarget;
use rift_syntax::DocumentPlacement;

const WORK_MAX: usize = 10_000;

mod python_overloads;

#[test]
fn test_python_property_returns_resolve_instance_members_and_aliases() {
    let source = "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: return Item()\n    def use(self): return self.item.read()\nclass Child(Owner):\n    def use(self): return self.item.read()\ndef typed(owner: Owner):\n    owner.item.read()\n    alias = owner.item\n    alias.read()\ndef constructed(): return Owner().item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let reads = calls(&result, "main.py")
        .filter(|edge| target(edge) == "rift://symbol/local/python/main/Item/read")
        .map(|edge| {
            (
                source_of(edge, source),
                edge.edge_from.as_ref().expect("caller").as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reads,
        [
            (
                "self.item.read()",
                "rift://symbol/local/python/main/Owner/use"
            ),
            (
                "self.item.read()",
                "rift://symbol/local/python/main/Child/use"
            ),
            ("owner.item.read()", "rift://symbol/local/python/main/typed"),
            ("alias.read()", "rift://symbol/local/python/main/typed"),
            (
                "Owner().item.read()",
                "rift://symbol/local/python/main/constructed"
            ),
        ],
        "{result:?}"
    );
    assert!(
        result.occurrences.iter().any(|edge| {
            edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && source_of(edge, source) == "self.item"
                && target(edge) == "rift://symbol/local/python/main/Owner/item"
        }),
        "{result:?}"
    );
    assert!(
        result.occurrences.iter().any(|edge| {
            edge.role == ReferenceRole::Type
                && source_of(edge, source) == "Item"
                && target(edge) == "rift://symbol/local/python/main/Item"
        }),
        "{result:?}"
    );
    assert!(!result.exhausted);
}

#[test]
fn test_python_property_returns_refuse_shadowing_in_decorated_classes() {
    let source = "class Item:\n    def read(self): pass\n@decorate\nclass Owner:\n    @property\n    def item(self) -> Item: pass\n    from missing import item\ndef use(owner: Owner): return owner.item.read()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert!(
        calls(&result, "main.py")
            .all(|edge| target(edge) != "rift://symbol/local/python/main/Item/read"),
        "{result:?}"
    );
}

#[test]
fn test_python_property_returns_follow_imported_receiver_annotations() {
    let result = resolution(
        "python",
        &[
            (
                "models.py",
                "class Item:\n    def read(self): pass\nclass Owner:\n    @property\n    def item(self) -> Item: pass\n",
            ),
            (
                "main.py",
                "from models import Owner\ndef use(owner: Owner): return owner.item.read()\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/models/Item/read"],
        "{result:?}"
    );
}

#[test]
fn test_python_property_returns_refuse_descriptors_shadowing_and_ambiguity() {
    let prefix = "class Item:\n    def read(self): pass\nclass Other: pass\n";
    let cases = [
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    @classmethod\n    def use(cls): return cls.item.read()\ndef use():\n    Owner.item.read()\n    alias = Owner\n    alias.item.read()\n",
        "property = unknown\nclass Owner:\n    @property\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @other.property\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    @decorate\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item | Other: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    @property\n    def item(self) -> Other: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    def item(self) -> Item: pass\ndef use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\ndef use(owner: Owner):\n    owner = unknown\n    return owner.item.read()\n",
        "def outer(property):\n    class Owner:\n        @property\n        def item(self) -> Item: pass\n    def use(owner: Owner): return owner.item.read()\n",
        "class Owner:\n    @property\n    def item(self) -> Item: pass\n    item = unknown\ndef use(owner: Owner): return owner.item.read()\n",
    ];
    for case in cases {
        let source = format!("{prefix}{case}");
        let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX);
        assert!(
            calls(&result, "main.py")
                .all(|edge| { target(edge) != "rift://symbol/local/python/main/Item/read" }),
            "{source}\n{result:?}"
        );
        assert!(!result.exhausted);
    }
}

#[test]
fn test_python_wildcard_imports_keep_modules_without_resolving_exports() {
    let source = "from .leaf import *\ndef use(): return leaf()\n";
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", source),
            ("pkg/leaf.py", "value = 1\ndef leaf(): pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let imports = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Import)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 1, "{result:?}");
    assert_eq!(source_of(imports[0], source), "*");
    assert_eq!(target(imports[0]), "rift://symbol/local/python/pkg/leaf");
    assert_eq!(
        imports[0]
            .edge_from
            .as_ref()
            .map(rift_core::SymbolId::as_str),
        Some("rift://symbol/local/python/pkg")
    );
    assert_eq!(calls(&result, "pkg/__init__.py").count(), 0);
}

#[test]
fn test_python_wildcard_imports_refuse_unknown_and_ambiguous_modules() {
    let result = resolution(
        "python",
        &[
            ("main.py", "from missing import *\nfrom pkg.leaf import *\n"),
            ("pkg/leaf.py", "value = 1\n"),
            ("pkg/leaf/__init__.py", "value = 2\n"),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .all(|edge| edge.role != ReferenceRole::Import),
        "{result:?}"
    );
}

#[test]
fn test_python_static_receiver_depth_declines_without_blocking_later_reads() {
    let deep = ".again()".repeat(40);
    let source = format!(
        "class Value:\n    count: int\n    def again(self) -> 'Value': return self\ndef deep(value: Value): return value{deep}.count\ndef short(value: Value): return value.again().count\n"
    );
    let result = resolution("python", &[("main.py", &source)], &[], WORK_MAX * 100);
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/main/Value/count"
        })
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1, "{result:?}");
    assert_eq!(
        reads[0].edge_from.as_ref().map(rift_core::SymbolId::as_str),
        Some("rift://symbol/local/python/main/short")
    );
    assert!(!result.exhausted, "{result:?}");
}

#[test]
fn test_python_static_inherited_fields_keep_assigned_receiver_types() {
    let source = "class Value:\n    count: int\nclass Base:\n    value: object\nclass Middle(Base):\n    def __init__(self): self.value = Value()\nclass Child(Middle):\n    def __init__(self): self.value = Value()\n    def read(self):\n        alias = self.value\n        return self.value.count + alias.count\nclass GrandChild(Child):\n    def read(self): return self.value.count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        reads.contains(&"rift://symbol/local/python/main/Base/value"),
        "{result:?}"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|target| **target == "rift://symbol/local/python/main/Value/count")
            .count(),
        3,
        "{result:?}"
    );
}

#[test]
fn test_python_static_checked_receivers_reject_shadowing_and_rebinding() {
    let source = "class Value:\n    count: int\ndef valid(value):\n    if not isinstance(value, Value) or disabled:\n        return\n    value.count = 1\ndef changed(value):\n    if not isinstance(value, Value): return\n    value = unknown()\n    value.count = 1\ndef shadowed(value, isinstance):\n    if not isinstance(value, Value): return\n    value.count = 1\ndef nested(value):\n    if not isinstance(value, Value): return\n    def write(): value.count = 1\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        ["rift://symbol/local/python/main/Value/count"],
        "{result:?}"
    );
}

#[test]
fn test_python_static_alias_cycles_and_function_objects_remain_unresolved() {
    let source = "class Value:\n    count: int\ndef build() -> Value: return Value()\ndef root(value: Value):\n    alias = value\n    alias.count = 1\n    build.count = 2\n    cyclic = cyclic.item\n    cyclic.count = 3\n    first = second\n    second = first\n    first.count = 4\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        ["rift://symbol/local/python/main/Value/count"],
        "{result:?}"
    );
    assert!(!result.exhausted, "{result:?}");
}

#[test]
fn test_python_static_inherited_fields_keep_the_declared_base_target() {
    let source = "class Base:\n    value: int\nclass Child(Base):\n    def __init__(self): self.value = 1\n    def read(self): return self.value\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let accesses = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.edge_kind.is_none()
                && matches!(edge.role, ReferenceRole::Read | ReferenceRole::Write)
        })
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        accesses, ["rift://symbol/local/python/main/Base/value"; 2],
        "{result:?}"
    );
}

#[test]
fn test_python_static_aliases_and_return_annotations_resolve_members() {
    let source = "class Value:\n    count: int\n    def run(self): pass\ndef build() -> Value | None: return None\ndef root():\n    first = build()\n    second = first\n    second.count = 1\n    second.run()\n    build().count = 2\n    return build().count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let writes = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Write)
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        writes, ["rift://symbol/local/python/main/Value/count"; 2],
        "{result:?}"
    );
    assert!(
        calls(&result, "main.py")
            .any(|edge| target(edge) == "rift://symbol/local/python/main/Value/run"),
        "{result:?}"
    );
}

#[test]
fn test_python_static_module_aliases_and_instance_class_attributes() {
    let result = resolution(
        "python",
        &[
            (
                "pkg/__init__.py",
                "count = 0\nclass Value:\n    count = 0\n",
            ),
            (
                "main.py",
                "import pkg\nmodule = pkg\ndef root():\n    module.count = 1\n    value = pkg.Value()\n    alias = value\n    return alias.count\ndef nested():\n    class Base:\n        count = 0\n    class Child(Base):\n        count = 1\n    value = Child()\n    return value.count\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Write
                && target(edge) == "rift://symbol/local/python/pkg/count"),
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/pkg/Value/count"),
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.role == ReferenceRole::Read
                && edge.edge_kind.is_none()
                && target(edge) == "rift://symbol/local/python/main/nested/Child/count"),
        "{result:?}"
    );
}

#[test]
fn test_python_static_imports_keep_submodules_and_captured_external_exports() {
    let result = resolution(
        "python",
        &[
            (
                "main.py",
                "from pkg import models\nfrom pkg.bridge import External\n",
            ),
            ("pkg/__init__.py", ""),
            ("pkg/models.py", "class Value: pass\n"),
            (
                "pkg/bridge.py",
                "from external import External as External\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let imported = result
        .occurrences
        .iter()
        .filter(|edge| {
            edge.role == ReferenceRole::Import && edge.binding.unit().key().as_str() == "main.py"
        })
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        imported,
        [
            "rift://symbol/local/python/pkg/models",
            "rift://symbol/local/python/pkg/bridge"
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_static_reexports_keep_module_import_owners() {
    let result = resolution(
        "python",
        &[
            ("pkg/__init__.py", "from .models import Value as Value\n"),
            ("pkg/models.py", "class Value: pass\n"),
            ("main.py", "from pkg import Value\n"),
        ],
        &[],
        WORK_MAX,
    );
    let imports = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Import)
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2, "{result:?}");
    assert_eq!(
        imports[1]
            .edge_from
            .as_ref()
            .map(rift_core::SymbolId::as_str),
        Some("rift://symbol/local/python/pkg")
    );
}

#[test]
fn test_python_static_super_calls_and_typed_parameters() {
    let source = "class Base:\n    def __init__(self): pass\n    def run(self): pass\nclass Child(Base):\n    def __init__(self): super().__init__()\n    @classmethod\n    def build(cls): return cls()\n    @staticmethod\n    def invoke(value: Base): value.run()\ndef root(value: 'Base'):\n    value.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        [
            "rift://symbol/local/python/main/Base/__init__",
            "rift://symbol/local/python/main/Child",
            "rift://symbol/local/python/main/Base/run",
            "rift://symbol/local/python/main/Base/run",
        ],
        "{result:?}"
    );
}

#[test]
fn test_python_static_annotations_keep_values_and_reject_ambiguous_receivers() {
    let source = "class Value:\n    count: int = 0\n    def run(self): pass\nclass Owner:\n    def __init__(self):\n        self.value: Value | None = None\n    def run(self):\n        self.value.run()\n        self.value.count += 1\ncurrent: Value\ndef root(value: Value | Owner, cyclic: cyclic):\n    value.run()\n    cyclic.run()\n    return current.count\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Value/run"],
        "{result:?}"
    );
    let reads = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Read && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        reads.contains(&"rift://symbol/local/python/main/Owner/value"),
        "{reads:?}"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|target| **target == "rift://symbol/local/python/main/Value/count")
            .count(),
        2,
        "{reads:?}"
    );
}

#[test]
fn test_python_static_super_rejects_shadowed_and_static_calls() {
    let source = "class Base:\n    def run(self): pass\nclass Child(Base):\n    @staticmethod\n    def invalid(): super().run()\n    def shadowed(self, super): super().run()\n    def explicit(self): super(Child, self).run()\n    def rebound(self): self = None; super().run()\n    def valid(self): super( ).run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Base/run"],
        "{result:?}"
    );
}

#[test]
fn test_python_static_module_chains_and_decorated_calls_keep_their_owners() {
    let source = "import pkg.handlers\nfrom pkg import leaf\nfirst = leaf()\nsecond = pkg.handlers.leaf()\nclass Owner:\n    value = leaf()\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("pkg/__init__.py", "from .handlers import leaf as leaf\n"),
            ("pkg/handlers.py", "@decorator\ndef leaf(): pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.py").collect::<Vec<_>>();
    assert_eq!(calls.len(), 3, "{result:?}");
    for (call, owner) in calls.into_iter().zip(["main", "main", "main/Owner"]) {
        assert_eq!(target(call), "rift://symbol/local/python/pkg/handlers/leaf");
        assert_eq!(
            call.edge_from.as_ref().map(rift_core::SymbolId::as_str),
            Some(format!("rift://symbol/local/python/{owner}").as_str())
        );
    }
}

#[test]
fn test_python_static_constructor_bindings_follow_named_members_only() {
    let source = "class Value:\n    def run(self): pass\nclass Owner:\n    def __init__(self): self.value = Value()\n    def run(self): self.value.run()\ndef factory(): return Value()\ndef root():\n    value.run()\n    value = Value()\n    value.run()\n    unknown = factory()\n    unknown.run()\n    first, second = Value()\n    first.run()\n    second.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let methods = calls(&result, "main.py")
        .filter(|call| target(call).ends_with("/run"))
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "rift://symbol/local/python/main/Value/run",
            "rift://symbol/local/python/main/Value/run"
        ],
        "{result:?}"
    );
}

#[test]
fn test_edge_heritage_is_captured_for_each_language() {
    for (path, source, expected) in [
        ("app.js", "class Base {}\nclass Derived extends Base {}", 1),
        (
            "app.ts",
            "class Base {}\ninterface Task {}\nclass Derived extends Base implements Task {}",
            2,
        ),
        (
            "app.tsx",
            "class Base {}\ninterface Task {}\nclass Derived extends Base implements Task {}",
            2,
        ),
        ("app.py", "class Base: pass\nclass Derived(Base): pass\n", 1),
        (
            "lib.rs",
            "trait Task {}\nstruct Worker;\nimpl Task for Worker {}",
            1,
        ),
    ] {
        let parsed = syntax(path, source);
        let captured = parsed.facts().static_facts().expect("captured edge syntax");
        assert_eq!(captured.heritage_edges().len(), expected, "{path}");
    }
}

#[test]
fn test_edge_typescript_calls_follow_import_bindings_in_each_dialect() {
    for extension in ["ts", "tsx", "mts", "cts"] {
        let path = format!("app/main.{extension}");
        let source = "import { leaf as run } from '../lib/jobs';\nexport function root(): number { return run(); }\n";
        let result = resolution(
            "typescript",
            &[
                (path.as_str(), source),
                (
                    "lib/jobs.ts",
                    "export function leaf(): number { return 1; }",
                ),
            ],
            &[],
            WORK_MAX,
        );
        let edges = calls(&result, &path).collect::<Vec<_>>();
        assert_eq!(edges.len(), 1, "{extension}: {result:?}");
        assert_eq!(source_of(edges[0], source), "run()");
        assert_eq!(edges[0].target_binding.unit().key().as_str(), "lib/jobs.ts");
    }
}

#[test]
fn test_edge_namespace_imports_resolve_calls_and_heritage() {
    let source = "import * as tasks from './jobs.ts';\nexport function root() { tasks.leaf(); }\nclass Derived extends tasks.Base {}";
    let result = resolution(
        "typescript",
        &[
            ("main.ts", source),
            ("jobs.ts", "export function leaf() {}\nexport class Base {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.ts").count(), 1);
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends))
    );
}

#[test]
fn test_edge_python_calls_follow_local_and_import_bindings() {
    let source = "from jobs import leaf as run\nimport jobs as tasks\ndef local(): return 1\ndef root():\n    return run() + tasks.leaf() + local()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("jobs.py", "def leaf(): return 1\n")],
        &[],
        WORK_MAX,
    );
    let edges = calls(&result, "main.py").collect::<Vec<_>>();
    let authored = edges
        .iter()
        .map(|edge| source_of(edge, source))
        .collect::<Vec<_>>();
    assert_eq!(authored, ["run()", "tasks.leaf()", "local()"]);
    assert_eq!(edges[0].target_binding.unit().key().as_str(), "jobs.py");
}

#[test]
fn test_python_calls_in_local_initializers_keep_the_enclosing_callable() {
    let source = "def leaf(): return 1\ndef root():\n    value: int = leaf()\n    def nested():\n        other = leaf()\n    return leaf()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let callers = calls(&result, "main.py")
        .map(|call| call.edge_from.as_ref().map(rift_core::SymbolId::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        callers,
        [
            Some("rift://symbol/local/python/main/root"),
            Some("rift://symbol/local/python/main/root/nested"),
            Some("rift://symbol/local/python/main/root"),
        ]
    );
}

#[test]
fn test_edge_python_imports_keep_relative_bindings_and_reject_dotted_module_shadows() {
    let result = resolution(
        "python",
        &[
            (
                "pkg/main.py",
                "from .jobs import leaf as run\ndef root(): return run()\n",
            ),
            ("pkg/jobs.py", "def leaf(): return 1\n"),
            (
                "other.py",
                "import pkg.jobs\ndef root(): return pkg.leaf()\n",
            ),
            ("stub.pyi", "def declared(): ...\n"),
        ],
        &[],
        WORK_MAX,
    );
    let resolved = calls(&result, "pkg/main.py").collect::<Vec<_>>();
    assert_eq!(resolved.len(), 1, "{result:?}");
    assert_eq!(
        resolved[0].target_binding.unit().key().as_str(),
        "pkg/jobs.py"
    );
    assert_eq!(calls(&result, "other.py").count(), 0);
}

#[test]
fn test_python_static_calls_resolve_receivers_constructors_and_reexports() {
    let source = "from pkg import Derived\nimport pkg.models as models\ndef root():\n    Derived()\n    models.Derived()\n    Derived.create()\nclass Child(Derived):\n    def run(self):\n        self.base()\n        self.run()\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            (
                "pkg/__init__.py",
                "from .models import Derived as Derived\n",
            ),
            (
                "pkg/models.py",
                "class Base(External):\n    def base(self): pass\nclass Derived(Base):\n    @staticmethod\n    def create(): pass\n",
            ),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.py").count(), 5, "{result:?}");
}

#[test]
fn test_python_static_contains_accesses_overrides_and_type_usage() {
    use rift_protocol::read::EdgeKind;
    let source = "import models\nclass Base:\n    value: int = 0\n    def run(self): pass\nclass Child(Base):\n    def run(self, value: models.Value) -> models.Value:\n        self.value = value\n        self.value += 1\n        return models.current\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("models.py", "class Value: pass\ncurrent: Value\n"),
        ],
        &[],
        WORK_MAX,
    );
    for (kind, minimum) in [
        (EdgeKind::Contains, 7),
        (EdgeKind::Reads, 2),
        (EdgeKind::Writes, 2),
        (EdgeKind::Overrides, 1),
        (EdgeKind::HasType, 3),
        (EdgeKind::Imports, 1),
    ] {
        let count = result
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.edge_kind.unwrap_or(match occurrence.role {
                    ReferenceRole::Read => EdgeKind::Reads,
                    ReferenceRole::Write => EdgeKind::Writes,
                    ReferenceRole::Type => EdgeKind::HasType,
                    ReferenceRole::Import => EdgeKind::Imports,
                    _ => EdgeKind::References,
                }) == kind
            })
            .count();
        assert!(count >= minimum, "{kind:?}: {count}; {result:?}");
    }
}

#[test]
fn test_python_static_bindings_keep_lexical_scopes_and_class_order() {
    let source = "def leaf(): pass\ndef run():\n    first, second = (1, 2)\n    values = [leaf() for item in []]\n    leaf()\nclass Root:\n    def run(self): pass\nclass Left(Root): pass\nclass Right(Root):\n    def run(self): pass\nclass Child(Left, Right):\n    def run(  # type: ignore\n        self): self.run(); self.other()\n    def other(self): leaf()\n    @staticmethod\n    def static(self): self.run()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    let targets = calls(&result, "main.py").map(target).collect::<Vec<_>>();
    assert_eq!(
        targets,
        [
            "rift://symbol/local/python/main/leaf",
            "rift://symbol/local/python/main/leaf",
            "rift://symbol/local/python/main/Child/run",
            "rift://symbol/local/python/main/Child/other",
            "rift://symbol/local/python/main/leaf",
        ],
        "{result:?}"
    );
    let overridden = result
        .occurrences
        .iter()
        .filter(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Overrides))
        .map(target)
        .collect::<Vec<_>>();
    assert!(
        overridden.contains(&"rift://symbol/local/python/main/Right/run"),
        "{overridden:?}"
    );
}

#[test]
fn test_python_static_namespace_packages_and_forward_type_arguments() {
    let source = "import pkg.models\nfrom pkg import models\nclass Child(pkg.models.Base[models.Value]):\n    def run(self, value: list['pkg.models.Value']) -> 'models.Value': pass\n";
    let result = resolution(
        "python",
        &[
            ("main.py", source),
            ("pkg/models.py", "class Base: pass\nclass Value: pass\n"),
        ],
        &[],
        WORK_MAX,
    );
    let types = result
        .occurrences
        .iter()
        .filter(|edge| edge.role == ReferenceRole::Type && edge.edge_kind.is_none())
        .map(target)
        .collect::<Vec<_>>();
    assert_eq!(
        types
            .iter()
            .filter(|id| **id == "rift://symbol/local/python/pkg/models/Value")
            .count(),
        3,
        "{result:?}"
    );
    assert!(
        result
            .occurrences
            .iter()
            .any(|edge| edge.edge_kind == Some(rift_protocol::read::EdgeKind::Extends)),
        "{result:?}"
    );
}

#[test]
fn test_python_global_and_nonlocal_names_do_not_block_other_bindings() {
    let source = "from jobs import leaf\ndef root():\n    active = False\n    def finish():\n        nonlocal active\n        active = True\n        leaf()\n    def send():\n        nonlocal active\n        finish()\n    def global_call():\n        global leaf\n        leaf()\n";
    let result = resolution(
        "python",
        &[("main.py", source), ("jobs.py", "def leaf(): pass\n")],
        &[],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "main.py").count(), 3, "{result:?}");
}

#[test]
fn test_python_static_incomplete_bases_keep_only_the_first_base_member() {
    let source = "class Root:\n    def inherited(self): pass\nclass Left(Root):\n    def own(self): pass\nclass Right(External): pass\nclass Child(Left, Right):\n    def run(self):\n        self.own()\n        self.inherited()\n";
    let result = resolution("python", &[("main.py", source)], &[], WORK_MAX);
    assert_eq!(
        calls(&result, "main.py").map(target).collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/Left/own"]
    );
    assert!(!result.exhausted);
    let exhausted = resolution("python", &[("main.py", source)], &[], 1);
    assert!(exhausted.exhausted);
    assert!(exhausted.occurrences.is_empty());
}

fn syntax(path: &str, text: &str) -> SyntaxDocument {
    let path = ProjectPath::new(path).expect("fixture path");
    let extension = path.as_str().rsplit_once('.').expect("fixture extension").1;
    let provider = registry::provider_for_extension(extension).expect("fixture provider");
    provider
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("fixture syntax")
}

fn resolution(
    language: &str,
    sources: &[(&str, &str)],
    metadata: &[(&str, &str)],
    work_max: usize,
) -> StaticResolution {
    let language = Language::from_identity_segment(language).expect("fixture language");
    let parsed = sources
        .iter()
        .map(|(path, text)| syntax(path, text))
        .collect::<Vec<_>>();
    let metadata_paths = metadata
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("fixture metadata path"))
        .collect::<Vec<_>>();
    let context = metadata_paths
        .iter()
        .zip(metadata)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let selected = parsed
        .iter()
        .zip(sources)
        .map(|(document, (_, source))| SelectedFile {
            path: document.path(),
            source,
            syntax: document.facts(),
        })
        .collect::<Vec<_>>();
    let package_sources = selected
        .iter()
        .map(|source| PackageSource::new(source.path, source.source))
        .collect::<Vec<_>>();
    let input = NamespaceInput::project(
        &SymbolOwner::Local,
        &language,
        &package_sources,
        &context,
        &[],
        ExactPackageLimits::new(32, 32_768),
    )
    .expect("fixture namespace");
    let mut namespaces = prepare_context(&input, &selected);
    let placed = selected
        .iter()
        .map(|source| PlacedFacts {
            facts: source.syntax,
            path: source.path,
            placement: DocumentPlacement::project_path(source.path)
                .expect("fixture placement")
                .with_identity_anchors(
                    namespaces
                        .anchors
                        .remove(source.path.as_str())
                        .unwrap_or_default(),
                )
                .with_logical_declarations(
                    namespaces
                        .mappings
                        .remove(source.path.as_str())
                        .unwrap_or_default(),
                ),
        })
        .collect::<Vec<_>>();
    let semantics = WorkspaceSemantics::build_facts_placed(&placed, 1024, 7, None)
        .expect("fixture graph")
        .semantics;
    let documents = placed
        .iter()
        .zip(&selected)
        .map(|(placed, source)| StaticSource {
            path: source.path,
            source: source.source,
            facts: source.syntax,
            placement: &placed.placement,
        })
        .collect::<Vec<_>>();
    resolve(semantics.graph(), &documents, &input, work_max)
}

fn calls<'source>(
    result: &'source StaticResolution,
    path: &'source str,
) -> impl Iterator<Item = &'source StaticOccurrence> {
    let unit = format!("rift://source/project/{path}");
    result.occurrences.iter().filter(move |occurrence| {
        occurrence.role == ReferenceRole::Call && occurrence.binding.unit().to_string() == unit
    })
}

fn target(occurrence: &StaticOccurrence) -> &str {
    match &occurrence.target {
        NormalizedTarget::Symbol(symbol) => symbol.as_str(),
        NormalizedTarget::Contribution(reference) => reference.symbol().as_str(),
    }
}

fn source_of<'source>(occurrence: &StaticOccurrence, source: &'source str) -> &'source str {
    let range = occurrence.binding.range();
    let start = usize::try_from(range.start()).expect("fixture range start");
    let end = usize::try_from(range.end()).expect("fixture range end");
    &source[start..end]
}

#[test]
fn javascript_calls_follow_local_import_aliases_and_keep_each_occurrence() {
    let source = "import { launch as run } from './barrel.mjs';\n\
                  function caller() { run(); run(); { const run = 0; run(); } }";
    let result = resolution(
        "javascript",
        &[
            ("app/main.mjs", source),
            (
                "app/barrel.mjs",
                "export { start as launch } from '../lib/jobs.mjs';",
            ),
            ("app/decoy.mjs", "export function launch() {}"),
            ("lib/jobs.mjs", "export function start() {}"),
        ],
        &[],
        WORK_MAX,
    );
    assert_eq!(result.analyzed_sources, 4);
    assert_eq!(result.unavailable_sources, 0);
    assert_eq!(result.unresolved_sites, 1);
    assert!(!result.exhausted);
    let calls = calls(&result, "app/main.mjs").collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].binding.range(), calls[1].binding.range());
    for call in calls {
        assert_eq!(source_of(call, source), "run()");
        assert_eq!(
            target(call),
            "rift://symbol/local/javascript/.%2Flib%2Fjobs.mjs/start"
        );
        assert_eq!(
            call.source_digest,
            rift_core::FileDigest::of(source.as_bytes())
        );
    }
    assert!(result.occurrences.iter().any(|occurrence| {
        occurrence.role == ReferenceRole::Import && source_of(occurrence, source) == "run"
    }));
}

#[test]
fn javascript_default_import_and_local_class_method_keep_authored_targets() {
    let source = "import execute from './other.mjs';\n\
                  class Task { run() {} static wrong() {} }\n\
                  function caller() { const task = new Task(); task.run(); task.wrong(); execute(); }";
    let result = resolution(
        "javascript",
        &[
            ("main.mjs", source),
            ("other.mjs", "export default function external() {}"),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.mjs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            (
                "task.run()",
                "rift://symbol/local/javascript/.%2Fmain.mjs/Task/run"
            ),
            (
                "execute()",
                "rift://symbol/local/javascript/.%2Fother.mjs/external"
            ),
        ]
    );
}

#[test]
fn javascript_rebinding_and_class_prototype_mutation_leave_calls_unresolved() {
    let source = "function original() {}\n\
                  function replacement() {}\n\
                  class Task { run() {} }\n\
                  original = replacement;\n\
                  Task.prototype.run = replacement;\n\
                  original(); const task = new Task(); task.run();";
    let result = resolution("javascript", &[("main.mjs", source)], &[], WORK_MAX);
    assert_eq!(calls(&result, "main.mjs").count(), 0);
    assert_eq!(result.unresolved_sites, 3);
}

#[test]
fn javascript_module_level_occurrence_keeps_its_physical_target() {
    let source = "import { run } from './tasks.mjs'; run();";
    let tasks = "export function run() {}";
    let result = resolution(
        "javascript",
        &[("main.mjs", source), ("tasks.mjs", tasks)],
        &[],
        WORK_MAX,
    );
    let mut calls = calls(&result, "main.mjs");
    let call = calls.next().expect("module-level call");
    assert_eq!(source_of(call, source), "run()");
    assert_eq!(
        call.target_binding.unit().to_string(),
        "rift://source/project/tasks.mjs"
    );
    let range = call.target_binding.range();
    let start = usize::try_from(range.start()).expect("fixture range start");
    let end = usize::try_from(range.end()).expect("fixture range end");
    assert_eq!(&tasks[start..end], "function run() {}");
    assert!(calls.next().is_none());
}

#[test]
fn javascript_literal_require_resolves_only_with_commonjs_mode_and_unshadowed_binding() {
    let source = "const run = require('./jobs.cjs').start;\n\
                  function caller() { run(); }\n\
                  function decoy(require) { const fake = require('./jobs.cjs').start; fake(); }";
    let result = resolution(
        "javascript",
        &[
            ("main.cjs", source),
            (
                "jobs.cjs",
                "function start() {}\nmodule.exports = { start };",
            ),
        ],
        &[],
        WORK_MAX,
    );
    let calls = calls(&result, "main.cjs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [("run()", "rift://symbol/local/javascript/.%2Fjobs.cjs/start")]
    );
}

#[test]
fn javascript_js_modules_follow_the_nearest_explicit_package_mode() {
    let source = "const run = require('../jobs.js').start; run();";
    let files = [
        ("common/main.js", source),
        ("esm/main.js", source),
        (
            "jobs.js",
            "function start() {}\nmodule.exports = { start };",
        ),
    ];
    let common = r#"{"type":"commonjs"}"#;
    let esm = r#"{"type":"module"}"#;
    let result = resolution(
        "javascript",
        &files,
        &[("package.json", common), ("esm/package.json", esm)],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "common/main.js").count(), 1);
    assert_eq!(calls(&result, "esm/main.js").count(), 0);
    let missing = resolution("javascript", &files, &[], WORK_MAX);
    assert!(missing.occurrences.is_empty());
}

#[test]
fn javascript_empty_analysis_completes_without_an_unresolved_site() {
    let result = resolution(
        "javascript",
        &[("empty.mjs", "export function idle() {}")],
        &[],
        WORK_MAX,
    );
    assert!(result.occurrences.is_empty());
    assert_eq!(result.analyzed_sources, 1);
    assert_eq!(result.unavailable_sources, 0);
    assert_eq!(result.unresolved_sites, 0);
    assert!(!result.exhausted);
}

#[test]
fn javascript_cycles_malformed_sources_and_work_exhaustion_remain_distinct() {
    let sources = [
        ("main.mjs", "import { start } from './cycle.mjs'; start();"),
        ("cycle.mjs", "export { start } from './cycle.mjs';"),
        ("bad.mjs", "function {"),
    ];
    let result = resolution("javascript", &sources, &[], WORK_MAX);
    assert!(result.occurrences.is_empty());
    assert_eq!(result.analyzed_sources, 2);
    assert_eq!(result.unavailable_sources, 1);
    assert_eq!(result.unresolved_sites, 2);
    assert!(!result.exhausted);
    let exhausted = resolution("javascript", &sources, &[], 1);
    assert!(exhausted.exhausted);
    assert_eq!(exhausted.analyzed_sources, 0);
}

#[test]
fn rust_uses_admitted_module_paths_and_excludes_trait_and_conditional_candidates() {
    let source = "mod tasks;\n\
                  use crate::tasks::run as start;\n\
                  struct Job;\n\
                  trait Other { fn wrong(&self); }\n\
                  impl Other for Job { fn wrong(&self) {} }\n\
                  impl Job { fn execute(&self) {} }\n\
                  #[cfg(test)] fn hidden() {}\n\
                  fn caller(job: Job) { start(); crate::tasks::run(); job.execute(); job.wrong(); hidden(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source), ("src/tasks.rs", "pub fn run() {}")],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    let calls = calls(&result, "src/lib.rs")
        .map(|occurrence| (source_of(occurrence, source), target(occurrence)))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            ("start()", "rift://symbol/local/rust/fixture/tasks/run"),
            (
                "crate::tasks::run()",
                "rift://symbol/local/rust/fixture/tasks/run"
            ),
            (
                "job.execute()",
                "rift://symbol/local/rust/fixture/Job/execute"
            ),
        ]
    );
    assert!(!result.exhausted);
}

#[test]
fn rust_shadowed_value_and_conditional_module_do_not_reach_decoys() {
    let source = "#[cfg(test)] mod tasks;\n\
                  fn run() {}\n\
                  fn caller(run: fn()) { run(); crate::tasks::run(); }";
    let result = resolution(
        "rust",
        &[("src/lib.rs", source), ("src/tasks.rs", "pub fn run() {}")],
        &[("Cargo.toml", "[package]\nname='fixture'\nversion='0.1.0'\n")],
        WORK_MAX,
    );
    assert_eq!(calls(&result, "src/lib.rs").count(), 0);
    assert_eq!(result.unresolved_sites, 2);
}
