#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["trustme==1.2.1", "mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "PyYAML==6.0.3", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove release gate failures keep the draft blocked and fixture bytes unchanged."""

from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from collections.abc import Mapping, Sequence
from contextlib import nullcontext
from dataclasses import replace
from pathlib import Path
from typing import cast
from unittest.mock import Mock, patch

import yaml
from check_release_gate import (
    Options,
    Report,
    UpgradeResult,
    installer_command,
    run_gate,
    upgrade,
)
from check_release_promotion import require_promotion
from release_assets import (
    ROOT,
    ReleaseAssets,
)
from release_fixture import (
    API_HOST,
    LATEST_PATH,
    ReleaseFixture,
)
from release_upgrade import windows_flush_error
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

    def test_recovery_evidence_keeps_method_and_historical_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            options = replace(
                self.options(Path(temporary)),
                mode="upgrade",
                target="x86_64-pc-windows-msvc",
            )
            previous = ReleaseAssets(
                "v0.0.33", options.target, b"old", b"manifest", "a" * 64
            )
            candidate = ReleaseAssets(
                options.tag, options.target, b"new", b"manifest", "b" * 64
            )
            staged = r"C:\upgrade\.rift-update-new.exe"
            evidence = {
                "method": "installer-recovery",
                "historical_error": windows_flush_error(staged),
                "staged_path": staged,
            }
            fixture = Mock(spec=ReleaseFixture)
            fixture.requests = []
            fixture.certificate = None
            fixture.running.return_value = nullcontext()
            with (
                patch("check_release_gate.prepare", return_value=(previous, candidate)),
                patch("check_release_gate.ReleaseFixture", return_value=fixture),
                patch(
                    "check_release_gate.trusted_certificate", return_value=nullcontext()
                ),
                patch(
                    "check_release_gate.upgrade",
                    return_value=UpgradeResult(Path("unused"), evidence),
                ),
            ):
                run_gate(options)
            self.assertEqual(
                json.loads(options.evidence.read_text())["upgrade"], evidence
            )


class UpgradeTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.installed: list[tuple[str, Path]] = []
        self.smoked: list[Path] = []

    def attempt(
        self,
        *,
        target: str = "x86_64-pc-windows-msvc",
        from_tag: str = "v0.0.33",
        tag: str = "v0.0.34",
        platform: str = "win32",
        fault: str = "",
    ) -> UpgradeResult:
        previous = ReleaseAssets(
            from_tag,
            target,
            b"old archive",
            b"manifest",
            hashlib.sha256(b"old").hexdigest(),
        )
        candidate = ReleaseAssets(
            tag, target, b"new archive", b"manifest", hashlib.sha256(b"new").hexdigest()
        )
        mock_fixture = Mock(spec=ReleaseFixture)
        mock_fixture.requests = []
        mock_fixture.environment.return_value = {}
        fixture = cast(ReleaseFixture, mock_fixture)
        binary = (
            self.directory / "upgrade" / ("rift.exe" if "windows" in target else "rift")
        )
        staged = binary.parent / ".rift-update-new.exe"

        def install_assets(
            assets: ReleaseAssets,
            prefix: Path,
            _shell: str,
            _fixture: ReleaseFixture,
            *,
            deadline: float | None = None,
        ) -> Path:
            self.assertIsNotNone(deadline)
            self.installed.append((assets.tag, prefix))
            if assets is candidate and fault == "reinstall-failure":
                raise RuntimeError("installer failed")
            prefix.mkdir(exist_ok=True)
            binary.write_bytes(b"old" if assets is previous else b"new")
            return binary

        def invoke_update(command: Sequence[str], **_options: object) -> str:
            self.assertEqual(list(command), [str(binary), "update"])
            fixture.requests.extend(candidate.responses())
            if fault != "missing-request":
                fixture.requests.append((API_HOST, LATEST_PATH))
            staged.write_bytes(b"bad" if fault == "staged-bytes" else b"new")
            if fault == "old-bytes":
                binary.write_bytes(b"bad")
            if fault == "backup":
                (binary.parent / ".rift-update-old.exe").write_bytes(b"old")
            if fault == "updater-success":
                binary.write_bytes(b"new")
                return ""
            # Exact stderr recorded on both native Windows v0.0.33 updaters.
            error = (
                "rift.exe exited 1: rift: error[update_publish_failed]: "
                "Rift update could not be published: flushing the staged binary "
                f"`{staged}` failed: Access is denied. (os error 5): "
                "ensure the directory is writable and retry `rift update`\n"
                "  caused by: Access is denied. (os error 5)"
            )
            replacements = {
                "wrong-exit": ("exited 1", "exited 2"),
                "wrong-code": ("update_publish_failed", "update_download_failed"),
                "wrong-stage": ("flushing the staged binary", "replacing"),
                "wrong-os-error": ("os error 5", "os error 32"),
                "wrong-path": (str(staged), str(binary)),
            }
            if fault in replacements:
                error = error.replace(*replacements[fault])
            if fault == "timeout":
                error = "rift.exe exceeded 180s"
            raise RuntimeError(error + "\n")

        def binary_version(command: Sequence[str], **_options: object) -> str:
            path = Path(command[0])
            if (path == binary and fault == "old-version") or (
                path == staged and fault == "staged-version"
            ):
                return "rift 0.0.99"
            version = from_tag if path.read_bytes() == b"old" else tag
            return f"rift {version.removeprefix('v')}"

        def smoke_binary(
            path: Path, _tag: str, _environment: Mapping[str, str], **_options: object
        ) -> None:
            self.assertEqual(path.read_bytes(), b"new")
            self.smoked.append(path)
            if fault == "smoke-failure":
                raise RuntimeError("smoke failed")

        with (
            patch("check_release_gate.sys.platform", platform),
            patch("check_release_gate.install", side_effect=install_assets),
            patch("check_release_gate.run", side_effect=invoke_update),
            patch("release_assets.run", side_effect=binary_version),
            patch("check_release_gate.smoke", side_effect=smoke_binary),
        ):
            return upgrade(
                previous,
                candidate,
                self.directory,
                fixture,
                Report(self.directory / "junit.xml"),
            )

    def test_both_windows_targets_recover_only_after_verifying_historical_state(
        self,
    ) -> None:
        for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
            with self.subTest(target=target):
                self.installed.clear()
                result = self.attempt(target=target)
                self.assertEqual(result.evidence["method"], "installer-recovery")
                self.assertEqual(
                    result.evidence["historical_error"],
                    windows_flush_error(result.evidence["staged_path"]),
                )
                self.assertEqual(
                    self.installed,
                    [
                        ("v0.0.33", result.binary.parent),
                        ("v0.0.34", result.binary.parent),
                    ],
                )
                self.assertEqual(self.smoked[-1], result.binary)
                suite = ET.parse(self.directory / "junit.xml").getroot()
                self.assertEqual(suite.get("failures"), "0")
                self.assertIsNotNone(
                    suite.find("testcase[@name='historical-update-refusal']")
                )
                self.assertIsNotNone(suite.find("testcase[@name='installer-recovery']"))
                self.assertIsNone(suite.find("testcase[@name='upgrade']"))

    def test_other_errors_or_changed_historical_state_never_reinstall(self) -> None:
        for fault in (
            "wrong-exit",
            "wrong-code",
            "wrong-stage",
            "wrong-os-error",
            "wrong-path",
            "timeout",
            "old-bytes",
            "old-version",
            "staged-bytes",
            "staged-version",
            "missing-request",
            "updater-success",
            "backup",
        ):
            with self.subTest(fault=fault):
                self.installed.clear()
                with self.assertRaises((RuntimeError, ValueError, AssertionError)):
                    self.attempt(fault=fault)
                self.assertEqual(len(self.installed), 1)
                self.assertFalse(self.smoked)
                self.assertEqual(
                    ET.parse(self.directory / "junit.xml").getroot().get("failures"),
                    "1",
                )

    def test_other_releases_targets_and_hosts_keep_failure(self) -> None:
        for options in (
            {"from_tag": "v0.0.32"},
            {"tag": "v0.0.35"},
            {"target": "x86_64-unknown-linux-gnu"},
            {"platform": "linux"},
        ):
            with self.subTest(options=options):
                self.installed.clear()
                with self.assertRaises(RuntimeError):
                    self.attempt(**options)
                self.assertEqual(len(self.installed), 1)

    def test_reinstall_or_smoke_failure_remains_a_failed_gate(self) -> None:
        for fault in ("reinstall-failure", "smoke-failure"):
            with self.subTest(fault=fault), self.assertRaises(RuntimeError):
                self.attempt(fault=fault)
            self.assertEqual(
                ET.parse(self.directory / "junit.xml").getroot().get("failures"), "1"
            )

    def test_normal_upgrade_records_update_without_installer_recovery(self) -> None:
        result = self.attempt(tag="v0.0.35", fault="updater-success")
        self.assertEqual(result.evidence, {"method": "update"})
        self.assertEqual(len(self.installed), 1)
        self.assertEqual(self.smoked, [result.binary])


