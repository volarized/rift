"""Own real Rift processes and validate messages through the MCP Python SDK.

MCP 1.26.0 ClientSession.call_tool validates structured content against the
advertised output schema. This module also validates requests and tool errors.
The SDK owns the stdio proxy and its bounded shutdown. The foreground server
inherits the caller environment, including LLVM_PROFILE_FILE; overrides win.
Harness output always lives outside the served workspace.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import tempfile
import threading
import time
import traceback
import xml.etree.ElementTree as ET
from collections.abc import AsyncIterator, Coroutine, Iterator, Mapping, Sequence
from contextlib import ExitStack, asynccontextmanager, contextmanager
from contextvars import ContextVar
from datetime import timedelta
from pathlib import Path
from types import TracebackType
from typing import Self, TextIO, TypeAlias, cast

import psutil
import tomllib
from jsonschema import Draft202012Validator
from mcp import ClientSession, StdioServerParameters, types
from mcp.client.stdio import stdio_client
from pydantic import AnyUrl

from rift_dev.check_mcp_conformance import REPOSITORY, build_server_binary
from rift_dev.release_process import (
    Drain,
    owned_environment,
    owned_process,
    run,
    termination_handler,
)

Json: TypeAlias = None | bool | int | float | str | list["Json"] | dict[str, "Json"]
JsonObject: TypeAlias = dict[str, Json]
LOG_BYTES_MAX = 8 * 1024 * 1024
MESSAGE_BYTES_MAX = 16 * 1024 * 1024
PAGE_COUNT_MAX = 32
POLL_SECONDS = 0.05
STOP_SECONDS = 5.0
_GATE_DEADLINE: ContextVar[float | None] = ContextVar(
    "rift_gate_deadline", default=None
)


def object_value(value: object, context: str) -> JsonObject:
    """Require a JSON object at a decoded message boundary."""
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise AssertionError(f"{context}: expected an object, received {value!r}")
    return cast(JsonObject, value)


def array_value(value: Json, context: str) -> list[Json]:
    """Require an array before inspecting its members."""
    if not isinstance(value, list):
        raise TypeError(f"{context}: expected an array, received {value!r}")
    return value


def string_value(value: Json, context: str) -> str:
    """Require a nonempty string before using an emitted address."""
    if not isinstance(value, str) or not value:
        raise AssertionError(
            f"{context}: expected a nonempty string, received {value!r}"
        )
    return value


def require(condition: bool, detail: str) -> None:
    """Fail a gate even when Python runs with assertions disabled."""
    if not condition:
        raise AssertionError(detail)


def outside_workspace(path: Path, root: Path) -> None:
    """Reject output beneath the canonical served root, including symlinks."""
    require(
        not path.resolve().is_relative_to(root.resolve()),
        f"harness output must be outside the served workspace: {path}",
    )


def run_command(
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    timeout_seconds: float = 30.0,
) -> str:
    """Run a bounded command with inherited environment and an explicit overlay."""
    environment = dict(os.environ)
    environment.update(env or {})
    return run(
        command,
        cwd=cwd,
        environment=environment,
        timeout=timeout_seconds,
        deadline=_GATE_DEADLINE.get(),
    )


def candidate_binary(binary: Path | None, target: str | None = None) -> Path:
    """Use supplied bytes, or reuse the conformance runner's Cargo artifact discovery."""
    if binary is not None:
        require(
            target is None, "--target selects a build and cannot accompany --binary"
        )
        require(binary.is_file(), f"supplied binary does not exist: {binary}")
        return binary.resolve()
    return build_server_binary(release=True, target=target)


def workspace_version() -> str:
    """Read the expected version from the workspace's existing Cargo manifest."""
    with (REPOSITORY / "Cargo.toml").open("rb") as manifest:
        document = tomllib.load(manifest)
    return string_value(
        document["workspace"]["package"]["version"], "workspace.package.version"
    )


def verify_version(binary: Path, version: str) -> None:
    """Require the supplied executable to report exactly the expected release version."""
    expected = f"rift {version.removeprefix('v')}"
    observed = run_command([str(binary.resolve()), "--version"]).strip()
    require(observed == expected, f"expected {expected!r}, received {observed!r}")


def remaining_seconds(seconds: float) -> float:
    """Limit a synchronous wait to the current gate's remaining budget."""
    deadline = _GATE_DEADLINE.get()
    return (
        seconds
        if deadline is None
        else min(seconds, max(0.0, deadline - time.monotonic()))
    )


