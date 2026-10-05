"""Fetch pinned trees or run their bounded suites through the compiled Rift binary."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import sys
import tempfile
import time
import traceback
from collections.abc import Callable
from pathlib import Path

from rift_dev.corpus_assertions import (
    CONTEXT_DEGRADED,
    CONTEXT_SPAN,
    HELD_UNPARSED_RECORD,
    PROBE_PATH,
    PROBE_SOURCE,
    READ_COUNT,
    SYMBOL_COUNT,
    active_stdout,
    build_records,
    chunked_answer,
    churn_answer,
    database_bytes,
    exact_degradation,
    fields,
    language_counts,
    last_line_pattern,
    lexical_breach,
    lexical_content,
    map_paths,
    no_failed_builds,
    number,
    open_history_batch,
    probe_units,
    records,
    sample_symbols,
    startup_published,
    stop_sizes,
    token_past_chunk,
    warnings,
)
from rift_dev.corpus_cache import Pin, git
from rift_dev.local_index_read import settled_local as read_settled_local
from rift_dev.log_records import (
    DATABASE_CLOSE,
    STAGE_ENDED,
    instant,
    stop_measurements,
)
from rift_dev.machine import machine, machine_line
from rift_dev.rift_test_client import (
    LOG_FILTER,
    Client,
    FailureLimit,
    Json,
    JsonObject,
    Server,
    ToolFailure,
    array_value,
    collector_counts,
    gate_deadline,
    object_value,
    require,
    string_value,
    utc_now,
)
from rift_dev.trace import TEST_CASE_KEY, Collector, collector, resource_attribute

# The OpenTelemetry specification's "Disable the SDK for all signals"; any value other than
# "true" leaves the export enabled.
SDK_DISABLED = "OTEL_SDK_DISABLED"

# The budgets bounding one corpus case each stand strictly inside the one outside them,
# so a breach fails naming the action that ran long instead of tearing down whatever the
# next budget out was waiting on:
#
#     STEADY_READ_SECONDS < READINESS_SECONDS < READ_SECONDS < CONVERGENCE_SECONDS
#                                                            < Corpus.work_seconds()
#
# READINESS_SECONDS is what the corpus writes to `[server] readiness_timeout`. The server
# answers one read within it, degrading to identifier matching when the lexical lane has
# not landed its transaction. A client deadline equal to that budget tears the transport
# down at the instant the server answers, so a read whose lane genuinely needs the whole
# budget can never deliver the answer the contract promises. The read carries the budget
# plus room for that answer to arrive.
READINESS_SECONDS = 30.0
READ_SECONDS = READINESS_SECONDS + 10.0
# A tool's first read over a large workspace under sustained edits waits for the first
# index to land, so it gets the whole read budget. Every read after it answers from a
# published index: run 35661449692 over `nextjs` measured 53 such reads at a median of
# 1.56 seconds, one outlier at 7.24 waiting out a rebuild, and everything else under 3.34,
# against a first `search` that spent the server's whole budget. The steady budget stands
# well above that outlier and strictly inside the readiness budget, so a steady read that
# starts waiting for the index fails the case instead of hiding in the first read's room.
STEADY_READ_SECONDS = READINESS_SECONDS * 2 / 3
# One `settled_local` call resends a read until the local index preparation warning
# clears, so it spans the startup snapshot's publication, the wait for the next poll, the
# server's answer, and the client's own handling of that answer. Over 17 passing runs of
# the `nextjs` workspace case the interval from the first `search` to the return from
# `settled_local` was 44.79 to 59.12 seconds: publication of the startup snapshot 41.68 to
# 52.72 seconds after the first call, up to 3.36 seconds until the next poll, a server
# answer of 1.35 to 1.63 seconds, and 1.16 to 2.34 seconds of client handling. A bound of
# 60 seconds left 0.88 seconds at the closest, and job 111716521102 crossed it by at most
# 0.53 seconds with the server's answer already sent at 58.47 seconds. The bound is twice
# the longest passing interval, 59.12 seconds, rounded up. Callers are the `workspace`
# case of `bun`, `nextjs`, and `fastapi` (the `shallow` read only for `fastapi`) and the
# `nextjs` `churn` case, with work budgets of 510, 690, and 210 seconds (the pinned
# 540, 720, and 240 less CLEANUP_RESERVE_SECONDS). The bound stands inside the smallest,
# `fastapi` at 210. Reads of one case run in sequence, so a case where every read ran
# to this bound would reach its work budget first and fail there, naming the action.
LOCAL_PREPARATION_SECONDS = 120.0
# Seconds a case keeps inside its own deadline for the served tree's removal,
# the report write, and the process exit. Nextest allows the same grace after
# it ends a corpus case, so the two bounds agree on what cleanup costs.
CLEANUP_RESERVE_SECONDS = 30.0
SEED = 34
POLL_SECONDS = 0.1
OBSERVATION_SECONDS = 60.0
STARTUP_PUBLICATION = "the startup index publication"
CONFIGURATION = (
    f'[server]\nreadiness_timeout = "{int(READINESS_SECONDS)}s"\n'
    "[search.vector]\ndisabled = true\n"
    "[logs]\npage_records = 5000\n"
    'capture = "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info"\n'
)
SAMPLE_LANGUAGES = {
    "bun": ("rust", "typescript", "typescript:tsx"),
    "nextjs": ("rust", "typescript", "typescript:tsx"),
    "fastapi": ("python",),
}
REQUIRED_LANGUAGES = {
    "bun": {"rust", "typescript"},
    "nextjs": {"typescript"},
    "fastapi": {"python"},
}


CHURN_REQUESTS: dict[str, JsonObject] = {
    "search": {
        "query": "corpus_probe",
        "target": "symbol",
        "include": ["source"],
        "limit": 1,
    },
    "get_symbol": {"name": "corpus_probe"},
    "nodes": {"path": PROBE_PATH, "position": 0},
}
# The convergence loop resends every churn tool until the newest source lands, so it
# stands outside the reads it repeats: one whole read budget for each of them.
CONVERGENCE_SECONDS = READ_SECONDS * len(CHURN_REQUESTS)


class Corpus:
    """One disposable corpus checkout, its external evidence, and tested actions."""

    def __init__(
        self, pin: Pin, binary: Path, report: Path, case: str = "workspace"
    ) -> None:
        require(case in ("workspace", "stop", "churn"), f"unknown corpus case: {case}")
        require(case != "stop" or pin.name == "bun", "only bun has a stop case")
        require(case != "churn" or pin.name == "nextjs", "only nextjs has a churn case")
        self.pin = pin
        self.case = case
        self.binary = binary.resolve()
        self.report = report.resolve()
        self.actions: list[Json] = []
        self.servers: list[Server] = []
        self.evidence: list[Json] = []
        self.stops: list[JsonObject] = []
        self.root = Path()
        self.sequence = 0
        self.started = time.monotonic()
        self.mark = utc_now()
        # The action `mark` is the end of; None before the first one finishes.
        self.last_action: str | None = None
        # The OTLP collector every server of the case exports to; None outside `run`.
        self.telemetry: Collector | None = None

    def record(self, action: str, **values: Json) -> None:
        """Append one finished action.

        `started_at` is when the previous action finished, or the case began, and
        `ended_at` is now, both UTC: the interval holds everything the action did.
        """
        ended = utc_now()
        entry: JsonObject = {
            "action": action,
            **values,
            "started_at": self.mark,
            "ended_at": ended,
            "elapsed_seconds": time.monotonic() - self.started,
        }
        self.mark = ended
        self.last_action = action
        self.actions.append(entry)

    def server(self, root: Path | None = None) -> Server:
        self.sequence += 1
        server = Server(
            self.binary,
            root or self.root,
            self.report.parent / f"{self.report.stem}.server-{self.sequence}.log",
            startup_seconds=180.0,
            env={
                "RUST_LOG": LOG_FILTER,
                "NO_COLOR": "1",
                "OTEL_RESOURCE_ATTRIBUTES": resource_attribute(
                    TEST_CASE_KEY, self.test_case_name()
                ),
                # The nextest runner disables export in every test process; the server
                # this case starts exports to the case's collector.
                SDK_DISABLED: "false",
            },
            collector=self.telemetry,
        )
        self.servers.append(server)
        return server

    def test_case_name(self) -> str:
        """The `test.case.name` every server of this case carries: the nextest attempt
        that runs the case, as the nextest runner files telemetry under it, else the
        repository and case."""
        return (
            os.environ.get("NEXTEST_ATTEMPT_ID")
            or f"corpus:{self.pin.name}:{self.case}"
        )

    def work_seconds(self) -> float:
        """The wall clock this case's own actions get.

        Nextest ends the case at the pinned deadline, so the actions stop before
        it: the reserve is the room the served tree's removal, the report write,
        and the process exit need inside that same deadline. A case that crosses
        this bound fails naming the action it was running, instead of being
        killed with no evidence of where it stood.
        """
        return max(self.pin.seconds - CLEANUP_RESERVE_SECONDS, 1.0)

    async def run(self) -> None:
        """A timeout fails the suite after server cleanup writes its evidence.

        The OTLP collector starts before the first server and stops after the served
        tree, and with it the last server, is gone.
        """
        started = time.monotonic()
        self.started = started
        self.mark = utc_now()
        status = "failed"
        failure = ""
        budget = self.work_seconds()
        self.report.parent.mkdir(parents=True, exist_ok=True)
        facts = machine()
        print(machine_line(facts), flush=True)
        try:
            async with asyncio.timeout(budget):
                with (
                    collector() as telemetry,
                    tempfile.TemporaryDirectory(
                        prefix=f"rift-corpus-{self.pin.name}-"
                    ) as directory,
                ):
                    self.telemetry = telemetry
                    await self.tree(Path(directory).resolve())
                elapsed = time.monotonic() - started
                require(
                    elapsed <= budget,
                    f"{self.pin.name}: elapsed {elapsed:.3f}s exceeded {budget}s",
                )
                status = "passed"
        except BaseException as error:
            failure = "".join(traceback.format_exception(error))
            raise
        finally:
            self.report.write_text(
                json.dumps(
                    {
                        "corpus": self.pin.name,
                        "case": self.case,
                        "commit": self.pin.commit,
                        "seed": SEED,
                        "machine": facts,
                        "status": status,
                        "failure": failure,
                        "evidence": self.evidence,
                        "stops": self.stops,
                        "collector": self.collector_counts(),
                        "elapsed_seconds": time.monotonic() - started,
                        "actions": self.actions,
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )

    def collector_counts(self) -> JsonObject | None:
        """What the case's collector received and dropped; None before it started."""
        return collector_counts(self.telemetry)

    async def tree(self, directory: Path) -> None:
        """Run the case; on failure keep each server's evidence before the tree goes."""
        try:
            await self.cases(directory)
        except BaseException:
            self.collect_evidence()
            raise

    def stop(self, server: Server) -> None:
        """Stop `server`, then keep its database sizes and persisted records.

        The served tree still exists here. The report's `stops` entry names the
        records file, or the error of a records read that failed; a failed read
        never fails the case. From the records of this server alone it also carries
        the `database.close` values (`database_close`: `busy`, `log`, and
        `checkpointed` per database) and the `stop stage ended` values (`stop_stages`:
        `stage`, `remaining`, `outcome`). `lacks` names each kind the records did
        not hold, and `records_lines` counts the records read.
        """
        server.stop()
        entry: JsonObject = {
            "stderr": str(server.log_path),
            "sizes": stop_sizes(server.root),
            "records": str(server.records_path),
        }
        try:
            text = server.read_records()
        except (OSError, RuntimeError, ValueError) as error:
            entry["records"] = None
            entry["records_error"] = str(error)
            entry["lacks"] = [DATABASE_CLOSE, STAGE_ENDED]
        else:
            measured = stop_measurements(text, instant(server.started_at))
            entry["records_lines"] = measured["records_lines"]
            entry["database_close"] = list(measured["database_close"])
            entry["stop_stages"] = list(measured["stop_stages"])
            entry["lacks"] = list(measured["lacks"])
        self.stops.append(entry)

    def collect_evidence(self) -> None:
        """Keep every server's stderr, proxy stderr, and persisted records of a failed case.

        The files sit beside the report. The report names them, and the newest part of
        each stream is written to stderr, which a pass never receives. The served
        tree still exists here, so `rift server logs` can read its `.rift/metrics`.
        """
        previous = self.last_action
        lower_bound = (
            f"the end of the last recorded action, {previous}; the failing action "
            "began at or after it"
            if previous is not None
            else "the start of the case; no action had finished"
        )
        for index, server in enumerate(self.servers, 1):
            since = max(self.mark, server.started_at)
            bound = (
                lower_bound
                if since == self.mark
                else "the start of this server, later than the last recorded action"
            )
            for note in server.evidence(since, bound):
                sys.stderr.write(note if note.endswith("\n") else note + "\n")
            self.evidence.append(
                {
                    "server": index,
                    "root": str(server.root),
                    "stderr": str(server.log_path),
                    "stderr_cut": server.output_cut,
                    "proxy_stderr": [str(path) for path in server.proxy_logs],
                    "records": str(server.records_path),
                    "window": str(server.window_path),
                    "window_since": since,
                    "window_lower_bound": bound,
                }
            )
        sys.stderr.flush()

    async def cases(self, directory: Path) -> None:
        self.root = directory / "workspace"
        self.pin.checkout(self.root)
        self.configure()
        if self.case == "stop":
            await self.stop_states()
            return
        if self.case == "churn":
            await self.churn_case()
            return
        await self.baseline()
        await self.symlink_root()
        if self.pin.name == "fastapi":
            await self.shallow(directory / "shallow")
        if self.pin.name == "nextjs":
            await self.source_bound()
        if self.pin.name == "bun":
            await self.lexical_bound()

    def configure(self, extra: str = "", root: Path | None = None) -> None:
        """Disable model downloads while preserving source, lexical, and dependency defaults."""
        (root or self.root).joinpath("rift.toml").write_text(
            CONFIGURATION + extra, encoding="utf-8"
        )

    async def baseline(self) -> None:
        with self.server() as server:
            async with server.connect() as client:
                answer = await settled_local(
                    client,
                    "search",
                    {
                        "query": "test",
                        "target": "symbol",
                        "limit": 1000,
                        "order": "identity",
                    },
                )
                candidates = objects(answer, "results")
                require(
                    len(candidates) >= SYMBOL_COUNT,
                    f"{self.pin.name}: fewer than {SYMBOL_COUNT} symbols",
                )
                self.record("publication", symbols=len(candidates))
                warnings(answer)
                await client.resource("rift://workspace")
                workspace_map = await client.resource("rift://map")
                found = await observed(
                    client,
                    "rift://logs/component/dependency",
                    lambda rows: any(
                        row.get("message") == CONTEXT_SPAN for row in rows
                    ),
                )
                self.dependencies(found)
                if self.pin.name == "fastapi":
                    packages = objects(workspace_map, "packages")
                    require(
                        any(package.get("manager") == "pypi" for package in packages),
                        "uv.lock produced no pypi packages",
                    )
                await self.symbols(client, candidates)
                await self.lexical_persistence(client)
                await self.oversized(client)
                await self.unparsed(client)
                if self.pin.name == "nextjs":
                    await self.symlinks(client)
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            self.stop(server)
            self.record(
                "stop", state="idle", process_gone=server.process.poll() is not None
            )

    def dependencies(self, found: list[JsonObject]) -> None:
        """Pin the resolver's visible manifest count separately from raw tree blobs."""
        count = self.visible_manifests()
        expected = (
            None
            if count <= 256
            else f"{count - 256} of {count} package.json manifests were not read: at most 256 are read per workspace"
        )
        exact_degradation(found, expected)
        passes = [row for row in found if row.get("message") == CONTEXT_SPAN]
        require(
            len(passes) == 1, f"startup read the dependency context {len(passes)} times"
        )
        require(
            fields(passes[0]).get("span") == "closed",
            "the dependency context span did not close",
        )
        require(
            {"entries", "degraded"}.issubset(fields(passes[0])),
            "the dependency context lost named fields",
        )
        if self.pin.name == "fastapi":
            degraded = [
                fields(row).get("reason")
                for row in found
                if row.get("message") == CONTEXT_DEGRADED
            ]
            require(
                degraded == [],
                f"the fastapi context reads manifests and lockfiles alone: {degraded}",
            )
        self.record(
            "manifests",
            raw=self.pin.measurement.package_json,
            visible=count,
            degradation=expected,
            resolvers=[] if expected is None else ["bun", "npm"],
        )

    def visible_manifests(self) -> int:
        """Git applies nested ignore files; Rift's hard floor excludes target directories."""
        listing = git(
            self.root, "ls-files", "--cached", "--others", "--exclude-standard", "-z"
        ).output_bytes()
        candidates = [
            path.decode()
            for path in listing.split(b"\0")
            if path and Path(path.decode()).name == "package.json"
        ]
        if not candidates:
            return 0
        # --cached includes tracked ignored paths. --no-index makes check-ignore inspect them.
        encoded = b"".join(path.encode() + b"\0" for path in candidates)
        ignored = set(
            git(
                self.root,
                "-c",
                "core.excludesFile=" + os.devnull,
                "check-ignore",
                "--no-index",
                "--stdin",
                "-z",
            )
            .with_input(encoded)
            .with_accepted(0, 1)
            .output_bytes()
            .split(b"\0")
        )
        return sum(
            path.encode() not in ignored
            and "target" not in Path(path).parts
            and not (self.root / path).is_symlink()
            for path in candidates
        )

    async def oversized(self, client: Client) -> None:
        """Search the pinned oversized file past its first chunk under the default `split`.

        The file is past `[search.text] max_chunk`, so the index holds its text as chunks.
        A token that first occurs past the first chunk answers only from a later one.
        """
        if not self.pin.oversized_path:
            return
        path = self.pin.oversized_path
        data = (self.root / path).read_bytes()
        require(
            len(data) == self.pin.oversized_bytes, f"{path}: pinned byte count changed"
        )
        token, offset = token_past_chunk(data)
        answer = await settled_pattern(
            client,
            {
                "pattern": token,
                "target": "file",
                "paths": {"include": [path]},
                "limit": 1,
            },
        )
        named = chunked_answer(answer, path, len(data), offset)
        require(not named, f"{path}: named by {named} although split holds it whole")
        self.record(
            "oversized", path=path, bytes=len(data), pattern=token, offset=offset
        )

    async def unparsed(self, client: Client) -> None:
        """Search the pinned file past `[providers.syntax] max_file` held as text.

        The provider refuses its source for its size alone, so under the default `split`
        its text answers search, every answer names it in `large_file_unparsed`, and the
        build that reads it records it as held unparsed rather than left out.
        """
        if not self.pin.unparsed_path:
            return
        path = self.pin.unparsed_path
        data = (self.root / path).read_bytes()
        require(
            len(data) == self.pin.unparsed_bytes, f"{path}: pinned byte count changed"
        )
        pattern, offset = last_line_pattern(data)
        answer = await settled_pattern(
            client,
            {
                "pattern": pattern,
                "target": "file",
                "paths": {"include": [path]},
                "limit": 1,
            },
        )
        named = chunked_answer(answer, path, len(data), offset)
        require(
            named == ["large_file_unparsed"],
            f"{path}: named by {named}, expected large_file_unparsed alone",
        )
        found = await observed(
            client,
            "rift://logs/component/index",
            lambda rows: bool(build_records(rows, path)),
        )
        # One record per build that read the file; a rebuild of the whole tree reads it again.
        messages = build_records(found, path)
        require(
            set(messages) == {HELD_UNPARSED_RECORD},
            f"{path}: build records {messages}, expected {HELD_UNPARSED_RECORD!r} alone",
        )
        self.record(
            "unparsed", path=path, bytes=len(data), pattern=pattern, offset=offset
        )

    async def symbols(self, client: Client, candidates: list[JsonObject]) -> None:
        """Resolve 200 sampled symbol identities through syntax reads."""
        initial = language_counts(candidates)
        pool = list(candidates)
        workspace = await client.resource("rift://workspace")
        configured = objects(workspace, "languages")
        for language in SAMPLE_LANGUAGES[self.pin.name]:
            matching = [row for row in configured if row.get("language") == language]
            require(len(matching) == 1, f"workspace lost language {language}")
            row = matching[0]
            require(
                row.get("enabled") is True and row.get("syntax") is True,
                f"workspace does not serve syntax for {language}",
            )
            included = array_value(row.get("include"), "language include patterns")
            require(bool(included), f"workspace lost include patterns for {language}")
            paths: JsonObject = {"include": included, "exclude": row.get("exclude", [])}
            answer = await client.call(
                "search",
                {
                    "query": "test",
                    "target": "symbol",
                    "paths": paths,
                    "limit": 1000,
                    "order": "identity",
                },
            )
            warnings(answer)
            pool.extend(objects(answer, "results"))
        selected = sample_symbols(
            pool, SYMBOL_COUNT, SEED, REQUIRED_LANGUAGES[self.pin.name]
        )
        details: list[Json] = []
        for hit in selected:
            symbol = object_value(
                object_value(hit.get("hit"), "search hit").get("symbol"), "symbol"
            )
            details.append(
                {
                    "identity": symbol.get("id"),
                    "language": symbol.get("language"),
                    "path": hit.get("path"),
                    "range": hit.get("range"),
                }
            )
        self.record(
            "identity_languages",
            initial=initial,
            pool=language_counts(pool),
            sample=language_counts(selected),
            selected=details,
        )
        identities: set[str] = set()
        for hit in selected:
            symbol = object_value(
                object_value(hit.get("hit"), "search hit").get("symbol"), "symbol"
            )
            identity = string_value(symbol.get("id"), "symbol identity")
            require(
                identity not in identities, f"duplicate sampled identity: {identity}"
            )
            identities.add(identity)
            path = self.root / string_value(hit.get("path"), "symbol path")
            span = object_value(hit.get("range"), "symbol range")
            start, end = (
                number(span.get("start"), "range start"),
                number(span.get("end"), "range end"),
            )
            content = path.read_bytes()
            require(0 <= start < end <= len(content), "symbol range exceeds source")
            # Declarations can start with attached comments or attributes. Extraction
            # keeps their end equal to the item end, so its final byte is in the item.
            answer = await client.call(
                "nodes", {"path": str(path.relative_to(self.root)), "position": end - 1}
            )
            nodes = objects(answer, "nodes")
            require(
                any(node.get("symbol") == identity for node in nodes),
                f"nodes omitted sampled declaration: {identity} at {end - 1}; "
                f"returned {[(node.get('kind'), node.get('range'), node.get('symbol')) for node in nodes]}",
            )
            require(path.read_bytes() == content, f"read changed {path}")
        self.record("identities", count=len(identities), seed=SEED)

    async def lexical_persistence(self, client: Client) -> None:
        """Preserve unrelated lexical rows after external source creation and removal."""
        before = lexical_content(self.root)
        sizes = database_bytes(self.root)
        path = self.root / PROBE_PATH
        path.write_bytes(PROBE_SOURCE.encode())
        answer = await client.call("get_symbol", {"name": "corpus_probe"})
        hits = objects(answer, "hits")
        require(
            len(hits) == 1 and hits[0].get("source") == PROBE_SOURCE.rstrip("\n"),
            f"new source was absent from reads: {answer}",
        )
        await self.await_probe(True)
        require(
            lexical_content(self.root) == before,
            "source creation changed unrelated persisted lexical rows",
        )
        require(
            path.read_bytes() == PROBE_SOURCE.encode(),
            "source read changed probe bytes",
        )
        self.record(
            "lexical_persistence", before=sizes, after=database_bytes(self.root)
        )
        path.unlink()
        answer = await client.call("get_symbol", {"name": "corpus_probe"})
        require(
            not objects(answer, "hits"), f"removed source remained indexed: {answer}"
        )
        await self.await_probe(False)
        require(
            lexical_content(self.root) == before,
            "source removal changed unrelated persisted lexical rows",
        )

    async def await_probe(self, present: bool) -> None:
        """Wait for the lexical publication to reach `present` under one budget.

        The deadline drives the loop on its own. A separate iteration count of
        OBSERVATION_SECONDS / POLL_SECONDS is the same budget spelled twice, and the
        loop could never reach its last iteration inside it, so the count decided
        nothing and the failure it raised was unreachable.
        """
        deadline = time.monotonic() + OBSERVATION_SECONDS
        while time.monotonic() < deadline:
            if (probe_units(self.root) > 0) == present:
                return
            await asyncio.sleep(POLL_SECONDS)
        raise AssertionError(
            f"lexical probe publication never reached present={present} "
            f"within {OBSERVATION_SECONDS}s"
        )

    async def symlinks(self, client: Client) -> None:
        workspace = map_paths(await client.resource("rift://map"))
        control = await client.call(
            "search",
            {
                "query": "next",
                "paths": {"include": ["package.json"], "exclude": ["*/**"]},
                "limit": 100,
            },
        )
        control_hits = objects(control, "results")
        require(
            bool(control_hits)
            and all(hit.get("path") == "package.json" for hit in control_hits),
            "root-only selector lost the regular package.json control",
        )
        chain = "test/development/app-dir/hmr-symlink/app/symlink-chain/page.tsx"
        chain_path = self.root / chain
        require(
            (chain_path.parent / chain_path.readlink()).is_symlink(),
            "nextjs pinned chain must point to another symlink",
        )
        for path in ("readme.md", chain):
            await self.symlink_refused(client, path, workspace)
        self.record(
            "symlinks",
            nodes="resource_not_found",
            search=0,
            paths=["readme.md", chain],
            control="package.json",
            control_hits=len(control_hits),
        )

    async def symlink_refused(
        self, client: Client, path: str, workspace: set[str]
    ) -> None:
        require((self.root / path).is_symlink(), f"{path}: pinned symlink is absent")
        try:
            await client.call("nodes", {"path": path, "position": 0})
        except ToolFailure as error:
            require(
                error.code == "resource_not_found",
                f"symlink nodes wrong refusal: {error}",
            )
        else:
            raise AssertionError("nodes accepted a symlink")
        selector: JsonObject = {"include": [path]}
        if "/" not in path:
            selector["exclude"] = ["*/**"]
        answer = await client.call("search", {"query": "next", "paths": selector})
        require(
            not objects(answer, "results"),
            f"search indexed symlink {path}: {answer}",
        )
        require(path not in workspace, "workspace map lists a symlink")

    async def churn_case(self) -> None:
        """Run sustained reads and edits in their own required bounded case."""
        with self.server() as server:
            async with server.connect() as client:
                # Listener readiness precedes file preparation. Observe the existing
                # startup budget before measuring reads against external writes.
                await settled_local(client, "get_symbol", CHURN_REQUESTS["get_symbol"])
                await self.churn(client)
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            self.stop(server)
            self.record("stop", state="after_churn", process_gone=True)

    async def churn(self, client: Client) -> None:
        """Read complete source revisions while external writes continue every two seconds."""
        path = self.root / PROBE_PATH
        sources: list[str] = []
        written: list[float] = []
        timings: dict[str, list[float]] = {name: [] for name in CHURN_REQUESTS}
        overlaps = 0
        converging = False

        def write(edit: int) -> None:
            source = f"pub fn corpus_probe() {{ let value = {edit}; }}\n"
            path.write_text(source, encoding="utf-8")
            sources.append(source.rstrip("\n"))
            written.append(time.monotonic())

        async def writer() -> None:
            for edit in range(1, int(self.work_seconds()) // 2):
                await asyncio.sleep(2.0)
                write(edit)

        async def read(name: str, identity: str | None) -> tuple[str, int]:
            nonlocal overlaps
            before = len(sources)
            # A tool's first read waits for the index this workspace is still building;
            # every read after it answers from a published one.
            first = not timings[name]
            budget = READ_SECONDS if first else STEADY_READ_SECONDS
            started = time.monotonic()
            codes: list[str] = []
            revision: int | None = None
            try:
                async with gate_deadline(f"churn {name}", budget):
                    answer = await read_settled_local(
                        client,
                        name,
                        CHURN_REQUESTS[name],
                        seconds=budget,
                        poll_seconds=POLL_SECONDS,
                    )
                codes = [
                    string_value(warning.get("code"), "warning code")
                    for warning in warnings(answer)
                ]
                found, source = churn_answer(name, answer, sources, identity)
                revision = sources.index(source)
                require(
                    revision >= before - 1 or "stale_index" in codes,
                    f"{name} returned old source without stale_index",
                )
                return found, revision
            finally:
                elapsed = time.monotonic() - started
                timings[name].append(elapsed)
                changed = len(sources) - before
                overlaps += changed
                self.record(
                    "churn_read",
                    tool=name,
                    seconds=elapsed,
                    deadline_seconds=budget,
                    first=first,
                    writes=changed,
                    source_revision=revision,
                    source_revision_at_start=before - 1,
                    # The age of the newest write when the read began, and the loop the
                    # read ran in: a read that outlasts its deadline leaves no answer, so
                    # these say which write the server still owed a publication for.
                    seconds_since_write=started - written[before - 1],
                    convergence=converging,
                    warning_codes=codes,
                )

        write(0)
        task: asyncio.Task[None] | None = None
        try:
            identity, _revision = await read("get_symbol", None)
            task = asyncio.create_task(writer())
            reads = 0
            tools = tuple(CHURN_REQUESTS)
            pressure_calls = {name: 0 for name in tools}
            async with gate_deadline(
                "corpus churn", self.work_seconds() - CONVERGENCE_SECONDS
            ):
                while reads < READ_COUNT or overlaps < 2:
                    name = tools[reads % len(tools)]
                    await read(name, identity)
                    pressure_calls[name] += 1
                    reads += 1
                    if task.done():
                        await task
                        raise AssertionError("churn writer ended before reads")
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
            final = [sources[-1]]
            converging = True
            convergence = time.monotonic()
            async with gate_deadline("churn final source", CONVERGENCE_SECONDS):
                for name in CHURN_REQUESTS:
                    while True:
                        _identity, revision = await read(name, identity)
                        if revision == len(sources) - 1:
                            break
                        # read already refused every older answer without stale_index.
                        await asyncio.sleep(POLL_SECONDS)
            self.record(
                "churn_convergence",
                seconds=time.monotonic() - convergence,
                source_revision=len(sources) - 1,
            )
            require(
                path.read_text(encoding="utf-8").rstrip("\n") == final[0],
                "reads changed probe source",
            )
            self.record(
                "churn",
                reads=reads,
                pressure_calls=pressure_calls,
                edits=len(sources),
                overlapping_writes=overlaps,
                read_seconds_max=READ_SECONDS,
                steady_read_seconds_max=STEADY_READ_SECONDS,
                latency={
                    name: {
                        "calls": len(values),
                        "first_seconds": values[0],
                        "maximum_seconds": max(values),
                    }
                    for name, values in timings.items()
                },
                final_source=final[0],
                temporarily_unavailable=0,
            )
        finally:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            path.unlink(missing_ok=True)

    async def symlink_root(self) -> None:
        link = self.root.parent / "linked-workspace"
        link.symlink_to(self.root, target_is_directory=True)
        with self.server(link) as server:
            async with server.connect() as client:
                answer = await settled_local(
                    client, "search", {"query": "test", "limit": 1}
                )
                require(
                    bool(objects(answer, "results")),
                    "symlink root has no search results",
                )
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            self.stop(server)
        self.record("symlink_root", reads=True)

    async def shallow(self, root: Path) -> None:
        self.pin.checkout(root, depth=1)
        self.configure(root=root)
        with self.server(root) as server:
            async with server.connect() as client:
                found = await settled_local(
                    client,
                    "get_symbol",
                    {"name": "FastAPI", "include": ["history"], "limit": 5},
                )
                hits = objects(found, "hits")
                require(bool(hits), "shallow checkout has no FastAPI declaration")
                for hit in hits:
                    history = object_value(hit.get("history"), "symbol history")
                    require(
                        history.get("complete") is False,
                        "shallow history must report complete=false",
                    )
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            self.stop(server)
        self.record("shallow_history", complete=False, depth=1)

    async def source_bound(self) -> None:
        # https://github.com/volarized/rift/issues/495
        self.configure("[source]\nfiles = 20000\n")
        server = self.server()
        try:
            async with gate_deadline("source.files refusal", 180.0):
                server.start()
                async with server.connect() as client:
                    while True:
                        try:
                            answer = await client.call(
                                "get_symbol", {"name": "corpus_probe"}
                            )
                        except ToolFailure as error:
                            require(
                                error.code == "limit_exceeded"
                                and error.retry == "never",
                                f"source bound wrong refusal: {error}",
                            )
                            message = error.message
                            prefix = (
                                "workspace contains more files than its accepted limit of 20000: "
                                "field source.files, observed 20001, path "
                            )
                            action = "; reduce workspace files below 20000 and retry"
                            require(
                                message.startswith(prefix) and message.endswith(action),
                                f"source bound wrong message: {message}",
                            )
                            path = message[len(prefix) : -len(action)]
                            require(bool(path.strip()), "source bound omitted its path")
                            reported = Path(path).resolve()
                            root = self.root.resolve()
                            require(
                                reported != root and reported.is_relative_to(root),
                                f"source bound path is outside the workspace: {path}",
                            )
                            require(
                                error.limit
                                == FailureLimit("source.files", 20001, 20000),
                                f"source bound wrong limit: {error}",
                            )
                            require(
                                error.causes == [],
                                f"direct source bound has unexpected causes: {error.causes}",
                            )
                            break
                        require(
                            any(
                                warning.get("code") == "local_index_preparing"
                                for warning in warnings(answer)
                            ),
                            f"source.files overflow returned a complete read: {answer}",
                        )
                        await asyncio.sleep(POLL_SECONDS)
                    await observed(
                        client,
                        "rift://logs/component/index",
                        lambda rows: any(
                            row.get("message") == "index rebuild failed"
                            and fields(row).get("error_code") == "limit_exceeded"
                            for row in rows
                        ),
                    )
                server.check_running()
                self.stop(server)
                self.record(
                    "source_bound", field="source.files", observed=20001, maximum=20000
                )
        finally:
            server.close()
            self.configure()

    async def lexical_bound(self) -> None:
        self.configure("[search.lexical]\nunits_max = 20000\n")
        with self.server() as server:
            async with server.connect() as client:
                await observed(
                    client,
                    "rift://logs/component/search",
                    lambda rows: any(
                        row.get("message")
                        == "the lexical commit failed; the next publication compares every "
                        "file with the digests the store recorded"
                        for row in rows
                    ),
                )
                answer = await client.call("search", {"query": "test", "limit": 1})
                count = lexical_breach(answer, 20000)
                require(
                    bool(objects(answer, "results")),
                    "lexical overflow removed identifier matches",
                )
                require(
                    bool(
                        objects(
                            await client.call(
                                "get_symbol", {"name": "test", "limit": 1}
                            ),
                            "hits",
                        )
                    ),
                    "lexical overflow removed symbol reads",
                )
            self.stop(server)
        self.configure()
        self.record(
            "lexical_bound",
            warning="lexical_ranking_unavailable",
            observed=count,
            maximum=20000,
        )

    async def stop_states(self) -> None:
        with self.server() as server:
            async with server.connect() as client:
                await client.call("search", {"query": "test", "limit": 1})
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            self.stop(server)
        self.record("stop", state="idle", process_gone=True)
        await self.stop_during_rebuild()
        await self.stop_during_history_fill()

    async def stop_during_rebuild(self) -> None:
        """Observe filesystem rebuild output without a proxy that restarts a stopped server."""
        with self.server() as server:
            startup = await observed_state(
                server, 0, STARTUP_PUBLICATION, startup_published
            )
            (self.root / PROBE_PATH).write_text(PROBE_SOURCE)
            output = await observed_output(
                server, len(startup), "index capture started"
            )
            await self.stop_observed(server, "rebuild", output)
        (self.root / PROBE_PATH).unlink(missing_ok=True)

    async def stop_during_history_fill(self) -> None:
        """Stop while a history store batch with pending commits has not finished.

        A fill analyzes only the commits the history store lacks, so the case first
        deletes the store the earlier servers filled, in the `.rift` folder of the common
        git directory: this server's fill then owes every commit its plan selects. The
        fill starts independently of source preparation with no request. The owned
        server document establishes listener readiness; observe the fill from the start
        of this server's output because it can finish before startup publication.
        """
        common = git(self.root, "rev-parse", "--git-common-dir").output().strip()
        store = self.root / common / ".rift"
        if store.exists():
            shutil.rmtree(store)
        with self.server() as server:
            output = await observed_state(
                server,
                0,
                "a history store batch with pending commits",
                lambda text: open_history_batch(text) is not None,
            )
            await self.stop_observed(server, "history", output)

    async def stop_observed(self, server: Server, operation: str, output: str) -> None:
        """Stop once `output` shows the operation started and not finished."""
        evidence = active_stdout(output, operation, None)
        await asyncio.to_thread(self.stop, server)
        self.record(
            "stop", state=f"mid_{operation}", stderr=evidence, process_gone=True
        )


async def observed_output(server: Server, offset: int, message: str) -> str:
    """Read owned output without waiting for the log store's database write turn."""
    return await observed_state(server, offset, message, lambda text: message in text)


async def observed_state(
    server: Server, offset: int, wanted: str, holds: Callable[[str], bool]
) -> str:
    """Read owned output until its complete records satisfy `holds`.

    The deadline alone bounds the loop, as in `Corpus.await_probe`, so a breach fails
    naming what the case waited for.
    """
    deadline = time.monotonic() + OBSERVATION_SECONDS
    while time.monotonic() < deadline:
        server.check_running()
        output = server.read_log()[offset:]
        if output.endswith("\n") and holds(output):
            return output
        await asyncio.sleep(POLL_SECONDS)
    raise AssertionError(
        f"required record never reached server output within "
        f"{OBSERVATION_SECONDS}s: {wanted}"
    )


async def settled_local(client: Client, name: str, request: JsonObject) -> JsonObject:
    """Resend partial local reads until local index preparation completes.

    The wait is bounded by `LOCAL_PREPARATION_SECONDS`.
    """
    return await read_settled_local(
        client,
        name,
        request,
        seconds=LOCAL_PREPARATION_SECONDS,
        poll_seconds=POLL_SECONDS,
    )


async def settled_pattern(client: Client, request: JsonObject) -> JsonObject:
    """Resend a `pattern` search until the trigram index covers every stored row.

    An answer warning `pattern_index_preparing` covers only the rows the trigram index
    holds, and the warning asks the caller to resend once the index catches up. The
    deadline alone bounds the loop, as in `Corpus.await_probe`.
    """
    deadline = time.monotonic() + OBSERVATION_SECONDS
    while time.monotonic() < deadline:
        answer = await client.call("search", request)
        preparing = [
            warning
            for warning in warnings(answer)
            if warning.get("code") == "pattern_index_preparing"
        ]
        if not preparing:
            return answer
        await asyncio.sleep(POLL_SECONDS)
    raise AssertionError(
        f"the trigram index never covered every stored row within {OBSERVATION_SECONDS}s"
    )


def objects(answer: JsonObject, key: str) -> list[JsonObject]:
    return [object_value(value, key) for value in array_value(answer.get(key), key)]


async def observed(
    client: Client, uri: str, predicate: Callable[[list[JsonObject]], bool]
) -> list[JsonObject]:
    """Wait for the log drain under one deadline, including resource calls and polls."""
    async with asyncio.timeout(OBSERVATION_SECONDS):
        while True:
            found = records(await client.resource(uri))
            if predicate(found):
                return found
            await asyncio.sleep(POLL_SECONDS)
