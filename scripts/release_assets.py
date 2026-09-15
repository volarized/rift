"""Read and validate the exact archives exercised by the release gate."""

from __future__ import annotations

import hashlib
import io
import json
import sys
import tarfile
import zipfile
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Final, cast

from release_fixture import DOWNLOAD_HOST, REPOSITORY, download_path
from release_process import run

ROOT: Final = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools/rift-release/src"))
from rift_release.release import (
    archive_name,
    binary_name,
    package_release,
    release_version,
)

ARCHIVE_BYTES_MAX: Final = 512 * 1024 * 1024
BINARY_BYTES_MAX: Final = 256 * 1024 * 1024
METADATA_BYTES_MAX: Final = 1024 * 1024


def object_document(text: str) -> dict[str, object]:
    """Require GitHub metadata to be a bounded JSON object."""
    if len(text.encode()) > METADATA_BYTES_MAX:
        raise ValueError("release metadata exceeded its byte bound")
    value: object = json.loads(text)
    if not isinstance(value, dict):
        raise TypeError("release metadata must be an object")
    return cast(dict[str, object], value)


def release_document(tag: str) -> dict[str, object]:
    """Read authenticated GitHub metadata before any fixture environment exists."""
    release_version(tag)
    return object_document(
        run(
            [
                "gh",
                "release",
                "view",
                tag,
                "--repo",
                REPOSITORY,
                "--json",
                "tagName,isDraft,isPrerelease,assets",
            ]
        )
    )


def previous_tag(candidate: str) -> str:
    """Select the greatest public stable version below the candidate within 100 releases."""
    document = object_document(
        run(
            [
                "gh",
                "release",
                "list",
                "--repo",
                REPOSITORY,
                "--limit",
                "100",
                "--exclude-drafts",
                "--exclude-pre-releases",
                "--json",
                "tagName,isDraft,isPrerelease",
                "--jq",
                "{releases: .}",
            ]
        )
    )
    return select_previous(candidate, document.get("releases"))


def select_previous(candidate: str, releases: object) -> str:
    """Require a real prior version when nightly tests rebuild the published version."""
    if not isinstance(releases, list) or len(releases) > 100:
        raise ValueError("release list must contain at most 100 entries")
    candidate_version = tuple(map(int, release_version(candidate).split(".")))
    accepted: dict[tuple[int, ...], str] = {}
    for document in releases:
        if not isinstance(document, dict):
            raise TypeError("release list entry must be an object")
        tag = document.get("tagName")
        if not isinstance(tag, str):
            raise TypeError("release list entry must carry a tag")
        require_release(document, tag, draft=False)
        version = tuple(map(int, release_version(tag).split(".")))
        if version < candidate_version:
            accepted[version] = tag
    if not accepted:
        raise ValueError(
            "no previous stable release found: provide --from-tag with an older published tag"
        )
    return accepted[max(accepted)]


def require_release(document: dict[str, object], tag: str, *, draft: bool) -> None:
    """Refuse a changed tag, prerelease, or unexpected draft state before downloading."""
    if (
        document.get("tagName") != tag
        or document.get("isDraft") is not draft
        or document.get("isPrerelease") is not False
    ):
        raise ValueError(f"release {tag} must have draft={draft} and prerelease=false")


def read_bounded(path: Path, maximum: int) -> bytes:
    """Read at most one byte past the file bound, including after concurrent file growth."""
    with path.open("rb") as source:
        data = source.read(maximum + 1)
    if not data or len(data) > maximum:
        raise ValueError(
            f"release file {path.name} is empty or exceeds {maximum} bytes"
        )
    return data


def binary_digest(archive: bytes, tag: str, target: str) -> str:
    """Read only the exact binary member, enforcing member names and byte bounds."""
    root = archive_name(tag, target).removesuffix(".tar.gz").removesuffix(".zip")
    expected = [
        f"{root}/{binary_name(target)}",
        f"{root}/README.md",
        f"{root}/LICENSE.md",
    ]
    if target.endswith("windows-msvc"):
        with zipfile.ZipFile(io.BytesIO(archive)) as package:
            if package.namelist() != expected:
                raise ValueError("release archive contains unexpected members")
            member = package.getinfo(expected[0])
            if member.file_size > BINARY_BYTES_MAX or member.is_dir():
                raise ValueError(
                    "release binary exceeds its byte bound or is not a file"
                )
            with package.open(member) as zip_source:
                data = zip_source.read(BINARY_BYTES_MAX + 1)
    else:
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
            members: list[tarfile.TarInfo] = []
            for _ in range(len(expected) + 1):
                entry = package.next()
                if entry is None:
                    break
                members.append(entry)
            if [entry.name for entry in members] != expected or not all(
                entry.isfile() for entry in members
            ):
                raise ValueError("release archive contains unexpected members")
            if members[0].size > BINARY_BYTES_MAX:
                raise ValueError("release binary exceeds its byte bound")
            source = package.extractfile(members[0])
            if source is None:
                raise ValueError("release archive has no binary")
            with source:
                data = source.read(BINARY_BYTES_MAX + 1)
    if not data or len(data) > BINARY_BYTES_MAX:
        raise ValueError("release binary is empty or exceeds its byte bound")
    require_machine(data, target)
    return hashlib.sha256(data).hexdigest()


