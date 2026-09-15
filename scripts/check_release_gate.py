#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["trustme==1.2.1", "mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Install and upgrade real release binaries before a draft can be promoted.

GitHub CLI downloads authenticated draft assets before the gate creates its
loopback HTTPS fixture. The fixture serves those exact checksummed bytes under
the URLs already compiled into historical updaters. TLS remains validated;
only disposable hosted runners add the fixture CA to native OS trust stores.
No GitHub credential reaches the installer, updater, or fixture requests.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import sys
import tempfile
import time
import xml.etree.ElementTree as ET
from collections.abc import Callable, Sequence
from contextlib import ExitStack
from dataclasses import dataclass
from pathlib import Path
from typing import Final, TypeVar

import tomllib
from release_assets import ROOT, ReleaseAssets, previous_tag
from release_fixture import (
    API_HOST,
    LATEST_PATH,
    ReleaseFixture,
    metadata,
    trusted_certificate,
)
from release_process import run
from rift_release.release import binary_name, release_version
from rift_test_client import xml_text

T = TypeVar("T")
STAGE_SECONDS_MAX: Final = 240.0


@dataclass(frozen=True)
class Options:
    """Own the parsed gate inputs, with one explicit mode for candidate publication."""

    tag: str
    from_tag: str
    target: str
    candidate_binary: Path | None
    published: bool
    coldstart: bool
    agent: bool
    mode: str
    shell: list[str] | None
    junit: Path
    evidence: Path


def host_shells() -> tuple[str, ...]:
    """Exercise the documented Bash command from each supported interactive shell."""
    if sys.platform == "win32":
        return ("pwsh",)
    if sys.platform == "darwin":
        return ("bash", "zsh")
    if platform.machine() in ("x86_64", "AMD64"):
        return ("sh", "bash", "dash", "zsh")
    return ("bash",)


def installer_command(shell: str, tag: str) -> list[str]:
    """Keep shell arguments outside command text; install.sh itself requires Bash."""
    if shell == "pwsh":
        return [
            shell,
            "-NoProfile",
            "-NonInteractive",
            "-File",
            str(ROOT / "docs/public/install.ps1"),
            "-Version",
            tag,
        ]
    return [
        shell,
        "-c",
        'exec bash "$1" --version "$2"',
        "rift-install",
        str(ROOT / "docs/public/install.sh"),
        tag,
    ]


def install(
    assets: ReleaseAssets,
    prefix: Path,
    shell: str,
    fixture: ReleaseFixture,
    *,
    deadline: float | None = None,
) -> Path:
    """Run the shipped installer, then compare its output binary to the verified archive."""
    if shutil.which(shell) is None:
        raise RuntimeError(f"release gate requires shell {shell}")
    environment = fixture.environment()
    environment["RIFT_INSTALL_DIR"] = str(prefix)
    before = len(fixture.requests)
    run(
        installer_command(shell, assets.tag),
        environment=environment,
        cwd=prefix.parent,
        deadline=deadline,
    )
    if set(fixture.requests[before:]) != set(assets.responses()):
        raise AssertionError(
            f"{shell} installer did not fetch exactly the verified release assets"
        )
    binary = prefix / binary_name(assets.target)
    assets.verify_installed(binary, environment, deadline=deadline)
    return binary


def smoke(
    binary: Path,
    tag: str,
    environment: dict[str, str],
    *,
    deadline: float | None = None,
) -> None:
    """Use the same real-process artifact suite for built, installed, and updated binaries."""
    run(
        [
            sys.executable,
            str(ROOT / "scripts/check_artifact.py"),
            "--binary",
            str(binary),
            "--version",
            tag,
        ],
        environment=environment,
        timeout=240,
        deadline=deadline,
    )


def upgrade(
    previous: ReleaseAssets,
    candidate: ReleaseAssets,
    directory: Path,
    fixture: ReleaseFixture,
) -> Path:
    """Execute the installed historical updater without patching or rebuilding its bytes."""
    deadline = time.monotonic() + STAGE_SECONDS_MAX
    binary = install(
        previous, directory / "upgrade", host_shells()[0], fixture, deadline=deadline
    )
    before = len(fixture.requests)
    run(
        [str(binary), "update"],
        environment=fixture.environment(),
        cwd=directory,
        timeout=180,
        deadline=deadline,
    )
    expected = set(candidate.responses()) | {(API_HOST, LATEST_PATH)}
    if set(fixture.requests[before:]) != expected:
        raise AssertionError(
            "historical updater did not fetch candidate metadata and verified assets"
        )
    candidate.verify_installed(binary, fixture.environment(), deadline=deadline)
    smoke(binary, candidate.tag, fixture.environment(), deadline=deadline)
    return binary


class Report:
    """Write a JUnit case for every attempted stage, including a failing stage."""

    def __init__(self, path: Path):
        self.path = path
        self.suite = ET.Element("testsuite", name="release-gate")

    def check(self, name: str, operation: Callable[[], T]) -> T:
        started = time.monotonic()
        case = ET.SubElement(
            self.suite, "testcase", name=name, classname="release-gate"
        )
        try:
            return operation()
        except BaseException as error:
            ET.SubElement(
                case, "failure", message=xml_text(str(error))
            ).text = xml_text(repr(error))
            raise
        finally:
            case.set("time", f"{time.monotonic() - started:.3f}")
            self.suite.set("tests", str(len(self.suite)))
            self.suite.set(
                "failures",
                str(sum(case.find("failure") is not None for case in self.suite)),
            )
            self.suite.set("errors", "0")
            self.suite.set("skipped", "0")
            self.suite.set(
                "time", str(sum(float(case.get("time", "0")) for case in self.suite))
            )
            self.path.parent.mkdir(parents=True, exist_ok=True)
            ET.ElementTree(self.suite).write(
                self.path, encoding="utf-8", xml_declaration=True
            )


