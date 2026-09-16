"""Artifact inputs must preserve supplied executable and fixture source bytes."""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from check_artifact import CONFIGURATION, SOURCE, lay_out_workspace


def test_supplied_binary_is_never_built(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import rift_test_client

    binary = tmp_path / "rift"
    binary.write_bytes(b"release bytes")
    builder = AsyncMock()
    monkeypatch.setattr(rift_test_client, "build_server_binary", builder)
    assert rift_test_client.candidate_binary(binary) == binary.resolve()
    assert binary.read_bytes() == b"release bytes"
    builder.assert_not_called()
    with pytest.raises(AssertionError, match="cannot accompany"):
        rift_test_client.candidate_binary(binary, "aarch64-unknown-linux-gnu")
    builder.assert_not_called()


def test_workspace_preserves_lf_bytes_on_every_platform(tmp_path: Path) -> None:
    lay_out_workspace(tmp_path)
    assert (tmp_path / "lib.rs").read_bytes() == SOURCE.encode()
    assert (tmp_path / "rift.toml").read_bytes() == CONFIGURATION.encode()


def test_patch_gate_rejects_a_line_ending_rewrite(tmp_path: Path) -> None:
    import asyncio
    from typing import cast

    from check_artifact import check_reads_and_patch
    from rift_test_client import Client, JsonObject

    lay_out_workspace(tmp_path)
    client = AsyncMock(spec=Client)

    async def call(name: str, _arguments: JsonObject) -> JsonObject:
        if name == "search":
            return {"results": [{}]}
        if name == "get_symbol":
            return {
                "hits": [
                    {
                        "symbol": {"name": "beacon_one"},
                        "source": "pub fn beacon_one() -> u8 { 1 }",
                    }
                ]
            }
        if name == "patch":
            changed = SOURCE.replace("{ 1 }", "{ 3 }").replace("\n", "\r\n")
            (tmp_path / "lib.rs").write_bytes(changed.encode())
            return {"status": "applied"}
        raise AssertionError(f"unexpected tool: {name}")

    client.call.side_effect = call
    with pytest.raises(AssertionError, match="unexpected bytes"):
        asyncio.run(check_reads_and_patch(cast(Client, client), tmp_path))
