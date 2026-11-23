"""Compare serial ordinary named and independent cold observations."""

import json
from pathlib import Path

from audit import BUN_SOURCE, GATES, PHASES, RIFT_SOURCE, RIFT_TREE, audit, require, valid_digest


def read_pass(path: Path, pass_name: str, binary: str):
    require(path.stat().st_size <= 256 * 1024, f"{pass_name} raw output exceeds its bound")
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    require(1 <= len(rows) <= 40, f"{pass_name} raw output exceeds its row bound")
    head, *events = rows
    expected = {
        "event": "provenance", "pass": pass_name, "mode": "ordinary_observer",
        "rift_source": RIFT_SOURCE, "rift_tree": RIFT_TREE, "bun_source": BUN_SOURCE,
        "observer_binary_sha256": binary,
    }
    require(head == expected, f"{pass_name} provenance differs")
    gates = {}
    outcomes = {}
    baseline = None
    for row in events:
        kind = row["event"]
        if kind == "baseline":
            require(pass_name == "named" and baseline is None, "unexpected baseline")
            baseline = row
        elif kind == "progress":
            require(pass_name == "named" and row.get("phase") in PHASES, "unexpected progress")
        elif kind == "gate":
            key = row["phase"], row["family"]
            require(key[0] in PHASES and key[1] in GATES and key not in gates, "invalid or duplicate gate")
            if pass_name != "named":
                require(key[0] == pass_name, "foreign cold gate phase")
            gates[key] = row["observation"]
        elif kind == ("ordinary" if pass_name == "named" else "cold"):
            phase = row["phase"]
            require(phase in PHASES and phase not in outcomes, "invalid or duplicate ordinary outcome")
            if pass_name != "named":
                require(phase == pass_name, "foreign cold outcome")
            outcomes[phase] = row
        else:
            raise ValueError(f"{pass_name} unexpected event {kind}")
    return head, baseline, outcomes, gates


def gate_row(phase, family, named, independent):
    if named is None and independent is None:
        return None
    count_source = independent if independent is not None else named
    witnesses = [item for item in (named, independent) if item is not None]
    affected = next((item["first_affected"] for item in witnesses if item["first_affected"]), None)
    first = affected or next((item["first"] for item in witnesses if item["first"]), None)
    unknown = max(item["unknown"] for item in witnesses)
    # The cold capture emits no named-path observation. Every other owner
    # requires both ordinary paths before a quiet gate can be considered covered.
    coverage_gap = named is None or (family != "capture" and independent is None)
    scope = "named_path_dependency" if affected else "complete_corpus"
    if not first and coverage_gap:
        first = {"reason": "one ordinary pass did not observe this owner"}
        scope = "unobserved"
    return {
        "event": "gate", "phase": phase, "family": family, "first": first,
        "first_scope": scope, "status": "unproved" if first or unknown else "closed",
        "observed": count_source["observed"], "unknown": unknown,
        "wall_ns": named["wall_ns"] if named else 0,
        "named_counters": named["counters"] if named else None,
        "independent_cold_counters": independent["counters"] if independent else None,
        "independent_cold_observation_ns": independent["wall_ns"] if independent else None,
    }


def merge(directory: Path, binary: str):
    require(valid_digest(binary), "bad observer binary digest")
    header, base, named, named_gates = read_pass(directory / "named.jsonl", "named", binary)
    require(base is not None and tuple(named) == tuple(PHASES), "incomplete named pass")
    require(valid_digest(base["graph_sha256"]) and valid_digest(base["capture_sha256"]), "bad baseline digest")
    header.pop("pass")
    rows = [header]
    for phase, count in PHASES.items():
        _, _, cold_set, cold_gates = read_pass(directory / f"cold-{phase}.jsonl", phase, binary)
        require(set(cold_set) == {phase}, f"{phase} cold pass missing output")
        real, cold = named[phase], cold_set[phase]
        require(real.get("source_paths_changed") == count, f"{phase} wrong saved path count")
        require(real.get("held_key_check") is True, f"{phase} held keys differ")
        require(real.get("complete_graph") is True, f"{phase} ordinary graph incomplete")
        require(real.get("capture_verified") is True and cold.get("capture_verified") is True,
                f"{phase} closing capture not verified")
        for field in ("ordered_records_and_edges_sha256", "capture_sha256"):
            require(valid_digest(real.get(field)) and real[field] == cold.get(field),
                    f"{phase} independent cold differs in {field}")
        for field in ("current_revision", "accepted_files", "omitted_or_unparsed"):
            require(real[field] == cold[field], f"{phase} independent cold differs in {field}")
        if phase.endswith("_restore"):
            require(real["ordered_records_and_edges_sha256"] == base["graph_sha256"]
                    and real["capture_sha256"] == base["capture_sha256"]
                    and real["current_revision"] == base["revision"], f"{phase} accepted original differs")
        rows.append(real | {"independent_cold_agrees": True,
                            "ordinary_independent_cold_ns": cold["ordinary_independent_cold_ns"]})
        for family in GATES:
            key = phase, family
            gate = gate_row(phase, family, named_gates.get(key), cold_gates.get(key))
            if gate:
                rows.append(gate)
    summary = audit(rows, binary)
    for report in summary["phases"].values():
        actual = [row for row in report["gates"]
                  if row["status"] == "unproved" and row.get("first_scope") in
                  {"named_path_dependency", "complete_corpus"}]
        report["first_actual_refusal"] = next((row for row in actual
                                               if row["first_scope"] == "named_path_dependency"),
                                              actual[0] if actual else None)
    return rows, summary
