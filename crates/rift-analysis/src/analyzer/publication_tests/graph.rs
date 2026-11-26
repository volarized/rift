//! Captured graph projection, source occurrences, and independent coverage.

use super::*;
use rift_core::{
    Contribution, ContributionEdge, ContributionKey, ContributionReference, DeclarationBinding,
    IndexRevision, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
    SourceApplicability, SourceRange, SourceRevision, TreeRevision,
};
use rift_protocol::read::{EdgeDerivation, EdgeKind, ExactKind};
use rift_provider::{Normalizer, ProviderPublication, PublicationLimits, PublicationSet};

const SOURCE: &str = "// é\npub fn start() { finish(); finish(); }\npub fn finish() {}\n";

#[test]
fn package_stores_python_direct_function_local_reads() {
    let source = "def choose(parameter: int) -> int:\n    local = parameter\n    def nested() -> int:\n        local = 7\n        return local\n    return local + nested()\n";
    let result = publication(&[("click.py", source)], None, &["click"]);
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let rows = result
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/local"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    for (scope, text) in [("choose/nested", "local\n"), ("choose", "local +")] {
        let owner = format!("{module}/{scope}");
        let target = format!("{owner}/local");
        let row = rows
            .iter()
            .find(|row| row.to.as_str() == target)
            .expect("local target");
        assert_eq!(row.from.as_str(), owner);
        let site = row.occurrence.as_ref().expect("source use");
        let start = u64::try_from(source.find(text).expect("written read")).expect("offset");
        assert_eq!(site.range.start, start);
        assert_eq!(site.range.end, start + 5);
        assert!(
            result
                .declarations
                .iter()
                .any(|entry| entry.symbol.as_str() == target)
        );
    }
}

const PYTHON_REASSIGNED_LOCAL_SOURCE: &str = concat!(
    "def reassigned() -> int:\n",
    "    local = 1\n",
    "    local = 2\n",
    "    return local\n",
    "def outer() -> int:\n",
    "    local = 9\n",
    "    def read() -> int:\n",
    "        return local\n",
    "    return read()\n",
    "def nested_scope():\n",
    "    local = 3\n",
    "    local = 4\n",
    "    def read():\n",
    "        local = 5\n",
    "        return local\n",
    "    return local + read()\n",
    "def removed():\n",
    "    local = 6\n",
    "    local = 7\n",
    "    del local\n",
    "    return local\n",
    "def expired():\n",
    "    local = 8\n",
    "    local = 9\n",
    "    try:\n",
    "        raise ValueError()\n",
    "    except ValueError as local:\n",
    "        pass\n",
    "    return local\n",
);

#[test]
fn package_stores_python_reassigned_local_reads_in_their_own_scope() {
    let source = PYTHON_REASSIGNED_LOCAL_SOURCE;
    let result = publication(&[("click.py", source)], None, &["click"]);
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let id = |scope| format!("{module}/{scope}");
    let declarations: Vec<_> = result
        .declarations
        .iter()
        .filter(|declaration| declaration.symbol.as_str() == id("reassigned/local"))
        .map(|declaration| declaration.range.start)
        .collect();
    let start = |anchor| {
        u64::try_from(source.find(anchor).expect("authored assignment")).expect("byte offset")
    };
    assert_eq!(declarations, [start("local = 1"), start("local = 2")]);

    let reads: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/local"))
        .collect();
    assert_eq!(reads.len(), 4);
    for (from, to, context) in [
        ("reassigned", "reassigned", "local = 2\n    return local"),
        (
            "outer/read",
            "outer",
            "def read() -> int:\n        return local",
        ),
        ("nested_scope", "nested_scope", "return local +"),
        (
            "nested_scope/read",
            "nested_scope/read",
            "local = 5\n        return local",
        ),
    ] {
        let offset = start(context)
            + u64::try_from(context.rfind("local").expect("written name")).expect("local offset");
        assert!(
            reads.iter().any(|edge| {
                edge.from.as_str() == id(from)
                    && edge.to.as_str() == format!("{}/local", id(to))
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.range.start == offset && site.range.end == offset + 5
                    })
            }),
            "{from} to {to} at {offset}"
        );
    }
    assert!(result.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str() == id("outer")
            && edge.to.as_str() == id("outer/read")
    }));
}

const PYTHON_CAPTURE_REFUSALS: &str = concat!(
    "def shadowed():\n",
    "    local = 1\n",
    "    def pick():\n",
    "        return local\n",
    "        local = 2\n",
    "    return pick()\n",
    "def redirected():\n",
    "    local = 1\n",
    "    def rewrite():\n",
    "        nonlocal local\n",
    "        local = 3\n",
    "        return local\n",
    "    def pick():\n",
    "        return local\n",
    "    return rewrite() + pick()\n",
    "def removed_nonlocal():\n",
    "    local = 1\n",
    "    local = 2\n",
    "    def erase():\n",
    "        nonlocal local\n",
    "        del local\n",
    "    erase()\n",
    "    return local\n",
    "def removed_outer():\n",
    "    local = 1\n",
    "    def pick():\n",
    "        return local\n",
    "    del local\n",
    "    return pick()\n",
);

#[test]
fn package_refuses_shadowed_redirected_or_deleted_python_captures() {
    let result = publication(&[("click.py", PYTHON_CAPTURE_REFUSALS)], None, &["click"]);
    assert!(
        !result.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/local")
        })
    );
    for (from, to) in [
        ("/shadowed", "/shadowed/pick"),
        ("/redirected", "/redirected/rewrite"),
        ("/redirected", "/redirected/pick"),
        ("/removed_nonlocal", "/removed_nonlocal/erase"),
        ("/removed_outer", "/removed_outer/pick"),
    ] {
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.from.as_str().ends_with(from)
                    && edge.to.as_str().ends_with(to)
            }),
            "{from} to {to}"
        );
    }
}

const PYTHON_PARAMETER_SOURCE: &str = concat!(
    "parameter = 9\n",
    "def choose(parameter: int) -> int:\n",
    "    local = parameter\n",
    "    def nested() -> int:\n",
    "        local = 7\n",
    "        return local\n",
    "    return local + nested()\n",
    "def other(parameter=parameter):\n",
    "    return parameter\n",
    "def owner(parameter):\n",
    "    def nested(parameter):\n",
    "        return parameter\n",
    "    return parameter\n",
    "def rebind(parameter):\n",
    "    parameter = 3\n",
    "    return parameter\n",
    "class Receiver:\n",
    "    def run(self, *args, **kwargs):\n",
    "        return self, args, kwargs\n",
);

#[test]
fn package_stores_python_function_parameter_names_and_reads() {
    let source = PYTHON_PARAMETER_SOURCE;
    let result = publication(&[("click.py", source)], None, &["click"]);
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let id = |relative: &str| format!("{module}/{relative}");
    let offset = |prefix: &str, spelling: &str| {
        u64::try_from(
            source.find(prefix).expect("written site")
                + prefix.rfind(spelling).expect("token within site"),
        )
        .expect("offset")
    };
    let read_at = |from: &str, to: &str, at| {
        result.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge.from.as_str() == id(from)
                && edge.to.as_str() == id(to)
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range.start == at && site.range.end == at + 9)
        })
    };
    for (owner, signature, use_site) in [
        ("choose", "choose(parameter", "local = parameter"),
        (
            "other",
            "other(parameter",
            "other(parameter=parameter):\n    return parameter",
        ),
        (
            "owner",
            "owner(parameter",
            "        return parameter\n    return parameter",
        ),
        (
            "owner/nested",
            "nested(parameter",
            "        return parameter",
        ),
    ] {
        let target = format!("{owner}/parameter");
        assert!(
            result.objects.iter().any(|object| {
                object
                    .id
                    .as_ref()
                    .is_some_and(|symbol| symbol.as_str() == id(&target))
                    && object.kinds.contains(&SymbolKind::Parameter)
            }),
            "{target}"
        );
        let name = offset(signature, "parameter");
        assert!(
            result.declarations.iter().any(|declaration| {
                declaration.symbol.as_str() == id(&target)
                    && declaration.range.start == name
                    && declaration.range.end == name + 9
                    && declaration.source == "parameter"
            }),
            "{target}"
        );
        assert!(
            read_at(owner, &target, offset(use_site, "parameter")),
            "{target}"
        );
    }
    let default = offset("other(parameter=parameter", "parameter");
    assert!(read_at("other", "parameter", default));
    assert!(!read_at("other", "other/parameter", default));
    assert!(result.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str() == id("choose")
            && edge.to.as_str() == id("choose/nested")
    }));

    let rebind = id("rebind/parameter");
    let declarations = result
        .declarations
        .iter()
        .filter(|declaration| declaration.symbol.as_str() == rebind)
        .collect::<Vec<_>>();
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0].source, "parameter = 3");
    assert!(!result.objects.iter().any(|object| {
        object.kinds.contains(&SymbolKind::Parameter)
            && object.id.as_ref().is_some_and(|id| id.as_str() == rebind)
    }));
}

const PYTHON_RECEIVER_SOURCE: &str = concat!(
    "class Counter:\n",
    "    def current(self):\n",
    "        return self\n",
    "    def call_current(self):\n",
    "        return self.current()\n",
    "    @classmethod\n",
    "    def factory(cls):\n",
    "        return cls()\n",
    "    @staticmethod\n",
    "    def identity(self):\n",
    "        return self\n",
    "def free(value):\n",
    "    return value\n",
);

fn is_python_receiver_fixture_name(symbol: &str) -> bool {
    ["/current/self", "/call_current/self", "/factory/cls"]
        .iter()
        .any(|suffix| symbol.ends_with(suffix))
}

