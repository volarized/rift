#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["trustme==1.2.1", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove release assets retain their verified bytes through the HTTPS fixture."""

from __future__ import annotations

import hashlib
import http.client
import os
import ssl
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from release_assets import (
    ROOT,
    ReleaseAssets,
    object_document,
    require_machine,
    require_release,
    require_windows_machine,
    select_previous,
)
from release_fixture import (
    API_HOST,
    DOWNLOAD_HOST,
    LATEST_PATH,
    ReleaseFixture,
    metadata,
    require_macos_trust,
    trusted_certificate,
)
from rift_release.release import package_release


def windows_image(machine: int) -> bytes:
    """Build the documented DOS offset, PE signature, and machine fields for validation."""
    return (
        b"MZ"
        + bytes(58)
        + (64).to_bytes(4, "little")
        + b"PE\0\0"
        + machine.to_bytes(2, "little")
    )


def macho_image(machine: int) -> bytes:
    """Build a full Mach-O 64-bit header with the documented machine field."""
    return b"\xcf\xfa\xed\xfe" + machine.to_bytes(4, "little") + bytes(24)


def elf_image(machine: int) -> bytes:
    """Build a full ELF64 header with little-endian machine and current version."""
    return (
        b"\x7fELF\x02\x01\x01" + bytes(11) + machine.to_bytes(2, "little") + bytes(44)
    )


IMAGES: dict[str, bytes] = {
    "x86_64-pc-windows-msvc": windows_image(0x8664),
    "aarch64-pc-windows-msvc": windows_image(0xAA64),
    "x86_64-apple-darwin": macho_image(0x01000007),
    "aarch64-apple-darwin": macho_image(0x0100000C),
    "x86_64-unknown-linux-gnu": elf_image(62),
    "aarch64-unknown-linux-gnu": elf_image(183),
}


