"""The ownership check lists every backend import outside `rift-tracing`.

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
) -> dict[str, Any]:
    directory = root / name
    for relative, text in files.items():
        path = directory / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return {
        "name": name,
        "manifest_path": str(directory / "Cargo.toml"),
        "dependencies": dependencies,
    }


def dependency(name: str, **fields: Any) -> dict[str, Any]:
    return {"name": name, "rename": None, "kind": None, "optional": False, **fields}


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
                '#[cfg(feature = "otlp")]\n'
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


def test_report_mode_summarizes_and_lists_each_package(tmp_path: Path) -> None:
    consumer = package(
        tmp_path,
        "consumer",
        [dependency("tracing"), dependency("log")],
        {"src/lib.rs": "use tracing::info;\nuse log::warn;\nuse tracing::debug;\n"},
    )
    lines = architecture.ownership_report(
        [consumer], architecture.ownership_findings([consumer])
    )
    assert lines == [
        "  consumer: 3 (log 1, tracing 2)",
        (
            "ownership (report only): 3 backend imports outside rift-tracing in 1 "
            "packages, 2 backend dependencies declared there"
        ),
    ]


def test_report_mode_never_fails_on_findings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
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

    assert architecture.main() == 0
    assert "ownership (report only): 1 backend imports" in capsys.readouterr().out
