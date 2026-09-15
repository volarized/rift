#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Run the MCP conformance suite against a live rift server.

The runner is `@modelcontextprotocol/conformance`, pinned in
`tools/mcp-conformance`. It drives one scenario set over Streamable HTTP and
validates every message the server sent against the specification's own JSON
schema for the negotiated revision.

The revision is 2025-11-25 because that is the one the served surface
negotiates: rmcp 3.1.3 declares `ProtocolVersion::LATEST = V_2025_11_25`,
and this repository pins no revision of its own.

The runner addresses a server by URL alone and sends no `Authorization`
header, so the server this script starts runs under `--auth skip`, which the
CLI accepts only together with `--foreground`. The served workspace is a
throwaway source tree in a temporary directory, so the run indexes nothing
of this repository and leaves nothing behind.

Usage:

    uv run --script scripts/check_mcp_conformance.py
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
TOOL_DIRECTORY = REPOSITORY / "tools" / "mcp-conformance"
EXPECTED_FAILURES = TOOL_DIRECTORY / "expected-failures.yml"

# The specification revision the served surface negotiates.
REQUIREMENTS_REVISION = "2025-11-25"

# Longest wait for the started server to publish `.rift/server.json`. The
# span covers a `cargo run` that still has to build the binary.
PUBLISH_SECONDS_MAX = 600.0
# Span between two reads of the published document.
PUBLISH_POLL_SECONDS = 0.1
# Longest wait for the stopped server to leave.
STOP_SECONDS_MAX = 30.0
# Longest wait for `bun install` and for one runner invocation.
INSTALL_SECONDS_MAX = 300.0
RUNNER_SECONDS_MAX = 300.0
# Longest wait for the `rift` binary to build.
BUILD_SECONDS_MAX = 1800.0

FIXTURE_CONFIGURATION = """[search.semantic]
disabled = true
"""
FIXTURE_SOURCE = 'fn main() {\n    println!("beacon");\n}\n'


def lay_out_workspace(root: Path) -> None:
    """Write the throwaway source tree the server indexes.

    The tree carries no cargo manifest. A manifest without a lockfile makes
    `cargo metadata --format-version 1 --locked --offline` refuse, and the
    degraded resolution that follows rebuilds the index while the server is
    still capturing its first tree, so the server exits before it publishes.
    The configuration keeps the semantic tier off, so the run reaches no
    model hub.
    """
    (root / "src").mkdir(parents=True, exist_ok=True)
    (root / "rift.toml").write_text(FIXTURE_CONFIGURATION, encoding="utf-8")
    (root / "src" / "main.rs").write_text(FIXTURE_SOURCE, encoding="utf-8")


def build_server_binary(*, release: bool = False, target: str | None = None) -> Path:
    """Build `rift` and answer the executable Cargo wrote.

    The build runs in this repository, where `rust-toolchain.toml` selects
    the compiler; the server itself then runs with the served workspace as
    its working directory, which is the root `rift server` serves.
    """
    command = [
        "cargo",
        "build",
        "--locked",
        "-p",
        "rift",
        "--message-format=json-render-diagnostics",
    ]
    if release:
        command.append("--release")
    if target is not None:
        command.extend(["--target", target])
    completed = subprocess.run(
        command,
        cwd=REPOSITORY,
        check=True,
        capture_output=True,
        text=True,
        timeout=BUILD_SECONDS_MAX,
    )
    for line in completed.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("executable")
            and message.get("target", {}).get("name") == "rift"
        ):
            return Path(message["executable"])
    raise RuntimeError("the build reported no rift executable")


def start_server(binary: Path, root: Path, log_path: Path) -> subprocess.Popen[bytes]:
    """Start a foreground server over `root` with the token check off.

    The child leads its own process group, so the stop reaches it whatever
    the run did.
    """
    command = [
        str(binary),
        "server",
        "start",
        "--foreground",
        "--auth",
        "skip",
    ]
    log = log_path.open("wb")
    return subprocess.Popen(
        command,
        cwd=root,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )


