"""The ownership check refuses every backend import and declaration outside `rift-tracing`.

Each fixture is a workspace package written to disk with the Cargo metadata
shape `cargo metadata --no-deps` returns, so the check resolves a library the
way it does on the real workspace: through the dependency's package name and
its `rename`, never through the identifier alone.
"""

from pathlib import Path
from typing import Any

import pytest
from rift_dev import check_rust_architecture as architecture


def package(
    root: Path,
    name: str,
    dependencies: list[dict[str, Any]],
    files: dict[str, str],
    features: dict[str, list[str]] | None = None,
) -> dict[str, Any]:
    directory = root / name
    files = {"Cargo.toml": f'[package]\nname = "{name}"\n', **files}
    for relative, text in files.items():
        path = directory / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return {
        "name": name,
        "manifest_path": str(directory / "Cargo.toml"),
        "dependencies": dependencies,
        "features": features or {},
    }


def dependency(name: str, **fields: Any) -> dict[str, Any]:
    return {
        "name": name,
        "rename": None,
        "kind": None,
        "optional": False,
        "features": [],
        "target": None,
        **fields,
    }


def found(packages: list[dict[str, Any]]) -> set[tuple[str, str, int]]:
    return {
        (finding.package, finding.library, finding.line)
        for finding in architecture.ownership_findings(packages)
    }


def test_plain_import_and_qualified_call_are_listed(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing")],
        {
            "src/lib.rs": (
                'use tracing::info;\nfn run() {\n    tracing::debug!("x");\n}\n'
            )
        },
    )
    assert found([consumer]) == {("consumer", "tracing", 1), ("consumer", "tracing", 3)}


def test_renamed_dependency_is_found_by_its_package_name(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing", rename="spans")],
        {"src/lib.rs": "use spans::info;\nextern crate spans;\n"},
    )
    assert found([consumer]) == {("consumer", "tracing", 1), ("consumer", "tracing", 2)}


def test_dev_dependency_in_a_test_file_is_listed(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing-subscriber", kind="dev")],
        {
            "src/lib.rs": "pub fn run() {}\n",
            "tests/capture.rs": "use tracing_subscriber::fmt;\n",
        },
    )
    assert found([consumer]) == {("consumer", "tracing-subscriber", 1)}


def test_code_behind_an_inactive_feature_is_listed(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("opentelemetry_sdk", optional=True)],
        {
            "src/export.rs": (
                '#[cfg(feature = "export")]\n'
                "mod export {\n"
                "    use opentelemetry_sdk::trace::Tracer;\n"
                "}\n"
            )
        },
    )
    assert found([consumer]) == {("consumer", "opentelemetry_sdk", 3)}


def test_attribute_and_reexport_are_listed(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing")],
        {
            "src/lib.rs": (
                "#[tracing::instrument]\n"
                "fn work() {}\n"
                "pub use tracing::info as report;\n"
                "pub use tracing as spans;\n"
            )
        },
    )
    assert found([consumer]) == {
        ("consumer", "tracing", 1),
        ("consumer", "tracing", 3),
        ("consumer", "tracing", 4),
    }


def test_facade_imported_under_a_backend_name_is_listed(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("rift-tracing")],
        {"src/lib.rs": "use rift_tracing as tracing;\nuse rift_tracing::info;\n"},
    )
    assert found([consumer]) == {("consumer", "rift-tracing alias", 1)}


def test_doctest_is_listed_and_prose_comment_is_not(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing")],
        {
            "src/lib.rs": (
                "// tracing::debug! in a plain comment\n"
                "/// Mentions tracing::info! in prose.\n"
                "///\n"
                "/// ```\n"
                '/// tracing::info!("x");\n'
                "/// ```\n"
                "fn documented() {}\n"
            )
        },
    )
    assert found([consumer]) == {("consumer", "tracing", 5)}


def test_domain_module_named_log_is_not_the_log_library(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("serde")],
        {
            "src/lib.rs": "mod log;\nuse crate::log::Record;\nfn f() { log::append(); }\n"
        },
    )
    assert found([consumer]) == set()


def test_rift_tracing_owns_the_backends(tmp_path: Path) -> None:
    owner = package(
        tmp_path,
        "rift-tracing",
        [dependency("tracing"), dependency("tracing-subscriber")],
        {"src/lib.rs": "use tracing::info;\nuse tracing_subscriber::fmt;\n"},
    )
    assert found([owner]) == set()


