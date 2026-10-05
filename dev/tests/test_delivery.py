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
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Any

import tomllib
import yaml
from rift_dev.commands import Command

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
RELEASE_WORKFLOW = "rift-release.yml"
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

# The shared end-to-end harness, the helper name a suite includes it by, the
# bounds it puts on one proxied call and on one proxied engine call, and the
# helper a case makes an engine call through.
HARNESS = "crates/rift/tests/harness.rs"
HARNESS_HELPER = "harness"
PROXIED_CALL_BOUND = "PROXIED_CALL_MAX"
PROXIED_ENGINE_CALL_BOUND = "PROXIED_ENGINE_CALL_MAX"
PROXIED_ENGINE_CALL = "proxied_engine_call"

# One test function in a suite, by name.
TEST_FUNCTION = re.compile(
    r"#\[(?:tokio::)?test[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)"
)

# A nextest filterset names one test by its exact name as `test(=<name>)`.
FILTERED_TEST = re.compile(r"test\(=([a-z0-9_]+)\)")

# A nextest timeout is written as a count and a unit suffix.
TIMEOUT_PERIOD = re.compile(r"^(\d+)(ms|s|m)$")
TIMEOUT_SECONDS = {"ms": 0.001, "s": 1.0, "m": 60.0}

# The command that starts sccache against the R2 build cache, the secrets only
# its step may read, and a step that runs Cargo or a recipe that does.
BUILD_CACHE_COMMAND = "rift-dev build-cache"
BUILD_CACHE_SECRET = re.compile(r"secrets\.R2_BUILD_CACHE_[A-Z_]+")
CARGO_COMMAND = re.compile(r"(?:^|\s)(?:cargo|just)\s")

# The job that builds and runs the unit suite on each release target but x86-64
# Linux, the profile its legs run under unless a leg names its own, and the one
# leg that does: the slowest runner, whose run needs a longer global timeout.
NATIVE_JOB = "native"
CI_PROFILE = "ci"
MACOS_INTEL_RUNNER = "macos-15-intel"
MACOS_INTEL_PROFILE = "ci-macos-intel"

# A workflow expression reading one matrix value, or the text after `||` when
# the leg sets none: `${{ matrix.profile || 'ci' }}`.
MATRIX_VALUE = re.compile(r"\$\{\{\s*matrix\.([a-z_]+)\s*\|\|\s*'([^']*)'\s*\}\}")

# The upload action that carries a leg's test report out of its job.
UPLOAD_ACTION = "actions/upload-artifact@"

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


def harness_bound(name: str) -> float:
    """The seconds one bound the harness declares as a `Duration` constant holds."""
    source = (REPOSITORY / HARNESS).read_text(encoding="utf-8")
    declared = re.compile(
        rf"const {name}: Duration =\s*Duration::from_(millis|secs|mins)\((\d[\d_]*)\)"
    )
    matched = declared.search(source)
    assert matched, f"{HARNESS} declares no {name}"
    unit, count = matched.groups()
    return WAIT_SECONDS[unit] * int(count.replace("_", ""))


def functions_calling(binary: str, call: str) -> set[str]:
    """The test functions of `crates/rift/tests/<binary>.rs` whose body calls `call`.

    A function's body runs from its own test attribute to the next one.
    """
    source = (REPOSITORY / f"crates/rift/tests/{binary}.rs").read_text(encoding="utf-8")
    functions = list(TEST_FUNCTION.finditer(source))
    calling: set[str] = set()
    for index, function in enumerate(functions):
        end = (
            functions[index + 1].start() if index + 1 < len(functions) else len(source)
        )
        if f"{call}(" in source[function.end() : end]:
            calling.add(function.group(1))
    return calling


def suites_including(helper: str) -> set[str]:
    """Every test binary in `crates/rift/tests` that includes `helper`."""
    including: set[str] = set()
    for path in sorted((REPOSITORY / "crates/rift/tests").glob("*.rs")):
        source = path.read_text(encoding="utf-8")
        if TEST_ATTRIBUTE.search(source) and helper in INCLUDED_HELPER.findall(source):
            including.add(path.stem)
    return including


