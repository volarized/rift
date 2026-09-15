#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["trustme==1.2.1", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Require every draft archive to retain the bytes verified by its native release gate."""

from __future__ import annotations

import argparse
from pathlib import Path

from release_assets import (
    METADATA_BYTES_MAX,
    object_document,
    read_bounded,
    release_document,
    require_release,
)
from rift_release.release import SUPPORTED_TARGETS, archive_name


def require_promotion(
    tag: str,
    document: dict[str, object],
    evidence: list[dict[str, object]],
) -> None:
    """Reject missing target results or assets changed after their gate finished."""
    require_release(document, tag, draft=True)
    targets = [item.get("target") for item in evidence]
    if len(targets) != len(SUPPORTED_TARGETS) or set(targets) != set(SUPPORTED_TARGETS):
        raise ValueError(
            "promotion requires one verified result for every release target"
        )
    assets = document.get("assets")
    if not isinstance(assets, list):
        raise TypeError("release assets must be an array")
    actual: dict[str, str] = {}
    for asset in assets:
        if not isinstance(asset, dict):
            raise TypeError("release asset must be an object")
        name, digest = asset.get("name"), asset.get("digest")
        if not isinstance(name, str) or not isinstance(digest, str) or name in actual:
            raise ValueError(
                "release assets require unique names and GitHub SHA256 digests"
            )
        actual[name] = digest
    for item in evidence:
        target = item.get("target")
        if item.get("tag") != tag or not isinstance(target, str):
            raise ValueError("gate evidence must name the candidate tag and target")
        expected = {
            archive_name(tag, target): item.get("archive_sha256"),
            f"rift-{tag}-checksums.sha256": item.get("manifest_sha256"),
        }
        for name, digest in expected.items():
            if not isinstance(digest, str) or actual.get(name) != f"sha256:{digest}":
                raise ValueError(f"draft asset changed after verification: {name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    options = parser.parse_args()
    paths = sorted(options.evidence.glob("*/evidence.json"))
    if len(paths) != len(SUPPORTED_TARGETS):
        raise ValueError("promotion requires every native gate evidence artifact")
    evidence = [
        object_document(read_bounded(path, METADATA_BYTES_MAX).decode("utf-8"))
        for path in paths
    ]
    require_promotion(options.tag, release_document(options.tag), evidence)


if __name__ == "__main__":
    main()
