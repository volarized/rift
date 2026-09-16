"""Check report ownership and writer scheduling without a corpus server."""

import asyncio
from contextlib import nullcontext
from pathlib import Path
from typing import cast
from unittest.mock import AsyncMock

import pytest
from rift_dev.check_corpus import Corpus
from rift_dev.corpus_assertions import PROBE_PATH, READ_COUNT
from rift_dev.corpus_cache import pins
from rift_dev.rift_test_client import Client, JsonObject, object_value


def test_bun_cases_preserve_each_server_log(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    reports = tmp_path / "reports"
    reports.mkdir()
    paths: list[Path] = []
    for case in ("workspace", "stop"):
        corpus = Corpus(
            pins()["bun"], tmp_path / "rift", reports / f"bun.{case}.json", case
        )
        corpus.root = workspace
        for sequence in range(1, 3):
            server = corpus.server()
            assert server.log_path == reports / f"bun.{case}.server-{sequence}.log"
            server.log_path.write_text(f"{case}:{sequence}", encoding="utf-8")
            paths.append(server.log_path)
    assert [path.read_text(encoding="utf-8") for path in paths] == [
        "workspace:1",
        "workspace:2",
        "stop:1",
        "stop:2",
    ]


def test_corpus_logs_disable_ansi_even_when_parent_allows_color(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("NO_COLOR", "")
    corpus = Corpus(pins()["bun"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path / "workspace"
    assert corpus.server().env["NO_COLOR"] == "1"


@pytest.mark.parametrize("failing_read", [None, 3])
def test_churn_writes_before_reads_and_keeps_writer_cadence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failing_read: int | None
) -> None:
    corpus = Corpus(pins()["nextjs"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    path = tmp_path / PROBE_PATH
    sleeps: list[float] = []
    read_sources: list[str] = []
    real_sleep = asyncio.sleep
    client = AsyncMock(spec=Client)

    async def sleep(seconds: float) -> None:
        sleeps.append(seconds)
        await real_sleep(0)

    async def call(name: str, arguments: JsonObject) -> JsonObject:
        assert name == "search"
        assert arguments == {"query": "test", "limit": 1}
        read_sources.append(path.read_text(encoding="utf-8"))
        if len(read_sources) == failing_read:
            raise OSError("search failed")
        await real_sleep(0)
        return {"warnings": []}

    async def exercise() -> None:
        with (
            pytest.raises(OSError, match="search failed")
            if failing_read
            else nullcontext()
        ):
            await corpus.churn(cast(Client, client))
        assert not [
            task for task in asyncio.all_tasks() if task is not asyncio.current_task()
        ]

    client.call.side_effect = call
    monkeypatch.setattr(asyncio, "sleep", sleep)
    asyncio.run(exercise())
    assert read_sources[0] == "pub fn corpus_probe() { let value = 0; }\n"
    assert len(read_sources) == (failing_read or READ_COUNT)
    assert sleeps and set(sleeps) == {2.0}
    assert not path.exists()
    if failing_read is None:
        assert len(set(read_sources)) > 1
        assert object_value(corpus.actions[-1], "churn action")["reads"] == READ_COUNT
    else:
        assert not corpus.actions


def test_churn_write_failure_prevents_first_read(tmp_path: Path) -> None:
    corpus = Corpus(pins()["nextjs"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    (tmp_path / PROBE_PATH).mkdir()
    client = AsyncMock(spec=Client)
    with pytest.raises(IsADirectoryError):
        asyncio.run(corpus.churn(cast(Client, client)))
    client.call.assert_not_awaited()
