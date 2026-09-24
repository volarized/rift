"""Tag the commit `origin/main` names right now, and push the tag.

The command reads that commit from the remote, so the local checkout's branch and
its uncommitted work decide nothing. Pushing the tag starts `rift-release`: six
target archives, the checksum manifest, the GitHub release, and the docs deploy.

The remote reads go through `repository`. Signing and pushing run `git` itself:
it signs with the user's signing configuration, and it authenticates through the
user's credential helper, which dulwich never calls. The push verifies GitHub's
certificate even where the Git configuration turns verification off.
"""

from __future__ import annotations

import re

import tomllib

from rift_dev import repository
from rift_dev.commands import REPOSITORY, CommandFailed, GitCommand, fail

TAG = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
MAIN = b"refs/heads/main"


def declared_version(manifest: str) -> str:
    """The workspace version a root `Cargo.toml` declares."""
    return tomllib.loads(manifest)["workspace"]["package"]["version"]


def release(tag: str) -> None:
    """Signs `tag` onto `origin/main` once the tag is new and the version matches."""
    if not TAG.match(tag):
        fail(f"release tag must match vX.Y.Z: {tag}")
    if repository.remote_ref(f"refs/tags/{tag}".encode()) is not None:
        fail(f"origin already carries {tag}")
    commit = repository.fetch(MAIN)
    manifest = repository.file_at(commit, "Cargo.toml").decode("utf-8")
    declared = declared_version(manifest)
    if declared != tag.removeprefix("v"):
        fail(
            f"origin/main declares {declared}; "
            f"bump the workspace version before tagging {tag}"
        )
    print(f"tagging {repository.summary(commit)}")
    GitCommand(
        REPOSITORY, "tag", "--sign", "--message", f"Rift {tag}", tag, commit.decode()
    ).run()
    try:
        GitCommand(
            REPOSITORY,
            "-c",
            "http.sslVerify=true",
            "push",
            "--quiet",
            "origin",
            f"refs/tags/{tag}",
        ).run()
    except CommandFailed:
        GitCommand(REPOSITORY, "tag", "--delete", tag).run()
        raise
    print(f"{tag} pushed; watch with: gh run list --workflow rift-release")
