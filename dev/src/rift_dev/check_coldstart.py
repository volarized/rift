"""Prove first use in a Linux container with no workspace configuration or tools.

Only the supplied Linux binary is mounted. The validating MCP SDK runs on the
host and drives `docker exec -i ... rift mcp`, so no Python environment or cache
enters the container. Docker bounds the server log outside the served workspace.
It runs without the OTLP collector: the container runs with `--network none`, so no
host endpoint is reachable.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
import uuid
from collections.abc import Sequence
from pathlib import Path

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

from rift_dev.check_artifact import incoming_references, symbol_hit, symbol_id
from rift_dev.commands import DockerCommand, owned_environment
from rift_dev.log_records import parse_line
from rift_dev.rift_test_client import (
    LOG_BYTES_MAX,
    LOG_FILTER,
    POLL_SECONDS,
    RECORD_TAIL,
    Client,
    ToolFailure,
    array_value,
    current_deadline,
    failure_window,
    gate_deadline,
    object_value,
    remaining_seconds,
    require,
    retained_directory,
    stderr_log,
    tail_text,
    utc_now,
)

COLDSTART_SECONDS = 240.0
START_SECONDS = 120.0
STOP_SECONDS = 5.0
EVIDENCE_SECONDS = 10.0
CONTAINER_LOG_LINES = 200
SOURCE = "pub fn beacon_cold() -> u8 { 7 }\n"
START = r"""
set -eu
for program in cargo rustup bun uv node rust-analyzer; do
    if command -v "$program" >/dev/null 2>&1; then
        echo "unexpected installed tool: $program" >&2
        exit 1
    fi
done
for path in /root/.rift /root/.cargo /root/.rustup /root/.cache /rift.toml; do
    test ! -e "$path"
done
test -z "$(find /workspace -mindepth 1 -print -quit)"
test -z "$(find /root /workspace /etc /usr -name rift.toml -print -quit 2>/dev/null)"
/rift server start --foreground --auth skip &
server_pid=$!
set +e
wait "$server_pid"
server_status=$?
printf '%s\n' "$server_status" > /server.exit
exec sleep infinity
"""
STOP = r"""
set -eu
"$1" server stop >&2
while kill -0 "$2" 2>/dev/null || test ! -s "$3"; do
    sleep 0.01
