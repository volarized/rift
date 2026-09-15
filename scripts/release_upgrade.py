"""Validate the one approved historical Windows installer recovery in gate evidence."""

from __future__ import annotations

from pathlib import PureWindowsPath

from release_assets import release_version
from rift_release.release import WINDOWS_TARGETS


def windows_v33_recovery(from_tag: str, tag: str, target: str) -> bool:
    """Limit installer recovery to the two Windows v0.0.33 -> v0.0.34 releases."""
    return from_tag == "v0.0.33" and tag == "v0.0.34" and target in WINDOWS_TARGETS


def windows_flush_error(staged_path: str) -> str:
    """Match the recorded exit status, publication operation, path, and Windows error."""
    return (
        "rift.exe exited 1: rift: error[update_publish_failed]: "
        "Rift update could not be published: flushing the staged binary "
        f"`{staged_path}` failed: Access is denied. (os error 5): "
        "ensure the directory is writable and retry `rift update`\n"
        "  caused by: Access is denied. (os error 5)"
    )


def require_upgrade_evidence(
    from_tag: object, tag: str, target: str, evidence: object
) -> None:
    """Require successful self-update or the exact approved installer recovery result."""
    if not isinstance(from_tag, str) or tuple(
        map(int, release_version(from_tag).split("."))
    ) >= tuple(map(int, release_version(tag).split("."))):
        raise ValueError("upgrade evidence requires a strictly older release")
    if not isinstance(evidence, dict):
        raise TypeError("promotion requires upgrade method evidence")
    if evidence == {"method": "update"}:
        if windows_v33_recovery(from_tag, tag, target):
            raise ValueError(
                "Windows v0.0.33 -> v0.0.34 requires installer recovery evidence"
            )
        return
    if (
        set(evidence) != {"method", "historical_error", "staged_path"}
        or evidence.get("method") != "installer-recovery"
        or not windows_v33_recovery(from_tag, tag, target)
    ):
        raise ValueError("installer recovery is limited to Windows v0.0.33 -> v0.0.34")
    staged_path = evidence.get("staged_path")
    if (
        not isinstance(staged_path, str)
        or PureWindowsPath(staged_path).name != ".rift-update-new.exe"
        or "\n" in staged_path
        or "\r" in staged_path
        or evidence.get("historical_error") != windows_flush_error(staged_path)
    ):
        raise ValueError(
            "installer recovery requires the recorded Windows flush failure"
        )
