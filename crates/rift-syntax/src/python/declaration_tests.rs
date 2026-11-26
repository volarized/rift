//! Declarations the Python rules emit beyond `def`, `class`, and one-name
//! assignments: methods, destructured and conditional module assignments,
//! `type` statements, instance attributes, and `__doc__` assignments.

use rift_core::ProjectPath;
use rift_protocol::read::SymbolKind;

use super::*;

fn analyze(text: &str) -> SyntaxDocument {
    let path = ProjectPath::new("pkg/models.py").expect("valid fixture path");
    let document = PythonSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("Python fixture must parse");
    assert!(!document.has_errors());
    document
}

/// Each declaration's qualified name, container, and kind word.
fn declared(document: &SyntaxDocument) -> Vec<(&str, Option<&str>, &str)> {
    document
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind != MODULE_KIND)
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.container.as_deref(),
                symbol.kind,
            )
        })
        .collect()
}

fn symbol<'document>(
    document: &'document SyntaxDocument,
    qualified: &str,
) -> &'document crate::SyntaxSymbol {
    document
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == qualified)
        .unwrap_or_else(|| panic!("fixture declares {qualified}: {:?}", declared(document)))
}

fn spelled(text: &str, range: crate::ByteRange) -> &str {
    let start = usize::try_from(range.start).expect("fixture start");
    let end = usize::try_from(range.end).expect("fixture end");
    &text[start..end]
}

#[test]
fn test_overloads_keep_source_evidence_beside_unrelated_local_imports() {
    let text = "from typing import overload\n@overload\ndef leaf(value: int) -> int: ...\ndef leaf(value): return value\ndef unrelated():\n    import os\n";
    let document = analyze(text);
    let functions = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.qualified_name == "leaf")
        .collect::<Vec<_>>();
    assert_eq!(functions.len(), 2);
    let Some(crate::PythonOverload::Overload {
        decorator, module, ..
    }) = functions[0].python_overload
    else {
        panic!("validated overload: {:?}", functions[0]);
    };
    assert_eq!(spelled(text, decorator), "overload");
    assert_eq!(spelled(text, module), "typing");
    assert_eq!(
        spelled(text, functions[0].name_range.expect("name")),
        "leaf"
    );
    assert_eq!(
        spelled(text, functions[1].item_range),
        "def leaf(value): return value"
    );
    assert_ne!(functions[0].name_range, functions[1].name_range);
}

#[test]
fn test_property_returns_keep_annotation_and_member_source() {
    let text = "class Item: pass\nclass Owner:\n    @property\n    def item(self) -> Item: pass\n    def use(self): return self.item.read()\n";
    let document = analyze(text);
    let getter = symbol(&document, "Owner.item");
    assert_eq!(getter.container.as_deref(), Some("Owner"));
    assert!(getter.kinds.contains(&SymbolKind::Property));
    let facts = document.facts().static_facts().expect("static facts");
    let binding = facts
        .bindings()
        .iter()
        .find(|binding| Some(binding.name) == getter.name_range)
        .expect("getter evidence");
    let crate::StaticBindingKind::PythonFunction {
        annotation,
        decorator,
    } = binding.kind
    else {
        panic!("return annotation: {binding:?}");
    };
    assert_eq!(spelled(text, annotation), "Item");
    assert_eq!(
        spelled(text, decorator.expect("property decorator")),
        "property"
    );
    assert!(
        facts
            .calls()
            .iter()
            .any(|call| spelled(text, call.callee_range) == "self.item.read")
    );
}

#[test]
fn test_wildcard_import_keeps_module_evidence_and_unknown_bindings() {
    let text = "from .models import *\n";
    let document = analyze(text);
    let facts = document.facts().static_facts().expect("static facts");
    let import = facts
        .bindings()
        .iter()
        .find(|binding| spelled(text, binding.name) == "*")
        .expect("wildcard import evidence");
    let crate::StaticBindingKind::PythonImport { module, export } = import.kind else {
        panic!("module import: {import:?}");
    };
    assert_eq!(spelled(text, module), ".models");
    assert!(export.is_none());
    assert_eq!(facts.unsupported_bindings(), [import.scope]);
    assert_eq!(spelled(text, import.scope), text);
    assert_eq!(declared(&document), []);
}

