"""Check corpus reads against partial preparation and the shared deadline."""

import asyncio
from pathlib import Path
from typing import cast
from unittest.mock import AsyncMock, MagicMock, call, patch

import pytest
from rift_dev import check_corpus
from rift_dev.check_corpus import Corpus, settled_local
from rift_dev.commands import Process
from rift_dev.corpus_assertions import CONTEXT_SPAN, SYMBOL_COUNT
from rift_dev.corpus_cache import pins
from rift_dev.rift_test_client import (
    Client,
    JsonObject,
    Server,
    gate_deadline,
    object_value,
)


@pytest.mark.parametrize("name", ["bun", "fastapi", "nextjs"])
@pytest.mark.parametrize("complete", [True, False])
def test_baseline_checks_symbol_floor_only_after_preparation(
    tmp_path: Path, name: str, complete: bool
) -> None:
    candidates: list[JsonObject] = [{} for _ in range(SYMBOL_COUNT)]
    preparing: JsonObject = {
        "results": [],
        "warnings": [{"code": "local_index_preparing", "prepared": 0}],
    }
    settled_candidates = candidates if complete else candidates[:-1]
    answer: JsonObject = {"results": settled_candidates}
    client = AsyncMock(spec=Client)
    client.call.side_effect = (
        [preparing, {**preparing, "results": candidates}, answer]
        if complete
        else [answer]
    )
    client.resource.return_value = {
        "packages": [{"manager": "pypi"}],
        "records": [{"message": CONTEXT_SPAN}],
    }
    server = MagicMock(spec=Server)
    server.started_at = ""
    server.read_records.return_value = ""
    server.root = Path("workspace")
    server.log_path = Path("server.log")
    process = MagicMock(spec_set=Process)
    process.poll.return_value = 0
    server.process = process
    server.__enter__.return_value = server
    server.connect.return_value.__aenter__.return_value = client
    corpus = Corpus(pins()[name], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path

    async def exercise() -> None:
        if complete:
            await corpus.baseline()
        else:
            with pytest.raises(AssertionError, match=f"fewer than {SYMBOL_COUNT}"):
                await corpus.baseline()

    with (
        patch.object(corpus, "server", return_value=server),
        patch.object(corpus, "dependencies"),
        patch.object(corpus, "symbols", new_callable=AsyncMock) as symbols,
        patch.object(corpus, "lexical_persistence", new_callable=AsyncMock),
        patch.object(corpus, "oversized", new_callable=AsyncMock),
        patch.object(corpus, "unparsed", new_callable=AsyncMock),
        patch.object(corpus, "symlinks", new_callable=AsyncMock),
    ):
        asyncio.run(exercise())
    request = {
        "query": "test",
        "target": "symbol",
        "limit": 1000,
        "order": "identity",
    }
    assert client.call.await_args_list == [call("search", request)] * (
        3 if complete else 1
    )
    if complete:
        symbols.assert_awaited_once_with(client, settled_candidates)
        server.stop.assert_called_once()
        process.poll.assert_called_once_with()
        publication = object_value(corpus.actions[0], "publication action")
        assert publication["action"] == "publication"
        assert publication["symbols"] == SYMBOL_COUNT
        stopped = object_value(corpus.actions[-1], "stop action")
        assert stopped["action"] == "stop"
        assert stopped["state"] == "idle"
        assert stopped["process_gone"] is True
    else:
        symbols.assert_not_awaited()
        process.poll.assert_not_called()
        assert not corpus.actions


def test_preparation_preserves_other_warnings_and_tool_refusal() -> None:
    preparing: JsonObject = {
        "warnings": [{"code": "local_index_preparing", "prepared": 0}]
    }
    settled: JsonObject = {
        "results": [],
        "warnings": [{"code": "stale_index"}],
    }
    request: JsonObject = {"query": "test", "limit": 1}
    client = AsyncMock(spec=Client)
    client.call.side_effect = [preparing, settled]
    answer = asyncio.run(settled_local(cast(Client, client), "search", request))
    assert answer is settled
    assert client.call.await_args_list == [call("search", request)] * 2

    refusal = AssertionError("search reported a tool error")
    client.call.reset_mock()
    client.call.side_effect = [preparing, refusal]
    with pytest.raises(AssertionError) as caught:
        asyncio.run(settled_local(cast(Client, client), "search", request))
    assert caught.value is refusal
    assert client.call.await_args_list == [call("search", request)] * 2


@pytest.mark.parametrize("blocked", [True, False])
def test_preparation_shares_one_deadline_and_cancels_blocked_reads(
    monkeypatch: pytest.MonkeyPatch, blocked: bool
) -> None:
    client = AsyncMock(spec=Client)
    cancelled = False

    async def read(_name: str, _request: JsonObject) -> JsonObject:
        nonlocal cancelled
        if blocked:
            try:
                await asyncio.Future()
            except asyncio.CancelledError:
                cancelled = True
                raise
        return {"warnings": [{"code": "local_index_preparing", "prepared": 0}]}

    async def exercise() -> None:
        async with gate_deadline("corpus preparation fixture", 0.02):
            await settled_local(cast(Client, client), "search", {"query": "test"})

    client.call.side_effect = read
    monkeypatch.setattr(check_corpus, "POLL_SECONDS", 0)
    with pytest.raises(TimeoutError):
        asyncio.run(exercise())
    if blocked:
        assert cancelled
        assert client.call.await_count == 1
    else:
        assert client.call.await_count > 1
