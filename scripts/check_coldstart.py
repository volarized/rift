#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove first use in a Linux container with no workspace configuration or tools.

Only the supplied Linux binary is mounted. The validating MCP SDK runs on the
host and drives `docker exec -i ... rift mcp`, so no Python environment or cache
enters the container. Docker bounds the server log outside the served workspace.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
import uuid
from datetime import timedelta
from pathlib import Path

from check_artifact import symbol_hit, symbol_id
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.shared.exceptions import McpError
from release_process import owned_environment, run
from rift_test_client import (
    Client,
    array_value,
    candidate_binary,
    gate_deadline,
    object_value,
    remaining_seconds,
    require,
    run_command,
    run_gate,
    stderr_log,
    workspace_version,
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


async def check_first_use(client: Client, name: str) -> None:
    """Start empty, index a later file, and keep reads after an absent engine refuses."""
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
    refused = await client.call(
        "rename_symbol", {"symbol": symbol_id(hit), "new_name": "beacon_renamed"}
    )
    require(
        refused.get("status") == "refused", f"missing engine was not refused: {refused}"
    )
    require(
        "no engine configured for language rust" in json.dumps(refused),
        f"missing engine refusal omitted the language: {refused}",
    )
    await symbol_hit(client, "beacon_cold")
    logs = await client.resource("rift://logs")
    require(
        bool(array_value(logs.get("records"), "logs.records")),
        "cold startup recorded no logs",
    )
    await check_missing_executable(client, name)


async def check_missing_executable(client: Client, name: str) -> None:
    """After first use, configure an absent engine and require its recorded failure."""
    configuration = '[languages.rust.lsp]\ncommand = ["rust-analyzer"]\n'
    container_command(
        name, ["sh", "-c", 'printf %s "$1" > rift.toml', "sh", configuration]
    )
    hit = await symbol_hit(client, "beacon_cold")
    try:
        await client.call(
            "rename_symbol", {"symbol": symbol_id(hit), "new_name": "beacon_renamed"}
        )
    except McpError as launch_error:
        detail = object_value(launch_error.error.data, "engine error")
        require(
            detail.get("code") == "capability_unavailable",
            f"unexpected engine error: {detail}",
        )
        require(
            "launch_failed" in str(launch_error),
            f"unexpected engine failure: {launch_error}",
        )
    else:
        raise AssertionError("missing executable returned success")
    await symbol_hit(client, "beacon_cold")
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        logs = await client.resource("rift://logs")
        records = array_value(logs.get("records"), "logs.records")
        failure = [
            record
            for record in records
            if "language engine restart budget is spent" in json.dumps(record)
        ]
        if failure:
            require(
                container_command(name, ["cat", "lib.rs"]) == SOURCE,
                "missing engine changed source bytes",
            )
            require(
                "rust" in json.dumps(failure),
                f"engine diagnostic omitted its language: {failure}",
            )
            return
        await asyncio.sleep(0.1)
    raise AssertionError(f"logs omitted the failed engine launch: {logs}")


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
            stop = '/rift server stop && ! kill -0 "$1" 2>/dev/null'
            container_command(
                name, ["sh", "-c", stop, "sh", str(pid)], timeout=STOP_SECONDS
            )
            status = container_command(name, ["cat", "/server.exit"]).strip()
            require(status == "0", f"cold server exited {status}")
        except BaseException as error:
            failure = error
            try:
                print(run(["docker", "logs", "--tail", "200", name], timeout=10))
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--image", default="ubuntu:24.04")
    parser.add_argument("--version")
    parser.add_argument("--junit", type=Path)
    args = parser.parse_args()
    if args.binary is None and args.target is None and sys.platform != "linux":
        parser.error(
            "cold start requires --binary pointing to a Linux executable, or --target naming a Linux target with a configured cross compiler"
        )
    binary = candidate_binary(args.binary, args.target)
    version = args.version or workspace_version()
    run_gate("coldstart", check_coldstart(binary, args.image, version), args.junit)
    print("coldstart: empty workspace, later file, missing engine, and stop passed")


if __name__ == "__main__":
    main()