def test_internal_attribute_package_is_covered(tmp_path: Path) -> None:
    attributes = package(
        tmp_path,
        "rift-tracing-macros",
        [dependency("tracing")],
        {"src/lib.rs": "use tracing::Span;\n"},
    )
    assert found([attributes]) == {("rift-tracing-macros", "tracing", 1)}


def complaints(packages: list[dict[str, Any]], resolved: str = "") -> list[str]:
    return architecture.ownership_complaints(packages, resolved)


def test_workspace_with_one_backend_import_fails(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing")],
        {
            "Cargo.toml": (
                '[package]\nname = "consumer"\n\n[dependencies]\n'
                "tracing.workspace = true\n"
            ),
            "src/lib.rs": "pub fn run() {}\nuse tracing::info;\n",
        },
    )
    source = tmp_path / "consumer" / "src" / "lib.rs"
    manifest = tmp_path / "consumer" / "Cargo.toml"
    assert complaints([consumer]) == [
        (
            f"consumer: {source}:2: use tracing::info;: imports tracing; use the "
            "rift_tracing facade: traced!, measure_elapsed!, info_span!, debug_span!, "
            "Span, and trace! through error!"
        ),
        (
            f"consumer: {manifest}:5: tracing.workspace = true: declares tracing; "
            "remove the line and use the rift_tracing facade: traced!, "
            "measure_elapsed!, info_span!, debug_span!, Span, and trace! through error!"
        ),
    ]


def test_clean_workspace_passes(tmp_path: Path) -> None:
    owner = package(
        tmp_path,
        "rift-tracing",
        [dependency("tracing"), dependency("tracing-subscriber")],
        {"src/lib.rs": "pub use tracing::info;\n"},
        {"fixtures": []},
    )
    consumer = package(
        tmp_path,
        "consumer",
        [
            dependency("rift-tracing"),
            dependency("rift-tracing", kind="dev", features=["fixtures"]),
        ],
        {"src/lib.rs": 'fn run() {\n    rift_tracing::info!("x");\n}\n'},
    )
    assert complaints([owner, consumer]) == []


def test_dev_dependency_declaration_alone_fails(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing-subscriber", kind="dev")],
        {
            "Cargo.toml": (
                '[package]\nname = "consumer"\n\n[dependencies]\n'
                'tracing-subscriber = "0.3"\n\n[dev-dependencies]\n'
                "tracing-subscriber.workspace = true\n"
            ),
            "src/lib.rs": "pub fn run() {}\n",
        },
    )
    manifest = tmp_path / "consumer" / "Cargo.toml"
    assert complaints([consumer]) == [
        (
            f"consumer: {manifest}:8: tracing-subscriber.workspace = true: declares "
            "tracing-subscriber; remove the line and install through "
            "rift_tracing::TracingRuntime, and capture in tests through "
            "rift_tracing::ScopedRecorder"
        )
    ]


def test_target_table_declaration_names_its_line(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("opentelemetry_sdk", target="cfg(windows)")],
        {
            "Cargo.toml": (
                '[package]\nname = "consumer"\n\n'
                "[target.'cfg(windows)'.dependencies.opentelemetry_sdk]\n"
                "workspace = true\n"
            ),
        },
    )
    manifest = tmp_path / "consumer" / "Cargo.toml"
    assert complaints([consumer]) == [
        (
            f"consumer: {manifest}:4: [target.'cfg(windows)'.dependencies."
            "opentelemetry_sdk]: declares opentelemetry_sdk; remove the line and "
            "export through rift-tracing, which owns the OTLP export"
        )
    ]


def test_hidden_expansion_machinery_is_refused(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("rift-tracing")],
        {
            "src/lib.rs": (
                "use rift_tracing::__private::tracing;\n"
                "use rift_tracing::{\n"
                "    info,\n"
                "    __rift_traced_span,\n"
                "};\n"
                "use rift_tracing::traced;\n"
            )
        },
    )
    assert found([consumer]) == {
        ("consumer", "rift_tracing::__private", 1),
        ("consumer", "rift_tracing::__rift_traced_span", 4),
    }
    assert "which only the rift-tracing macros expand to" in complaints([consumer])[0]


