"""Keep a bounded benchmark process tree inside a proven Linux CPU set."""
from __future__ import annotations

from fractions import Fraction
import json
import os
from pathlib import Path
import shutil
import time

import psutil

DISK_FLOOR = 5 << 30


def file_text(path: Path, missing=None):
    try:
        return path.read_text().strip()
    except FileNotFoundError:
        return missing


def parse_cpuset(value: str) -> set[int]:
    cpus = set()
    for item in value.split(","):
        bounds = item.split("-")
        cpus.update(range(int(bounds[0]), int(bounds[-1]) + 1))
    return cpus


def cgroup_mount(pid: int) -> tuple[Path, Path, str]:
    memberships = Path(f"/proc/{pid}/cgroup").read_text().splitlines()
    groups = [entry[3:] for entry in memberships if entry.startswith("0::")]
    if len(groups) != 1:
        raise ValueError("cannot establish one unified workload cgroup")
    group = groups[0]
    location = Path(group)
    mounts = []
    for row in Path(f"/proc/{pid}/mountinfo").read_text().splitlines():
        origin, kind = row.split(" - ", 1)
        if kind.split()[0] != "cgroup2":
            continue
        columns = origin.split()
        base, mount = Path(columns[3]), Path(columns[4])
        if location.is_relative_to(base):
            mounts.append((base, mount))
    if not mounts:
        raise ValueError("cannot resolve the unified workload cgroup mount")
    base, mount = max(mounts, key=lambda pair: len(pair[0].parts))
    effective = mount / location.relative_to(base)
    if not effective.is_dir():
        raise ValueError("effective cgroup path does not exist")
    return mount, effective, group


def cgroup_limits(mount: Path, leaf: Path) -> list[dict]:
    limits = []
    path = leaf
    while True:
        limits.append({
            "path": str(path), "cpu_max": file_text(path / "cpu.max"),
            "cpuset_effective": file_text(path / "cpuset.cpus.effective"),
            "memory_max": file_text(path / "memory.max"),
        })
        if path == mount:
            return limits
        path = path.parent


def cpu_selection(online: set[int], affinity: set[int], limits: list[dict]) -> tuple[list[int], Fraction]:
    eligible = online & affinity
    quotas = []
    for level in limits:
        if cpuset := level["cpuset_effective"]:
            eligible &= parse_cpuset(cpuset)
        if maximum := level["cpu_max"]:
            quota, period = maximum.split()
            if quota != "max":
                quotas.append(Fraction(int(quota), int(period)))
    capacity = min([Fraction(len(eligible)), *quotas])
    grant = min(2, len(eligible) - 1, int(capacity * Fraction(4, 5)))
    if grant < 1:
        raise ValueError("insufficient proven capacity to leave CPU headroom")
    return sorted(eligible)[:grant], capacity


def cpu_interval(previous: dict, current: dict) -> dict:
    # Same Linux accounting as locked psutil.cpu_percent, with the exact input delta retained.
    delta = {key: max(0, value - previous[key]) for key, value in current.items()}
    total = sum(delta.values()) - delta.get("guest", 0) - delta.get("guest_nice", 0)
    busy = total - delta["idle"] - delta.get("iowait", 0)
    percent = round((busy / total) * 100, 1) if total else 0.0
    return {"percent": percent, "busy_seconds": busy, "total_seconds": total, "delta": delta}


def cgroup_cpu_stat(leaf: Path) -> dict:
    value = file_text(leaf / "cpu.stat")
    return {key: int(number) for key, number in (row.split() for row in value.splitlines())} if value else {}


def cgroup_cpu_stats(limits: list[dict]) -> dict:
    return {level["path"]: cgroup_cpu_stat(Path(level["path"])) for level in limits}


def process_sample(process: psutil.Process) -> dict | None:
    base = Path(f"/proc/{process.pid}")
    memory = {}
    try:
        for filename, keys in [("status", {"VmHWM", "VmRSS"}), ("smaps_rollup", {"Pss"})]:
            for line in (base / filename).read_text().splitlines():
                key, _, value = line.partition(":")
                if key in keys:
                    memory[key] = int(value.split()[0]) * 1024
        affinities = {}
        for thread in process.threads():
            try:
                affinities[str(thread.id)] = sorted(os.sched_getaffinity(thread.id))
            except ProcessLookupError:
                continue
        return {"pid": process.pid, "threads": affinities, "memory": memory,
                "cgroup": (base / "cgroup").read_text().strip()}
    except (FileNotFoundError, ProcessLookupError, psutil.NoSuchProcess):
        return None


