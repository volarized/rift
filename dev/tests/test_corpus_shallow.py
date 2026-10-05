"""Check shallow history only after the local index finishes preparation."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, call, patch

import pytest
from rift_dev.check_corpus import Corpus
from rift_dev.corpus_cache import Pin, pins
from rift_dev.rift_test_client import Client, JsonObject, Server, object_value


@pytest.mark.parametrize("outcome", ["shallow", "absent", "complete"])
def test_shallow_history_preserves_declaration_and_history_checks_after_preparation(
    tmp_path: Path, outcome: str
) -> None:
    preparing: JsonObject = {
        "hits": [],
        "warnings": [{"code": "local_index_preparing", "prepared": 0}],
    }
    settled: JsonObject = {
        "hits": (
            []
            if outcome == "absent"
            else [{"history": {"complete": outcome == "complete"}}]
        )
    }
    client = AsyncMock(spec=Client)
    client.call.side_effect = [preparing, settled]
    client.resource.return_value = {"records": []}
    server = MagicMock(spec=Server)
    server.root = Path("workspace")
    server.log_path = Path("server.log")
    server.__enter__.return_value = server
    server.connect.return_value.__aenter__.return_value = client
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", tmp_path / "fastapi.json")
    root = tmp_path / "shallow"
    failure = {
        "absent": "shallow checkout has no FastAPI declaration",
        "complete": "shallow history must report complete=false",
    }

    async def exercise() -> None:
        if outcome in failure:
            with pytest.raises(AssertionError, match=failure[outcome]):
                await corpus.shallow(root)
        else:
            await corpus.shallow(root)

    with (
        patch.object(Pin, "checkout") as checkout,
        patch.object(corpus, "configure") as configure,
        patch.object(corpus, "server", return_value=server),
    ):
        asyncio.run(exercise())
    checkout.assert_called_once_with(root, depth=1)
    configure.assert_called_once_with(root=root)
    request = {"name": "FastAPI", "include": ["history"], "limit": 5}
    assert client.call.await_args_list == [
        call("get_symbol", request),
        call("get_symbol", request),
    ]
    if outcome == "shallow":
        server.stop.assert_called_once()
        recorded = object_value(corpus.actions[-1], "shallow action")
        assert recorded["complete"] is False
        assert recorded["depth"] == 1
    else:
        assert not corpus.actions
