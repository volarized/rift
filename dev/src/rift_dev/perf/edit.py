"""Measure one-file edits through a supplied Rift binary on a pinned corpus."""

from __future__ import annotations

import asyncio
import json
import platform
import shutil
import sqlite3
import tempfile
import time
from collections import deque
from contextlib import ExitStack, closing
from dataclasses import asdict
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal, Self
from urllib.parse import quote

import psutil
from pydantic import BaseModel, ConfigDict, Field, model_validator

from rift_dev.commands import REPOSITORY, Command, Process
from rift_dev.corpus_cache import Pin, git, pins
from rift_dev.log_records import Line, lines
from rift_dev.machine import machine
from rift_dev.perf.stages import parse_stages, render_table, stage_table
from rift_dev.rift_test_client import (
    Client,
    Server,
    ToolFailure,
    array_value,
    object_value,
    string_value,
)

Corpus = Literal["bun", "fastapi"]
PATHS = {"bun": "bench/test/isolate/setup.ts", "fastapi": "fastapi/exceptions.py"}
LOG_FILTER = "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info,rift_analysis=info,rift_history=info,rift_search=info"
CONFIGURATION = f'''[server]
readiness_timeout = "600s"
[search.vector]
disabled = true
[logs]
capture = "{LOG_FILTER}"
stderr_limit = "64mb"
'''
LOG_READ_BYTES_MAX = 64 * 1024 * 1024


class EditOptions(BaseModel):
    """Validated measurement bounds; the binary is always supplied, never rebuilt."""

    model_config = ConfigDict(extra="forbid", frozen=True)
    corpus: Corpus
    binary: Path
    output: Path
    binary_commit: str | None = Field(default=None, pattern=r"^[0-9a-f]{40}$")
    steps: int = Field(default=3, ge=3, le=64)
    timeout_seconds: float = Field(default=120, gt=0, le=1200)
    startup_seconds: float = Field(default=900, gt=0, le=1200)
    run_seconds: float = Field(default=1800, gt=0, le=7200)
    sample_seconds: int = Field(default=30, ge=0, le=300)
    pre_sample_seconds: float = Field(default=4, gt=0, le=30)

    @model_validator(mode="after")
    def sample_covers_write(self) -> Self:
        if self.sample_seconds and self.sample_seconds <= self.pre_sample_seconds:
            raise ValueError("sample duration must exceed PRE_SAMPLE_SECONDS")
        return self

    def sample_command(self, pid: int) -> tuple[Command | None, Path, str]:
        """Select an optional platform profiler; timing uses the same path everywhere."""
        path = self.output.with_suffix(".sample.txt")
        if self.sample_seconds == 0:
            return None, path, "disabled"
        if platform.system() == "Darwin" and (program := shutil.which("sample")):
            command = Command(
                program, str(pid), str(self.sample_seconds), "1", "-file", path
            )
            return command, path, "sample"
        if (
            platform.system() == "Linux"
            and (program := shutil.which("perf"))
            and shutil.which("sleep")
        ):
            path = self.output.with_suffix(".perf.data")
            command = Command(
                program,
                "record",
                "-F",
                "99",
                "--call-graph",
                "dwarf,16384",
                "-p",
                str(pid),
                "-o",
                path,
                "--",
                "sleep",
                str(self.sample_seconds),
            )
            return command, path, "perf record"
        return None, path, "unavailable: no supported platform profiler found"


def host() -> dict[str, Any]:
    """Record load and capacity beside process CPU; no host names or environment dump."""
    memory = psutil.virtual_memory()
    return {
        "at": datetime.now(UTC).isoformat(),
        "load": list(psutil.getloadavg()),
        "cpu_percent": psutil.cpu_percent(interval=None),
        "available_memory_bytes": memory.available,
        "memory_percent": memory.percent,
        "swap_used_bytes": psutil.swap_memory().used,
    }


def log_since(path: Path, offset: int) -> str:
    with path.open("rb") as stream:
        stream.seek(offset)
        content = stream.read(LOG_READ_BYTES_MAX + 1)
    if len(content) > LOG_READ_BYTES_MAX:
        raise RuntimeError(
            "measurement log exceeded 64 MiB; reduce steps or log capture"
        )
    return content.decode("utf-8", errors="replace")


