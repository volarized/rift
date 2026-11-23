"""Run one observer build, one named pass and four isolated cold passes."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import subprocess
import tempfile
import time

from audit import BUN_SOURCE, PHASES, RIFT_SOURCE, RIFT_TREE, require
from merge import merge

HERE = Path(__file__).resolve().parent
MARKER = "bench/test/isolate/setup.ts"
GIB = 1024**3
CORPUS_PEAK = 9_923_735_552
MAX_RAW = 256 * 1024


def file_sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def git(repo, *args):
    return subprocess.check_output(["git", "--no-replace-objects", "-C", str(repo), *args], timeout=10)


def recipe(bun, output=None):
    require(git(bun, "rev-parse", "HEAD").decode().strip() == BUN_SOURCE, "wrong Bun checkout pin")
    saved = json.loads((HERE / "saved-edits.json").read_text())
    require(saved["bun"] == BUN_SOURCE and saved["marker"] == MARKER, "wrong saved corpus fixture")
    expect = saved["paths"]
    names = [row["path"] for row in expect]
    require(len(names) == len(set(names)) == 50 and MARKER in names, "wrong saved path count")
    for row in expect:
        path = row["path"]
        relative = PurePosixPath(path)
        require(not relative.is_absolute() and ".." not in relative.parts
                and str(relative) == path, "malformed saved path")
        source = git(bun, "show", f"{BUN_SOURCE}:{path}")
        contents = {"original": source}
        if path == MARKER:
            contents["single"] = source + base64.b64decode(row["single_suffix"], validate=True)
            contents["burst"] = source + base64.b64decode(row["burst_suffix"], validate=True)
        else:
            cut = source.index(b"\n") + 1 if source.startswith(b"#!") else 0
            contents["burst"] = source[:cut] + b"\n" + source[cut:]
        for phase, value in contents.items():
            require(hashlib.sha256(value).hexdigest() == row[phase], f"{phase} digest differs for {path}")
            if output:
                destination = output / phase / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(value)
    if output:
        (output / "paths.txt").write_text("".join(f"{name}\n" for name in names))
    return names


class Guard:
    def __init__(self, output, scratch):
        import psutil

        self.psutil = psutil
        self.output = output
        self.scratch = scratch
        cpus = sorted(os.sched_getaffinity(0))
        require(len(cpus) >= 2, "observer needs two available CPUs")
        self.cpu_set = ",".join(map(str, cpus[:2]))
        group = Path("/sys/fs/cgroup")
        self.cgroup_limit = group / "memory.max"
        self.cgroup_use = group / "memory.current"
        self.cgroup_stat = group / "memory.stat"

    def memory(self):
        host = self.psutil.virtual_memory()
        limit = host.total
        bytes_used = host.total - host.available
        try:
            group_limit = self.cgroup_limit.read_text().strip()
            if group_limit != "max" and int(group_limit) <= limit:
                limit = int(group_limit)
                stats = dict(line.split() for line in self.cgroup_stat.read_text().splitlines())
                bytes_used = max(0, int(self.cgroup_use.read_text()) - int(stats.get("inactive_file", "0")))
        except FileNotFoundError:
            pass
        return limit, bytes_used

    def sample(self, stage, proc, started):
        cpu = self.psutil.cpu_percent()
        limit, used = self.memory()
        rss = None
        if proc is not None:
            try:
                rss = sum(p.memory_info().rss for p in [proc, *proc.children(recursive=True)])
            except self.psutil.NoSuchProcess:
                pass
        disk = shutil.disk_usage(self.scratch).free
        row = {"stage": stage, "seconds": time.monotonic() - started,
               "aggregate_cpu_percent": cpu, "effective_ram_limit": limit,
               "effective_ram_used": used, "owned_rss": rss, "free_bytes": disk}
        with (self.output / "resources.jsonl").open("a") as trace:
            print(json.dumps(row), file=trace)
        require(cpu < 90 and used < limit * 0.9, f"{stage} host resource limit crossed")
        require(disk >= 2 * GIB, f"{stage} scratch disk reserve crossed")
        return cpu, limit, used

    def run(self, stage, args, env, ceiling, reserve=0, progress_bound=None):
        self.psutil.cpu_percent()
        for _ in range(3):
            time.sleep(.35)
            cpu, limit, used = self.sample(f"{stage}-preflight", None, time.monotonic())
            require(cpu < 80, f"{stage} host CPU reserve unavailable")
            require(limit * .9 - used >= reserve, f"{stage} RAM reserve unavailable")
        output = self.output / f"{stage}.jsonl" if progress_bound else self.output / f"{stage}.log"
        errors = self.output / f"{stage}.stderr.log"
        with output.open("xb") as sink, errors.open("xb") as stderr:
            child = subprocess.Popen(["taskset", "-c", self.cpu_set, *args], env=env,
                                     stdout=sink, stderr=stderr, start_new_session=True)
            start = checkpoint = time.monotonic()
            size = 0
            try:
                try:
                    proc = self.psutil.Process(child.pid)
                except self.psutil.NoSuchProcess:
                    proc = None
                while child.poll() is None:
                    time.sleep(.35)
                    if child.poll() is not None:
                        break
                    self.sample(stage, proc, start)
                    now = time.monotonic()
                    require(now - start < ceiling, f"{stage} elapsed limit crossed")
                    if progress_bound:
                        current = output.stat().st_size
                        require(current <= MAX_RAW, f"{stage} raw observation bound crossed")
                        if current != size:
                            checkpoint, size = now, current
                        require(now - checkpoint < progress_bound, f"{stage} ordinary phase progress stopped")
                require(child.returncode == 0, f"{stage} exited {child.returncode}")
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=5)


def witnessed_partials(results):
    found = {}
    for name in ("named", *(f"cold-{phase}" for phase in PHASES)):
        raw = results / f"{name}.jsonl"
        if not raw.is_file():
            continue
        with raw.open("rb") as stream:
            lines = stream.read(MAX_RAW).splitlines()
        for line in lines:
            try:
                row = json.loads(line)
            except (UnicodeError, json.JSONDecodeError):
                continue
            if row.get("event") != "gate":
                continue
            record = row["observation"]
            scope = "named_path_dependency" if record["first_affected"] else "complete_corpus"
            witness = record["first_affected"] or record["first"]
            if witness:
                found.setdefault(row["phase"], []).append(
                    {"pass": name, "family": row["family"], "scope": scope, "first": witness})
    return {phase: next((row for row in rows if row["scope"] == "named_path_dependency"), rows[0])
            for phase, rows in found.items()}


def observe(args):
    results = args.output.resolve()
    results.mkdir(parents=True, exist_ok=True)
    require(not (results / "receipt.json").exists(), "already used result directory")
    temp = args.scratch.resolve()
    temp.mkdir(parents=True, exist_ok=False)
    recipe_path = temp / "recipe"
    recipe_path.mkdir()
    paths = recipe(args.bun, recipe_path)
    sha = git(args.source, "rev-parse", "HEAD").decode().strip()
    require(sha == args.workflow_head, "workflow source head differs")
    receipt = {"workflow_head": sha, "rift_source": RIFT_SOURCE, "rift_tree": RIFT_TREE,
               "bun_source": BUN_SOURCE, "saved_paths": len(paths), "status": "ordinary build pending"}
    receipt_path = results / "receipt.json"

    def persist():
        receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")

    persist()
    guard = Guard(results, temp)
    target = temp / "target"
    env = dict(os.environ, RIFT_BUN_ELIGIBILITY="1", CARGO_BUILD_JOBS="2",
               RAYON_NUM_THREADS="2", TOKIO_WORKER_THREADS="2",
               CARGO_TARGET_DIR=str(target), CARGO_TERM_COLOR="never")
    try:
        guard.run("build", ["cargo", "+1.98", "build", "--manifest-path", str(args.source / "Cargo.toml"),
                            "--locked", "--profile", "corpus", "-p", "rift", "--example", "bun_eligibility"],
                  env, 1500)
        binary = target / "corpus/examples/bun_eligibility"
        digest = file_sha(binary)
        receipt.update(observer_binary_sha256=digest, build_toolchain=subprocess.check_output(
            ["rustc", "+1.98", "-vV"], text=True, timeout=5).strip(), status="ordinary runs pending",
            selected_cpus=guard.cpu_set)
        persist()
        archive = temp / "bun.tar"
        guard.run("archive", ["git", "-C", str(args.bun), "archive", "--format=tar", "-o",
                              str(archive), BUN_SOURCE], env, 90)
        started = time.monotonic()
        for name in ("named", *PHASES):
            require(time.monotonic() - started < 810, "combined observation time exhausted")
            with tempfile.TemporaryDirectory(prefix=f"{name}-", dir=temp) as directory:
                phase = Path(directory)
                workspace = phase / "bun"
                workspace.mkdir()
                guard.run(f"unpack-{name}", ["tar", "-xf", str(archive), "-C", str(workspace)], env, 75)
                (phase / "bun-observer-scratch").write_text(BUN_SOURCE + "\n")
                isolated = dict(env)
                for variable in ("TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
                    isolated[variable] = str(phase / variable.lower())
                    Path(isolated[variable]).mkdir()
                stage = name if name == "named" else f"cold-{name}"
                ceiling = min(320 if name == "named" else 145, 810 - (time.monotonic() - started))
                guard.run(stage, [str(binary), str(workspace), str(recipe_path), name],
                          isolated, ceiling, CORPUS_PEAK, progress_bound=180)
                receipt["completed_passes"] = [*receipt.get("completed_passes", []), name]
                persist()
        rows, summary = merge(results, digest)
        (results / "comparison.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
        (results / "eligibility.json").write_text(json.dumps(summary, indent=2) + "\n")
        receipt["status"] = "ordinary comparison complete"
    except Exception as error:
        receipt["status"] = "incomplete ordinary observation"
        receipt["failure"] = str(error)
        receipt["first_observed_partial_refusals"] = witnessed_partials(results)
        raise
    finally:
        persist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bun", type=Path, required=True)
    parser.add_argument("--check-recipe", action="store_true")
    parser.add_argument("--source", type=Path)
    parser.add_argument("--workflow-head")
    parser.add_argument("--scratch", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.check_recipe:
        print(json.dumps({"bun_source": BUN_SOURCE, "verified_paths": len(recipe(args.bun))}))
        return
    require(all((args.source, args.workflow_head, args.scratch, args.output)), "hosted run is missing its paths")
    observe(args)


if __name__ == "__main__":
    main()
