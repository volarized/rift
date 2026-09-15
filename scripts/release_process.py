"""Run test commands with bounded streams and owned child-process cleanup."""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Iterator, Mapping, Sequence
from contextlib import contextmanager, nullcontext
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, BinaryIO, Final

import psutil
from release_process_unix import UnixOwner

OUTPUT_BYTES_MAX: Final = 4 * 1024 * 1024
COMMAND_SECONDS_MAX: Final = 300.0
JOIN_SECONDS_MAX: Final = 10.0
KILL_SECONDS_MAX: Final = 1.0
INPUT_BYTES_MAX: Final = 16 * 1024 * 1024


@dataclass
class Drain:
    """Keep one sentinel byte past a stream ceiling, reporting read failures to its owner."""

    source: IO[bytes]
    maximum: int
    failed: threading.Event
    data: bytearray = field(default_factory=bytearray)
    error: Exception | None = None

    def read(self) -> None:
        try:
            while len(self.data) <= self.maximum:
                chunk = self.source.read(min(65536, self.maximum + 1 - len(self.data)))
                if not chunk:
                    return
                self.data.extend(chunk)
            raise RuntimeError(f"command output exceeded {self.maximum} bytes")
        except Exception as error:  # noqa: BLE001 - capture re-raises the reader failure.
            self.error = error
            self.failed.set()