class WorkflowTests(unittest.TestCase):
    def test_release_smoke_uses_native_gate_python_selection(self) -> None:
        release = yaml.safe_load(
            (ROOT / ".github/workflows/rift-release.yml").read_text()
        )["jobs"]["build"]
        gate = yaml.safe_load(
            (ROOT / ".github/workflows/release-gate.yml").read_text()
        )["jobs"]["release-gate"]
        self.assertEqual(release["env"]["UV_PYTHON"], gate["env"]["UV_PYTHON"])
        smoke = next(
            step
            for step in release["steps"]
            if step.get("name") == "Smoke test release binary"
        )
        self.assertNotIn("--python", smoke["run"])
        self.assertNotIn("UV_PYTHON", smoke.get("env", {}))

    def test_native_builds_select_process_exit_and_platform_publication_cases(
        self,
    ) -> None:
        portable = (
            "server::tests::stopped_process_does_not_wait_for_a_replacement_election",
            "server::tests::changed_or_missing_document_does_not_prove_process_exit",
            "server::tests::already_exited_process_does_not_wait_for_a_building_holder",
            "server::tests::process_observation_errors_keep_waiting_and_retain_their_cause",
            "server::tests::await_stopped_times_out_while_the_holder_keeps_the_election",
            "server::tests::await_election_released_returns_once_the_holder_is_gone",
        )
        for name in ("release-gate.yml", "rift-release.yml"):
            with self.subTest(workflow=name):
                workflow = yaml.safe_load(
                    (ROOT / ".github/workflows" / name).read_text()
                )
                build = workflow["jobs"]["build"]
                native = yaml.safe_load(
                    (ROOT / ".github/workflows/native-tests.yml").read_text()
                )["jobs"]["test"]
                call = workflow["jobs"]["native-tests"]
                self.assertEqual(call["needs"], "build")
                self.assertEqual(call["uses"], "./.github/workflows/native-tests.yml")
                self.assertEqual(native["timeout-minutes"], 20)
                self.assertEqual(native["strategy"], build["strategy"])
                steps = native["steps"]
                test = next(
                    step
                    for step in steps
                    if step.get("name")
                    == "Test native process exit and update publication"
                )
                installer = next(
                    step
                    for step in steps
                    if step.get("with", {}).get("tool") == "cargo-nextest@0.9.140"
                )
                self.assertNotIn("if", installer)
                self.assertNotIn("if", test)
                self.assertEqual(build["timeout-minutes"], 30)
                self.assertEqual(test["shell"], "bash")
                self.assertEqual(
                    test["env"]["RIFT_UPDATE_TEST_BINARY"],
                    "${{ github.workspace }}/${{ matrix.binary }}",
                )
                common, conditional = test["env"]["RIFT_NATIVE_TESTS"].split("${{", 1)
                self.assertEqual(
                    {part.strip() for part in common.split("or ")},
                    {f"test(={case})" for case in portable},
                )
                self.assertTrue(
                    conditional.startswith(" runner.os == 'Windows' && 'or ")
                )
                self.assertTrue(conditional.endswith("' || '' }}"))
                for case in (
                    "windows_publish_flushes_staging_and_preserves_backup",
                    "windows_publish_replaces_running_binary_and_cleans_backup",
                ):
                    self.assertIn(f"test(=update::tests::{case})", conditional)
                self.assertNotIn("probe", test["env"]["RIFT_NATIVE_TESTS"])
                self.assertIn("--bin rift", test["run"])
                self.assertNotIn("--test ", test["run"])
                self.assertIn("--no-tests fail", test["run"])
                self.assertIn(
                    "${{ runner.os == 'Windows' && '--run-ignored all' || '' }}",
                    test["run"],
                )
                self.assertIn('-E "$RIFT_NATIVE_TESTS"', test["run"])
                cli = [
                    step
                    for step in build["steps"]
                    if "cargo build --release --locked" in step.get("run", "")
                ]
                self.assertEqual(len(cli), 1)
                self.assertTrue(
                    all("cargo build" not in step.get("run", "") for step in steps)
                )
                self.assertNotIn("continue-on-error", test)
                report = next(
                    step
                    for step in steps
                    if step.get("with", {}).get("name")
                    == "native-tests-${{ matrix.target }}"
                )
                self.assertEqual(report["if"], "always()")
                self.assertEqual(report["with"]["path"], "target/nextest/ci/junit.xml")
                self.assertEqual(report["with"]["if-no-files-found"], "error")

    def test_candidate_builds_are_required_and_draft_skips_are_explicit(self) -> None:
        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/release-gate.yml").read_text()
        )
        build = workflow["jobs"]["build"]
        gate = workflow["jobs"]["release-gate"]
        self.assertEqual(build["if"], "${{ !inputs.draft }}")
        self.assertEqual(gate["needs"], ["build", "native-tests"])
        self.assertEqual(
            gate["if"],
            "${{ !cancelled() && ((inputs.draft && needs.build.result == 'skipped' "
            "&& needs.native-tests.result == 'skipped') "
            "|| (!inputs.draft && needs.build.result == 'success' "
            "&& needs.native-tests.result == 'success')) }}",
            "Only successful candidate builds or intentional draft skips may run gates",
        )
        self.assertEqual(build["timeout-minutes"], 30)
        self.assertEqual(gate["timeout-minutes"], 20)
        targets = build["strategy"]["matrix"]["include"]
        self.assertEqual(targets, gate["strategy"]["matrix"]["include"])
        self.assertEqual(
            {target["target"] for target in targets}, set(SUPPORTED_TARGETS)
        )
        for job in (build, gate):
            self.assertNotIn("continue-on-error", job)
            self.assertTrue(
                all("continue-on-error" not in step for step in job["steps"])
            )
        upload = next(
            step
            for step in build["steps"]
            if "upload-artifact" in step.get("uses", "")
            and step["with"]["name"].startswith("candidate-")
        )
        download = next(
            step
            for step in gate["steps"]
            if "download-artifact" in step.get("uses", "")
        )
        self.assertEqual(upload["with"]["name"], download["with"]["name"])
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        self.assertTrue(
            all("cargo build" not in step.get("run", "") for step in gate["steps"])
        )

    def test_promotion_requires_every_target_and_unchanged_github_asset_digests(
        self,
    ) -> None:
        tag = "v0.0.34"
        staged = r"C:\release-gate\upgrade\.rift-update-new.exe"
        recovery = {
            "method": "installer-recovery",
            "historical_error": windows_flush_error(staged),
            "staged_path": staged,
        }
        evidence: list[dict[str, object]] = [
            {
                "tag": tag,
                "target": target,
                "from_tag": "v0.0.33",
                "upgrade": recovery if "windows" in target else {"method": "update"},
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
        for item in evidence:
            if "windows" in str(item["target"]):
                with (
                    self.subTest(target=item["target"]),
                    self.assertRaisesRegex(ValueError, "requires installer recovery"),
                ):
                    item["upgrade"] = {"method": "update"}
                    require_promotion(tag, document, evidence)
                item["upgrade"] = recovery
        future_tag = "v0.0.35"
        future_document = document | {
            "tagName": future_tag,
            "assets": [
                asset | {"name": asset["name"].replace(tag, future_tag)}
                for asset in assets
            ],
        }
        future_evidence = [
            item | {"tag": future_tag, "upgrade": {"method": "update"}}
            for item in evidence
        ]
        require_promotion(future_tag, future_document, future_evidence)
        with self.assertRaisesRegex(ValueError, "every release target"):
            require_promotion(tag, document, evidence[:-1])
        with self.assertRaisesRegex(ValueError, "every release target"):
            require_promotion(tag, document, evidence[:-1] + [evidence[0]])
        windows = next(
            item for item in evidence if item["target"] == "x86_64-pc-windows-msvc"
        )
        windows["upgrade"] = recovery
        require_promotion(tag, document, evidence)
        for invalid in (
            None,
            {"method": "update"},
            {"method": "install"},
            {"method": "installer-recovery"},
            recovery | {"historical_error": "other failure"},
            recovery | {"staged_path": r"C:\other\.rift-update-new.exe"},
        ):
            with (
                self.subTest(upgrade=invalid),
                self.assertRaises((ValueError, TypeError)),
            ):
                windows["upgrade"] = invalid
                require_promotion(tag, document, evidence)
        windows["upgrade"] = recovery
        windows["from_tag"] = "v0.0.32"
        with self.assertRaisesRegex(ValueError, "limited to Windows"):
            require_promotion(tag, document, evidence)
        windows["from_tag"] = "v0.0.33"
        linux = next(
            item for item in evidence if item["target"] == "x86_64-unknown-linux-gnu"
        )
        linux["upgrade"] = recovery
        with self.assertRaisesRegex(ValueError, "limited to Windows"):
            require_promotion(tag, document, evidence)
        linux["upgrade"] = {"method": "update"}
        windows["upgrade"] = recovery
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
        self.assertEqual(set(jobs["publish"]["needs"]), {"build", "native-tests", "docs"})
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
