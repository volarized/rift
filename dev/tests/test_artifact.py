"""Artifact inputs must preserve supplied executable and fixture source bytes."""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from rift_dev.check_artifact import CONFIGURATION, SOURCE, lay_out_workspace


def test_supplied_binary_is_never_built(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from rift_dev import rift_test_client

    binary = tmp_path / "rift"
    binary.write_bytes(b"release bytes")
    builder = AsyncMock()
    monkeypatch.setattr(rift_test_client, "build_server_binary", builder)
    assert rift_test_client.candidate_binary(binary) == binary.resolve()
    assert binary.read_bytes() == b"release bytes"
    builder.assert_not_called()
    with pytest.raises(AssertionError, match="cannot accompany"):
        rift_test_client.candidate_binary(binary, "aarch64-unknown-linux-gnu")
    builder.assert_not_called()


def test_workspace_preserves_lf_bytes_on_every_platform(tmp_path: Path) -> None:
    lay_out_workspace(tmp_path)
    assert (tmp_path / "lib.rs").read_bytes() == SOURCE.encode()
    assert (tmp_path / "rift.toml").read_bytes() == CONFIGURATION.encode()