#[test]
fn package_retains_python_method_receiver_declarations() {
    let source = PYTHON_RECEIVER_SOURCE;
    let files = [("click.py", source)];
    let complete = publication(&files, None, &["click"]);
    let parameter =
        |owner, name| format!("rift://symbol/pypi/pypi.org/click/python/click/{owner}/{name}");
    for (owner, written, name) in [
        ("Counter/current", "def current(self)", "self"),
        ("Counter/call_current", "def call_current(self)", "self"),
        ("Counter/factory", "def factory(cls)", "cls"),
        ("Counter/identity", "def identity(self)", "self"),
        ("free", "def free(value)", "value"),
    ] {
        let symbol = parameter(owner, name);
        let start = source.find(written).expect("authored method")
            + written.find(name).expect("parameter name");
        let range = rift_protocol::read::TextRange {
            start: u64::try_from(start).expect("start"),
            end: u64::try_from(start + name.len()).expect("end"),
        };
        assert!(
            complete.declarations.iter().any(|entry| {
                entry.symbol.as_str() == symbol && entry.range == range && entry.source == name
            }),
            "{symbol}"
        );
        assert!(
            complete.objects.iter().any(|object| {
                object.id.as_ref().is_some_and(|id| id.as_str() == symbol)
                    && object.kinds.contains(&SymbolKind::Parameter)
            }),
            "{symbol}"
        );
    }
    let prior = publication_with_configuration(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration {
            symbols: 9,
            ..rift_protocol::configuration::PackageConfiguration::default()
        },
    );
    assert_eq!(
        prior.declarations,
        complete
            .declarations
            .iter()
            .filter(|entry| !is_python_receiver_fixture_name(entry.symbol.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        prior.edges,
        complete
            .edges
            .iter()
            .filter(|entry| !is_python_receiver_fixture_name(entry.to.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    );
    for (caller, target) in [
        ("/Counter/call_current", "/Counter/current"),
        ("/Counter/factory", "/Counter"),
    ] {
        assert!(
            prior.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.from.as_str().ends_with(caller)
                    && edge.to.as_str().ends_with(target)
            }),
            "{caller}"
        );
    }
}

#[test]
fn package_python_method_receiver_reads_follow_earlier_edges() {
    let files = [("click.py", PYTHON_RECEIVER_SOURCE)];
    let complete = publication(&files, None, &["click"]);
    for (owner, token, written) in [
        ("/Counter/current", "self", "return self\n"),
        ("/Counter/call_current", "self", "return self.current()"),
        ("/Counter/factory", "cls", "return cls()"),
    ] {
        let target = format!("{owner}/{token}");
        let start = PYTHON_RECEIVER_SOURCE.find(written).expect("authored use") + "return ".len();
        assert!(
            complete.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.from.as_str().ends_with(owner)
                    && edge.to.as_str().ends_with(&target)
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.range.start == u64::try_from(start).expect("start")
                            && site.range.end == u64::try_from(start + token.len()).expect("end")
                    })
            }),
            "{target}"
        );
    }
    let prior = complete
        .edges
        .iter()
        .filter(|edge| !is_python_receiver_fixture_name(edge.to.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let tight = python_publication_with_limits(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(prior.len()),
    );
    assert_eq!(tight.declarations, complete.declarations);
    assert_eq!(tight.edges, prior);
}

#[test]
fn package_python_loop_targets_keep_authored_declarations() {
    const SOURCE: &str = concat!(
        "def collect(items):\n",
        "    value = []\n",
        "    for item in items:\n",
        "        value.append(item)\n",
        "    return value\n",
        "def pairs(items):\n",
        "    value = []\n",
        "    for key, amount in items:\n",
        "        value.append((key, amount))\n",
        "    return value\n",
    );
    let files = [("click.py", SOURCE)];
    let complete = publication(&files, None, &["click"]);
    for (owner, name, written) in [
        ("collect", "item", "for item in"),
        ("pairs", "key", "for key, amount in"),
        ("pairs", "amount", "for key, amount in"),
    ] {
        let symbol = format!("rift://symbol/pypi/pypi.org/click/python/click/{owner}/{name}");
        let start =
            SOURCE.find(written).expect("written loop") + written.find(name).expect("target name");
        assert!(
            complete.declarations.iter().any(|entry| {
                entry.symbol.as_str() == symbol
                    && entry.source == name
                    && entry.range.start == u64::try_from(start).expect("start")
                    && entry.range.end == u64::try_from(start + name.len()).expect("end")
            }),
            "{symbol}"
        );
    }
    let limited = publication_with_configuration(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration {
            symbols: 7,
            ..rift_protocol::configuration::PackageConfiguration::default()
        },
    );
    assert_eq!(
        limited.declarations,
        complete
            .declarations
            .iter()
            .filter(|entry| {
                !["/collect/item", "/pairs/key", "/pairs/amount"]
                    .iter()
                    .any(|suffix| entry.symbol.as_str().ends_with(suffix))
            })
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(limited.edges, complete.edges);
}

#[test]
fn package_python_loop_targets_respect_existing_names_and_scope_redirects() {
    const SOURCE: &str = concat!(
        "global_item = 1\n",
        "def outer(items):\n",
        "    prior = 1\n",
        "    def nested():\n",
        "        global global_item\n",
        "        nonlocal prior\n",
        "        for global_item, prior in items:\n",
        "            pass\n",
        "    for prior in items:\n",
        "        pass\n",
        "    return nested\n",
        "async def receive(items):\n",
        "    async for item in items:\n",
        "        return item\n",
    );
    let result = publication(&[("click.py", SOURCE)], None, &["click"]);
    assert_eq!(
        result
            .declarations
            .iter()
            .filter(|entry| entry.symbol.as_str().ends_with("/outer/prior"))
            .map(|entry| entry.source.as_str())
            .collect::<Vec<_>>(),
        ["prior = 1"]
    );
    assert!(!result.declarations.iter().any(|entry| {
        ["/nested/global_item", "/nested/prior"]
            .iter()
            .any(|suffix| entry.symbol.as_str().ends_with(suffix))
    }));
    assert!(result.declarations.iter().any(|entry| {
        entry.symbol.as_str().ends_with("/receive/item") && entry.source == "item"
    }));
}

#[test]
fn package_python_variadic_parameters_keep_names_and_reads() {
    const SOURCE: &str = concat!(
        "def collect(*values: int, **options: str):\n",
        "    return values, options\n",
        "class Control:\n",
        "    def existing(self, value):\n",
        "        return self, value\n"
    );
    let files = [("click.py", SOURCE)];
    let complete = publication(&files, None, &["click"]);
    for (name, signature) in [("values", "*values"), ("options", "**options")] {
        let symbol = format!("rift://symbol/pypi/pypi.org/click/python/click/collect/{name}");
        let declaration_start = SOURCE.find(signature).expect("written parameter")
            + signature.find(name).expect("parameter name");
        let read_start = SOURCE.rfind(name).expect("written read");
        assert!(
            complete.declarations.iter().any(|entry| {
                entry.symbol.as_str() == symbol
                    && entry.source == name
                    && entry.range.start == u64::try_from(declaration_start).expect("start")
                    && entry.range.end
                        == u64::try_from(declaration_start + name.len()).expect("end")
            }),
            "{symbol}"
        );
        assert!(
            complete.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.from.as_str().ends_with("/collect")
                    && edge.to.as_str() == symbol
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        site.range.start == u64::try_from(read_start).expect("start")
                            && site.range.end
                                == u64::try_from(read_start + name.len()).expect("end")
                    })
            }),
            "{symbol}"
        );
    }
    let prior = complete
        .edges
        .iter()
        .filter(|edge| {
            !["/collect/values", "/collect/options"]
                .iter()
                .any(|suffix| edge.to.as_str().ends_with(suffix))
        })
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(prior.len()),
    );
    assert_eq!(limited.declarations, complete.declarations);
    assert_eq!(limited.edges, prior);
    assert!(limited.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/existing/self")
    }));
}

#[test]
fn package_python_parameter_keeps_original_symbols_and_local_reads_under_caps() {
    const SOURCE: &str = concat!(
        "def helper(): return 1\n",
        "def choose(parameter):\n",
        "    local = parameter\n",
        "    local + helper()\n",
        "    return local\n",
    );
    let files = [("click.py", SOURCE)];
    let original = publication(&files, None, &["click"]);
    let parameter = |id: &str| id.ends_with("/choose/parameter");
    let symbol_limited = publication_with_configuration(
        &files,
        None,
        &["click"],
        rift_protocol::configuration::PackageConfiguration {
            symbols: 4,
            ..rift_protocol::configuration::PackageConfiguration::default()
        },
    );
    assert_eq!(
        symbol_limited.declarations,
        original
            .declarations
            .iter()
            .filter(|entry| !parameter(entry.symbol.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        symbol_limited.edges,
        original
            .edges
            .iter()
            .filter(|edge| !parameter(edge.to.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    );
    assert!(
        symbol_limited
            .edges
            .iter()
            .any(|edge| edge.kinds == [EdgeKind::Calls])
    );
    assert_eq!(
        symbol_limited
            .edges
            .iter()
            .filter(|edge| {
                edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/choose/local")
            })
            .count(),
        2
    );
    assert!(
        original
            .edges
            .iter()
            .any(|edge| { edge.kinds == [EdgeKind::Reads] && parameter(edge.to.as_str()) })
    );
}

#[test]
fn package_imports_keep_physical_declarations_in_paired_python_modules() {
    for (module, source_path, stub_path, paired_importer) in [
        ("pkg.model", "pkg/model.py", "pkg/model.pyi", false),
        ("pkg.model", "pkg/model.py", "pkg/model.pyi", true),
        ("pkg", "pkg/__init__.py", "pkg/__init__.pyi", false),
    ] {
        assert_paired_python_module_edges(module, source_path, stub_path, paired_importer);
    }
}

fn assert_paired_python_module_edges(
    module: &str,
    source_path: &str,
    stub_path: &str,
    paired_importer: bool,
) {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    let implementation = "def build(value): return value\nclass Base: pass\n\
                          def local():\n    class StubOnly: pass\n    return StubOnly\n";
    let stub = "def build(value: int) -> int: ...\nclass Base: ...\nclass StubOnly: ...\n";
    let importer = format!(
        "from {module} import build, Base, StubOnly\nimport {module} as joined\n\
         class Child(Base): pass\n\
         def direct(value: StubOnly): return build(1)\n\
         def qualified(): return joined.build(2)\n\
         def read(): return joined.build\n"
    );
    let analysis = paired_python_analysis(
        module,
        (source_path, implementation),
        (stub_path, stub),
        paired_importer,
        &importer,
    );
    let (publication, _, semantics, _) = analysis.into_parts();
    publication.validate_facts().expect("valid package facts");
    let module_id = module.replace('.', "/");
    let importer_unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "pkg/consumer.py")
        .expect("published importer")
        .unit;
    for (role, kind, written, name) in [
        (ReferenceRole::Import, EdgeKind::Imports, "build", "build"),
        (
            ReferenceRole::Import,
            EdgeKind::Imports,
            "StubOnly",
            "StubOnly",
        ),
        (ReferenceRole::Type, EdgeKind::Extends, "Base", "Base"),
        (
            ReferenceRole::Type,
            EdgeKind::HasType,
            "StubOnly",
            "StubOnly",
        ),
        (ReferenceRole::Call, EdgeKind::Calls, "build(1)", "build"),
        (
            ReferenceRole::Call,
            EdgeKind::Calls,
            "joined.build(2)",
            "build",
        ),
        (
            ReferenceRole::Read,
            EdgeKind::Reads,
            "joined.build",
            "build",
        ),
    ] {
        let (path, source, declaration) = match name {
            "Base" => (source_path, implementation, "class Base: pass"),
            "StubOnly" => (stub_path, stub, "class StubOnly: ..."),
            _ => (
                source_path,
                implementation,
                "def build(value): return value",
            ),
        };
        let target = format!("/{module_id}/{name}");
        let mut observed = semantics.static_occurrences().iter().filter(|row| {
            row.role == role
                && row.binding.unit().key().as_str() == "pkg/consumer.py"
                && source_span(&importer, row.binding.range()) == written
                && matches!(&row.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(&target))
        }).peekable();
        assert!(
            observed.peek().is_some(),
            "{module} {paired_importer}: {role:?} {written}"
        );
        for row in observed {
            let binding = row.target_binding.as_ref().expect("physical declaration");
            assert_eq!(binding.unit().key().as_str(), path);
            assert_eq!(source_span(source, binding.range()), declaration);
        }
        assert!(
            publication.edges.iter().any(|edge| {
                edge.kinds == [kind]
                    && edge.to.as_str().ends_with(&target)
                    && edge.occurrence.as_ref().is_some_and(|span| {
                        &span.unit == importer_unit
                            && &importer[usize::try_from(span.range.start).expect("site start")
                                ..usize::try_from(span.range.end).expect("site end")]
                                == written
                    })
            }),
            "{module} {paired_importer}: missing published {kind:?} at {written}"
        );
    }
}

fn paired_python_analysis(
    module: &str,
    implementation: (&str, &str),
    stub: (&str, &str),
    paired_importer: bool,
    importer: &str,
) -> PackageAnalysis {
    let mut sources = vec![implementation, stub, ("pkg/consumer.py", importer)];
    if module != "pkg" {
        sources.push(("pkg/__init__.py", ""));
    }
    if paired_importer {
        sources.push((
            "pkg/consumer.pyi",
            "class Child: ...\ndef direct(value): ...\n\
             def qualified(): ...\ndef read(): ...\n",
        ));
    }
    fixture::package_analysis(ShippedLanguage::Python, sources)
}

#[test]
fn paired_python_imports_refuse_a_competing_module() {
    let analysis = fixture::package_analysis(
        ShippedLanguage::Python,
        vec![
            ("pkg/__init__.py", ""),
            ("pkg/model.py", "def build(): return 1\n"),
            ("pkg/model.pyi", "def build() -> int: ...\n"),
            ("pkg/model/__init__.py", "def build(): return 2\n"),
            (
                "pkg/consumer.py",
                "from pkg.model import build\ndef run(): return build()\n",
            ),
        ],
    );
    assert!(!analysis.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/consumer/run")
            && edge.to.as_str().ends_with("/model/build")
    }));
}

fn source_span(source: &str, range: SourceRange) -> &str {
    let start = usize::try_from(range.start()).expect("source start");
    let end = usize::try_from(range.end()).expect("source end");
    &source[start..end]
}

#[test]
fn package_collects_python_generic_aliases_in_their_own_scope() {
    let source = "class T: pass\nclass Bound: pass\n\
                  type Plain = T\n\
                  type Values[T] = T\n\
                  type WithDefault[T = int] = T\n\
                  type Bounded[\n    T: Bound,  # bound of the alias\n] = list[T]\n\
                  def accept(value: Values[int]) -> Values[int]: return value\n";
    let result = publication(&[("click.py", source)], None, &["click"]);
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let identity = |name| format!("{module}/{name}");
    for alias in ["Plain", "Values", "WithDefault", "Bounded"] {
        let name = identity(alias);
        assert!(
            result
                .objects
                .iter()
                .any(|object| { object.id.as_ref().is_some_and(|id| id.as_str() == name) }),
            "{alias}"
        );
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Contains]
                    && edge.from.as_str() == module
                    && edge.to.as_str() == name
            }),
            "{alias}"
        );
    }
    let module_type = identity("T");
    let plain_alias = identity("Plain");
    let uses = result
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::HasType] && edge.to.as_str() == module_type)
        .map(|edge| edge.from.as_str())
        .collect::<Vec<_>>();
    assert_eq!(uses, [plain_alias.as_str()]);
    for (source, target) in [("Bounded", "Bound"), ("accept", "Values")] {
        let from = identity(source);
        let to = identity(target);
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str() == from
                    && edge.to.as_str() == to
            }),
            "{source} -> {target}"
        );
    }
}