async def wait_publication(
    server: Server, offset: int, trigger: str
) -> tuple[int, Line]:
    """Observe a publication under the caller's deadline, not the transport's readiness."""
    cursor = offset
    partial = b""
    while True:
        server.check_running()
        try:
            with server.log_path.open("rb") as stream:
                stream.seek(cursor)
                chunk = stream.read(LOG_READ_BYTES_MAX + 1)
                cursor = stream.tell()
        except FileNotFoundError:
            chunk = b""
        if len(chunk) > LOG_READ_BYTES_MAX:
            raise RuntimeError("publication log exceeded 64 MiB")
        partial += chunk
        complete, separator, remainder = partial.rpartition(b"\n")
        if separator:
            partial = remainder
            for row in lines(complete.decode("utf-8", errors="replace")):
                if (
                    row.is_message("index snapshot published")
                    and row.label("trigger") == trigger
                ):
                    return time.perf_counter_ns(), row
        await asyncio.sleep(0.005)


def lexical_state(root: Path, marker: str, path: str) -> tuple[str, str] | None:
    """Observe the real FTS row and revision through a read-only connection."""
    database_path = (root / ".rift/index").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(database_path, uri=True, timeout=0.1)) as database:
        rows = database.execute(
            "SELECT d.identity FROM lexical_documents_fts JOIN lexical_documents d "
            "ON d.id=lexical_documents_fts.rowid WHERE lexical_documents_fts MATCH ? AND d.path=? AND d.name=?",
            ('"' + marker + '"', path, marker),
        ).fetchall()
        revision = database.execute(
            "SELECT tree_revision FROM lexical_index_state"
        ).fetchone()
    if len(rows) == 1 and revision:
        return rows[0][0], revision[0]
    return None


async def wait_lexical(
    server: Server, marker: str, path: str
) -> tuple[int, tuple[str, str]]:
    while True:
        server.check_running()
        try:
            state = lexical_state(server.root, marker, path)
        except sqlite3.OperationalError as error:
            if "locked" not in str(error):
                raise
            state = None
        if state is not None:
            return time.perf_counter_ns(), state
        await asyncio.sleep(0.01)


def declaration(corpus: Corpus, step: int) -> tuple[str, bytes, str]:
    marker = f"lane_incr_probe_{step}"
    body = (
        f"\ndef {marker}():\n    return {step}\n"
        if corpus == "fastapi"
        else f"\nexport function {marker}() {{ return {step}; }}\n"
    )
    prefix = (
        "rift://symbol/local/python/fastapi/exceptions/"
        if corpus == "fastapi"
        else "rift://symbol/local/typescript/"
        + quote("./" + PATHS[corpus], safe="._-!$&'()*+,;=:@")
        + "/"
    )
    return marker, body.encode(), prefix + marker


async def read_edit(
    client: Client, symbol: str, marker: str, step: int
) -> dict[str, Any]:
    attempts: deque[str] = deque(maxlen=64)
    attempt_count = 0
    while True:
        try:
            result = await client.call("facets", {"symbol": symbol, "max_results": 10})
            break
        except ToolFailure as failure:
            attempts.append(failure.text)
            attempt_count += 1
            if failure.retry != "same_request":
                raise
            await asyncio.sleep(0.01)
    declarations = array_value(result["declarations"], "facets declarations")
    sources = [
        object_value(object_value(row, "declaration")["source"], "source")
        for row in declarations
    ]
    texts = [string_value(source["text"], "source text") for source in sources]
    if not any(marker in text and f"return {step}" in text for text in texts):
        raise AssertionError(f"source read missed {marker}: {result}")
    return {
        "revision": result["revision"],
        "warnings": result.get("warnings", []),
        "attempts": list(attempts),
        "retry_count": attempt_count,
    }


