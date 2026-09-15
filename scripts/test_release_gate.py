#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["trustme==1.2.1", "mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "PyYAML==6.0.3", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove release gate failures keep the draft blocked and fixture bytes unchanged."""

from __future__ import annotations

import tempfile
import unittest
import xml.etree.ElementTree as ET
from contextlib import nullcontext
from pathlib import Path
from unittest.mock import patch

import yaml
from check_release_gate import Options, Report, installer_command, run_gate
from check_release_promotion import require_promotion
from release_assets import (
    ROOT,
    ReleaseAssets,
)
from release_fixture import (
    ReleaseFixture,
)
from rift_release.release import SUPPORTED_TARGETS, archive_name


class GateTests(unittest.TestCase):
    def options(self, directory: Path) -> Options:
        return Options(
            tag="v0.0.34",
            from_tag="v0.0.33",
            target="x86_64-unknown-linux-gnu",
            candidate_binary=None,
            published=False,
            coldstart=False,
            agent=False,
            mode="install",
            shell=["bash"],
            junit=directory / "junit.xml",
            evidence=directory / "evidence.json",
        )

    def test_prepare_failure_is_a_failing_junit_case(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            options = self.options(Path(temporary))
            with (
                patch(
                    "check_release_gate.prepare",
                    side_effect=RuntimeError("download\x00 failed"),
                ),
                self.assertRaisesRegex(RuntimeError, "failed"),
            ):
                run_gate(options)
            suite = ET.parse(options.junit).getroot()
            self.assertEqual(suite.get("failures"), "1")
            self.assertIsNotNone(suite.find("testcase[@name='prepare']/failure"))
            self.assertFalse(options.evidence.exists())

    def test_fixture_validation_failure_is_reported_and_cleanup_runs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            options = self.options(Path(temporary))
            previous = ReleaseAssets(
                "v0.0.33", options.target, b"previous", b"manifest", "a" * 64
            )
            candidate = ReleaseAssets(
                options.tag, options.target, b"candidate", b"manifest", "b" * 64
            )
            with (
                patch("check_release_gate.prepare", return_value=(previous, candidate)),
                patch(
                    "check_release_gate.trusted_certificate", return_value=nullcontext()
                ),
                patch("check_release_gate.install", return_value=Path("unused")),
                patch("check_release_gate.smoke"),
                patch.object(
                    ReleaseFixture,
                    "validate",
                    side_effect=AssertionError("denied request"),
                ),
                self.assertRaisesRegex(AssertionError, "denied request"),
            ):
                run_gate(options)
            suite = ET.parse(options.junit).getroot()
            self.assertEqual(suite.get("failures"), "1")
            self.assertIsNotNone(
                suite.find("testcase[@name='fixture-validation']/failure")
            )
            self.assertIsNotNone(suite.find("testcase[@name='fixture-cleanup']"))
            self.assertFalse(options.evidence.exists())

    def test_installer_shell_uses_documented_bash_command_without_interpolation(
        self,
    ) -> None:
        command = installer_command("dash", "v0.0.34")
        self.assertEqual(command[:3], ["dash", "-c", 'exec bash "$1" --version "$2"'])
        self.assertEqual(command[-1], "v0.0.34")

    def test_junit_records_failure_before_exception_leaves(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            report = Report(path)

            def failed() -> None:
                raise RuntimeError("gate failed")

            with self.assertRaisesRegex(RuntimeError, "gate failed"):
                report.check("upgrade", failed)
            self.assertIsNotNone(ET.parse(path).find("testcase/failure"))


class WorkflowTests(unittest.TestCase):
    def test_promotion_requires_every_target_and_unchanged_github_asset_digests(
        self,
    ) -> None:
        tag = "v0.0.34"
        evidence: list[dict[str, object]] = [
            {
                "tag": tag,
                "target": target,
                "archive_sha256": "a" * 64,
                "manifest_sha256": "b" * 64,
            }
            for target in SUPPORTED_TARGETS
        ]
        assets = [
            {"name": archive_name(tag, target), "digest": "sha256:" + "a" * 64}
            for target in SUPPORTED_TARGETS
        ]
        assets.append(
            {"name": f"rift-{tag}-checksums.sha256", "digest": "sha256:" + "b" * 64}
        )
        document: dict[str, object] = {
            "tagName": tag,
            "isDraft": True,
            "isPrerelease": False,
            "assets": assets,
        }
        require_promotion(tag, document, evidence)
        with self.assertRaisesRegex(ValueError, "every release target"):
            require_promotion(tag, document, evidence[:-1])
        with self.assertRaisesRegex(ValueError, "every release target"):
            require_promotion(tag, document, evidence[:-1] + [evidence[0]])
        assets[0]["digest"] = "sha256:" + "c" * 64
        with self.assertRaisesRegex(ValueError, "changed after verification"):
            require_promotion(tag, document, evidence)
        assets[0]["digest"] = "sha256:" + "a" * 64
        assets[-1]["digest"] = "sha256:" + "c" * 64
        with self.assertRaisesRegex(ValueError, "changed after verification"):
            require_promotion(tag, document, evidence)

    def test_failed_or_skipped_gate_has_no_promotion_path(self) -> None:
        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/rift-release.yml").read_text()
        )
        jobs = workflow["jobs"]
        self.assertEqual(set(jobs["promote"]["needs"]), {"release-gate", "full"})
        self.assertNotIn(
            "if",
            jobs["promote"],
            "GitHub's default success() must guard both dependencies",
        )
        self.assertEqual(jobs["deploy-docs"]["needs"], "promote")
        self.assertEqual(jobs["release-gate"]["needs"], "publish")
        self.assertEqual(jobs["release-gate"]["permissions"], {"contents": "write"})
        gate = yaml.safe_load((ROOT / ".github/workflows/release-gate.yml").read_text())
        self.assertNotIn("permissions", gate)
        self.assertNotIn("permissions", gate["jobs"]["release-gate"])
        full = yaml.safe_load((ROOT / ".github/workflows/full.yml").read_text())
        self.assertEqual(full["permissions"], {"contents": "read"})
        for name in ("promote", "release-gate", "full", "publish"):
            self.assertNotIn("continue-on-error", jobs[name])
        publish = "\n".join(step.get("run", "") for step in jobs["publish"]["steps"])
        self.assertIn("--draft", publish)
        self.assertNotIn("--latest", publish)
        self.assertTrue(
            all("continue-on-error" not in step for step in jobs["promote"]["steps"])
        )
        promote = "\n".join(step.get("run", "") for step in jobs["promote"]["steps"])
        self.assertIn("--draft=false --latest", promote)


if __name__ == "__main__":
    unittest.main()