#[test]
fn package_preserves_python_edges_beside_type_parameter_defaults() {
    let prefix = "import math\ndef leaf(): pass\ndef f(): leaf()\n";
    let module = "rift://symbol/pypi/pypi.org/click/python/click";
    let report = |suffix| {
        let source = format!("{prefix}{suffix}");
        publication(&[("click.py", &source)], None, &["click"])
    };
    let f = format!("{module}/f");
    let leaf = format!("{module}/leaf");
    for (suffix, bound) in [
        ("class A[T: int]: pass\n", true),
        ("class A[T = int]: pass\n", false),
        ("class A[T: int = int]: pass\n", true),
        (
            "def identity[T = leaf()](value: T) -> T: return value\n",
            false,
        ),
        ("type Values[T = int] = list[T]\n", false),
        (
            "class A[*Ts = *tuple[int, str], **P = [int, str]]: pass\n",
            false,
        ),
    ] {
        let result = report(suffix);
        let calls = result
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls])
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(calls, [(f.as_str(), leaf.as_str())], "{suffix}");
        assert!(
            !result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.from.as_str().ends_with("/identity")
                    && edge.to.as_str() == leaf
            }),
            "{suffix}"
        );
        assert_eq!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str().ends_with("/A")
                    && edge.to.as_str().ends_with("/builtins/int")
            }),
            bound,
            "{suffix}"
        );
        for member in [&f, &leaf] {
            assert!(
                result.edges.iter().any(|edge| {
                    edge.kinds == [EdgeKind::Contains]
                        && edge.from.as_str() == module
                        && edge.to.as_str() == member.as_str()
                }),
                "{suffix}"
            );
        }
        assert!(
            result
                .edges
                .iter()
                .any(|edge| { edge.kinds == [EdgeKind::Imports] && edge.from.as_str() == module }),
            "{suffix}"
        );
    }
    for suffix in [
        "class A[T = : pass\n",
        "class A[41 = int]: pass\n",
        "class A[T = *tuple[int]]: pass\n",
        "class A[**P = *tuple[int]]: pass\n",
        "def invalid(value: list[T = int]): pass\n",
    ] {
        assert!(report(suffix).edges.is_empty(), "{suffix}");
    }
}

const TYPESCRIPT_PARAMETER_SOURCE: &str = concat!(
    "export function choose<Value>(parameter: Value): Value {\n",
    "  const local = parameter;\n",
    "  { const local = parameter; void local; }\n",
    "  return local;\n",
    "}\n",
    "export const seed = 3;\n",
    "export function defaults(parameter: number = seed, fallback: number = parameter): number {\n",
    "  const seed = 4;\n",
    "  { const parameter = 5; void parameter; }\n",
    "  return parameter + fallback + seed;\n",
    "}\n",
    "export function defaultsStable(parameter: number = seed, fallback: number = parameter) {\n",
    "  return parameter + fallback;\n",
    "}\n",
    "export function first(name: number): number { return name; }\n",
    "export function second(name: string): string { return name; }\n",
    "export class Holder {\n",
    "  constructor(public readonly name: string) { void name; }\n",
    "  read(): string { return this.name; }\n",
    "}\n",
);

fn typescript_parameter_publication() -> PackagePublication {
    fixture::package_result_with_metadata(
        rift_syntax::ShippedLanguage::TypeScript,
        vec![("index.ts", TYPESCRIPT_PARAMETER_SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"index.ts"}"#,
        )),
    )
    .expect("admitted TypeScript source")
    .publication()
    .clone()
}

fn typescript_parameter_site(prefix: &str, spelling: &str) -> u64 {
    u64::try_from(
        TYPESCRIPT_PARAMETER_SOURCE
            .find(prefix)
            .expect("written fixture site")
            + prefix.rfind(spelling).expect("written name token"),
    )
    .expect("source offset")
}

#[test]
fn package_stores_typescript_ordinary_parameters_and_their_body_reads() {
    let publication = typescript_parameter_publication();
    for (owner, name, source, sites) in [
        (
            "choose",
            "parameter",
            "choose<Value>(parameter",
            &["const local = parameter;", "{ const local = parameter;"][..],
        ),
        (
            "first",
            "name",
            "first(name",
            &["first(name: number): number { return name;"][..],
        ),
        (
            "second",
            "name",
            "second(name",
            &["second(name: string): string { return name;"][..],
        ),
        (
            "defaultsStable",
            "parameter",
            "defaultsStable(parameter",
            &["return parameter + fallback;"][..],
        ),
    ] {
        let path = format!("/beacon/{owner}/{name}");
        let offset = typescript_parameter_site(source, name);
        let parameter = publication
            .objects
            .iter()
            .find(|object| {
                object
                    .id
                    .as_ref()
                    .is_some_and(|id| id.as_str().ends_with(&path))
            })
            .expect("physical parameter");
        assert!(parameter.kinds.contains(&SymbolKind::Parameter));
        let id = parameter.id.as_ref().expect("established identity");
        assert!(publication.declarations.iter().any(|declaration| {
            declaration.symbol == *id
                && declaration.range.start == offset
                && declaration.range.end
                    == offset + u64::try_from(name.len()).expect("physical name")
                && declaration.source == name
        }));
        let uses = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to == *id)
            .map(|edge| {
                assert!(edge.from.as_str().ends_with(&format!("/beacon/{owner}")));
                let range = &edge.occurrence.as_ref().expect("written read").range;
                assert_eq!(
                    range.end - range.start,
                    u64::try_from(name.len()).expect("physical name length")
                );
                range.start
            })
            .collect::<BTreeSet<_>>();
        let expected = sites
            .iter()
            .map(|site| typescript_parameter_site(site, name))
            .collect::<BTreeSet<_>>();
        assert_eq!(uses, expected, "{path}");
    }
}

#[test]
fn package_typescript_parameter_keeps_defaults_blocks_and_constructor_properties() {
    let publication = typescript_parameter_publication();
    let default_seed = typescript_parameter_site("defaultsStable(parameter: number = seed", "seed");
    let nested_local = typescript_parameter_site("void parameter;", "parameter");
    for (offset, target) in [
        (default_seed, "/beacon/seed"),
        (nested_local, "/beacon/defaults/parameter"),
    ] {
        assert!(
            publication.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.to.as_str().ends_with(target)
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| site.range.start == offset)
            }),
            "{target}"
        );
    }
    let collision = publication
        .objects
        .iter()
        .find(|object| {
            object
                .id
                .as_ref()
                .is_some_and(|id| id.as_str().ends_with("/beacon/defaults/parameter"))
        })
        .expect("prior block-local binding");
    assert!(!collision.kinds.contains(&SymbolKind::Parameter));
    let property = publication
        .objects
        .iter()
        .find(|object| {
            object
                .id
                .as_ref()
                .is_some_and(|id| id.as_str().ends_with("/beacon/Holder/name"))
        })
        .expect("prior constructor property");
    assert!(property.kinds.contains(&SymbolKind::Member));
    assert!(!property.kinds.contains(&SymbolKind::Parameter));
    assert!(!publication.declarations.iter().any(|declaration| {
        declaration
            .symbol
            .as_str()
            .ends_with("/Holder/constructor/name")
    }));
}

#[test]
fn package_stores_typescript_imported_type_uses_at_each_written_site() {
    use rift_protocol::read::EdgeProvider;

    let source = "import type { ZodType as Imported } from 'shape';\n\
                  export type Schema = Imported;\n\
                  export function accept(value: Imported): Imported { return value; }\n";
    let analysis = fixture::package_result_with_metadata(
        rift_syntax::ShippedLanguage::TypeScript,
        vec![("index.ts", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"index.ts","dependencies":{"shape":"npm:zod@^4"}}"#,
        )),
    )
    .expect("admitted TypeScript package");
    let (publication, files, semantics, _) = analysis.into_parts();
    let unit = files[0].placement.unit().to_string();
    let target = "rift://symbol/npm/npmjs.org/zod/typescript/zod/ZodType";
    let rows = |edges: &[rift_protocol::index::StoredEdge]| {
        let mut rows = edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::HasType] && edge.to.as_str() == target)
            .map(|edge| {
                let span = edge.occurrence.as_ref().expect("written type use");
                assert_eq!(span.unit.as_str(), unit.as_str());
                let start = usize::try_from(span.range.start).expect("type use start");
                let end = usize::try_from(span.range.end).expect("type use end");
                assert_eq!(&source[start..end], "Imported");
                assert_eq!(edge.provider, EdgeProvider::Static);
                assert_eq!(edge.derivation, EdgeDerivation::Syntax);
                (edge.from.0.clone(), start)
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };
    let positions = source
        .match_indices("Imported")
        .skip(1)
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    let [alias, parameter, returns] = positions.as_slice() else {
        panic!("one alias, parameter and return type use")
    };
    let expected = [
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/Schema".to_owned(),
            *alias,
        ),
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/accept".to_owned(),
            *parameter,
        ),
        (
            "rift://symbol/npm/registry.npmjs.org/beacon/typescript/beacon/accept".to_owned(),
            *returns,
        ),
    ];
    let stored = semantics
        .edges()
        .edges()
        .map(crate::edge::Edge::to_protocol)
        .collect::<Vec<_>>();
    assert_eq!(rows(&stored), expected);
    assert_eq!(rows(&publication.edges), expected);
}

#[test]
fn package_rust_module_source_facet_keeps_written_declaration_and_existing_caps() {
    let root = "pub mod child; pub fn call() { child::run(); }";
    let child = "// é\npub fn run() {}\n";
    let path = ProjectPath::new("src/lib.rs").expect("library root");
    let child_path = ProjectPath::new("src/child.rs").expect("authored module file");
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let package = fixture::identity();
    let owner = package.owner().expect("cargo owner");
    let language = fixture::language(ShippedLanguage::Rust);
    let origin = fixture::origin(&package);
    let sources = [
        PackageSource::new(&path, root),
        PackageSource::new(&child_path, child),
    ];
    let metadata = [PackageSource::new(
        &manifest,
        "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
    )];
    let analyze = |bound| {
        let limits = ExactPackageLimits::new(3, 4096)
            .with_declarations(bound)
            .expect("package declaration cap");
        let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
            .expect("exact source inventory")
            .with_framework_context(&metadata, &[])
            .expect("checked Cargo manifest");
        PackageAnalyzer::analyze(input, 1).expect("package analysis")
    };
    let baseline = analyze(4);
    let admitted = analyze(5);
    let baseline = baseline.publication();
    let admitted = admitted.publication();
    let facet = admitted
        .declarations
        .iter()
        .find(|row| {
            row.symbol.as_str().ends_with("/beacon/child")
                && row.unit.as_str().ends_with("/src/child.rs")
        })
        .expect("whole-file facet of the accepted module");
    assert_eq!(facet.syntax_element_kind.as_deref(), Some("source_file"));
    assert_eq!(facet.range.start, 0);
    assert_eq!(
        facet.range.end,
        u64::try_from(child.len()).expect("file byte length")
    );
    assert_eq!(facet.source, child);
    assert!(facet.source_complete);
    assert!(admitted.declarations.iter().any(|row| {
        row.symbol == facet.symbol
            && row.unit.as_str().ends_with("/src/lib.rs")
            && row.source == "pub mod child;"
    }));
    assert_eq!(baseline.declarations.len() + 1, admitted.declarations.len());
    assert!(
        baseline
            .declarations
            .iter()
            .all(|row| admitted.declarations.contains(row))
    );
    assert_eq!(baseline.units, admitted.units);
    assert_eq!(baseline.edges, admitted.edges);
    assert!(admitted.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls] && edge.to.as_str().ends_with("/beacon/child/run")
    }));
}

