"""Hold the delivery pipeline to what a job's own limit can carry.

A required job fails the gate when it crosses its limit, whatever its tests
did. Two conditions have ended a job that way, and both are structural rather
than behavioral: two jobs writing one cache key, so the second pays a save it
never needed, and a suite whose machine-global resource is serialized by an
in-process mutex, which nextest's separate test processes never share.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any

import tomllib
import yaml

REPOSITORY = Path(__file__).resolve().parents[2]
WORKFLOWS = REPOSITORY / ".github/workflows"
CODECOV_CONFIGURATION = REPOSITORY / "codecov.yml"
NEXTEST_CONFIGURATION = REPOSITORY / ".config/nextest.toml"
JUSTFILE = REPOSITORY / "justfile"

# The archive the integration jobs run from names the test targets it carries.
ARCHIVED_TARGET = re.compile(r"--test\s+([a-z0-9_]+)")

# The live profile selects a binary by name, and a test inside any binary by the
# test's own name.
INTEGRATION_TEST = re.compile(
    r"#\[(?:tokio::)?test[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?fn\s+(live_[a-z0-9_]*)"
)

# The action every Rust job restores its build directory with.
RUST_CACHE = "Swatinem/rust-cache@"

# The action every job reports to Codecov with. A step naming `report_type` sends
# test results rather than coverage, and no coverage status waits for it.
CODECOV_ACTION = "codecov/codecov-action@"

# The workflow whose coverage uploads a pull request's Codecov status is computed
# from, and whose `gate` job is the one check the `main` ruleset requires.
COVERAGE_WORKFLOW = "ci.yml"
GATE_JOB = "gate"

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

# The suites that read the model hub, and where each one lives. Every wait they
# declare is spelled as a `Duration` constant, so the deadline nextest ends them
# at has to sit above the sum of those waits: a test cut short by the deadline
# reports a timeout instead of the readiness it reached.
HUB_SUITES = {
    "live_vector_search": "crates/rift-mcp/tests/live_vector_search.rs",
    "live_model_sources": "crates/rift-search/tests/live_model_sources.rs",
}

# One declared wait, in the unit it was written in.
DECLARED_WAIT = re.compile(r"Duration::from_(millis|secs|mins)\((\d[\d_]*)\)")

# What one unit is worth in seconds.
WAIT_SECONDS = {"millis": 0.001, "secs": 1.0, "mins": 60.0}

# A nextest timeout is written as a count and a unit suffix.
TIMEOUT_PERIOD = re.compile(r"^(\d+)(ms|s|m)$")
TIMEOUT_SECONDS = {"ms": 0.001, "s": 1.0, "m": 60.0}

# The command that starts sccache against the R2 build cache, the secrets only
# its step may read, and a step that runs Cargo or a recipe that does.
BUILD_CACHE_COMMAND = "rift-dev build-cache"
BUILD_CACHE_SECRET = re.compile(r"secrets\.R2_BUILD_CACHE_[A-Z_]+")
CARGO_COMMAND = re.compile(r"(?:^|\s)(?:cargo|just)\s")

# The workflow that merges a dependency pull request without a human, the step
# that decides whether a bump may take that path, and the output it decides it
# in.
AUTOMERGE_WORKFLOW = "dependabot-automerge.yml"
RANGE_STEP = "range"
RANGE_OUTPUT = "breaking"


def declared_waits(path: str) -> float:
    """The seconds one suite's declared waits add up to.

    The sum is what one test of that suite can spend before it gives up, since
    no test waits on a budget it did not name.
    """
    source = (REPOSITORY / path).read_text(encoding="utf-8")
    return sum(
        WAIT_SECONDS[unit] * int(count.replace("_", ""))
        for unit, count in DECLARED_WAIT.findall(source)
    )


def timeout_seconds(period: str) -> float:
    """One nextest timeout period in seconds."""
    matched = TIMEOUT_PERIOD.match(period)
    assert matched, f"a nextest period is a count and a unit: {period!r}"
    return TIMEOUT_SECONDS[matched.group(2)] * int(matched.group(1))


def suite_deadline(binary: str) -> float:
    """The seconds nextest lets one test of `binary` run for.

    The deadline is `slow-timeout.period` times `terminate-after`, taken from
    the override that names the binary, or from the default profile when none
    does.
    """
    configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
    profile = configuration["profile"]["default"]
    timeout = profile["slow-timeout"]
    for override in profile.get("overrides", []):
        if f"binary(={binary})" in override["filter"] and "slow-timeout" in override:
            timeout = override["slow-timeout"]
    return timeout_seconds(timeout["period"]) * timeout.get("terminate-after", 1)


def automerge_step(step_id: str) -> dict[str, Any]:
    """One step of the auto-merge job, by its id."""
    document = workflow_documents()[AUTOMERGE_WORKFLOW]
    for step in document["jobs"]["automerge"]["steps"]:
        if step.get("id") == step_id:
            return step
    raise AssertionError(f"{AUTOMERGE_WORKFLOW} has no step with id {step_id!r}")


def updated_dependencies(*bumps: tuple[str, str]) -> str:
    """`fetch-metadata`'s own JSON for the dependencies one pull request bumps.

    Each bump is the version it moves from and the semver field that moved, the
    two members the classification reads. The field names are the action's:
    `dependabot/fetch-metadata@v3` serializes `updatedDependency`, whose version
    members are `prevVersion` and `newVersion`.
    """
    return json.dumps(
        [
            {
                "dependencyName": f"dependency-{index}",
                "prevVersion": previous,
                "newVersion": "",
                "updateType": update_type,
            }
            for index, (previous, update_type) in enumerate(bumps)
        ]
    )


def classified(updated: str) -> str:
    """Runs the workflow's own classification over `updated` and reads its output.

    The script is taken from the workflow file rather than restated here, so a
    rule that changes in one place cannot pass a test asserting the other.
    """
    script = automerge_step(RANGE_STEP)["run"]
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "output"
        output.touch()
        subprocess.run(
            ["bash", "-e", "-c", script],
            check=True,
            env={
                "PATH": os.environ["PATH"],
                "UPDATED": updated,
                "GITHUB_OUTPUT": str(output),
            },
        )
        written = dict(
            line.split("=", 1)
            for line in output.read_text(encoding="utf-8").splitlines()
            if "=" in line
        )
    return written[RANGE_OUTPUT]


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


def recipe_body(name: str) -> str:
    """One justfile recipe's body."""
    return JUSTFILE.read_text(encoding="utf-8").split(f"\n{name}:")[1].split("\n\n")[0]