@asynccontextmanager
async def gate_deadline(name: str, seconds: float) -> AsyncIterator[None]:
    """Bound direct callers as well as CLI runs, including synchronous work.

    asyncio.timeout schedules cancellation on the event loop. The elapsed check
    also rejects work that blocked that loop until its timeout handle was removed.
    Nested gates keep the earlier deadline; cleanup retains its own process bounds.
    """
    if seconds <= 0:
        raise ValueError("gate deadline must be positive")
    started = time.monotonic()
    inherited = _GATE_DEADLINE.get()
    deadline = started + seconds
    if inherited is not None:
        deadline = min(deadline, inherited)
    budget = max(0.0, deadline - started)
    token = _GATE_DEADLINE.set(deadline)
    try:
        async with asyncio.timeout(budget):
            yield
        if time.monotonic() > deadline:
            raise TimeoutError(f"{name} exceeded its {budget:.3f}s deadline")
    finally:
        _GATE_DEADLINE.reset(token)


def run_gate(
    name: str, operation: Coroutine[None, None, None], junit: Path | None = None
) -> None:
    """Write a JUnit result on success or failure, preserving the original failure."""
    started = time.monotonic()
    failure: str | None = None
    try:
        asyncio.run(operation)
    except BaseException:
        failure = traceback.format_exc()
        raise
    finally:
        if junit is not None:
            write_junit(junit, name, time.monotonic() - started, failure)


def xml_text(value: str) -> str:
    """Keep bounded terminal output valid in XML text and attribute values."""
    return "".join(
        character
        for character in value[-LOG_BYTES_MAX:]
        if character.isprintable() or character in "\t\n\r"
    )


def write_junit(path: Path, name: str, seconds: float, failure: str | None) -> None:
    """Serialize one gate result with XML escaping and a bounded failure transcript."""
    suite = ET.Element(
        "testsuite",
        name=name,
        tests="1",
        failures=str(int(failure is not None)),
        time=f"{seconds:.6f}",
    )
    case = ET.SubElement(
        suite, "testcase", classname="rift.gate", name=name, time=f"{seconds:.6f}"
    )
    if failure is not None:
        finding = ET.SubElement(case, "failure", message=f"{name} failed")
        finding.text = xml_text(failure)
    path.parent.mkdir(parents=True, exist_ok=True)
    ET.ElementTree(suite).write(path, encoding="utf-8", xml_declaration=True)


@contextmanager
def stderr_log(path: Path | None = None) -> Iterator[TextIO]:
    """Give the SDK a real stderr handle backed by the shared bounded drain.

    The SDK must close its process before this context exits. On overflow the
    drain stops reading, so the child's next write meets the request deadline.
    """
    read_fd, write_fd = os.pipe()
    with (
        os.fdopen(read_fd, "rb", buffering=0) as source,
        os.fdopen(write_fd, "w", encoding="utf-8") as destination,
    ):
        drain = Drain(source, LOG_BYTES_MAX, threading.Event())
        reader = threading.Thread(target=drain.read, daemon=True)
        reader.start()
        try:
            yield destination
        finally:
            destination.close()
            reader.join(timeout=STOP_SECONDS)
            require(not reader.is_alive(), "SDK stderr did not close within its bound")
            if path is not None:
                path.write_bytes(drain.data[:LOG_BYTES_MAX])
        if drain.error is not None:
            raise RuntimeError("SDK stderr collection failed") from drain.error


