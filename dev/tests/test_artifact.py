"""Artifact inputs must preserve supplied executable and fixture source bytes."""

import asyncio
from pathlib import Path
from typing import cast
from unittest.mock import AsyncMock, call

import pytest
from rift_dev.check_artifact import CONFIGURATION, SOURCE, lay_out_workspace
from rift_dev.rift_test_client import Client, JsonObject, gate_deadline


def test_supplied_binary_is_never_built(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from rift_dev import rift_test_client

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


@pytest.mark.parametrize("published", [True, False])
def test_external_change_requires_fresh_source(tmp_path: Path, published: bool) -> None:
    from contextlib import nullcontext

    from rift_dev.check_artifact import check_external_change

    lay_out_workspace(tmp_path)
    client = AsyncMock(spec=Client)
    body = (
        "pub fn beacon_one() -> u8 { 3 }"
        if published
        else "pub fn beacon_one() -> u8 { 1 }"
    )
    client.call.return_value = {
        "hits": [{"symbol": {"name": "beacon_one"}, "source": body}]
    }
    with (
        nullcontext()
        if published
        else pytest.raises(AssertionError, match="absent from reads")
    ):
        asyncio.run(check_external_change(cast(Client, client), tmp_path))
    assert (tmp_path / "lib.rs").read_bytes() == SOURCE.replace(
        "{ 1 }", "{ 3 }"
    ).encode()


def test_reads_require_settled_search_then_exact_declaration_source(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Issue #493: a partial answer cannot prove the artifact's complete read."""
    from rift_dev import check_artifact

    preparing: JsonObject = {
        "results": [],
        "warnings": [
            {"code": "local_index_preparing", "prepared": 0, "total": 3},
            {"code": "lexical_ranking_unavailable"},
        ],
    }
    results: JsonObject = {"results": [{"symbol": {"name": "beacon_one"}}]}
    client = AsyncMock(spec=Client)
    client.call.side_effect = [
        preparing,
        {**preparing, **results},
        {**results, "warnings": [{"code": "lexical_ranking_unavailable"}]},
        {
            "hits": [
                {
                    "symbol": {"name": "beacon_one"},
                    "source": "pub fn beacon_one() -> u8 { 1 }",
                }
            ]
        },
    ]
    monkeypatch.setattr(check_artifact, "POLL_SECONDS", 0)
    asyncio.run(check_artifact.check_reads(cast(Client, client)))
    assert client.call.await_args_list == [
        *[call("search", {"query": "beacon_one", "target": "symbol"})] * 3,
        call("get_symbol", {"name": "beacon_one", "language": "rust"}),
    ]


@pytest.mark.parametrize("missing", [True, False])
def test_settled_reads_still_reject_missing_search_or_wrong_source(
    missing: bool,
) -> None:
    from rift_dev.check_artifact import check_reads

    client = AsyncMock(spec=Client)
    client.call.side_effect = [
        {"results": [] if missing else [{"symbol": {"name": "beacon_one"}}]},
        {
            "hits": [
                {
                    "symbol": {"name": "beacon_one"},
                    "source": "pub fn beacon_one() -> u8 { 2 }",
                }
            ]
        },
    ]
    with pytest.raises(
        AssertionError,
        match="search missed beacon_one" if missing else "unexpected source",
    ):
        asyncio.run(check_reads(cast(Client, client)))
    assert client.call.await_count == (1 if missing else 2)


def test_preparing_reads_preserve_tool_refusal(monkeypatch: pytest.MonkeyPatch) -> None:
    from rift_dev import check_artifact

    refusal = AssertionError("search reported a tool error")
    client = AsyncMock(spec=Client)
    client.call.side_effect = [
        {"warnings": [{"code": "local_index_preparing", "prepared": 0}]},
        refusal,
    ]
    monkeypatch.setattr(check_artifact, "POLL_SECONDS", 0)
    with pytest.raises(AssertionError) as caught:
        asyncio.run(check_artifact.check_reads(cast(Client, client)))
    assert caught.value is refusal
    assert (
        client.call.await_args_list
        == [call("search", {"query": "beacon_one", "target": "symbol"})] * 2
    )


@pytest.mark.parametrize("blocked", [True, False])
def test_preparing_artifact_reads_keep_one_inherited_deadline(
    monkeypatch: pytest.MonkeyPatch, blocked: bool
) -> None:
    from rift_dev import check_artifact

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
        async with gate_deadline("artifact preparation fixture", 0.02):
            await check_artifact.check_reads(cast(Client, client))

    client.call.side_effect = read
    monkeypatch.setattr(check_artifact, "POLL_SECONDS", 0)
    with pytest.raises(TimeoutError):
        asyncio.run(exercise())
    if blocked:
        assert cancelled
        assert client.call.await_count == 1
    else:
        assert client.call.await_count > 1