def archived_corpus_targets() -> set[str]:
    """The test targets `just integration-archive` builds into its archive."""
    return set(ARCHIVED_TARGET.findall(recipe_body("integration-archive")))


def suites(selected: str) -> set[str]:
    """Every test binary a profile selects: one whose name carries the prefix,
    and, for the live profile, one holding a test named for it."""
    found: set[str] = set()
    for directory in sorted(REPOSITORY.glob("crates/*/tests")):
        for path in sorted(directory.glob("*.rs")):
            source = path.read_text(encoding="utf-8")
            if not TEST_ATTRIBUTE.search(source):
                continue
            named = path.stem.startswith(selected)
            holds = selected == "live_" and INTEGRATION_TEST.search(source)
            if named or holds:
                found.add(path.stem)
    return found


class ArchivedSuites(unittest.TestCase):
    """Each archive carries the suites its own profile runs.

    The corpus suites need the optimized build and travel in the integration
    archive; the live suites read the same build the unit suites do and travel
    in the fast archive. A suite selected by a profile whose archive leaves it
    out stops running with nothing to say so, which is why the live profile's
    second rule - a test named `live_` inside any binary - is checked too.
    """

    def test_the_integration_archive_carries_exactly_the_corpus_suites(self) -> None:
        archived = archived_corpus_targets()
        corpus = suites("corpus_")
        self.assertTrue(corpus, "no corpus suite exists")
        self.assertEqual(sorted(archived), sorted(corpus))

    def test_the_fast_archive_keeps_every_suite_the_live_profile_runs(self) -> None:
        body = recipe_body("fast-archive")
        live = suites("live_")
        self.assertTrue(live, "no live suite exists")
        self.assertIn("not binary(/^corpus_/)", body)
        self.assertNotIn(
            "live_",
            body,
            "the fast archive is what the live suites run from, so its filterset "
            f"leaves none of them out: {sorted(live)}",
        )

    def test_no_library_test_reaches_the_live_profile(self) -> None:
        """No archive names a `--lib` target, so a `live_` test in a library
        would be selected by the profile and absent from every archive."""
        offenders = [
            f"{path.relative_to(REPOSITORY)}::{name}"
            for path in sorted(REPOSITORY.glob("crates/*/src/**/*.rs"))
            for name in INTEGRATION_TEST.findall(path.read_text(encoding="utf-8"))
        ]
        self.assertEqual(
            offenders,
            [],
            f"a library test named `live_` never reaches an archive: {offenders}",
        )


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


