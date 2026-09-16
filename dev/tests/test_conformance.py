"""A supplied conformance binary must run without invoking Cargo."""

from pathlib import Path
from unittest.mock import Mock, patch

import pytest
from rift_dev import check_mcp_conformance as conformance


def test_supplied_binary_never_builds_and_preserves_failure_cleanup(
    tmp_path: Path,
) -> None:
    binary = tmp_path / "rift"
    binary.write_bytes(b"supplied executable")
    server = Mock()
    with (
        patch.object(conformance, "build_server_binary") as build,
        patch.object(conformance, "install_runner"),
        patch.object(conformance, "start_server", return_value=server) as start,
        patch.object(conformance, "await_published_port", return_value=4312),
        patch.object(conformance, "run_suite", return_value=7),
        patch.object(conformance, "stop_server") as stop,
    ):
        assert conformance.main(binary) == 7
    build.assert_not_called()
    assert start.call_args.args[0] == binary.resolve()
    stop.assert_called_once_with(server)
    assert binary.read_bytes() == b"supplied executable"


def test_missing_supplied_binary_refuses_without_build_or_install(
    tmp_path: Path,
) -> None:
    with (
        patch.object(conformance, "build_server_binary") as build,
        patch.object(conformance, "install_runner") as install,
        pytest.raises(RuntimeError, match="does not exist"),
    ):
        conformance.main(tmp_path / "missing")
    build.assert_not_called()
    install.assert_not_called()