def require_machine(data: bytes, target: str) -> None:
    """Require the archive's machine even when the host can emulate another one."""
    if target.endswith("windows-msvc"):
        require_windows_machine(data, target)
        return
    if target.endswith("apple-darwin"):
        # Apple mach_header_64: eight 32-bit fields, with cputype after magic.
        # Rust's individual targets emit one little-endian machine, never fat files.
        if len(data) < 32 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("release binary requires a single 64-bit Mach-O header")
        actual = int.from_bytes(data[4:8], "little")
        expected = {
            "x86_64-apple-darwin": 0x01000007,
            "aarch64-apple-darwin": 0x0100000C,
        }
    elif target.endswith("unknown-linux-gnu"):
        # System V ELF64: ident names class, encoding, and version; e_machine
        # follows the 16-byte ident and 2-byte e_type in the 64-byte header.
        if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
            raise ValueError("release binary requires a little-endian ELF64 header")
        actual = int.from_bytes(data[18:20], "little")
        expected = {
            "x86_64-unknown-linux-gnu": 62,
            "aarch64-unknown-linux-gnu": 183,
        }
    else:
        raise ValueError(f"unsupported release target: {target}")
    if actual != expected.get(target):
        raise ValueError(f"release binary machine does not match {target}")


def require_windows_machine(data: bytes, target: str) -> None:
    """Check the PE machine field so Windows emulation cannot hide a wrong archive."""
    # Microsoft PE format: offset 0x3c names the PE signature; the next two bytes
    # identify the machine. Archive size was bounded before this check.
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("release binary has no DOS header")
    offset = int.from_bytes(data[0x3C:0x40], "little")
    if offset < 64 or offset + 6 > len(data) or data[offset : offset + 4] != b"PE\0\0":
        raise ValueError("release binary has no bounded PE signature")
    expected = {"x86_64-pc-windows-msvc": 0x8664, "aarch64-pc-windows-msvc": 0xAA64}
    if int.from_bytes(data[offset + 4 : offset + 6], "little") != expected[target]:
        raise ValueError(f"release binary machine does not match {target}")


@dataclass(frozen=True)
class ReleaseAssets:
    """Own immutable archive and checksum bytes verified before the fixture serves them."""

    tag: str
    target: str
    archive: bytes
    manifest: bytes
    binary_sha256: str

    @classmethod
    def read(cls, directory: Path, tag: str, target: str) -> ReleaseAssets:
        name = archive_name(tag, target)
        archive = read_bounded(directory / name, ARCHIVE_BYTES_MAX)
        manifest = read_bounded(directory / f"rift-{tag}-checksums.sha256", 8192)
        entries = [line.split("  ") for line in manifest.decode("ascii").splitlines()]
        matches = [
            entry[0] for entry in entries if len(entry) == 2 and entry[1] == name
        ]
        if matches != [hashlib.sha256(archive).hexdigest()]:
            raise ValueError(f"release archive {name} has no unique matching checksum")
        return cls(tag, target, archive, manifest, binary_digest(archive, tag, target))

    @classmethod
    def download(
        cls, directory: Path, tag: str, target: str, *, draft: bool
    ) -> ReleaseAssets:
        require_release(release_document(tag), tag, draft=draft)
        directory.mkdir()
        run(
            [
                "gh",
                "release",
                "download",
                tag,
                "--repo",
                REPOSITORY,
                "--dir",
                str(directory),
                "--pattern",
                archive_name(tag, target),
                "--pattern",
                f"rift-{tag}-checksums.sha256",
            ]
        )
        return cls.read(directory, tag, target)

    @classmethod
    def candidate(
        cls, directory: Path, tag: str, target: str, binary: Path
    ) -> ReleaseAssets:
        expected = f"rift {release_version(tag)}"
        if run([str(binary), "--version"]).strip() != expected:
            raise ValueError(f"candidate binary must report {expected}")
        archive = package_release(ROOT, tag, target, binary, directory)
        digest = hashlib.sha256(read_bounded(archive, ARCHIVE_BYTES_MAX)).hexdigest()
        (directory / f"rift-{tag}-checksums.sha256").write_text(
            f"{digest}  {archive.name}\n", encoding="ascii"
        )
        return cls.read(directory, tag, target)

    def responses(self) -> dict[tuple[str, str], bytes]:
        """Map the updater's exact download paths to the previously verified bytes."""
        return {
            (
                DOWNLOAD_HOST,
                download_path(self.tag, archive_name(self.tag, self.target)),
            ): self.archive,
            (
                DOWNLOAD_HOST,
                download_path(self.tag, f"rift-{self.tag}-checksums.sha256"),
            ): self.manifest,
        }

    def verify_installed(
        self,
        binary: Path,
        environment: Mapping[str, str] | None = None,
        *,
        deadline: float | None = None,
    ) -> None:
        """Require the installed file to equal the archive's actual executable bytes."""
        digest = hashlib.sha256(read_bounded(binary, BINARY_BYTES_MAX)).hexdigest()
        if digest != self.binary_sha256:
            raise ValueError(
                "installed binary differs from the verified release archive"
            )
        expected = f"rift {release_version(self.tag)}"
        if (
            run(
                [str(binary), "--version"], environment=environment, deadline=deadline
            ).strip()
            != expected
        ):
            raise ValueError(f"installed binary must report {expected}")
