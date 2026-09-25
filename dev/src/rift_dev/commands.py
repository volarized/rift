"""Run every external program rift-dev starts through one builder, `Command`.

A command is built up before it runs, then runs in one of three ways:

- `run` streams the program's output to the terminal after echoing its command
  line, the way `just` echoes a recipe line. The program shares the terminal's
  process group, so Ctrl-C reaches it the way it reaches the recipe.
- `output` and `output_bytes` capture stdout within a byte bound and a deadline.
  The program runs in a Unix process group or a Windows job this module owns, so
  no descendant outlives the call, whatever it did.
- `spawn` starts a long-running program, such as a Rift server, under the same
  ownership until the context it returns closes.

A status the command does not accept raises `CommandFailed`. `GitCommand`,
`CargoCommand`, and `DockerCommand` name the programs rift-dev runs most. This
module is the only one that touches the operating system's process API, and the
platform decides how a program is owned: a caller never names it.
"""

from __future__ import annotations

import os
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import AbstractContextManager, contextmanager, nullcontext
from contextvars import ContextVar
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, BinaryIO, Final, Protocol, Self, cast

import psutil

REPOSITORY = Path(__file__).resolve().parents[3]

OUTPUT_BYTES_MAX: Final = 4 * 1024 * 1024
COMMAND_SECONDS_MAX: Final = 300.0
JOIN_SECONDS_MAX: Final = 10.0
KILL_SECONDS_MAX: Final = 1.0
INPUT_BYTES_MAX: Final = 16 * 1024 * 1024
OWNER_ENV: Final = "_RIFT_TEST_PROCESS_OWNERS"
OWNER_COUNT_MAX: Final = 64
OWNER_BYTES_MAX: Final = OWNER_COUNT_MAX * 33 - 1
PROCESS_COUNT_MAX: Final = 65536
_OWNERS: ContextVar[tuple[str, ...]] = ContextVar("rift_process_owners", default=())

Argument = str | Path


class CommandFailed(RuntimeError):
    """A program exited with a status its command does not accept."""

    def __init__(self, command: Command, status: int, detail: str) -> None:
        message = f"{command.name} exited {status}"
        super().__init__(f"{message}: {detail}" if detail else message)
        self.status = status


@dataclass(frozen=True, slots=True)
class Completion:
    """How a streamed command ended: its exit status, one the command accepts."""

    status: int


