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
from rift_dev.rift_test_client import (
    LOG_FILTER,
    RECORDS_FILE_BYTES,
    Client,
    FailureCause,
    FailureLimit,
    Server,
    ToolFailure,
    cut_notice,
    outside_workspace,
    parse_failure,
)

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


def test_invalid_structured_answer_fails() -> None:
    client = client_with_result(
        types.CallToolResult(content=[], structured_content={"status": "unknown"})
    )
    with pytest.raises(ValidationError):
        asyncio.run(client.call("search", {}))


def failed_result(*lines: str) -> types.CallToolResult:
    text = types.TextContent(text="".join(f"{line}\n" for line in lines))
    return types.CallToolResult(content=[text], is_error=True)


def test_success_result_returns_the_structured_answer() -> None:
    client = client_with_result(
        types.CallToolResult(
            content=[types.TextContent(text="0 results\n")],
            structured_content={"results": []},
        )
    )
    assert asyncio.run(client.call("search", {})) == {"results": []}
    assert client.exercised == {"search"}


def test_tool_error_is_parsed_without_the_output_schema() -> None:
    client = client_with_result(
        failed_result("1 error", "\tinvalid_request · retry never", "\t\tbad request")
    )
    with pytest.raises(ToolFailure) as caught:
        asyncio.run(client.call("search", {}))
    failure = caught.value
    assert failure.tool == "search"
    assert failure.code == "invalid_request"
    assert failure.message == "bad request"
    assert failure.retry == "never"
    assert failure.limit is None
    assert failure.causes == []
    assert failure.diagnostics == []
    assert "invalid_request" in str(failure)
    assert client.exercised == set()


def test_tool_error_message_is_unescaped() -> None:
    failure = parse_failure(
        "nodes",
        "1 error\n\tinternal_error · retry never\n"
        '\t\tfirst\\nsecond\\r\\t\\u{1b} "quoted" back\\slash\n',
    )
    assert failure.message == 'first\nsecond\r\t\x1b "quoted" back\\slash'


def test_tool_error_keeps_the_limit_line() -> None:
    failure = parse_failure(
        "search",
        "1 error\n\tlimit_exceeded · retry never\n\t\tcrosses a limit\n"
        "\t\tlimit source.files: 20001 over 20000\n",
    )
    assert failure.limit == FailureLimit("source.files", 20001, 20000)
    assert (failure.limit.field, failure.limit.required, failure.limit.limit) == (
        "source.files",
        20001,
        20000,
    )


def test_tool_error_causes_write_their_own_code_and_retry() -> None:
    failure = parse_failure(
        "search",
        "5 errors\n"
        "\tstorage_failure · retry same_request\n\t\tstore failed\n"
        "\tstorage_failure · retry same_request\n\t\tdisk refused\n"
        "\tinternal_error · retry same_request\n\t\ttask ended\n"
        "\tstorage_failure · retry never\n\t\tagain\n"
        "\tinternal_error · retry operator_action\n\t\tother\n",
    )
    assert failure.code == "storage_failure"
    assert failure.message == "store failed"
    assert failure.causes == [
        FailureCause("storage_failure", "disk refused", "same_request"),
        FailureCause("internal_error", "task ended", "same_request"),
        FailureCause("storage_failure", "again", "never"),
        FailureCause("internal_error", "other", "operator_action"),
    ]


def test_tool_error_keeps_diagnostics_lines_raw() -> None:
    failure = parse_failure(
        "search",
        "2 errors\n\tinvalid_request · retry never\n\t\tbad query\n"
        "\t\tlimit query.length: 9 over 8\n"
        "\t\terror rift.toml:3:1: unknown key\n"
        "\t\twarning rift.toml:4:2: unused\n"
        "\tinvalid_request · retry never\n\t\tnested\n",
    )
    assert failure.diagnostics == [
        "error rift.toml:3:1: unknown key",
        "warning rift.toml:4:2: unused",
    ]
    assert failure.causes == [FailureCause("invalid_request", "nested", "never")]