def coverage_uploads() -> list[tuple[str, str]]:
    """Every `ci` step that uploads coverage, as job and flag."""
    return [(job_name, flag) for job_name, flag, _legs in coverage_upload_steps()]


def coverage_builds() -> int:
    """How many coverage reports one `ci` run sends: a step inside a matrix job
    uploads once per leg, and Codecov counts each upload as one build."""
    return sum(legs for _job, _flag, legs in coverage_upload_steps())


def coverage_upload_steps() -> list[tuple[str, str, int]]:
    """Every `ci` step that uploads coverage, as job, flag, and matrix legs."""
    document = workflow_documents()[COVERAGE_WORKFLOW]
    uploads: list[tuple[str, str, int]] = []
    for job_name, job in (document.get("jobs") or {}).items():
        for step in job.get("steps") or []:
            if not step.get("uses", "").startswith(CODECOV_ACTION):
                continue
            inputs = step.get("with") or {}
            if inputs.get("report_type"):
                continue
            uploads.append(
                (job_name, str(inputs.get("flags", "")), matrix_legs(job_name, job))
            )
    return uploads


def matrix_legs(job_name: str, job: dict) -> int:
    """How many runs one job's matrix expands to; a job without a matrix runs once.

    An `include`-only matrix runs one leg per entry, and a matrix of axes runs their
    product. A matrix mixing axes with `include` or `exclude` can add or drop legs
    in ways this count does not model, so it is refused rather than guessed.
    """
    matrix = (job.get("strategy") or {}).get("matrix") or {}
    include = matrix.get("include") or []
    axes = [
        values for key, values in matrix.items() if key not in ("include", "exclude")
    ]
    if not axes:
        return max(len(include), 1)
    if include or matrix.get("exclude"):
        raise AssertionError(
            f"job `{job_name}` mixes matrix axes with include or exclude; "
            "count its coverage uploads by hand"
        )
    legs = 1
    for values in axes:
        legs *= len(values)
    return legs


class CoverageNotifications(unittest.TestCase):
    """Codecov reports once every coverage upload a pull request produces is in."""

    def expected_builds(self) -> int:
        codecov = yaml.safe_load(CODECOV_CONFIGURATION.read_text(encoding="utf-8"))
        return int(codecov["codecov"]["notify"]["after_n_builds"])

    def test_codecov_waits_for_every_coverage_upload(self) -> None:
        uploads = coverage_upload_steps()
        self.assertEqual(
            self.expected_builds(),
            coverage_builds(),
            f"{COVERAGE_WORKFLOW} uploads coverage from {uploads}; "
            "`codecov.notify.after_n_builds` names how many to wait for, or a "
            "status is computed from a fraction of the run",
        )

    def test_the_comment_waits_for_the_same_uploads(self) -> None:
        codecov = yaml.safe_load(CODECOV_CONFIGURATION.read_text(encoding="utf-8"))
        self.assertEqual(
            codecov["comment"]["after_n_builds"],
            self.expected_builds(),
            "the comment and the status describe one report",
        )

    def test_every_coverage_upload_carries_its_own_flag(self) -> None:
        flags = [flag for _job, flag in coverage_uploads()]
        self.assertEqual(
            sorted(flags),
            sorted(set(flags)),
            f"two coverage uploads share one flag: {flags}",
        )


def jobs_with_step_limits() -> list[tuple[str, str, int, list[int]]]:
    """Every job that bounds a step, with its own limit and those step limits.

    A step limit written as a workflow expression resolves from the matrix at
    run time and is left out: what it bounds is a leg of the job, not the job.
    """
    bounded: list[tuple[str, str, int, list[int]]] = []
    for name, document in workflow_documents().items():
        for job_name, job in (document.get("jobs") or {}).items():
            job_limit = job.get("timeout-minutes")
            if not isinstance(job_limit, int):
                continue
            steps = [
                step["timeout-minutes"]
                for step in job.get("steps") or []
                if isinstance(step.get("timeout-minutes"), int)
            ]
            if steps:
                bounded.append((name, job_name, job_limit, steps))
    return bounded


