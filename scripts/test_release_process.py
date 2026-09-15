#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Prove command limits, byte streams, environment inheritance, and descendant cleanup."""

from __future__ import annotations

import io
import os
import signal
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import psutil
from release_process import (
    OUTPUT_BYTES_MAX,
    Drain,
    owned_process,
    run,
    run_bytes,
    signal_group,
)


class ProcessTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "darwin", "XNU process-group zombie behavior")
    def test_darwin_zombie_group_is_complete_without_hiding_live_refusals(self) -> None:
        with (
            tempfile.TemporaryFile() as stdin,
            owned_process([sys.executable, "-c", "pass"], None, None, stdin) as process,
        ):
            deadline = time.monotonic() + 3
            while psutil.Process(process.pid).status() != psutil.STATUS_ZOMBIE:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.01)
            with self.assertRaises(PermissionError):
                os.killpg(process.pid, signal.SIGTERM)
            signal_group(process.pid, signal.SIGTERM)
            assert process.stdout is not None and process.stderr is not None
            process.stdout.close()
            process.stderr.close()
        with (
            patch("release_process.os.killpg", side_effect=PermissionError("denied")),
            self.assertRaisesRegex(PermissionError, "denied"),
        ):
            signal_group(os.getpgrp(), signal.SIGTERM)

    def test_byte_input_and_output_keep_nul_crlf_and_non_utf8(self) -> None:
        payload = b"name\x00\xff\r\n"
        command = [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read()); raise SystemExit(1)",
        ]
        self.assertEqual(
            run_bytes(command, input_bytes=payload, accepted=(0, 1)), payload
        )

    def test_reader_failure_reaches_owner(self) -> None:
        class FailedRead(io.BytesIO):
            def read(self, size: int | None = -1) -> bytes:
                raise OSError("fixture read failed")

        failed = threading.Event()
        drain = Drain(FailedRead(), 32, failed)
        drain.read()
        self.assertTrue(failed.is_set())
        self.assertIsInstance(drain.error, OSError)
        self.assertEqual(str(drain.error), "fixture read failed")

    def test_parent_exit_reaps_child_that_keeps_stdout_open(self) -> None:
        program = "import subprocess,sys; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); print(child.pid,flush=True)"
        pid = int(run([sys.executable, "-c", program], timeout=5).strip())
        try:
            child = psutil.Process(pid)
            self.assertEqual(child.status(), psutil.STATUS_ZOMBIE)
        except psutil.NoSuchProcess:
            pass

    @unittest.skipIf(
        sys.platform == "win32", "Windows uses job closure for termination"
    )
    def test_sigterm_unwinds_owner_and_reaps_descendants(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "child.pid"
            program = (
                "import os,pathlib,signal,subprocess,sys,time; "
                "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); "
                "os.kill(os.getppid(),signal.SIGTERM); time.sleep(30)"
            )
            with self.assertRaisesRegex(InterruptedError, "SIGTERM"):
                run([sys.executable, "-c", program, str(path)], timeout=5)
            pid = int(path.read_text())
            try:
                child = psutil.Process(pid)
                self.assertEqual(child.status(), psutil.STATUS_ZOMBIE)
            except psutil.NoSuchProcess:
                pass

    def test_child_environment_is_inherited_or_replaced_explicitly(self) -> None:
        command = [
            sys.executable,
            "-c",
            "import os; print(os.environ.get('RIFT_GATE_TEST', 'absent'))",
        ]
        with patch.dict(os.environ, {"RIFT_GATE_TEST": "inherited"}):
            self.assertEqual(run(command).strip(), "inherited")
            self.assertEqual(
                run(
                    command,
                    environment=dict(os.environ) | {"RIFT_GATE_TEST": "overlay"},
                ).strip(),
                "overlay",
            )
            self.assertEqual(run(command, environment={}).strip(), "absent")

    def test_failed_timed_out_and_flooding_children_fail_gate(self) -> None:
        for program in [
            "raise SystemExit(3)",
            "import time; time.sleep(30)",
            f"import sys; sys.stdout.write('x' * {OUTPUT_BYTES_MAX + 1}); sys.stdout.flush()",
        ]:
            with self.subTest(program=program), self.assertRaises(RuntimeError):
                run([sys.executable, "-c", program], timeout=0.5)

    def test_shared_deadline_cannot_reset_for_a_later_command(self) -> None:
        deadline = time.monotonic() - 1.0
        with self.assertRaisesRegex(RuntimeError, "deadline expired"):
            run([sys.executable, "-c", "raise SystemExit(0)"], deadline=deadline)
        with self.assertRaisesRegex(RuntimeError, "exceeded"):
            run(
                [sys.executable, "-c", "import time; time.sleep(10)"],
                timeout=5.0,
                deadline=time.monotonic() + 0.1,
            )

    def test_timeout_unwinds_nested_process_owners(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "child.pid"
            child = (
                "import os,pathlib,signal,sys,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)"
            )
            parent = (
                "import sys; from release_process import run; "
                "run([sys.executable, '-c', sys.argv[1], sys.argv[2]], timeout=30)"
            )
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "exceeded"):
                run(
                    [sys.executable, "-c", parent, child, str(path)],
                    cwd=Path(__file__).parent,
                    timeout=1,
                )
            self.assertLess(time.monotonic() - started, 13)
            pid = int(path.read_text())
            try:
                descendant = psutil.Process(pid)
                self.assertEqual(descendant.status(), psutil.STATUS_ZOMBIE)
            except psutil.NoSuchProcess:
                pass


if __name__ == "__main__":
    unittest.main()