@pytest.mark.parametrize(
    "text",
    [
        "",
        "no error here",
        "error x · retry never\nmessage",
        "1 error",
        "1 error\n\tx · retry never",
        "1 error\n\tx · retry never\n\n\t\tmessage",
        "1 error\n\tx retry never\n\t\tmessage",
        "1 error\n x · retry never\n\t\tmessage",
        "1 error\nx · retry never\n\t\tmessage",
        "1 error\n\t\tmessage\n\tx · retry never",
        "1 error\n\tx · retry never\n\t\tmessage\n\t\tlimit a: one over 2",
        (
            "1 error\n\tx · retry never\n\t\tmessage\n\t\tlimit a: 1 over 2\n\t\tlimit b: 1 over 2"
        ),
        "2 errors\n\tx · retry never\n\t\tmessage",
        "1 error\n\tx · retry never\n\t\tmessage\n\ty · retry never\n\t\tcause",
        "1 errors\n\tx · retry never\n\t\tmessage",
        "2 error\n\tx · retry never\n\t\ta\n\ty · retry never\n\t\tb",
        "2 errors\n\tx · retry never\n\t\ta\n\ty · retry never",
        "2 errors\n\tx · retry never\n\t\ta\n\ty · retry never\n\t\tb\n\t\tc",
        "2 errors\n\tx · retry never\n\t\ta\n\ty\n\t\tb",
    ],
)
def test_malformed_tool_error_text_fails(text: str) -> None:
    client = client_with_result(
        types.CallToolResult(content=[types.TextContent(text=text)], is_error=True)
    )
    with pytest.raises(AssertionError):
        asyncio.run(client.call("search", {}))


def test_json_rpc_error_still_raises_from_the_session() -> None:
    from mcp.shared.exceptions import MCPError

    client = client_with_result(types.CallToolResult(content=[]))
    cast(AsyncMock, client.session.call_tool).side_effect = MCPError(
        code=-32601, message="unknown tool", data={"code": "x"}
    )
    with pytest.raises(MCPError) as caught:
        asyncio.run(client.call("search", {}))
    assert caught.value.error.code == -32601
    assert not isinstance(caught.value, ToolFailure)


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


def resource_client(*contents: tuple[str, str, str]) -> Client:
    session = AsyncMock(spec=ClientSession)
    session.read_resource.return_value = types.ReadResourceResult(
        contents=[
            types.TextResourceContents(uri=uri, mime_type=mime_type, text=text)
            for uri, mime_type, text in contents
        ]
    )
    return Client(cast(ClientSession, session))


def test_resource_selects_the_json_content_of_two() -> None:
    client = resource_client(
        ("rift://map", "text/plain", "map 3f9a1c2e\n"),
        ("rift://map", "application/json", '{"revision": 1}'),
    )
    assert asyncio.run(client.resource("rift://map")) == {"revision": 1}


def test_resource_reads_one_json_content() -> None:
    client = resource_client(("rift://map", "application/json", '{"revision": 1}'))
    assert asyncio.run(client.resource("rift://map")) == {"revision": 1}


@pytest.mark.parametrize(
    "contents",
    [
        [("rift://map", "text/plain", "a"), ("rift://map", "text/plain", "b")],
        [("rift://map", "text/plain", "a")],
        [("rift://map", "application/json", "{}"), ("rift://map", "text/html", "a")],
        [("rift://map", "application/json", "{}")] * 2,
        [("rift://map", "application/json", "{}")] * 3,
    ],
)
def test_resource_without_exactly_one_json_content_fails(
    contents: list[tuple[str, str, str]],
) -> None:
    with pytest.raises(AssertionError):
        asyncio.run(resource_client(*contents).resource("rift://map"))


