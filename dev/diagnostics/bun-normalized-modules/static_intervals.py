"""Count wall time occupied by exclusively Rust, exclusively other, and overlapping file passes."""
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import sys


def utc_ns(value):
    delta = datetime.fromisoformat(value.replace("Z", "+00:00")) - datetime(1970, 1, 1, tzinfo=timezone.utc)
    return ((delta.days * 86400 + delta.seconds) * 1_000_000 + delta.microseconds) * 1000


def summarize(path):
    header, *lines = path.read_text().splitlines()
    tag, duration, origin, started = header.split("\t")
    assert tag == "wall_ns" and origin == "abs_start_ns"
    wall = int(duration)
    tail, spent = lines.pop().split("\t")
    assert tail == "write_ns"
    points = defaultdict(lambda: [0, 0])
    groups = defaultdict(Counter)
    membership = []
    for line in lines:
        language, began, ended, budget, charges, exhausted, local, external, name = line.split("\t", 8)
        a, b = int(began), int(ended)
        assert 0 <= a <= b <= wall and 0 <= int(charges) <= int(budget)
        membership.append((name, language))
        slot = 0 if language == "rust" else 1
        points[a][slot] += 1
        points[b][slot] -= 1
        groups[language].update(files=1, task_ns=b-a, budget=int(budget), charges=int(charges),
                                exhausted=int(exhausted == "true"), occurrences=int(local), external=int(external))
    assert membership == sorted(membership) and len(set(membership)) == len(membership)
    buckets = [0, 0, 0, 0]
    rust, other, last = 0, 0, 0
    for moment, (r, o) in sorted(points.items()):
        buckets[(rust > 0) + 2 * (other > 0)] += moment - last
        rust += r
        other += o
        assert rust >= 0 and other >= 0
        last = moment
    assert rust == other == 0
    buckets[0] += wall - last
    assert sum(buckets) == wall
    return {"file": path.name, "start_utc_ns": int(started), "wall_ns": wall, "write_ns": int(spent),
            "outside_file_pass_ns": buckets[0], "rust_only_ns": buckets[1], "other_only_ns": buckets[2],
            "overlap_ns": buckets[3], "languages": groups, "files": len(membership),
            "inventory": hashlib.sha256(json.dumps(membership, separators=(",", ":")).encode()).hexdigest()}


def main(evidence):
    measurements = evidence / "measurements/instrumented"
    instrumented = [summarize(p) for p in sorted((evidence / "timings").glob("*.tsv"))]
    stages = {}
    for name in ("cold", "single", "single-restore", "burst", "burst-restore"):
        if name == "cold":
            start = 0
            event = json.loads((measurements / "cold.json").read_bytes())["publication_event"]
        else:
            stage = measurements / name
            physical = json.loads((stage / "physical-write.json").read_bytes())
            start = utc_ns(physical["utc"]) + physical["done_ns"] - physical["begin_ns"]
            event = json.loads((stage / "publication.json").read_bytes())["complete_event"]
        end = utc_ns("T".join(event.split()[:2])) + 1_000_000
        candidates = [row for row in instrumented if row["start_utc_ns"] >= start
                      and row["start_utc_ns"] + row["wall_ns"] + row["write_ns"] <= end]
        assert candidates, name
        selected = max(candidates, key=lambda row: row["start_utc_ns"])
        assert selected["languages"]["rust"]["files"] > 0
        if name != "cold":
            assert selected["inventory"] == stages["cold"]["inventory"]
        stages[name] = selected
    assert len({row["file"] for row in stages.values()}) == len(stages)
    (evidence / "static-intervals.json").write_text(json.dumps({"stages": stages, "all": instrumented}, indent=2) + "\n")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
