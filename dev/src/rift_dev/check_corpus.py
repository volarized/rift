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
    declaration_pool,
    exact_degradation,
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
    symbol_pool,
    token_past_chunk,
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
    name_request,
    node_request,
    optional_iri,
    project_source_unit,
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
    columns = ["symbol", "name", "kind", "language", "path", "unit", "start", "end"]
    if parameters.get("target") == "file":
        columns.extend(("target", "size"))
    for include in array_value(parameters.get("include", []), "retrieval include"):
        field = string_value(include, "retrieval field")
        if field in {"source", "score"}:
            columns.append(field)
    return retrieval_request(parameters, columns)


CHURN_SEARCH_INPUTS: JsonObject = {
    "query": "corpus_probe",
    "target": "symbol",
    "include": ["source"],
    "limit": 1,
}
CHURN_REQUESTS: dict[str, JsonObject] = {
    "search": corpus_request(CHURN_SEARCH_INPUTS),
    "get_symbol": {"include": ["source"]},
    "nodes": node_request(project_source_unit(PROBE_PATH), position=0),
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
        and final exhaustion of an optional language. These typed callback rows
        test control flow; the owning corpus run checks actual declarations.

        >>> from types import SimpleNamespace
        >>> from typing import cast
        >>> from unittest.mock import AsyncMock
        >>> def term(value, datatype="string"):
        ...     return {"type": "literal", "value": str(value), "datatype": "http://www.w3.org/2001/XMLSchema#" + datatype}
        >>> rows = [{**{key: term(value) for key, value in {"name": f"entry{index}", "kind": "function", "language": "typescript", "path": "src/test.ts"}.items()}, "start": term(index * 2, "integer"), "end": term(index * 2 + 1, "integer"), "symbol": {"type": "iri", "value": f"urn:fixture:sample:{index}"}} for index in range(SYMBOL_COUNT)]
        >>> def answer(bindings):
        ...     return {"answer": {"result": "bindings", "variables": list(rows[0]), "bindings": bindings}}
        >>> languages = [{"language": language, "enabled": True, "syntax": True, "include": ["**"]} for language in ("rust", "typescript", "typescript:tsx", "markdown")]
        >>> common = {"configuration_revision": "0123abcd", "languages": languages}
        >>> first = {**common, "pagination": {"page_index": 0, "total_pages": 2}, "source": [{"path": f"docs/a{index:04}.md", "digest": "0123abcd", "language": "markdown"} for index in range(1000)]}
        >>> second = {**common, "pagination": {"page_index": 1, "total_pages": 2}, "source": [{"path": path, "digest": "0123abcd", "language": language} for path, language in [("src/test.ts", "typescript"), ("src/ztest.tsx", "typescript:tsx")]]}
        >>> admission = {"answer": {"result": "bindings", "variables": ["symbol"], "bindings": [{"symbol": {"type": "iri", "value": "urn:fixture:startup"}}]}}
        >>> client = SimpleNamespace(resource=AsyncMock(side_effect=[first, second]), call=AsyncMock(side_effect=[admission, answer(rows), answer([])]))
        >>> sample = Corpus(cast(Pin, SimpleNamespace(name="nextjs")), Path("rift"), Path("report"))
        >>> selected = asyncio.run(sample.symbol_candidates(cast(Client, client)))
        >>> len(selected), client.call.await_count, client.resource.await_count
        (200, 3, 2)
        >>> all(literal(row, "path") == "src/test.ts" for row in selected)
        True
        >>> client.call.await_args_list[0].args[1] == name_request("corpus_probe")
        True
        >>> "SERVICE" in client.call.await_args_list[0].args[1]["query"]
        False
        """
        languages = SAMPLE_LANGUAGES[self.pin.name]

        async def read(path: str, query: str = "test") -> list[JsonObject]:
            answer = await settled_local(
                client,
                "search",
                corpus_request(
                    {
                        "query": query,
                        "target": "symbol",
                        "paths": {"include": [path]},
                        "limit": 1000,
                        "order": "identity",
                    }
                ),
            )
            warnings(answer)
            found = bindings(answer)
            require(
                all(literal(hit, "path") == path for hit in found),
                "literal sample path matched another file",
            )
            return found

        # The Domain query waits for index preparation without lexical ranking.
        # Its identity rows are admission evidence, not declarations in the sample.
        bindings(await settled_local(client, "search", name_request("corpus_probe")))
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
        exhausted: set[str] = set()
        seen: set[str] = set()
        while True:
            counts = language_counts(pool)
            enough = len(declaration_pool(pool)) >= minimum
            if minimum == SYMBOL_COUNT:
                enough = (
                    enough
                    and REQUIRED_LANGUAGES[self.pin.name].issubset(counts)
                    and bool(symbol_pool(pool))
                    and set(languages).issubset(represented | exhausted)
                )
            if enough:
                return pool
            progress = False
            for language in languages:
                if language in exhausted:
                    continue
                path = next(pending[language], None)
                if path is None:
                    exhausted.add(language)
                    progress = True
                    continue
                progress = True
                if path in seen:
                    continue
                seen.add(path)
                added = await read(path)
                pool.extend(added)
                if added:
                    represented.add(language)
            require(
                progress,
                f"source catalog has fewer than {minimum} sampled declarations",
            )

    async def baseline(self) -> None:
        with self.server() as server:
            async with server.connect() as client:
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
                await self.lexical_persistence(client)
                await self.oversized(client, server)
                await self.unparsed(client, server)
                if self.pin.name == "nextjs":
                    await self.symlinks(client)
                no_failed_builds(await logged(server, "index"))
            self.stop(server)
            self.record(
                "stop", state="idle", process_gone=server.process.poll() is not None
            )

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
        for hit in selected:
            identity = optional_iri(hit, "symbol")
            if identity is not None:
                identity = string_value(identity, "symbol identity")
                require(
                    identity not in identities,
                    f"duplicate sampled identity: {identity}",
                )
                identities.add(identity)
            path = self.root / literal(hit, "path")
            start, end = (
                int(literal(hit, "start")),
                int(literal(hit, "end")),
            )
            content = path.read_bytes()
            require(0 <= start < end <= len(content), "symbol range exceeds source")
            # Declarations can start with attached comments or attributes. Their
            # final byte remains inside the original declaration node.
            # This existing corpus check reads the current default local workspace.
            # Held or named selections must use an observed public unit instead.
            unit = project_source_unit(literal(hit, "path"))
            answer = await client.call("nodes", node_request(unit, position=end - 1))
            rows = bindings(answer)
            require(bool(rows), f"nodes omitted captured syntax for {path}")
            current_view = object_value(answer.get("view"), "node query view")
            require(
                bool(string_value(current_view.get("id"), "node query view id"))
                and instant(
                    string_value(current_view.get("expires_at"), "node query expiry")
                )
                is not None,
                "node query has an invalid captured view",
            )
            for node in rows:
                node_start = int(literal(node, "node_start"))
                node_end = int(literal(node, "node_end"))
                require(
                    0 <= node_start <= end - 1 < node_end <= len(content),
                    "node range exceeds original source or misses requested byte",
                )
                require(iri(node, "node_unit") == unit, "nodes changed captured unit")
                require(
                    literal(node, "node_language") == literal(hit, "language"),
                    "nodes changed original language",
                )
                string_value(literal(node, "node_id"), "original Node id")
                captured = literal(node, "text").encode()
                require(
                    captured == content,
                    f"node source disagrees with captured bytes at {path}",
                )
                require(
                    captured[node_start:node_end] == content[node_start:node_end],
                    f"node source span changed at {path}:{node_start}-{node_end}",
                )
            if identity is not None:
                require(
                    any(optional_iri(node, "node_symbol") == identity for node in rows),
                    f"nodes omitted sampled declaration: {identity} at {end - 1}; {rows}",
                )
            else:
                matching = [
                    node
                    for node in rows
                    if int(literal(node, "node_end")) >= end
                    and int(literal(node, "node_start")) <= end - 1
                    and (
                        int(literal(node, "node_end")) == end
                        or optional_iri(node, "facet")
                        == "urn:rift:vocabulary:declaration"
                    )
                ]
                require(
                    bool(matching),
                    f"nodes omitted raw declaration at {path}:{start}-{end}",
                )
                require(
                    all(node.get("node_symbol") is None for node in matching),
                    f"raw declaration acquired an unproved identity at {path}:{start}-{end}",
                )
            require(path.read_bytes() == content, f"read changed {path}")
        self.record("identities", count=len(identities), seed=SEED)

    async def lexical_persistence(self, client: Client) -> None:
        """Preserve unrelated lexical rows after external source creation and removal."""
        before = lexical_content(self.root)
        sizes = database_bytes(self.root)
        path = self.root / PROBE_PATH
        path.write_bytes(PROBE_SOURCE.encode())
        request: JsonObject = corpus_request(
            {
                "query": "corpus_probe",
                "target": "symbol",
                "paths": {"include": [PROBE_PATH]},
                "include": ["source"],
                "limit": 5,
            }
        )
        answer = await client.call("search", request)
        hits = bindings(answer)
        require(
            len(hits) == 1 and literal(hits[0], "source") == PROBE_SOURCE.rstrip("\n"),
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
        answer = await client.call("search", request)
        require(not bindings(answer), f"removed source remained indexed: {answer}")
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
            await client.call(
                "nodes", node_request(project_source_unit(path), position=0)
            )
        except ToolFailure as error:
            require(
                error.code == "temporarily_unavailable"
                and "required Nodes facts" in str(error),
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
            with self.server() as server:
                async with server.connect() as client:
                    # Listener readiness precedes file preparation. Observe the existing
                    # startup budget before measuring reads against external writes.
                    bindings(
                        await settled_local(
                            client, "search", name_request("corpus_probe")
                        )
                    )
                    await self.churn(client, probe.relative_to(self.root).as_posix())
                    no_failed_builds(await logged(server, "index"))
                self.stop(server)
                self.record("stop", state="after_churn", process_gone=True)
        finally:
            probe.unlink(missing_ok=True)
            manifest.unlink(missing_ok=True)
            directory.rmdir()

    async def churn(self, client: Client, probe_path: str = PROBE_PATH) -> None:
        """Read complete source revisions while external writes continue every two seconds."""
        path = self.root / probe_path
        requests = {name: dict(request) for name, request in CHURN_REQUESTS.items()}
        requests["nodes"] = node_request(project_source_unit(probe_path), position=0)
        requests["search"] = corpus_request(
            {**CHURN_SEARCH_INPUTS, "paths": {"include": [probe_path]}}
        )
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
                        requests[name],
                        seconds=budget,
                        poll_seconds=POLL_SECONDS,
                        selection="current",
                    )
                codes = [
                    string_value(warning.get("code"), "warning code")
                    for warning in warnings(answer)
                ]
                found, source = churn_answer(
                    name, answer, sources, identity, probe_path
                )
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
            identity, _revision = await read("search", None)
            requests["get_symbol"]["id"] = identity
            requests["nodes"] = node_request(
                project_source_unit(probe_path), position=0, symbol=identity
            )
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
                candidates = await self.symbol_candidates(client, minimum=1)
                require(bool(candidates), "symlink root has no search results")
                no_failed_builds(await logged(server, "index"))
            self.stop(server)
        self.record("symlink_root", reads=True)

    async def shallow(self, root: Path) -> None:
        self.pin.checkout(root, depth=1)
        self.configure(root=root)
        with self.server(root) as server:
            async with server.connect() as client:
                found = await settled_local(
                    client,
                    "search",
                    corpus_request(
                        {
                            "query": "FastAPI",
                            "target": "symbol",
                            "paths": {"include": ["fastapi/**/*.py"]},
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
                            literal(hit, "path").endswith(".py"),
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
                    "view": answer.get("view"),
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
                            any(
                                warning.get("code") == "local_index_preparing"
                                for warning in warnings(answer)
                            ),
                            f"source.files overflow returned a complete read: {answer}",
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
                await observed(
                    server,
                    "search",
                    lambda rows: any(
                        row.is_message(
                            "the lexical commit failed; the next publication compares every "
                            "file with the digests the store recorded"
                        )
                        for row in rows
                    ),
                )
                answer = await client.call(
                    "search", corpus_request({"query": "test", "limit": 1})
                )
                count = lexical_breach(answer, 20000)
                require(
                    answer.get("outcome") == "unavailable",
                    "incomplete lexical ranking reported a complete answer",
                )
                captured = await client.call("search", name_request("TEST"))
                canonical = {iri(row, "symbol") for row in bindings(captured)}
                anchor = "rift://symbol/local/rust/bun_bundler/defines/TEST"
                require(
                    anchor in canonical, "Bun lost its established Rust TEST identity"
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


async def settled_pattern(client: Client, request: JsonObject) -> JsonObject:
    """Resend a `pattern` search until the trigram index covers every stored row.

    An answer warning `pattern_index_preparing` covers only the rows the trigram index
    holds, and the warning asks the caller to resend once the index catches up. The
    deadline alone bounds the loop, as in `Corpus.await_probe`.
    """
    deadline = time.monotonic() + OBSERVATION_SECONDS
    query = corpus_request(request)
    while time.monotonic() < deadline:
        answer = await client.call("search", query)
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
