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

    uv run --project dev rift-dev conformance [--binary path/to/rift]
"""

from __future__ import annotations

import json
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from rift_dev.commands import (
    REPOSITORY,
    CargoCommand,
    Command,
    CommandFailed,
    Process,
)

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
# Longest wait for the `rift` binary to build, and the most JSON its build
# messages may take: one line per compiled crate, a few kilobytes each.
BUILD_SECONDS_MAX = 1800.0
BUILD_OUTPUT_BYTES_MAX = 64 * 1024 * 1024

FIXTURE_CONFIGURATION = """[search.vector]
disabled = true
"""
FIXTURE_SOURCE = 'fn main() {\n    println!("beacon");\n}\n'


def lay_out_workspace(root: Path) -> None:
    """Write the throwaway source tree the server indexes.

    The tree carries no cargo manifest. A manifest without a lockfile makes
    `cargo metadata --format-version 1 --locked --offline` refuse, and the
    degraded resolution that follows rebuilds the index while the server is
    still capturing its first tree, so the server exits before it publishes.
    The configuration keeps the vector ranking off, so the run reaches no
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
    command = CargoCommand(
        "build", "--locked", "-p", "rift", "--message-format=json-render-diagnostics"
    ).with_timeout(BUILD_SECONDS_MAX)
    if release:
        command.with_args("--release")
    if target is not None:
        command.with_args("--target", target)
    messages = command.with_output_limit(BUILD_OUTPUT_BYTES_MAX).output()
    for line in messages.splitlines():
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


@contextmanager
def started_server(binary: Path, root: Path, log_path: Path) -> Iterator[Process]:
    """A foreground server over `root` with the token check off, owned until the end.

    Its output goes to `log_path`. On the way out the server gets the interrupt
    it stops on, and whatever outlasts `STOP_SECONDS_MAX` is ended with it.
    """
    command = Command(
        binary, "server", "start", "--foreground", "--auth", "skip"
    ).with_cwd(root)
    with log_path.open("wb") as log, command.spawn(log) as server:
        try:
            yield server
        finally:
            stop_server(server)


def await_published_port(server: Process, root: Path, log_path: Path) -> int:
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


def stop_server(server: Process) -> None:
    """Interrupt the server, the signal a foreground server stops on, within a bound.

    A server still running at the bound is left to `Command.spawn`, which ends
    its whole process group.
    """
    if server.poll() is not None:
        return
    server.interrupt()
    try:
        server.wait(timeout=STOP_SECONDS_MAX)
    except TimeoutError:
        pass


def install_runner() -> None:
    """Install the pinned runner, refusing a lockfile the manifest outgrew."""
    Command("bun", "install", "--frozen-lockfile").with_cwd(
        TOOL_DIRECTORY
    ).with_timeout(INSTALL_SECONDS_MAX).run()


def run_suite(port: int) -> int:
    """Run the suite against the served port, printing the runner's output."""
    runner = Command(
        "bunx",
        "conformance",
        "server",
        "--url",
        f"http://127.0.0.1:{port}/api/mcp",
        "--requirements",
        REQUIREMENTS_REVISION,
        "--expected-failures",
        EXPECTED_FAILURES,
    )
    try:
        runner.with_cwd(TOOL_DIRECTORY).with_timeout(RUNNER_SECONDS_MAX).run()
    except CommandFailed as failure:
        return failure.status
    return 0


def main(supplied: Path | None = None) -> int:
    """Serve one throwaway workspace and score it against the baseline."""
    binary = supplied.resolve() if supplied is not None else build_server_binary()
    if not binary.is_file():
        raise RuntimeError(f"conformance binary does not exist: {binary}")
    install_runner()
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
        with started_server(binary, root, log_path) as server:
            port = await_published_port(server, root, log_path)
            return run_suite(port)
