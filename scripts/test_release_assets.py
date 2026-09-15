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
    require_release,
    select_previous,
)
from release_fixture import (
    API_HOST,
    DOWNLOAD_HOST,
    LATEST_PATH,
    ReleaseFixture,
    metadata,
    trusted_certificate,
)
from rift_release.release import package_release


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

    def test_local_macos_never_changes_native_trust(self) -> None:
        with (
            patch("release_fixture.sys.platform", "darwin"),
            patch.dict(os.environ, {}, clear=True),
            self.assertRaisesRegex(RuntimeError, "GitHub-hosted"),
            trusted_certificate(Path("unused")),
        ):
            self.fail("local trust must refuse")


class AssetTests(unittest.TestCase):
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
        for target in ("x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"):
            with (
                self.subTest(target=target),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                binary = root / "rift"
                binary.write_bytes(b"real fixture executable bytes")
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
