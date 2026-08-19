#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Offline contract tests for downloadable Rift installers."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path

REPOSITORY = Path(__file__).parents[2]
RELEASE_SCRIPT = REPOSITORY / "scripts" / "release.py"
SPEC = importlib.util.spec_from_file_location("rift_release", RELEASE_SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load release helper")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)

VERSION = "v1.2.3"


def architecture() -> str:
    """Return release architecture for current machine."""
    machine = platform.machine().lower()
    if machine in {"amd64", "x86_64"}:
        return "x86_64"
    if machine in {"aarch64", "arm64"}:
        return "aarch64"
    raise RuntimeError(f"unsupported test architecture: {machine}")


def unix_target() -> str:
    """Return native Unix release target."""
    system = platform.system()
    suffix = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu"}.get(system)
    if suffix is None:
        raise RuntimeError(f"unsupported Unix test system: {system}")
    return f"{architecture()}-{suffix}"


def windows_target() -> str:
    """Return native Windows release target."""
    return f"{architecture()}-pc-windows-msvc"


def write_manifest(directory: Path, archive: Path, *, digest: str | None = None) -> Path:
    """Write combined release checksum manifest for one fixture archive."""
    value = digest or hashlib.sha256(archive.read_bytes()).hexdigest()
    manifest = directory / f"rift-{VERSION}-checksums.sha256"
    manifest.write_text(f"{value}  {archive.name}\n", encoding="utf-8")
    return manifest


def package_fixture(root: Path, target: str, binary_name: str, data: bytes) -> Path:
    """Create fixture through production release packager."""
    repository = root / "repository"
    repository.mkdir()
    (repository / "README.md").write_text("Rift fixture\n", encoding="utf-8")
    (repository / "LICENSE").write_text("Apache-2.0\n", encoding="utf-8")
    binary = repository / binary_name
    binary.write_bytes(data)
    binary.chmod(0o755)
    artifacts = root / "artifacts"
    archive = release.package_release(repository, VERSION, target, binary, artifacts)
    write_manifest(artifacts, archive)
    return archive


def fake_curl(directory: Path) -> Path:
    """Create curl-compatible fixture transport without TLS bypasses."""
    executable = directory / "curl"
    executable.write_text(
        f"#!{sys.executable}\n"
        + textwrap.dedent(
            """
            import json
            import os
            import shutil
            import sys
            from pathlib import Path
            from urllib.parse import urlparse

            arguments = sys.argv[1:]
            with Path(os.environ["FAKE_CURL_LOG"]).open("a", encoding="utf-8") as log:
                log.write(json.dumps(arguments) + "\\n")
            url = arguments[-1]
            path = urlparse(url).path
            output = None
            for index, argument in enumerate(arguments):
                if argument in {"--output", "-o"}:
                    output = Path(arguments[index + 1])
            if path.endswith("/releases/latest"):
                content = json.dumps({"tag_name": os.environ["FIXTURE_TAG"]}).encode()
                if output is None:
                    sys.stdout.buffer.write(content)
                else:
                    output.write_bytes(content)
            else:
                source = Path(os.environ["FIXTURE_ROOT"]) / Path(path).name
                if not source.is_file():
                    print(f"fixture missing: {source}", file=sys.stderr)
                    raise SystemExit(22)
                if output is None:
                    sys.stdout.buffer.write(source.read_bytes())
                else:
                    shutil.copyfile(source, output)
            """
        ),
        encoding="utf-8",
    )
    executable.chmod(0o755)
    return executable


@unittest.skipIf(os.name == "nt", "bash installer targets Unix")
class BashInstallerTests(unittest.TestCase):
    """Validate curl installer through fake HTTPS transport."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.binary = b"#!/usr/bin/env bash\nprintf 'fixture-rift %s\\n' \"$*\"\n"
        self.archive = package_fixture(self.root, unix_target(), "rift", self.binary)
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        fake_curl(self.fake_bin)
        self.install_dir = self.root / "install"
        self.log = self.root / "curl.log"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_installer(
        self,
        version: str | None = VERSION,
        *,
        latest: str = VERSION,
        download_base: str = "https://fixture.invalid/download",
    ) -> subprocess.CompletedProcess[str]:
        """Run Bash installer with fixture environment."""
        environment = os.environ.copy()
        environment.update(
            {
                "FAKE_CURL_LOG": str(self.log),
                "FIXTURE_ROOT": str(self.archive.parent),
                "FIXTURE_TAG": latest,
                "PATH": f"{self.fake_bin}{os.pathsep}{environment['PATH']}",
                "RIFT_DOWNLOAD_BASE": download_base,
                "RIFT_GITHUB_API": "https://fixture.invalid/api",
                "RIFT_INSTALL_DIR": str(self.install_dir),
            }
        )
        command = ["bash", str(REPOSITORY / "scripts" / "install.sh")]
        if version is not None:
            command.append(version)
        return subprocess.run(
            command,
            env=environment,
            capture_output=True,
            check=False,
            text=True,
        )

    def test_explicit_and_latest_install(self) -> None:
        explicit = self.run_installer()
        self.assertEqual(explicit.returncode, 0, explicit.stderr)
        installed = self.install_dir / "rift"
        self.assertEqual(installed.read_bytes(), self.binary)
        self.assertTrue(os.access(installed, os.X_OK))

        shutil.rmtree(self.install_dir)
        latest = self.run_installer(None)
        self.assertEqual(latest.returncode, 0, latest.stderr)
        self.assertTrue(installed.is_file())

        requests = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertTrue(all("--proto-redir" in request for request in requests))
        self.assertTrue(all("=https" in request for request in requests))

    def test_bad_checksum_is_rejected_without_install(self) -> None:
        write_manifest(self.archive.parent, self.archive, digest="0" * 64)
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse((self.install_dir / "rift").exists())

    def test_invalid_latest_tag_is_rejected(self) -> None:
        result = self.run_installer(None, latest="2026-08-18")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid tag", result.stderr)

    def test_non_https_download_is_rejected(self) -> None:
        result = self.run_installer(download_base="http://fixture.invalid/download")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-HTTPS", result.stderr)

    def test_archive_without_exact_binary_is_rejected(self) -> None:
        root = self.archive.name.removesuffix(".tar.gz")
        data = b"wrong binary"
        with tarfile.open(self.archive, "w:gz") as archive:
            info = tarfile.TarInfo(f"{root}/not-rift")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        write_manifest(self.archive.parent, self.archive)

        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected files", result.stderr)
        self.assertFalse((self.install_dir / "rift").exists())


@unittest.skipUnless(os.name == "nt" and shutil.which("pwsh"), "Windows PowerShell is unavailable")
class PowerShellInstallerTests(unittest.TestCase):
    """Validate irm installer through overridden fetch boundary."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.binary = b"fixture windows binary"
        self.archive = package_fixture(self.root, windows_target(), "rift.exe", self.binary)
        self.install_dir = self.root / "install"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def harness(self) -> Path:
        """Write PowerShell harness overriding only network boundary."""
        harness = self.root / "harness.ps1"
        script = REPOSITORY / "scripts" / "install.ps1"
        harness.write_text(
            textwrap.dedent(
                f"""
                $ErrorActionPreference = "Stop"
                $env:RIFT_INSTALL_DIR = {json.dumps(str(self.install_dir))}
                $env:RIFT_DOWNLOAD_BASE = "https://fixture.invalid/download"
                $env:RIFT_GITHUB_API = "https://fixture.invalid/api"
                . {json.dumps(str(script))} -Version {json.dumps(VERSION)}

                try {{ Assert-HttpsUri ([uri]"http://fixture.invalid") ; throw "accepted HTTP" }}
                catch {{ if ($_.Exception.Message -notlike "*non-HTTPS*") {{ throw }} }}
                if (Test-Version "2026-08-18") {{ throw "accepted docs release tag" }}

                function Invoke-Fetch {{
                    param([uri]$Uri, [string]$OutFile)
                    $name = [System.IO.Path]::GetFileName($Uri.AbsolutePath)
                    Copy-Item -Path (Join-Path {json.dumps(str(self.archive.parent))} $name) -Destination $OutFile
                }}

                Main
                """
            ),
            encoding="utf-8",
        )
        return harness

    def test_windows_install(self) -> None:
        command = ["pwsh", "-NoProfile", "-File", str(self.harness())]
        for _ in range(2):
            result = subprocess.run(
                command,
                capture_output=True,
                check=False,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.install_dir / "rift.exe").read_bytes(), self.binary)

    def test_bad_checksum_is_rejected(self) -> None:
        write_manifest(self.archive.parent, self.archive, digest="0" * 64)
        result = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(self.harness())],
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse((self.install_dir / "rift.exe").exists())


if __name__ == "__main__":
    unittest.main()