def test_resource_rejects_wrong_uri() -> None:
    client = resource_client(("rift://map", "application/json", "{}"))
    with pytest.raises(AssertionError, match="resource returned"):
        asyncio.run(client.resource("rift://logs"))


def test_resource_rejects_wrong_uri_on_the_text_content() -> None:
    client = resource_client(
        ("rift://logs", "text/plain", "a"),
        ("rift://map", "application/json", "{}"),
    )
    with pytest.raises(AssertionError, match="resource returned"):
        asyncio.run(client.resource("rift://map"))


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
    notice = cut_notice("server output", LOG_BYTES_MAX)
    retained = server.log_path.read_bytes()
    assert len(retained) == LOG_BYTES_MAX + len(notice)
    assert retained.endswith(notice.encode())
    assert server.output_cut
    assert server.read_log().endswith(notice)


def test_tool_error_cannot_satisfy_a_read() -> None:
    client = client_with_result(
        failed_result("1 error", "\tinternal_error · retry never", "\t\toops")
    )
    with pytest.raises(ToolFailure, match="oops"):
        asyncio.run(client.call("search", {}))
    with pytest.raises(AssertionError, match="unexercised tools.*search"):
        client.require_complete({"search"})


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
    notice = cut_notice("SDK stderr", LOG_BYTES_MAX)
    retained = path.read_bytes()
    assert len(retained) == LOG_BYTES_MAX + len(notice)
    assert retained.endswith(notice.encode())


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


