"""Command validation fails before builds, downloads, or corpus execution."""

from pathlib import Path
from unittest.mock import Mock

import pytest
from pydantic import ValidationError
from rift_dev import cli
from rift_dev.config import CorpusPins
from rift_dev.corpus_cache import PINS
from typer.testing import CliRunner


def test_invalid_binary_selection_never_builds(monkeypatch: pytest.MonkeyPatch) -> None:
    build = Mock()
    monkeypatch.setattr(cli, "candidate_binary", build)
    result = CliRunner().invoke(
        cli.app, ["artifact", "--binary", "rift", "--target", "target"]
    )
    assert isinstance(result.exception, ValidationError)
    assert "cannot accompany" in str(result.exception)
    build.assert_not_called()


@pytest.mark.parametrize(
    "arguments",
    [
        ["test"],
        ["test", "bun"],
        ["test", "fastapi", "--binary", "rift", "--case", "stop"],
    ],
)
def test_invalid_corpus_selection_never_reads_pins(
    arguments: list[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    read = Mock()
    monkeypatch.setattr(cli, "pins", read)
    result = CliRunner().invoke(cli.app, ["corpus", *arguments])
    assert isinstance(result.exception, ValidationError)
    read.assert_not_called()


def test_conformance_preserves_runner_exit_status(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    run = Mock(return_value=7)
    monkeypatch.setattr(cli.check_mcp_conformance, "main", run)
    result = CliRunner().invoke(cli.app, ["conformance", "--binary", "rift"])
    assert result.exit_code == 7
    run.assert_called_once_with(Path("rift"))


@pytest.mark.parametrize(
    "field,value",
    [
        ("commit", "main"),
        ("seconds", 0),
        ("files", True),
        ("repository", "owner/../tree"),
    ],
)
def test_invalid_pin_refuses_before_git(field: str, value: object) -> None:
    import tomllib

    document = tomllib.loads(PINS.read_text())
    document["bun"][field] = value
    with pytest.raises(ValidationError):
        CorpusPins.model_validate(document)


def test_unknown_corpus_configuration_is_rejected() -> None:
    import tomllib

    document = tomllib.loads(PINS.read_text())
    document["extra"] = document["bun"]
    with pytest.raises(ValidationError, match="Extra inputs"):
        CorpusPins.model_validate(document)
