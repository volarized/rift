"""Require stop observations to remain independent of queued log writes."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from rift_dev.check_corpus import Corpus
from rift_dev.corpus_assertions import PROBE_PATH, PROBE_SOURCE
from rift_dev.corpus_cache import git, pins
from rift_dev.rift_test_client import Server, object_value

STARTUP = 'INFO index snapshot published operation="index.publish" trigger="startup" epoch=0\n'
BATCH_SPAN = 'history.batch{component="history" operation="history.batch"}'
BATCH_CLOSE = (
    f"INFO {BATCH_SPAN}: rift_mcp::history: close time.busy=1ms time.idle=2s\n"
)
ANALYZE_CLOSE = (
    f'INFO {BATCH_SPAN}:history.analyze{{component="history" operation="history.analyze"}}:'
    " rift_mcp::history: close time.busy=1ms\n"
)


def batch_start(pending: int) -> str:
    return (
        f"DEBUG {BATCH_SPAN}: rift_mcp::history: history batch started "
        f'component="history" operation="history.batch" phase="start" pending={pending}\n'
    )


@pytest.mark.parametrize("completed", [False, True])
def test_rebuild_stop_observes_filesystem_work_without_a_reconnecting_proxy(
    tmp_path: Path, completed: bool
) -> None:
    """The stop observer cannot start a replacement server through its proxy (#505)."""

    async def exercise() -> None:
        started = 'DEBUG index capture started component="index" operation="index.build" phase="start" epoch=1\n'
        observed = False

        def read_log() -> str:
            nonlocal observed
            if not (tmp_path / PROBE_PATH).exists():
                return STARTUP
            assert (tmp_path / PROBE_PATH).read_text() == PROBE_SOURCE
            observed = True
            output = STARTUP + started
            if completed:
                output += "INFO index.build{epoch=1}: rift_mcp::validation: close\n"
            return output

        def stop() -> None:
            assert observed, "the rebuild must be observed before stop"

        server = MagicMock(spec=Server)
        server.__enter__.return_value = server
        server.connect.side_effect = AssertionError(
            "a rebuild stop needs no proxy request"
        )
        server.read_log.side_effect = read_log
        server.stop.side_effect = stop
        server.log_path = tmp_path / "server.log"
        corpus = Corpus(pins()["bun"], tmp_path / "rift", tmp_path / "bun.json", "stop")
        corpus.root = tmp_path
        with patch.object(corpus, "server", return_value=server):
            if completed:
                with pytest.raises(AssertionError, match="completed before stop"):
                    await corpus.stop_during_rebuild()
                server.stop.assert_not_called()
                server.connect.assert_not_called()
                assert not corpus.actions
                return
            await corpus.stop_during_rebuild()
        server.stop.assert_called_once()
        server.connect.assert_not_called()
        assert not (tmp_path / PROBE_PATH).exists()
        recorded = object_value(corpus.actions[-1], "stop action")
        assert recorded["stderr"] == started.strip()
        assert recorded["process_gone"] is True

    asyncio.run(exercise())


def fill_server(tmp_path: Path, outputs: list[str]) -> MagicMock:
    """A server whose output reads `outputs` in turn, then keeps the last one."""
    reads = iter(outputs)
    current = outputs[0]

    def read_log() -> str:
        nonlocal current
        current = next(reads, current)
        return current

    server = MagicMock(spec=Server)
    server.__enter__.return_value = server
    server.connect.side_effect = AssertionError("a history fill needs no request")
    server.read_log.side_effect = read_log
    server.log_path = tmp_path / "server.log"
    return server


def filled_corpus(tmp_path: Path) -> Corpus:
    """A stop corpus over a repository whose history store earlier servers filled."""
    git(tmp_path, "init", "--quiet").output_bytes()
    store = tmp_path / ".git" / ".rift"
    store.mkdir()
    (store / "store-filled.db").write_bytes(b"filled")
    corpus = Corpus(pins()["bun"], tmp_path / "rift", tmp_path / "bun.json", "stop")
    corpus.root = tmp_path
    return corpus


def test_history_stop_waits_until_a_batch_with_pending_commits_is_open(
    tmp_path: Path,
) -> None:
    finished = STARTUP + batch_start(3) + ANALYZE_CLOSE + BATCH_CLOSE
    unfinished = finished + batch_start(2) + ANALYZE_CLOSE
    server = fill_server(tmp_path, [STARTUP, finished, finished, unfinished])
    corpus = filled_corpus(tmp_path)
    store_at_start: list[bool] = []

    def started() -> MagicMock:
        store_at_start.append((tmp_path / ".git" / ".rift").exists())
        return server

    with (
        patch.object(corpus, "server", side_effect=started),
        patch("rift_dev.check_corpus.POLL_SECONDS", 0.001),
    ):
        asyncio.run(corpus.stop_during_history_fill())

    assert store_at_start == [False], "the server starts over an empty history store"
    server.stop.assert_called_once()
    server.connect.assert_not_called()
    recorded = object_value(corpus.actions[-1], "stop action")
    assert recorded["state"] == "mid_history"
    assert recorded["stderr"] == batch_start(2).strip()
    assert recorded["process_gone"] is True


@pytest.mark.parametrize("startup_published", [False, True])
def test_history_stop_observes_a_pending_batch_before_source_publication(
    tmp_path: Path, startup_published: bool
) -> None:
    output = batch_start(3) + ANALYZE_CLOSE
    if startup_published:
        output += STARTUP
    server = fill_server(tmp_path, [output])
    corpus = filled_corpus(tmp_path)

    with (
        patch.object(corpus, "server", return_value=server),
        patch("rift_dev.check_corpus.POLL_SECONDS", 0.001),
        patch("rift_dev.check_corpus.OBSERVATION_SECONDS", 0.05),
    ):
        asyncio.run(corpus.stop_during_history_fill())

    server.stop.assert_called_once()
    server.connect.assert_not_called()
    recorded = object_value(corpus.actions[-1], "stop action")
    assert recorded["state"] == "mid_history"
    assert recorded["stderr"] == batch_start(3).strip()
    assert recorded["process_gone"] is True


@pytest.mark.parametrize(
    "outputs",
    [
        [STARTUP + batch_start(3) + BATCH_CLOSE],
        [STARTUP + batch_start(3) + BATCH_CLOSE + batch_start(0)],
        [batch_start(3) + BATCH_CLOSE + STARTUP],
        [STARTUP, STARTUP + batch_start(3).rstrip("\n")],
    ],
    ids=["closed", "drained", "closed_before_startup", "partial_record"],
)
def test_history_stop_refuses_to_stop_without_an_open_batch(
    tmp_path: Path, outputs: list[str]
) -> None:
    server = fill_server(tmp_path, outputs)
    corpus = filled_corpus(tmp_path)

    with (
        patch.object(corpus, "server", return_value=server),
        patch("rift_dev.check_corpus.POLL_SECONDS", 0.001),
        patch("rift_dev.check_corpus.OBSERVATION_SECONDS", 0.05),
        pytest.raises(AssertionError, match="a history store batch with pending"),
    ):
        asyncio.run(corpus.stop_during_history_fill())

    server.stop.assert_not_called()
    assert not corpus.actions