@contextmanager
def termination_handler() -> Iterator[None]:
    """Turn SIGTERM into unwinding so nextest's grace period reaches child cleanup."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    previous = signal.getsignal(signal.SIGTERM)

    def interrupted(_number: int, _frame: object) -> None:
        # Selectors treat InterruptedError as an interrupted OS wait and suppress it.
        raise RuntimeError("test command interrupted by SIGTERM")

    signal.signal(signal.SIGTERM, interrupted)
    try:
        yield
    finally:
        signal.signal(signal.SIGTERM, previous)


def signal_group(pid: int, number: signal.Signals) -> None:
    """Signal the owned group; Darwin can return EPERM for a group containing only zombies."""
    try:
        os.killpg(pid, number)
    except ProcessLookupError:
        return
    except PermissionError:
        if sys.platform != "darwin":
            raise
        # XNU killpg1 excludes SZOMB, then returns EPERM when nfound is zero.
        # Accept that race only after proving the group has no live member.
        for member in psutil.process_iter():
            try:
                if (
                    os.getpgid(member.pid) == pid
                    and member.status() != psutil.STATUS_ZOMBIE
                ):
                    raise
            except (ProcessLookupError, psutil.NoSuchProcess):
                continue


def descendants(pid: int) -> list[psutil.Process]:
    """Retain process identities before termination can remove their parent links."""
    try:
        return psutil.Process(pid).children(recursive=True)
    except psutil.NoSuchProcess:
        return []


def stop_children(children: Sequence[psutil.Process], deadline: float) -> None:
    """Kill captured identities, then check their exit within the cleanup deadline."""
    for child in reversed(children):
        try:
            child.kill()
        except psutil.NoSuchProcess:
            continue
    _, alive = psutil.wait_procs(children, timeout=max(0, deadline - time.monotonic()))
    for child in alive:
        try:
            if child.is_running() and child.status() != psutil.STATUS_ZOMBIE:
                raise RuntimeError(f"command descendant {child.pid} did not stop")
        except psutil.NoSuchProcess:
            continue


@contextmanager
def owned_environment(
    environment: Mapping[str, str] | None,
) -> Iterator[dict[str, str]]:
    """Track cooperating Unix children around a transport that creates its own subprocess.

    SDK transports receive this complete environment explicitly. Windows keeps
    its existing transport cleanup; this scope changes only Unix ownership.
    """
    if sys.platform == "win32":
        yield dict(os.environ if environment is None else environment)
        return
    owner = UnixOwner(environment)
    owner.attach(os.getpid())
    with owner.active():
        try:
            yield owner.environment
        finally:
            deadline = time.monotonic() + JOIN_SECONDS_MAX
            children: list[psutil.Process] = []
            try:
                children.extend(owner.processes())
            finally:
                stop_children(children, deadline)


def stop_unix_process(process: subprocess.Popen[bytes], owner: UnixOwner) -> None:
    """Stop captured children and detached children retaining the inherited owner token."""
    deadline = time.monotonic() + JOIN_SECONDS_MAX
    children: list[psutil.Process] = []
    try:
        children.extend(descendants(process.pid))
        children.extend(owner.processes())
        if process.poll() is None:
            signal_group(process.pid, signal.SIGTERM)
            try:
                process.wait(
                    timeout=max(0, deadline - time.monotonic() - KILL_SECONDS_MAX)
                )
            except subprocess.TimeoutExpired:
                pass
    finally:
        try:
            children.extend(descendants(process.pid))
            children.extend(owner.processes())
        finally:
            try:
                signal_group(process.pid, signal.SIGKILL)
            finally:
                # Process.kill checks identity even after a middle owner has exited.
                # Discovery failures must still stop the known group and identities.
                stop_children(children, deadline)


@contextmanager
def owned_process(
    command: Sequence[str],
    environment: Mapping[str, str] | None,
    cwd: Path | None,
    stdin: BinaryIO,
    *,
    stderr: int = subprocess.PIPE,
) -> Iterator[subprocess.Popen[bytes]]:
    """Own a Unix process group or Windows job before the command can spawn children."""
    job = None
    owner = None
    flags = 0
    if sys.platform == "win32":
        from release_process_windows import WindowsJob

        job = WindowsJob()
        flags = job.creation_flags
    else:
        owner = UnixOwner(environment)
    process: subprocess.Popen[bytes] | None = None
    with owner.active() if owner is not None else nullcontext():
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=owner.environment if owner is not None else environment,
                stdin=stdin,
                stdout=subprocess.PIPE,
                stderr=stderr,
                start_new_session=sys.platform != "win32",
                creationflags=flags,
            )
            if owner is not None:
                owner.attach(process.pid)
            if job is not None:
                job.assign(process.pid)
                psutil.Process(process.pid).resume()
            yield process
        finally:
            try:
                if job is not None:
                    job.close()
                elif process is not None and owner is not None:
                    stop_unix_process(process, owner)
            finally:
                if process is not None:
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=JOIN_SECONDS_MAX)


def run_bytes(
    command: Sequence[str],
    *,
    environment: Mapping[str, str] | None = None,
    cwd: Path | None = None,
    timeout: float = COMMAND_SECONDS_MAX,
    deadline: float | None = None,
    input_bytes: bytes | None = None,
    accepted: Sequence[int] = (0,),
    output_bytes_max: int = OUTPUT_BYTES_MAX,
) -> bytes:
    """Return exact stdout, with separately bounded stderr, input, and process lifetime.

    Children inherit the environment unless the caller supplies its complete
    replacement. Stream drains stop after their sentinel byte.
    On Unix, detached children must retain the inherited owner token and a
    readable same-user environment. Windows jobs own descendants independently.
    """
    if deadline is not None:
        timeout = min(timeout, deadline - time.monotonic())
        if timeout <= 0:
            raise RuntimeError("command deadline expired")
    if not command or timeout <= 0 or output_bytes_max <= 0:
        raise ValueError("command, timeout, and output bound must be positive")
    if input_bytes is not None and len(input_bytes) > INPUT_BYTES_MAX:
        raise ValueError("command input exceeded its byte bound")
    with tempfile.TemporaryFile() as stdin, termination_handler():
        stdin.write(input_bytes or b"")
        stdin.seek(0)
        output = capture(
            command, environment, cwd, stdin, timeout, accepted, output_bytes_max
        )
        if deadline is not None and time.monotonic() > deadline:
            raise RuntimeError("command deadline expired during cleanup")
        return output


def capture(
    command: Sequence[str],
    environment: Mapping[str, str] | None,
    cwd: Path | None,
    stdin: BinaryIO,
    timeout: float,
    accepted: Sequence[int],
    maximum: int,
) -> bytes:
    """Join both drains after process cleanup, including exceptions and external termination."""
    failed = threading.Event()
    drains: list[Drain] = []
    readers: list[threading.Thread] = []
    try:
        with owned_process(command, environment, cwd, stdin) as process:
            assert process.stdout is not None and process.stderr is not None
            drains = [
                Drain(source, maximum, failed)
                for source in (process.stdout, process.stderr)
            ]
            readers = [
                threading.Thread(target=drain.read, daemon=True) for drain in drains
            ]
            for reader in readers:
                reader.start()
            deadline = time.monotonic() + timeout
            while (
                process.poll() is None
                and not failed.is_set()
                and time.monotonic() < deadline
            ):
                time.sleep(0.02)
            if process.poll() is None and not failed.is_set():
                raise RuntimeError(f"{Path(command[0]).name} exceeded {timeout}s")
    finally:
        for reader in readers:
            reader.join(timeout=JOIN_SECONDS_MAX)
        if any(reader.is_alive() for reader in readers):
            raise RuntimeError("command stream did not close within its bound")
        for drain in drains:
            drain.source.close()
    for drain in drains:
        if drain.error is not None:
            raise RuntimeError(f"command stream failed: {drain.error}") from drain.error
    stdout, stderr = drains
    detail = stderr.data.decode("utf-8", errors="replace")
    if process.returncode not in accepted:
        raise RuntimeError(
            f"{Path(command[0]).name} exited {process.returncode}: {detail}"
        )
    if detail:
        sys.stderr.write(detail)
    return bytes(stdout.data)


def run(
    command: Sequence[str],
    *,
    environment: Mapping[str, str] | None = None,
    cwd: Path | None = None,
    timeout: float = COMMAND_SECONDS_MAX,
    deadline: float | None = None,
) -> str:
    """Run a text command through the same bounded byte runner."""
    return run_bytes(
        command, environment=environment, cwd=cwd, timeout=timeout, deadline=deadline
    ).decode("utf-8", errors="replace")