#[test]
fn package_rust_shared_literal_modules_keep_distinct_owners_and_one_source() {
    let root = r#"
#[path = "shared.rs"]
pub mod left;
#[path = "shared.rs"]
pub mod right;
pub fn call() { left::one(); right::one(); }
"#;
    let shared = "#[cfg(test)] mod tests; pub trait Borrowed: where for<'a> &'a Self: IntoIterator {} pub fn one() {}";
    let analysis = fixture::package_result(
        ShippedLanguage::Rust,
        vec![("src/lib.rs", root), ("src/shared.rs", shared)],
        None,
    )
    .expect("checked literal modules");
    let (publication, _, _, _) = analysis.into_parts();
    assert_eq!(publication.units.len(), 2);
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0.as_str() == "src/shared.rs")
        .expect("one physical unit")
        .unit;
    let call_site = |needle: &str| {
        u64::try_from(root.find(needle).expect("written call")).expect("call offset")
    };
    let one = u64::try_from(shared.find("one").expect("authored name")).expect("name offset");
    for module in ["left", "right"] {
        let suffix = format!("/beacon/{module}/one");
        assert_eq!(
            publication
                .declarations
                .iter()
                .filter(|entry| {
                    entry.symbol.as_str().ends_with(&suffix)
                        && &entry.unit == unit
                        && entry.range.start <= one
                        && entry.range.end >= one + 3
                        && entry.source == "pub fn one() {}"
                })
                .count(),
            1,
            "{module}: {:?}",
            publication.declarations,
        );
        assert!(
            publication.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.from.as_str().ends_with("/beacon/call")
                    && edge.to.as_str().ends_with(&suffix)
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        let call = format!("{module}::one()");
                        let start = call_site(call.as_str());
                        site.range.start == start
                            && site.range.end
                                == start + u64::try_from(call.len()).expect("call length")
                    })
            }),
            "{module}: {:?}",
            publication.edges
        );
    }
}

#[test]
fn package_rust_shared_literal_modules_resolve_each_outgoing_scope() {
    const ROOT: &str = r#"
#[path = "shared.rs"]
pub mod east;
#[path = "shared.rs"]
pub mod west;
pub fn outer() -> u8 { east::caller() + west::caller() }
"#;
    const SHARED: &str = "pub type Number = u8;
pub fn leaf() -> Number { 7 }
pub fn caller() -> Number { leaf() }
pub fn through_self() -> Number { self::leaf() }
pub fn shadowed() -> Number { let leaf = || 3; leaf() }
#[cfg(any())]
pub fn inactive() -> Number { leaf() }
";
    let publication = fixture::package_result(
        ShippedLanguage::Rust,
        vec![("src/lib.rs", ROOT), ("src/shared.rs", SHARED)],
        None,
    )
    .expect("checked literal modules")
    .into_parts()
    .0;
    assert_eq!(publication.units.len(), 2);
    let unit = &publication
        .units
        .iter()
        .find(|unit| unit.path.0.as_str() == "src/shared.rs")
        .expect("shared physical unit")
        .unit;
    for module in ["east", "west"] {
        for (caller, spelling) in [("caller", "leaf()"), ("through_self", "self::leaf()")] {
            let declaration = format!("fn {caller}() -> Number");
            let first = SHARED.find(&declaration).expect("declared function");
            let start = first + SHARED[first..].find(spelling).expect("authored call");
            let end = start + spelling.len();
            let from = format!("/beacon/{module}/{caller}");
            let to = format!("/beacon/{module}/leaf");
            assert!(publication.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.from.as_str().ends_with(&from)
                    && edge.to.as_str().ends_with(&to)
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        &site.unit == unit
                            && site.range.start == start as u64
                            && site.range.end == end as u64
                    })
            }));
            let ty = format!("/beacon/{module}/Number");
            assert!(publication.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::HasType]
                    && edge.from.as_str().ends_with(&from)
                    && edge.to.as_str().ends_with(&ty)
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| &site.unit == unit)
            }));
        }
        assert!(publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && edge.from.as_str().ends_with("/beacon/outer")
                && edge
                    .to
                    .as_str()
                    .ends_with(&format!("/beacon/{module}/caller"))
        }));
    }
    let wrong = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.from.as_str().ends_with("/inactive")
                || (edge.from.as_str().ends_with("/shadowed")
                    && ["/beacon/east/leaf", "/beacon/west/leaf"]
                        .iter()
                        .any(|suffix| edge.to.as_str().ends_with(suffix)))
        })
        .collect::<Vec<_>>();
    assert!(wrong.is_empty(), "{wrong:?}");
}

#[test]
fn package_rust_constructors_and_fields_use_their_block_owner() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    const ROOT: &str = "pub struct Target { pub item: u8 }";
    const LOCAL: &str = "struct Target { item: u16 }";
    const READ: &str = "let value = Target { item: 7 }; value.item";
    for (body, returns, owner, definition, field) in [
        (
            format!("{{ {LOCAL} }} {READ}"),
            "u8",
            "Target",
            ROOT,
            "pub item: u8",
        ),
        (
            format!("let value = Target {{ item: 7 }}; {{ {LOCAL} }} value.item"),
            "u8",
            "Target",
            ROOT,
            "pub item: u8",
        ),
        (READ.to_owned(), "u8", "Target", ROOT, "pub item: u8"),
        (
            format!("{{ {LOCAL} {READ} }}"),
            "u16",
            "run/Target",
            LOCAL,
            "item: u16",
        ),
        (
            format!("{{ let value = Target {{ item: 7 }}; {LOCAL} value.item }}"),
            "u16",
            "run/Target",
            LOCAL,
            "item: u16",
        ),
    ] {
        let source = format!("{ROOT}\npub fn run() -> {returns} {{ {body} }}");
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", &source)], None)
                .expect("admitted Rust package");
        let (_, _, semantics, _) = analysis.into_parts();
        let written = |binding: &DeclarationBinding| {
            let range = binding.range();
            &source[offset_in(range.start())..offset_in(range.end())]
        };
        for (role, text, target, authored) in [
            (
                ReferenceRole::Call,
                "Target { item: 7 }",
                format!("/beacon/{owner}"),
                definition,
            ),
            (
                ReferenceRole::Read,
                "value.item",
                format!("/beacon/{owner}/item"),
                field,
            ),
        ] {
            let found = semantics
                .static_occurrences()
                .iter()
                .filter(|edge| edge.role == role && written(&edge.binding) == text)
                .collect::<Vec<_>>();
            let [edge] = found.as_slice() else {
                panic!("expected one {role:?} of {text} in {body}, got {found:?}");
            };
            assert!(
                matches!(&edge.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(&target)),
                "{body}: {:?}",
                edge.target
            );
            assert_eq!(
                written(edge.target_binding.as_ref().expect("physical target")),
                authored,
                "{body}"
            );
        }
    }
}

#[test]
fn package_rust_same_name_block_constructors_keep_distinct_physical_targets() {
    use rift_core::ReferenceRole;

    let source = "pub fn run() -> u16 {\n\
        let first = { struct Target { item: u8 } let value = Target { item: 1 }; value.item };\n\
        let second = { struct Target { item: u16 } let value = Target { item: 2 }; value.item };\n\
        u16::from(first) + second\n}";
    let analysis =
        fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", source)], None)
            .expect("admitted Rust package");
    let (_, files, semantics, _) = analysis.into_parts();
    let qualified = files[0]
        .file()
        .syntax()
        .symbols()
        .iter()
        .filter(|symbol| symbol.name == "Target")
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(qualified, ["run::Target", "run::Target"]);

    let written = |binding: &DeclarationBinding| {
        let range = binding.range();
        &source[offset_in(range.start())..offset_in(range.end())]
    };
    for (label, declaration) in [
        ("Target { item: 1 }", "struct Target { item: u8 }"),
        ("Target { item: 2 }", "struct Target { item: u16 }"),
    ] {
        let found = semantics
            .static_occurrences()
            .iter()
            .filter(|edge| edge.role == ReferenceRole::Call && written(&edge.binding) == label)
            .collect::<Vec<_>>();
        let [edge] = found.as_slice() else {
            panic!("expected one Call of {label}, got {found:?}");
        };
        assert_eq!(
            written(edge.target_binding.as_ref().expect("physical target")),
            declaration
        );
    }
}

#[test]
fn package_rust_block_types_respect_import_namespaces_and_inline_modules() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    for (source, target, constructor, field) in [
        (
            r"pub mod src { pub struct Target { pub item: u8 } }

pub fn run() -> u8 {
    struct Target { item: u16 }
    {
        use crate::src::Target;
        let value = Target { item: 7 };
        value.item
    }
}",
            "src/Target",
            "pub struct Target { pub item: u8 }",
            "pub item: u8",
        ),
        (
            r"pub fn run() -> u8 {
    struct Target { item: u16 }
    mod inner {
        pub struct Target { pub item: u8 }
        pub fn read() -> u8 {
            let value = Target { item: 7 };
            value.item
        }
    }
    inner::read()
}",
            "run/inner/Target",
            "pub struct Target { pub item: u8 }",
            "pub item: u8",
        ),
        (
            r"pub mod src { pub fn maker() -> u8 { 1 } }

pub fn run() -> u16 {
    struct Target { item: u16 }
    {
        use crate::src::maker as Target;
        let value = Target { item: 7 };
        let _ = Target();
        value.item
    }
}",
            "run/Target",
            "struct Target { item: u16 }",
            "item: u16",
        ),
    ] {
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", source)], None)
                .expect("admitted Rust package");
        let (_, _, semantics, _) = analysis.into_parts();
        let written = |binding: &DeclarationBinding| {
            let range = binding.range();
            &source[offset_in(range.start())..offset_in(range.end())]
        };
        for (role, occurrence, suffix, definition) in [
            (
                ReferenceRole::Call,
                "Target { item: 7 }",
                format!("/beacon/{target}"),
                constructor,
            ),
            (
                ReferenceRole::Read,
                "value.item",
                format!("/beacon/{target}/item"),
                field,
            ),
        ] {
            let found = semantics
                .static_occurrences()
                .iter()
                .filter(|edge| edge.role == role && written(&edge.binding) == occurrence)
                .collect::<Vec<_>>();
            let [edge] = found.as_slice() else {
                panic!("expected one {role:?} of {occurrence} in {source}, got {found:?}");
            };
            assert!(
                matches!(&edge.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(&suffix)),
                "{source}: {:?}",
                edge.target,
            );
            assert_eq!(
                written(edge.target_binding.as_ref().expect("physical target")),
                definition,
                "{source}",
            );
        }
    }
}

#[test]
fn package_rust_closure_local_constructor_keeps_its_physical_owner() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    let source = r"
pub struct Holder(pub u8);

pub fn run(value: u16) -> (u16, u8) {
    let inner = || {
        struct Holder(u16);
        let item = Holder(value);
        let outer = crate::Holder(3);
        (item.0, outer.0)
    };
    inner()
}

