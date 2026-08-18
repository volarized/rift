"""Snapshot the generated protocol artifacts as one dated version.

``uv run python -m rift.snapshot 2026-08-04`` copies the generated contract
into ``protocol/versions/<version>/`` — mirroring the live tree's layout, so
the docs site can point either tree at the same loader — and writes a
manifest of SHA-256 digests that freezes the snapshot.
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import sys
from pathlib import Path

PROTOCOL = Path(__file__).parents[2]
ARTIFACTS = (
    "mcp.json",
    "rift.schema.json",
    "rift/core.proto",
    "rift/mcp.proto",
    "rift/scip.proto",
    "scip/scip.proto",
)


def snapshot(version: str) -> Path:
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", version):
        raise SystemExit(f"version must be YYYY-MM-DD, got {version!r}")
    target = PROTOCOL / "versions" / version
    if target.exists():
        shutil.rmtree(target)
    digests: dict[str, str] = {}
    for relative in ARTIFACTS:
        source = PROTOCOL / relative
        destination = target / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        digests[relative] = hashlib.sha256(source.read_bytes()).hexdigest()
    manifest = {"protocol_version": version, "artifacts": digests}
    (target / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return target


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: python -m rift.snapshot YYYY-MM-DD")
    print(snapshot(sys.argv[1]))