def test_fixtures_from_a_normal_dependency_is_refused(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [
            dependency("rift-tracing", features=["fixtures"]),
            dependency("rift-tracing", kind="dev", features=["fixtures"]),
        ],
        {
            "Cargo.toml": (
                '[package]\nname = "consumer"\n\n[dependencies]\n'
                'rift-tracing = { workspace = true, features = ["fixtures"] }\n\n'
                "[dev-dependencies]\n"
                'rift-tracing = { workspace = true, features = ["fixtures"] }\n'
            ),
        },
    )
    manifest = tmp_path / "consumer" / "Cargo.toml"
    assert complaints([consumer]) == [
        (
            f"consumer: {manifest}:5: rift-tracing = {{ workspace = true, features = "
            '["fixtures"] }: enables the rift-tracing fixtures feature outside '
            "[dev-dependencies]; enable it from [dev-dependencies] only"
        )
    ]


def test_feature_forwarding_to_fixtures_is_refused(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("rift-tracing")],
        {
            "Cargo.toml": (
                '[package]\nname = "consumer"\n\n[features]\n'
                'testing = ["rift-tracing/fixtures"]\n'
            ),
        },
        {"testing": ["rift-tracing/fixtures"]},
    )
    manifest = tmp_path / "consumer" / "Cargo.toml"
    assert complaints([consumer]) == [
        (
            f'consumer: {manifest}:5: testing = ["rift-tracing/fixtures"]: enables '
            "the rift-tracing fixtures feature outside [dev-dependencies]; enable it "
            "from [dev-dependencies] only"
        )
    ]


def test_fixtures_resolved_in_the_normal_graph_is_refused(tmp_path: Path) -> None:
    resolved = (
        "rift-tracing v0.0.47\n"
        '└── rift-tracing feature "fixtures"\n'
        '    └── rift-index feature "testing"\n'
    )
    assert complaints([], resolved) == [
        (
            'the normal or build graph resolves rift-tracing feature "fixtures", so a '
            "release build carries the test recorder; enable it from "
            "[dev-dependencies] only:\n" + resolved
        )
    ]


def test_enforce_mode_fails_on_findings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    consumer = package(
        tmp_path,
        "rift",
        [dependency("tracing")],
        {"src/lib.rs": "use tracing::info;\n"},
    )
    consumer["targets"] = [{"kind": ["bin"], "name": "rift"}]
    generator = package(tmp_path, "rift-schema-export", [], {"src/lib.rs": ""})
    generator["targets"] = [{"kind": ["bin"], "name": "rift-schema-export"}]
    packages = [consumer, generator]
    monkeypatch.setattr(architecture, "cargo_metadata", dict)
    monkeypatch.setattr(architecture, "rift_packages", lambda _: packages)
    monkeypatch.setattr(
        architecture, "dependency_edges", lambda _: architecture.EXPECTED_EDGES
    )
    monkeypatch.setattr(architecture, "fail_test_targets", lambda _: None)
    monkeypatch.setattr(architecture, "fail_storage_independence", lambda: None)
    monkeypatch.setattr(architecture, "resolved_fixtures", lambda: "")

    with pytest.raises(
        RuntimeError, match="Backend libraries are owned by rift-tracing"
    ):
        architecture.main()


def clock_found(packages: list[dict[str, Any]]) -> set[tuple[str, str, int]]:
    return {
        (finding.package, Path(finding.path).name, finding.line)
        for finding in architecture.clock_findings(packages)
    }


def test_a_clock_read_in_an_unlisted_file_is_refused(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [],
        {
            "src/lib.rs": (
                "fn run() {\n"
                "    let started = std::time::Instant::now();\n"
                "    let wall = SystemTime::now().duration_since(UNIX_EPOCH);\n"
                "    let took = started.elapsed();\n"
                "}\n"
            )
        },
    )
    assert clock_found([consumer]) == {
        ("consumer", "lib.rs", 2),
        ("consumer", "lib.rs", 3),
        ("consumer", "lib.rs", 4),
    }
    with pytest.raises(RuntimeError, match=r"lib\.rs:2: .*Instant::now"):
        architecture.fail_clocks([consumer])


def test_a_listed_file_a_test_suite_and_rift_tracing_may_read_clocks(
    tmp_path: Path,
) -> None:
    read = "fn run() {\n    let started = Instant::now();\n}\n"
    listed = package(tmp_path, "rift-lsp", [], {"src/session.rs": read})
    suite = package(tmp_path, "consumer", [], {"tests/wait.rs": read})
    owner = package(tmp_path, "rift-tracing", [], {"src/clock.rs": read})
    assert clock_found([listed, suite, owner]) == set()
    architecture.fail_clocks([listed, suite, owner])