class FixtureTests(unittest.TestCase):
    def test_https_serves_exact_bytes_and_rejects_unknown_requests_and_credentials(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            body = b"verified archive\x00\xff"
            fixture = ReleaseFixture(
                Path(directory), {(DOWNLOAD_HOST, "/archive"): body}
            )
            context = ssl.create_default_context()
            fixture.ca.configure_trust(context)
            with fixture.running():
                for method, path, headers, status in [
                    ("GET", "/archive", {}, 200),
                    ("GET", "/unknown", {}, 403),
                    ("GET", "/archive", {"Authorization": "secret"}, 403),
                    ("POST", "/archive", {}, 501),
                ]:
                    connection = http.client.HTTPSConnection(
                        "127.0.0.1", fixture.server_port, context=context, timeout=5
                    )
                    connection.set_tunnel(DOWNLOAD_HOST, 443)
                    connection.request(method, path, headers=headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, status)
                    actual = response.read()
                    if status == 200:
                        self.assertEqual(actual, body)
                    connection.close()
            self.assertEqual(fixture.requests, [(DOWNLOAD_HOST, "/archive")])
            self.assertEqual(len(fixture.failures), 3)
            with self.assertRaisesRegex(AssertionError, "refused requests"):
                fixture.validate()

    def test_https_requires_trust_and_rejects_unknown_connect_host(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = ReleaseFixture(
                Path(directory), {(API_HOST, LATEST_PATH): metadata("v0.0.34")}
            )
            with fixture.running():
                connection = http.client.HTTPSConnection(
                    "127.0.0.1", fixture.server_port, timeout=5
                )
                connection.set_tunnel(API_HOST, 443)
                with self.assertRaises(ssl.SSLCertVerificationError):
                    connection.request("GET", LATEST_PATH)
                connection.close()
                connection = http.client.HTTPSConnection(
                    "127.0.0.1", fixture.server_port, timeout=5
                )
                connection.set_tunnel("example.com", 443)
                with self.assertRaises(OSError):
                    connection.request("GET", "/")
                connection.close()
                connection = http.client.HTTPSConnection(
                    "127.0.0.1", fixture.server_port, timeout=5
                )
                connection.set_tunnel(API_HOST, 443, {"Proxy-Authorization": "secret"})
                with self.assertRaises(OSError):
                    connection.request("GET", LATEST_PATH)
                connection.close()
            self.assertEqual(fixture.requests, [])

    def test_environment_preserves_coverage_and_removes_authentication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = ReleaseFixture(Path(directory), {})
            try:
                with patch.dict(
                    os.environ,
                    {
                        "GH_TOKEN": "secret",
                        "LLVM_PROFILE_FILE": "test-%p.profraw",
                        "HTTPS_PROXY": "https://other",
                        "NO_PROXY": "*",
                    },
                ):
                    environment = fixture.environment()
                self.assertNotIn("GH_TOKEN", environment)
                self.assertNotIn("NO_PROXY", environment)
                self.assertEqual(environment["LLVM_PROFILE_FILE"], "test-%p.profraw")
                self.assertEqual(environment["HTTPS_PROXY"], fixture.proxy)
            finally:
                fixture.server_close()

    def test_local_macos_and_windows_never_change_native_trust(self) -> None:
        for platform in ("darwin", "win32"):
            with (
                self.subTest(platform=platform),
                patch("release_fixture.sys.platform", platform),
                patch.dict(os.environ, {}, clear=True),
                self.assertRaisesRegex(RuntimeError, "GitHub-hosted"),
                trusted_certificate(Path("unused")),
            ):
                self.fail("local trust must refuse")

    def test_windows_machine_store_removes_only_owned_certificate_after_failure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = ReleaseFixture(Path(directory), {})
            fixture.server_close()
            certificate = fixture.certificate
            der = ssl.PEM_cert_to_DER_cert(certificate.read_text(encoding="ascii"))
            thumbprint = hashlib.sha1(der, usedforsecurity=False).hexdigest()
            for add_result in ("", RuntimeError("import failed")):
                with (
                    self.subTest(add_result=add_result),
                    patch("release_fixture.sys.platform", "win32"),
                    patch.dict(
                        os.environ,
                        {
                            "GITHUB_ACTIONS": "true",
                            "RUNNER_ENVIRONMENT": "github-hosted",
                        },
                    ),
                    patch(
                        "release_process.run", side_effect=[add_result, ""]
                    ) as command,
                    self.assertRaisesRegex(RuntimeError, "failed"),
                    trusted_certificate(certificate),
                ):
                    raise RuntimeError("gate failed")
                add, remove = command.call_args_list
                self.assertEqual(
                    add.args[0],
                    ["certutil", "-f", "-addstore", "Root", str(certificate)],
                )
                self.assertEqual(
                    remove.args[0], ["certutil", "-delstore", "Root", thumbprint]
                )
                self.assertEqual(add.kwargs["timeout"], 60)
                self.assertEqual(remove.kwargs["timeout"], 60)

    def test_macos_cleanup_clears_only_owned_trust_and_requires_native_rejection(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = ReleaseFixture(Path(directory), {})
            fixture.server_close()
            rejected = RuntimeError(
                "security exited 1: Cert Verify Result: CSSMERR_TP_NOT_TRUSTED\n"
            )
            with (
                patch("release_fixture.sys.platform", "darwin"),
                patch.dict(
                    os.environ,
                    {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"},
                ),
                patch(
                    "release_process.run", side_effect=["", "", "", "", rejected]
                ) as command,
                self.assertRaisesRegex(ValueError, "gate failed"),
                trusted_certificate(fixture.certificate),
            ):
                raise ValueError("gate failed")
            calls = command.call_args_list
            self.assertEqual(len(calls), 5)
            self.assertEqual(calls[0].args[0][6], "trustRoot")
            self.assertEqual(calls[2].args[0][6], "unspecified")
            self.assertEqual(calls[0].args[0][-1], str(fixture.certificate))
            self.assertEqual(calls[2].args[0][-1], str(fixture.certificate))
            self.assertEqual(calls[3].args[0][3], "delete-certificate")
            self.assertNotIn("-t", calls[3].args[0])
            self.assertEqual(calls[1].args, calls[4].args)
            self.assertTrue(all(call.kwargs["timeout"] <= 60 for call in calls))

    def test_macos_cleanup_attempts_deletion_when_clearing_trust_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = ReleaseFixture(Path(directory), {})
            fixture.server_close()
            with (
                patch("release_fixture.sys.platform", "darwin"),
                patch.dict(
                    os.environ,
                    {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"},
                ),
                patch(
                    "release_process.run",
                    side_effect=[
                        "",
                        "",
                        RuntimeError("clear failed"),
                        "",
                        RuntimeError(
                            "security exited 1: Cert Verify Result: CSSMERR_TP_NOT_TRUSTED"
                        ),
                    ],
                ) as command,
                self.assertRaisesRegex(RuntimeError, "clear failed"),
                trusted_certificate(fixture.certificate),
            ):
                pass
            self.assertEqual(command.call_args_list[3].args[0][3], "delete-certificate")

    def test_macos_verification_cannot_accept_other_errors_or_remaining_trust(
        self,
    ) -> None:
        for result in (
            "",
            RuntimeError("security exceeded 15s"),
            RuntimeError("security exited 1: invalid certificate"),
        ):
            with (
                self.subTest(result=result),
                patch("release_process.run", side_effect=[result]),
                self.assertRaises((RuntimeError, AssertionError)),
            ):
                require_macos_trust(Path("owned-ca.pem"), trusted=False)


class AssetTests(unittest.TestCase):
    def test_valid_checksum_cannot_accept_wrong_machine_for_any_target(self) -> None:
        for source, data in IMAGES.items():
            architecture, platform = source.split("-", 1)
            other = "x86_64" if architecture == "aarch64" else "aarch64"
            target = f"{other}-{platform}"
            with (
                self.subTest(source=source, target=target),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                binary = root / "rift"
                binary.write_bytes(data)
                binary.chmod(0o755)
                archive = package_release(ROOT, "v0.0.34", target, binary, root)
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                (root / "rift-v0.0.34-checksums.sha256").write_text(
                    f"{digest}  {archive.name}\n"
                )
                with self.assertRaisesRegex(ValueError, "machine does not match"):
                    ReleaseAssets.read(root, "v0.0.34", target)

    def test_unix_machine_rejects_universal_wrong_encoding_and_truncated_headers(
        self,
    ) -> None:
        macho = macho_image(0x01000007)
        elf = elf_image(62)
        invalid = {
            "x86_64-apple-darwin": (
                macho[:31],
                bytes.fromhex("cafebabe") + macho[4:],
                bytes.fromhex("bebafeca") + macho[4:],
                bytes.fromhex("cafebabf") + macho[4:],
                bytes.fromhex("bfbafeca") + macho[4:],
                bytes.fromhex("feedfacf") + macho[4:],
                bytes.fromhex("cefaedfe") + macho[4:],
            ),
            "x86_64-unknown-linux-gnu": (
                elf[:63],
                b"invalid" + elf[7:],
                elf[:4] + b"\x01" + elf[5:],
                elf[:5] + b"\x02" + elf[6:],
                elf[:6] + b"\x00" + elf[7:],
            ),
        }
        for target, images in invalid.items():
            for data in images:
                with (
                    self.subTest(target=target, data=data),
                    self.assertRaisesRegex(ValueError, "header"),
                ):
                    require_machine(data, target)

    def test_windows_machine_rejects_wrong_architecture_and_invalid_headers(
        self,
    ) -> None:
        x64 = "x86_64-pc-windows-msvc"
        arm64 = "aarch64-pc-windows-msvc"
        require_windows_machine(windows_image(0x8664), x64)
        require_windows_machine(windows_image(0xAA64), arm64)
        for machine, target in ((0x8664, arm64), (0xAA64, x64), (0xA641, arm64)):
            with (
                self.subTest(machine=machine, target=target),
                self.assertRaisesRegex(ValueError, "machine"),
            ):
                require_windows_machine(windows_image(machine), target)
        for data in (
            b"MZ",
            b"invalid" + windows_image(0x8664)[7:],
            windows_image(0x8664)[:64],
            windows_image(0x8664)[:60]
            + bytes.fromhex("ffffffff")
            + windows_image(0x8664)[64:],
        ):
            with (
                self.subTest(data=data),
                self.assertRaisesRegex(ValueError, "header|signature"),
            ):
                require_windows_machine(data, x64)

    def test_previous_release_is_lower_even_when_candidate_is_already_latest(
        self,
    ) -> None:
        releases = [
            {"tagName": tag, "isDraft": False, "isPrerelease": False}
            for tag in ("v0.0.34", "v0.0.9", "v0.0.33", "v0.0.32")
        ]
        self.assertEqual(select_previous("v0.0.34", releases), "v0.0.33")
        self.assertEqual(select_previous("v0.0.35", releases), "v0.0.34")
        with self.assertRaisesRegex(ValueError, "--from-tag"):
            select_previous("v0.0.1", releases)
        with self.assertRaisesRegex(ValueError, "100"):
            select_previous("v0.0.35", releases * 26)

    def test_exact_packaged_bytes_are_served_and_corruption_refuses(self) -> None:
        for target, data in IMAGES.items():
            with (
                self.subTest(target=target),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                binary = root / "rift"
                binary.write_bytes(data)
                binary.chmod(0o755)
                archive = package_release(ROOT, "v0.0.34", target, binary, root)
                original = archive.read_bytes()
                digest = hashlib.sha256(original).hexdigest()
                manifest = root / "rift-v0.0.34-checksums.sha256"
                manifest.write_text(f"{digest}  {archive.name}\n")
                assets = ReleaseAssets.read(root, "v0.0.34", target)
                self.assertEqual(
                    assets.binary_sha256,
                    hashlib.sha256(binary.read_bytes()).hexdigest(),
                )
                self.assertIn(original, assets.responses().values())
                archive.write_bytes(b"corrupted")
                self.assertIn(original, assets.responses().values())
                with self.assertRaisesRegex(ValueError, "checksum"):
                    ReleaseAssets.read(root, "v0.0.34", target)
                archive.write_bytes(original)
                manifest.write_text(f"{digest}  {archive.name}\n" * 2)
                with self.assertRaisesRegex(ValueError, "unique"):
                    ReleaseAssets.read(root, "v0.0.34", target)

    def test_metadata_requires_matching_tag_draft_and_stable_release(self) -> None:
        good: dict[str, object] = {
            "tagName": "v0.0.34",
            "isDraft": True,
            "isPrerelease": False,
        }
        require_release(good, "v0.0.34", draft=True)
        for change in (
            {"isDraft": False},
            {"isPrerelease": True},
            {"tagName": "v0.0.33"},
            {"isDraft": None},
        ):
            with self.subTest(change=change), self.assertRaises(ValueError):
                require_release(good | change, "v0.0.34", draft=True)
        with self.assertRaises(TypeError):
            object_document("[]")


if __name__ == "__main__":
    unittest.main()