pub fn outside(value: u8) -> u8 {
    { struct Holder(u32); let _ = Holder(4); }
    Holder(value).0
}";
    let analysis =
        fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", source)], None)
            .expect("admitted Rust package");
    let (_, _, semantics, _) = analysis.into_parts();
    let written = |binding: &DeclarationBinding| {
        let range = binding.range();
        &source[offset_in(range.start())..offset_in(range.end())]
    };
    let mut occurrences = source.match_indices("Holder(value)");
    let (closure_call, _) = occurrences.next().expect("closure invocation");
    let (outside_call, _) = occurrences.next().expect("sibling control invocation");
    for (start, occurrence, owner, definition) in [
        (
            closure_call,
            "Holder(value)",
            "/beacon/run/inner/Holder",
            "struct Holder(u16);",
        ),
        (
            source
                .find("crate::Holder(3)")
                .expect("qualified invocation"),
            "crate::Holder(3)",
            "/beacon/Holder",
            "pub struct Holder(pub u8);",
        ),
        (
            outside_call,
            "Holder(value)",
            "/beacon/Holder",
            "pub struct Holder(pub u8);",
        ),
    ] {
        let edges = semantics
            .static_occurrences()
            .iter()
            .filter(|edge| {
                edge.role == ReferenceRole::Call && offset_in(edge.binding.range().start()) == start
            })
            .collect::<Vec<_>>();
        let [edge] = edges.as_slice() else {
            panic!("expected one Call of {occurrence} at {start}, got {edges:?}");
        };
        assert_eq!(written(&edge.binding), occurrence);
        assert!(
            matches!(&edge.target, NormalizedTarget::Symbol(id) if id.as_str().ends_with(owner)),
            "{:?}",
            edge.target
        );
        assert_eq!(
            written(edge.target_binding.as_ref().expect("physical target")),
            definition
        );
    }
}

#[test]
fn package_rust_initializer_local_types_do_not_name_the_module_type() {
    use rift_core::ReferenceRole;
    use rift_provider::NormalizedTarget;

    let module_target =
        |id: &str| id.ends_with("/beacon/Target") || id.ends_with("/beacon/Target/item");
    for (label, source) in [
        (
            "named const",
            "pub struct Target { pub item: u8 }\npub const CHECK: u16 = {\nstruct Target { item: u16 }\nlet value = Target { item: 7 };\nvalue.item\n};\n",
        ),
        (
            "named static",
            "pub struct Target { pub item: u8 }\npub static CHECK: u16 = {\nstruct Target { item: u16 }\nlet value = Target { item: 7 };\nvalue.item\n};\n",
        ),
        (
            "tuple const",
            "pub struct Target(pub u8);\npub const CHECK: u16 = {\nstruct Target(u16);\nlet value = Target(7);\nvalue.0\n};\n",
        ),
    ] {
        let source = format!(
            "{source}\npub mod visible {{ pub struct Target {{ pub item: u8 }} }}\n\
             pub const EXPLICIT: u8 = crate::visible::Target {{ item: 1 }}.item;\n"
        );
        let analysis =
            fixture::package_result(ShippedLanguage::Rust, vec![("src/lib.rs", &source)], None)
                .expect("admitted Rust package");
        let initializer =
            u64::try_from(source.find("CHECK:").expect("initializer")).expect("offset");
        let edges = &analysis.publication().edges;
        assert!(
            edges.iter().all(|edge| {
                !module_target(edge.to.as_str())
                    || !edge
                        .kinds
                        .iter()
                        .any(|kind| matches!(kind, EdgeKind::Calls | EdgeKind::Reads))
                    || edge
                        .occurrence
                        .as_ref()
                        .is_none_or(|site| site.range.start < initializer)
            }),
            "{label}: {edges:?}",
        );
        assert!(
            edges.iter().any(|edge| {
                edge.kinds.contains(&EdgeKind::Calls)
                    && edge.to.as_str().ends_with("/beacon/visible/Target")
            }),
            "missing explicit constructor in {label}: {edges:?}"
        );
        let (_, _, semantics, _) = analysis.into_parts();
        let leaked = semantics.static_occurrences().iter().find(|edge| {
            edge.binding.range().start() >= initializer
                && matches!(edge.role, ReferenceRole::Call | ReferenceRole::Read)
                && matches!(&edge.target, NormalizedTarget::Symbol(id) if module_target(id.as_str()))
        });
        assert!(leaked.is_none(), "{label}: {leaked:?}");
    }
}

#[test]
fn package_calls_keep_their_javascript_initializer_owners() {
    let source = "function load() { return 1; }\n\
                  export const value = load();\n\
                  export const items = [load()];\n\
                  export const object = { property: load() };\n\
                  export class Owner { field = load(); static shared = load(); }\n\
                  export function run() { const local = load(); return local; }\n";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls] && edge.to.as_str().ends_with("/load"))
        .map(|edge| {
            let span = edge.occurrence.as_ref().expect("written call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            assert_eq!(source.get(start..end), Some("load()"));
            (start, edge.from.as_str())
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let owners = [
        "value",
        "items",
        "object/property",
        "Owner/field",
        "Owner/shared",
        "run",
    ];
    assert_eq!(calls.len(), owners.len(), "{calls:?}");
    for ((start, owner), (expected, (position, _))) in calls.into_iter().zip(
        owners
            .into_iter()
            .zip(source.match_indices("load()").skip(1)),
    ) {
        assert_eq!(start, position);
        assert!(owner.ends_with(&format!("/beacon/{expected}")), "{owner}");
    }
}

#[test]
fn package_ecmascript_direct_class_values_name_stable_calls() {
    let source = r"
export var Named = class Inner { static make() { return 1; } };
export const Anonymous = class { static make() { return 2; } };
export class Declared { static make() { return 3; } }
export var Reassigned = class { static make() { return 4; } };
Reassigned = class { static make() { return 5; } };
var Competing = class { static make() { return 6; } };
var Competing = class { static make() { return 7; } };
export { Competing };
export function stable() { Named.make(); Anonymous.make(); new Anonymous(); Declared.make(); }
export function unsettled() { Reassigned.make(); new Reassigned(); Competing.make(); new Competing(); }
export function plain() { Anonymous(); }
export const Patched = class { static make() { return 8; } };
export function replace(replacement) { Patched.make = replacement; }
export function changed() { Patched.make(); }
";
    for (language, filename, entry) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            r#""main":"index.js""#,
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            r#""types":"index.ts""#,
        ),
    ] {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(filename, source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        let mut calls = analysis
            .publication()
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Calls])
            .map(|edge| {
                let span = edge.occurrence.as_ref().expect("written call");
                let start = usize::try_from(span.range.start).expect("start");
                let end = usize::try_from(span.range.end).expect("end");
                let owner = edge.from.as_str().rsplit("/beacon/").next().expect("owner");
                let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
                (start, &source[start..end], owner, target)
            })
            .collect::<Vec<_>>();
        calls.sort_unstable();
        assert_eq!(
            calls
                .into_iter()
                .map(|(_, label, owner, target)| (label, owner, target))
                .collect::<Vec<_>>(),
            [
                ("Named.make()", "stable", "Named/make"),
                ("Anonymous.make()", "stable", "Anonymous/make"),
                ("new Anonymous()", "stable", "Anonymous"),
                ("Declared.make()", "stable", "Declared/make"),
            ],
            "{filename}",
        );
    }
}

#[test]
fn package_ecmascript_class_heritage_accepts_a_stable_holder() {
    const COMMON: &str = r"
export const Held = class { static make() { return 1; } };
export class Written { static make() { return 2; } }
export class Derived extends Held {}
export class WrittenChild extends Written {}
export const inherited = () => Derived.make();
export var Replaced = class { static make() { return 3; } };
Replaced = class { static make() { return 4; } };
export class Dynamic extends Replaced {}
export function shadow(Held) { class Local extends Held {} return Local; }
";
    const TYPES: &str = r"
export interface Surface { item: number; }
export interface Refined extends Surface {}
export class Implementation implements Surface { item = 1; }
export type Name = Surface;
";
    for (language, filename, entry, typescript) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            r#""main":"index.js""#,
            false,
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            r#""types":"index.ts""#,
            true,
        ),
    ] {
        let source = if typescript {
            format!("{COMMON}{TYPES}")
        } else {
            COMMON.to_owned()
        };
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(filename, &source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        let mut expected = vec![
            ("Derived", EdgeKind::Extends, "Held", "Held"),
            ("WrittenChild", EdgeKind::Extends, "Written", "Written"),
            ("inherited", EdgeKind::Calls, "Held/make", "Derived.make()"),
        ];
        if typescript {
            expected.extend([
                ("Refined", EdgeKind::Extends, "Surface", "Surface"),
                ("Refined", EdgeKind::HasType, "Surface", "Surface"),
                ("Implementation", EdgeKind::Implements, "Surface", "Surface"),
                ("Implementation", EdgeKind::HasType, "Surface", "Surface"),
                ("Name", EdgeKind::HasType, "Surface", "Surface"),
            ]);
        }
        let mut resolved = analysis
            .publication()
            .edges
            .iter()
            .filter_map(|edge| {
                let [kind] = edge.kinds.as_slice() else {
                    return None;
                };
                if !matches!(
                    kind,
                    EdgeKind::Extends | EdgeKind::Implements | EdgeKind::HasType | EdgeKind::Calls
                ) {
                    return None;
                }
                let span = edge.occurrence.as_ref().expect("written relation");
                let start = usize::try_from(span.range.start).expect("start");
                let end = usize::try_from(span.range.end).expect("end");
                let from = edge
                    .from
                    .as_str()
                    .rsplit("/beacon/")
                    .next()
                    .expect("from symbol");
                let to = edge
                    .to
                    .as_str()
                    .rsplit("/beacon/")
                    .next()
                    .expect("to symbol");
                Some((from, *kind, to, &source[start..end]))
            })
            .collect::<Vec<_>>();
        resolved.sort_unstable();
        expected.sort_unstable();
        assert_eq!(resolved, expected, "{filename}");
    }
}

