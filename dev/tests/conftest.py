"""Fixtures every `dev` test shares."""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager

import pytest
from rift_dev import check_agent, check_artifact, check_corpus
from rift_dev.trace import Collector, collector


@contextmanager
def unserved() -> Iterator[Collector]:
    """Stores no receiver serves: no thread, no port, and no environment for a server."""
    yield Collector()


@pytest.fixture(autouse=True)
def corpus_collector(monkeypatch: pytest.MonkeyPatch) -> None:
    """Corpus cases under test run fixture binaries that export nothing, so the runner's
    collector serves no port; starting and stopping uvicorn costs each case 0.2 s. The
    artifact and agent runners under test start no server either."""
    for module in (check_corpus, check_artifact, check_agent):
        monkeypatch.setattr(module, "collector", unserved)


@pytest.fixture
def otlp_collector() -> Iterator[Collector]:
    """A served collector for one test, stopped when the test ends."""
    with collector() as served:
        yield served