done
cat "$3"
"""


def container_command(name: str, arguments: list[str], timeout: float = 30.0) -> str:
    """Run one command inside the isolated container with bounded output."""
    return (
        DockerCommand("exec", "--workdir", "/workspace", name, *arguments)
        .with_timeout(timeout)
        .with_deadline(current_deadline())
        .output()
    )


async def container_logs(name: str, arguments: list[str]) -> str:
    """Observe persisted records within the existing evidence deadline."""
    try:
        async with gate_deadline("cold persisted diagnostics", EVIDENCE_SECONDS):
            while True:
                text = await asyncio.to_thread(
                    container_command, name, arguments, EVIDENCE_SECONDS
                )
                if any(parse_line(line) is not None for line in text.splitlines()):
                    return text
                await asyncio.sleep(POLL_SECONDS)
    except TimeoutError as error:
        raise TimeoutError(
            f"cold persisted diagnostics exceeded its {EVIDENCE_SECONDS}s deadline: "
            f"{arguments}"
        ) from error


def await_publication(name: str) -> int:
    """Read the server document under a startup deadline, failing on process exit."""
    timeout_seconds = remaining_seconds(START_SECONDS)
    deadline = time.monotonic() + timeout_seconds
    probe = "if test -f /server.exit; then exit 1; fi; if test -f .rift/server.json; then cat .rift/server.json; fi"
    while time.monotonic() < deadline:
        encoded = container_command(name, ["sh", "-c", probe], timeout=5)
        if encoded:
            document = object_value(json.loads(encoded), "server.json")
            pid = document.get("pid")
            require(type(pid) is int and pid > 1, f"invalid server pid: {document}")
            return int(str(pid))
        time.sleep(0.1)
    raise AssertionError(f"cold server did not publish within {timeout_seconds}s")


def stop_container(name: str, pid: int) -> None:
    """Require CLI stop, process exit, and exit status within the same deadline."""
    deadline = time.monotonic() + STOP_SECONDS
    budget = remaining_seconds(max(0.0, deadline - time.monotonic()))
    require(budget > 0, "cold server stop exceeded its deadline")
    status = container_command(
        name,
        ["sh", "-c", STOP, "sh", "/rift", str(pid), "/server.exit"],
        timeout=budget,
    ).strip()
    require(status == "0", f"cold server exited {status}")
    require(time.monotonic() <= deadline, "cold server stop exceeded its deadline")


async def check_first_use(client: Client, name: str) -> None:
    """Start empty and index a later file."""
    workspace = await client.resource("rift://workspace")
    require(
        array_value(workspace.get("source"), "workspace.source") == [],
        f"cold workspace was not empty: {workspace}",
    )
    container_command(name, ["sh", "-c", 'printf %s "$1" > lib.rs', "sh", SOURCE])
    hit = await symbol_hit(client, "beacon_cold")
    require(
        hit.get("source") == SOURCE.rstrip("\n"), f"late file was not indexed: {hit}"
    )
    logs = await container_logs(
        name, ["/rift", "server", "logs", "--tail", str(RECORD_TAIL)]
    )
    require(
        any(parse_line(line) is not None for line in logs.splitlines()),
        "cold startup recorded no logs",
    )


async def check_missing_executable(client: Client, name: str) -> None:
    """Keep syntax reads after a configured language engine fails to launch."""
    configuration = '[languages.rust.lsp]\ncommand = ["rust-analyzer"]\n'
    container_command(
        name, ["sh", "-c", 'printf %s "$1" > rift.toml', "sh", configuration]
    )
    hit = await symbol_hit(client, "beacon_cold")
    try:
        await incoming_references(client, symbol_id(hit))
    except ToolFailure as launch_error:
        require(
            launch_error.code == "capability_unavailable",
            f"unexpected engine error: {launch_error}",
        )
        require(
            bool(launch_error.causes),
            f"engine failure lost launch cause: {launch_error}",
        )
        require(
            any(
                "No such file or directory" in cause.message
                for cause in launch_error.causes
            ),
            f"engine failure lost launch source: {launch_error}",
        )
    else:
        raise AssertionError("missing configured engine answered references")
    require(
        (await symbol_hit(client, "beacon_cold")).get("source") == SOURCE.rstrip("\n"),
        "engine failure removed syntax reads",
    )
    logs = await container_logs(
        name,
        [
            "/rift",
            "server",
            "logs",
            "--tail",
            str(RECORD_TAIL),
            "--component",
            "engine",
        ],
    )
    require(
        any(parse_line(line) is not None for line in logs.splitlines()),
        "engine launch failure was not recorded",
    )


def container_evidence(name: str, proxy_log: Path, started: str) -> list[str]:
    """What a cold failure keeps: server stderr, proxy stderr, and the persisted records.

    The container still exists, so Docker returns the server's last
    `CONTAINER_LOG_LINES` lines of stderr and the container's own `rift server
    logs` reads `.rift/metrics`. Each read has its own bound and no gate
    deadline, because a timed-out gate is the failure this explains; a read that
    fails is reported as text and never replaces the failure. The failure window
    runs from `started`, read just before the container was created, until now; it is
    printed only, since the container and its files go with the case.
    """
    notes: list[str] = []
    reads = [
        (
            f"server stderr (last {CONTAINER_LOG_LINES} lines)",
            DockerCommand("logs", "--tail", str(CONTAINER_LOG_LINES), name),
        ),
        (
            "persisted log records",
            DockerCommand(
                "exec",
                "--workdir",
                "/workspace",
                name,
                "/rift",
                "server",
                "logs",
                "--tail",
                str(RECORD_TAIL),
            ),
        ),
    ]
    for label, command in reads:
        try:
            text = (
                command.with_timeout(EVIDENCE_SECONDS)
                .with_output_limit(LOG_BYTES_MAX)
                .output()
            )
        except (RuntimeError, OSError, ValueError) as error:
            notes.append(f"{label} unavailable: {error}")
        else:
            notes.append(f"{label}:\n{tail_text(text)}")

    try:
        proxy = proxy_log.read_bytes().decode("utf-8", errors="replace")
    except OSError as error:
        notes.append(f"rift mcp stderr unavailable: {error}")
    else:
        notes.append(f"rift mcp stderr ({proxy_log}):\n{tail_text(proxy, proxy_log)}")

    def read(arguments: Sequence[str]) -> str:
        return (
            DockerCommand("exec", "--workdir", "/workspace", name, "/rift", *arguments)
            .with_timeout(EVIDENCE_SECONDS)
            .with_output_limit(LOG_BYTES_MAX)
            .output()
        )

    notes.extend(
        failure_window(
            read,
            since=started,
            lower_bound="read just before the container was created",
            until=utc_now(),
        )
    )
    return notes


async def check_coldstart(binary: Path, image: str, version: str | None = None) -> None:
    """Run supplied bytes under Docker and remove the container on every outcome."""
    async with gate_deadline("coldstart", COLDSTART_SECONDS):
        require(binary.is_file(), f"Linux binary does not exist: {binary}")
        with binary.open("rb") as executable:
            require(
                executable.read(4) == b"\x7fELF",
                "cold start requires a Linux ELF executable",
            )
        name = f"rift-cold-{uuid.uuid4().hex}"
        command = DockerCommand(
            "run",
            "--detach",
            "--name",
            name,
            "--network",
            "none",
            "--memory",
            "3g",
            "--cpus",
            "2",
            "--pids-limit",
            "128",
            "--env",
            f"RUST_LOG={LOG_FILTER}",
            "--log-opt",
            "max-size=8m",
            "--log-opt",
            "max-file=1",
            "--mount",
            f"type=bind,source={binary.resolve()},target=/rift,readonly",
            "--workdir",
            "/workspace",
            image,
            "sh",
            "-c",
            START,
        )
        failure: BaseException | None = None
        proxy_log = retained_directory("coldstart") / "proxy.log"
        started = utc_now()
        try:
            command.with_timeout(START_SECONDS).with_deadline(
                current_deadline()
            ).output()
            pid = await_publication(name)
            if version is not None:
                observed = container_command(name, ["/rift", "--version"]).strip()
                require(
                    observed == f"rift {version.removeprefix('v')}",
                    f"unexpected version: {observed}",
                )
            host = {
                key: value for key, value in os.environ.items() if key != "RUST_LOG"
            }
            with (
                stderr_log(proxy_log) as log,
                owned_environment(host) as environment,
            ):
                parameters = StdioServerParameters(
                    command="docker",
                    env=environment,
                    args=[
                        "exec",
                        "-i",
                        "--env",
                        f"RUST_LOG={LOG_FILTER}",
                        "--workdir",
                        "/workspace",
                        name,
                        "/rift",
                        "mcp",
                        "--output",
                        "all",
                    ],
                )
                async with (
                    stdio_client(parameters, errlog=log) as (read, write),
                    ClientSession(read, write, START_SECONDS) as session,
                ):
                    client = Client(session, START_SECONDS)
                    await client.initialize()
                    await check_first_use(client, name)
                    await check_missing_executable(client, name)
            stop_container(name, pid)
        except BaseException as error:
            failure = error
            for note in container_evidence(name, proxy_log, started):
                error.add_note(note)
            raise
        finally:
            try:
                DockerCommand("rm", "--force", name).with_timeout(30).output()
            except (RuntimeError, OSError) as cleanup_error:
                if failure is None:
                    raise
                failure.add_note(f"container cleanup failed: {cleanup_error}")
                failure.add_note(f"container cleanup failed: {cleanup_error}")
