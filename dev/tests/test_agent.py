"""Agent checks require resolved references and preserve the source fixture."""

import asyncio
from contextlib import nullcontext
from pathlib import Path
from typing import cast
from unittest.mock import AsyncMock

import pytest
from rift_dev.check_agent import PYTHON_SOURCE, check_embedded_references
from rift_dev.rift_test_client import Client, JsonObject


@pytest.mark.parametrize("derivation", ["resolution", "syntax"])
def test_embedded_reference_check_requires_engine_resolution(
    tmp_path: Path, derivation: str
) -> None:
    (tmp_path / "service.py").write_bytes(PYTHON_SOURCE.encode())
    seed = "rift://symbol/python/service.py/serve"
    caller = "rift://symbol/python/service.py/caller"
    client = AsyncMock(spec=Client)

    async def call(name: str, arguments: JsonObject) -> JsonObject:
        if name == "get_symbol":
            selected = arguments["name"]
            return {
                "hits": [
                    {
                        "symbol": {
                            "name": selected,
                            "id": seed if selected == "serve" else caller,
                        }
                    }
                ]
            }
        assert name == "search"
        return {
            "results": [
                {
                    "hit": {"symbol": {"id": caller}},
                    "traversal_path": [
                        {
                            "direction": "incoming",
                            "relationship": {
                                "from": caller,
                                "to": seed,
                                "facets": ["references"],
                                "derivation": derivation,
                            },
                        }
                    ],
                }
            ]
        }

    client.call.side_effect = call
    with (
        nullcontext()
        if derivation == "resolution"
        else pytest.raises(AssertionError, match="not resolved by the engine")
    ):
        asyncio.run(check_embedded_references(cast(Client, client), tmp_path))
    assert (tmp_path / "service.py").read_bytes() == PYTHON_SOURCE.encode()
