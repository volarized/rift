"""Tests for deterministic Rift release packaging."""

from __future__ import annotations

import importlib.util
import stat
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).parents[1] / "release.py"
SPEC = importlib.util.spec_from_file_location("rift_release", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load release helper")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseVersionTests(unittest.TestCase):
    """Tag and workspace version checks."""

    def test_accepts_canonical_tag(self) -> None:
        self.assertEqual(release.release_version("v1.20.3"), "1.20.3")

    def test_rejects_noncanonical_tags(self) -> None:
        for tag in ("1.2.3", "v01.2.3", "v1.2", "v1.2.3-rc.1", "docs-v1.2.3"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.release_version(tag)

    def test_reports_every_workspace_version_mismatch(self) -> None:
        packages = [
            {"name": "rift", "version": "1.2.3"},
            {"name": "rift-core", "version": "1.2.2"},
        ]
        with (
            tempfile.TemporaryDirectory() as temporary,
            mock.patch.object(release, "cargo_packages", return_value=packages),
        ):
            root = Path(temporary)
            for name in ("Cargo.lock", "LICENSE", "README.md"):
                (root / name).touch()
            with self.assertRaisesRegex(ValueError, "rift-core=1.2.2"):
                release.validate_workspace_version(root, "v1.2.3")

    def test_binary_version_must_equal_tag(self) -> None:
        version_process = mock.Mock(returncode=0, stdout="rift 1.2.3\n")
        help_process = mock.Mock(returncode=0, stdout="Usage: rift [OPTIONS]\n")
        with (
            tempfile.TemporaryDirectory() as temporary,
            mock.patch.object(
                release.subprocess,
                "run",
                side_effect=[version_process, help_process],
            ),
        ):
            binary = Path(temporary) / "rift"
            binary.touch()
            release.verify_binary_version(binary, "v1.2.3")

        version_process.stdout = "rift 1.2.2\n"
        with (
            tempfile.TemporaryDirectory() as temporary,
            mock.patch.object(release.subprocess, "run", return_value=version_process),
        ):
            binary = Path(temporary) / "rift"
            binary.touch()
            with self.assertRaisesRegex(ValueError, "rift 1.2.3"):
                release.verify_binary_version(binary, "v1.2.3")

    def test_binary_help_is_required(self) -> None:
        version_process = mock.Mock(returncode=0, stdout="rift 1.2.3\n")
        help_process = mock.Mock(returncode=0, stdout="")
        with (
            tempfile.TemporaryDirectory() as temporary,
            mock.patch.object(
                release.subprocess,
                "run",
                side_effect=[version_process, help_process],
            ),
        ):
            binary = Path(temporary) / "rift"
            binary.touch()
            with self.assertRaisesRegex(ValueError, "expose rift help"):
                release.verify_binary_version(binary, "v1.2.3")


class ReleaseArchiveTests(unittest.TestCase):
    """Archive shape and checksum checks."""

    def test_package_has_exact_files_and_modes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "rift"
            binary.write_bytes(b"#!/bin/sh\necho rift\n")
            binary.chmod(0o755)
            (root / "README.md").write_text("Rift\n", encoding="utf-8")
            (root / "LICENSE").write_text("Apache-2.0\n", encoding="utf-8")

            archive_path = release.package_release(
                root,
                "v1.2.3",
                "x86_64-unknown-linux-gnu",
                binary,
                root / "dist",
            )
            with tarfile.open(archive_path, "r:gz") as archive:
                members = archive.getmembers()
                self.assertEqual(
                    [member.name for member in members],
                    [
                        "rift-v1.2.3-x86_64-unknown-linux-gnu/rift",
                        "rift-v1.2.3-x86_64-unknown-linux-gnu/README.md",
                        "rift-v1.2.3-x86_64-unknown-linux-gnu/LICENSE",
                    ],
                )
                self.assertEqual(stat.S_IMODE(members[0].mode), 0o755)
                self.assertEqual(stat.S_IMODE(members[1].mode), 0o644)
                self.assertTrue(all(member.mtime == 0 for member in members))

    def test_windows_package_has_exact_files_and_modes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "rift.exe"
            binary.write_bytes(b"windows binary")
            (root / "README.md").write_text("Rift\n", encoding="utf-8")
            (root / "LICENSE").write_text("Apache-2.0\n", encoding="utf-8")

            archive_path = release.package_release(
                root,
                "v1.2.3",
                "x86_64-pc-windows-msvc",
                binary,
                root / "dist",
            )
            with zipfile.ZipFile(archive_path) as archive:
                members = archive.infolist()
                self.assertEqual(
                    [member.filename for member in members],
                    [
                        "rift-v1.2.3-x86_64-pc-windows-msvc/rift.exe",
                        "rift-v1.2.3-x86_64-pc-windows-msvc/README.md",
                        "rift-v1.2.3-x86_64-pc-windows-msvc/LICENSE",
                    ],
                )
                self.assertEqual(members[0].date_time, release.ZIP_DATE)
                self.assertEqual(members[0].external_attr >> 16, 0o755)
                self.assertEqual(members[1].external_attr >> 16, 0o644)

    def test_checksum_manifest_requires_every_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for target in release.SUPPORTED_TARGETS:
                name = release.archive_name("v1.2.3", target)
                (directory / name).write_bytes(target.encode())

            manifest = release.checksum_manifest("v1.2.3", directory)
            lines = manifest.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), len(release.SUPPORTED_TARGETS))
            self.assertEqual(lines, sorted(lines, key=lambda line: line.split("  ")[1]))

            (
                directory / release.archive_name("v1.2.3", release.SUPPORTED_TARGETS[0])
            ).unlink()
            with self.assertRaisesRegex(ValueError, "missing"):
                release.checksum_manifest("v1.2.3", directory)

    def test_archives_are_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "rift"
            binary.write_bytes(b"rift")
            binary.chmod(0o755)
            (root / "README.md").write_bytes(b"readme")
            (root / "LICENSE").write_bytes(b"license")

            first = release.package_release(
                root, "v1.2.3", release.SUPPORTED_TARGETS[0], binary, root / "first"
            )
            second = release.package_release(
                root, "v1.2.3", release.SUPPORTED_TARGETS[0], binary, root / "second"
            )
            self.assertEqual(first.read_bytes(), second.read_bytes())

            windows_binary = root / "rift.exe"
            windows_binary.write_bytes(b"windows")
            first_zip = release.package_release(
                root,
                "v1.2.3",
                "aarch64-pc-windows-msvc",
                windows_binary,
                root / "first",
            )
            second_zip = release.package_release(
                root,
                "v1.2.3",
                "aarch64-pc-windows-msvc",
                windows_binary,
                root / "second",
            )
            self.assertEqual(first_zip.read_bytes(), second_zip.read_bytes())


if __name__ == "__main__":
    unittest.main()
