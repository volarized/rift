"""The development recipes compose the commands their shell versions ran.

Each test stands a recorder in for the process runner, so no Cargo build, Git
remote, or test suite starts.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from rift_dev import generated, release_tag, suites, worktrees
from rift_dev.commands import REPOSITORY


class Commands:
    """Records each command a recipe runs, answering `output` from a table."""

    def __init__(self, outputs: dict[tuple[str, ...], str] | None = None) -> None:
        self.ran: list[tuple[list[str], dict[str, str] | None]] = []
        self.outputs = outputs or {}

    def run(self, *argv: Any, env: dict[str, str] | None = None) -> None:
        self.ran.append(([str(argument) for argument in argv], env))

    def output(self, *argv: Any) -> str:
        return self.outputs[tuple(str(argument) for argument in argv)]

    def commands(self) -> list[list[str]]:
        return [command for command, _env in self.ran]


@pytest.fixture
def commands(monkeypatch: pytest.MonkeyPatch) -> Commands:
    recorder = Commands()
    for module in (generated, release_tag, suites, worktrees):
        if hasattr(module, "run"):
            monkeypatch.setattr(module, "run", recorder.run)
        if hasattr(module, "output"):
            monkeypatch.setattr(module, "output", recorder.output)
    return recorder


def help_outputs() -> dict[tuple[str, ...], str]:
    return {
        ("cargo", "run", "-q", "-p", "rift", "--", *arguments): f"help {index}\n"
        for index, arguments in enumerate(generated.HELP_COMMANDS)
    }


def test_the_cli_help_transcript_matches_its_committed_layout(
    commands: Commands,
) -> None:
    commands.outputs = help_outputs()
    assert generated.cli_help() == (
        "$ rift --help\nhelp 0\n"
        "\n$ rift server --help\nhelp 1\n"
        "\n$ rift server logs --help\nhelp 2\n"
    )


def test_a_stale_cli_help_fails_the_check(
    commands: Commands, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    commands.outputs = help_outputs()
    committed = tmp_path / "cli-help.txt"
    committed.write_text("$ rift --help\nstale\n", encoding="utf-8")
    monkeypatch.setattr(generated, "CLI_HELP", committed)
    with pytest.raises(SystemExit) as exit:
        generated.check_cli_help()
    assert exit.value.code == 1


def test_the_unit_suite_runs_from_an_archive_under_the_floor(
    commands: Commands, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("CARGO_LLVM_COV_TARGET_DIR", str(tmp_path / "cov"))
    suites.unit(Path("target/fast.tar.zst"))
    [command] = commands.commands()
    assert command[:3] == ["cargo", "llvm-cov", "nextest"]
    assert command[3:8] == [
        "--archive-file",
        "target/fast.tar.zst",
        "--extract-overwrite",
        "--workspace-remap",
        ".",
    ]
    assert command[-2:] == ["--fail-under-lines", suites.COVERAGE_FLOOR]
    assert (tmp_path / "cov/CACHEDIR.TAG").read_text().startswith("Signature: ")


def test_the_unit_suite_builds_every_target_without_an_archive(
    commands: Commands, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("CARGO_LLVM_COV_TARGET_DIR", str(tmp_path / "cov"))
    suites.unit(None)
    [command] = commands.commands()
    assert command[3:7] == [
        "--workspace",
        "--all-targets",
        "--all-features",
        "--locked",
    ]


def test_the_live_suites_enable_their_engines(commands: Commands) -> None:
    suites.live(None)
    [(command, env)] = commands.ran
    assert command[:3] == ["cargo", "nextest", "run"]
    assert env == {"RIFT_ENGINE_LIVE": "1", "RIFT_LIVE_SEARCH": "1"}


@pytest.mark.parametrize(
    "test_name,archive,selection,tail",
    [
        (None, None, ["--test", "corpus_bun", "--cargo-profile", "corpus"], []),
        (
            "test_bun_stop",
            Path("target/integration.tar.zst"),
            ["-E", "binary(=corpus_bun)"],
            ["--", "--exact", "test_bun_stop"],
        ),
    ],
)
def test_a_corpus_suite_selects_its_repository(
    commands: Commands,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    test_name: str | None,
    archive: Path | None,
    selection: list[str],
    tail: list[str],
) -> None:
    monkeypatch.setenv("CARGO_LLVM_COV_TARGET_DIR", str(tmp_path / "cov"))
    suites.corpus("bun", test_name, archive)
    [command] = commands.commands()
    assert "--run-ignored" in command
    joined = " ".join(command)
    assert " ".join(selection) in joined
    assert command[len(command) - len(tail) :] == tail


def test_every_worktree_is_read_from_porcelain() -> None:
    porcelain = (
        "worktree /repo\nHEAD 1\nbranch refs/heads/main\n\n"
        "worktree /repo/.claude/worktrees/one\nHEAD 2\ndetached\n"
    )
    assert worktrees.worktrees(porcelain) == [
        Path("/repo"),
        Path("/repo/.claude/worktrees/one"),
    ]


@pytest.mark.parametrize("tag", ["0.0.46", "v0.0", "v01.0.0", "v0.0.46-rc1"])
def test_a_malformed_release_tag_is_refused_before_git(
    commands: Commands, tag: str
) -> None:
    with pytest.raises(SystemExit):
        release_tag.release(tag)
    assert commands.ran == []


def test_the_declared_version_comes_from_the_workspace_package() -> None:
    manifest = (REPOSITORY / "Cargo.toml").read_text(encoding="utf-8")
    version = release_tag.declared_version(manifest)
    assert release_tag.TAG.match(f"v{version}")
