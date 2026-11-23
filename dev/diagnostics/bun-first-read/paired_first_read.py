"""Pair two compiled corpus-profile release builds through their ordinary MCP server."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time

from artifact_gate import check_contract, require, sha

MARKER = "bench/test/isolate/setup.ts"
MARKER_SHA = "763085a2661ea6919341c2053c3b735e865a3522f68c839682f23f7f91e9cc7d"
MARKER_ID = "rift://symbol/local/typescript/.%2Fbench%2Ftest%2Fisolate%2Fsetup.ts/root"
MARKER_RANGE = (514, 547)
ALIAS_ID = "rift://symbol/local/typescript/.%2Fproof%2Findex.ts/leaf"
CONFIG = b'[search.vector]\ndisabled = true\n[dependencies]\nresolution = "static"\n'
GIB = 1024**3
MIB = 1024**2
EXPECTED_ACTIVE_BYTES = 9_923_735_552
MAX_LOG = 24 * MIB
CASE_SECONDS = 138
RUN_SECONDS = 1110
REVISION = re.compile(r"rift://revision/tree/[0-9a-f]{64}")
SEED_COUNTS = re.compile(r"paths: (\d+), read: (\d+)")
SEED_FLAGS = re.compile(r"\b(reused_selection|install)=(true|false)\b")
TRACE = ("index snapshot published", "lexical commit committing", "lexical commit settled",
         "filesystem invalidations coalesced", "fingerprint.prepared_capture", "operation=fingerprint.capture",
         "work=workspace fingerprint", "workspace source fingerprint captured")


def save(path, row):
    path.write_text(json.dumps(row, indent=2) + "\n")


def mcp(port, payload, session=None, version=None):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if session:
        headers["Mcp-Session-Id"] = session
    if version:
        headers["MCP-Protocol-Version"] = version
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=35)
    try:
        sent = time.monotonic_ns()
        connection.request("POST", "/api/mcp", json.dumps(payload), headers)
        response = connection.getresponse()
        token = response.headers.get("Mcp-Session-Id")
        require(response.status in (200, 202), f"MCP HTTP {response.status}")
        if response.status == 202:
            return None, token, sent, time.monotonic_ns()
        body = None
        if "text/event-stream" in response.headers.get("Content-Type", ""):
            for _ in range(80):
                line = response.readline(256 * 1024)
                if not line:
                    break
                if line.startswith(b"data:"):
                    decoded = json.loads(line[5:])
                    if decoded.get("id") == payload.get("id"):
                        body = decoded
                        break
        else:
            body = json.loads(response.read(256 * 1024))
        require(body is not None and body.get("id") == payload.get("id"), "missing correlated MCP reply")
        return body, token, sent, time.monotonic_ns()
    finally:
        connection.close()


def initialize(port):
    body, session, _, _ = mcp(port, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "first-read-pair", "version": "1"}}})
    version = body["result"]["protocolVersion"]
    mcp(port, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session, version)
    return session, version


def request(port, session, identity, barrier=None, request_id=2):
    if barrier is not None:
        barrier.wait(timeout=4)
    body, _, sent, received = mcp(port, {"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
        "params": {"name": "get_symbol", "arguments": {"id": identity}}}, *session)
    return {"raw": body, "sent_ns": sent, "received_ns": received, "round_trip_ms": (received - sent) / 1e6}


def verify_reply(row):
    result = row["raw"]["result"]["structuredContent"]
    require(result["outcome"] == "found" and result["symbol"]["id"] == MARKER_ID, "wrong current identity")
    declaration, = result["declarations"]
    extent = declaration["range"]
    location = (declaration["path"], extent["start"], extent["end"])
    require(location == (MARKER, *MARKER_RANGE), f"unexpected authored range {location}")
    require(REVISION.fullmatch(result["revision"]), "missing complete current tree revision")
    return {"id": MARKER_ID, "location": list(location), "revision": result["revision"]}


def memory(pid, *, proportional):
    readings = {}
    source = "smaps_rollup" if proportional else "status"
    with Path(f"/proc/{pid}/{source}").open() as process:
        for line in process:
            key, separator, value = line.partition(":")
            if separator and key in {"VmRSS", "VmHWM", "Rss", "Pss", "Private_Clean", "Private_Dirty"}:
                readings[key.lower() + "_bytes"] = int(value.split()[0]) * 1024
    require(("pss_bytes" if proportional else "vmrss_bytes") in readings, "missing process memory reading")
    return readings


class Guard:
    def __init__(self, out, scratch):
        import psutil
        self.psutil = psutil
        self.log = out / "resources.jsonl"
        self.scratch = scratch
        cpus = sorted(os.sched_getaffinity(0))
        require(len(cpus) >= 2, "two assigned CPUs required")
        self.cpus = ",".join(map(str, cpus[:2]))

    def sample(self, stage, pid=None):
        host = self.psutil.virtual_memory()
        limit, used = host.total, host.total - host.available
        group = Path("/sys/fs/cgroup")
        if (group / "memory.max").exists():
            cap = (group / "memory.max").read_text().strip()
            if cap != "max" and int(cap) <= limit:
                limit = int(cap)
                stats = dict(line.split() for line in (group / "memory.stat").read_text().splitlines())
                used = max(0, int((group / "memory.current").read_text()) - int(stats.get("inactive_file", "0")))
        cpu = self.psutil.cpu_percent()
        disk = shutil.disk_usage(self.scratch).free
        try:
            rss = self.psutil.Process(pid).memory_info().rss if pid is not None else None
        except self.psutil.NoSuchProcess:
            rss = None
        data = {"stage": stage, "monotonic_ns": time.monotonic_ns(), "cpu_percent": cpu,
                "ram_limit": limit, "ram_used": used, "disk_free": disk, "rss_bytes": rss}
        with self.log.open("a") as sink:
            print(json.dumps(data), file=sink)
        require(cpu < 90 and used < limit * .90 and (rss is None or rss < 11 * GIB)
                and disk >= 8 * GIB, f"resource ceiling crossed during {stage}")
        return cpu, limit * .90 - used

    def preflight(self, stage):
        self.psutil.cpu_percent()
        for _ in range(3):
            time.sleep(.35)
            cpu, available = self.sample(stage + "-preflight")
            require(cpu < 78 and available >= EXPECTED_ACTIVE_BYTES,
                    f"insufficient reserve for exact corpus during {stage}")


class Events:
    def __init__(self, log, out):
        self.stream = log.open(errors="replace")
        self.out = out
        self.rows = []

    def read(self):
        while True:
            offset = self.stream.tell()
            line = self.stream.readline()
            if not line.endswith("\n"):
                self.stream.seek(offset)
                return
            if any(marker in line for marker in TRACE):
                row = {"observed_ns": time.monotonic_ns(), "trace": line.rstrip()}
                self.rows.append(row)
                with self.out.open("a") as sink:
                    print(json.dumps(row), file=sink)

    def find(self, term):
        self.read()
        return [row for row in self.rows if term in row["trace"]]


def trace_seed(events):
    found = []
    for row in events.find("fingerprint.prepared_capture"):
        match = SEED_COUNTS.search(row["trace"])
        flags = dict(SEED_FLAGS.findall(row["trace"]))
        require(match is not None and set(flags) == {"reused_selection", "install"},
                "unexpected bounded capture trace shape")
        paths, read = match.groups()
        found.append(row | {"reused_selection": flags["reused_selection"] == "true",
                            "installed": flags["install"] == "true",
                            "paths": int(paths), "file_body_reads": int(read)})
    return found


def managed_group(pid):
    try:
        return os.getpgid(pid) == pid
    except ProcessLookupError:
        return False


def stop_process(process):
    if managed_group(process.pid):
        os.killpg(process.pid, signal.SIGINT)
    try:
        process.wait(timeout=7)
    except subprocess.TimeoutExpired:
        if managed_group(process.pid):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=4)
    try:
        os.killpg(process.pid, 0)
    except ProcessLookupError:
        return process.returncode
    raise ValueError("owned process group did not reap")


def stage_inputs(source, case, kind):
    root = case / "bun"
    root.mkdir()
    subprocess.run(["tar", "-xf", str(source), "-C", str(root)], check=True, timeout=40)
    require(not (root / "rift.toml").exists() and not (root / ".git").exists(),
            "unexpected workspace settings or checkout metadata")
    (root / "rift.toml").write_bytes(CONFIG)
    require(sha(root / MARKER) == MARKER_SHA, "pinned marker body changed")
    require((root / MARKER).read_bytes()[slice(*MARKER_RANGE)] == b'root = import.meta.dir + "/suite"',
            "pinned marker range changed")
    if kind == "late":
        proof = root / "proof"
        proof.mkdir(exist_ok=True)
        with (proof / "index.ts").open("x") as file:
            file.write("export * from './leaf.js';\nexport * from './late.js';\n")
        with (proof / "leaf.ts").open("x") as file:
            file.write("export function leaf(): number { return 1; }\n")
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RIFT_", "OTEL_", "HF_", "GH_", "GITHUB_", "ACTIONS_"))}
    for key in ("TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "HOME"):
        path = case / key.lower()
        path.mkdir()
        env[key] = str(path)
    env.update(RUST_LOG="rift=info,rift_mcp=debug,rift_server=debug,rift_index=debug,rift_analysis=info",
               RAYON_NUM_THREADS="2", TOKIO_WORKER_THREADS="2", NO_COLOR="1")
    return root, env


def supervise(stop, errors, guard, child, stage, log, deadline):
    guard.psutil.cpu_percent()
    while not stop.wait(.25) and child.poll() is None:
        try:
            require(time.monotonic() < deadline and log.stat().st_size < MAX_LOG,
                    "server time or trace bound crossed")
            guard.sample(stage, child.pid)
        except Exception as error:
            errors.append(repr(error))
            if managed_group(child.pid):
                os.killpg(child.pid, signal.SIGTERM)
            return


def await_publication(case, process, events, commit, deadline):
    document = case / ".rift/server.json"
    while time.monotonic() < deadline:
        require(process.poll() is None, "server stopped before complete publication")
        published = [row for row in events.find("index snapshot published")
                     if re.search(r'trigger="?startup\b', row["trace"])]
        if published and document.is_file():
            try:
                state = json.loads(document.read_text())
            except json.JSONDecodeError:
                time.sleep(.02)
                continue
            require(state["pid"] == process.pid and state["identity"]["version"] == "0.0.93+" + commit,
                    "binary runtime commit or process identity differs")
            return state["port"], state["identity"], published[0]
        time.sleep(.02)
    raise ValueError("complete startup publication exceeded early cap")


def issue(port, root, kind, row):
    first = initialize(port)
    if kind == "race":
        second = initialize(port)
        ready = threading.Barrier(2)
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(request, port, session, MARKER_ID, ready) for session in (first, second)]
            row["replies"] = [one.result(timeout=36) for one in futures]
        a, b = row["replies"]
        require(max(a["sent_ns"], b["sent_ns"]) < min(a["received_ns"], b["received_ns"]),
                "first requests did not overlap")
    elif kind == "late":
        path = root / "proof/late.ts"
        row["late_before_ns"] = time.monotonic_ns()
        with path.open("x") as new:
            new.write("export function leaf(): number { return 2; }\n")
        row["late_after_ns"] = time.monotonic_ns()
        row["late_source_sha256"] = sha(path)
        row["replies"] = [request(port, first, ALIAS_ID)]
        result = row["replies"][0]["raw"]["result"]["structuredContent"]
        require(result["outcome"] == "missing" and result["symbol_not_found"]["id"] == ALIAS_ID,
                "first raw late reply must be current missing for the competing alias")
        require(REVISION.fullmatch(result["revision"]), "late reply lacks the complete current tree revision")
        row["verified"] = {"id": ALIAS_ID, "outcome": "missing", "revision": result["revision"]}
    else:
        row["replies"] = [request(port, first, MARKER_ID)]
    if kind != "late":
        verified = [verify_reply(reply) for reply in row["replies"]]
        require(all(one == verified[0] for one in verified), "concurrent complete revisions disagree")
        row["verified"] = verified[0]


def restore_alias(port, root, row):
    (root / "proof/late.ts").unlink()
    row["late_restored_ns"] = time.monotonic_ns()
    answer = request(port, initialize(port), ALIAS_ID)
    row["late_restore_reply"] = answer
    result = answer["raw"]["result"]["structuredContent"]
    require(result["outcome"] == "found", "restored fixture did not establish the original alias")
    require(REVISION.fullmatch(result["revision"]) and result["revision"] != row["verified"]["revision"],
            "restored alias lacks a distinct complete revision")
    declaration, = result["declarations"]
    span = declaration["range"]
    require(declaration["path"] == "proof/leaf.ts" and (span["start"], span["end"]) == (7, 44),
            "restored public alias did not point to the original authored bytes")


def case_run(scratch, archive, out, binaries, arm, kind, guard, deadline):
    out.mkdir()
    row = {"arm": arm, "kind": kind, "status": "before startup"}
    contract = binaries["arms"][arm]
    save(out / "result.json", row)
    with tempfile.TemporaryDirectory(prefix="case-", dir=scratch) as stage:
        stage = Path(stage)
        root, env = stage_inputs(archive, stage, kind)
        guard.preflight(out.name)
        path = stage / "server.log"
        path.touch(exist_ok=False)
        events = Events(path, out / "events.jsonl")
        ceiling = min(deadline, time.monotonic() + (180 if kind == "late" else CASE_SECONDS))
        errors = []
        stop = threading.Event()
        argv = [str(guard.scratch / arm / "rift"), "server", "start", "--foreground", "--auth", "skip"]
        with path.open("wb") as sink:
            row["spawn_ns"] = time.monotonic_ns()
            with subprocess.Popen(["taskset", "-c", guard.cpus, *argv], cwd=root, env=env,
                                  stdout=sink, stderr=sink, start_new_session=True) as child:
                supervisor = threading.Thread(target=supervise,
                    args=(stop, errors, guard, child, out.name, path, ceiling), daemon=True)
                supervisor.start()
                try:
                    require(managed_group(child.pid), "server process group ownership failed")
                    port, identity, published = await_publication(root, child, events, contract["built_commit"],
                        min(ceiling, time.monotonic() + 105))
                    row.update(identity=identity, publication=published)
                    require(not trace_seed(events), "request already consumed startup seed")
                    row["memory_pre_read"] = memory(child.pid, proportional=False)
                    issue(port, root, kind, row)
                    row["memory_post_read"] = memory(child.pid, proportional=True)
                    if kind == "late":
                        restore_alias(port, root, row)
                    while time.monotonic() < ceiling - 8 and child.poll() is None:
                        lexical = events.find("lexical commit settled")
                        if lexical:
                            row["first_lexical_settle"] = lexical[0]
                            break
                        time.sleep(.04)
                    require("first_lexical_settle" in row, "startup lexical settle absent")
                    seeds = trace_seed(events)
                    row["prepared_captures"] = seeds
                    if arm == "base":
                        require(not seeds, "baseline unexpectedly emitted candidate trace")
                    elif kind == "clean":
                        one, = seeds
                        require(one["reused_selection"] and one["installed"] and one["file_body_reads"] == 0
                                and one["paths"] > 16_000, "candidate did not reuse full Bun inventory and zero bodies")
                    elif kind == "race":
                        require(len(seeds) <= 1, "two concurrent requests consumed a one-shot seed")
                    else:
                        one, = seeds
                        require(not one["reused_selection"] and one["file_body_reads"] >= 1,
                                "late competing source did not force physical freshness fallback")
                        row["late_watch_batches"] = events.find("filesystem invalidations coalesced")
                    row["status"] = "passed"
                except Exception as error:
                    row["refusal"] = repr(error)
                    raise
                finally:
                    save(out / "result.json", row)
                    stop.set()
                    supervisor.join(timeout=3)
                    row["server_exit_code"] = stop_process(child)
                    row["guard_errors"] = errors
                    events.read()
                    events.stream.close()
                    save(out / "result.json", row)
                    require(not errors, f"bounded guard refused {out.name}")
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bun", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    locked = json.loads(Path(__file__).with_name("contract.json").read_text())
    check_contract(locked, armed=True)
    args.scratch = args.scratch.resolve()
    args.evidence.mkdir(parents=True, exist_ok=True)
    verified = json.loads((args.scratch / "verified-pair.json").read_text())
    for label in ("base", "candidate"):
        arm = verified["arms"][label]
        require(all(arm[key] == value for key, value in locked[label].items()), "paired binary lock changed")
        require(sha(args.scratch / label / "rift") == arm["binary_sha256"], "paired binary bytes changed")
    require(subprocess.check_output(["git", "-C", str(args.bun), "rev-parse", "HEAD"], timeout=5).decode().strip()
            == locked["bun_pin"], "corpus checkout pin differs")
    archive = args.scratch / "bun.tar"
    subprocess.run(["git", "-C", str(args.bun), "archive", "--format=tar", "-o", str(archive), locked["bun_pin"]],
                   check=True, timeout=45)
    guard = Guard(args.evidence, args.scratch)
    rows = []
    receipt = {"archive_sha256": sha(archive), "settings_sha256": hashlib.sha256(CONFIG).hexdigest(),
               "selected_cpus": guard.cpus, "cases": rows, "retained_map_bytes": None}
    path = args.evidence / "receipt.json"
    save(path, receipt)
    deadline = time.monotonic() + RUN_SECONDS
    for number, (arm, kind) in enumerate((("base", "clean"), ("candidate", "clean"), ("candidate", "clean"),
                                         ("base", "clean"), ("base", "clean"), ("candidate", "clean"),
                                         ("candidate", "race"), ("candidate", "late")), 1):
        require(time.monotonic() < deadline, "combined case budget exhausted")
        row = case_run(args.scratch, archive, args.evidence / f"{number:02}-{arm}-{kind}", verified, arm,
                       kind, guard, deadline)
        rtts = [reply["round_trip_ms"] for reply in row["replies"]]
        rows.append(row | {"replies": len(rtts), "round_trips_ms": rtts})
        save(path, receipt)
    require(all(row["verified"] == rows[0]["verified"] for row in rows[:-1]),
            "pinned current source or complete revisions differed")
    require(rows[-1]["verified"]["revision"] != rows[0]["verified"]["revision"],
            "late current tree revision did not change")
    receipt["status"] = "all matched exact protocol controls passed"
    receipt["paired_round_trip_ms"] = [
        {row["arm"]: row["round_trips_ms"][0] for row in rows[begin:begin + 2]}
        for begin in (0, 2, 4)
    ]
    save(path, receipt)


if __name__ == "__main__":
    main()