def prepare(options: Options, directory: Path) -> tuple[ReleaseAssets, ReleaseAssets]:
    """Resolve and verify both releases before serving any download."""
    tag: str = options.tag
    from_tag: str = options.from_tag or previous_tag(tag)
    if tuple(map(int, release_version(from_tag).split("."))) >= tuple(
        map(int, release_version(tag).split("."))
    ):
        raise ValueError("upgrade source must precede the candidate version")
    previous = ReleaseAssets.download(
        directory / "previous", from_tag, options.target, draft=False
    )
    if options.candidate_binary:
        candidate = ReleaseAssets.candidate(
            directory / "candidate",
            tag,
            options.target,
            options.candidate_binary.resolve(),
        )
    else:
        candidate = ReleaseAssets.download(
            directory / "candidate",
            tag,
            options.target,
            draft=not options.published,
        )
    return previous, candidate


def run_gate(options: Options) -> None:
    """Keep one installer/server active at a time, then remove the fixture CA."""
    report = Report(options.junit)
    tag = options.tag
    with tempfile.TemporaryDirectory(prefix="rift-release-gate-") as temporary:
        directory = Path(temporary).resolve()
        previous, candidate = report.check(
            "prepare", lambda: prepare(options, directory)
        )
        responses = (
            previous.responses()
            | candidate.responses()
            | {(API_HOST, LATEST_PATH): metadata(tag)}
        )
        cleanup = ExitStack()

        def start_fixture() -> ReleaseFixture:
            fixture = ReleaseFixture(directory, responses)
            cleanup.callback(fixture.server_close)
            cleanup.enter_context(trusted_certificate(fixture.certificate))
            cleanup.enter_context(fixture.running())
            return fixture

        try:
            fixture = report.check("fixture-setup", start_fixture)
            if options.mode in ("all", "install"):
                for shell in options.shell or host_shells():

                    def install_and_serve(shell: str = shell) -> None:
                        deadline = time.monotonic() + STAGE_SECONDS_MAX
                        binary = install(
                            candidate,
                            directory / f"install-{shell}",
                            shell,
                            fixture,
                            deadline=deadline,
                        )
                        smoke(binary, tag, fixture.environment(), deadline=deadline)

                    report.check(f"install-{shell}", install_and_serve)
            if options.mode in ("all", "upgrade"):
                binary = report.check(
                    "upgrade", lambda: upgrade(previous, candidate, directory, fixture)
                )
                environment = fixture.environment()
                for enabled, suite, seconds in (
                    (options.coldstart, "coldstart", 240.0),
                    (options.agent, "agent", 300.0),
                ):
                    if enabled:
                        command = [
                            sys.executable,
                            str(ROOT / f"scripts/check_{suite}.py"),
                            "--binary",
                            str(binary),
                            "--version",
                            tag,
                        ]
                        report.check(
                            suite,
                            lambda command=command, seconds=seconds: run(
                                command, environment=environment, timeout=seconds
                            ),
                        )
            report.check("fixture-validation", fixture.validate)
        finally:
            report.check("fixture-cleanup", cleanup.close)
        evidence = {
            "tag": tag,
            "from_tag": previous.tag,
            "target": options.target,
            "archive_sha256": hashlib.sha256(candidate.archive).hexdigest(),
            "manifest_sha256": hashlib.sha256(candidate.manifest).hexdigest(),
            "binary_sha256": candidate.binary_sha256,
            "requests": fixture.requests,
        }

        def write_evidence() -> None:
            options.evidence.parent.mkdir(parents=True, exist_ok=True)
            options.evidence.write_text(
                json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
            )

        report.check("evidence", write_evidence)


def main(arguments: Sequence[str] | None = None) -> int:
    """Require explicit candidate bytes or a GitHub release with the expected draft state."""
    parser = argparse.ArgumentParser(description=__doc__)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))[
        "workspace"
    ]["package"]["version"]
    parser.add_argument("--tag", default=f"v{version}")
    parser.add_argument("--from-tag", default="")
    parser.add_argument("--target", required=True)
    parser.add_argument("--candidate-binary", type=Path)
    parser.add_argument("--published", action="store_true")
    parser.add_argument("--coldstart", action="store_true")
    parser.add_argument("--agent", action="store_true")
    parser.add_argument("--mode", choices=("all", "install", "upgrade"), default="all")
    parser.add_argument(
        "--shell", action="append", choices=("sh", "bash", "dash", "zsh", "pwsh")
    )
    parser.add_argument(
        "--junit", type=Path, default=Path("target/release-gate/junit.xml")
    )
    parser.add_argument(
        "--evidence", type=Path, default=Path("target/release-gate/evidence.json")
    )
    options = Options(**vars(parser.parse_args(arguments)))
    run_gate(options)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
