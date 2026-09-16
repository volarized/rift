"""Fetch immutable corpus trees and verify their Git object measurements."""

from __future__ import annotations

import dataclasses
import os
import re
import shutil
import tempfile
from pathlib import Path, PurePosixPath

import tomllib

from rift_dev.config import CorpusPins
from rift_dev.release_process import run_bytes

PINS = Path(__file__).resolve().parents[3] / "crates/rift/tests/corpus/pins.toml"
GIT_SECONDS_MAX = 600.0
GIT_OUTPUT_BYTES_MAX = 16 * 1024 * 1024
TREE_FILES_MAX = 100_000


@dataclasses.dataclass(frozen=True)
class Measurement:
    """Counts over Git blobs, including symlink contents."""

    files: int
    bytes: int
    symlinks: int
    depth: int
    package_json: int


@dataclasses.dataclass(frozen=True)
class Pin:
    """One immutable tree with independently recorded expectations."""

    name: str
    repository: str
    tag: str
    commit: str
    measurement: Measurement
    oversized_path: str
    oversized_bytes: int
    seconds: int

    @property
    def cache(self) -> Path:
        root = Path(
            os.environ.get("RIFT_CORPUS_DIR", "~/.cache/rift-corpus")
        ).expanduser()
        return root.resolve() / self.repository.replace("/", "-") / self.commit

    def verify(self, root: Path) -> Measurement:
        """Refuse a cache that moved, changed, or lost its measured objects."""
        observed = self._verify_tree(root)
        missing = missing_history_objects(root, self.commit)
        if missing:
            raise RuntimeError(
                f"{self.name}: {len(missing)} history objects are missing, first {missing[0]}; "
                "run just corpus-sync before testing"
            )
        return observed

    def _verify_tree(self, root: Path) -> Measurement:
        """Verify the pinned bytes before any repair fetch changes the object store."""
        head = git(root, "rev-parse", "HEAD").decode().strip()
        if head != self.commit:
            raise RuntimeError(
                f"{self.name}: expected commit {self.commit}, observed {head}"
            )
        if git(
            root, "status", "--porcelain", "--untracked-files=all", "--ignored=matching"
        ):
            raise RuntimeError(f"{self.name}: cached checkout changed; restore {root}")
        observed = measure(git(root, "ls-tree", "-r", "-l", "-z", self.commit))
        if observed != self.measurement:
            raise RuntimeError(
                f"{self.name}: expected {self.measurement}, observed {observed}; remeasure the pin"
            )
        if self.oversized_path:
            path = root / self.oversized_path
            if (
                not path.is_file()
                or path.is_symlink()
                or path.stat().st_size != self.oversized_bytes
            ):
                raise RuntimeError(
                    f"{self.name}: oversized path or byte count changed: {self.oversized_path}"
                )
        return observed

    def sync(self) -> Path:
        """Publish a verified depth-50 checkout by renaming its temporary directory."""
        destination = self.cache
        if destination.exists():
            self._verify_tree(destination)
            if missing_history_objects(destination, self.commit):
                git(
                    destination,
                    "fetch",
                    "--quiet",
                    "--depth=50",
                    "--no-filter",
                    "--refetch",
                    "--no-tags",
                    "origin",
                    self.commit,
                )
            self.verify(destination)
            return destination
        destination.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(
            prefix="fetch-", dir=destination.parent
        ) as temporary:
            checkout = Path(temporary) / "checkout"
            checkout.mkdir()
            git(checkout, "init", "--quiet")
            git(
                checkout,
                "remote",
                "add",
                "origin",
                f"https://github.com/{self.repository}.git",
            )
            git(
                checkout,
                "fetch",
                "--quiet",
                "--depth=50",
                "--no-tags",
                "origin",
                self.commit,
            )
            git(checkout, "checkout", "--quiet", "--detach", self.commit)
            self.verify(checkout)
            checkout.rename(destination)
        return destination

    def checkout(self, destination: Path, *, depth: int = 50) -> None:
        """Copy a disposable tree while the verified cache remains untouched.

        copytree preserves Git's remote and shallow metadata with every symlink.
        Verification requires all reachable history objects before serving.
        The separate depth-one case uses Git's fetch boundary directly.
        """
        self.verify(self.cache)
        if depth == 50:
            shutil.copytree(self.cache, destination, symlinks=True)
        elif depth == 1:
            destination.mkdir()
            git(destination, "init", "--quiet")
            git(
                destination,
                "remote",
                "add",
                "origin",
                f"https://github.com/{self.repository}.git",
            )
            git(
                destination,
                "fetch",
                "--quiet",
                "--depth=1",
                "--no-tags",
                "origin",
                self.commit,
            )
            git(destination, "checkout", "--quiet", "--detach", self.commit)
        else:
            raise ValueError("corpus depth must be 1 or 50")
        self.verify(destination)


def missing_history_objects(root: Path, commit: str) -> list[str]:
    """Read missing IDs without Git fetching them or printing every available object."""
    missing = git(
        root, "rev-list", "--quiet", "--objects", "--missing=print", commit
    ).splitlines()
    for row in missing:
        if re.fullmatch(rb"\?[0-9a-f]{40}", row) is None:
            raise ValueError(f"invalid missing history object: {row!r}")
    return [row[1:].decode("ascii") for row in missing]


def git(
    root: Path,
    *arguments: str,
    input_bytes: bytes | None = None,
    accepted: tuple[int, ...] = (0,),
) -> bytes:
    """Run Git with inherited environment and byte-exact bounded streams."""
    return run_bytes(
        ["git", "-C", str(root), *arguments],
        timeout=GIT_SECONDS_MAX,
        input_bytes=input_bytes,
        accepted=accepted,
        output_bytes_max=GIT_OUTPUT_BYTES_MAX,
    )


def measure(tree: bytes) -> Measurement:
    """Measure NUL-delimited `git ls-tree -r -l -z` records without path unquoting."""
    rows = tree.split(b"\0")
    if len(rows) > TREE_FILES_MAX + 1 or rows[-1] != b"":
        raise ValueError("Git tree is incomplete or exceeds 100000 records")
    files = size = symlinks = depth = manifests = 0
    for row in rows[:-1]:
        header, raw_path = row.split(b"\t", 1)
        mode, kind, _identity, raw_size = header.split()
        if kind == b"commit":
            continue
        if kind != b"blob" or mode not in (b"100644", b"100755", b"120000"):
            raise ValueError(f"unsupported Git entry: {header!r}")
        path = PurePosixPath(raw_path.decode("utf-8"))
        byte_count = int(raw_size)
        if byte_count < 0 or path.is_absolute() or ".." in path.parts:
            raise ValueError(f"invalid Git entry: {row!r}")
        files += 1
        size += byte_count
        symlinks += mode == b"120000"
        depth = max(depth, len(path.parts) - 1)
        manifests += path.name == "package.json"
    return Measurement(files, size, symlinks, depth, manifests)


def pins(path: Path = PINS) -> dict[str, Pin]:
    """Read the closed corpus set and reject invalid names or unconstrained pins."""
    with path.open("rb") as stream:
        document = CorpusPins.model_validate(tomllib.load(stream))
    result: dict[str, Pin] = {}
    for name in ("bun", "nextjs", "fastapi"):
        row = getattr(document, name)
        result[name] = Pin(
            name,
            row.repository,
            row.tag,
            row.commit,
            Measurement(
                row.files, row.bytes, row.symlinks, row.depth, row.package_json
            ),
            row.oversized_path,
            row.oversized_bytes,
            row.seconds,
        )
    return result
