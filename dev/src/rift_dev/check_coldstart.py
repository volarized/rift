"""Prove first use in a Linux container with no workspace configuration or tools.

Only the supplied Linux binary is mounted. The validating MCP SDK runs on the
host and drives `docker exec -i ... rift mcp`, so no Python environment or cache
enters the container. Docker bounds the server log outside the served workspace.
"""

from __future__ import annotations

import json
import os
import time
import uuid
from datetime import timedelta
from pathlib import Path

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.shared.exceptions import McpError

from rift_dev.check_artifact import incoming_references, symbol_hit, symbol_id
from rift_dev.release_process import owned_environment, run
from rift_dev.rift_test_client import (
    Client,
    array_value,
    gate_deadline,
    object_value,
    remaining_seconds,
    require,
    run_command,
    stderr_log,
)

COLDSTART_SECONDS = 240.0
START_SECONDS = 120.0
STOP_SECONDS = 5.0
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
    return run_command(
        ["docker", "exec", "--workdir", "/workspace", name, *arguments],
        timeout_seconds=timeout,
    )


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
    logs = await client.resource("rift://logs")
    require(
        bool(array_value(logs.get("records"), "logs.records")),
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
    except McpError as launch_error:
        detail = object_value(launch_error.error.data, "engine error")
        require(
            detail.get("code") == "capability_unavailable",
            f"unexpected engine error: {detail}",
        )
        require(
            "launch_failed" in str(launch_error),
            f"engine failure lost launch cause: {launch_error}",
        )
    else:
        raise AssertionError("missing configured engine answered references")
    require(
        (await symbol_hit(client, "beacon_cold")).get("source") == SOURCE.rstrip("\n"),
        "engine failure removed syntax reads",
    )
    logs = await client.resource("rift://logs/component/engine")
    require(
        bool(array_value(logs.get("records"), "engine logs")),
        "engine launch failure was not recorded",
    )


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
        command = [
            "docker",
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
        ]
        failure: BaseException | None = None
        try:
            run_command(command, timeout_seconds=START_SECONDS)
            pid = await_publication(name)
            if version is not None:
                observed = container_command(name, ["/rift", "--version"]).strip()
                require(
                    observed == f"rift {version.removeprefix('v')}",
                    f"unexpected version: {observed}",
                )
            with (
                stderr_log() as log,
                owned_environment(dict(os.environ)) as environment,
            ):
                parameters = StdioServerParameters(
                    command="docker",
                    env=environment,
                    args=[
                        "exec",
                        "-i",
                        "--workdir",
                        "/workspace",
                        name,
                        "/rift",
                        "mcp",
                    ],
                )
                async with (
                    stdio_client(parameters, errlog=log) as (read, write),
                    ClientSession(
                        read, write, timedelta(seconds=START_SECONDS)
                    ) as session,
                ):
                    client = Client(session, START_SECONDS)
                    await client.initialize()
                    await check_first_use(client, name)
                    await check_missing_executable(client, name)
            stop_container(name, pid)
        except BaseException as error:
            failure = error
            try:
                error.add_note(
                    run(["docker", "logs", "--tail", "200", name], timeout=10)
                )
            except (RuntimeError, OSError) as log_error:
                error.add_note(f"container log collection failed: {log_error}")
            raise
        finally:
            try:
                run(["docker", "rm", "--force", name], timeout=30)
            except (RuntimeError, OSError) as cleanup_error:
                if failure is None:
                    raise
                failure.add_note(f"container cleanup failed: {cleanup_error}")
