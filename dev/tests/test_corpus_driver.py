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


def probe_answer(name: str, source: str, identity: str = "probe") -> JsonObject:
    span: JsonObject = {"start": 0, "end": len(source.encode())}
    symbol: JsonObject = {"id": identity}
    if name == "nodes":
        return {"nodes": [{"symbol": identity, "range": span}], "source": [source]}
    hit: JsonObject = {"path": PROBE_PATH, "range": span, "source": source}
    if name == "search":
        hit["hit"] = {"symbol": symbol}
        return {"results": [hit]}
    hit["symbol"] = symbol
    return {"hits": [hit]}


@pytest.mark.parametrize(
    "failure",
    [None, "read", "unmarked_stale", "identity", "marked_stale", "perpetual_stale"],
)
def test_churn_validates_all_tools_overlap_and_final_source(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, failure: str | None
) -> None:
    from rift_dev import check_corpus

    corpus = Corpus(pins()["nextjs"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    path = tmp_path / PROBE_PATH
    real_sleep = asyncio.sleep
    calls: list[str] = []
    writer_stopped = False
    final_calls: dict[str, int] = {}
    sleeps: list[float] = []
    client = AsyncMock(spec=Client)
    initial = "pub fn corpus_probe() { let value = 0; }"

    async def sleep(seconds: float) -> None:
        sleeps.append(seconds)
        nonlocal writer_stopped
        try:
            # A fast server can finish the minimum reads before two writes occur.
            while seconds == 2.0 and len(calls) <= READ_COUNT:
                await real_sleep(0)
            await real_sleep(0)
        except asyncio.CancelledError:
            writer_stopped = True
            raise

    async def call(name: str, arguments: JsonObject) -> JsonObject:
        assert arguments == check_corpus.CHURN_REQUESTS[name]
        calls.append(name)
        if failure == "read" and len(calls) == 3:
            raise OSError("search failed")
        await real_sleep(0)
        source = path.read_text().rstrip("\n")
        identity = "wrong" if failure == "identity" and len(calls) > 1 else "probe"
        marked = False
        if writer_stopped:
            final_calls[name] = final_calls.get(name, 0) + 1
            if failure in ("unmarked_stale", "perpetual_stale") or (
                failure == "marked_stale" and final_calls[name] == 1
            ):
                source = initial
                marked = failure != "unmarked_stale"
        answer = probe_answer(name, source, identity)
        if marked:
            answer["warnings"] = [{"code": "stale_index"}]
        return answer

    async def exercise() -> None:
        expected = {
            "read": (OSError, "search failed"),
            "unmarked_stale": (AssertionError, "without stale_index"),
            "perpetual_stale": (TimeoutError, None),
            "identity": (AssertionError, "changed probe identity"),
        }
        with (
            pytest.raises(*expected[failure][:1], match=expected[failure][1])
            if failure in expected
            else nullcontext()
        ):
            await corpus.churn(cast(Client, client))
        assert not [
            task for task in asyncio.all_tasks() if task is not asyncio.current_task()
        ]

    client.call.side_effect = call
    monkeypatch.setattr(asyncio, "sleep", sleep)
    if failure == "perpetual_stale":
        monkeypatch.setattr(check_corpus, "READ_SECONDS", 0.05)
    asyncio.run(exercise())
    assert not path.exists()
    assert sleeps and set(sleeps) <= {2.0, check_corpus.POLL_SECONDS}
    if failure in (None, "marked_stale"):
        summary = object_value(corpus.actions[-1], "churn action")
        assert cast(int, summary["reads"]) > READ_COUNT
        assert all(
            cast(int, count) >= READ_COUNT // 3
            for count in object_value(
                summary["pressure_calls"], "pressure calls"
            ).values()
        )
        assert cast(int, summary["overlapping_writes"]) >= 2
        assert summary["read_seconds_max"] == 30.0
        assert set(object_value(summary["latency"], "latency")) == {
            "search",
            "get_symbol",
            "nodes",
        }
        assert set(final_calls) == {"search", "get_symbol", "nodes"}


def test_churn_enforces_read_deadline_and_cleans_up(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from rift_dev import check_corpus

    corpus = Corpus(pins()["nextjs"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    client = AsyncMock(spec=Client)

    async def slow_call(*_args: object) -> JsonObject:
        await asyncio.sleep(0.02)
        return {}

    monkeypatch.setattr(check_corpus, "READ_SECONDS", 0.001)
    client.call.side_effect = slow_call
    with pytest.raises(TimeoutError):
        asyncio.run(corpus.churn(cast(Client, client)))
    assert not (tmp_path / PROBE_PATH).exists()


@pytest.mark.parametrize("tool", ["search", "get_symbol", "nodes"])
def test_churn_rejects_wrong_ranges_and_partial_source(tool: str) -> None:
    from rift_dev.corpus_assertions import churn_answer

    source = "pub fn corpus_probe() {}"
    assert churn_answer(tool, probe_answer(tool, source), [source], "probe") == (
        "probe",
        source,
    )
    with pytest.raises(AssertionError, match="expected revisions"):
        churn_answer(
            tool, probe_answer(tool, "pub fn corpus_probe() {"), [source], "probe"
        )
    answer = probe_answer(tool, source)
    rows = cast(
        list[JsonObject],
        answer[
            "nodes" if tool == "nodes" else "results" if tool == "search" else "hits"
        ],
    )
    object_value(rows[0]["range"], "range")["end"] = 1
    with pytest.raises(AssertionError, match="range disagrees"):
        churn_answer(tool, answer, [source], "probe")


def test_churn_write_failure_prevents_first_read(tmp_path: Path) -> None:
    corpus = Corpus(pins()["nextjs"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    (tmp_path / PROBE_PATH).mkdir()
    client = AsyncMock(spec=Client)
    with pytest.raises(IsADirectoryError):
        asyncio.run(corpus.churn(cast(Client, client)))
    client.call.assert_not_awaited()


@pytest.mark.parametrize("returned_identity", ["sample", "different"])
def test_sampled_symbol_must_resolve_through_nodes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, returned_identity: str
) -> None:
    from rift_dev import check_corpus

    monkeypatch.setattr(check_corpus, "SYMBOL_COUNT", 1)
    source = "def sample(): pass\n"
    (tmp_path / "source.py").write_text(source)
    corpus = Corpus(pins()["fastapi"], tmp_path / "rift", tmp_path / "report.json")
    corpus.root = tmp_path
    hit: JsonObject = {
        "hit": {"symbol": {"id": "sample", "language": "python"}},
        "path": "source.py",
        "range": {"start": 0, "end": len(source.encode())},
    }
    client = AsyncMock(spec=Client)
    client.resource.return_value = {
        "languages": [
            {
                "language": "python",
                "enabled": True,
                "syntax": True,
                "include": ["**/*.py"],
            }
        ],
    }

    async def call(name: str, arguments: JsonObject) -> JsonObject:
        if name == "search":
            return {"results": [hit], "warnings": []}
        assert name == "nodes"
        assert arguments == {"path": "source.py", "position": 0}
        return {"nodes": [{"symbol": returned_identity}]}

    client.call.side_effect = call
    with (
        pytest.raises(AssertionError, match="omitted sampled declaration")
        if returned_identity != "sample"
        else nullcontext()
    ):
        asyncio.run(corpus.symbols(cast(Client, client), [hit]))
    assert (tmp_path / "source.py").read_text() == source
