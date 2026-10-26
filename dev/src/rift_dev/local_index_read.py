"""Repeat partial local reads under the caller's existing deadline."""

from __future__ import annotations

import asyncio
from typing import Literal

from rift_dev.corpus_assertions import warnings
from rift_dev.rift_test_client import Client, JsonObject, ToolFailure, gate_deadline


def preparing_local(
    answer: JsonObject, *, selection: Literal["current", "captured"] = "captured"
) -> bool:
    """Recognize current local preparation without retrying captured coverage.

    >>> preparing_local({"outcome": "unavailable", "reason": "index_preparing"}, selection="current")
    True
    >>> preparing_local({"outcome": "unavailable", "reason": "insufficient_coverage"}, selection="current")
    False
    >>> preparing_local({"warnings": [{"code": "local_index_preparing"}]}, selection="current")
    True
    >>> preparing_local({"answer": {"result": "bindings", "bindings": []}}, selection="current")
    False
    >>> preparing_local({"outcome": "missing"}, selection="current")
    False
    >>> preparing_local({"outcome": "unavailable", "reason": "index_preparing"})
    False
    """
    if selection != "current":
        return False
    observed = warnings(answer)
    accepted = ("local_index_preparing", "lexical_ranking_unavailable")
    if any(warning.get("code") not in accepted for warning in observed):
        return False
    if answer.get("outcome") == "unavailable":
        if answer.get("reason") == "index_preparing":
            return True
        if answer.get("reason") != "insufficient_coverage":
            return False
    return any(warning.get("code") == "local_index_preparing" for warning in observed)


def retry_local_failure(failure: ToolFailure) -> bool:
    """Recognize registered failures that permit resending the same request.

    >>> failure = ToolFailure("search", "read", "temporarily_unavailable", "read", "same_request")
    >>> retry_local_failure(failure)
    True
    >>> failure.retry = "correct_input"
    >>> retry_local_failure(failure)
    False
    >>> failure.retry = "same_request"
    >>> failure.code = "capability_unavailable"
    >>> retry_local_failure(failure)
    False
    >>> failure.code = "invalid_request"
    >>> retry_local_failure(failure)
    False
    """
    return failure.code == "temporarily_unavailable" and failure.retry == "same_request"