def await_published_port(
    server: subprocess.Popen[bytes], root: Path, log_path: Path
) -> int:
    """The port the started server published, waiting up to the bound.

    Raises when the server exits first or the bound passes; either way the
    server's own output is named.
    """
    document = root / ".rift" / "server.json"
    deadline = time.monotonic() + PUBLISH_SECONDS_MAX
    while time.monotonic() < deadline:
        if document.is_file():
            try:
                return int(json.loads(document.read_text(encoding="utf-8"))["port"])
            except (ValueError, KeyError):
                # The document is written through a rename, so a partial read
                # is not expected; retry once the next poll comes round.
                pass
        if server.poll() is not None:
            raise RuntimeError(
                f"the server exited {server.returncode} before publishing:\n"
                f"{read_log(log_path)}"
            )
        time.sleep(PUBLISH_POLL_SECONDS)
    raise RuntimeError(
        f"the server published nothing within {PUBLISH_SECONDS_MAX:.0f}s:\n"
        f"{read_log(log_path)}"
    )


def read_log(log_path: Path) -> str:
    """The server's recorded output, or a note that there is none."""
    if not log_path.is_file():
        return "(the server wrote no output)"
    return log_path.read_text(encoding="utf-8", errors="replace")


def stop_server(server: subprocess.Popen[bytes]) -> None:
    """End the server's process group, bounded, whatever the run did.

    The interrupt is what a foreground server stops on, so it is sent first;
    a group that outlasts the bound is ended outright.
    """
    if server.poll() is not None:
        return
    end_group(server, signal.SIGINT)
    try:
        server.wait(timeout=STOP_SECONDS_MAX)
        return
    except subprocess.TimeoutExpired:
        pass
    end_group(server, getattr(signal, "SIGKILL", signal.SIGTERM))
    try:
        server.wait(timeout=STOP_SECONDS_MAX)
    except subprocess.TimeoutExpired:
        print("warning: the server outlasted its stop", file=sys.stderr)


def end_group(server: subprocess.Popen[bytes], number: int) -> None:
    """Signal the server's whole process group, falling back to the child."""
    try:
        os.killpg(os.getpgid(server.pid), number)
    except (AttributeError, OSError):
        if number == getattr(signal, "SIGKILL", None):
            server.kill()
        else:
            server.terminate()


def install_runner() -> None:
    """Install the pinned runner, refusing a lockfile the manifest outgrew."""
    subprocess.run(
        ["bun", "install", "--frozen-lockfile"],
        cwd=TOOL_DIRECTORY,
        check=True,
        timeout=INSTALL_SECONDS_MAX,
    )


def run_suite(port: int) -> int:
    """Run the suite against the served port, printing the runner's output."""
    completed = subprocess.run(
        [
            "bunx",
            "conformance",
            "server",
            "--url",
            f"http://127.0.0.1:{port}/api/mcp",
            "--requirements",
            REQUIREMENTS_REVISION,
            "--expected-failures",
            str(EXPECTED_FAILURES),
        ],
        cwd=TOOL_DIRECTORY,
        check=False,
        timeout=RUNNER_SECONDS_MAX,
    )
    return completed.returncode


def main() -> int:
    """Serve one throwaway workspace and score it against the baseline."""
    install_runner()
    binary = build_server_binary()
    with (
        tempfile.TemporaryDirectory(prefix="rift-conformance-") as directory,
        tempfile.TemporaryDirectory(prefix="rift-conformance-log-") as log_directory,
    ):
        root = Path(directory)
        lay_out_workspace(root)
        # The server watches the tree it serves, so its own output stays
        # outside that tree. A log written beside `src` moves the workspace
        # fingerprint on every line, and the startup capture never settles:
        # the server refuses to start, naming `server.log` as the file that
        # moved between two scans.
        log_path = Path(log_directory) / "server.log"
        server = start_server(binary, root, log_path)
        try:
            port = await_published_port(server, root, log_path)
            return run_suite(port)
        finally:
            stop_server(server)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.SubprocessError) as failure:
        print(f"error: {failure}", file=sys.stderr)
        raise SystemExit(1) from failure
