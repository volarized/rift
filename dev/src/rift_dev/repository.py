"""Read this repository's Git state through dulwich, a Git implementation in Python.

dulwich reads the object store, the refs, and the worktree list itself, and talks
to a remote on its own, so reading starts no `git` process and parses no
porcelain text. Writing a signed tag and pushing it stay with `git` (see
`release_tag`), which signs and authenticates with the user's own configuration.

A remote read over HTTPS verifies the server's certificate whatever
`http.sslVerify` says in the Git configuration: the release reads the commit it
tags from here.
"""

from __future__ import annotations

import os
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from pathlib import Path

from dulwich.client import (
    AuthCallbackPoolManager,
    GitClient,
    default_urllib3_manager,
    get_transport_and_path,
)
from dulwich.object_store import tree_lookup_path
from dulwich.objects import Blob, Commit, ObjectID
from dulwich.refs import Ref
from dulwich.repo import Repo
from dulwich.worktree import list_worktrees

from rift_dev.commands import REPOSITORY

ORIGIN = b"origin"
HTTP_SCHEMES = ("https://", "http://")


@contextmanager
def opened(path: Path = REPOSITORY) -> Iterator[Repo]:
    """The repository checked out at `path`, closed when the context ends."""
    with Repo(str(path)) as repository:
        yield repository


def worktrees(path: Path = REPOSITORY) -> list[Path]:
    """Every checkout of the repository at `path` whose directory still exists.

    A linked worktree lists only itself, so the list is read from the main
    checkout, which holds the shared `.git` directory. Entries Git calls
    prunable, whose directory is gone, are left out.
    """
    with opened(path) as current:
        common = Path(os.path.normpath(current.commondir()))
    with opened(common.parent) as main:
        return [Path(tree.path) for tree in list_worktrees(main) if not tree.prunable]


def origin(path: Path = REPOSITORY) -> tuple[GitClient, str]:
    """A client for the `origin` remote and the path it addresses the repository by."""
    with opened(path) as repository:
        url = repository.get_config_stack().get((b"remote", ORIGIN), b"url").decode()
    if url.startswith(HTTP_SCHEMES):
        verified = default_urllib3_manager(config=None, cert_reqs="CERT_REQUIRED")
        # Only a Git configuration's credentials make dulwich wrap the pool.
        if isinstance(verified, AuthCallbackPoolManager):
            raise TypeError("a pool built without configuration carries no credentials")
        return get_transport_and_path(url, pool_manager=verified)
    return get_transport_and_path(url)


def remote_ref(ref: bytes, path: Path = REPOSITORY) -> bytes | None:
    """The object `origin` holds at `ref`, or `None` when it holds no such ref."""
    client, remote_path = origin(path)
    return client.get_refs(remote_path.encode(), ref_prefix=[ref]).refs.get(ref)


def fetch(ref: bytes, path: Path = REPOSITORY) -> bytes:
    """Fetches `origin`'s `ref` into the object store and returns the object it names.

    Only that ref's history is requested, at full depth: a depth would make the
    repository shallow.
    """
    client, remote_path = origin(path)
    wanted: list[ObjectID] = []

    def want(refs: Mapping[Ref, ObjectID], depth: int | None = None) -> list[ObjectID]:
        found = refs.get(Ref(ref))
        if found is None:
            raise LookupError(f"origin holds no {ref.decode()}")
        wanted.append(found)
        return wanted

    with opened(path) as repository:
        client.fetch(
            remote_path.encode(), repository, determine_wants=want, ref_prefix=[ref]
        )
    return wanted[0]


def commit(repository: Repo, identity: bytes) -> Commit:
    """The commit `identity` names in `repository`."""
    found = repository[identity]
    if not isinstance(found, Commit):
        raise TypeError(f"{identity.decode()} is not a commit")
    return found


def file_at(identity: bytes, file: str, path: Path = REPOSITORY) -> bytes:
    """The bytes `file` holds in the tree of commit `identity`."""
    with opened(path) as repository:
        tree = commit(repository, identity).tree
        _mode, blob = tree_lookup_path(repository.__getitem__, tree, file.encode())
        found = repository[blob]
        if not isinstance(found, Blob):
            raise TypeError(f"{file} is not a file in {identity.decode()}")
        return found.data


def summary(identity: bytes, path: Path = REPOSITORY) -> str:
    """Commit `identity`'s abbreviated id and subject, as `git log --oneline` prints."""
    with opened(path) as repository:
        message = commit(repository, identity).message.decode("utf-8", "replace")
    subject = message.splitlines()[0] if message else ""
    return f"{identity.decode()[:7]} {subject}"
