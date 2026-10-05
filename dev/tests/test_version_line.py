"""`verify_version` compares the whole `--version` line with `rift <version>`."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from rift_dev import rift_test_client


class Reported:
    """A `Command` whose output is a fixed `--version` line."""

    line = ""

    def __init__(self, *_arguments: Any) -> None:
        pass

    def with_deadline(self, _deadline: float | None) -> Reported:
        return self

    def output(self) -> str:
        return self.line + "\n"


@pytest.fixture
def reported(monkeypatch: pytest.MonkeyPatch) -> type[Reported]:
    monkeypatch.setattr(rift_test_client, "Command", Reported)
    return Reported


def test_the_exact_line_passes(reported: type[Reported], tmp_path: Path) -> None:
    reported.line = "rift 1.2.3"
    rift_test_client.verify_version(tmp_path / "rift", "v1.2.3")


@pytest.mark.parametrize("line", ["rift 1.2.3+abc1234", "rift 1.2.4", "rift"])
def test_any_other_line_is_refused(
    reported: type[Reported], tmp_path: Path, line: str
) -> None:
    reported.line = line
    with pytest.raises(Exception, match="expected 'rift 1.2.3'"):
        rift_test_client.verify_version(tmp_path / "rift", "1.2.3")
