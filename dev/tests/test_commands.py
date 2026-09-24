"""Prove command limits, byte streams, environment inheritance, and descendant cleanup.

Three tests start a process the way the MCP SDK's stdio transport does, through
`anyio.open_process`, because `owned_environment` exists for processes a
transport starts outside `Command`.
"""

from __future__ import annotations

import io
import json
import os
import selectors
import signal
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import anyio
import psutil
from rift_dev.commands import (
    OUTPUT_BYTES_MAX,
    OWNER_ENV,
    Command,
    Drain,
    owned_environment,
    owned_process,
    signal_group,
    termination_handler,
)

SLEEPER = [sys.executable, "-c", "import time; time.sleep(60)"]


class ProcessTests(unittest.TestCase):
    @unittest.skipIf(sys.platform == "win32", "Unix process observation")
    def test_observation_bound_still_stops_known_process_group(self) -> None:
        program = (
            "import subprocess,sys,time; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
            "print(child.pid,flush=True); time.sleep(60)"
        )
        child: psutil.Process | None = None
        try:
            with (
                tempfile.TemporaryFile() as stdin,
                patch("rift_dev.commands.PROCESS_COUNT_MAX", 0),
                self.assertRaisesRegex(RuntimeError, "observation exceeded"),
                owned_process(
                    [sys.executable, "-c", program], None, None, stdin
                ) as process,
            ):
                assert process.stdout is not None and process.stderr is not None
                child = psutil.Process(int(process.stdout.readline().strip()))
                process.stdout.close()
                process.stderr.close()
            assert child is not None
            self.assertTrue(
                not child.is_running() or child.status() == psutil.STATUS_ZOMBIE
            )
        finally:
            if child is not None:
                try:
                    child.kill()
                except psutil.NoSuchProcess:
                    pass

    @unittest.skipIf(sys.platform == "win32", "Unix transport process observation")
    def test_observation_failure_keeps_already_captured_transport_child(self) -> None:
        async def observe() -> psutil.Process:
            with (
                self.assertRaisesRegex(RuntimeError, "observation exceeded"),
                patch("rift_dev.commands.PROCESS_COUNT_MAX", 1),
                patch("rift_dev.commands.psutil.process_iter") as process_iter,
                owned_environment({}) as environment,
            ):
                process = await anyio.open_process(
                    SLEEPER, env=environment, start_new_session=True
                )
                child = psutil.Process(process.pid)
                # The process starts before exec, and observation matches the owner
                # token in the environment the child carries only after exec.
                exec_deadline = time.monotonic() + 5
                while OWNER_ENV not in child.environ():
                    self.assertLess(
                        time.monotonic(), exec_deadline, "the child never ran exec"
                    )
                    await anyio.sleep(0.01)
                process_iter.return_value = [child, psutil.Process(os.getpid())]
            with anyio.fail_after(5):
                await process.aclose()
            return child

        child = anyio.run(observe)
        self.assertTrue(
            not child.is_running() or child.status() == psutil.STATUS_ZOMBIE
        )

    @unittest.skipIf(sys.platform == "win32", "Windows jobs own detached children")
    def test_detached_child_is_found_after_immediate_parent_exit(self) -> None:
        program = (
            "import subprocess,sys; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
            "start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); "
            "print(child.pid,flush=True)"
        )
        for _ in range(10):
            pid = int(
                Command(sys.executable, "-c", program).with_timeout(5).output().strip()
            )
            try:
                child = psutil.Process(pid)
                try:
                    self.assertEqual(child.status(), psutil.STATUS_ZOMBIE)
                finally:
                    if child.is_running() and child.status() != psutil.STATUS_ZOMBIE:
                        child.kill()
            except psutil.NoSuchProcess:
                pass

    @unittest.skipIf(sys.platform == "win32", "Unix environment ownership contract")
    def test_nested_command_keeps_outer_owner_with_replaced_environment(self) -> None:
        previous = os.environ.get(OWNER_ENV)
        program = (
            "import json,os,sys; from rift_dev.commands import Command; "
            "from rift_dev.commands import OWNER_ENV; "
            "inner=Command(sys.executable,'-c','import os,sys; print(os.environ[sys.argv[1]])',OWNER_ENV).with_environment({}).output(); "
            "print(json.dumps({'outer':os.environ[OWNER_ENV].split(','),'inner':inner.strip().split(',')}))"
        )
        document = json.loads(
            Command(sys.executable, "-c", program)
            .with_cwd(Path(__file__).parent)
            .output()
        )
        self.assertTrue(set(document["outer"]) < set(document["inner"]))
        self.assertEqual(len(document["inner"]), len(document["outer"]) + 1)
        self.assertEqual(os.environ.get(OWNER_ENV), previous)

    @unittest.skipIf(sys.platform == "win32", "Unix transport environment ownership")
    def test_transport_environment_owns_detached_child_after_parent_exit(self) -> None:
        program = (
            "import subprocess,sys; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
            "start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); "
            "print(child.pid,flush=True)"
        )

        async def transport(environment: dict[str, str]) -> bytes:
            with anyio.fail_after(5):
                result = await anyio.run_process(
                    [sys.executable, "-c", program], env=environment
                )
            return result.stdout

        with owned_environment({}) as environment:
            pid = int(anyio.run(transport, environment).strip())
        try:
            child = psutil.Process(pid)
            try:
                self.assertEqual(child.status(), psutil.STATUS_ZOMBIE)
            finally:
                if child.is_running() and child.status() != psutil.STATUS_ZOMBIE:
                    child.kill()
        except psutil.NoSuchProcess:
            pass

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
            patch(
                "rift_dev.commands.os.killpg",
                side_effect=PermissionError("denied"),
            ),
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
            Command(*command).with_input(payload).with_accepted(0, 1).output_bytes(),
            payload,
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
        pid = int(
            Command(sys.executable, "-c", program).with_timeout(5).output().strip()
        )
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
            with self.assertRaisesRegex(RuntimeError, "SIGTERM"):
                Command(sys.executable, "-c", program, path).with_timeout(5).output()
            pid = int(path.read_text())
            try:
                child = psutil.Process(pid)
                self.assertEqual(child.status(), psutil.STATUS_ZOMBIE)
            except psutil.NoSuchProcess:
                pass

    @unittest.skipIf(sys.platform == "win32", "Unix SIGTERM during a selector wait")
    def test_sigterm_interrupts_a_selector_wait(self) -> None:
        program = (
            "import os,signal,time; time.sleep(0.05); "
            "os.kill(os.getppid(),signal.SIGTERM)"
        )
        with (
            termination_handler(),
            selectors.DefaultSelector() as selector,
            Command(sys.executable, "-c", program).spawn() as child,
            self.assertRaisesRegex(RuntimeError, "SIGTERM"),
        ):
            selector.select(timeout=2)
        self.assertEqual(child.returncode, 0)

    def test_child_environment_is_inherited_or_replaced_explicitly(self) -> None:
        # A Windows process needs SystemRoot for side-by-side assemblies.
        replacement = (
            {"SystemRoot": os.environ["SystemRoot"]} if sys.platform == "win32" else {}
        )
        command = [
            sys.executable,
            "-c",
            "import os; print(os.environ.get('RIFT_GATE_TEST', 'absent'))",
        ]
        with patch.dict(os.environ, {"RIFT_GATE_TEST": "inherited"}):
            self.assertEqual(Command(*command).output().strip(), "inherited")
            self.assertEqual(
                Command(*command).with_env(RIFT_GATE_TEST="overlay").output().strip(),
                "overlay",
            )
            self.assertEqual(
                Command(*command).with_environment(replacement).output().strip(),
                "absent",
            )

    def test_failed_timed_out_and_flooding_children_fail_gate(self) -> None:
        for program in [
            "raise SystemExit(3)",
            "import time; time.sleep(30)",
            f"import sys; sys.stdout.write('x' * {OUTPUT_BYTES_MAX + 1}); sys.stdout.flush()",
        ]:
            with self.subTest(program=program), self.assertRaises(RuntimeError):
                Command(sys.executable, "-c", program).with_timeout(0.5).output()

    def test_shared_deadline_cannot_reset_for_a_later_command(self) -> None:
        deadline = time.monotonic() - 1.0
        with self.assertRaisesRegex(RuntimeError, "deadline expired"):
            Command(sys.executable, "-c", "raise SystemExit(0)").with_deadline(
                deadline
            ).output()
        with self.assertRaisesRegex(RuntimeError, "exceeded"):
            Command(sys.executable, "-c", "import time; time.sleep(10)").with_timeout(
                5.0
            ).with_deadline(time.monotonic() + 0.1).output()

    def test_timeout_unwinds_nested_process_owners(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "child.pid"
            child = (
                "import os,pathlib,signal,sys,time; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)"
            )
            parent = (
                "import sys; from rift_dev.commands import Command; "
                "Command(sys.executable, '-c', sys.argv[1], sys.argv[2]).with_timeout(30).output()"
            )
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "exceeded"):
                Command(sys.executable, "-c", parent, child, path).with_cwd(
                    Path(__file__).parent
                ).with_timeout(1).output()
            self.assertLess(time.monotonic() - started, 13)
            pid = int(path.read_text())
            try:
                descendant = psutil.Process(pid)
                self.assertEqual(descendant.status(), psutil.STATUS_ZOMBIE)
            except psutil.NoSuchProcess:
                pass


if __name__ == "__main__":
    unittest.main()
