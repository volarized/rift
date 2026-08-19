"""Tests for deterministic Rift release packaging."""

from __future__ import annotations

import stat
import tarfile
import zipfile
from pathlib import Path
from types import SimpleNamespace

import pytest
from rift_release import release
from rift_release.cli import app
from typer.testing import CliRunner


@pytest.mark.parametrize(
    "tag",
    ["1.2.3", "v01.2.3", "v1.2", "v1.2.3-rc.1", "docs-v1.2.3"],
)
def test_release_version_rejects_noncanonical_tag(tag: str) -> None:
    with pytest.raises(ValueError, match="release tag must match"):
        release.release_version(tag)


def test_release_version_accepts_canonical_tag() -> None:
    assert release.release_version("v1.20.3") == "1.20.3"


def test_workspace_version_reports_every_mismatch(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    packages = [
        {"name": "rift", "version": "1.2.3"},
        {"name": "rift-core", "version": "1.2.2"},
    ]
    monkeypatch.setattr(release, "cargo_packages", lambda _repository: packages)
    for name in ("Cargo.lock", "LICENSE.md", "README.md"):
        (tmp_path / name).touch()

    with pytest.raises(ValueError, match="rift-core=1.2.2"):
        release.validate_workspace_version(tmp_path, "v1.2.3")


def test_binary_version_must_equal_tag(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    binary = tmp_path / "rift"
    binary.touch()
    processes = iter(
        [
            SimpleNamespace(returncode=0, stdout="rift 1.2.3\n"),
            SimpleNamespace(returncode=0, stdout="Usage: rift [OPTIONS]\n"),
        ]
    )
    monkeypatch.setattr(
        release.subprocess, "run", lambda *args, **kwargs: next(processes)
    )
    release.verify_binary_version(binary, "v1.2.3")

    mismatch = SimpleNamespace(returncode=0, stdout="rift 1.2.2\n")
    monkeypatch.setattr(release.subprocess, "run", lambda *args, **kwargs: mismatch)
    with pytest.raises(ValueError, match="rift 1.2.3"):
        release.verify_binary_version(binary, "v1.2.3")


def test_binary_help_is_required(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    binary = tmp_path / "rift"
    binary.touch()
    processes = iter(
        [
            SimpleNamespace(returncode=0, stdout="rift 1.2.3\n"),
            SimpleNamespace(returncode=0, stdout=""),
        ]
    )
    monkeypatch.setattr(
        release.subprocess, "run", lambda *args, **kwargs: next(processes)
    )

    with pytest.raises(ValueError, match="expose rift help"):
        release.verify_binary_version(binary, "v1.2.3")


def test_package_has_exact_files_and_modes(tmp_path: Path) -> None:
    binary = tmp_path / "rift"
    binary.write_bytes(b"#!/bin/sh\necho rift\n")
    binary.chmod(0o755)
    (tmp_path / "README.md").write_text("Rift\n", encoding="utf-8")
    (tmp_path / "LICENSE.md").write_text("MIT\n", encoding="utf-8")

    archive_path = release.package_release(
        tmp_path,
        "v1.2.3",
        "x86_64-unknown-linux-gnu",
        binary,
        tmp_path / "dist",
    )
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        assert [member.name for member in members] == [
            "rift-v1.2.3-x86_64-unknown-linux-gnu/rift",
            "rift-v1.2.3-x86_64-unknown-linux-gnu/README.md",
            "rift-v1.2.3-x86_64-unknown-linux-gnu/LICENSE.md",
        ]
        assert stat.S_IMODE(members[0].mode) == 0o755
        assert stat.S_IMODE(members[1].mode) == 0o644
        assert all(member.mtime == 0 for member in members)


def test_windows_package_has_exact_files_and_modes(tmp_path: Path) -> None:
    binary = tmp_path / "rift.exe"
    binary.write_bytes(b"windows binary")
    (tmp_path / "README.md").write_text("Rift\n", encoding="utf-8")
    (tmp_path / "LICENSE.md").write_text("MIT\n", encoding="utf-8")

    archive_path = release.package_release(
        tmp_path,
        "v1.2.3",
        "x86_64-pc-windows-msvc",
        binary,
        tmp_path / "dist",
    )
    with zipfile.ZipFile(archive_path) as archive:
        members = archive.infolist()
        assert [member.filename for member in members] == [
            "rift-v1.2.3-x86_64-pc-windows-msvc/rift.exe",
            "rift-v1.2.3-x86_64-pc-windows-msvc/README.md",
            "rift-v1.2.3-x86_64-pc-windows-msvc/LICENSE.md",
        ]
        assert members[0].date_time == release.ZIP_DATE
        assert members[0].external_attr >> 16 == 0o755
        assert members[1].external_attr >> 16 == 0o644


def test_checksum_manifest_requires_every_target(tmp_path: Path) -> None:
    for target in release.SUPPORTED_TARGETS:
        name = release.archive_name("v1.2.3", target)
        (tmp_path / name).write_bytes(target.encode())

    manifest = release.checksum_manifest("v1.2.3", tmp_path)
    lines = manifest.read_text(encoding="utf-8").splitlines()
    assert len(lines) == len(release.SUPPORTED_TARGETS)
    assert lines == sorted(lines, key=lambda line: line.split("  ")[1])

    (tmp_path / release.archive_name("v1.2.3", release.SUPPORTED_TARGETS[0])).unlink()
    with pytest.raises(ValueError, match="missing"):
        release.checksum_manifest("v1.2.3", tmp_path)


def test_archives_are_reproducible(tmp_path: Path) -> None:
    binary = tmp_path / "rift"
    binary.write_bytes(b"rift")
    binary.chmod(0o755)
    (tmp_path / "README.md").write_bytes(b"readme")
    (tmp_path / "LICENSE.md").write_bytes(b"license")

    first = release.package_release(
        tmp_path, "v1.2.3", release.SUPPORTED_TARGETS[0], binary, tmp_path / "first"
    )
    second = release.package_release(
        tmp_path, "v1.2.3", release.SUPPORTED_TARGETS[0], binary, tmp_path / "second"
    )
    assert first.read_bytes() == second.read_bytes()

    windows_binary = tmp_path / "rift.exe"
    windows_binary.write_bytes(b"windows")
    first_zip = release.package_release(
        tmp_path,
        "v1.2.3",
        "aarch64-pc-windows-msvc",
        windows_binary,
        tmp_path / "first",
    )
    second_zip = release.package_release(
        tmp_path,
        "v1.2.3",
        "aarch64-pc-windows-msvc",
        windows_binary,
        tmp_path / "second",
    )
    assert first_zip.read_bytes() == second_zip.read_bytes()


def test_cli_reports_operation_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def reject(_repository: Path, _tag: str) -> None:
        raise ValueError("version mismatch")

    monkeypatch.setattr(release, "validate_workspace_version", reject)
    result = CliRunner().invoke(
        app,
        ["validate-tag", "--tag", "v1.2.3", "--repository", str(tmp_path)],
    )
    assert result.exit_code == 1
    assert "error: version mismatch" in result.stderr
