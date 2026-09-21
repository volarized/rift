"""Fetch pinned trees or run their bounded suites through the compiled Rift binary."""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import time
import traceback
from collections.abc import Callable
from pathlib import Path

from mcp.shared.exceptions import McpError

from rift_dev.corpus_assertions import (
    PROBE_PATH,
    PROBE_SOURCE,
    READ_COUNT,
    SYMBOL_COUNT,
    active_stdout,
    churn_answer,
    database_bytes,
    exact_degradation,
    fields,
    language_counts,
    lexical_breach,
    lexical_content,
    map_paths,
    no_failed_builds,
    number,
    probe_units,
    records,
    sample_symbols,
    warnings,
)
from rift_dev.corpus_cache import Pin, git
from rift_dev.rift_test_client import (
    Client,
    Json,
    JsonObject,
    Server,
    array_value,
    gate_deadline,
    object_value,
    require,
    string_value,
)

READ_SECONDS = 30.0
# Seconds a case keeps inside its own deadline for the served tree's removal,
# the report write, and the process exit. Nextest allows the same grace after
# it ends a corpus case, so the two bounds agree on what cleanup costs.
CLEANUP_RESERVE_SECONDS = 30.0
SEED = 34
POLL_SECONDS = 0.1
OBSERVATION_SECONDS = 60.0
CONFIGURATION = '[server]\nreadiness_timeout = "30s"\n[search.vector]\ndisabled = true\n[logs]\npage_records = 5000\ncapture = "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info"\n'
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
        self.root = Path()
        self.sequence = 0
        self.started = time.monotonic()

    def record(self, action: str, **values: Json) -> None:
        entry: JsonObject = {
            "action": action,
            **values,
            "elapsed_seconds": time.monotonic() - self.started,
        }
        self.actions.append(entry)
        print(json.dumps(entry), flush=True)

    def server(self, root: Path | None = None) -> Server:
        self.sequence += 1
        return Server(
            self.binary,
            root or self.root,
            self.report.parent / f"{self.report.stem}.server-{self.sequence}.log",
            startup_seconds=180.0,
            env={
                "RUST_LOG": "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info",
                "NO_COLOR": "1",
            },
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
        """A timeout fails the suite after server cleanup writes its evidence."""
        started = time.monotonic()
        self.started = started
        status = "failed"
        failure = ""
        budget = self.work_seconds()
        self.report.parent.mkdir(parents=True, exist_ok=True)
        try:
            async with asyncio.timeout(budget):
                with tempfile.TemporaryDirectory(
                    prefix=f"rift-corpus-{self.pin.name}-"
                ) as directory:
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
                        "status": status,
                        "failure": failure,
                        "elapsed_seconds": time.monotonic() - started,
                        "actions": self.actions,
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )

    async def tree(self, directory: Path) -> None:
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
                answer = await client.call(
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
                        row.get("message") == "dependency.resolve" for row in rows
                    ),
                )
                self.dependencies(found)
                if self.pin.name == "fastapi":
                    packages = objects(workspace_map, "packages")
                    require(
                        any(package.get("manager") == "pypi" for package in packages),
                        "uv.lock produced no pypi packages",
                    )
                await self.left_out(client)
                await self.symbols(client, candidates)
                await self.lexical_persistence(client)
                if self.pin.name == "nextjs":
                    await self.symlinks(client)
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            server.stop()
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
        passes = [row for row in found if row.get("message") == "dependency.resolve"]
        require(len(passes) == 1, f"startup resolved dependencies {len(passes)} times")
        require(
            fields(passes[0]).get("span") == "closed",
            "dependency resolution span did not close",
        )
        require(
            {"entries", "degraded"}.issubset(fields(passes[0])),
            "dependency resolution lost named fields",
        )
        if self.pin.name == "fastapi":
            degraded = [
                row
                for row in found
                if fields(row).get("resolver") == "uv"
                and row.get("message") == "dependency resolution degraded"
            ]
            expected_environment = f"pyproject.toml: no environment at {self.root}/.venv; packages cataloged without source roots"
            require(
                [fields(row).get("reason") for row in degraded]
                == [expected_environment],
                f"unexpected fastapi uv degradation: {degraded}",
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
        )
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
                input_bytes=encoded,
                accepted=(0, 1),
            ).split(b"\0")
        )
        return sum(
            path.encode() not in ignored
            and "target" not in Path(path).parts
            and not (self.root / path).is_symlink()
            for path in candidates
        )

    async def left_out(self, client: Client) -> None:
        if not self.pin.oversized_path:
            return
        path = self.pin.oversized_path
        require(
            (self.root / path).stat().st_size == self.pin.oversized_bytes,
            f"{path}: pinned byte count changed",
        )
        found = await observed(
            client,
            "rift://logs/component/index",
            lambda rows: any(fields(row).get("path") == path for row in rows),
        )
        matching = [row for row in found if fields(row).get("path") == path]
        require(
            any("left out" in str(row.get("message")) for row in matching),
            f"{path}: missing left-out reason",
        )
        self.record(
            "left_out", path=path, bytes=self.pin.oversized_bytes, records=matching
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
        async with asyncio.timeout(OBSERVATION_SECONDS):
            for _ in range(int(OBSERVATION_SECONDS / POLL_SECONDS)):
                if (probe_units(self.root) > 0) == present:
                    return
                await asyncio.sleep(POLL_SECONDS)
        raise AssertionError(
            f"lexical probe publication never reached present={present}"
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
        except McpError as error:
            require(
                object_value(error.error.data, "nodes refusal").get("code")
                == "resource_not_found",
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
                await self.churn(client)
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            server.stop()
            self.record("stop", state="after_churn", process_gone=True)

    async def churn(self, client: Client) -> None:
        """Read complete source revisions while external writes continue every two seconds."""
        path = self.root / PROBE_PATH
        sources: list[str] = []
        timings: dict[str, list[float]] = {name: [] for name in CHURN_REQUESTS}
        overlaps = 0

        def write(edit: int) -> None:
            source = f"pub fn corpus_probe() {{ let value = {edit}; }}\n"
            path.write_text(source, encoding="utf-8")
            sources.append(source.rstrip("\n"))

        async def writer() -> None:
            for edit in range(1, int(self.work_seconds()) // 2):
                await asyncio.sleep(2.0)
                write(edit)

        async def read(name: str, identity: str | None) -> tuple[str, int]:
            nonlocal overlaps
            before = len(sources)
            started = time.monotonic()
            codes: list[str] = []
            revision: int | None = None
            try:
                async with gate_deadline(f"churn {name}", READ_SECONDS):
                    answer = await client.call(name, CHURN_REQUESTS[name])
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
                    writes=changed,
                    source_revision=revision,
                    source_revision_at_start=before - 1,
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
            async with gate_deadline("corpus churn", self.work_seconds()):
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
            convergence = time.monotonic()
            async with gate_deadline("churn final source", READ_SECONDS):
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
                latency={
                    name: {"calls": len(values), "maximum_seconds": max(values)}
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
                answer = await client.call("search", {"query": "test", "limit": 1})
                require(
                    bool(objects(answer, "results")),
                    "symlink root has no search results",
                )
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            server.stop()
        self.record("symlink_root", reads=True)

    async def shallow(self, root: Path) -> None:
        self.pin.checkout(root, depth=1)
        self.configure(root=root)
        with self.server(root) as server:
            async with server.connect() as client:
                found = await client.call(
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
            server.stop()
        self.record("shallow_history", complete=False, depth=1)

    async def source_bound(self) -> None:
        self.configure("[source]\nfiles = 20000\n")
        server = self.server()
        try:
            server.start(wait_for_publication=False)
            await asyncio.to_thread(server.process.wait, timeout=180.0)
            require(
                server.process.returncode != 0, "source.files overflow started a server"
            )
            detail = server.read_log()
            expected = "field source.files, observed 20001, maximum 20000"
            require(
                expected in detail, f"source bound refusal missing {expected}: {detail}"
            )
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
                        == "the lexical commit failed; the next publication replaces the whole unit set"
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
            server.stop()
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
            server.stop()
        self.record("stop", state="idle", process_gone=True)
        await self.stop_during("rebuild")
        await self.stop_during("history")

    async def stop_during(self, operation: str) -> None:
        """Observe synchronous output while the operation can hold the database writer."""
        with self.server() as server:
            async with server.connect() as client:
                startup = await observed_output(
                    server, 0, 'operation="index.publish" trigger="startup"'
                )
                log_offset = len(startup)
                if operation == "rebuild":
                    (self.root / PROBE_PATH).write_text(PROBE_SOURCE)
                    pending = asyncio.create_task(
                        client.call("search", {"query": "corpus_probe"})
                    )
                    wanted = "index capture started"
                else:
                    pending = asyncio.create_task(
                        client.call(
                            "get_symbol",
                            {"name": "test", "include": ["history"], "limit": 200},
                        )
                    )
                    wanted = "symbol history started"
                try:
                    output = await observed_output(server, log_offset, wanted)
                    require(
                        operation == "rebuild" or not pending.done(),
                        "history request completed before stop",
                    )
                    evidence = active_stdout(output, operation, None)
                    await asyncio.to_thread(server.stop)
                    self.record(
                        "stop",
                        state=f"mid_{operation}",
                        stderr=evidence,
                        process_gone=True,
                    )
                finally:
                    pending.cancel()
                    await asyncio.gather(pending, return_exceptions=True)
        (self.root / PROBE_PATH).unlink(missing_ok=True)


async def observed_output(server: Server, offset: int, message: str) -> str:
    """Read owned output without waiting for the log store's database write turn."""
    async with asyncio.timeout(OBSERVATION_SECONDS):
        for _ in range(int(OBSERVATION_SECONDS / POLL_SECONDS)):
            server.check_running()
            output = server.read_log()[offset:]
            if output.endswith("\n") and message in output:
                return output
            await asyncio.sleep(POLL_SECONDS)
    raise AssertionError(f"required record never reached server output: {message}")


def objects(answer: JsonObject, key: str) -> list[JsonObject]:
    return [object_value(value, key) for value in array_value(answer.get(key), key)]


async def observed(
    client: Client, uri: str, predicate: Callable[[list[JsonObject]], bool]
) -> list[JsonObject]:
    """Wait for the log drain under the documented asynchronous logging contract."""
    async with asyncio.timeout(OBSERVATION_SECONDS):
        for _ in range(int(OBSERVATION_SECONDS / POLL_SECONDS)):
            found = records(await client.resource(uri))
            if predicate(found):
                return found
            await asyncio.sleep(POLL_SECONDS)
    raise AssertionError(f"required record never reached {uri}")
