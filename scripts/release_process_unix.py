"""Track Unix harness children that retain their inherited owner environment.

Process groups cover ordinary children. A private owner token also identifies
children after setsid and parent exit, without relying on retained parent links.
This is a contract for the harness's cooperating commands, not OS containment:
detached children that remove the token or make their environment unreadable are
unsupported. Process environments are inspected only for this token, never logged.
"""

from __future__ import annotations

import os
import uuid
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from contextvars import ContextVar
from typing import Final

import psutil

OWNER_ENV: Final = "_RIFT_TEST_PROCESS_OWNERS"
OWNER_COUNT_MAX: Final = 64
OWNER_BYTES_MAX: Final = OWNER_COUNT_MAX * 33 - 1
PROCESS_COUNT_MAX: Final = 65536
_OWNERS: ContextVar[tuple[str, ...]] = ContextVar("rift_process_owners", default=())


def inherit_owners(environment: Mapping[str, str] | None) -> dict[str, str]:
    """Preserve inherited owners even when a command replaces its other environment."""
    result = dict(os.environ if environment is None else environment)
    owners: list[str] = []
    for value in (os.environ.get(OWNER_ENV, ""), result.get(OWNER_ENV, "")):
        if len(value) > OWNER_BYTES_MAX:
            raise ValueError("command owner chain exceeds its byte bound")
        if value:
            owners.extend(value.split(","))
    owners.extend(_OWNERS.get())
    unique = tuple(dict.fromkeys(owners))
    if len(unique) > OWNER_COUNT_MAX or any(
        len(owner) != 32
        or any(character not in "0123456789abcdef" for character in owner)
        for owner in unique
    ):
        raise ValueError("command owner chain is invalid or exceeds its bound")
    if unique:
        result[OWNER_ENV] = ",".join(unique)
    return result


class UnixOwner:
    """Find same-user children by an inherited token after their ancestry disappears."""

    def __init__(self, environment: Mapping[str, str] | None) -> None:
        self.environment = inherit_owners(environment)
        owners = self.environment.get(OWNER_ENV, "").split(",")
        owners = [owner for owner in owners if owner]
        if len(owners) >= OWNER_COUNT_MAX:
            raise ValueError("command owner chain exceeds its bound")
        self.token = uuid.uuid4().hex
        self.environment[OWNER_ENV] = ",".join([*owners, self.token])
        self.started: float | None = None
        self.pid: int | None = None
        self.uid = os.getuid()

    def attach(self, pid: int) -> None:
        """Read the command's creation time before Popen can reap its exit."""
        self.started = psutil.Process(pid).create_time()
        self.pid = pid

    @contextmanager
    def active(self) -> Iterator[None]:
        """Pass owners to nested commands and explicit SDK environments in this context."""
        token = _OWNERS.set(tuple(self.environment[OWNER_ENV].split(",")))
        try:
            yield
        finally:
            _OWNERS.reset(token)

    def processes(self) -> Iterator[psutil.Process]:
        """Yield identities as observed, so a later failure cannot discard earlier matches."""
        if self.started is None:
            return
        for count, process in enumerate(psutil.process_iter()):
            if count >= PROCESS_COUNT_MAX:
                raise RuntimeError("process observation exceeded its count bound")
            try:
                # process_iter caches identities; refresh before checking a reused PID.
                process = psutil.Process(process.pid)
                if (
                    process.pid == self.pid
                    or process.create_time() < self.started
                    or process.uids().real != self.uid
                    or process.status() == psutil.STATUS_ZOMBIE
                ):
                    continue
                value = process.environ().get(OWNER_ENV, "")
                if len(value) <= OWNER_BYTES_MAX and self.token in value.split(","):
                    yield process
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                # Unreadable detached environments are outside this contract.
                continue
