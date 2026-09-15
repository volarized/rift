"""Serve verified release bytes through a loopback HTTPS CONNECT fixture."""

from __future__ import annotations

import hashlib
import json
import os
import socket
import ssl
import sys
import threading
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Final

import trustme

API_HOST: Final = "api.github.com"
DOWNLOAD_HOST: Final = "github.com"
REPOSITORY: Final = "volarized/rift"
LATEST_PATH: Final = f"/repos/{REPOSITORY}/releases/latest"
REQUEST_SECONDS_MAX: Final = 30.0
REQUEST_COUNT_MAX: Final = 128


class ReleaseFixture(HTTPServer):
    """Serve exact allowed responses, with one bounded connection at a time."""

    def __init__(self, directory: Path, responses: Mapping[tuple[str, str], bytes]):
        self.responses = dict(responses)
        self.requests: list[tuple[str, str]] = []
        self.failures: list[str] = []
        self.ca = trustme.CA()
        self.certificate = directory / "ca.pem"
        self.ca.cert_pem.write_to_path(self.certificate)
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.ca.issue_cert(API_HOST, DOWNLOAD_HOST).configure_cert(self.context)
        super().__init__(("127.0.0.1", 0), ConnectHandler)

    def get_request(self) -> tuple[socket.socket, tuple[str, int]]:
        connection, address = super().get_request()
        connection.settimeout(REQUEST_SECONDS_MAX)
        return connection, address

    @property
    def proxy(self) -> str:
        """Return the loopback address inherited by installer and updater children."""
        return f"http://127.0.0.1:{self.server_port}"

    def environment(self) -> dict[str, str]:
        """Inherit runtime paths and coverage, dropping credentials and proxy overrides."""
        removed = {
            "gh_token",
            "github_token",
            "gh_enterprise_token",
            "github_enterprise_token",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "rift_version",
            "rift_github_api",
            "rift_repository",
            "rift_download_base",
            "rift_install_dir",
        }
        environment = {
            key: value
            for key, value in os.environ.items()
            if key.lower() not in removed
        }
        environment.update(
            {
                "HTTPS_PROXY": self.proxy,
                "https_proxy": self.proxy,
                "CURL_CA_BUNDLE": str(self.certificate),
                "SSL_CERT_FILE": str(self.certificate),
            }
        )
        return environment

    def validate(self) -> None:
        """Refuse successful completion after any denied or failed fixture request."""
        if self.failures:
            raise AssertionError(f"release fixture refused requests: {self.failures}")

    def record_failure(self, message: str) -> None:
        """Keep failures bounded, including HTTP parser refusals and timeouts."""
        if len(self.failures) < REQUEST_COUNT_MAX:
            self.failures.append(message)

    @contextmanager
    def running(self) -> Iterator[ReleaseFixture]:
        """Join the listener after every outcome, including failed client assertions."""
        thread = threading.Thread(
            target=self.serve_forever, kwargs={"poll_interval": 0.05}
        )
        thread.start()
        try:
            yield self
        finally:
            self.shutdown()
            self.server_close()
            thread.join(timeout=REQUEST_SECONDS_MAX + 1)
            if thread.is_alive():
                raise RuntimeError("release fixture did not stop")


class ConnectHandler(BaseHTTPRequestHandler):
    """Restrict CONNECT to GitHub release hosts; HTTP parsing belongs to the standard library."""

    server: ReleaseFixture

    def do_CONNECT(self) -> None:
        credentials = self.headers.get("Authorization") or self.headers.get(
            "Proxy-Authorization"
        )
        if credentials or self.path not in (f"{API_HOST}:443", f"{DOWNLOAD_HOST}:443"):
            self.send_error(403)
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        self.wfile.flush()
        try:
            with self.server.context.wrap_socket(
                self.connection, server_side=True
            ) as connection:
                ReleaseHandler(connection, self.client_address, self.server)
        except (OSError, TimeoutError) as error:
            self.server.record_failure(f"TLS request failed: {error}")
        self.close_connection = True

    def log_message(self, format: str, *args: object) -> None:
        """Keep request output out of logs; assertions report allowed paths explicitly."""

    def log_error(self, format: str, *args: object) -> None:
        """Include standard-library parser and method refusals in fixture validation."""
        self.server.record_failure("HTTP request failed")


class ReleaseHandler(ConnectHandler):
    """Answer a single HTTPS GET from the immutable release response table."""

    def do_CONNECT(self) -> None:
        self.send_error(403)

    def do_GET(self) -> None:
        key = (self.headers.get("Host", ""), self.path)
        credentials = self.headers.get("Authorization") or self.headers.get(
            "Proxy-Authorization"
        )
        body = self.server.responses.get(key)
        if (
            credentials
            or body is None
            or len(self.server.requests) >= REQUEST_COUNT_MAX
        ):
            self.send_error(403)
            return
        self.server.requests.append(key)
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True


def download_path(tag: str, name: str) -> str:
    """Return the same artifact path the compiled updater constructs."""
    return f"/{REPOSITORY}/releases/download/{tag}/{name}"


def metadata(tag: str) -> bytes:
    """Present the candidate as latest inside the fixture, without publishing it on GitHub."""
    return json.dumps({"tag_name": tag, "draft": False, "prerelease": False}).encode()


@contextmanager
def trusted_certificate(certificate: Path) -> Iterator[None]:
    """Trust the test CA in native TLS, restricting OS store writes to hosted CI runners.

    Linux OpenSSL uses the child's SSL_CERT_FILE. macOS and Windows native TLS
    use OS stores, so the gate adds and removes only this CA on disposable runners.
    """
    from release_process import run

    if sys.platform.startswith("linux"):
        yield
        return
    hosted = (
        os.environ.get("GITHUB_ACTIONS") == "true"
        and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
    )
    if not hosted:
        raise RuntimeError(
            "native TLS release gate requires Linux or a GitHub-hosted runner"
        )
    der = ssl.PEM_cert_to_DER_cert(certificate.read_text(encoding="ascii"))
    thumbprint = hashlib.sha1(der, usedforsecurity=False).hexdigest()
    if sys.platform == "darwin":
        keychain = "/Library/Keychains/System.keychain"
        add = [
            "sudo",
            "-n",
            "security",
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-k",
            keychain,
            str(certificate),
        ]
        remove = [
            "sudo",
            "-n",
            "security",
            "remove-trusted-cert",
            "-d",
            str(certificate),
        ]
        delete = [
            "sudo",
            "-n",
            "security",
            "delete-certificate",
            "-Z",
            thumbprint,
            keychain,
        ]
    else:
        add = ["certutil", "-user", "-addstore", "Root", str(certificate)]
        remove = ["certutil", "-user", "-delstore", "Root", thumbprint]
        delete = None
    try:
        run(add)
        yield
    finally:
        try:
            run(remove)
        finally:
            if delete is not None:
                run(delete)