def test_server_sets_its_log_filter_and_never_inherits_rust_log(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("RUST_LOG", "trace")
    server = Server(tmp_path / "rift", tmp_path / "workspace", tmp_path / "server.log")
    assert server.env["RUST_LOG"] == LOG_FILTER
    explicit = Server(
        tmp_path / "rift",
        tmp_path / "workspace",
        tmp_path / "server.log",
        env={"RUST_LOG": "rift=error"},
    )
    assert explicit.env["RUST_LOG"] == "rift=error"


RECORDS_BINARY = """
import os, sys
with open(os.environ["RECORD_ARGUMENTS"], "w") as seen:
    seen.write(" ".join(sys.argv[1:]) + "|" + os.getcwd() + "|" + os.environ["RUST_LOG"])
print("2026-10-05T09:00:00.000Z ERROR index build failed")
"""


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_records_read_the_persisted_log_without_a_server(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    seen = tmp_path / "arguments"
    monkeypatch.setenv("RECORD_ARGUMENTS", str(seen))
    server = Server(
        fake_binary(tmp_path, RECORDS_BINARY), root, tmp_path / "server.log"
    )
    text = server.records()
    assert seen.read_text() == f"server logs --tail 5000|{root.resolve()}|{LOG_FILTER}"
    assert "ERROR index build failed" in text
    assert text.startswith(f"persisted log records ({server.records_path}):")
    assert "ERROR index build failed" in server.records_path.read_text()
    assert server.records_path == tmp_path / "server.records.log"


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_records_failure_is_text_and_never_raises(tmp_path: Path) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    binary = fake_binary(tmp_path, "import sys\nsys.exit(3)\n")
    server = Server(binary, root, tmp_path / "server.log")
    assert server.records().startswith(
        "persisted log records unavailable: rift exited 3"
    )
    missing = Server(tmp_path / "absent", root, tmp_path / "other.log")
    assert missing.records().startswith("persisted log records unavailable:")


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_evidence_keeps_server_stderr_proxy_stderr_and_records(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    monkeypatch.setenv("RECORD_ARGUMENTS", str(tmp_path / "arguments"))
    server = Server(
        fake_binary(tmp_path, RECORDS_BINARY), root, tmp_path / "server.log"
    )
    server.log_path.write_text("server: bound 127.0.0.1\n", encoding="utf-8")
    proxy = server.log_path.with_suffix(".mcp.log")
    proxy.write_text("proxy: waiting for lock\n", encoding="utf-8")
    server._proxy_logs.append(proxy)
    notes = server.evidence()
    assert notes[0] == f"server stderr ({server.log_path}):\nserver: bound 127.0.0.1\n"
    assert notes[1] == f"rift mcp stderr ({proxy}):\nproxy: waiting for lock\n"
    assert "ERROR index build failed" in notes[2]
    assert notes[-1].startswith("machine: logical_cpus=")
    assert server.proxy_logs == (proxy,)


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_evidence_falls_back_to_server_stderr_after_a_store_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from rift_dev.rift_test_client import STORE_FALLBACK, STORE_REFUSAL

    root = tmp_path / "workspace"
    root.mkdir()
    monkeypatch.setenv("RECORD_ARGUMENTS", str(tmp_path / "arguments"))
    server = Server(
        fake_binary(tmp_path, RECORDS_BINARY), root, tmp_path / "server.log"
    )
    refusal = f"{STORE_REFUSAL} of 3; the log drain keeps it: database is locked\n"
    server.log_path.write_text(f"server: bound\n{refusal}", encoding="utf-8")
    notes = server.evidence()
    assert notes[0] == (
        f"server stderr ({server.log_path}):\n{STORE_FALLBACK}server: bound\n{refusal}"
    )
    server.log_path.write_text(f"server: quoted {refusal}", encoding="utf-8")
    assert STORE_FALLBACK not in server.evidence()[0]


def test_evidence_tail_names_the_file_holding_the_rest(tmp_path: Path) -> None:
    from rift_dev.rift_test_client import EVIDENCE_TAIL_BYTES, tail_text

    path = tmp_path / "server.log"
    text = "a" * 10 + "b" * EVIDENCE_TAIL_BYTES
    kept = tail_text(text, path)
    assert kept.startswith(f"[10 earlier bytes are in {path}]\n")
    assert kept.endswith("b" * EVIDENCE_TAIL_BYTES)
    assert tail_text("short", path) == "short"
    assert tail_text(text).startswith("[10 earlier bytes were left out]\n")


BIG_RECORDS_BINARY = """
print("x" * 1000 + "\\n", end="")
print("y" * (1024 * 1024) + "\\nnewest stop record")
"""


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_records_file_keeps_the_newest_bytes_and_states_the_cut(
    tmp_path: Path,
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    server = Server(
        fake_binary(tmp_path, BIG_RECORDS_BINARY), root, tmp_path / "server.log"
    )
    server.read_records()
    written = server.records_path.read_bytes()
    assert written.endswith(b"newest stop record\n")
    assert written.startswith(b"[")
    assert b"earlier bytes were left out]\n" in written[:80]
    assert len(written) <= RECORDS_FILE_BYTES + 80


@pytest.mark.skipif(os.name == "nt", reason="fixture executable uses a Unix shebang")
def test_read_records_raises_on_failure_for_the_caller_to_note(
    tmp_path: Path,
) -> None:
    root = tmp_path / "workspace"
    root.mkdir()
    server = Server(
        fake_binary(tmp_path, "import sys\nsys.exit(3)\n"), root, tmp_path / "s.log"
    )
    with pytest.raises(RuntimeError, match="rift exited 3"):
        server.read_records()


def test_tool_error_keeps_the_registered_identity_apart_from_diagnostics() -> None:
    failure = parse_failure(
        "search",
        "1 error\n\tinvalid_request · retry never\n"
        "\t\tthe request does not match the documented form\n"
        "\t\terror[E0308] src/lib.rs:3:5: mismatched types\n"
        "\t\trift.server.read_invalid\n",
    )
    assert failure.identity == "rift.server.read_invalid"
    assert failure.diagnostics == ["error[E0308] src/lib.rs:3:5: mismatched types"]
    unregistered = parse_failure(
        "search", "1 error\n\tinternal_error · retry never\n\t\tbad request\n"
    )
    assert unregistered.identity == ""