#[test]
fn package_ecmascript_stable_class_publication_keeps_declared_visibility() {
    const JS: &str = r"
export const Stable = class {
  static make() { return 1; }
  method() { return 2; }
  static #secret() { return 3; }
  #internal() { return 4; }
};
export var Mutable = class { static retired() { return 5; } };
Mutable = class { static replacement() { return 6; } };
export class Written { static make() { return 7; } static #secret() { return 8; } }
const Local = class { static make() { return 9; } };
export const localResult = Local.make();
";
    const TS: &str = r"
export const Stable = class {
  static make() { return 1; }
  method() { return 2; }
  private static secret() { return 3; }
  protected internal() { return 4; }
};
export class Written { static make() { return 7; } private static secret() { return 8; } }
";
    for (language, file, source, entry, expected) in [
        (
            ShippedLanguage::JavaScript,
            "index.js",
            JS,
            r#""main":"index.js""#,
            &[
                ("Stable/make", true),
                ("Stable/method", true),
                ("Stable/%23secret", false),
                ("Stable/%23internal", false),
                ("Mutable/retired", false),
                ("Local/make", false),
                ("Written/make", true),
                ("Written/%23secret", false),
            ][..],
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            TS,
            r#""types":"index.ts""#,
            &[
                ("Stable/make", true),
                ("Stable/method", true),
                ("Stable/secret", false),
                ("Stable/internal", false),
                ("Written/make", true),
                ("Written/secret", false),
            ][..],
        ),
    ] {
        let manifest = format!(r#"{{"name":"beacon","version":"1.0.0","type":"module",{entry}}}"#);
        let analysis = fixture::package_result_with_metadata(
            language,
            vec![(file, source)],
            None,
            Some(("package.json", &manifest)),
        )
        .expect("admitted ECMAScript package");
        for &(name, public) in expected {
            let address = format!("/beacon/{name}");
            let declarations = analysis
                .publication()
                .declarations
                .iter()
                .filter(|declaration| declaration.symbol.as_str().ends_with(&address))
                .map(|declaration| declaration.public)
                .collect::<Vec<_>>();
            assert_eq!(declarations, [public], "{file}: {name}");
        }
    }
}

#[test]
fn package_ecmascript_class_initializer_this_preserves_replacement_refusals() {
    let source = r"
export const Held = class {
  static make() { return 1; }
  static swap(next) { this.make = next; }
};
export class Written {
  static make() { return 2; }
  static swap(next) { this.make = next; }
}
export const Dynamic = class {
  static make() { return 3; }
  static swap(next) { function inner() { this.make = next; } return inner; }
};
export const Lexical = class {
  static make() { return 4; }
  static swap(next) { const bind = () => { this.make = next; }; bind(); }
};
export function run(next) { Held.swap(next); return Held.make(); }
export function control(next) { Written.swap(next); return Written.make(); }
export function nested(next) { Dynamic.swap(next); return Dynamic.make(); }
export function captured(next) { Lexical.swap(next); return Lexical.make(); }
";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .filter_map(|edge| {
            let span = edge.occurrence.as_ref().expect("physical call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
            let text = &source[start..end];
            text.contains('.').then_some((start, text, target))
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let roles = calls
        .into_iter()
        .map(|(_, source, target)| (source, target))
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            ("Held.swap(next)", "Held/swap"),
            ("Written.swap(next)", "Written/swap"),
            ("Dynamic.swap(next)", "Dynamic/swap"),
            ("Dynamic.make()", "Dynamic/make"),
            ("Lexical.swap(next)", "Lexical/swap"),
        ]
    );
}

#[test]
fn package_javascript_class_alias_writes_withhold_the_replaced_method() {
    let source = r"
export class Aliased { static make() { return 1; } }
const Alias = Aliased;
export class Direct { static make() { return 2; } }
export class Scoped { static make() { return 3; } }
export class Other { static make() { return 4; } }
export class Unchanged { static make() { return 5; } }
const Exposed = Scoped;
let Former = Unchanged;
Former = Other;
export function throughAlias(next) { Alias.make = next; return Aliased.make(); }
export function throughDirect(next) { Direct.make = next; return Direct.make(); }
export function nested(next) { const Exposed = Other; Exposed.make = next; return Scoped.make(); }
export function reassigned(next) { Former.make = next; return Unchanged.make(); }
";
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", source)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.js"}"#,
        )),
    )
    .expect("admitted JavaScript package");
    let mut calls = analysis
        .publication()
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls])
        .map(|edge| {
            let span = edge.occurrence.as_ref().expect("written call");
            let start = usize::try_from(span.range.start).expect("start");
            let end = usize::try_from(span.range.end).expect("end");
            let owner = edge.from.as_str().rsplit("/beacon/").next().expect("owner");
            let target = edge.to.as_str().rsplit("/beacon/").next().expect("target");
            (start, &source[start..end], owner, target)
        })
        .collect::<Vec<_>>();
    calls.sort_unstable();
    let calls = calls
        .into_iter()
        .map(|(_, text, owner, target)| (text, owner, target))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            ("Scoped.make()", "nested", "Scoped/make"),
            ("Unchanged.make()", "reassigned", "Unchanged/make"),
        ]
    );
}

struct GraphFixture {
    analysis: PackageAnalysis,
    owner: SymbolOwner,
    origin: ContributionOrigin,
    language: Language,
    anchor_language: Option<Language>,
}

impl GraphFixture {
    fn new() -> Self {
        let package = fixture::identity();
        Self {
            analysis: fixture::package_result(
                rift_syntax::ShippedLanguage::Rust,
                vec![("src/lib.rs", SOURCE)],
                None,
            )
            .expect("captured physical source"),
            owner: package.owner().expect("canonical package owner"),
            origin: fixture::origin(&package),
            language: Language::from_identity_segment("rust").expect("language"),
            anchor_language: None,
        }
    }

    fn identity(&self, name: &str) -> rift_core::SymbolId {
        let identity = rift_protocol::identity::SymbolIdentity::new(
            self.owner.clone(),
            self.anchor_language
                .as_ref()
                .unwrap_or(&self.language)
                .clone(),
            vec!["beacon".to_owned(), name.to_owned()],
        )
        .expect("current logical identity");
        rift_core::SymbolId::new(identity.wire_identity()).expect("symbol address")
    }

    fn reference(&self, name: &str) -> ContributionReference {
        ContributionReference::new(
            Self::provider(),
            ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
        )
    }

    fn provider() -> ProviderId {
        ProviderId::new("publication.fixture").expect("provider")
    }

    fn contribution(&self, name: &str, edges: Vec<ContributionEdge>) -> Contribution {
        Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            PortableSymbolFacts::new(
                self.language.clone(),
                name,
                format!("beacon::{name}"),
                ExactKind::try_from("function".to_owned()).expect("kind"),
            ),
            self.origin.clone(),
        )
        .identity_anchor(self.identity(name))
        .edges(edges)
        .build()
        .expect("source-less logical contribution")
    }

    fn graph(&self, edges: Vec<ContributionEdge>) -> WorkspaceSemantics {
        Self::graph_with_contributions(vec![
            self.contribution("start", edges),
            self.contribution("finish", Vec::new()),
        ])
    }

    fn graph_with_contributions(contributions: Vec<Contribution>) -> WorkspaceSemantics {
        let limits = PublicationLimits::new(1, 2, 2).expect("graph bounds");
        let publication = ProviderPublication::new(
            Self::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            contributions,
            limits,
        )
        .expect("captured contributions");
        let publications = std::sync::Arc::new(
            PublicationSet::empty(limits)
                .replaced(publication)
                .expect("bounded publication set"),
        );
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            &publications,
            None,
        )
        .expect("normalized graph");
        WorkspaceSemantics::from_graph(graph, Self::provider(), 16)
    }

    fn named_contribution(
        &self,
        name: &str,
        edges: Vec<ContributionEdge>,
        established: bool,
    ) -> Result<Contribution, RiftError> {
        use rift_protocol::read::{Documentation, DocumentationFormat, Extensions, Signature};

        let signatures = ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
            .into_iter()
            .map(|display| Signature {
                display: display.to_owned(),
                links: Vec::new(),
                language: self.language.clone(),
                receiver: None,
                parameters: Vec::new(),
                returns: Vec::new(),
                type_parameters: Vec::new(),
                throws: Vec::new(),
                effects: Vec::new(),
                extensions: Extensions::default(),
            })
            .collect();
        let facts = PortableSymbolFacts::new(
            self.language.clone(),
            name,
            "beacon::wide",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        )
        .signatures(signatures)
        .documentation(vec![Documentation {
            format: DocumentationFormat::Markdown,
            text: "Both callable forms remain in the captured object.".to_owned(),
        }]);
        let mut builder = Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity("wide").as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            facts,
            self.origin.clone(),
        )
        .edges(edges);
        if established {
            builder = builder.identity_anchor(self.identity("wide"));
        }
        builder.build()
    }

    fn occurrence(&self, start: usize, end: usize) -> DeclarationBinding {
        DeclarationBinding::new(
            self.analysis.files()[0].placement.unit().clone(),
            SourceRange::new(
                u64::try_from(start).expect("source start"),
                u64::try_from(end).expect("source end"),
            )
            .expect("nonempty source range"),
        )
    }

    fn edge(&self, occurrence: DeclarationBinding) -> ContributionEdge {
        ContributionEdge::new(rift_core::EdgeKind::Reference, self.reference("finish"))
            .with_derivation(EdgeDerivation::Syntax)
            .with_occurrence(occurrence)
    }

    fn publication(
        &self,
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<PackagePublication, RiftError> {
        let sources = self
            .analysis
            .files()
            .iter()
            .map(|held| {
                crate::PackageSource::new(held.file.path(), held.file.source())
                    .with_source_unit(held.placement.unit(), held.placement.origin())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let input =
            ExactPackageInput::new(&self.owner, &self.language, &self.origin, &sources, limits)?;
        publish(&input, self.analysis.files(), semantics, false, None)
            .map(|(publication, _)| publication)
    }
}

#[test]
fn artifact_name_bytes_and_public_name_characters_keep_distinct_bounds() {
    let fixture = GraphFixture::new();
    for (name, public) in [
        ("n".repeat(4096), true),
        ("n".repeat(4097), false),
        ("é".repeat(4096), true),
        ("n".repeat(8192), false),
    ] {
        assert!(rift_core::is_portable_name(&name));
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, Vec::new(), true)
                .expect("provider name within byte bound"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("full artifact name retained");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("original normalized object");
        assert_eq!(object.name_is_public(), public);
        assert!(object.id.is_some());
        assert_eq!(object.signatures.len(), 2);
        assert_eq!(
            publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, field, bound, .. }
                if field == "name" && *bound == 4096)),
            !public
        );
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| coverage.identity_complete == public)
        );
        let schema: serde_json::Value =
            serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
                .expect("artifact schema");
        let validator = jsonschema::validator_for(&schema).expect("artifact schema compiles");
        assert!(validator.is_valid(&serde_json::to_value(&publication).expect("artifact")));
    }
    for name in ["n".repeat(8193), "é".repeat(4097)] {
        assert!(!rift_core::is_portable_name(&name));
        assert!(fixture.named_contribution(&name, Vec::new(), true).is_err());
    }
}

#[test]
fn source_less_wide_objects_roundtrip_all_facts_and_edge_occurrences() {
    let fixture = GraphFixture::new();
    let name = "n".repeat(4097);
    for established in [true, false] {
        let edges = if established {
            SOURCE
                .match_indices("finish();")
                .map(|(start, _)| fixture.edge(fixture.occurrence(start, start + "finish()".len())))
                .collect()
        } else {
            Vec::new()
        };
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, edges, established)
                .expect("full source-less facts"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("source-less artifact projection");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("full source-less object");
        assert_eq!(object.id.is_some(), established);
        assert!(!object.name_is_public());
        assert_eq!(
            object
                .signatures
                .iter()
                .map(|signature| signature.display.as_str())
                .collect::<Vec<_>>(),
            ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
        );
        assert_eq!(
            object.documentation[0].text,
            "Both callable forms remain in the captured object."
        );
        assert!(publication.declarations.is_empty());
        assert!(
            !publication
                .documents
                .iter()
                .any(|document| document.name == name)
        );
        assert_eq!(publication.units[0].source, SOURCE);
        assert!(publication.units[0].source_complete);
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| !coverage.identity_complete)
        );
        assert!(publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, qualified_name, field, bound }
                if qualified_name == "beacon::wide" && field == "name" && *bound == 4096)));
        if !established {
            assert!(publication.warnings.iter().any(|warning| matches!(warning,
                PackageAnalysisWarning::IdentityUnresolved { unit: None, range: None, qualified_name }
                    if qualified_name == "beacon::wide")));
        }
        let wire = serde_json::to_value(&publication).expect("full artifact serialization");
        let restored: PackagePublication =
            serde_json::from_value(wire).expect("full artifact restore");
        assert_eq!(restored, publication);
        assert_eq!(
            semantics.graph().edges().len(),
            if established { 2 } else { 0 }
        );
        if established {
            assert_eq!(restored.edges.len(), 2);
            let starts = restored
                .edges
                .iter()
                .map(|edge| {
                    let occurrence = edge.occurrence.as_ref().expect("actual source occurrence");
                    assert_eq!(occurrence.unit, restored.units[0].unit);
                    let start = usize::try_from(occurrence.range.start).expect("source start");
                    let end = usize::try_from(occurrence.range.end).expect("source end");
                    assert_eq!(&SOURCE[start..end], "finish()");
                    start
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(starts.len(), 2);
        }
    }
}

#[test]
fn public_name_omission_retains_original_source_binding_and_artifact_object() {
    let name = "n".repeat(4097);
    let source = format!("pub fn {name}() {{}}\n");
    let analysis = fixture::package_result(
        rift_syntax::ShippedLanguage::Rust,
        vec![("src/lib.rs", &source)],
        None,
    )
    .expect("captured Cargo root and source");
    let publication = analysis.publication();
    let object = publication
        .objects
        .iter()
        .find(|object| object.name == name)
        .expect("full artifact object");
    let id = object.id.as_ref().expect("established canonical identity");
    assert!(!object.name_is_public());
    let bindings = publication
        .declarations
        .iter()
        .filter(|binding| &binding.symbol == id)
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 1);
    let binding = bindings[0];
    assert!(binding.public);
    assert!(binding.source_complete);
    let start = usize::try_from(binding.range.start).expect("original start");
    let end = usize::try_from(binding.range.end).expect("original end");
    assert_eq!(binding.source, source[start..end]);
    assert_eq!(publication.units[0].source, source);
    assert!(
        !publication
            .documents
            .iter()
            .any(|document| document.kind == PackageDocumentKind::Symbol)
    );
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::ObjectUnavailable { unit: Some(unit), range: Some(range), qualified_name, field, bound }
            if unit == &binding.unit && range == &binding.range && qualified_name == &name
                && field == "name" && *bound == 4096)));
}