#[test]
fn test_annotated_locals_keep_the_enclosing_callable_and_source_range() {
    let text = "def build():\n    visited: list[str] = []\n    if True:\n        pending: int\n    def nested():\n        result: str = ''\n";
    let document = analyze(text);
    for (name, container, source) in [
        ("build.visited", "build", "visited: list[str] = []"),
        ("build.pending", "build", "pending: int"),
        ("build.nested.result", "build.nested", "result: str = ''"),
    ] {
        let declaration = symbol(&document, name);
        assert_eq!(declaration.container.as_deref(), Some(container));
        assert_eq!(spelled(text, declaration.range), source);
        assert_eq!(declaration.kinds, [SymbolKind::Value]);
    }
}

#[test]
fn test_empty_python_module_keeps_a_source_facet() {
    let document = analyze("");
    let module = symbol(&document, ".");
    assert_eq!(module.kind, "module");
    assert_eq!(module.range, crate::ByteRange { start: 0, end: 0 });
    assert_eq!(module.item_range, module.range);
    assert!(module.name_range.is_none());
}

#[test]
fn test_python_modules_without_tokens_keep_a_source_facet() {
    for text in ["\n", "   \n\n", "\r\n", " ", "\t\n", "# c\n"] {
        let document = analyze(text);
        let [module] = document.symbols() else {
            panic!("{text:?} declares its module alone");
        };
        assert_eq!(module.qualified_name, ".", "{text:?}");
        assert_eq!(module.kind, "module", "{text:?}");
        assert!(module.item_range.start < module.item_range.end, "{text:?}");
        assert!(
            module.range.end <= u64::try_from(text.len()).expect("text length"),
            "{text:?}"
        );
        assert_eq!(module.range, module.item_range, "{text:?}");
        if text.trim().is_empty() {
            assert_eq!(spelled(text, module.range), text);
        }
    }
}

/// A `def` directly in a class body declares a `method` member of the
/// class; `__init__` constructs it, `staticmethod` and `classmethod` belong
/// to the class, and `property` reads like a field. A class owns members,
/// can be instantiated, and can be inherited from.
#[test]
fn test_class_bodies_declare_methods_with_member_categories() {
    let document = analyze(
        "class Model:\n    def __init__(self, name):\n        pass\n\n    \
         @property\n    def label(self):\n        return 1\n\n    \
         @staticmethod\n    def make():\n        pass\n\n    \
         @classmethod\n    def build(cls):\n        pass\n\n    \
         def save(self):\n        def step():\n            pass\n\n\
         def helper():\n    pass\n",
    );
    let declarations = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind != MODULE_KIND)
        .map(|symbol| {
            (
                symbol.qualified_name.as_str(),
                symbol.kind,
                symbol.kinds.clone(),
            )
        })
        .collect::<Vec<_>>();
    let method = |extra: &[SymbolKind]| {
        let mut kinds = vec![SymbolKind::Value, SymbolKind::Callable, SymbolKind::Member];
        kinds.extend_from_slice(extra);
        kinds
    };
    assert_eq!(
        declarations,
        [
            (
                "Model",
                "class",
                vec![
                    SymbolKind::Type,
                    SymbolKind::MemberContainer,
                    SymbolKind::Constructible,
                    SymbolKind::Extensible,
                ]
            ),
            (
                "Model.__init__",
                "method",
                method(&[SymbolKind::Constructor])
            ),
            (
                "Model.__init__.name",
                "parameter",
                vec![SymbolKind::Value, SymbolKind::Parameter]
            ),
            ("Model.label", "method", method(&[SymbolKind::Property])),
            ("Model.make", "method", method(&[SymbolKind::Static])),
            ("Model.build", "method", method(&[SymbolKind::Static])),
            ("Model.save", "method", method(&[])),
            (
                "Model.save.step",
                "function",
                vec![SymbolKind::Value, SymbolKind::Callable]
            ),
            (
                "helper",
                "function",
                vec![SymbolKind::Value, SymbolKind::Callable]
            ),
        ]
    );
}