async def settled_local(
    client: Client,
    name: str,
    request: JsonObject,
    *,
    seconds: float,
    poll_seconds: float,
    selection: Literal["current", "captured"] = "captured",
) -> JsonObject:
    """Resend an eligible current read until local index preparation completes.

    Callers name the original selection; the helper does not parse query text.
    A top-level captured view always keeps captured behavior. Current preparation
    may use `index_preparing`, or `insufficient_coverage` with the local preparation
    warning and its lexical preparation warning. Other coverage failures return
    unchanged. One inherited deadline bounds every call and poll together.

    The actual callback sequence retains typed refusals until current preparation
    completes, then preserves the strict SELECT answer.

    >>> from types import SimpleNamespace
    >>> from typing import cast
    >>> from unittest.mock import AsyncMock
    >>> from rift_dev.query import bindings
    >>> pending = {'outcome': 'unavailable', 'reason': 'insufficient_coverage', 'view': {'id': '8bf7a1164e666a3553f4f67da9c82e1f3966f32ce1b5c20128d0f55840187d68', 'expires_at': '2026-10-25T23:14:44.867513397+00:00'}, 'warnings': [{'code': 'local_index_preparing', 'prepared': 0, 'total': 3138, 'detail': 'selected local files are still being prepared'}, {'code': 'lexical_ranking_unavailable', 'detail': 'local files are still being prepared, so the answer was ranked by identifier matching alone'}]}
    >>> ready = {"answer": {"result": "bindings", "variables": ["symbol"], "bindings": [{"symbol": {"type": "blank_node", "value": "original"}}]}}
    >>> async def read(sequence, *, selection="captured", request=None):
    ...     call = AsyncMock(side_effect=sequence)
    ...     client = cast(Client, SimpleNamespace(call=call))
    ...     answer = await settled_local(client, "search", request or {"query": "SELECT ?symbol { ?symbol a :Symbol }"}, seconds=1.0, poll_seconds=0, selection=selection)
    ...     return answer, call.call_count
    >>> answer, count = asyncio.run(read([pending, ready], selection="current"))
    >>> bindings(answer)[0]["symbol"]["type"], count
    ('blank_node', 2)
    >>> answer, count = asyncio.run(read([pending], selection="captured"))
    >>> answer is pending, count
    (True, 1)
    >>> answer, count = asyncio.run(read([pending], selection="current", request={"view": "0" * 64}))
    >>> answer is pending, count
    (True, 1)
    >>> answer, count = asyncio.run(read([pending], request={"query": "SELECT ?symbol { SERVICE <urn:rift:result:" + "0" * 64 + "> {} }"}))
    >>> answer is pending, count
    (True, 1)
    >>> refused = {"outcome": "unavailable", "reason": "insufficient_coverage"}
    >>> answer, count = asyncio.run(read([refused], selection="current"))
    >>> answer is refused, count
    (True, 1)
    >>> mixed = {**pending, "warnings": [*pending["warnings"], {"code": "global_response_invalid"}]}
    >>> answer, count = asyncio.run(read([mixed], selection="current"))
    >>> answer is mixed, count
    (True, 1)
    >>> direct_mixed = {**mixed, "reason": "index_preparing"}
    >>> answer, count = asyncio.run(read([direct_mixed], selection="current"))
    >>> answer is direct_mixed, count
    (True, 1)
    >>> empty = {"answer": {"result": "bindings", "variables": [], "bindings": []}}
    >>> answer, count = asyncio.run(read([empty], selection="current"))
    >>> bindings(answer), count
    ([], 1)
    >>> for reason in ("view_context", "view_expired"):
    ...     answer, count = asyncio.run(read([{**pending, "reason": reason}], selection="current"))
    ...     assert answer["reason"] == reason and count == 1
    >>> try:
    ...     bindings(answer)
    ... except AssertionError:
    ...     print("typed refusal is not bindings")
    typed refusal is not bindings
    >>> direct = {"outcome": "unavailable", "reason": "index_preparing"}
    >>> answer, count = asyncio.run(read([direct, ready], selection="current"))
    >>> answer is ready, count
    (True, 2)
    >>> failure = ToolFailure("search", "read", "temporarily_unavailable", "read", "same_request")
    >>> answer, count = asyncio.run(read([failure, ready], selection="current"))
    >>> answer is ready, count
    (True, 2)
    >>> for failure in (ToolFailure("search", "read", "capability_unavailable", "read", "operator_action"), ToolFailure("search", "read", "invalid_request", "read", "correct_input")):
    ...     call = AsyncMock(side_effect=failure)
    ...     try:
    ...         asyncio.run(settled_local(cast(Client, SimpleNamespace(call=call)), "search", {}, seconds=1.0, poll_seconds=0, selection="current"))
    ...     except ToolFailure as actual:
    ...         assert actual is failure and call.call_count == 1

    >>> call = AsyncMock(side_effect=ToolFailure("search", "read", "temporarily_unavailable", "read", "same_request"))
    >>> try:
    ...     asyncio.run(settled_local(cast(Client, SimpleNamespace(call=call)), "search", {}, seconds=1.0, poll_seconds=0))
    ... except ToolFailure:
    ...     assert call.call_count == 1

    Calls and polling retain the earlier caller deadline; cancellation reaches the
    actual callback instead of being treated as a retryable refusal.

    >>> async def inherited_deadline():
    ...     call = AsyncMock(return_value=pending)
    ...     try:
    ...         async with gate_deadline("outer", 0.01):
    ...             await settled_local(cast(Client, SimpleNamespace(call=call)), "search", {}, seconds=1.0, poll_seconds=1.0, selection="current")
    ...     except TimeoutError:
    ...         return call.call_count
    >>> asyncio.run(inherited_deadline())
    1
    >>> async def callback_deadline():
    ...     ended = asyncio.Event()
    ...     async def operation(*_):
    ...         try:
    ...             await asyncio.Future()
    ...         finally:
    ...             ended.set()
    ...     call = AsyncMock(side_effect=operation)
    ...     try:
    ...         await settled_local(cast(Client, SimpleNamespace(call=call)), "search", {}, seconds=0.01, poll_seconds=0, selection="current")
    ...     except TimeoutError:
    ...         return ended.is_set(), call.call_count
    >>> asyncio.run(callback_deadline())
    (True, 1)
    >>> async def cancelled_callback():
    ...     started, ended = asyncio.Event(), asyncio.Event()
    ...     async def operation(*_):
    ...         started.set()
    ...         try:
    ...             await asyncio.Future()
    ...         finally:
    ...             ended.set()
    ...     call = AsyncMock(side_effect=operation)
    ...     task = asyncio.create_task(settled_local(cast(Client, SimpleNamespace(call=call)), "search", {}, seconds=1.0, poll_seconds=0, selection="current"))
    ...     await started.wait()
    ...     task.cancel()
    ...     try:
    ...         await task
    ...     except asyncio.CancelledError:
    ...         return ended.is_set(), call.call_count
    >>> asyncio.run(cancelled_callback())
    (True, 1)
    """
    if request.get("view") is not None:
        selection = "captured"
    async with gate_deadline("local index preparation", seconds):
        while True:
            try:
                answer = await client.call(name, request)
            except ToolFailure as failure:
                if selection != "current" or not retry_local_failure(failure):
                    raise
            else:
                if not preparing_local(answer, selection=selection):
                    return answer
            await asyncio.sleep(poll_seconds)