class HostGuard:
    def __init__(self, evidence: Path):
        self.workload = psutil.Process()
        self.mount, self.leaf, group = cgroup_mount(self.workload.pid)
        self.group = "0::" + group
        self.limits = cgroup_limits(self.mount, self.leaf)
        host = {int(row.split()[0][3:]) for row in Path("/proc/stat").read_text().splitlines()
                if row.startswith("cpu") and row[3:4].isdigit()}
        affinity = set(os.sched_getaffinity(0))
        self.selected, capacity = cpu_selection(host, affinity, self.limits)
        self.host = sorted(host)
        for thread in self.workload.threads():
            os.sched_setaffinity(thread.id, self.selected)
        self.previous_host = psutil.cpu_times()._asdict()
        self.previous_stat = cgroup_cpu_stats(self.limits)
        self.previous_time = time.monotonic_ns()
        (evidence / "cpu-profile.json").write_text(json.dumps({
            "available_host_cpu_ids": self.host, "runner_initial_affinity": sorted(affinity),
            "selected_workload_cpu_ids": self.selected, "effective_cgroup_capacity": str(capacity),
            "capacity_and_memory_limits": self.limits, "unified_cgroup": group,
            "launcher_pid": self.workload.pid, "host_ram_bytes": psutil.virtual_memory().total,
            "psutil_version": psutil.__version__,
        }, indent=2) + "\n")

    def check(self, journal, stage: Path, phase: str, server_pid: int | None) -> None:
        now = time.monotonic_ns()
        host = psutil.cpu_times()._asdict()
        cpu = cpu_interval(self.previous_host, host)
        cgroup = cgroup_cpu_stats(self.limits)
        group_delta = {
            level: {
                key: value - self.previous_stat.get(level, {}).get(key, value)
                for key, value in counters.items()
            }
            for level, counters in cgroup.items()
        }
        memory = psutil.virtual_memory()
        disk = shutil.disk_usage(stage).free
        owned = [self.workload, *self.workload.children(recursive=True)]
        parts = [capture for child in owned if (capture := process_sample(child))]
        rss = sum(row["memory"].get("VmRSS", 0) for row in parts)
        limits = cgroup_limits(self.mount, self.leaf)
        exact = all(
            row["cgroup"] == self.group
            and "VmRSS" in row["memory"]
            and bool(row["threads"])
            and all(set(affinity) <= set(self.selected) for affinity in row["threads"].values())
            for row in parts
        )
        present = {row["pid"] for row in parts}
        passed = {
            "host_cpu_below_90": cpu["percent"] < 90,
            "host_ram_below_90": memory.percent < 90,
            "workload_rss_below_host_90": rss < .90 * memory.total,
            "disk_reserve": disk >= DISK_FLOOR,
            "valid_host_interval": cpu["total_seconds"] > 0,
            "same_cgroup_capacity": limits == self.limits,
            "owned_affinity_and_cgroup": exact and self.workload.pid in present
            and (server_pid is None or server_pid in present),
        }
        failed = [name for name, accepted in passed.items() if not accepted]
        print(json.dumps({
            "phase": phase, "server_pid": server_pid, "at_ns": now,
            "sample_interval_ns": now - self.previous_time,
            "host_cpu": cpu, "host_cpu_count": len(self.host),
            "host_ram_percent": memory.percent, "host_ram_bytes": memory.total,
            "rss_threshold_bytes": .90 * memory.total, "owned_rss_bytes": rss,
            "bytes_free": disk, "cgroup_cpu_stat": cgroup, "cgroup_cpu_delta": group_delta,
            "cgroup_capacity_this_sample": limits, "workload_processes": parts, "failed": failed,
        }), file=journal, flush=True)
        self.previous_host = host
        self.previous_stat = cgroup
        self.previous_time = now
        if failed:
            raise ValueError(f"benchmark resource guard refused {phase}: " + ", ".join(failed))