def timeout_seconds(period: str) -> float:
    """One nextest timeout period in seconds."""
    matched = TIMEOUT_PERIOD.match(period)
    assert matched, f"a nextest period is a count and a unit: {period!r}"
    return TIMEOUT_SECONDS[matched.group(2)] * int(matched.group(1))


def suite_deadline(binary: str) -> float:
    """The seconds nextest lets any test of `binary` run for, overrides that
    name single tests aside."""
    return case_deadline(binary, None)


def case_deadline(binary: str, test: str | None) -> float:
    """The seconds nextest lets `test` of `binary` run for.

    The deadline is `slow-timeout.period` times `terminate-after`, taken from
    the first override that names the binary and sets a slow timeout, or from
    the default profile when none does, the precedence nextest applies to each
    setting. An override that also names tests applies only to those tests.
    """
    configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
    profile = configuration["profile"]["default"]
    timeout = profile["slow-timeout"]
    for override in profile.get("overrides", []):
        selected = f"binary(={binary})" in override["filter"]
        named = FILTERED_TEST.findall(override["filter"])
        if selected and "slow-timeout" in override and (not named or test in named):
            timeout = override["slow-timeout"]
            break
    return timeout_seconds(timeout["period"]) * timeout.get("terminate-after", 1)


def nextest_profile_setting(name: str, *keys: str) -> Any:
    """One setting of a nextest profile, read through its inheritance chain.

    A profile names its parent with `inherits`, every chain ends at the
    `default` profile, and nextest looks each setting up along the chain, so a
    nested key such as `junit.path` comes from the nearest profile that sets it.
    """
    profiles = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))[
        "profile"
    ]
    visited: list[str] = []
    while name not in visited:
        visited.append(name)
        profile = profiles.get(name, {})
        value: Any = profile
        for key in keys:
            value = value.get(key) if isinstance(value, dict) else None
        if value is not None:
            return value
        if name == "default":
            return None
        name = profile.get("inherits", "default")
    raise AssertionError(f"nextest profiles inherit in a cycle: {visited}")


def native_legs() -> list[dict[str, Any]]:
    """Each leg of the native job, as its matrix entry."""
    job = workflow_documents()[COVERAGE_WORKFLOW]["jobs"][NATIVE_JOB]
    return job["strategy"]["matrix"]["include"]


def native_step(accepts: Callable[[dict[str, Any]], bool]) -> dict[str, Any]:
    """The one native step `accepts` holds for."""
    steps = workflow_documents()[COVERAGE_WORKFLOW]["jobs"][NATIVE_JOB]["steps"]
    found = [step for step in steps if accepts(step)]
    assert len(found) == 1, f"expected one such native step, found {len(found)}"
    return found[0]


def for_leg(text: str, leg: dict[str, Any]) -> str:
    """`text` with every matrix expression replaced by the value `leg` gives it."""
    return MATRIX_VALUE.sub(
        lambda matched: str(leg.get(matched.group(1)) or matched.group(2)), text
    )


def leg_profile(leg: dict[str, Any]) -> str:
    """The nextest profile one native leg runs the unit suite under."""
    run = native_step(
        lambda step: step.get("name") == "Build every target and run the unit suite"
    )["run"]
    command = for_leg(run, leg)
    matched = re.search(r"--profile\s+(\S+)", command)
    assert matched, f"the native suite names no profile: {command}"
    return matched.group(1)