class EditRun:
    """Own the disposable checkout, report, profiler and measured server."""

    def __init__(self, options: EditOptions, pin: Pin, root: Path) -> None:
        self.options = options
        self.pin = pin
        self.root = root
        self.path = root / PATHS[options.corpus]
        self.server = Server(
            options.binary,
            root,
            options.output.with_suffix(".server.log"),
            startup_seconds=options.startup_seconds,
            env={"RUST_LOG": LOG_FILTER, "NO_COLOR": "1"},
        )
        self.report: dict[str, Any] = {
            "schema_version": 1,
            "corpus": options.corpus,
            "corpus_commit": pin.commit,
            "corpus_repository": pin.repository,
            "corpus_measurement": asdict(pin.measurement),
            "source_commit": git(REPOSITORY, "rev-parse", "HEAD").output().strip(),
            "version": Command(options.binary, "--version")
            .with_timeout(10)
            .output()
            .strip(),
            "binary": str(options.binary),
            "binary_commit": options.binary_commit,
            "binary_stat": {
                "size": options.binary.stat().st_size,
                "mtime_ns": options.binary.stat().st_mtime_ns,
            },
            "machine": machine(),
            "options": options.model_dump(mode="json"),
            "path": PATHS[options.corpus],
            "log": str(self.server.log_path),
            "host_before": host(),
            "sampling": {"status": "pending"},
            "edits": [],
        }

    def save(self) -> None:
        self.options.output.write_text(
            json.dumps(self.report, indent=2, default=lambda value: value.isoformat())
            + "\n",
            encoding="utf-8",
        )

    async def measure_edit(self, client: Client, original: bytes, step: int) -> None:
        options = self.options
        marker, body, symbol = declaration(options.corpus, step)
        offset = self.server.log_path.stat().st_size
        process = psutil.Process(self.server.process.pid)
        before_host = host()
        cpu_before = process.cpu_times()
        rss_before = process.memory_info().rss
        written = datetime.now(UTC)
        started = time.perf_counter_ns()
        self.path.write_bytes(original + body)
        self.report["pending_edit"] = {
            "step": step,
            "write": written.isoformat(),
            "offset": offset,
        }
        self.save()
        # Observers run beside the read, so read latency cannot delay their start.
        async with asyncio.TaskGroup() as tasks:
            publication = tasks.create_task(
                wait_publication(self.server, offset, "filesystem")
            )
            lexical = tasks.create_task(
                wait_lexical(self.server, marker, PATHS[options.corpus])
            )
            result = await read_edit(client, symbol, marker, step)
            read = time.perf_counter_ns()
        ended = time.perf_counter_ns()
        cpu_after = process.cpu_times()
        output = log_since(self.server.log_path, offset)
        publication_observed, published = publication.result()
        publications = [
            row
            for row in lines(output)
            if row.is_message("index snapshot published")
            and row.label("trigger") == "filesystem"
        ]
        stages = parse_stages(output)
        lexical_time, lexical_row = lexical.result()
        edit = {
            "step": step,
            "marker": marker,
            "symbol": symbol,
            "write": written.isoformat(),
            "read_seconds": (read - started) / 1e9,
            "publication_seconds": (published.time - written).total_seconds(),
            "publication_observed_seconds": (publication_observed - started) / 1e9,
            "final_publication_seconds": (
                publications[-1].time - written
            ).total_seconds(),
            "publication_times": [
                (row.time - written).total_seconds() for row in publications
            ],
            "lexical_observed_seconds": (lexical_time - started) / 1e9,
            "lexical_identity": lexical_row[0],
            "lexical_revision": lexical_row[1],
            "cpu_seconds": cpu_after.user
            + cpu_after.system
            - cpu_before.user
            - cpu_before.system,
            "cpu_window_seconds": (ended - started) / 1e9,
            "rss_before": rss_before,
            "rss_after": process.memory_info().rss,
            "host_before": before_host,
            "host_after": host(),
            "stages": [asdict(stage) for stage in stages],
            "stage_table": [
                asdict(row) for row in stage_table(stages, written, published.time)
            ],
            "d18_pass": (read - started) <= 1_000_000_000
            and (publication_observed - started) <= 1_000_000_000,
            **result,
        }
        self.report["edits"].append(edit)
        self.report.pop("pending_edit", None)
        self.save()
        print(
            f"edit {step}: publication={edit['publication_seconds']:.6f}s final={edit['final_publication_seconds']:.6f}s read={edit['read_seconds']:.6f}s lexical={edit['lexical_observed_seconds']:.6f}s cpu={edit['cpu_seconds']:.6f}s D18={'PASS' if edit['d18_pass'] else 'FAIL'}",
            flush=True,
        )
        print(render_table(stage_table(stages, written, published.time)), flush=True)

    async def sample(self, stack: ExitStack) -> tuple[Process, float] | None:
        command, path, kind = self.options.sample_command(self.server.process.pid)
        sampling: dict[str, Any] = {"status": kind, "path": str(path)}
        self.report["sampling"] = sampling
        if command is None:
            print(f"sampling: {kind}", flush=True)
            return None
        log = stack.enter_context(path.with_suffix(path.suffix + ".log").open("wb"))
        process = stack.enter_context(command.spawn(log=log))
        started = time.perf_counter_ns()
        sampling.update(
            started=datetime.now(UTC).isoformat(),
            lead_seconds=self.options.pre_sample_seconds,
            command=command.argv,
        )
        await asyncio.sleep(self.options.pre_sample_seconds)
        if process.poll() is not None:
            self.report["sampling"]["status"] = (
                f"profiler exited before edit: {process.returncode}; see profiler log"
            )
            print(f"sampling: {self.report['sampling']['status']}", flush=True)
        return process, started / 1e9

    async def run(self) -> None:
        original = self.path.read_bytes()
        self.save()
        try:
            self.server.start(wait_for_publication=False)
            self.report["server_pid"] = self.server.process.pid
            async with asyncio.timeout(self.options.startup_seconds):
                await wait_publication(self.server, 0, "startup")
                while "lexical commit settled" not in self.server.read_log():
                    self.server.check_running()
                    await asyncio.sleep(0.1)
            async with self.server.connect(self.options.timeout_seconds) as client:
                with ExitStack() as stack:
                    sampler = await self.sample(stack)
                    for step in range(self.options.steps):
                        async with asyncio.timeout(self.options.timeout_seconds):
                            await self.measure_edit(client, original, step)
                    if sampler is not None:
                        process, started = sampler
                        remaining = max(
                            0.01,
                            self.options.sample_seconds
                            + 30
                            - (time.perf_counter() - started),
                        )
                        status = await asyncio.to_thread(process.wait, remaining)
                        self.report["sampling"]["exit_code"] = status
                        if status != 0:
                            self.report["sampling"]["status"] = (
                                "profiler failed; see profiler log"
                            )
        except BaseException as error:
            self.report["failure"] = repr(error)
            pending = self.report.get("pending_edit")
            if pending and self.server.log_path.exists():
                self.report["failed_edit_stages"] = [
                    asdict(stage)
                    for stage in parse_stages(
                        log_since(self.server.log_path, pending["offset"])
                    )
                ]
            raise
        finally:
            try:
                self.path.write_bytes(original)
            finally:
                try:
                    self.server.close()
                finally:
                    self.report["host_after"] = host()
                    self.save()


async def run(options: EditOptions) -> None:
    """Fetch the existing pin and remove the owned copy on success or failure."""
    options = options.model_copy(
        update={"binary": options.binary.resolve(), "output": options.output.resolve()}
    )
    if not options.binary.is_file():
        raise ValueError(
            "binary is missing; pass an existing release binary with --binary"
        )
    if options.output.exists():
        raise ValueError("report already exists; choose a new --output path")
    options.output.parent.mkdir(parents=True, exist_ok=True)
    pin = pins()[options.corpus]
    pin.sync()
    with tempfile.TemporaryDirectory(
        prefix=f"perf-{options.corpus}-", dir=options.output.parent
    ) as directory:
        root = Path(directory) / "checkout"
        pin.checkout(root)
        (root / "rift.toml").write_text(CONFIGURATION, encoding="utf-8")
        measurement = EditRun(options, pin, root)
        async with asyncio.timeout(options.run_seconds):
            await measurement.run()
    print(f"JSON: {options.output}", flush=True)
