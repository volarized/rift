"""Validate and package Rift binary releases."""

from __future__ import annotations

import gzip
import hashlib
import io
import json
import os
import re
import subprocess
import tarfile
import zipfile
from pathlib import Path
from typing import Final

TAG_PATTERN: Final = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SUPPORTED_TARGETS: Final = (
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
)
WINDOWS_TARGETS: Final = frozenset(
    {"aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc"}
)
SOURCE_DATE_EPOCH: Final = 0
ZIP_DATE: Final = (1980, 1, 1, 0, 0, 0)


def release_version(tag: str) -> str:
    """Return version carried by valid release tag."""
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ValueError(f"release tag must match vX.Y.Z: {tag}")
    return tag.removeprefix("v")


def cargo_packages(repository: Path) -> list[dict[str, object]]:
    """Load workspace packages through Cargo metadata."""
    process = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=repository,
        capture_output=True,
        check=False,
        text=True,
    )
    if process.returncode != 0:
        raise RuntimeError(process.stderr.strip() or "cargo metadata failed")
    metadata = json.loads(process.stdout)
    packages = metadata.get("packages")
    if not isinstance(packages, list) or not packages:
        raise RuntimeError("Cargo workspace contains no packages")
    return packages


def validate_workspace_version(repository: Path, tag: str) -> None:
    """Require every workspace package to carry release tag version."""
    expected = release_version(tag)
    mismatches = sorted(
        f"{package['name']}={package['version']}"
        for package in cargo_packages(repository)
        if package.get("version") != expected
    )
    if mismatches:
        values = ", ".join(mismatches)
        raise ValueError(f"workspace packages must use {expected}: {values}")

    required = (
        repository / "Cargo.lock",
        repository / "LICENSE.md",
        repository / "README.md",
    )
    missing = [path.name for path in required if not path.is_file()]
    if missing:
        raise ValueError(f"release inputs missing: {', '.join(missing)}")


def archive_name(tag: str, target: str) -> str:
    """Return canonical archive filename."""
    release_version(tag)
    if target not in SUPPORTED_TARGETS:
        raise ValueError(f"unsupported release target: {target}")
    extension = "zip" if target in WINDOWS_TARGETS else "tar.gz"
    return f"rift-{tag}-{target}.{extension}"


def binary_name(target: str) -> str:
    """Return platform binary filename for supported target."""
    if target not in SUPPORTED_TARGETS:
        raise ValueError(f"unsupported release target: {target}")
    return "rift.exe" if target in WINDOWS_TARGETS else "rift"


def verify_binary_version(binary: Path, tag: str) -> None:
    """Require built binary version and help surface to match release."""
    expected = f"rift {release_version(tag)}"
    if not binary.is_file():
        raise ValueError(f"release binary missing: {binary}")
    process = subprocess.run(
        [str(binary), "--version"], capture_output=True, check=False, text=True
    )
    actual = process.stdout.strip()
    if process.returncode != 0 or actual != expected:
        raise ValueError(f"release binary version must be {expected!r}: {actual!r}")

    help_process = subprocess.run(
        [str(binary), "--help"], capture_output=True, check=False, text=True
    )
    if help_process.returncode != 0 or "Usage: rift" not in help_process.stdout:
        raise ValueError("release binary must expose rift help")


def tar_info(name: str, mode: int, size: int) -> tarfile.TarInfo:
    """Create deterministic archive member metadata."""
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.size = size
    info.mtime = SOURCE_DATE_EPOCH
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    return info


def add_bytes(archive: tarfile.TarFile, name: str, data: bytes, mode: int) -> None:
    """Add bytes under deterministic metadata."""
    archive.addfile(tar_info(name, mode, len(data)), io.BytesIO(data))


def zip_info(name: str, mode: int) -> zipfile.ZipInfo:
    """Create deterministic ZIP member metadata."""
    info = zipfile.ZipInfo(name, ZIP_DATE)
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3
    info.external_attr = mode << 16
    return info


def package_release(
    repository: Path,
    tag: str,
    target: str,
    binary: Path,
    output: Path,
) -> Path:
    """Create deterministic Rift release archive."""
    name = archive_name(tag, target)
    if not binary.is_file():
        raise ValueError(f"release binary missing: {binary}")
    if target not in WINDOWS_TARGETS and not os.access(binary, os.X_OK):
        raise ValueError(f"release binary is not executable: {binary}")

    root = name.removesuffix(".zip").removesuffix(".tar.gz")
    members = (
        (f"{root}/{binary_name(target)}", binary.read_bytes(), 0o755),
        (f"{root}/README.md", (repository / "README.md").read_bytes(), 0o644),
        (f"{root}/LICENSE.md", (repository / "LICENSE.md").read_bytes(), 0o644),
    )

    output.mkdir(parents=True, exist_ok=True)
    destination = output / name
    temporary = destination.with_suffix(f"{destination.suffix}.tmp")
    if target in WINDOWS_TARGETS:
        with zipfile.ZipFile(temporary, "w", compresslevel=9) as archive:
            for member_name, data, mode in members:
                archive.writestr(zip_info(member_name, mode), data)
    else:
        with (
            temporary.open("wb") as raw,
            gzip.GzipFile(
                fileobj=raw, mode="wb", filename="", mtime=SOURCE_DATE_EPOCH
            ) as zipped,
            tarfile.open(fileobj=zipped, mode="w") as archive,
        ):
            for member_name, data, mode in members:
                add_bytes(archive, member_name, data, mode)
    temporary.replace(destination)
    return destination


def checksum_manifest(tag: str, directory: Path) -> Path:
    """Write manifest after verifying all target archives exist."""
    release_version(tag)
    expected = {archive_name(tag, target) for target in SUPPORTED_TARGETS}
    manifest = directory / f"rift-{tag}-checksums.sha256"
    actual = {
        path.name
        for path in directory.glob(f"rift-{tag}-*")
        if path.is_file() and path != manifest
    }
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        raise ValueError(
            f"release archives differ; missing={missing}, unexpected={unexpected}"
        )

    lines = []
    for name in sorted(expected):
        digest = hashlib.sha256((directory / name).read_bytes()).hexdigest()
        lines.append(f"{digest}  {name}\n")
    manifest.write_text("".join(lines), encoding="utf-8")
    return manifest
