"""Tag the commit `origin/main` names right now, and push the tag.

The command reads that commit from the remote, so the local checkout's branch and
its uncommitted work decide nothing. Pushing the tag starts `rift-release`: six
target archives, the checksum manifest, the GitHub release, and the docs deploy.
"""

from __future__ import annotations

import re
import subprocess

import tomllib

from rift_dev.commands import REPOSITORY, fail, output, run

TAG = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def declared_version(manifest: str) -> str:
    """The workspace version a root `Cargo.toml` declares."""
    return tomllib.loads(manifest)["workspace"]["package"]["version"]


def release(tag: str) -> None:
    """Signs `tag` onto `origin/main` once the tag is new and the version matches."""
    if not TAG.match(tag):
        fail(f"release tag must match vX.Y.Z: {tag}")
    existing = subprocess.run(
        ["git", "ls-remote", "--exit-code", "--tags", "origin", f"refs/tags/{tag}"],
        cwd=REPOSITORY,
        capture_output=True,
        check=False,
    )
    if existing.returncode == 0:
        fail(f"origin already carries {tag}")
    run("git", "fetch", "--quiet", "origin", "main")
    commit = output("git", "rev-parse", "FETCH_HEAD").strip()
    declared = declared_version(output("git", "show", f"{commit}:Cargo.toml"))
    if declared != tag.removeprefix("v"):
        fail(
            f"origin/main declares {declared}; "
            f"bump the workspace version before tagging {tag}"
        )
    summary = output("git", "--no-pager", "log", "-1", "--format=%h %s", commit)
    print(f"tagging {summary.strip()}")
    run("git", "tag", "--sign", "--message", f"Rift {tag}", tag, commit)
    pushed = subprocess.run(
        ["git", "push", "--quiet", "origin", f"refs/tags/{tag}"],
        cwd=REPOSITORY,
        check=False,
    )
    if pushed.returncode != 0:
        run("git", "tag", "--delete", tag)
        raise SystemExit(1)
    print(f"{tag} pushed; watch with: gh run list --workflow rift-release")