/// A tuple or list target declares one variable per bound name, a chained
/// assignment declares each target, and a `type` statement declares a type
/// alias.
#[test]
fn test_module_assignments_declare_every_bound_name_and_type_statements() {
    let text = "A, B = 1, 2\n(C, D) = 3, 4\n[E, *F] = 5, 6\nG = H = 7\n\
                type Pair = tuple[int, int]\nobj.attr, I = 8, 9\n";
    let document = analyze(text);
    assert_eq!(
        declared(&document),
        [
            ("A", None, "variable"),
            ("B", None, "variable"),
            ("C", None, "variable"),
            ("D", None, "variable"),
            ("E", None, "variable"),
            ("F", None, "variable"),
            ("G", None, "variable"),
            ("H", None, "variable"),
            ("Pair", None, "type_alias"),
            ("I", None, "variable"),
        ]
    );
    assert_eq!(spelled(text, symbol(&document, "B").range), "B");
    assert_eq!(
        symbol(&document, "Pair").kinds,
        [SymbolKind::Type, SymbolKind::Alias]
    );
    assert_eq!(
        spelled(text, symbol(&document, "Pair").range),
        "type Pair = tuple[int, int]"
    );
}

/// An assignment inside a module-level `if`, `elif`, `else`, `try`,
/// `except`, or `finally` suite declares a module variable; one inside a
/// function stays local.
#[test]
fn test_module_level_conditional_suites_declare_module_variables() {
    let document = analyze(
        "if True:\n    FLAG = 1\nelif False:\n    FLAG = 2\nelse:\n    OTHER = 3\n\
         try:\n    import json\nexcept ImportError:\n    json = None\nfinally:\n    DONE = 1\n\
         def run():\n    if True:\n        local = 1\n\
         class Config:\n    if True:\n        DEBUG = 1\n",
    );
    assert_eq!(
        declared(&document),
        [
            ("FLAG", None, "variable"),
            ("FLAG", None, "variable"),
            ("OTHER", None, "variable"),
            ("json", None, "variable"),
            ("DONE", None, "variable"),
            ("run", None, "function"),
            ("run.local", Some("run"), "variable"),
            ("Config", None, "class"),
            ("Config.DEBUG", Some("Config"), "variable"),
        ]
    );
}

/// An attribute a method assigns through its first parameter declares a
/// member variable of the class; an assignment through another object or
/// in a nested function declares nothing.
#[test]
fn test_instance_attributes_assigned_in_methods_declare_class_members() {
    let text = "class Model:\n    kind = 'model'\n    def __init__(self, name):\n        \
                self.name = name\n        self.a, self.b = 1, 2\n        \
                if name:\n            self.c: int = 3\n        other.d = 4\n        \
                def inner():\n            self.e = 5\n    \
                def reset(this):\n        this.name = None\n";
    let document = analyze(text);
    assert_eq!(
        declared(&document),
        [
            ("Model", None, "class"),
            ("Model.kind", Some("Model"), "variable"),
            ("Model.__init__", Some("Model"), "method"),
            ("Model.__init__.name", Some("Model.__init__"), "parameter"),
            ("Model.name", Some("Model"), "variable"),
            ("Model.a", Some("Model"), "variable"),
            ("Model.b", Some("Model"), "variable"),
            ("Model.c", Some("Model"), "variable"),
            ("Model.__init__.inner", Some("Model.__init__"), "function"),
            ("Model.reset", Some("Model"), "method"),
            ("Model.name", Some("Model"), "variable"),
        ]
    );
    let parameter = symbol(&document, "Model.__init__.name");
    assert_eq!(parameter.name_range, Some(parameter.item_range));
    assert_eq!(spelled(text, parameter.item_range), "name");
    assert_eq!(parameter.kinds, [SymbolKind::Value, SymbolKind::Parameter]);
    let name = symbol(&document, "Model.name");
    assert_eq!(name.kinds, [SymbolKind::Value, SymbolKind::Member]);
    assert_eq!(spelled(text, name.range), "self.name = name");
    assert_eq!(
        name.name_range.map(|range| spelled(text, range)),
        Some("name")
    );
    assert_eq!(
        symbol(&document, "Model.kind").kinds,
        [SymbolKind::Value, SymbolKind::Member]
    );
}