class Client:
    """Validate calls against the schemas advertised by one SDK session."""

    def __init__(self, session: ClientSession, call_seconds: float = 120.0) -> None:
        self.session = session
        self.call_seconds = call_seconds
        self.tools: dict[str, types.Tool] = {}
        self.exercised: set[str] = set()

    async def initialize(self) -> None:
        """Reject missing schemas or a tool listing that exceeds its page bound."""
        async with asyncio.timeout(self.call_seconds):
            await self.session.initialize()
            cursor: str | None = None
            for _ in range(PAGE_COUNT_MAX):
                page = await self.session.list_tools(
                    params=types.PaginatedRequestParams(cursor=cursor)
                )
                for tool in page.tools:
                    require(tool.name not in self.tools, f"duplicate tool: {tool.name}")
                    if tool.outputSchema is None:
                        raise AssertionError(f"{tool.name} has no output schema")
                    for schema in (tool.inputSchema, tool.outputSchema):
                        require(
                            schema is not None and schema.get("type") == "object",
                            f"{tool.name}: MCP requires object schemas",
                        )
                        Draft202012Validator.check_schema(schema)
                    self.tools[tool.name] = tool
                cursor = page.nextCursor
                if cursor is None:
                    require(bool(self.tools), "tools/list returned no tools")
                    return
        raise AssertionError(f"tools/list exceeded {PAGE_COUNT_MAX} pages")

    async def call(self, name: str, arguments: JsonObject) -> JsonObject:
        """Validate requests and structured answers, preserving refusal values."""
        tool = self.tools[name]
        Draft202012Validator(tool.inputSchema).validate(arguments)
        async with asyncio.timeout(self.call_seconds):
            result = await self.session.call_tool(name, arguments)
        answer = object_value(result.structuredContent, name)
        require(
            len(json.dumps(answer).encode()) <= MESSAGE_BYTES_MAX,
            f"{name} result exceeds {MESSAGE_BYTES_MAX} bytes",
        )
        if tool.outputSchema is None:
            raise AssertionError(f"{name} has no output schema")
        Draft202012Validator(tool.outputSchema).validate(answer)
        require(not result.isError, f"{name} reported a tool error: {answer}")
        self.exercised.add(name)
        return answer

    async def resource(self, uri: str) -> JsonObject:
        """Require the SDK resource envelope to contain exactly one JSON document."""
        async with asyncio.timeout(self.call_seconds):
            result = await self.session.read_resource(AnyUrl(uri))
        require(len(result.contents) == 1, f"{uri}: expected one resource document")
        content = result.contents[0]
        if not isinstance(content, types.TextResourceContents):
            raise TypeError(f"{uri}: expected text content")
        require(str(content.uri) == uri, f"{uri}: resource returned {content.uri}")
        require(
            content.mimeType == "application/json", f"{uri}: expected application/json"
        )
        require(
            len(content.text.encode()) <= MESSAGE_BYTES_MAX,
            f"{uri}: document too large",
        )
        return object_value(json.loads(content.text), uri)

    def require_complete(self, read_tools: set[str]) -> None:
        """Require every selected read tool to be advertised and exercised."""
        require(
            read_tools <= set(self.tools),
            f"missing tools: {sorted(read_tools - set(self.tools))}",
        )
        require(
            read_tools <= self.exercised,
            f"unexercised tools: {sorted(read_tools - self.exercised)}",
        )


def process_alive(process: psutil.Process) -> bool:
    """Check a captured process identity while tolerating exit between observations."""
    try:
        return process.is_running() and process.status() != psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        return False