#[test]
fn source_less_objects_keep_logical_runtime_origin() {
    let mut fixture = GraphFixture::new();
    let runtime = rift_protocol::read::RuntimeIdentity {
        runtime: "rust".to_owned(),
        version: "1.98.1".to_owned(),
    };
    fixture.owner = runtime.owner().expect("logical runtime owner");
    fixture.origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime.clone()),
        }),
        rift_core::SourceKind::Authored,
    )
    .expect("runtime origin");
    let semantics = fixture.graph(Vec::new());
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("source-less object projection");
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.declarations.is_empty());
    for object in &publication.objects {
        let identity = rift_protocol::identity::SymbolIdentity::parse(
            object.id.as_ref().expect("established identity").as_str(),
        )
        .expect("canonical runtime identity");
        assert_eq!(identity.owner(), &fixture.owner);
        assert_eq!(object.origin.runtime.as_ref(), Some(&runtime));
        assert!(object.origin.package.is_none());
    }
    assert!(
        publication
            .documents
            .iter()
            .all(|document| { document.kind != rift_protocol::index::PackageDocumentKind::Symbol })
    );
    assert_eq!(semantics.graph().records().len(), 2);
}

#[test]
fn captured_records_assemble_source_less_facts_without_assigning_unknown_identity() {
    let fixture = GraphFixture::new();
    for established in [true, false] {
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution("wide", Vec::new(), established)
                .expect("source-less contribution"),
        ]);
        let record = &semantics.graph().records()[0];
        for key in record.contributions() {
            assert!(
                semantics
                    .graph()
                    .contribution(key)
                    .expect("captured contribution")
                    .source()
                    .is_none()
            );
        }
        let object = semantics
            .assembled_record(record)
            .expect("source-less object assembly");
        assert_eq!(object.identity().is_some(), established);
        if established {
            assert_eq!(object.identity(), Some(&fixture.identity("wide")));
        }
        let facts = object.facts().expect("retained facts");
        assert_eq!(facts.name(), "wide");
        assert_eq!(facts.signatures_slice().len(), 2);
        assert_eq!(facts.documentation_blocks().len(), 1);
    }
}

#[test]
fn foreign_logical_owner_cannot_receive_publication_origin() {
    let fixture = GraphFixture::new();
    let mut foreign = GraphFixture::new();
    foreign.owner = SymbolOwner::Local;
    let local = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&local, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign owner retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(local.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    let SymbolOwner::Package { registry, .. } = &mut foreign.owner else {
        panic!("fixture requires a package owner");
    };
    *registry = "private.example".to_owned();
    foreign
        .owner
        .validate()
        .expect("canonical foreign registry");
    let private = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&private, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign registry retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(private.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    foreign.anchor_language = Some(Language::from_identity_segment("python").expect("language"));
    let invalid = Contribution::builder(
        ContributionKey::new(
            GraphFixture::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            ProviderSymbolId::for_symbol(foreign.identity("start").as_str()).expect("provider key"),
        ),
        SourceApplicability::Independent,
        PortableSymbolFacts::new(
            foreign.language.clone(),
            "start",
            "beacon::start",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        ),
        foreign.origin.clone(),
    )
    .identity_anchor(foreign.identity("start"))
    .build();
    assert!(invalid.is_err());
}

fn assert_foreign_facts_retained(publication: &PackagePublication) {
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.objects.iter().all(|object| object.id.is_none()));
    assert_eq!(
        publication
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["start", "finish"])
    );
    assert!(publication.declarations.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(
        publication
            .warnings
            .iter()
            .filter(|warning| matches!(
                warning,
                PackageAnalysisWarning::IdentityUnresolved {
                    unit: None,
                    range: None,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        publication
            .warnings
            .iter()
            .all(PackageAnalysisWarning::is_valid)
    );
}

#[test]
fn portable_edges_keep_two_original_call_sites_without_syntax_elements() {
    let fixture = GraphFixture::new();
    let first = SOURCE.find("finish();").expect("first call");
    let second = SOURCE.rfind("finish();").expect("second call");
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(first, first + "finish()".len())),
        fixture.edge(fixture.occurrence(second, second + "finish()".len())),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_retained_source_bytes(8, 1024)
                .expect("bounded retained excerpts"),
        )
        .expect("portable occurrence projection");
    assert_eq!(publication.edges.len(), 2);
    assert!(!publication.units[0].source_complete);
    let starts = publication
        .edges
        .iter()
        .map(|edge| {
            assert_eq!(edge.from.0, fixture.identity("start").as_str());
            assert_eq!(edge.to.0, fixture.identity("finish").as_str());
            assert_eq!(edge.derivation, EdgeDerivation::Syntax);
            assert_eq!(edge.kinds, [EdgeKind::References]);
            assert!(edge.confidence.is_none());
            let occurrence = edge.occurrence.as_ref().expect("original call site");
            assert_eq!(occurrence.unit, publication.units[0].unit);
            let start = usize::try_from(occurrence.range.start).expect("source start");
            let end = usize::try_from(occurrence.range.end).expect("source end");
            assert_eq!(&SOURCE[start..end], "finish()");
            start
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(starts, BTreeSet::from([first, second]));
    assert_eq!(semantics.graph().edges().len(), 2);
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.edges.is_empty())
    );
}

#[test]
fn absent_derivation_keeps_graph_fact_without_portable_edge() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![ContributionEdge::new(
        rift_core::EdgeKind::Reference,
        fixture.reference("finish"),
    )]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unresolved edge projection");
    assert_eq!(semantics.graph().edges().len(), 1);
    assert!(publication.edges.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.edges.is_empty())
    );
}

#[test]
fn invalid_occurrence_refuses_original_byte_range_and_unknown_unit() {
    let fixture = GraphFixture::new();
    for (start, end) in [(0, SOURCE.len() + 1), (4, 5)] {
        let semantics = fixture.graph(vec![fixture.edge(fixture.occurrence(start, end))]);
        assert!(
            fixture
                .publication(&semantics, ExactPackageLimits::new(1, 1024))
                .is_err()
        );
        assert_eq!(semantics.graph().edges().len(), 1);
    }
    let unit = rift_core::SourceUnitId::for_owner(fixture.owner.clone(), "missing.rs")
        .expect("canonical unit outside selected inventory");
    let occurrence =
        DeclarationBinding::new(unit, SourceRange::new(0, 1).expect("nonempty source range"));
    let semantics = fixture.graph(vec![fixture.edge(occurrence)]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unavailable occurrence projection");
    assert!(publication.edges.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(publication.units[0].source, SOURCE);
    assert_eq!(semantics.graph().edges().len(), 1);
}

#[test]
fn edge_bound_keeps_captured_graph_immutable() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(0, 1)),
        fixture.edge(fixture.occurrence(1, 2)),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_edges(1)
                .expect("portable edge bound"),
        )
        .expect("bounded portable edges");
    assert_eq!(publication.edges.len(), 1);
    assert_eq!(semantics.graph().edges().len(), 2);
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::PublicationTruncated { collection, bound }
            if collection == "edges" && *bound == 1)));
}

#[test]
fn four_byte_names_keep_full_artifact_facts_and_neighbor_publication() {
    for characters in [3000, 4097] {
        let name = "𐐀".repeat(characters);
        let source = format!(
            "/// Original callable documentation.\npub fn {name}(value: u8) -> u8 {{ value }}\npub fn finish() {{}}\n"
        );
        let analysis = fixture::package_result(
            rift_syntax::ShippedLanguage::Rust,
            vec![("src/lib.rs", &source)],
            None,
        )
        .expect("captured full source and package metadata");
        let publication = analysis.publication();
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("full omitted object");
        assert!(object.id.is_none());
        assert_eq!(object.name_is_public(), characters <= 4096);
        assert_eq!(object.signatures.len(), 1);
        assert!(object.signatures[0].display.contains(&name));
        assert_eq!(
            object.documentation[0].text,
            "Original callable documentation."
        );
        assert!(
            publication
                .objects
                .iter()
                .any(|object| object.name == "finish" && object.id.is_some())
        );
        assert!(
            publication
                .documents
                .iter()
                .any(|document| document.kind == PackageDocumentKind::Symbol
                    && document.name == "finish")
        );
        assert!(
            !publication
                .documents
                .iter()
                .any(|document| document.name == name)
        );
        assert_eq!(publication.units[0].source, source);
        assert!(publication.units[0].source_complete);
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| !coverage.identity_complete)
        );
        assert!(publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::IdentityUnresolved { unit: Some(unit), range: Some(range), qualified_name }
            if unit == &publication.units[0].unit && qualified_name == &name
                && source[usize::try_from(range.start).expect("item start")..usize::try_from(range.end).expect("item end")].contains(&name))));
        assert_eq!(
            publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { field, bound, qualified_name, .. }
            if field == "name" && *bound == 4096 && qualified_name == &name)),
            characters > 4096
        );
        let wire = serde_json::to_value(publication).expect("full artifact encoding");
        let schema: serde_json::Value =
            serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
                .expect("artifact schema");
        assert!(
            jsonschema::validator_for(&schema)
                .expect("schema validator")
                .is_valid(&wire)
        );
        let restored: PackagePublication =
            serde_json::from_value(wire).expect("full artifact restore");
        assert_eq!(&restored, publication);
    }
}
#[test]
fn package_python_local_reads_keep_earlier_supplemental_reads_at_the_cap() {
    let package = rift_protocol::read::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "click".to_owned(),
        version: "8.3.3".to_owned(),
    };
    let owner = package.owner().expect("package owner");
    let origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Dependency { package }),
        rift_core::SourceKind::Authored,
    )
    .expect("authored source");
    let language = Language {
        name: "python".to_owned(),
        dialect: None,
    };
    let path = ProjectPath::new("click.py").expect("source path");
    let text = concat!(
        "sentinel = 1\n",
        "def a_local():\n",
        "    local_value = 2\n",
        "    return local_value\n",
        "def z_prior():\n",
        "    return sentinel\n",
    );
    let source = [PackageSource::new(&path, text)];
    let roots = [PackageImportRoot::new(
        None,
        vec!["click".to_owned()],
        PackageImportRootOrigin::PyModules,
    )
    .expect("declared module root")];
    for (cap, expected) in [
        (5, &[("/z_prior", "/sentinel", 97_u64, 105_u64)][..]),
        (
            6,
            &[
                ("/a_local", "/a_local/local_value", 59, 70),
                ("/z_prior", "/sentinel", 97, 105),
            ][..],
        ),
    ] {
        let limits = ExactPackageLimits::new(1, 4096)
            .with_edges(cap)
            .expect("bounded edges");
        let input = ExactPackageInput::new(&owner, &language, &origin, &source, limits)
            .expect("valid source")
            .with_import_roots(&roots)
            .expect("stable module scope");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("analyzed package");
        let edges = &analysis.publication().edges;
        assert_eq!(edges.len(), cap);
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.kinds == [EdgeKind::Contains])
                .count(),
            4
        );
        for (from, to, start, end) in expected {
            assert!(
                edges.iter().any(|edge| {
                    edge.kinds == [EdgeKind::Reads]
                        && edge.from.as_str().ends_with(from)
                        && edge.to.as_str().ends_with(to)
                        && edge.occurrence.as_ref().is_some_and(|site| {
                            site.range.start == *start && site.range.end == *end
                        })
                }),
                "{from} {to} capacity {cap}"
            );
        }
    }
}

#[test]
fn package_python_value_parameters_preserve_required_provider_slots() {
    parameter_provider_bound(rift_syntax::ShippedLanguage::Python, 2);
}

