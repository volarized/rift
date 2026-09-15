"""Require stop observations to remain independent of queued log writes."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from check_corpus import Corpus
from corpus_cache import pins
from rift_test_client import Client, JsonObject, Server, object_value


@pytest.mark.parametrize("operation", ["rebuild", "history"])
@pytest.mark.parametrize("completed", [False, True])
def test_stop_observes_active_work_while_log_writes_are_pending(
    tmp_path: Path, operation: str, completed: bool
) -> None:
    async def exercise() -> None:
        startup = 'INFO index snapshot published operation="index.publish" trigger="startup" epoch=0\n'
        started = (
            'DEBUG index capture started operation="index.build" phase="start" epoch=1\n'
            if operation == "rebuild"
            else 'DEBUG symbol history started operation="get_symbol" phase="start"\n'
        )
        output = startup
        cancelled = asyncio.Event()

        async def request(_name: str, _arguments: JsonObject) -> JsonObject:
            nonlocal output
            output += started
            if completed:
                output += (
                    'INFO index.build{epoch=1}: rift_mcp::validation: close\n'
                    if operation == "rebuild"
                    else 'DEBUG get_symbol{phase="history"}: rift_server::history: close\n'
                )
                return {}
            try:
                await asyncio.Future[None]()
            finally:
                cancelled.set()
            raise AssertionError("the request must remain active until stop")

        client = AsyncMock(spec=Client)
        client.call.side_effect = request
        client.resource.side_effect = AssertionError(
            "the lexical transaction still owns the log store's write turn"
        )
        server = MagicMock(spec=Server)
        server.__enter__.return_value = server
        server.connect.return_value.__aenter__.return_value = client
        server.read_log.side_effect = lambda: output
        server.log_path = tmp_path / "server.log"
        corpus = Corpus(pins()["bun"], tmp_path / "rift", tmp_path / "bun.json", "stop")
        corpus.root = tmp_path
        with patch.object(corpus, "server", return_value=server):
            if completed:
                with pytest.raises(AssertionError, match="completed before stop"):
                    await corpus.stop_during(operation)
                server.stop.assert_not_called()
                assert not corpus.actions
                return
            await corpus.stop_during(operation)
        server.stop.assert_called_once()
        client.resource.assert_not_called()
        assert cancelled.is_set()
        recorded = object_value(corpus.actions[-1], "stop action")
        assert recorded["stderr"] == started.strip()
        assert recorded["process_gone"] is True

    asyncio.run(exercise())
