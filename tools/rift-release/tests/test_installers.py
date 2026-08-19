"""Offline contract tests for downloadable Rift installers."""

from __future__ import annotations

import hashlib
import io
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import textwrap
from dataclasses import dataclass
from pathlib import Path

import pytest
from rift_release import release

REPOSITORY = Path(__file__).parents[3]
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


def write_manifest(
    directory: Path, archive: Path, *, digest: str | None = None
) -> Path:
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
    (repository / "LICENSE.md").write_text("MIT\n", encoding="utf-8")
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


@dataclass(frozen=True)
class BashInstallerFixture:
    """Offline Bash installer fixture."""

    archive: Path
    fake_bin: Path
    install_dir: Path
    log: Path
    binary: bytes

    def run(
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
            command.extend(["--version", version])
        return subprocess.run(
            command,
            env=environment,
            capture_output=True,
            check=False,
            text=True,
        )


@pytest.fixture
def bash_installer(tmp_path: Path) -> BashInstallerFixture:
    binary = b"#!/usr/bin/env bash\nprintf 'fixture-rift %s\\n' \"$*\"\n"
    archive = package_fixture(tmp_path, unix_target(), "rift", binary)
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_curl(fake_bin)
    return BashInstallerFixture(
        archive=archive,
        fake_bin=fake_bin,
        install_dir=tmp_path / "install",
        log=tmp_path / "curl.log",
        binary=binary,
    )


@pytest.mark.skipif(os.name == "nt", reason="Bash installer targets Unix")
class TestBashInstaller:
    def test_explicit_and_latest_install(
        self,
        bash_installer: BashInstallerFixture,
    ) -> None:
        explicit = bash_installer.run()
        assert explicit.returncode == 0, explicit.stderr
        installed = bash_installer.install_dir / "rift"
        assert installed.read_bytes() == bash_installer.binary
        assert os.access(installed, os.X_OK)

        shutil.rmtree(bash_installer.install_dir)
        latest = bash_installer.run(None)
        assert latest.returncode == 0, latest.stderr
        assert installed.is_file()

        requests = [
            json.loads(line) for line in bash_installer.log.read_text().splitlines()
        ]
        assert all("--proto-redir" in request for request in requests)
        assert all("=https" in request for request in requests)

    def test_bad_checksum_is_rejected_without_install(
        self,
        bash_installer: BashInstallerFixture,
    ) -> None:
        write_manifest(
            bash_installer.archive.parent, bash_installer.archive, digest="0" * 64
        )
        result = bash_installer.run()
        assert result.returncode != 0
        assert "checksum mismatch" in result.stderr
        assert not (bash_installer.install_dir / "rift").exists()

    def test_invalid_latest_tag_is_rejected(
        self,
        bash_installer: BashInstallerFixture,
    ) -> None:
        result = bash_installer.run(None, latest="2026-08-18")
        assert result.returncode != 0
        assert "invalid tag" in result.stderr

    def test_non_https_download_is_rejected(
        self,
        bash_installer: BashInstallerFixture,
    ) -> None:
        result = bash_installer.run(download_base="http://fixture.invalid/download")
        assert result.returncode != 0
        assert "non-HTTPS" in result.stderr

    def test_archive_without_exact_binary_is_rejected(
        self,
        bash_installer: BashInstallerFixture,
    ) -> None:
        root = bash_installer.archive.name.removesuffix(".tar.gz")
        data = b"wrong binary"
        with tarfile.open(bash_installer.archive, "w:gz") as archive:
            info = tarfile.TarInfo(f"{root}/not-rift")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        write_manifest(bash_installer.archive.parent, bash_installer.archive)

        result = bash_installer.run()
        assert result.returncode != 0
        assert "unexpected files" in result.stderr
        assert not (bash_installer.install_dir / "rift").exists()


@dataclass(frozen=True)
class PowerShellInstallerFixture:
    """Offline PowerShell installer fixture."""

    archive: Path
    install_dir: Path
    root: Path

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


@pytest.fixture
def powershell_installer(tmp_path: Path) -> PowerShellInstallerFixture:
    archive = package_fixture(
        tmp_path,
        windows_target(),
        "rift.exe",
        b"fixture windows binary",
    )
    return PowerShellInstallerFixture(
        archive=archive,
        install_dir=tmp_path / "install",
        root=tmp_path,
    )


@pytest.mark.skipif(
    os.name != "nt" or shutil.which("pwsh") is None,
    reason="Windows PowerShell is unavailable",
)
class TestPowerShellInstaller:
    def test_windows_install(
        self,
        powershell_installer: PowerShellInstallerFixture,
    ) -> None:
        command = ["pwsh", "-NoProfile", "-File", str(powershell_installer.harness())]
        for _ in range(2):
            result = subprocess.run(
                command, capture_output=True, check=False, text=True
            )
            assert result.returncode == 0, result.stderr
        assert (powershell_installer.install_dir / "rift.exe").read_bytes() == (
            b"fixture windows binary"
        )

    def test_bad_checksum_is_rejected(
        self,
        powershell_installer: PowerShellInstallerFixture,
    ) -> None:
        write_manifest(
            powershell_installer.archive.parent,
            powershell_installer.archive,
            digest="0" * 64,
        )
        result = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(powershell_installer.harness())],
            capture_output=True,
            check=False,
            text=True,
        )
        assert result.returncode != 0
        assert "checksum mismatch" in result.stderr
        assert not (powershell_installer.install_dir / "rift.exe").exists()