/// A module-level `name.__doc__ = "literal"` documents the module-level
/// declaration it names; a formatted string or a nested assignment does not.
#[test]
fn test_a_literal_doc_assignment_documents_the_named_declaration() {
    let text = "def keys(d):\n    pass\n\ndef values(d):\n    pass\n\n\
                keys.__doc__ = \"Return an iterator over keys.\"\ncallback = lambda: None\ncallback.__doc__ = \"Run callback.\"\n\
                values.__doc__ = f\"Return {1}.\"\n\
                def wrap():\n    keys.__doc__ = 'Nested.'\n";
    let document = analyze(text);
    let keys = symbol(&document, "keys");
    assert_eq!(
        keys.documentation.as_ref(),
        [Documentation {
            format: DocumentationFormat::Plain,
            text: "Return an iterator over keys.".to_owned(),
        }]
    );
    assert!(
        keys.documentation_ranges.is_empty(),
        "the assigned string sits outside the declaration"
    );
    assert!(symbol(&document, "values").documentation.is_empty());
    let callback = symbol(&document, "callback");
    assert_eq!(callback.documentation[0].text, "Run callback.");
    assert!(callback.documentation_ranges.is_empty());
}

/// A string statement directly after an assignment that declares one name
/// documents that name, at module level, in a class body, and for an
/// instance attribute; after a tuple target, or as a formatted string, it
/// documents nothing.
#[test]
fn test_a_string_after_an_assignment_documents_the_assigned_name() {
    let text = "LIMIT = 10\n\"\"\"Most rows per page.\"\"\"\n\
                A, B = 1, 2\n\"Pair.\"\nG = H = 1\n\"Chain.\"\n\
                class Model:\n    kind: str = 'model'\n    \"\"\"The model kind.\"\"\"\n    \
                late = 1\n    f\"Formatted {late}.\"\n    \
                def __init__(self):\n        self.name = 'x'\n        \"The display name.\"\n";
    let document = analyze(text);
    let doc = |qualified: &str| {
        symbol(&document, qualified)
            .documentation
            .first()
            .map(|doc| doc.text.clone())
    };
    assert_eq!(doc("LIMIT").as_deref(), Some("Most rows per page."));
    let limit = symbol(&document, "LIMIT");
    assert_eq!(
        spelled(text, limit.documentation_ranges[0]),
        "Most rows per page."
    );
    assert_eq!(
        spelled(text, limit.range),
        "LIMIT = 10\n\"\"\"Most rows per page.\"\"\""
    );
    assert_eq!(doc("A"), None);
    assert_eq!(doc("B"), None);
    assert_eq!(doc("G"), None);
    assert_eq!(doc("H"), None);
    assert_eq!(doc("Model.kind").as_deref(), Some("The model kind."));
    assert_eq!(doc("Model.late"), None);
    assert_eq!(doc("Model.name").as_deref(), Some("The display name."));
}

/// A module-level `from module import name as name`, the explicit
/// re-export spelling, records an export binding naming the imported name,
/// the exported name, and the module; a renamed, plain, nested, or dotted
/// import records none.
#[test]
fn test_redundant_import_aliases_record_export_bindings() {
    let text = "from .applications import FastAPI as FastAPI, Other\n\
                from pkg.models import Model as Model\n\
                from . import routing as routing\n\
                from .x import a as b\nfrom .y import c\nimport os as os\n\
                try:\n    from ._speed import fast as fast\nexcept ImportError:\n    pass\n\
                def run():\n    from .z import d as d\n";
    let document = analyze(text);
    let bindings = document
        .facts()
        .export_bindings()
        .expect("Python captures export bindings")
        .iter()
        .map(|binding| {
            (
                binding.kind,
                binding.local.map(|range| spelled(text, range)),
                binding.exported.map(|range| spelled(text, range)),
                binding.source.map(|range| spelled(text, range)),
            )
        })
        .collect::<Vec<_>>();
    let named = crate::SyntaxExportKind::Named;
    assert_eq!(
        bindings,
        [
            (
                named,
                Some("FastAPI"),
                Some("FastAPI"),
                Some(".applications")
            ),
            (named, Some("Model"), Some("Model"), Some("pkg.models")),
            (named, Some("routing"), Some("routing"), Some(".")),
            (named, Some("fast"), Some("fast"), Some("._speed")),
        ]
    );
}

