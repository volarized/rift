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
from collections.abc import Callable, Iterator, Sequence
from datetime import UTC, datetime
from itertools import batched
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
    chunked_warnings,
    churn_answer,
    database_bytes,
    declaration_pool,
    exact_degradation,
    language_counts,
    last_line_pattern,
    lexical_breach,
    lexical_content,
    lists_position,
    logged_read_warnings,
    map_paths,
    no_failed_builds,
    number,
    open_history_batch,
    probe_units,
    records,
    sample_symbols,
    startup_published,
    stop_sizes,
    symbol_pool,
    token_past_chunk,
    unit_path,
    warnings,
)
from rift_dev.corpus_cache import Pin, git
from rift_dev.local_index_read import settled_local as read_settled_local
from rift_dev.log_records import (
    DATABASE_CLOSE,
    STAGE_ENDED,
    Line,
    closes_span,
    instant,
    stop_measurements,
)
from rift_dev.machine import machine, machine_line
from rift_dev.nextest_run import CollectionSummary, retained_collector
from rift_dev.query import (
    bindings,
    iri,
    literal,
    optional_iri,
    retrieval_batch_request,
    retrieval_request,
)
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
from rift_dev.trace import (
    TEST_CASE_KEY,
    Collector,
    resource_attribute,
)

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
# Pages one sampled symbol read may follow before its declaration must have appeared.
NODE_PAGES_MAX = 50
READ_SECONDS = READINESS_SECONDS + 10.0
# A tool's first read over a large workspace under sustained edits waits for the first
# index to land, so it gets the whole read budget. Every read after it answers from a
# published index: run 35661449692 over `nextjs` measured 53 such reads at a median of
# 1.56 seconds, one outlier at 7.24 waiting out a rebuild, and everything else under 3.34,
# against a first `search` that spent the server's whole budget. The steady budget stands
# well above that outlier and strictly inside the readiness budget, so a steady read that
# starts waiting for the index fails the case instead of hiding in the first read's room.
STEADY_READ_SECONDS = READINESS_SECONDS * 2 / 3
# One `settled_local` call resends a read until the index is ready, so it spans the
# startup snapshot's publication, the wait for the next poll, the
# server's answer, and the client's own handling of that answer. Over 17 passing runs of
# the `nextjs` workspace case the interval from the first `search` to the return from
# `settled_local` was 44.79 to 59.12 seconds: publication of the startup snapshot 41.68 to
# 52.72 seconds after the first call, up to 3.36 seconds until the next poll, a server
# answer of 1.35 to 1.63 seconds, and 1.16 to 2.34 seconds of client handling. A bound of
# 60 seconds left 0.88 seconds at the closest, and job 111716521102 crossed it by at most
# 0.53 seconds with the server's answer already sent at 58.47 seconds. The bound is twice
# the longest passing interval, 59.12 seconds, rounded up. Callers are the `workspace`
# case of `bun`, `nextjs`, and `fastapi` (the `shallow` read only for `fastapi`) and the
# `nextjs` `churn` case, with work budgets of 570, 750, 690, and 210 seconds for
# Bun workspace, Next.js workspace, Next.js churn, and FastAPI, respectively.
# The bound stands inside the smallest, `fastapi` at 210. A case where every read
# ran to this bound would reach its work budget first and fail there, naming the action.
LOCAL_PREPARATION_SECONDS = 120.0
# Seconds a case keeps inside its own deadline for the served tree's removal,
# the report write, and the process exit. Nextest allows the same grace after
# it ends a corpus case, so the two bounds agree on what cleanup costs.
CLEANUP_RESERVE_SECONDS = 30.0
# The Bun and Next.js workspace cases measure the full declaration sample and two
# further cold starts. Retained Linux timings put Next.js at up to 688 seconds and
# Bun at 463 seconds before its final cold start and checks, which took 64 seconds
# on the preceding green run. Keep one minute in the matching Nextest overrides.
WORKSPACE_ALLOWANCE_SECONDS = 60
SEED = 34
POLL_SECONDS = 0.1
OBSERVATION_SECONDS = 60.0
# `SYNTAX_FILE_BYTES_DEFAULT`; the corpus preserves the syntax provider's defaults.
SYNTAX_FILE_BYTES = 4 << 20
STARTUP_PUBLICATION = "the startup index publication"
CONFIGURATION = (
    f'[server]\nreadiness_timeout = "{int(READINESS_SECONDS)}s"\n'
    "[search.vector]\ndisabled = true\n"
    "[logs]\n"
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
CORPUS_COLUMNS = ("symbol", "name", "kind", "language", "path", "unit", "start", "end")
PATH_HIT_LIMIT = 1000


def catalog_paths(workspace: JsonObject) -> dict[str, list[str]]:
    """Select catalog paths that are literal under the existing glob selector.

    This is a source sample, not a complete listing of the workspace. Paths with
    glob syntax remain covered by the independent path and declaration gates.

    >>> catalog_paths({"source": [{"path": "src/test.rs", "digest": "0123abcd", "language": "rust"}, {"path": "app/[slug]/page.tsx", "digest": "0123abcd", "language": "typescript:tsx"}]})
    {'rust': ['src/test.rs']}
    >>> catalog_paths({"source": [{"path": "README.md", "digest": "0123abcd"}]})
    {}
    """
    paths: dict[str, list[str]] = {}
    for source in objects(workspace, "source"):
        path = string_value(source.get("path"), "catalog source path")
        string_value(source.get("digest"), "catalog source digest")
        language = source.get("language")
        if language is None:
            continue
        language = string_value(language, "catalog source language")
        if path.isascii() and all(c.isalnum() or c in "/_.-" for c in path):
            paths.setdefault(language, []).append(path)
    return paths


def corpus_request(parameters: JsonObject) -> JsonObject:
    """Project corpus facts through the served SPARQL retrieval service."""
    columns = list(CORPUS_COLUMNS)
    if parameters.get("target") == "file":
        columns.extend(("target", "size"))
    for include in array_value(parameters.get("include", []), "retrieval include"):
        field = string_value(include, "retrieval field")
        if field in {"source", "score"}:
            columns.append(field)
    return retrieval_request(parameters, columns)


def sample_path_request(path: str, query: str = "test") -> JsonObject:
    return {
        "query": query,
        "target": "symbol",
        "paths": {"include": [path]},
        "limit": PATH_HIT_LIMIT,
        "order": "identity",
    }


async def read_sample_paths(
    client: Client, paths: Sequence[str]
) -> list[list[JsonObject]]:
    """Read each exact path under the original preparation deadline.

    >>> from unittest.mock import AsyncMock, patch
    >>> from typing import cast
    >>> async def split_sample():
    ...     failure = ToolFailure("search", "", "limit_exceeded", "", "never", identity="rift.search.query_deadline_exceeded")
    ...     with patch(__name__ + "._read_sample_query", new_callable=AsyncMock) as query:
    ...         query.side_effect = [failure, [[{"path": "a.ts"}]], [[{"path": "b.ts"}]]]
    ...         grouped = await read_sample_paths(cast(Client, object()), ["a.ts", "b.ts"])
    ...         return grouped, [call.args[1] for call in query.await_args_list]
    >>> asyncio.run(split_sample())
    ([[{'path': 'a.ts'}], [{'path': 'b.ts'}]], [['a.ts', 'b.ts'], ['a.ts'], ['b.ts']])
    """

    async def read(paths: Sequence[str]) -> list[list[JsonObject]]:
        try:
            return await _read_sample_query(client, paths)
        except ToolFailure as failure:
            if (
                len(paths) == 1
                or failure.identity != "rift.search.query_deadline_exceeded"
            ):
                raise
        middle = len(paths) // 2
        return await read(paths[:middle]) + await read(paths[middle:])

    async with gate_deadline("sample paths", LOCAL_PREPARATION_SECONDS):
        return await read(paths)


async def _read_sample_query(
    client: Client, paths: Sequence[str]
) -> list[list[JsonObject]]:
    """Read independently ranked exact paths through one retained graph query."""
    requests = [sample_path_request(path) for path in paths]
    multiple = len(paths) > 1
    request = (
        retrieval_batch_request(requests, CORPUS_COLUMNS)
        if multiple
        else corpus_request(requests[0])
    )
    answer = await settled_local(client, "search", request)
    warnings(answer)
    rows = bindings(answer)
    view = answer.get("view")
    pagination = object_value(answer.get("pagination"), "sample pagination")
    count = number(pagination.get("total_pages"), "sample page count")
    require(
        0 <= count <= len(paths) * PATH_HIT_LIMIT, "sample page count exceeds hit bound"
    )
    grouped: list[list[JsonObject]] = [[] for _ in paths]
    previous = -1
    for page in range(max(count, 1)):
        if page:
            next_query = string_value(
                pagination.get("next_query"), "sample continuation"
            )
            answer = await client.call("search", {"query": next_query})
            warnings(answer)
            rows = bindings(answer)
            require(
                view is not None and answer.get("view") == view, "sample view changed"
            )
            pagination = object_value(answer.get("pagination"), "sample pagination")
        require(
            pagination.get("page_index") == page
            and pagination.get("total_pages") == count,
            "sample pagination changed",
        )
        require(count > 0 or not rows, "sample returned rows without pages")
        for row in rows:
            slot = int(literal(row, "slot")) if multiple else 0
            require(
                0 <= slot < len(paths) and slot >= previous, "sample slot out of order"
            )
            require(
                literal(row, "path") == paths[slot],
                "literal sample path matched another file",
            )
            require(
                len(grouped[slot]) < PATH_HIT_LIMIT, "sample slot exceeds hit bound"
            )
            grouped[slot].append(
                {key: value for key, value in row.items() if key != "slot"}
            )
            previous = slot
    require(
        pagination.get("next_query") is None, "sample returned an unadvertised page"
    )
    return grouped


def churn_source(edit: int) -> str:
    return f"pub fn corpus_probe() {{ let value = {edit}; }}\n"


CHURN_PROBE_ID = "rift://symbol/local/rust/rift_corpus_probe/corpus_probe"
CHURN_REQUESTS: dict[str, JsonObject] = {
    "search": {
        "query": f"SELECT (<{CHURN_PROBE_ID}> AS ?symbol) ?unit ?source ?start ?end "
        f"{{ ?node :of <{CHURN_PROBE_ID}>; :index 0; :source ?source; "
        ":range [ :unit ?unit; :start ?start; :end ?end ] }"
    },
    "symbol_nodes": {"symbol": CHURN_PROBE_ID},
    "nodes": {"path": PROBE_PATH, "position": 0},
}
# The tool each churn read calls: `symbol_nodes` reads the probe declaration's nodes by its
# symbol identity, the way an agent reads a declaration's source.
CHURN_TOOLS = {"symbol_nodes": "nodes"}
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
                # The server uses the same enabled SDK policy as its parent.
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

        Nextest ends the case at its configured deadline, so the actions stop before
        it: the reserve is the room the served tree's removal, the report write,
        and the process exit need inside that same deadline. A case that crosses
        this bound fails naming the action it was running, instead of being
        killed with no evidence of where it stood.
        """
        deadline = self.pin.seconds
        if self.pin.name in {"bun", "nextjs"} and self.case == "workspace":
            deadline += WORKSPACE_ALLOWANCE_SECONDS
        return max(deadline - CLEANUP_RESERVE_SECONDS, 1.0)

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
        work_timeout = asyncio.timeout(budget)
        try:
            async with work_timeout:
                with (
                    retained_collector(
                        self.report.parent / f"{self.report.stem}.telemetry"
                    ) as telemetry,
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
            # Issue #578: the FastAPI action deadline expired entering shallow history.
            # Preserve the traceback and artifacts; other errors remain failures.
            if (
                self.pin.name == "fastapi"
                and self.case == "workspace"
                and self.last_action == "symlink_root"
                and type(error) is TimeoutError
                and work_timeout.expired()
            ):
                status = "xfail"
                print(
                    "XFAIL https://github.com/volarized/rift/issues/578: "
                    "FastAPI action deadline expired after symlink_root",
                    file=sys.stderr,
                    flush=True,
                )
            else:
                raise
        finally:
            try:
                telemetry_artifact = self.write_telemetry_artifact()
            except (OSError, TypeError, ValueError) as error:
                telemetry_artifact = {
                    "path": str(
                        self.report.parent
                        / f"{self.report.stem}.telemetry"
                        / "telemetry.jsonl.gz"
                    ),
                    "error": str(error),
                }
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
                        "telemetry_artifact": telemetry_artifact,
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

    def write_telemetry_artifact(self) -> JsonObject | None:
        """Read final collection accounting after bounded collector shutdown."""
        if self.telemetry is None:
            return None
        directory = self.report.parent / f"{self.report.stem}.telemetry"
        summary = CollectionSummary.model_validate_json(
            (directory / "telemetry-summary.json").read_bytes()
        )
        return {
            "path": str(directory / "telemetry.jsonl.gz"),
            "summary": str(directory / "telemetry-summary.json"),
            "received": summary.received,
            "retained": summary.retained,
            "omitted": summary.omitted,
            "received_bytes": summary.received_bytes,
            "errors": list(summary.errors),
            "collector_dropped": dict(summary.cache_evictions.counts()),
        }

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

    async def source_catalog(
        self, client: Client
    ) -> tuple[JsonObject, dict[str, list[str]]]:
        """Read typed source paths through the existing resource page contract."""
        first = await client.resource("rift://workspace")
        revision = string_value(
            first.get("configuration_revision"), "configuration revision"
        )
        pagination = object_value(first.get("pagination"), "catalog pagination")
        pages = number(pagination.get("total_pages"), "catalog total pages")
        groups: dict[str, list[str]] = {}
        previous = ""
        for index in range(pages):
            page = (
                first
                if index == 0
                else await client.resource(f"rift://workspace?page_index={index}")
            )
            require(
                page.get("configuration_revision") == revision,
                "catalog configuration changed",
            )
            require(
                page.get("languages") == first.get("languages"),
                "catalog languages changed",
            )
            position = object_value(page.get("pagination"), "catalog pagination")
            require(
                position.get("page_index") == index
                and position.get("total_pages") == pages,
                "catalog page changed",
            )
            require(not warnings(page), "source catalog is still preparing")
            for source in objects(page, "source"):
                path = string_value(source.get("path"), "catalog source path")
                require(path > previous, "catalog paths must be unique and sorted")
                previous = path
            for language, paths in catalog_paths(page).items():
                groups.setdefault(language, []).extend(paths)
        return first, groups

    async def symbol_candidates(
        self, client: Client, *, minimum: int = SYMBOL_COUNT
    ) -> list[JsonObject]:
        """Collect complete exact-path results for the existing physical sample.

        The callback boundary covers a first catalog page with no sampled language
        and final exhaustion of an optional language. It also checks continuation
        of one batched query. The owning corpus run checks actual declarations.

        >>> from types import SimpleNamespace
        >>> from typing import cast
        >>> from unittest.mock import AsyncMock
        >>> def term(value, datatype="string"):
        ...     return {"type": "literal", "value": str(value), "datatype": "http://www.w3.org/2001/XMLSchema#" + datatype}
        >>> rows = [{**{key: term(value) for key, value in {"name": f"entry{index}", "kind": "function", "language": "typescript", "path": "src/test.ts"}.items()}, "start": term(index * 2, "integer"), "end": term(index * 2 + 1, "integer"), "symbol": {"type": "iri", "value": f"urn:fixture:sample:{index}"}} for index in range(SYMBOL_COUNT)]
        >>> def answer(bindings, index, **continuation):
        ...     return {"answer": {"result": "bindings", "variables": ["slot", *rows[0]], "bindings": [{**row, "slot": term(0, "integer")} for row in bindings]}, "view": {"id": "retained"}, "pagination": {"page_index": index, "total_pages": 2, **continuation}}
        >>> languages = [{"language": language, "enabled": True, "syntax": True, "include": ["**"]} for language in ("rust", "typescript", "typescript:tsx", "markdown")]
        >>> common = {"configuration_revision": "0123abcd", "languages": languages}
        >>> first = {**common, "pagination": {"page_index": 0, "total_pages": 2}, "source": [{"path": f"docs/a{index:04}.md", "digest": "0123abcd", "language": "markdown"} for index in range(1000)]}
        >>> second = {**common, "pagination": {"page_index": 1, "total_pages": 2}, "source": [{"path": path, "digest": "0123abcd", "language": language} for path, language in [("src/test.ts", "typescript"), ("src/ztest.tsx", "typescript:tsx")]]}
        >>> client = SimpleNamespace(resource=AsyncMock(side_effect=[first, second]), call=AsyncMock(side_effect=[answer(rows[:100], 0, next_query="retained next page"), answer(rows[100:], 1)]))
        >>> sample = Corpus(cast(Pin, SimpleNamespace(name="nextjs")), Path("rift"), Path("report"))
        >>> selected = asyncio.run(sample.symbol_candidates(cast(Client, client)))
        >>> len(selected), client.call.await_count, client.resource.await_count
        (200, 2, 2)
        >>> all(literal(row, "path") == "src/test.ts" for row in selected)
        True
        >>> client.call.await_args_list[0].args[1]["query"].count("SERVICE :search")
        2
        >>> client.call.await_args_list[1].args[1] == {"query": "retained next page"}
        True
        """
        languages = SAMPLE_LANGUAGES[self.pin.name]

        async def read(path: str, query: str = "test") -> list[JsonObject]:
            answer = await settled_local(
                client,
                "search",
                corpus_request(sample_path_request(path, query)),
            )
            warnings(answer)
            found = bindings(answer)
            require(
                all(literal(hit, "path") == path for hit in found),
                "literal sample path matched another file",
            )
            return found

        pool: list[JsonObject] = []
        represented: set[str] = set()
        workspace, groups = await self.source_catalog(client)
        configured = objects(workspace, "languages")
        for language in languages:
            matching = [row for row in configured if row.get("language") == language]
            require(len(matching) == 1, f"workspace lost language {language}")
            row = matching[0]
            require(
                row.get("enabled") is True and row.get("syntax") is True,
                f"workspace does not serve syntax for {language}",
            )
            require(
                bool(array_value(row.get("include"), "language include patterns")),
                f"workspace lost include patterns for {language}",
            )
        anchor = (
            {
                "bun": "rift://symbol/local/rust/bun_bundler/defines/TEST",
                "fastapi": "rift://symbol/local/python/fastapi/routing/APIRouter",
            }.get(self.pin.name)
            if minimum == SYMBOL_COUNT
            else None
        )
        if anchor is not None:
            detail = await client.call("get_symbol", {"id": anchor})
            require(
                detail.get("outcome") == "found"
                and object_value(detail.get("symbol"), "anchor symbol").get("id")
                == anchor,
                "canonical sample anchor was not found",
            )
            paths = {
                path for language in languages for path in groups.get(language, [])
            }
            for declaration in objects(detail, "declarations"):
                path = string_value(declaration.get("path"), "anchor declaration path")
                require(
                    path in paths, "anchor declaration is outside literal catalog paths"
                )
                added = await read(path, anchor.rsplit("/", 1)[1])
                pool.extend(added)
                if added:
                    represented.update(
                        language
                        for language in languages
                        if path in groups.get(language, [])
                    )
            require(
                anchor in symbol_pool(pool),
                "scoped search lost the canonical sample anchor",
            )
        pending = {
            language: iter(
                sorted(
                    groups.get(language, []),
                    key=lambda path: ("test" not in path.lower(), path),
                )
            )
            for language in languages
        }

        def rounds() -> Iterator[tuple[list[tuple[str, str]], frozenset[str]]]:
            exhausted: set[str] = set()
            seen: set[str] = set()
            while len(exhausted) < len(languages):
                paths: list[tuple[str, str]] = []
                for language in languages:
                    if language in exhausted:
                        continue
                    path = next(pending[language], None)
                    if path is None:
                        exhausted.add(language)
                    elif path not in seen:
                        seen.add(path)
                        paths.append((language, path))
                yield paths, frozenset(exhausted)

        def enough(exhausted: frozenset[str]) -> bool:
            counts = language_counts(pool)
            return len(declaration_pool(pool)) >= minimum and (
                minimum != SYMBOL_COUNT
                or REQUIRED_LANGUAGES[self.pin.name].issubset(counts)
                and bool(symbol_pool(pool))
                and set(languages).issubset(represented | exhausted)
            )

        if enough(frozenset()):
            return pool
        grouped = minimum == SYMBOL_COUNT and self.pin.name in {"nextjs", "bun"}
        parallel = grouped and self.pin.name == "nextjs"
        batches = batched(rounds(), 2 if grouped else 1)

        async def read_batch(
            batch: Sequence[tuple[list[tuple[str, str]], frozenset[str]]],
        ) -> list[list[JsonObject]]:
            paths = [path for paths, _ in batch for _, path in paths]
            if grouped and len(paths) > 1:
                return await read_sample_paths(client, paths)
            return [await read(path) for path in paths]

        # The original language rounds decide the sample, even when the next query
        # starts early. A speculative read's result cannot add paths to that sample.
        batch = next(batches, ())
        pending_read = (
            asyncio.create_task(read_batch(batch)) if parallel and batch else None
        )
        try:
            while batch:
                current_read = pending_read
                following = next(batches, ())
                pending_read = (
                    asyncio.create_task(read_batch(following))
                    if parallel and following
                    else None
                )
                result = iter(
                    await current_read
                    if current_read is not None
                    else await read_batch(batch)
                )
                for paths, exhausted in batch:
                    for language, _ in paths:
                        added = next(result)
                        pool.extend(added)
                        if added:
                            represented.add(language)
                    if enough(exhausted):
                        return pool
                batch = following
        finally:
            if pending_read is not None:
                pending_read.cancel()
                await asyncio.gather(pending_read, return_exceptions=True)
        raise AssertionError(
            f"source catalog has fewer than {minimum} sampled declarations"
        )

    async def baseline(self) -> None:
        with self.server() as server:
            async with server.connect() as client:
                await await_startup_publication(server)
                candidates = await self.symbol_candidates(client)
                require(
                    len(candidates) >= SYMBOL_COUNT,
                    f"{self.pin.name}: fewer than {SYMBOL_COUNT} symbols",
                )
                self.record("publication", symbols=len(candidates))
                await client.resource("rift://workspace")
                workspace_map = await client.resource("rift://map")
                found = await observed(
                    server,
                    "dependency",
                    lambda rows: any(closes_span(row, CONTEXT_SPAN) for row in rows),
                )
                self.dependencies(found)
                if self.pin.name == "fastapi":
                    packages = objects(workspace_map, "packages")
                    require(
                        any(package.get("manager") == "pypi" for package in packages),
                        "uv.lock produced no pypi packages",
                    )
                await self.symbols(client, candidates)
                await self.oversized(client, server)
                await self.unparsed(client, server)
                if self.pin.name == "nextjs":
                    await self.symlinks(client)
                no_failed_builds(await logged(server, "index"))
            self.stop(server)
            self.record(
                "stop", state="idle", process_gone=server.process.poll() is not None
            )
        await self.lexical_persistence()

    def dependencies(self, found: list[Line]) -> None:
        """Pin the resolver's visible manifest count separately from raw tree blobs."""
        count = self.visible_manifests()
        expected = (
            None
            if count <= 256
            else f"{count - 256} of {count} package.json manifests were not read: at most 256 are read per workspace"
        )
        exact_degradation(found, expected)
        passes = [row for row in found if closes_span(row, CONTEXT_SPAN)]
        require(
            len(passes) == 1, f"startup read the dependency context {len(passes)} times"
        )
        require(
            {"entries", "degraded"}.issubset(passes[0].fields("close")),
            "the dependency context lost named fields",
        )
        if self.pin.name == "fastapi":
            degraded = [
                row.fields(CONTEXT_DEGRADED).get("reason")
                for row in found
                if row.is_message(CONTEXT_DEGRADED)
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

    async def oversized(self, client: Client, server: Server) -> None:
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
        started = datetime.now(UTC)
        answer = await settled_pattern(
            client,
            {
                "pattern": token,
                "target": "file",
                "paths": {"include": [path]},
                "limit": 1,
            },
        )
        ended = datetime.now(UTC)
        chunked_answer(answer, path, len(data), offset)
        named = chunked_warnings(
            await pattern_warnings(
                server, started, ended, self.pin.name == "bun", path
            ),
            path,
        )
        if self.pin.name == "bun":
            workspace = await client.resource("rift://workspace")
            claimed = [
                row
                for row in objects(workspace, "languages")
                if row.get("language") == "c"
            ]
            require(
                len(claimed) == 1
                and claimed[0].get("enabled") is True
                and claimed[0].get("syntax") is True,
                "Bun's pinned C source must have an enabled syntax provider",
            )
            require(
                named == ["large_file_unparsed"],
                f"{path}: named by {named}, expected large_file_unparsed alone",
            )
            found = await observed(
                server, "index", lambda rows: bool(build_records(rows, path))
            )
            held_unparsed_records(found, path, len(data), SYNTAX_FILE_BYTES)
        else:
            require(
                not named, f"{path}: named by {named} although split holds it whole"
            )
        self.record(
            "oversized", path=path, bytes=len(data), pattern=token, offset=offset
        )

    async def unparsed(self, client: Client, server: Server) -> None:
        """Search the pinned file past `[providers.syntax] max_file` held as text.

        The provider refuses its source for its size alone, so under the default `split`
        its text answers search, the logs name it in `large_file_unparsed`, and the
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
        started = datetime.now(UTC)
        answer = await settled_pattern(
            client,
            {
                "pattern": pattern,
                "target": "file",
                "paths": {"include": [path]},
                "limit": 1,
            },
        )
        ended = datetime.now(UTC)
        chunked_answer(answer, path, len(data), offset)
        named = chunked_warnings(
            await pattern_warnings(server, started, ended, True, path), path
        )
        require(
            named == ["large_file_unparsed"],
            f"{path}: named by {named}, expected large_file_unparsed alone",
        )
        found = await observed(
            server,
            "index",
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
        """Check 200 physical declarations and every sampled established identity."""
        initial = language_counts(candidates)
        pool = list(candidates)
        selected = sample_symbols(
            pool, SYMBOL_COUNT, SEED, REQUIRED_LANGUAGES[self.pin.name]
        )
        canonical = symbol_pool(pool)
        require(bool(canonical), "symbol pool has no established canonical identities")
        anchor = min(canonical)
        if self.pin.name == "bun":
            anchor = "rift://symbol/local/rust/bun_bundler/defines/TEST"
            require(
                anchor in canonical,
                "Bun lost its established Rust TEST identity",
            )
        elif self.pin.name == "fastapi":
            anchor = "rift://symbol/local/python/fastapi/routing/APIRouter"
            require(
                anchor in canonical, "FastAPI lost its established APIRouter identity"
            )
        anchor_hit = canonical[anchor]
        anchor_language = literal(anchor_hit, "language")
        if anchor_hit not in selected:
            position = next(
                index
                for index, hit in enumerate(selected)
                if literal(hit, "language") == anchor_language
            )
            selected[position] = anchor_hit
        details: list[Json] = []
        for hit in selected:
            details.append(
                {
                    "identity": optional_iri(hit, "symbol"),
                    "language": literal(hit, "language"),
                    "path": literal(hit, "path"),
                    "range": {
                        "start": int(literal(hit, "start")),
                        "end": int(literal(hit, "end")),
                    },
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

        async def verify(hit: JsonObject) -> int:
            identity = optional_iri(hit, "symbol")
            if identity is not None:
                identity = string_value(identity, "symbol identity")
                require(
                    identity not in identities,
                    f"duplicate sampled identity: {identity}",
                )
                identities.add(identity)
            unit = literal(hit, "path")
            path = self.root / unit
            start, end = (
                int(literal(hit, "start")),
                int(literal(hit, "end")),
            )
            content = path.read_bytes()
            require(0 <= start < end <= len(content), "symbol range exceeds source")
            if identity is not None:
                await self.require_symbol_item(
                    client, identity, unit, end, content, path
                )
                require(path.read_bytes() == content, f"read changed {path}")
                return 0
            # Declarations can start with attached comments or attributes. Extraction
            # keeps their end equal to the item end, so its final byte is in the item.
            request: JsonObject = {
                "path": str(path.relative_to(self.root)),
                "position": end - 1,
            }
            answer = await client.call("nodes", request)
            nodes = objects(answer, "nodes")
            require(bool(nodes), f"nodes listed nothing at {path}:{end - 1}")
            if answer.get("symbol") is None:
                require(
                    nodes[0].get("kind") == "file"
                    and nodes[0].get("range") == {"start": 0, "end": len(content)},
                    f"a unit answer did not open with the file node of {path}",
                )
            self.require_node_sources(nodes, unit, content, path)
            # The nodes covering the position can lie on a later page. Each page is another
            # read, which the deadline cannot afford for every sample, so the report counts
            # the declarations whose covering nodes the first page did not reach.
            listed = answer.get("node_cursor") is None or lists_position(
                answer, nodes, unit, end - 1
            )
            if not listed:
                require(path.read_bytes() == content, f"read changed {path}")
                return 1
            return 0

        beyond = 0
        if self.pin.name in {"nextjs", "bun"}:
            # Each read validates the full tree. Two independent reads can use the
            # server's worker pool while the corpus leaves the checkout unchanged.
            for pair in batched(selected, 2):
                async with asyncio.TaskGroup() as group:
                    reads = [group.create_task(verify(hit)) for hit in pair]
                beyond += sum(read.result() for read in reads)
        else:
            for hit in selected:
                beyond += await verify(hit)
        self.record(
            "identities",
            count=len(identities),
            seed=SEED,
            raw_beyond_first_page=beyond,
        )

    @staticmethod
    def require_node_sources(
        nodes: list[JsonObject], unit: str, content: bytes, path: Path
    ) -> None:
        """Require each node of the file at project path `unit` to carry its bytes."""
        for node in nodes:
            if unit_path(node) != unit:
                continue
            node_span = object_value(node.get("range"), "node range")
            node_start = number(node_span.get("start"), "node start")
            node_end = number(node_span.get("end"), "node end")
            require(
                0 <= node_start <= node_end <= len(content),
                "node range exceeds source",
            )
            excerpt = node.get("source")
            require(
                isinstance(excerpt, str)
                and excerpt.encode() == content[node_start:node_end],
                f"node source disagrees with captured bytes at {path}:{node_start}-{node_end}",
            )

    async def require_symbol_item(
        self,
        client: Client,
        identity: str,
        unit: str,
        end: int,
        content: bytes,
        path: Path,
    ) -> None:
        """Require the symbol read of `identity` to list the sampled declaration's item."""
        arguments: JsonObject = {"symbol": identity}
        for _ in range(NODE_PAGES_MAX):
            answer = await client.call("nodes", arguments)
            require(
                answer.get("symbol") == identity,
                f"a symbol read named another symbol: {identity}",
            )
            nodes = objects(answer, "nodes")
            self.require_node_sources(nodes, unit, content, path)
            if any(
                node.get("index") == 0
                and unit_path(node) == unit
                and object_value(node.get("range"), "node range").get("end") == end
                for node in nodes
            ):
                return
            cursor = answer.get("node_cursor")
            if cursor is None:
                break
            arguments = {"symbol": identity, "node_cursor": cursor}
        require(
            False,
            f"nodes omitted sampled declaration: {identity} ending at {path}:{end}",
        )

    async def lexical_persistence(self) -> None:
        """Preserve unrelated lexical rows after external source creation and removal."""
        before = lexical_content(self.root)
        sizes = database_bytes(self.root)
        path = self.root / PROBE_PATH
        request: JsonObject = corpus_request(
            {
                "query": "corpus_probe",
                "target": "symbol",
                "paths": {"include": [PROBE_PATH]},
                "include": ["source"],
                "limit": 5,
            }
        )
        with self.server() as server:
            async with server.connect() as client:
                await await_startup_publication(server)
                path.write_bytes(PROBE_SOURCE.encode())
                await self.await_probe(True)
                answer = await settled_local(client, "search", request)
                hits = bindings(answer)
                require(
                    len(hits) == 1
                    and literal(hits[0], "path") == PROBE_PATH
                    and literal(hits[0], "source") == PROBE_SOURCE.rstrip("\n"),
                    f"new source was absent from reads: {answer}",
                )
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
                await self.await_probe(False)
                answer = await settled_local(client, "search", request)
                require(
                    not bindings(answer), f"removed source remained indexed: {answer}"
                )
                require(
                    lexical_content(self.root) == before,
                    "source removal changed unrelated persisted lexical rows",
                )
                no_failed_builds(await logged(server, "index"))
            self.stop(server)

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
            corpus_request(
                {
                    "query": "next",
                    "paths": {"include": ["package.json"], "exclude": ["*/**"]},
                    "limit": 100,
                }
            ),
        )
        control_hits = bindings(control)
        require(
            bool(control_hits)
            and all(literal(hit, "path") == "package.json" for hit in control_hits),
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
                error.code == "resource_not_found"
                and error.retry == "never"
                and error.identity == "rift.server.read_not_found",
                f"symlink nodes wrong refusal: {error}",
            )
        else:
            raise AssertionError("nodes accepted a symlink")
        selector: JsonObject = {"include": [path]}
        if "/" not in path:
            selector["exclude"] = ["*/**"]
        answer = await client.call(
            "search", corpus_request({"query": "next", "paths": selector})
        )
        require(
            not bindings(answer),
            f"search indexed symlink {path}: {answer}",
        )
        require(path not in workspace, "workspace map lists a symlink")

    async def churn_case(self) -> None:
        """Run sustained reads and edits in their own required bounded case."""
        directory = self.root / "rift_corpus_probe"
        directory.mkdir()
        manifest = directory / "Cargo.toml"
        probe = directory / PROBE_PATH
        try:
            manifest.write_text(
                '[package]\nname = "rift_corpus_probe"\nversion = "1.0.0"\n'
                f'[lib]\npath = "{PROBE_PATH}"\n[workspace]\n',
                encoding="utf-8",
            )
            probe.write_text(churn_source(0), encoding="utf-8")
            with self.server() as server:
                async with server.connect() as client:
                    await await_startup_publication(server)
                    await self.churn(
                        client, server, probe.relative_to(self.root).as_posix()
                    )
                    no_failed_builds(await logged(server, "index"))
                self.stop(server)
                self.record("stop", state="after_churn", process_gone=True)
        finally:
            probe.unlink(missing_ok=True)
            manifest.unlink(missing_ok=True)
            directory.rmdir()

    async def churn(self, client: Client, server: Server, probe_path: str) -> None:
        """Read complete source revisions while external writes continue every two seconds."""
        path = self.root / probe_path
        requests = {name: dict(request) for name, request in CHURN_REQUESTS.items()}
        requests["nodes"] = {"path": probe_path, "position": 0}
        sources = [churn_source(0).rstrip("\n")]
        written = [time.monotonic()]
        timings: dict[str, list[float]] = {name: [] for name in CHURN_REQUESTS}
        stale_reads: list[tuple[str, datetime, datetime]] = []
        overlaps = 0
        converging = False

        def write(edit: int) -> None:
            source = churn_source(edit)
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
            # First reads and convergence can wait for a publication. Keep the shorter
            # budget for steady reads while the writer is running.
            first = not timings[name]
            budget = READ_SECONDS if first or converging else STEADY_READ_SECONDS
            started = time.monotonic()
            started_at = datetime.now(UTC)
            revision: int | None = None
            try:
                async with gate_deadline(f"churn {name}", budget):
                    answer = await read_settled_local(
                        client,
                        CHURN_TOOLS.get(name, name),
                        requests[name],
                        seconds=budget,
                        poll_seconds=POLL_SECONDS,
                        selection="current",
                    )
                ended_at = datetime.now(UTC)
                found, source = churn_answer(
                    name, answer, sources, identity, probe_path
                )
                revision = sources.index(source)
                if revision < before - 1:
                    stale_reads.append((name, started_at, ended_at))
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
                )

        task: asyncio.Task[None] | None = None
        try:
            identity, _revision = await read("symbol_nodes", CHURN_PROBE_ID)
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
                # First confirm the final publication through the exact declaration.
                # The graph read then has its request budget for retaining that view.
                for name in ("symbol_nodes", "search", "nodes"):
                    while True:
                        _identity, revision = await read(name, identity)
                        if revision == len(sources) - 1:
                            break
                        await asyncio.sleep(POLL_SECONDS)
            if stale_reads:
                await observed(
                    server,
                    "mcp",
                    lambda rows: all(
                        any(
                            started <= row.time <= ended
                            and row.operation == "read.warning"
                            and row.label("tool") == CHURN_TOOLS.get(name, name)
                            and row.label("warning_code") == "stale_index"
                            for row in rows
                        )
                        for name, started, ended in stale_reads
                    ),
                )
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
                await await_startup_publication(server)
                candidates = await self.symbol_candidates(client, minimum=1)
                require(bool(candidates), "symlink root has no search results")
                no_failed_builds(await logged(server, "index"))
            self.stop(server)
        self.record("symlink_root", reads=True)

    async def shallow(self, root: Path) -> None:
        self.pin.checkout(root, depth=1)
        self.configure(root=root)
        application = "fastapi/applications.py"
        with self.server(root) as server:
            async with server.connect() as client:
                found = await settled_local(
                    client,
                    "search",
                    corpus_request(
                        {
                            "query": "FastAPI",
                            "target": "symbol",
                            "paths": {"include": [application]},
                            "limit": 1000,
                        }
                    ),
                )
                selected = []
                for hit in bindings(found):
                    if (
                        literal(hit, "name") == "FastAPI"
                        and literal(hit, "kind") == "class"
                    ):
                        identity = iri(hit, "symbol")
                        require(
                            identity.startswith("rift://symbol/local/python/"),
                            "shallow class lost its canonical Python identity",
                        )
                        require(
                            literal(hit, "path") == application,
                            "shallow class lost its original Python source path",
                        )
                        selected.append(hit)
                require(
                    len(selected) == 1,
                    "shallow checkout must have one canonical FastAPI class",
                )
                hit = selected[0]
                identity = iri(hit, "symbol")
                answer = await settled_local(
                    client,
                    "get_symbol",
                    {"id": identity, "include": ["history"]},
                )
                require(answer.get("outcome") == "found", "shallow class was not found")
                require(
                    object_value(answer.get("symbol"), "shallow symbol").get("id")
                    == identity,
                    "shallow class changed its identity",
                )
                evidence = {
                    "name": literal(hit, "name"),
                    "id": identity,
                    "path": literal(hit, "path"),
                    "history": answer.get("history"),
                    "revision": answer.get("revision"),
                    "root": str(root),
                }
                history = object_value(
                    answer.get("history"),
                    "symbol history " + json.dumps(evidence, sort_keys=True)[:4096],
                )
                require(
                    history.get("complete") is False,
                    "shallow history must report complete=false",
                )
                no_failed_builds(await logged(server, "index"))
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
                                "search", CHURN_REQUESTS["search"]
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
                            answer.get("outcome") == "unavailable"
                            and answer.get("reason") == "index_preparing",
                            f"source.files overflow returned an unexpected answer: {answer}",
                        )
                        await asyncio.sleep(POLL_SECONDS)
                    await observed(
                        server,
                        "index",
                        lambda rows: any(
                            row.is_message("index rebuild failed")
                            and row.fields("index rebuild failed").get("error_code")
                            == "limit_exceeded"
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
                await settled_local(
                    client, "search", corpus_request({"query": "test", "limit": 1})
                )
                failure_records = await observed(
                    server,
                    "search",
                    lambda rows: any(
                        row.operation == "search.commit"
                        and row.label("outcome") == "error"
                        for row in rows
                    ),
                )
                count = lexical_breach(failure_records, 20000)
                answer = await client.call(
                    "search", corpus_request({"query": "test", "limit": 1})
                )
                require(
                    answer.get("outcome") == "unavailable"
                    and answer.get("reason") == "insufficient_coverage",
                    "incomplete lexical ranking reported a complete answer",
                )
                anchor = "rift://symbol/local/rust/bun_bundler/defines/TEST"
                captured = await client.call(
                    "search",
                    {"query": f"SELECT ?name {{ <{anchor}> a :Symbol; :name ?name }}"},
                )
                require(
                    [literal(row, "name") for row in bindings(captured)] == ["TEST"],
                    "Bun lost its established Rust TEST identity",
                )
                exact = await client.call("get_symbol", {"id": anchor})
                require(
                    exact.get("outcome") == "found"
                    and object_value(exact.get("symbol"), "symbol").get("id") == anchor
                    and bool(objects(exact, "declarations")),
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
                await client.call(
                    "search", corpus_request({"query": "test", "limit": 1})
                )
                no_failed_builds(await logged(server, "index"))
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
    server: Server,
    offset: int,
    wanted: str,
    holds: Callable[[str], bool],
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
        f"required record never reached server output within {OBSERVATION_SECONDS}s: {wanted}"
    )


async def settled_local(client: Client, name: str, request: JsonObject) -> JsonObject:
    """Resend partial current reads until local index preparation completes.

    The wait is bounded by `LOCAL_PREPARATION_SECONDS`. Captured selections use
    `read_settled_local` with its captured selection instead.
    """
    return await read_settled_local(
        client,
        name,
        request,
        seconds=LOCAL_PREPARATION_SECONDS,
        poll_seconds=POLL_SECONDS,
        selection="current",
    )


async def await_startup_publication(server: Server) -> None:
    """Wait for startup publication before asking the server to retain a view."""
    async with gate_deadline(STARTUP_PUBLICATION, LOCAL_PREPARATION_SECONDS):
        while True:
            server.check_running()
            output = server.read_log()
            if startup_published(output):
                return
            if server.output_cut:
                published = await logged(server, "index")
                if any(
                    row.operation == "index.publish"
                    and row.label("trigger") == "startup"
                    for row in published
                ):
                    return
            await asyncio.sleep(POLL_SECONDS)


async def settled_pattern(client: Client, request: JsonObject) -> JsonObject:
    """Resend a current pattern search while its trigram index is preparing."""
    return await read_settled_local(
        client,
        "search",
        corpus_request(request),
        seconds=OBSERVATION_SECONDS,
        poll_seconds=POLL_SECONDS,
        selection="current",
    )


async def pattern_warnings(
    server: Server, started: datetime, ended: datetime, expected: bool, path: str
) -> list[JsonObject]:
    """Read diagnostics from the completed pattern request, including pending log writes."""

    def selected(rows: list[Line]) -> list[Line]:
        matching = [
            row
            for row in rows
            if started <= row.time <= ended and row.label("tool") == "search"
        ]
        completed = [
            row for row in matching if row.is_message("tool request completed")
        ]
        if not completed:
            return []
        request = completed[-1].label("req")
        return [row for row in matching if row.label("req") == request]

    def available(rows: list[Line]) -> bool:
        request_rows = selected(rows)
        if not request_rows:
            return False
        if not expected:
            return True
        return "large_file_unparsed" in chunked_warnings(
            logged_read_warnings(request_rows), path
        )

    return logged_read_warnings(selected(await observed(server, "mcp", available)))


def objects(answer: JsonObject, key: str) -> list[JsonObject]:
    return [object_value(value, key) for value in array_value(answer.get(key), key)]


async def logged(server: Server, component: str) -> list[Line]:
    """Read persisted records through the bounded CLI command."""
    arguments = ["server", "logs", "--tail", "all", "--component", component]
    text = await asyncio.to_thread(server.read_logs, arguments)
    return records(text)


def held_unparsed_records(
    found: list[Line], path: str, size: int, maximum: int
) -> None:
    """Require a size refusal while the index retains the complete source text.

    >>> from rift_dev.log_records import parse_line
    >>> path = "src/jsc/bindings/sqlite/sqlite3.c"
    >>> reason = f"Rust syntax analysis failed: cause source bytes 9508000 exceed accepted limit 4194304: path {path}; reduce source bytes below 4194304 and retry"
    >>> line = parse_line(f'2026-10-09 17:50:00.852Z WARN rift_index::workspace::log_held_unparsed component=index operation=index.build path={path} reason="{reason}"  file held unparsed in the index')
    >>> assert line is not None
    >>> held_unparsed_records([line], path, 9508000, 4194304)
    >>> held_unparsed_records([line], path, 9508000, 8388608)
    Traceback (most recent call last):
    ...
    AssertionError: src/jsc/bindings/sqlite/sqlite3.c: size refusal must name 9508000 bytes and accepted limit 8388608
    >>> held_unparsed_records([line], "other.c", 9508000, 4194304)
    Traceback (most recent call last):
    ...
    AssertionError: other.c: expected held unparsed build records alone, received []
    >>> held_unparsed_records([line], path, 9508001, 4194304)
    Traceback (most recent call last):
    ...
    AssertionError: src/jsc/bindings/sqlite/sqlite3.c: size refusal must name 9508001 bytes and accepted limit 4194304
    """
    messages = build_records(found, path)
    require(
        bool(messages) and set(messages) == {HELD_UNPARSED_RECORD},
        f"{path}: expected held unparsed build records alone, received {messages}",
    )
    expected = f"source bytes {size} exceed accepted limit {maximum}: path {path};"
    for row in found:
        if row.operation == "index.build" and row.label("path") == path:
            require(
                expected in row.label("reason"),
                f"{path}: size refusal must name {size} bytes and accepted limit {maximum}",
            )


async def observed(
    server: Server, component: str, predicate: Callable[[list[Line]], bool]
) -> list[Line]:
    """Wait for the log drain under one deadline, including CLI reads and polls."""
    async with asyncio.timeout(OBSERVATION_SECONDS):
        while True:
            found = await logged(server, component)
            if predicate(found):
                return found
            await asyncio.sleep(POLL_SECONDS)
