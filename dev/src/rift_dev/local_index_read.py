"""Repeat partial local reads under the caller's existing deadline."""

from __future__ import annotations

import asyncio

from rift_dev.corpus_assertions import warnings
from rift_dev.rift_test_client import Client, JsonObject, gate_deadline


def preparing_local(answer: JsonObject) -> bool:
    """Recognize local preparation without treating other refusals as empty answers.

    >>> preparing_local({"outcome": "unavailable", "reason": "index_preparing"})
    True
    >>> preparing_local({"outcome": "unavailable", "reason": "insufficient_coverage"})
    False
    >>> preparing_local({"warnings": [{"code": "local_index_preparing"}]})
    True
    >>> preparing_local({"answer": {"result": "bindings", "bindings": []}})
    False
    >>> preparing_local({"outcome": "missing"})
    False
    """
    if answer.get("outcome") == "unavailable":
        return answer.get("reason") == "index_preparing"
    return any(
        warning.get("code") == "local_index_preparing" for warning in warnings(answer)
    )


async def settled_local(
    client: Client,
    name: str,
    request: JsonObject,
    *,
    seconds: float,
    poll_seconds: float,
) -> JsonObject:
    """Resend one read until local index preparation completes.

    A `local_index_preparing` answer covers only prepared files. A query can
    instead refuse with `index_preparing` until its required facts are ready. One inherited
    deadline bounds every call and poll together; a settled answer keeps the
    caller's result checks and every other warning.
    """
    async with gate_deadline("local index preparation", seconds):
        while True:
            answer = await client.call(name, request)
            if not preparing_local(answer):
                return answer
            await asyncio.sleep(poll_seconds)
