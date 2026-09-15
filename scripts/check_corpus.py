#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp==1.26.0", "jsonschema==4.26.0", "psutil==7.2.2", "pywin32==312; sys_platform == 'win32'"]
# ///
"""Fetch pinned trees or run their bounded suites through the compiled Rift binary."""

from __future__ import annotations

import argparse
import asyncio
import dataclasses
import json
import os
import tempfile
import time
import traceback
from collections.abc import Callable
from pathlib import Path

from corpus_assertions import (
    PROBE_PATH,
    PROBE_SOURCE,
    READ_COUNT,
    SYMBOL_COUNT,
    active_operation,
    active_stdout,
    capture_count,
    change_patch,
    database_bytes,
    exact_degradation,
    fields,
    identity_resolved,
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
from corpus_cache import Pin, git, measure, pins
from mcp.shared.exceptions import McpError
from rift_test_client import (
    Client,
    Json,
    JsonObject,
    Server,
    array_value,
    object_value,
    require,
    string_value,
)

SEED = 34
POLL_SECONDS = 0.1
OBSERVATION_SECONDS = 60.0
CONFIGURATION = '[search.semantic]\ndisabled = true\n[logs]\npage_records = 5000\ncapture = "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info"\n'
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


class Corpus:
    """One disposable corpus checkout, its external evidence, and tested actions."""

    def __init__(
        self, pin: Pin, binary: Path, report: Path, case: str = "workspace"
    ) -> None:
        require(case in ("workspace", "stop"), f"unknown corpus case: {case}")
        require(case != "stop" or pin.name == "bun", "only bun has a stop case")
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
                "RUST_LOG": "rift=info,rift_mcp=debug,rift_server=debug,rift_index=info"
            },
        )

    async def run(self) -> None:
        """A timeout fails the suite after server cleanup writes its evidence."""
        started = time.monotonic()
        self.started = started
        status = "failed"
        failure = ""
        self.report.parent.mkdir(parents=True, exist_ok=True)
        try:
            async with asyncio.timeout(self.pin.seconds):
                with tempfile.TemporaryDirectory(
                    prefix=f"rift-corpus-{self.pin.name}-"
                ) as directory:
                    await self.tree(Path(directory).resolve())
                elapsed = time.monotonic() - started
                require(
                    elapsed <= self.pin.seconds,
                    f"{self.pin.name}: elapsed {elapsed:.3f}s exceeded {self.pin.seconds}s",
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
                await self.single_capture(client)
                if self.pin.name == "nextjs":
                    await self.symlinks(client)
                    await self.churn(client)
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
        """Issue #264: exercise 200 emitted addresses without changing their bytes."""
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
            body = content[start:end].decode()
            answer = await client.call(
                "replace_symbol", {"symbol": identity, "body": body}
            )
            identity_resolved(answer, identity)
            require(
                path.read_bytes() == content,
                f"byte-equal symbol replacement changed {path}",
            )
        self.record("identities", count=len(identities), seed=SEED)

    async def single_capture(self, client: Client) -> None:
        before = lexical_content(self.root)
        byte_sizes = database_bytes(self.root)
        prior = records(await client.resource("rift://logs/component/index"))
        last = max(
            (number(row["identity"], "record identity") for row in prior), default=0
        )
        started = time.monotonic()
        applied = await client.call(
            "patch", {"patch": change_patch(PROBE_PATH, "", PROBE_SOURCE)}
        )
        require(applied.get("status") == "applied", f"probe patch refused: {applied}")
        found = await observed(
            client,
            "rift://logs/component/index",
            lambda rows: any(
                row.get("operation") == "index.publish"
                and fields(row).get("trigger") == "rift_change"
                and number(row["identity"], "record identity") > last
                for row in rows
            ),
        )
        require(
            time.monotonic() - started <= OBSERVATION_SECONDS,
            "edit publication exceeded 60 seconds",
        )
        await self.await_probe(True)
        require(
            (self.root / PROBE_PATH).read_bytes() == PROBE_SOURCE.encode(),
            "published probe changed source bytes",
        )
        answer = await client.call("get_symbol", {"name": "corpus_probe"})
        hits = objects(answer, "hits")
        require(
            len(hits) == 1 and hits[0].get("source") == PROBE_SOURCE.removesuffix("\n"),
            f"newly published probe source differs: {hits}",
        )
        found = records(await client.resource("rift://logs/component/index"))
        captures = capture_count(found, last)
        require(
            lexical_content(self.root) == before,
            "one edit changed unrelated persisted lexical rows",
        )
        self.record(
            "capture",
            count=captures,
            source_bytes=len(PROBE_SOURCE.encode()),
            before=byte_sizes,
            after=database_bytes(self.root),
        )
        removed = await client.call(
            "patch", {"patch": change_patch(PROBE_PATH, PROBE_SOURCE, "")}
        )
        require(removed.get("status") == "applied", f"probe removal refused: {removed}")
        await self.await_probe(False)
        require(
            lexical_content(self.root) == before,
            "probe removal changed unrelated persisted lexical rows",
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

    async def churn(self, client: Client) -> None:
        path = self.root / PROBE_PATH
        completed: list[Json] = []

        def write(edit: int) -> None:
            path.write_text(
                f"pub fn corpus_probe() {{ let value = {edit}; }}\n",
                encoding="utf-8",
            )
            completed.append(edit)

        async def writer() -> None:
            for edit in range(1, self.pin.seconds // 2):
                await asyncio.sleep(2.0)
                write(edit)

        write(0)
        task = asyncio.create_task(writer())
        try:
            for read in range(READ_COUNT):
                answer = await client.call("search", {"query": "test", "limit": 1})
                warnings(answer)
                require(not task.done(), f"churn ended before read {read}")
            self.record(
                "churn",
                reads=READ_COUNT,
                edits=len(completed),
                temporarily_unavailable=0,
            )
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
            path.unlink(missing_ok=True)

    async def symlink_root(self) -> None:
        link = self.root.parent / "linked-workspace"
        link.symlink_to(self.root, target_is_directory=True)
        with self.server(link) as server:
            async with server.connect() as client:
                applied = await client.call(
                    "patch", {"patch": change_patch(PROBE_PATH, "", PROBE_SOURCE)}
                )
                require(
                    applied.get("status") == "applied",
                    f"symlink-root patch refused: {applied}",
                )
                moved = await client.call(
                    "move_file", {"from": PROBE_PATH, "to": "rift_corpus_moved.rs"}
                )
                require(
                    moved.get("status") == "applied",
                    f"symlink-root move refused: {moved}",
                )
                require(
                    (self.root / "rift_corpus_moved.rs").read_bytes()
                    == PROBE_SOURCE.encode(),
                    "move changed file bytes",
                )
                require(
                    not (self.root / PROBE_PATH).exists(), "move retained its old path"
                )
                removed = await client.call(
                    "patch",
                    {"patch": change_patch("rift_corpus_moved.rs", PROBE_SOURCE, "")},
                )
                require(
                    removed.get("status") == "applied", "moved probe removal refused"
                )
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            server.stop()
        self.record("symlink_root", move="applied")

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
                previous = ""
                for edit in range(15):
                    replacement = f"pub fn corpus_probe() {{ let value = {edit}; }}\n"
                    answer = await client.call(
                        "patch",
                        {"patch": change_patch(PROBE_PATH, previous, replacement)},
                    )
                    require(
                        answer.get("status") == "applied",
                        f"edit {edit} refused: {answer}",
                    )
                    previous = replacement
                no_failed_builds(
                    records(await client.resource("rift://logs/component/index"))
                )
            server.stop()
        (self.root / PROBE_PATH).unlink()
        self.record("stop", state="after_fifteen_edits", edits=15, process_gone=True)
        await self.stop_during("rebuild")
        await self.stop_during("history")

    async def stop_during(self, operation: str) -> None:
        """Observe through a second proxy while the operation occupies its HTTP connection."""
        started_ms = time.time_ns() // 1_000_000
        with self.server() as server:
            async with (
                server.connect() as client,
                server.connect(
                    log_path=server.log_path.with_suffix(".observer.mcp.log")
                ) as observer,
            ):
                found = await observed(
                    observer,
                    "rift://logs/component/index",
                    lambda rows: any(
                        row.get("operation") == "index.publish"
                        and fields(row).get("trigger") == "startup"
                        and number(row.get("recorded_at_ms"), "recorded timestamp")
                        >= started_ms
                        for row in rows
                    ),
                )
                last = max(
                    (number(row["identity"], "record identity") for row in found),
                    default=0,
                )
                log_offset = len(server.read_log())
                if operation == "rebuild":
                    (self.root / PROBE_PATH).write_text(PROBE_SOURCE)
                    pending = asyncio.create_task(
                        client.call("search", {"query": "corpus_probe"})
                    )
                    wanted = "index.build"
                else:
                    pending = asyncio.create_task(
                        client.call(
                            "get_symbol",
                            {"name": "test", "include": ["history"], "limit": 200},
                        )
                    )
                    wanted = "get_symbol"
                try:
                    found = await observed(
                        observer,
                        "rift://logs/component/index",
                        lambda rows: any(
                            row.get("operation") == wanted
                            and fields(row).get("phase") == "start"
                            and number(row["identity"], "record identity") > last
                            for row in rows
                        ),
                    )
                    latest = active_operation(
                        found, operation, last, not pending.done()
                    )
                    start = next(row for row in found if row.get("identity") == latest)
                    epoch = (
                        str(fields(start).get("epoch"))
                        if operation == "rebuild"
                        else None
                    )
                    evidence = active_stdout(
                        server.read_log()[log_offset:], operation, epoch
                    )
                    await asyncio.to_thread(server.stop)
                    self.record(
                        "stop",
                        state=f"mid_{operation}",
                        start=latest,
                        stderr=evidence,
                        process_gone=True,
                    )
                finally:
                    pending.cancel()
                    await asyncio.gather(pending, return_exceptions=True)
        (self.root / PROBE_PATH).unlink(missing_ok=True)


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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("sync", "measure", "test"))
    parser.add_argument("name", nargs="?", choices=("bun", "nextjs", "fastapi"))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--case", choices=("workspace", "stop"), default="workspace")
    arguments = parser.parse_args()
    selected = pins()
    if arguments.name:
        selected = {arguments.name: selected[arguments.name]}
    for pin in selected.values():
        if arguments.command == "sync":
            print(pin.sync(), flush=True)
        elif arguments.command == "measure":
            print(
                json.dumps(
                    dataclasses.asdict(
                        measure(git(pin.cache, "ls-tree", "-r", "-l", "-z", pin.commit))
                    )
                )
            )
        else:
            require(
                arguments.name is not None and arguments.binary is not None,
                "test requires one corpus name and --binary",
            )
            report = arguments.report or Path(
                f"target/test-results/corpus/{pin.name}/{arguments.case}/report.json"
            )
            asyncio.run(Corpus(pin, arguments.binary, report, arguments.case).run())


if __name__ == "__main__":
    main()