class Command:
    """One program and its arguments, environment, and bounds, built before it runs.

    Each `with_` method changes this command and returns it, so a call site can
    chain them or add to a command conditionally:

        command = CargoCommand("nextest", "run")
        if archive is not None:
            command.with_args("--archive-file", archive)
        command.run()

    The program runs from the repository root and inherits this process's
    environment unless the command says otherwise.
    """

    def __init__(self, program: Argument, *arguments: Argument) -> None:
        self.program = str(program)
        self.arguments = [str(argument) for argument in arguments]
        self.directory = REPOSITORY
        self.overlay: dict[str, str] = {}
        self.replacement: dict[str, str] | None = None
        self.timeout_seconds: float | None = None
        self.deadline: float | None = None
        self.input_bytes = b""
        self.accepted: tuple[int, ...] = (0,)
        self.output_bytes_max = OUTPUT_BYTES_MAX

    def with_args(self, *arguments: Argument) -> Self:
        """Appends `arguments` after the ones the command already carries."""
        self.arguments.extend(str(argument) for argument in arguments)
        return self

    def with_env(self, **variables: str) -> Self:
        """Sets `variables` over the environment the program would otherwise see."""
        self.overlay.update(variables)
        return self

    def with_environment(self, environment: Mapping[str, str]) -> Self:
        """Gives the program exactly `environment` instead of this process's own."""
        self.replacement = dict(environment)
        return self

    def with_cwd(self, directory: Path) -> Self:
        """Runs the program from `directory` instead of the repository root."""
        self.directory = directory
        return self

    def with_timeout(self, seconds: float) -> Self:
        """Ends the program once it has run for `seconds`."""
        self.timeout_seconds = seconds
        return self

    def with_deadline(self, deadline: float | None) -> Self:
        """Ends the program at the monotonic time `deadline` as well; `None` sets none."""
        self.deadline = deadline
        return self

    def with_input(self, data: bytes) -> Self:
        """Writes `data` to the program's stdin, which is empty otherwise."""
        if len(data) > INPUT_BYTES_MAX:
            raise ValueError("command input exceeded its byte bound")
        self.input_bytes = data
        return self

    def with_accepted(self, *statuses: int) -> Self:
        """Accepts `statuses` as the exit statuses of a program that succeeded."""
        self.accepted = statuses
        return self

    def with_output_limit(self, bytes_max: int) -> Self:
        """Fails a captured program that writes more than `bytes_max` to a stream."""
        self.output_bytes_max = bytes_max
        return self

    @property
    def name(self) -> str:
        """The program's file name, the way an error names it."""
        return Path(self.program).name

    @property
    def argv(self) -> list[str]:
        """The program and its arguments, as the operating system receives them."""
        return [self.program, *self.arguments]

    def environment(self) -> dict[str, str] | None:
        """The program's complete environment, or `None` when it inherits this one."""
        if self.replacement is None and not self.overlay:
            return None
        base = dict(os.environ) if self.replacement is None else self.replacement
        return base | self.overlay

    def __str__(self) -> str:
        return shlex.join(self.argv)

    def run(self) -> Completion:
        """Streams the program's output to the terminal and waits for it to exit."""
        print(self, file=sys.stderr, flush=True)
        try:
            completed = subprocess.run(
                self.argv,
                cwd=self.directory,
                env=self.environment(),
                input=self.input_bytes or None,
                timeout=self._streamed_timeout(),
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(f"{self.name} exceeded {error.timeout}s") from error
        if completed.returncode not in self.accepted:
            raise CommandFailed(self, completed.returncode, "")
        return Completion(completed.returncode)

    def output(self) -> str:
        """The program's stdout as text, captured under ownership and bounds."""
        return self.output_bytes().decode("utf-8", errors="replace")

    def output_bytes(self) -> bytes:
        """The program's exact stdout, with stderr, input, and lifetime bounded.

        Stderr is echoed once the program succeeds and carried in the failure
        otherwise. On Unix, detached children must keep the inherited owner token
        and a readable same-user environment; Windows jobs own descendants
        independently.
        """
        timeout = self._captured_timeout()
        if not self.argv[0] or self.output_bytes_max <= 0:
            raise ValueError("command program and output bound must be set")
        with tempfile.TemporaryFile() as stdin, termination_handler():
            stdin.write(self.input_bytes)
            stdin.seek(0)
            output = self._capture(stdin, timeout)
            if self.deadline is not None and time.monotonic() > self.deadline:
                raise RuntimeError("command deadline expired during cleanup")
            return output

    @contextmanager
    def spawn(self, log: BinaryIO | None = None) -> Iterator[Process]:
        """Starts the program and owns it, with every descendant, until the context ends.

        Stdout and stderr go together to `log`, or to the process's `stdout` pipe
        when no log is given. Leaving the context stops whatever is still running.
        """
        with tempfile.TemporaryFile() as stdin:
            owned = owned_process(
                self.argv,
                self.environment(),
                self.directory,
                stdin,
                stdout=log if log is not None else subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            with owned as process:
                yield Process(process)

    def _streamed_timeout(self) -> float | None:
        if self.deadline is None:
            return self.timeout_seconds
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError("command deadline expired")
        return (
            remaining
            if self.timeout_seconds is None
            else min(self.timeout_seconds, remaining)
        )

    def _captured_timeout(self) -> float:
        timeout = self.timeout_seconds or COMMAND_SECONDS_MAX
        if self.deadline is not None:
            timeout = min(timeout, self.deadline - time.monotonic())
            if timeout <= 0:
                raise RuntimeError("command deadline expired")
        if timeout <= 0:
            raise ValueError("command timeout must be positive")
        return timeout

    def _capture(self, stdin: BinaryIO, timeout: float) -> bytes:
        """Joins both drains after process cleanup, including after a failure."""
        failed = threading.Event()
        drains: list[Drain] = []
        readers: list[threading.Thread] = []
        try:
            with owned_process(
                self.argv, self.environment(), self.directory, stdin
            ) as process:
                assert process.stdout is not None and process.stderr is not None
                drains = [
                    Drain(source, self.output_bytes_max, failed)
                    for source in (process.stdout, process.stderr)
                ]
                readers = [
                    threading.Thread(target=drain.read, daemon=True) for drain in drains
                ]
                for reader in readers:
                    reader.start()
                ends = time.monotonic() + timeout
                while (
                    process.poll() is None
                    and not failed.is_set()
                    and time.monotonic() < ends
                ):
                    time.sleep(0.02)
                if process.poll() is None and not failed.is_set():
                    raise RuntimeError(f"{self.name} exceeded {timeout}s")
        finally:
            for reader in readers:
                reader.join(timeout=JOIN_SECONDS_MAX)
            if any(reader.is_alive() for reader in readers):
                raise RuntimeError("command stream did not close within its bound")
            for drain in drains:
                drain.source.close()
        for drain in drains:
            if drain.error is not None:
                raise RuntimeError(
                    f"command stream failed: {drain.error}"
                ) from drain.error
        stdout, stderr = drains
        detail = stderr.data.decode("utf-8", errors="replace")
        if process.returncode not in self.accepted:
            raise CommandFailed(self, process.returncode, detail)
        if detail:
            sys.stderr.write(detail)
        return bytes(stdout.data)


class GitCommand(Command):
    """`git`, run against one repository through `-C`."""

    def __init__(self, repository: Path, *arguments: Argument) -> None:
        super().__init__("git", "-C", repository, *arguments)


class CargoCommand(Command):
    """`cargo`, run from the repository root, where `rust-toolchain.toml` selects it."""

    def __init__(self, *arguments: Argument) -> None:
        super().__init__("cargo", *arguments)


class DockerCommand(Command):
    """`docker`, run against the local engine."""

    def __init__(self, *arguments: Argument) -> None:
        super().__init__("docker", *arguments)


class Process:
    """A program `Command.spawn` started, owned until that context ends."""

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        self._process = process

    @property
    def pid(self) -> int:
        return self._process.pid

    @property
    def stdout(self) -> IO[bytes] | None:
        """The pipe stdout and stderr arrive on, or `None` when they go to a log."""
        return self._process.stdout

    @property
    def returncode(self) -> int | None:
        return self._process.returncode

    def poll(self) -> int | None:
        """The exit status once the program has exited, `None` while it runs."""
        return self._process.poll()

    def wait(self, timeout: float) -> int:
        """Waits up to `timeout` seconds for the exit status.

        Raises `TimeoutError` when the program is still running at the bound.
        """
        try:
            return self._process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise TimeoutError(
                f"process {self.pid} still running after {timeout}s"
            ) from error

    def interrupt(self) -> None:
        """Sends Ctrl-C's signal to the process group, or terminates on Windows.

        A foreground Rift server stops cleanly on the interrupt alone.
        """
        if sys.platform == "win32":
            self._process.terminate()
            return
        signal_group(self._process.pid, signal.SIGINT)


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
    """Track cooperating Unix children around a transport that creates its own process.

    The MCP SDK's stdio transport starts the server itself and receives this
    complete environment explicitly. Windows keeps its existing transport
    cleanup; this scope changes only Unix ownership.
    """
    if sys.platform == "win32":
        yield dict(os.environ if environment is None else environment)
        return
    owner = GroupOwner(environment)
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


class ProcessOwner(Protocol):
    """What owns a started program and every descendant it starts.

    `process_owner` picks the one this platform provides; `owned_process` is its
    only user.
    """

    @property
    def environment(self) -> Mapping[str, str] | None:
        """The complete environment the program starts with, `None` to inherit."""
        ...

    @property
    def start_new_session(self) -> bool:
        """Whether the program leads a new Unix session and process group."""
        ...

    @property
    def creation_flags(self) -> int:
        """The Windows process creation flags the program starts with."""
        ...

    def active(self) -> AbstractContextManager[None]:
        """The scope nested commands inherit this owner in."""
        ...

    def adopt(self, process: subprocess.Popen[bytes]) -> None:
        """Takes `process` over before its first instruction can start a child."""
        ...

    def release(self, process: subprocess.Popen[bytes] | None) -> None:
        """Stops `process` and everything it started that is still running."""
        ...


def process_owner(environment: Mapping[str, str] | None) -> ProcessOwner:
    """The owner this platform provides: a Windows job, or a Unix process group."""
    if sys.platform == "win32":
        return JobOwner(environment)
    return GroupOwner(environment)


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


class GroupOwner:
    """A Unix process group, and a token that finds children after they leave it.

    The process group covers ordinary children. A private owner token in the
    environment also identifies children after setsid and parent exit, without
    relying on retained parent links. This is a contract for the harness's
    cooperating commands, not OS containment: detached children that remove the
    token or make their environment unreadable are unsupported. Process
    environments are inspected only for this token, never logged.
    """

    start_new_session = True
    creation_flags = 0

    def __init__(self, environment: Mapping[str, str] | None) -> None:
        self.environment = inherit_owners(environment)
        owners = [
            owner for owner in self.environment.get(OWNER_ENV, "").split(",") if owner
        ]
        if len(owners) >= OWNER_COUNT_MAX:
            raise ValueError("command owner chain exceeds its bound")
        self.token = uuid.uuid4().hex
        self.environment[OWNER_ENV] = ",".join([*owners, self.token])
        self.started: float | None = None
        self.pid: int | None = None
        self.uid = os.getuid()

    def attach(self, pid: int) -> None:
        """Read the owned process's creation time before Popen can reap its exit."""
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

    def adopt(self, process: subprocess.Popen[bytes]) -> None:
        self.attach(process.pid)

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

    def release(self, process: subprocess.Popen[bytes] | None) -> None:
        """Stop captured children and detached children retaining the owner token."""
        if process is None:
            return
        deadline = time.monotonic() + JOIN_SECONDS_MAX
        children: list[psutil.Process] = []
        try:
            children.extend(descendants(process.pid))
            children.extend(self.processes())
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
                children.extend(self.processes())
            finally:
                try:
                    signal_group(process.pid, signal.SIGKILL)
                finally:
                    # Process.kill checks identity even after a middle owner has exited.
                    # Discovery failures must still stop the known group and identities.
                    stop_children(children, deadline)


class Handle(Protocol):
    """The PyHANDLE operations used here, returned by pywin32's native wrappers."""

    def __int__(self) -> int: ...

    def Close(self) -> None: ...


class JobOwner:
    """A Windows job that kills every process in it once its only handle closes.

    The program starts suspended and joins the job before its first instruction
    runs, so every descendant is in the job too, including after its parent exits.
    """

    start_new_session = False

    def __init__(self, environment: Mapping[str, str] | None) -> None:
        import win32con
        import win32job

        self.environment = environment
        self.creation_flags = win32con.CREATE_SUSPENDED
        # pywin32 b312 requires a WCHAR string and returns PyHANDLE; its stub
        # incorrectly declares the return as None. A unique name avoids sharing jobs.
        create = cast(Callable[[object, str], Handle], win32job.CreateJobObject)
        self.handle = create(None, f"rift-test-{uuid.uuid4().hex}")
        try:
            limits = win32job.QueryInformationJobObject(
                int(self.handle), win32job.JobObjectExtendedLimitInformation
            )
            limits["BasicLimitInformation"]["LimitFlags"] = (
                win32job.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            )
            win32job.SetInformationJobObject(
                int(self.handle), win32job.JobObjectExtendedLimitInformation, limits
            )
        except Exception:
            self.handle.Close()
            raise

    def active(self) -> AbstractContextManager[None]:
        return nullcontext()

    def adopt(self, process: subprocess.Popen[bytes]) -> None:
        """Attach the suspended process to the job, then resume its threads."""
        import win32api
        import win32con
        import win32job

        access = win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE
        handle = cast(
            Handle, cast(object, win32api.OpenProcess(access, False, process.pid))
        )
        try:
            win32job.AssignProcessToJobObject(int(self.handle), int(handle))
        finally:
            handle.Close()
        psutil.Process(process.pid).resume()

    def release(self, process: subprocess.Popen[bytes] | None) -> None:
        """Close the only job handle, which kills every process still in it."""
        self.handle.Close()


@contextmanager
def owned_process(
    command: Sequence[str],
    environment: Mapping[str, str] | None,
    cwd: Path | None,
    stdin: BinaryIO,
    *,
    stdout: IO[bytes] | int = subprocess.PIPE,
    stderr: int = subprocess.PIPE,
) -> Iterator[subprocess.Popen[bytes]]:
    """Start `command` under this platform's owner, and stop what remains on exit."""
    owner = process_owner(environment)
    process: subprocess.Popen[bytes] | None = None
    with owner.active():
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=owner.environment,
                stdin=stdin,
                stdout=stdout,
                stderr=stderr,
                start_new_session=owner.start_new_session,
                creationflags=owner.creation_flags,
            )
            owner.adopt(process)
            yield process
        finally:
            try:
                owner.release(process)
            finally:
                if process is not None:
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=JOIN_SECONDS_MAX)


def fail(message: str) -> None:
    """Ends the recipe with status 1 after printing `message` as an error."""
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)
