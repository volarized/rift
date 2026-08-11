"""Executable contracts for the dated version snapshots.

No version has been released yet, so `protocol/versions/` may not exist.
Every snapshot that does exist has to satisfy its manifest — the digest is
what freezes a released version.
"""

import hashlib
import json
from unittest import TestCase

from rift.models import core
from rift.snapshot import ARTIFACTS, PROTOCOL

VERSIONS = PROTOCOL / "versions"


class VersionSnapshotTests(TestCase):
    def test_every_snapshot_matches_its_manifest(self) -> None:
        if not VERSIONS.is_dir():
            self.skipTest("no version has been released")
        for snapshot in sorted(VERSIONS.iterdir()):
            manifest = json.loads((snapshot / "manifest.json").read_text())
            self.assertEqual(manifest["protocol_version"], snapshot.name)
            core.ProtocolVersion.model_validate(snapshot.name)
            self.assertEqual(tuple(manifest["artifacts"]), ARTIFACTS)
            for name, digest in manifest["artifacts"].items():
                self.assertEqual(
                    hashlib.sha256((snapshot / name).read_bytes()).hexdigest(),
                    digest,
                    f"{snapshot.name}/{name} does not match its manifest digest",
                )