class Server:
    """Own a foreground server, its external log, and observed descendants."""

    def __init__(
        self,
        binary: Path,
        root: Path,
        log_path: Path,
        *,
        startup_seconds: float = 120.0,
        env: Mapping[str, str] | None = None,
    ) -> None:
        outside_workspace(log_path, root)
        require(startup_seconds > 0, "startup timeout must be positive")
        self.binary = binary.resolve()
        self.root = root
        self.log_path = log_path
        self.startup_seconds = startup_seconds
        self.env = dict(os.environ)
        self.env.update(env or {})
        self.process: subprocess.Popen[bytes]
        self.port = 0
        self._process_stack = ExitStack()
        self._owner: psutil.Process | None = None
        self._reader: threading.Thread | None = None
        self._log_failure: BaseException | None = None
        self._descendants: list[psutil.Process] = []
        self._stopped = False

    def start(self, *, wait_for_publication: bool = True) -> Self:
        """Start once; callers may inspect output before awaiting publication."""
        require(self._reader is None, "server has already been started")
        require(
            not (self.root / ".rift" / "server.json").exists(),
            "workspace already has a server document; use a disposable workspace",
        )
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._process_stack.enter_context(termination_handler())
            with tempfile.TemporaryFile() as stdin:
                self.process = self._process_stack.enter_context(
                    owned_process(
                        [
                            str(self.binary),
                            "server",
                            "start",
                            "--foreground",
                            "--auth",
                            "skip",
                        ],
                        self.env,
                        self.root,
                        stdin,
                        stderr=subprocess.STDOUT,
                    )
                )
            try:
                self._owner = psutil.Process(self.process.pid)
            except psutil.NoSuchProcess:
                self._owner = None
            self._reader = threading.Thread(target=self._drain, daemon=True)
            self._reader.start()
            if wait_for_publication:
                self.await_publication()
            return self
        except BaseException:
            self.close()
            raise

    def _drain(self) -> None:
        """Count at most LOG_BYTES_MAX + 1 bytes; a flooding child then blocks."""
        try:
            if self.process.stdout is None:
                raise AssertionError("server stdout pipe is missing")
            with self.log_path.open("wb") as log:
                remaining = LOG_BYTES_MAX
                while remaining >= 0:
                    chunk = os.read(
                        self.process.stdout.fileno(), min(65536, remaining + 1)
                    )
                    if not chunk:
                        return
                    require(
                        len(chunk) <= remaining,
                        f"server output exceeded {LOG_BYTES_MAX} bytes",
                    )
                    log.write(chunk)
                    log.flush()
                    remaining -= len(chunk)
        except (OSError, ValueError, AssertionError) as failure:
            self._log_failure = failure

    def read_log(self) -> str:
        """Read only the bounded output this server owns."""
        if not self.log_path.exists():
            return ""
        with self.log_path.open("rb") as log:
            return log.read(LOG_BYTES_MAX).decode("utf-8", errors="replace")

    def await_publication(self) -> int:
        """Wait at most startup_seconds, checking process ownership in server.json."""
        timeout_seconds = remaining_seconds(self.startup_seconds)
        deadline = time.monotonic() + timeout_seconds
        document = self.root / ".rift" / "server.json"
        while time.monotonic() < deadline:
            self.check_running()
            if document.exists():
                with document.open("rb") as source:
                    encoded = source.read(65537)
                require(len(encoded) <= 65536, "server document exceeded 65536 bytes")
                value = object_value(json.loads(encoded), "server.json")
                require(
                    value.get("pid") == self.process.pid,
                    "server.json names another process",
                )
                port = value.get("port")
                require(
                    type(port) is int and 0 < port < 65536,
                    "server.json has an invalid port",
                )
                self.port = cast(int, port)
                return self.port
            time.sleep(POLL_SECONDS)
        raise AssertionError(
            f"server did not publish within {timeout_seconds}s\n{self.read_log()}"
        )

    def _check_log(self) -> None:
        if self._log_failure is not None:
            raise RuntimeError("server log collection failed") from self._log_failure

    def check_running(self) -> None:
        """Fail immediately on an exited server or failed log collection."""
        self._check_log()
        require(
            self.process.poll() is None,
            f"server exited {self.process.returncode}\n{self.read_log()}",
        )

    @asynccontextmanager
    async def connect(
        self, call_seconds: float = 120.0, *, log_path: Path | None = None
    ) -> AsyncIterator[Client]:
        """Connect through the SDK's real `rift mcp` stdio subprocess.

        An explicit log_path keeps concurrent connections' stderr files separate.
        """
        proxy_log = log_path or self.log_path.with_suffix(".mcp.log")
        outside_workspace(proxy_log, self.root)
        with (
            stderr_log(proxy_log) as log,
            owned_environment(self.env) as environment,
        ):
            parameters = StdioServerParameters(
                command=str(self.binary),
                args=["mcp"],
                cwd=str(self.root),
                env=environment,
            )
            async with (
                stdio_client(parameters, errlog=log) as (read, write),
                ClientSession(read, write, timedelta(seconds=call_seconds)) as session,
            ):
                client = Client(session, call_seconds)
                await client.initialize()
                yield client
                self._check_log()
                if not self._stopped:
                    self.check_running()

    def _observe_descendants(self) -> None:
        try:
            if self._owner is not None:
                self._descendants.extend(self._owner.children(recursive=True))
        except psutil.NoSuchProcess:
            pass

    def stop(self, timeout_seconds: float = STOP_SECONDS) -> None:
        """Require CLI stop and owned process exit within one deadline."""
        started = time.monotonic()
        deadline = started + timeout_seconds
        self._observe_descendants()
        budget = remaining_seconds(max(0.0, deadline - time.monotonic()))
        require(budget > 0, "server stop exceeded its deadline")
        run_command(
            [str(self.binary), "server", "stop"],
            cwd=self.root,
            env=self.env,
            timeout_seconds=budget,
        )
        # The CLI waits for election release. Process exit may follow it.
        try:
            self.process.wait(
                timeout=remaining_seconds(max(0.0, deadline - time.monotonic()))
            )
        except subprocess.TimeoutExpired as error:
            raise AssertionError(
                "server stop returned while its process remained alive"
            ) from error
        alive = [process.pid for process in self._descendants if process_alive(process)]
        require(not alive, f"server stop left child processes alive: {alive}")
        require(
            self.process.returncode == 0,
            f"server exited {self.process.returncode}\n{self.read_log()}",
        )

        self._check_log()
        require(
            time.monotonic() <= deadline,
            "server stop exceeded its deadline",
        )
        self._stopped = True

    def close(self) -> None:
        """Close the shared process owner, then join its bounded log reader."""
        self._process_stack.close()
        if self._reader is None:
            return
        self._reader.join(timeout=STOP_SECONDS)
        require(not self._reader.is_alive(), "server log reader did not stop")
        if self.process.stdout is not None:
            self.process.stdout.close()

    def __enter__(self) -> Self:
        return self.start()

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()