fn parameter_provider_bound(shipped: rift_syntax::ShippedLanguage, prior: u32) {
    let mut package = fixture::identity();
    let (file, text, manifest, roots) = if shipped == rift_syntax::ShippedLanguage::TypeScript {
        package.manager = "npm".to_owned();
        package.registry = "registry.npmjs.org".to_owned();
        (
            "index.ts",
            "export function choose(value: number): number { return value; }",
            Some(r#"{"name":"beacon","version":"1.0.0","types":"index.ts"}"#),
            Vec::new(),
        )
    } else {
        package.manager = "pypi".to_owned();
        package.registry = "pypi.org".to_owned();
        package.name = "click".to_owned();
        package.version = "8.3.3".to_owned();
        let root = PackageImportRoot::new(
            None,
            vec!["click".to_owned()],
            PackageImportRootOrigin::PyModules,
        )
        .expect("physical module root");
        (
            "click.py",
            "def choose(value: int) -> int:\n    return value\n",
            None,
            vec![root],
        )
    };
    let language = fixture::language(shipped);
    let owner = package.owner().expect("package identity");
    let origin = fixture::origin(&package);
    let path = ProjectPath::new(file).expect("source path");
    let metadata = ProjectPath::new("package.json").expect("metadata path");
    let sources = [PackageSource::new(&path, text)];
    let manifest = manifest
        .map(|text| PackageSource::new(&metadata, text))
        .into_iter()
        .collect::<Vec<_>>();
    for (bound, optional) in [(prior, false), (prior + 1, true)] {
        let limits = ExactPackageLimits::new(2, 4096)
            .with_declarations(bound)
            .expect("provider declaration bound");
        let exact = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
            .expect("valid source")
            .with_framework_context(&manifest, &[])
            .expect("authored metadata")
            .with_import_roots(&roots)
            .expect("authored route");
        let analysis = PackageAnalyzer::analyze(exact, 1).expect("primary declaration accepted");
        let publication = analysis.publication();
        assert_eq!(
            publication.declarations.len(),
            usize::try_from(bound).expect("small bound")
        );
        assert!(
            publication
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().ends_with("/choose"))
        );
        if shipped == rift_syntax::ShippedLanguage::Python {
            assert!(
                publication
                    .declarations
                    .iter()
                    .any(|row| row.symbol.as_str().ends_with("/click"))
            );
        }
        assert_eq!(
            publication.declarations.iter().any(|row| row
                .symbol
                .as_str()
                .ends_with("/choose/value")
                && row.source == "value"),
            optional
        );
        assert_eq!(
            publication
                .edges
                .iter()
                .any(|edge| edge.kinds == [EdgeKind::Reads]
                    && edge.to.as_str().ends_with("/choose/value")),
            optional
        );
    }
}

#[test]
fn package_typescript_value_parameters_preserve_required_provider_slots() {
    parameter_provider_bound(rift_syntax::ShippedLanguage::TypeScript, 1);
}

#[test]
fn private_ecmascript_lexical_groups_keep_exact_declarations_and_callers() {
    const SOURCE: &str = r"function leaf() {}
function exposed() {}
exports.exposed = exposed;
unknown('f', function () {
    function helper() { leaf(); }
    helper();
});
unknown('v', function () {
    var helper = 'first';
    var helper = 'same';
    consume(helper);
});
unknown('w', function () {
    var helper = 'other';
    consume(helper);
});
";
    const MANIFEST: &str =
        r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"index.js"}"#;
    const PREFIX: &str = "rift://symbol/npm/registry.npmjs.org/beacon/javascript/beacon/";
    let result = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", SOURCE)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("captured scoped JavaScript package");
    let publication = result.publication();
    publication
        .validate_facts()
        .expect("valid physical records");
    let unit = publication
        .units
        .iter()
        .find(|unit| unit.path.0 == "index.js")
        .expect("original source unit");
    let declaration = |text: &str| {
        let start = SOURCE.find(text).expect("written declaration");
        let range = TextRange {
            start: u64::try_from(start).expect("declaration start"),
            end: u64::try_from(start + text.len()).expect("declaration end"),
        };
        let mut declarations = publication
            .declarations
            .iter()
            .filter(|row| row.unit == unit.unit && row.range == range);
        let found = declarations.next().expect("physical declaration");
        assert!(declarations.next().is_none(), "{text}");
        assert!(!found.public);
        found
    };
    let first = declaration("function helper() { leaf(); }");
    let second = declaration("helper = 'first'");
    let same = declaration("helper = 'same'");
    let other = declaration("helper = 'other'");
    assert_eq!(first.symbol.as_str(), format!("{PREFIX}helper"));
    assert_eq!(second.symbol, same.symbol);
    assert_ne!(first.symbol, second.symbol);
    assert_ne!(first.symbol, other.symbol);
    assert_ne!(second.symbol, other.symbol);
    for (row, scopes) in [(second, "helper#1"), (other, "helper#3")] {
        let identity = SymbolIdentity::parse(row.symbol.as_str()).expect("scoped identity");
        assert_eq!(identity.qualified_path(), ["beacon", scopes]);
    }
    for (row, callable) in [(first, true), (second, false), (other, false)] {
        let symbol = publication
            .objects
            .iter()
            .find(|object| object.id.as_ref() == Some(&row.symbol))
            .expect("object for scope");
        assert_eq!(symbol.name, "helper");
        assert_eq!(symbol.kinds.contains(&SymbolKind::Callable), callable);
    }
    let calls = publication
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Calls] && edge.to.as_str() == format!("{PREFIX}leaf")
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].from, first.symbol);
    let authored = calls[0].occurrence.as_ref().expect("physical call");
    assert_eq!(authored.unit, unit.unit);
    assert_eq!(
        SOURCE.get(offset_in(authored.range.start)..offset_in(authored.range.end)),
        Some("leaf()")
    );
}

#[test]
fn package_stores_javascript_named_function_parameters_and_reads() {
    const SOURCE: &str = concat!(
        "export function choose(value) { return value; }\n",
        "export function* another(value) { yield value; }\n",
    );
    let package = fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.mjs", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","type":"module","main":"index.mjs"}"#,
        )),
    )
    .expect("admitted JavaScript source");
    let publication = package.publication();
    for (function, usage) in [("choose", "return value"), ("another", "yield value")] {
        let function_start = SOURCE.find(function).expect("written function");
        let declaration = function_start + function.len() + 1;
        let read = SOURCE.find(usage).expect("written read") + usage.len() - "value".len();
        let binding = publication
            .objects
            .iter()
            .find(|object| {
                object.kinds.contains(&SymbolKind::Parameter)
                    && object
                        .id
                        .as_ref()
                        .is_some_and(|id| id.as_str().ends_with(&format!("/{function}/value")))
            })
            .and_then(|object| object.id.as_ref())
            .expect("function-owned value parameter");
        assert!(publication.declarations.iter().any(|site| {
            site.symbol == *binding
                && site.source == "value"
                && usize::try_from(site.range.start) == Ok(declaration)
                && usize::try_from(site.range.end) == Ok(declaration + "value".len())
        }));
        let uses = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to == *binding)
            .map(|edge| {
                assert!(edge.from.as_str().ends_with(&format!("/{function}")));
                let site = &edge.occurrence.as_ref().expect("written read").range;
                assert_eq!(usize::try_from(site.end), Ok(read + "value".len()));
                usize::try_from(site.start).expect("written start")
            })
            .collect::<Vec<_>>();
        assert_eq!(uses, [read]);
    }
}

#[test]
fn package_javascript_parameters_follow_typescript_parameters_under_provider_bound() {
    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("package identity");
    let origin = fixture::origin(&package);
    let language = fixture::language(ShippedLanguage::TypeScript);
    let javascript = ProjectPath::new("a.mjs").expect("JavaScript path");
    let typescript = ProjectPath::new("index.ts").expect("TypeScript path");
    let manifest = ProjectPath::new("package.json").expect("metadata path");
    let sources = [
        PackageSource::new(&javascript, "export function first(gift) { return gift; }"),
        PackageSource::new(
            &typescript,
            "export function last(value: number): number { return value; }",
        ),
    ];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","main":"a.mjs","types":"index.ts"}"#,
    )];

    for (bound, expected) in [
        (2, &[][..]),
        (3, &["value"][..]),
        (4, &["gift", "value"][..]),
    ] {
        let limits = ExactPackageLimits::new(3, 4096)
            .with_declarations(bound)
            .expect("provider declaration bound");
        let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
            .expect("package input")
            .with_framework_context(&metadata, &[])
            .expect("authored metadata");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("primary declarations accepted");
        let publication = analysis.publication();
        let parameters = publication
            .objects
            .iter()
            .filter(|object| object.kinds.contains(&SymbolKind::Parameter))
            .map(|object| object.name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            parameters,
            expected.iter().copied().collect(),
            "bound {bound}, declarations {:?}, warnings {:?}",
            publication
                .declarations
                .iter()
                .map(|row| &row.symbol)
                .collect::<Vec<_>>(),
            publication.warnings
        );
        let reads = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads])
            .filter_map(|edge| edge.to.as_str().rsplit('/').next())
            .collect::<BTreeSet<_>>();
        assert_eq!(reads, parameters);
    }
}

#[test]
fn package_retains_typescript_method_parameters_and_body_reads() {
    const SOURCE: &str = concat!(
        "export class Holder { method(value: number) { return value; } }\n",
        "export interface Reader { method(value: number): number; }\n",
        "export function choose(name: number) { return name; }\n",
    );
    let analysis = fixture::package_result_with_metadata(
        ShippedLanguage::TypeScript,
        vec![("index.ts", SOURCE)],
        None,
        Some((
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","types":"index.ts"}"#,
        )),
    )
    .expect("authored class and interface");
    let publication = analysis.publication();
    let holder = SOURCE.find("method(value").expect("class parameter") + "method(".len();
    let reader = SOURCE.rfind("method(value").expect("interface parameter") + "method(".len();
    let usage = SOURCE.find("return value").expect("class method read") + "return ".len();
    for (container, start, read) in [("Holder", holder, Some(usage)), ("Reader", reader, None)] {
        let identity = format!("/{container}/method/value");
        let binding = publication
            .objects
            .iter()
            .filter(|object| object.kinds.contains(&SymbolKind::Parameter))
            .filter_map(|object| object.id.as_ref())
            .find(|id| id.as_str().ends_with(&identity))
            .expect("method-owned value parameter");
        assert!(publication.declarations.iter().any(|site| {
            site.symbol == *binding
                && site.source == "value"
                && usize::try_from(site.range.start) == Ok(start)
                && usize::try_from(site.range.end) == Ok(start + "value".len())
        }));
        let uses = publication
            .edges
            .iter()
            .filter(|edge| edge.kinds == [EdgeKind::Reads] && edge.to == *binding)
            .map(|edge| {
                assert!(
                    edge.from
                        .as_str()
                        .ends_with(&format!("/{container}/method"))
                );
                let site = &edge.occurrence.as_ref().expect("written read").range;
                assert_eq!(site.end - site.start, 5);
                usize::try_from(site.start).expect("physical byte offset")
            })
            .collect::<Vec<_>>();
        assert_eq!(uses, read.into_iter().collect::<Vec<_>>());
    }

    let mut package = fixture::identity();
    package.manager = "npm".to_owned();
    package.registry = "registry.npmjs.org".to_owned();
    let owner = package.owner().expect("package identity");
    let origin = fixture::origin(&package);
    let language = fixture::language(ShippedLanguage::TypeScript);
    let path = ProjectPath::new("index.ts").expect("source path");
    let manifest = ProjectPath::new("package.json").expect("metadata path");
    let sources = [PackageSource::new(&path, SOURCE)];
    let metadata = [PackageSource::new(
        &manifest,
        r#"{"name":"beacon","version":"1.0.0","types":"index.ts"}"#,
    )];
    let limits = ExactPackageLimits::new(2, 4096)
        .with_declarations(6)
        .expect("five ordinary declarations and one parameter");
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("package input")
        .with_framework_context(&metadata, &[])
        .expect("authored metadata");
    let bounded = PackageAnalyzer::analyze(input, 1).expect("primary declarations accepted");
    let parameters = bounded
        .publication()
        .objects
        .iter()
        .filter(|object| object.kinds.contains(&SymbolKind::Parameter))
        .map(|object| object.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(parameters, ["name"]);
    assert!(bounded.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/choose/name")
    }));
}
