"""Own Windows command descendants in a job before their first instruction runs."""

from __future__ import annotations

import uuid
from collections.abc import Callable
from typing import Protocol, cast

import win32api
import win32con
import win32job


class Handle(Protocol):
    """The PyHANDLE operations used here, returned by pywin32's native wrappers."""

    def __int__(self) -> int: ...

    def Close(self) -> None: ...


class WindowsJob:
    """Kill all descendants on close, including after the original parent exits."""

    creation_flags = win32con.CREATE_SUSPENDED

    def __init__(self) -> None:
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

    def assign(self, pid: int) -> None:
        """Attach the suspended process before psutil resumes its threads."""
        access = win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE
        process = cast(Handle, cast(object, win32api.OpenProcess(access, False, pid)))
        try:
            win32job.AssignProcessToJobObject(int(self.handle), int(process))
        finally:
            process.Close()

    def close(self) -> None:
        """Release the only job handle, which kills every process still in it."""
        self.handle.Close()
