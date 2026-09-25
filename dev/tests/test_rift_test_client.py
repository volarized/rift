"""Gate regressions for schema validation, process cleanup, and workspace isolation."""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path
from typing import TextIO, cast
from unittest.mock import AsyncMock

import psutil
import pytest
from jsonschema import ValidationError
from mcp import ClientSession, types
from rift_dev.commands import Command, Process
from rift_dev.rift_test_client import Client, Server, outside_workspace

SCHEMA = {
    "type": "object",
    "properties": {"results": {"type": "array"}},
    "required": ["results"],
}


def client_with_result(result: types.CallToolResult) -> Client:
    session = AsyncMock(spec=ClientSession)
    session.call_tool.return_value = result
    client = Client(cast(ClientSession, session))
    client.tools["search"] = types.Tool(
        name="search", input_schema={"type": "object"}, output_schema=SCHEMA
    )
    return client


@pytest.mark.parametrize("error", [False, True])
def test_invalid_structured_answer_fails_including_tool_errors(error: bool) -> None:
    client = client_with_result(
        types.CallToolResult(
            content=[], structured_content={"status": "unknown"}, is_error=error
        )
    )
    with pytest.raises(ValidationError):
        asyncio.run(client.call("search", {}))


def test_missing_structured_answer_fails() -> None:
    client = client_with_result(types.CallToolResult(content=[]))
    with pytest.raises(AssertionError, match="expected an object"):
        asyncio.run(client.call("search", {}))


def test_non_object_tool_schema_fails_before_calls() -> None:
    session = AsyncMock(spec=ClientSession)
    session.list_tools.return_value = types.ListToolsResult(
        tools=[
            types.Tool(
                name="search",
                input_schema={"type": "object"},
                output_schema={"oneOf": [SCHEMA]},
            ),
        ]
    )
    with pytest.raises(AssertionError, match="requires object schemas"):
        asyncio.run(Client(cast(ClientSession, session)).initialize())


def test_invalid_input_never_reaches_server() -> None:
    client = client_with_result(
        types.CallToolResult(content=[], structured_content={"results": []})
    )
    client.tools["search"].input_schema = {"type": "object", "required": ["search"]}
    with pytest.raises(ValidationError):
        asyncio.run(client.call("search", {}))
    cast(AsyncMock, client.session.call_tool).assert_not_called()


def test_resource_rejects_wrong_uri() -> None:
    session = AsyncMock(spec=ClientSession)
    session.read_resource.return_value = types.ReadResourceResult(
        contents=[
            types.TextResourceContents(
                uri="rift://map", mime_type="application/json", text="{}"
            ),
        ]
    )
    with pytest.raises(AssertionError, match="resource returned"):
        asyncio.run(Client(cast(ClientSession, session)).resource("rift://logs"))


def test_log_path_cannot_be_inside_served_workspace(tmp_path: Path) -> None:
    with pytest.raises(AssertionError, match="outside the served workspace"):
        outside_workspace(tmp_path / "server.log", tmp_path)


@pytest.mark.skipif(
    os.name == "nt", reason="symlink creation requires a Windows privilege"
)
def test_log_path_cannot_enter_through_a_symlink(tmp_path: Path) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    link = tmp_path / "linked"
    link.symlink_to(root, target_is_directory=True)
    with pytest.raises(AssertionError, match="outside the served workspace"):
        outside_workspace(link / "server.log", root)


