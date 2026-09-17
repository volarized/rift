"""Hold the delivery pipeline to what a job's own limit can carry.

A required job fails the gate when it crosses its limit, whatever its tests
did. Two conditions have ended a job that way, and both are structural rather
than behavioral: two jobs writing one cache key, so the second pays a save it
never needed, and a suite whose machine-global resource is serialized by an
in-process mutex, which nextest's separate test processes never share.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path
from typing import Any

import tomllib
import yaml

REPOSITORY = Path(__file__).resolve().parents[2]
WORKFLOWS = REPOSITORY / ".github/workflows"
NEXTEST_CONFIGURATION = REPOSITORY / ".config/nextest.toml"

# The action every Rust job restores its build directory with.
RUST_CACHE = "Swatinem/rust-cache@"

# The nextest group that serializes the suites taking one machine-global
# resource, and the resource itself: the loopback range an elected server binds.
ELECTION_GROUP = "election"
ELECTION_MARKERS = (
    # Binding inside the range itself.
    "SERVER_PORT_MIN",
    "SERVER_PORT_MAX",
    # Starting a server through the CLI.
    '"server", "start"',
    # Starting `rift mcp`, which elects one for the workspace it serves.
    "proxy_client",
    "proxy_command",
    "proxied_call",
    "proxied_engine_call",
    # Stopping the elected server the test left behind.
    "StopOnDrop::new",
)

# Cargo runs a test function only from a target it compiles, so a file holding
# one of these attributes is a suite rather than a helper another suite includes.
TEST_ATTRIBUTE = re.compile(r"#\[(?:tokio::)?test\b")

# A suite reaches a helper beside it with `mod <name>;`.
INCLUDED_HELPER = re.compile(r"^\s*mod\s+([a-z_][a-z0-9_]*)\s*;", re.MULTILINE)

# A nextest filterset names one test binary as `binary(=<name>)`.
FILTERED_BINARY = re.compile(r"binary\(=([a-z0-9_]+)\)")


def workflow_documents() -> dict[str, Any]:
    """Every workflow file, by name."""
    return {
        path.name: yaml.safe_load(path.read_text(encoding="utf-8"))
        for path in sorted(WORKFLOWS.glob("*.yml"))
    }


def cache_savers() -> list[tuple[str, str, str]]:
    """Every workflow step that writes the Rust cache, as workflow, job, key.

    A step setting `save-if: false` restores and never writes, so it owns no
    key. A step naming no `shared-key` gets one scoped to its own job, which no
    other job can collide with.
    """
    savers: list[tuple[str, str, str]] = []
    for name, document in workflow_documents().items():
        for job_name, job in (document.get("jobs") or {}).items():
            for step in job.get("steps") or []:
                uses = step.get("uses", "")
                if not uses.startswith(RUST_CACHE):
                    continue
                inputs = step.get("with") or {}
                if inputs.get("save-if") is False or inputs.get("save-if") == "false":
                    continue
                shared_key = inputs.get("shared-key")
                if shared_key is None:
                    continue
                savers.append((name, job_name, str(shared_key)))
    return savers


def election_group_binaries() -> set[str]:
    """The test binaries the nextest configuration serializes on the election."""
    configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
    overrides = configuration.get("profile", {}).get("default", {}).get("overrides", [])
    named: set[str] = set()
    for override in overrides:
        if override.get("test-group") != ELECTION_GROUP:
            continue
        named.update(FILTERED_BINARY.findall(override.get("filter", "")))
    return named


def suites_taking_the_election() -> set[str]:
    """Every test binary whose own source, or a helper it includes, binds the
    loopback range an elected server holds."""
    taking: set[str] = set()
    for directory in sorted(REPOSITORY.glob("crates/*/tests")):
        sources = {
            path.stem: path.read_text(encoding="utf-8")
            for path in directory.glob("*.rs")
        }
        for name, source in sources.items():
            if not TEST_ATTRIBUTE.search(source):
                continue
            reached = [source]
            reached.extend(
                sources[helper]
                for helper in INCLUDED_HELPER.findall(source)
                if helper in sources
            )
            if any(marker in text for text in reached for marker in ELECTION_MARKERS):
                taking.add(name)
    return taking


class CacheOwnership(unittest.TestCase):
    """One job writes one cache key."""

    def test_no_two_jobs_write_the_same_rust_cache_key(self) -> None:
        owners: dict[str, list[str]] = {}
        for workflow, job, key in cache_savers():
            owners.setdefault(key, []).append(f"{workflow}:{job}")
        for key, jobs in sorted(owners.items()):
            self.assertEqual(
                len(jobs),
                1,
                f"the cache key {key!r} is written by {jobs}; every job but its "
                "owner restores it with `save-if: false`",
            )

    def test_the_repository_declares_rust_cache_steps_to_check(self) -> None:
        self.assertTrue(cache_savers(), "no workflow step writes the Rust cache")


class MachineGlobalSuites(unittest.TestCase):
    """A machine-global resource is serialized by a nextest test group.

    Nextest runs each test in its own process, so a mutex inside one of them
    serializes nothing. Only a group, applied to every binary reaching the
    resource, keeps two of them off it at once.
    """

    def test_every_suite_taking_the_election_is_in_the_election_group(self) -> None:
        grouped = election_group_binaries()
        taking = suites_taking_the_election()
        self.assertTrue(taking, "no suite reaches the election port range")
        self.assertEqual(
            sorted(taking - grouped),
            [],
            f"these suites bind the election port range outside the "
            f"{ELECTION_GROUP!r} group: {sorted(taking - grouped)}",
        )

    def test_the_election_group_admits_one_suite_at_a_time(self) -> None:
        configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
        group = configuration.get("test-groups", {}).get(ELECTION_GROUP)
        self.assertEqual(
            group,
            {"max-threads": 1},
            f"the {ELECTION_GROUP!r} group must admit one test at a time",
        )

    def test_no_suite_serializes_on_a_mutex_of_its_own(self) -> None:
        offenders = [
            str(path.relative_to(REPOSITORY))
            for directory in sorted(REPOSITORY.glob("crates/*/tests"))
            for path in sorted(directory.glob("*.rs"))
            if re.search(r"static\s+SERIAL\b", path.read_text(encoding="utf-8"))
        ]
        self.assertEqual(
            offenders,
            [],
            "a mutex inside one test process serializes nothing across the "
            f"processes nextest runs; group these suites instead: {offenders}",
        )
