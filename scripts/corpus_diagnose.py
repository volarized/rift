#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Record bounded process and host resource observations for the failed Bun cases."""

from __future__ import annotations

import json
import os
import shutil
import threading
import time
from pathlib import Path

import psutil
from release_process import run


def observe(stopped: threading.Event, path: Path) -> None:
    owner = psutil.Process()
    with path.open("w", encoding="utf-8") as output:
        for sample in range(960):
            memory = psutil.virtual_memory()
            processes: list[dict[str, str | int | float]] = []
            for process in owner.children(recursive=True)[:128]:
                try:
                    with process.oneshot():
                        cpu = process.cpu_times()
                        processes.append(
                            {
                                "pid": process.pid,
                                "name": process.name(),
                                "status": process.status(),
                                "rss": process.memory_info().rss,
                                "cpu_user": cpu.user,
                                "cpu_system": cpu.system,
                                "threads": process.num_threads(),
                            }
                        )
                except psutil.NoSuchProcess:
                    continue
            row = {
                "time": time.time(),
                "available": memory.available,
                "total": memory.total,
                "swap_used": psutil.swap_memory().used,
                "disk_free": shutil.disk_usage(Path.cwd()).free,
                "processes": processes,
            }
            output.write(json.dumps(row) + "\n")
            output.flush()
            if sample % 5 == 0:
                print(json.dumps(row), flush=True)
            if stopped.wait(1.0):
                return


def main() -> None:
    case = os.environ["CORPUS_DIAGNOSE_CASE"]
    if case not in ("workspace", "stop"):
        raise ValueError("unknown Bun case")
    directory = Path("target/corpus")
    directory.mkdir(parents=True, exist_ok=True)
    stopped = threading.Event()
    observer = threading.Thread(
        target=observe, args=(stopped, directory / "resources.jsonl"), daemon=True
    )
    observer.start()
    try:
        print(
            run(
                [
                    "just",
                    "corpus-test",
                    "bun",
                    f"test_bun_{case}",
                    "target/corpus.tar.zst",
                ],
                timeout=900.0,
            )
        )
    finally:
        stopped.set()
        observer.join(timeout=2.0)
        if observer.is_alive():
            raise RuntimeError("resource observer did not stop")


if __name__ == "__main__":
    main()