def test_commands_preserve_coverage_environment_and_overlays(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("LLVM_PROFILE_FILE", "/tmp/rift-%p-%m.profraw")
    monkeypatch.setenv("RIFT_TEST_VALUE", "inherited")
    code = "import json,os; print(json.dumps([os.environ['LLVM_PROFILE_FILE'],os.environ['RIFT_TEST_VALUE']]))"
    assert json.loads(Command(sys.executable, "-c", code).output()) == [
        "/tmp/rift-%p-%m.profraw",
        "inherited",
    ]
    assert json.loads(
        Command(sys.executable, "-c", code).with_env(RIFT_TEST_VALUE="overlay").output()
    ) == [
        "/tmp/rift-%p-%m.profraw",
        "overlay",
    ]


def fake_binary(tmp_path: Path, behavior: str) -> Path:
    binary = tmp_path / "rift"
    binary.write_text(f"#!{sys.executable}\n" + behavior, encoding="utf-8")
    binary.chmod(0o755)
    return binary


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
@pytest.mark.parametrize("interruption", ["timeout", "sigterm"])
def test_active_sdk_request_interruption_reaps_every_owned_process(
    tmp_path: Path, interruption: str
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    binary = fake_binary(
        tmp_path,
        """import asyncio,json,os,pathlib,signal,subprocess,sys,time
from mcp.server.mcpserver import MCPServer

if sys.argv[1:]==['server','start','--foreground','--auth','skip']:
    pathlib.Path('.rift').mkdir()
    pathlib.Path('.rift/server.json').write_text(json.dumps({'pid':os.getpid(),'port':12000}))
    time.sleep(30)
else:
    app = MCPServer('interrupted request')

    @app.tool()
    async def wait() -> dict[str, str]:
        child = subprocess.Popen(
            [sys.executable, '-c', 'import time; time.sleep(30)'],
            start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        pathlib.Path('active.json').write_text(json.dumps([os.getpid(), child.pid]))
        if os.environ['RIFT_TEST_INTERRUPTION'] == 'sigterm':
            await asyncio.sleep(0.05)
            os.kill(os.getppid(), signal.SIGTERM)
        await asyncio.sleep(30)
        return {'status': 'finished'}

    app.run(transport='stdio')
""",
    )
    server = Server(
        binary,
        root,
        tmp_path / "server.log",
        env={"RIFT_TEST_INTERRUPTION": interruption},
    )

    async def operation() -> None:
        with server:
            async with server.connect() as client:
                async with asyncio.timeout(2):
                    await client.call("wait", {})

    started = time.monotonic()
    expected = RuntimeError if interruption == "sigterm" else TimeoutError
    with pytest.RaisesGroup(
        pytest.RaisesExc(
            expected,
            match="interrupted by SIGTERM" if interruption == "sigterm" else None,
        ),
        allow_unwrapped=True,
        flatten_subgroups=True,
    ):
        asyncio.run(operation())
    assert time.monotonic() - started < 10
    assert server.process.poll() is not None
    assert not psutil.pid_exists(server.process.pid)
    proxy, detached = json.loads((root / "active.json").read_text())
    assert not psutil.pid_exists(proxy)
    try:
        child = psutil.Process(detached)
        assert child.status() == psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        pass


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_failed_startup_reaps_server(tmp_path: Path) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    binary = fake_binary(tmp_path, "import time\ntime.sleep(30)\n")
    server = Server(binary, root, tmp_path / "server.log", startup_seconds=0.1)
    with pytest.raises(AssertionError, match="did not publish"):
        server.start()
    assert server.process.poll() is not None
    assert not psutil.pid_exists(server.process.pid)


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_dishonest_stop_fails_and_cleanup_reaps_server(tmp_path: Path) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    binary = fake_binary(
        tmp_path,
        """import json,os,pathlib,sys,time
if sys.argv[1:]==['server','stop']:
    sys.exit(0)
pathlib.Path('.rift').mkdir()
pathlib.Path('.rift/server.json').write_text(json.dumps({'pid':os.getpid(),'port':12000}))
time.sleep(30)
""",
    )
    with (
        Server(binary, root, tmp_path / "server.log") as server,
        pytest.raises(AssertionError, match="remained alive"),
    ):
        server.stop()
    assert server.process.poll() is not None
    assert not psutil.pid_exists(server.process.pid)


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_stop_waits_for_owned_process_within_remaining_budget(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    binary = fake_binary(
        tmp_path,
        """import json,os,pathlib,sys,time
if sys.argv[1:]==['server','stop']:
    pathlib.Path('stop.request').write_text('requested')
    sys.exit(0)
pathlib.Path('.rift').mkdir()
pathlib.Path('.rift/server.json').write_text(json.dumps({'pid':os.getpid(),'port':12000}))
while not pathlib.Path('stop.release').exists():
    time.sleep(0.01)
""",
    )
    with Server(binary, root, tmp_path / "server.log") as server:
        wait = server.process.wait
        budgets: list[float] = []

        def release_and_wait(timeout: float) -> int:
            if server.process.poll() is None:
                assert (root / "stop.request").exists()
                assert timeout is not None
                budgets.append(timeout)
                (root / "stop.release").write_text("released")
            return wait(timeout=timeout)

        monkeypatch.setattr(server.process, "wait", release_and_wait)
        server.stop(timeout_seconds=1.0)
        assert server.process.returncode == 0
        assert len(budgets) == 1
        assert 0 < budgets[0] < 1.0
    assert not psutil.pid_exists(server.process.pid)


@pytest.mark.parametrize("fails", [False, True])
def test_junit_records_success_and_failure(tmp_path: Path, fails: bool) -> None:
    import xml.etree.ElementTree as element_tree

    from rift_dev.rift_test_client import run_gate

    async def operation() -> None:
        if fails:
            raise RuntimeError("source <expected> & observed")

    report = tmp_path / "results" / "gate.xml"
    if fails:
        with pytest.raises(RuntimeError, match="source"):
            run_gate("artifact", operation(), report)
    else:
        run_gate("artifact", operation(), report)
    suite = element_tree.parse(report).getroot()
    assert suite.get("tests") == "1"
    assert suite.get("failures") == str(int(fails))
    assert (suite.find("testcase/failure") is not None) == fails


def test_server_drain_stops_at_its_byte_bound(tmp_path: Path) -> None:
    import tempfile
    from unittest.mock import Mock

    from rift_dev.rift_test_client import LOG_BYTES_MAX

    root = tmp_path / "workspace"
    root.mkdir()
    server = Server(tmp_path / "rift", root, tmp_path / "server.log")
    process = Mock(spec=Process)
    with tempfile.TemporaryFile() as source:
        source.write(b"x" * (LOG_BYTES_MAX + 1))
        source.seek(0)
        process.stdout = source
        server.process = process
        server._drain()
    with pytest.raises(RuntimeError, match="log collection failed"):
        server.check_running()
    process.poll.assert_not_called()
    assert server.log_path.stat().st_size == LOG_BYTES_MAX


def test_tool_error_cannot_satisfy_a_read() -> None:
    client = client_with_result(
        types.CallToolResult(
            content=[], structured_content={"results": []}, is_error=True
        )
    )
    with pytest.raises(AssertionError, match="reported a tool error"):
        asyncio.run(client.call("search", {}))


def test_failed_process_creation_restores_signal_handler(tmp_path: Path) -> None:
    import signal

    root = tmp_path / "workspace"
    root.mkdir()
    previous = signal.getsignal(signal.SIGTERM)
    with pytest.raises(FileNotFoundError):
        Server(tmp_path / "missing-rift", root, tmp_path / "server.log").start()
    assert signal.getsignal(signal.SIGTERM) == previous


def test_junit_keeps_terminal_output_valid_xml(tmp_path: Path) -> None:
    import xml.etree.ElementTree as element_tree

    from rift_dev.rift_test_client import write_junit

    path = tmp_path / "failure.xml"
    write_junit(path, "cold", 1.0, "engine\x1b[31m failed\x00")
    root = element_tree.parse(path).getroot()
    assert root.find("testcase/failure") is not None


@pytest.mark.parametrize("explicit_stop", [False, True])
def test_connection_accepts_only_a_verified_stop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, explicit_stop: bool
) -> None:
    from collections.abc import AsyncIterator
    from contextlib import asynccontextmanager
    from unittest.mock import Mock

    from rift_dev import rift_test_client

    root = tmp_path / "workspace"
    root.mkdir()
    server = Server(tmp_path / "rift", root, tmp_path / "server.log")
    process = Mock(spec=Process)
    process.poll.return_value = None
    process.returncode = 0
    server.process = process

    @asynccontextmanager
    async def transport(
        *args: object, **kwargs: object
    ) -> AsyncIterator[tuple[None, None]]:
        yield None, None

    session = AsyncMock(spec=ClientSession)
    monkeypatch.setattr(rift_test_client, "stdio_client", transport)
    monkeypatch.setattr(rift_test_client, "ClientSession", lambda *args: session)
    monkeypatch.setattr(Client, "initialize", AsyncMock())
    monkeypatch.setattr(Command, "output", lambda command: "")

    async def operation() -> None:
        async with server.connect():
            process.poll.return_value = 0
            if explicit_stop:
                server.stop()

    if explicit_stop:
        asyncio.run(operation())
    else:
        with pytest.raises(AssertionError, match="server exited"):
            asyncio.run(operation())


def test_concurrent_connections_preserve_separate_proxy_logs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from collections.abc import AsyncIterator
    from contextlib import asynccontextmanager
    from unittest.mock import Mock

    from rift_dev import rift_test_client

    server = Server(tmp_path / "rift", tmp_path / "workspace", tmp_path / "server.log")
    process = Mock(spec=Process)
    process.poll.return_value = None
    process.returncode = None
    server.process = process
    count = 0

    @asynccontextmanager
    async def transport(
        *args: object, errlog: TextIO, **kwargs: object
    ) -> AsyncIterator[tuple[None, None]]:
        nonlocal count
        count += 1
        errlog.write(f"connection-{count}")
        errlog.flush()
        yield None, None

    session = AsyncMock(spec=ClientSession)
    monkeypatch.setattr(rift_test_client, "stdio_client", transport)
    monkeypatch.setattr(rift_test_client, "ClientSession", lambda *args: session)
    monkeypatch.setattr(Client, "initialize", AsyncMock())
    observer_log = tmp_path / "observer.mcp.log"

    async def operation() -> None:
        async with server.connect(), server.connect(log_path=observer_log):
            assert count == 2

    asyncio.run(operation())
    assert (tmp_path / "server.mcp.log").read_bytes() == b"connection-1"
    assert observer_log.read_bytes() == b"connection-2"


def test_explicit_proxy_log_cannot_enter_served_workspace(tmp_path: Path) -> None:
    root = tmp_path / "workspace"
    server = Server(tmp_path / "rift", root, tmp_path / "server.log")

    async def operation() -> None:
        async with server.connect(log_path=root / "observer.log"):
            pytest.fail("connection must refuse a log inside its workspace")

    with pytest.raises(AssertionError, match="outside the served workspace"):
        asyncio.run(operation())
    assert not root.exists()


def test_sdk_stderr_overflow_is_bounded(tmp_path: Path) -> None:
    import anyio
    from rift_dev.rift_test_client import LOG_BYTES_MAX, stderr_log

    path = tmp_path / "mcp.log"
    # The SDK's stdio transport starts its server through `anyio.open_process`
    # with this log as stderr; `anyio.run_process` does the same.
    program = f"import os; os.write(2, b'x' * {LOG_BYTES_MAX + 1})"

    async def transport(log: TextIO) -> None:
        with anyio.fail_after(5):
            await anyio.run_process([sys.executable, "-c", program], stderr=log)

    with (
        pytest.raises(RuntimeError, match="SDK stderr collection failed"),
        stderr_log(path) as log,
    ):
        anyio.run(transport, log)
    assert path.stat().st_size == LOG_BYTES_MAX


def test_gate_deadline_cancels_async_work_after_cleanup_and_records_failure(
    tmp_path: Path,
) -> None:
    import xml.etree.ElementTree as element_tree

    from rift_dev.rift_test_client import gate_deadline, run_gate

    cleanup: list[bool] = []

    async def operation() -> None:
        async with gate_deadline("agent", 0.02):
            try:
                await asyncio.Event().wait()
            finally:
                cleanup.append(True)

    path = tmp_path / "deadline.xml"
    with pytest.raises(TimeoutError):
        run_gate("agent", operation(), path)
    assert cleanup == [True]
    assert element_tree.parse(path).getroot().get("failures") == "1"


def test_gate_deadline_rejects_synchronous_work_that_blocks_cancellation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from types import SimpleNamespace

    from rift_dev import rift_test_client

    clock = [100.0]
    monkeypatch.setattr(
        rift_test_client, "time", SimpleNamespace(monotonic=lambda: clock[0])
    )

    async def operation() -> None:
        async with rift_test_client.gate_deadline("artifact", 1.0):
            clock[0] += 2.0

    with pytest.raises(TimeoutError, match="artifact exceeded"):
        asyncio.run(operation())
    assert rift_test_client.remaining_seconds(30.0) == 30.0


def test_nested_gate_keeps_and_restores_the_earlier_deadline(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from types import SimpleNamespace

    from rift_dev import rift_test_client

    clock = [100.0]
    monkeypatch.setattr(
        rift_test_client, "time", SimpleNamespace(monotonic=lambda: clock[0])
    )

    async def operation() -> None:
        async with rift_test_client.gate_deadline("upgrade", 2.0):
            with pytest.raises(ValueError, match="fixture"):
                async with rift_test_client.gate_deadline("artifact", 240.0):
                    assert rift_test_client.remaining_seconds(300.0) == 2.0
                    raise ValueError("fixture")
            assert rift_test_client.remaining_seconds(300.0) == 2.0
        assert rift_test_client.remaining_seconds(300.0) == 300.0

    asyncio.run(operation())


def test_gate_deadline_limits_a_synchronous_command() -> None:
    from rift_dev.rift_test_client import current_deadline, gate_deadline

    async def operation() -> None:
        async with gate_deadline("artifact", 0.1):
            Command(sys.executable, "-c", "import time; time.sleep(30)").with_timeout(
                30
            ).with_deadline(current_deadline()).output()

    with pytest.raises(RuntimeError, match="exceeded"):
        asyncio.run(operation())


@pytest.mark.parametrize("phase", ["_observe_descendants", "_check_log"])
def test_stop_deadline_includes_observation_and_validation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, phase: str
) -> None:
    from types import SimpleNamespace
    from unittest.mock import Mock

    from rift_dev import rift_test_client

    clock = [100.0]
    monkeypatch.setattr(
        rift_test_client, "time", SimpleNamespace(monotonic=lambda: clock[0])
    )
    server = Server(tmp_path / "rift", tmp_path / "workspace", tmp_path / "server.log")
    process = Mock(spec=Process)
    process.returncode = 0
    server.process = process
    monkeypatch.setattr(Command, "output", lambda command: "")

    def exceed_deadline() -> None:
        clock[0] += 6.0

    monkeypatch.setattr(server, phase, exceed_deadline)
    with pytest.raises(AssertionError, match="stop exceeded its deadline"):
        server.stop()
    assert not server._stopped


def test_missing_read_tool_is_rejected() -> None:
    client = client_with_result(
        types.CallToolResult(content=[], structured_content={"results": []})
    )
    with pytest.raises(AssertionError, match="missing tools.*nodes"):
        client.require_complete({"search", "nodes"})


def test_unexercised_read_tool_is_rejected() -> None:
    client = client_with_result(
        types.CallToolResult(content=[], structured_content={"results": []})
    )
    with pytest.raises(AssertionError, match="unexercised tools.*search"):
        client.require_complete({"search"})
    asyncio.run(client.call("search", {}))
    client.require_complete({"search"})