#[test]
fn test_module_declaration_retains_documentation_and_distinct_bound_names() {
    let text = "# Module comment.\n\"\"\"Module docstring.\"\"\"\nmodule = 1\n";
    let document = analyze(text);
    let module = document
        .symbols()
        .iter()
        .find(|symbol| symbol.kind == "module")
        .expect("each Python file declares its module");
    assert_eq!(module.qualified_name, ".");
    assert_eq!(module.kinds, [SymbolKind::Namespace, SymbolKind::Module]);
    assert_eq!(module.container, None);
    assert_eq!(spelled(text, module.range), text);
    assert_eq!(module.documentation[0].text, "Module docstring.");
    assert_eq!(
        spelled(text, module.documentation_ranges[0]),
        "Module docstring."
    );
    assert_eq!(symbol(&document, "module").kind, "variable");
    assert_eq!(symbol(&analyze(""), ".").kind, "module");
    for text in ["# Comment.\n", "f\"{value}\"\n", "b\"bytes\"\n"] {
        let document = analyze(text);
        let module = document
            .symbols()
            .iter()
            .find(|symbol| symbol.kind == "module")
            .expect("a module without documentation still declares");
        assert!(module.documentation.is_empty());
    }
}

#[test]
fn test_python_member_accesses_keep_assignment_roles() {
    use rift_core::ReferenceRole;
    let text = "class Task:\n    def run(self):\n        self.first, self.second = (1, 2)\n        self.first += 1\n        self.declared: int\n        self.child.value = 2\n";
    let document = analyze(text);
    let accesses = document
        .facts()
        .static_facts()
        .expect("captured facts")
        .references()
        .iter()
        .filter(|reference| reference.role != ReferenceRole::Type)
        .map(|reference| (spelled(text, reference.target), reference.role))
        .collect::<Vec<_>>();
    assert_eq!(
        accesses,
        [
            ("self.first", ReferenceRole::Write),
            ("self.second", ReferenceRole::Write),
            ("self.first", ReferenceRole::Read),
            ("self.first", ReferenceRole::Write),
            ("self.child.value", ReferenceRole::Write),
            ("self.child", ReferenceRole::Read),
        ]
    );
}

#[test]
fn test_python_rebindings_keep_the_declared_variable_scope() {
    let text = "shared = 0\ndef root():\n    active = False\n    def finish():\n        nonlocal active\n        active = True\n    def publish():\n        global shared\n        shared = 1\n";
    let document = analyze(text);
    for (name, qualified, container) in [
        ("active", "root.active", Some("root")),
        ("shared", "shared", None),
    ] {
        let declarations = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == name)
            .collect::<Vec<_>>();
        assert_eq!(declarations.len(), 2);
        for (ordinal, symbol) in declarations.into_iter().enumerate() {
            assert_eq!(symbol.qualified_name, qualified);
            assert_eq!(symbol.container.as_deref(), container);
            assert_eq!(symbol.declaration_ordinal, ordinal);
        }
    }
}

#[test]
fn test_python_rebindings_keep_class_members_and_skip_class_scopes() {
    let source = "shared = 0\nclass Holder:\n    global shared\n    shared = 1\n    def run(self):\n        self.shared = 2\ndef root():\n    value = 0\n    class Inner:\n        value = 1\n        def run(self):\n            nonlocal value\n            value = 2\n";
    let document = analyze(source);
    let values = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.name == "value")
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(values, ["root.value", "root.Inner.value", "root.value"]);
    let shared = document
        .symbols()
        .iter()
        .filter(|symbol| symbol.name == "shared")
        .collect::<Vec<_>>();
    assert_eq!(
        shared
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>(),
        ["shared", "shared", "Holder.shared"]
    );
    assert!(!shared[1].kinds.contains(&SymbolKind::Member));
    assert!(shared[2].kinds.contains(&SymbolKind::Member));
}
