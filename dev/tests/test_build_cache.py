"""The build cache starts from the one R2 key a job receives, and only then."""

import subprocess
from pathlib import Path
from typing import Any

import pytest
from rift_dev import build_cache

ENDPOINT = "https://d41d8cd98f00b204e9800998ecf8427e.r2.cloudflarestorage.com"


class Recorder:
    """Stands in for `subprocess.run`, recording each command and its environment."""

    def __init__(self, returncode: int = 0) -> None:
        self.returncode = returncode
        self.calls: list[tuple[list[str], dict[str, str]]] = []

    def __call__(
        self, command: list[str], *, env: dict[str, str], check: bool, **_: Any
    ) -> subprocess.CompletedProcess[bytes]:
        assert check is False
        self.calls.append((command, env))
        return subprocess.CompletedProcess(command, self.returncode)


def job(tmp_path: Path, **secrets: str) -> dict[str, str]:
    """A step environment: the runner's own variables plus the secrets mapped in."""
    return {
        "GITHUB_ENV": str(tmp_path / "github_env"),
        "PATH": "/usr/bin",
        **secrets,
    }


def exported(tmp_path: Path) -> str:
    path = tmp_path / "github_env"
    return path.read_text(encoding="utf-8") if path.exists() else ""


WRITE = {
    "R2_BUILD_CACHE_ENDPOINT": ENDPOINT,
    "R2_BUILD_CACHE_WRITE_ACCESS_KEY_ID": "write-id",
    "R2_BUILD_CACHE_WRITE_SECRET_ACCESS_KEY": "write-secret",
}
READ = {
    "R2_BUILD_CACHE_ENDPOINT": ENDPOINT,
    "R2_BUILD_CACHE_READ_ACCESS_KEY_ID": "read-id",
    "R2_BUILD_CACHE_READ_SECRET_ACCESS_KEY": "read-secret",
}


def test_a_job_without_a_key_compiles_without_sccache(tmp_path: Path) -> None:
    run = Recorder()
    environment = job(tmp_path, R2_BUILD_CACHE_ENDPOINT=ENDPOINT)
    assert build_cache.main(environment, run) == 0
    assert run.calls == []
    assert exported(tmp_path) == ""


@pytest.mark.parametrize(
    "secrets,mode,key",
    [(WRITE, "READ_WRITE", "write-id"), (READ, "READ_ONLY", "read-id")],
)
def test_the_received_pair_selects_the_mode(
    tmp_path: Path, secrets: dict[str, str], mode: str, key: str
) -> None:
    run = Recorder()
    assert build_cache.main(job(tmp_path, **secrets), run) == 0
    [(command, environment)] = run.calls
    assert command == ["sccache", "--start-server"]
    assert environment["SCCACHE_S3_RW_MODE"] == mode
    assert environment["AWS_ACCESS_KEY_ID"] == key
    assert environment["SCCACHE_BUCKET"] == "rift-oss-build-cache"
    assert environment["SCCACHE_ENDPOINT"] == ENDPOINT
    assert environment["SCCACHE_IDLE_TIMEOUT"] == "0"
    assert exported(tmp_path) == "RUSTC_WRAPPER=sccache\nCARGO_INCREMENTAL=0\n"


def test_the_server_receives_no_secret_under_its_own_name(tmp_path: Path) -> None:
    run = Recorder()
    build_cache.main(job(tmp_path, **WRITE), run)
    [(_, environment)] = run.calls
    assert [name for name in environment if name.startswith("R2_BUILD_CACHE_")] == []
    assert environment["PATH"] == "/usr/bin"


def test_nothing_exported_holds_the_key(tmp_path: Path) -> None:
    build_cache.main(job(tmp_path, **WRITE), Recorder())
    assert "write-secret" not in exported(tmp_path)
    assert "write-id" not in exported(tmp_path)


def test_a_server_that_fails_to_start_leaves_rustc_unwrapped(tmp_path: Path) -> None:
    assert build_cache.main(job(tmp_path, **WRITE), Recorder(returncode=2)) == 0
    assert exported(tmp_path) == ""


@pytest.mark.parametrize(
    "secrets,message",
    [
        (WRITE | READ, "never both"),
        (
            {
                "R2_BUILD_CACHE_ENDPOINT": ENDPOINT,
                "R2_BUILD_CACHE_READ_ACCESS_KEY_ID": "read-id",
            },
            "other half",
        ),
        (
            {
                "R2_BUILD_CACHE_WRITE_ACCESS_KEY_ID": "write-id",
                "R2_BUILD_CACHE_WRITE_SECRET_ACCESS_KEY": "write-secret",
            },
            "R2_BUILD_CACHE_ENDPOINT",
        ),
    ],
)
def test_a_malformed_secret_mapping_fails_before_sccache_starts(
    tmp_path: Path, secrets: dict[str, str], message: str
) -> None:
    run = Recorder()
    with pytest.raises(ValueError, match=message):
        build_cache.main(job(tmp_path, **secrets), run)
    assert run.calls == []


def test_a_step_outside_ci_is_refused(tmp_path: Path) -> None:
    run = Recorder()
    environment = job(tmp_path, **WRITE)
    del environment["GITHUB_ENV"]
    with pytest.raises(ValueError, match="GITHUB_ENV"):
        build_cache.main(environment, run)
    assert run.calls == []
