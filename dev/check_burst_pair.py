"""Run the same bounded Bun edit against two supplied corpus executables on Linux."""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Literal

import psutil
from rift_dev.corpus_burst import FILE_COUNT, MARKER_PATH
from rift_dev.corpus_cache import pins

EditCase = Literal["burst", "single-edit"]
VARIANTS = ("base", "candidate", "candidate", "base")


def resource_sample(scratch: Path) -> dict[str, int | float]:
    memory = psutil.virtual_memory()
    return {
        "time": time.time(),
        "cpu_percent": psutil.cpu_percent(interval=0.5),
        "memory_percent": memory.percent,
        "available_bytes": memory.available,
        "free_disk_bytes": shutil.disk_usage(scratch).free,
    }


def ready(row: dict[str, int | float]) -> bool:
    return (
        row["cpu_percent"] <= 35
        and row["memory_percent"] < 90
        and row["available_bytes"] >= 8 * 2**30
        and row["free_disk_bytes"] >= 3 * 2**30
    )


def stop(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        descendants = psutil.Process(process.pid).children(recursive=True)
        os.killpg(process.pid, signal.SIGTERM)
        for child in descendants:
            try:
                child.terminate()
            except psutil.NoSuchProcess:
                pass
        _, alive = psutil.wait_procs(descendants, timeout=3)
        for child in alive:
            try:
                child.kill()
            except psutil.NoSuchProcess:
                pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def run(binary: Path, output: Path, cpus: str, case: EditCase = "burst") -> dict:
    output.mkdir()
    scratch = Path(os.environ["RUNNER_TEMP"]) / "rift-burst-work" / output.name
    scratch.mkdir(parents=True)
    report = output / "burst.json"
    with (output / "resources.jsonl").open("x") as samples:

        def sample() -> dict[str, int | float]:
            row = resource_sample(scratch)
            samples.write(json.dumps(row) + "\n")
            samples.flush()
            return row

        deadline = time.monotonic() + 90
        quiet = 0
        while quiet < 10:
            if time.monotonic() > deadline:
                raise RuntimeError(
                    "insufficient quiet memory, CPU or disk for the burst"
                )
            quiet = quiet + 1 if ready(sample()) else 0
        command = [
            "taskset",
            "-c",
            cpus,
            str(Path(sys.executable).parent / "rift-dev"),
            "corpus",
            "test",
            "bun",
            "--binary",
            str(binary),
            "--case",
            case,
            "--report",
            str(report),
        ]
        process = subprocess.Popen(
            command,
            start_new_session=True,
            env={
                **os.environ,
                "TMPDIR": str(scratch),
                "RAYON_NUM_THREADS": "2",
                "TOKIO_WORKER_THREADS": "2",
            },
        )
        deadline = time.monotonic() + pins()["bun"].seconds
        try:
            while process.poll() is None:
                row = sample()
                if (
                    row["cpu_percent"] > 90
                    or row["memory_percent"] > 90
                    or row["free_disk_bytes"] < 2 * 2**30
                ):
                    raise RuntimeError(f"resource bound exceeded: {row}")
                if time.monotonic() >= deadline:
                    raise TimeoutError("Bun burst deadline expired")
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode, command)
        finally:
            stop(process)
    result = json.loads(report.read_bytes())
    if (result["commit"], result["case"], result["status"]) != (
        pins()["bun"].commit,
        case,
        "passed",
    ):
        raise ValueError("unexpected corpus revision, case or outcome")
    records = {row["action"]: row for row in result["actions"]}
    inputs, burst, restored = (
        records[key] for key in ("burst_inputs", "burst", "burst_restore")
    )
    file_count = FILE_COUNT if case == "burst" else 1
    if (
        len(inputs["original_sha256"]) != file_count
        or inputs["original_sha256"].keys() != inputs["changed_sha256"].keys()
        or MARKER_PATH not in inputs["original_sha256"]
        or any(
            digest == inputs["changed_sha256"][path]
            for path, digest in inputs["original_sha256"].items()
        )
        or burst["files"] != file_count
        or inputs["initial"] != restored["restored"]
        or inputs["original_sha256"] != restored["disk_sha256"]
        or not restored["map_equal"]
    ):
        raise ValueError("burst restoration differs")
    return {
        "input": {
            key: inputs[key] for key in ("original_sha256", "changed_sha256", "initial")
        },
        "result": {
            "settled": burst["settled"],
            "facets_revision": burst["facets"]["revision"],
            "facets_warnings": burst["facets"]["warnings"],
            "restored": restored["restored"],
        },
        "publication_seconds": burst["final_matching_publication_seconds"],
        "facets_seconds": burst["facets_seconds"],
        "digests_seconds": burst["all_digests_seconds"],
        "write_seconds": burst["write_seconds"],
        "startup_publication_observed_seconds": records["burst_startup"][
            "publication_observed_seconds"
        ],
        "cold_lexical_observed_seconds": records["burst_lexical_ready"]["seconds"],
    }


def main() -> None:
    baseline, candidate, output = (
        Path(value).resolve(strict=True) for value in sys.argv[1:]
    )
    allowed = sorted(psutil.Process().cpu_affinity())
    if len(allowed) < 4:
        raise RuntimeError("the paired corpus requires four available logical CPUs")
    cpus = ",".join(str(cpu) for cpu in allowed[:2])
    results = {}
    expected = None
    binaries = {"base": baseline, "candidate": candidate}
    for slot, label in enumerate(VARIANTS, 1):
        name = f"{slot}-{label}"
        measurement = run(binaries[label], output / name, cpus)
        inputs = measurement.pop("input")
        if expected is None:
            expected = inputs
        elif expected != inputs:
            raise ValueError(
                "paired runs did not observe the same source edit and starting rows"
            )
        results[name] = measurement
        (output / "pair.json").write_text(json.dumps(results, indent=2) + "\n")
        print(name, json.dumps(measurement), flush=True)


if __name__ == "__main__":
    main()
