"""Check report ownership without starting a corpus server."""

from pathlib import Path

from check_corpus import Corpus
from corpus_cache import pins


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