def test_a_clock_name_in_a_comment_or_another_identifier_is_not_a_read(
    tmp_path: Path,
) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [],
        {
            "src/lib.rs": (
                "// Instant::now() in prose\\n"
                "fn run(instant: u8) -> u8 {\n"
                "    let _ = instant;\n"
                "    MyInstant::new()\n"
                "}\n"
            ).replace("\\n", "\n")
        },
    )
    assert clock_found([consumer]) == set()


def scoped_workspace(
    root: Path, bound: str, emitters: list[str]
) -> list[dict[str, Any]]:
    """An owner declaring `SCOPES_MAX = bound` and one emitting crate per name."""
    owner = package(
        root,
        "rift-tracing",
        [],
        {
            "src/metrics.rs": (
                "pub(crate) const SCOPE: InstrumentScope =\n"
                '    InstrumentScope::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));\n'
                f"pub const SCOPES_MAX: usize = {bound};\n"
            )
        },
    )
    crates = [
        package(
            root,
            name,
            [],
            {
                "src/lib.rs": (
                    "const SCOPE: rift_tracing::InstrumentScope =\n"
                    "    rift_tracing::InstrumentScope::new("
                    'env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));\n'
                )
            },
        )
        for name in emitters
    ]
    return [owner, *crates]


def test_scopes_max_is_read_from_the_rift_tracing_source(tmp_path: Path) -> None:
    assert architecture.scopes_max(scoped_workspace(tmp_path, "6", [])) == 6


def test_a_rift_tracing_source_without_scopes_max_is_refused(tmp_path: Path) -> None:
    packages = scoped_workspace(tmp_path, "6", [])
    metrics = Path(packages[0]["manifest_path"]).parent / "src/metrics.rs"
    metrics.write_text("pub const OTHER: usize = 6;\n", encoding="utf-8")
    with pytest.raises(RuntimeError, match="declares no `pub const SCOPES_MAX"):
        architecture.scopes_max(packages)


def test_emitting_crates_as_many_as_scopes_max_pass(tmp_path: Path) -> None:
    packages = scoped_workspace(tmp_path, "3", ["rift-a", "rift-b"])
    assert architecture.emitting_crates(packages) == [
        "rift-tracing",
        "rift-a",
        "rift-b",
    ]
    architecture.fail_scopes(packages)


def test_one_emitting_crate_past_scopes_max_is_refused(tmp_path: Path) -> None:
    packages = scoped_workspace(tmp_path, "3", ["rift-a", "rift-b", "rift-c"])
    with pytest.raises(
        RuntimeError,
        match=r"4 crates define an instrumentation scope, past SCOPES_MAX = 3: "
        r"rift-tracing, rift-a, rift-b, rift-c",
    ):
        architecture.fail_scopes(packages)


def test_traced_in_shipped_source_counts_and_in_a_test_suite_or_comment_does_not(
    tmp_path: Path,
) -> None:
    packages = scoped_workspace(tmp_path, "9", [])
    shipped = package(
        tmp_path,
        "rift-shipped",
        [],
        {"src/lib.rs": 'fn run() { rift_tracing::traced!(component = "a", {}); }\n'},
    )
    attribute = package(
        tmp_path,
        "rift-attribute",
        [],
        {"src/lib.rs": "#[rift_tracing::traced]\nfn run() {}\n"},
    )
    quiet = package(
        tmp_path,
        "rift-quiet",
        [],
        {
            "src/lib.rs": "// rift_tracing::traced!(a)\nfn run() {}\n",
            "src/tests.rs": "fn t() { rift_tracing::traced!(a); }\n",
            "tests/suite.rs": "fn t() { rift_tracing::traced!(a); }\n",
        },
    )
    assert architecture.emitting_crates([*packages, shipped, attribute, quiet]) == [
        "rift-tracing",
        "rift-shipped",
        "rift-attribute",
    ]


def test_a_procedural_macro_crate_naming_traced_is_not_an_emitter(
    tmp_path: Path,
) -> None:
    packages = scoped_workspace(tmp_path, "9", [])
    expansion = package(
        tmp_path, "rift-macros", [], {"src/lib.rs": "fn x() { quote!(traced!(a)); }\n"}
    )
    expansion["targets"] = [{"kind": ["proc-macro"]}]
    assert architecture.emitting_crates([*packages, expansion]) == ["rift-tracing"]