class RequiredGate(unittest.TestCase):
    """The ruleset requires `gate` alone, so `gate` has to wait on every job."""

    def test_the_gate_needs_every_other_job(self) -> None:
        jobs = workflow_documents()[COVERAGE_WORKFLOW]["jobs"]
        needed = set(jobs[GATE_JOB]["needs"])
        others = set(jobs) - {GATE_JOB}
        self.assertEqual(
            needed,
            others,
            "a job outside the gate's needs merges red without the ruleset seeing it",
        )


class JobBudgets(unittest.TestCase):
    """A job's limit covers the work its steps do not bound."""

    def test_a_job_limit_leaves_room_past_its_longest_step(self) -> None:
        for workflow, job_name, job_limit, steps in jobs_with_step_limits():
            longest = max(steps)
            self.assertGreater(
                job_limit,
                longest,
                f"{workflow}:{job_name} bounds a step at {longest} minutes inside a "
                f"{job_limit}-minute job, leaving nothing for the setup, the cache "
                "save, and the artifact upload around it",
            )

    def test_the_repository_declares_bounded_steps_to_check(self) -> None:
        self.assertTrue(jobs_with_step_limits(), "no job bounds a step of its own")


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


class AutoMergeHoldsBreakingBumps(unittest.TestCase):
    """Auto-merge admits a bump only while it keeps its compatibility range.

    A major bump is not the same thing. Cargo, npm and uv read a pre-1.0
    version's leftmost non-zero field as the breaking one, so `0.10.9` to
    `0.11.0` and `0.0.3` to `0.0.4` leave their ranges while Dependabot reports
    them as a minor and a patch. Nothing reached `main` through this path while
    every dependency run failed the required `rust` check; once those runs go
    green the classification is the only thing left holding a breaking bump
    back.
    """

    def setUp(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("the classification reads its metadata with jq")

    def test_a_major_bump_above_one_is_held(self) -> None:
        self.assertEqual(
            classified(updated_dependencies(("1.4.0", "version-update:semver-major"))),
            "true",
        )

    def test_a_minor_bump_above_one_merges(self) -> None:
        self.assertEqual(
            classified(updated_dependencies(("1.4.0", "version-update:semver-minor"))),
            "false",
        )

    def test_a_patch_bump_above_one_merges(self) -> None:
        self.assertEqual(
            classified(
                updated_dependencies(("2.87.15", "version-update:semver-patch"))
            ),
            "false",
        )

    def test_a_pre_one_minor_bump_is_held(self) -> None:
        for previous in ("0.10.9", "0.26.13"):
            with self.subTest(previous=previous):
                self.assertEqual(
                    classified(
                        updated_dependencies((previous, "version-update:semver-minor"))
                    ),
                    "true",
                    f"{previous} leaves its compatibility range on a minor bump",
                )

    def test_a_pre_one_patch_bump_merges(self) -> None:
        self.assertEqual(
            classified(
                updated_dependencies(("0.26.12", "version-update:semver-patch"))
            ),
            "false",
        )

    def test_a_zero_zero_patch_bump_is_held(self) -> None:
        self.assertEqual(
            classified(updated_dependencies(("0.0.3", "version-update:semver-patch"))),
            "true",
            "every field of a 0.0.z version is breaking",
        )

    def test_a_tag_spelling_carrying_v_is_read_as_its_version(self) -> None:
        self.assertEqual(
            classified(
                updated_dependencies(("v4.37.6", "version-update:semver-patch"))
            ),
            "false",
        )

    def test_a_group_keeping_every_range_merges(self) -> None:
        self.assertEqual(
            classified(
                updated_dependencies(
                    ("1.4.0", "version-update:semver-minor"),
                    ("0.26.12", "version-update:semver-patch"),
                    ("2.87.15", "version-update:semver-patch"),
                )
            ),
            "false",
        )

    def test_one_breaking_entry_holds_a_whole_group(self) -> None:
        self.assertEqual(
            classified(
                updated_dependencies(
                    ("1.4.0", "version-update:semver-minor"),
                    ("0.10.9", "version-update:semver-minor"),
                )
            ),
            "true",
            "a group's highest field is a minor while one member leaves its range",
        )

    def test_a_version_the_metadata_could_not_name_is_held(self) -> None:
        self.assertEqual(
            classified(updated_dependencies(("", "version-update:semver-patch"))),
            "true",
            "a bump this cannot classify waits for a human",
        )

    def test_both_branches_hang_off_the_classification(self) -> None:
        conditions = [
            step["if"]
            for step in workflow_documents()[AUTOMERGE_WORKFLOW]["jobs"]["automerge"][
                "steps"
            ]
            if "if" in step
        ]
        self.assertEqual(
            sorted(conditions),
            sorted(
                [
                    f"success() && steps.{RANGE_STEP}.outputs.{RANGE_OUTPUT} != 'true'",
                    f"success() && steps.{RANGE_STEP}.outputs.{RANGE_OUTPUT} == 'true'",
                ]
            ),
            "auto-merge and its comment both decide on the classification, and "
            "both carry the success check an explicit `if` would otherwise drop",
        )


class HubSuitesOutliveTheWaitsTheyDeclare(unittest.TestCase):
    """A suite reading the model hub is not cut short by its own deadline.

    Each of these suites names the budget it will wait for a download, and
    fails naming the state it reached once that budget is spent. A deadline
    below the sum of those budgets ends the test first, and the report is a
    nextest timeout carrying none of what the suite was about to say.
    """

    def test_every_hub_suite_outlives_the_waits_it_declares(self) -> None:
        for binary, path in HUB_SUITES.items():
            with self.subTest(binary=binary):
                declared = declared_waits(path)
                self.assertGreater(declared, 0.0, f"{path} declares no wait")
                self.assertGreater(
                    suite_deadline(binary),
                    declared,
                    f"{binary} can wait {declared}s and is ended at "
                    f"{suite_deadline(binary)}s, so its own failure never prints",
                )


class BuildCacheKey(unittest.TestCase):
    """The R2 key reaches the step that starts sccache, and every job that
    compiles starts it before its first compile.

    The server that step starts holds the key for the rest of the job, so a
    secret mapped anywhere else hands the key to a process that never needed
    it, and a compile that runs before the server starts reaches no cache.
    """

    def test_only_the_build_cache_step_reads_the_r2_secrets(self) -> None:
        offenders: list[str] = []
        for name, document in workflow_documents().items():
            if BUILD_CACHE_SECRET.search(yaml.safe_dump(document.get("env") or {})):
                offenders.append(f"{name}: workflow env")
            for job_name, job in (document.get("jobs") or {}).items():
                if BUILD_CACHE_SECRET.search(yaml.safe_dump(job.get("env") or {})):
                    offenders.append(f"{name}:{job_name}: job env")
                for step in job.get("steps") or []:
                    if BUILD_CACHE_SECRET.search(
                        yaml.safe_dump(step)
                    ) and BUILD_CACHE_COMMAND not in step.get("run", ""):
                        offenders.append(f"{name}:{job_name}: {step.get('name')}")
        self.assertEqual(
            offenders, [], "an R2 secret reaches a step that does not start sccache"
        )

    def test_every_compiling_job_starts_the_build_cache_first(self) -> None:
        """A job restoring the Cargo registry with `Swatinem/rust-cache` is one
        that compiles, and none of its steps before the build cache runs Cargo."""
        jobs = workflow_documents()[COVERAGE_WORKFLOW]["jobs"]
        compiling = {
            name: job["steps"]
            for name, job in jobs.items()
            if any(step.get("uses", "").startswith(RUST_CACHE) for step in job["steps"])
        }
        self.assertTrue(compiling, "no ci job restores the Cargo registry")
        for name, steps in compiling.items():
            with self.subTest(job=name):
                starts = [
                    index
                    for index, step in enumerate(steps)
                    if BUILD_CACHE_COMMAND in step.get("run", "")
                ]
                self.assertTrue(starts, f"{name} compiles without the build cache")
                early = [
                    step.get("name") or step["run"]
                    for step in steps[: starts[0]]
                    if CARGO_COMMAND.search(step.get("run", ""))
                ]
                self.assertEqual(early, [], f"{name} compiles before sccache starts")