def leg_report(leg: dict[str, Any]) -> list[str]:
    """The paths one native leg uploads its test report and failure reports from."""
    upload = native_step(
        lambda step: (
            step.get("uses", "").startswith(UPLOAD_ACTION)
            and "native-tests" in (step.get("with") or {}).get("name", "")
        )
    )
    return for_leg(upload["with"]["path"], leg).split()


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
        Command("bash", "-e", "-c", script).with_environment(
            {
                "PATH": os.environ["PATH"],
                "UPDATED": updated,
                "GITHUB_OUTPUT": str(output),
            }
        ).output()
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

    def test_corpus_failures_reach_the_required_gate(self) -> None:
        corpus = workflow_documents()[COVERAGE_WORKFLOW]["jobs"]["corpus"]
        self.assertFalse(
            corpus.get("continue-on-error", False),
            "a corpus job failure must reach the required gate",
        )
        steps = [step for step in corpus["steps"] if step.get("id") == "corpus"]
        self.assertEqual(len(steps), 1, "the corpus job must run one corpus step")
        self.assertFalse(
            steps[0].get("continue-on-error", False),
            "a corpus test failure must fail its job, including Next.js churn",
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


class NativeRunBounds(unittest.TestCase):
    """The slowest native leg alone runs under a longer global timeout.

    The macos-15-intel runner needs more than the `ci` profile's bound for the
    same suite, so it runs under a profile of its own that changes that bound
    and nothing else, while every other leg keeps `ci`. Nextest writes a run's
    report under the folder of the profile it ran, so each leg's upload has to
    name that same folder or the report never leaves the job.
    """

    def test_the_intel_profile_changes_only_the_global_timeout(self) -> None:
        configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
        profile = configuration["profile"][MACOS_INTEL_PROFILE]
        self.assertEqual(
            profile.keys(),
            {"inherits", "global-timeout"},
            f"{MACOS_INTEL_PROFILE!r} must differ from {CI_PROFILE!r} in its "
            "global timeout alone",
        )
        self.assertEqual(profile["inherits"], CI_PROFILE)
        self.assertGreater(
            timeout_seconds(profile["global-timeout"]),
            timeout_seconds(nextest_profile_setting(CI_PROFILE, "global-timeout")),
        )

    def test_only_the_intel_leg_runs_under_the_intel_profile(self) -> None:
        profiles = {leg["os"]: leg_profile(leg) for leg in native_legs()}
        self.assertEqual(profiles.pop(MACOS_INTEL_RUNNER), MACOS_INTEL_PROFILE)
        self.assertTrue(profiles, "the native job runs no other leg")
        self.assertEqual(
            set(profiles.values()),
            {CI_PROFILE},
            f"every leg but {MACOS_INTEL_RUNNER} runs under {CI_PROFILE!r}: {profiles}",
        )

    def test_every_leg_uploads_the_report_its_profile_writes(self) -> None:
        configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
        store = configuration["store"]["dir"]
        for leg in native_legs():
            with self.subTest(runner=leg["os"]):
                profile = leg_profile(leg)
                report = nextest_profile_setting(profile, "junit", "path")
                self.assertTrue(report, f"the {profile!r} profile writes no report")
                self.assertEqual(
                    leg_report(leg),
                    [
                        f"{store}/{profile}/{report}",
                        f"{store}/{profile}/failure-windows/",
                        "target/integration/nextest/",
                    ],
                )


class WindowsUpdateSelection(unittest.TestCase):
    """The ignored publisher parent runs on both Windows targets without its child.

    The parent needs the built CLI and launches the child with its own fixture
    environment. A separate nextest profile preserves the unit suite's report.
    """

    def test_the_windows_run_selects_only_the_ignored_publisher_parent(self) -> None:
        step = native_step(
            lambda step: (
                step.get("name") == "Run the Windows running binary update regression"
            )
        )
        self.assertEqual(step["if"], "${{ !cancelled() && runner.os == 'Windows' }}")
        self.assertEqual(
            step["env"]["RIFT_UPDATE_TEST_BINARY"],
            "${{ github.workspace }}/target/debug/rift.exe",
        )
        self.assertEqual(step["env"]["RUSTFLAGS"], "${{ matrix.rustflags }}")
        command = step["run"]
        self.assertIn("--run-ignored all", command)
        self.assertIn("--no-tests fail", command)
        self.assertIn("--workspace --all-targets --all-features", command)
        self.assertIn("--profile ci-windows-update", command)
        self.assertEqual(
            re.findall(r"-E\s+'([^']+)'", command),
            [
                (
                    "binary_id(=rift::bin/rift) and "
                    "test(=update::tests::windows_publish_replaces_running_binary_and_cleans_backup)"
                )
            ],
        )
        unit = native_step(
            lambda step: step.get("name") == "Build every target and run the unit suite"
        )["run"]
        self.assertNotIn("--run-ignored", unit)
        self.assertIn("not test(/_probe$/)", unit)
        windows_targets = {
            leg["target"] for leg in native_legs() if leg["os"].startswith("windows-")
        }
        self.assertEqual(
            windows_targets,
            {"x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"},
        )

    def test_the_windows_run_retains_its_report_and_the_original_bounds(self) -> None:
        configuration = tomllib.loads(NEXTEST_CONFIGURATION.read_text(encoding="utf-8"))
        profile = configuration["profile"]["ci-windows-update"]
        self.assertEqual(profile.keys(), {"inherits", "junit"})
        self.assertEqual(profile["inherits"], CI_PROFILE)
        upload = native_step(
            lambda step: (
                (step.get("with") or {}).get("name")
                == "native-update-tests-${{ matrix.target }}"
            )
        )
        self.assertEqual(upload["if"], "${{ always() && runner.os == 'Windows' }}")
        self.assertEqual(
            upload["with"]["path"],
            "target/nextest/ci-windows-update/junit.xml",
        )
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        for setting in ("global-timeout", "slow-timeout", "retries", "leak-timeout"):
            self.assertEqual(
                nextest_profile_setting("ci-windows-update", setting),
                nextest_profile_setting(CI_PROFILE, setting),
            )


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


class ProxiedCallsFailInsideTheDeadline(unittest.TestCase):
    """A proxied call that never answers fails the case naming the call.

    The harness bounds one proxied call, and one proxied engine call, and the
    proxy's own forward budget ends inside the first. A bound at or past
    nextest's deadline never trips: nextest ends the case first, and the report
    is a timeout carrying nothing about the call that hung.
    """

    def test_the_proxied_call_bound_ends_inside_every_harness_suite_deadline(
        self,
    ) -> None:
        bound = harness_bound(PROXIED_CALL_BOUND)
        including = suites_including(HARNESS_HELPER)
        self.assertTrue(including, f"no suite includes {HARNESS}")
        for binary in sorted(including):
            with self.subTest(binary=binary):
                self.assertLess(
                    bound,
                    suite_deadline(binary),
                    f"{binary} bounds one proxied call at {bound}s and is ended at "
                    f"{suite_deadline(binary)}s, so a call that hangs never fails by name",
                )

    def test_every_engine_call_bound_ends_inside_its_case_deadline(self) -> None:
        """A case making an engine call makes one proxied call before it, so its
        deadline has to hold both bounds for the engine call's own to trip."""
        call_bound = harness_bound(PROXIED_CALL_BOUND)
        engine_bound = harness_bound(PROXIED_ENGINE_CALL_BOUND)
        cases = [
            (binary, test)
            for binary in sorted(suites_including(HARNESS_HELPER))
            for test in sorted(functions_calling(binary, PROXIED_ENGINE_CALL))
        ]
        self.assertTrue(cases, f"no harness suite calls {PROXIED_ENGINE_CALL}")
        for binary, test in cases:
            with self.subTest(binary=binary, test=test):
                deadline = case_deadline(binary, test)
                self.assertLess(
                    call_bound + engine_bound,
                    deadline,
                    f"{binary}::{test} bounds its calls at {call_bound}s and "
                    f"{engine_bound}s and is ended at {deadline}s, so an engine call "
                    f"that hangs never fails by name",
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
        documents = workflow_documents()
        compiling = {
            f"{workflow}:{name}": job["steps"]
            for workflow in (COVERAGE_WORKFLOW, RELEASE_WORKFLOW)
            for name, job in documents[workflow]["jobs"].items()
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
