"""Observe one bounded set of modifications in the disposable Bun corpus."""

from __future__ import annotations

import asyncio
import hashlib
import re
import sqlite3
import time
from contextlib import closing
from datetime import UTC, datetime
from pathlib import Path
from typing import Protocol

from rift_dev.corpus_cache import git
from rift_dev.log_records import lines
from rift_dev.perf.edit import (
    PATHS,
    declaration,
    log_since,
    read_edit,
    wait_publication,
)
from rift_dev.perf.stages import parse_stages, stage_table
from rift_dev.rift_test_client import (
    Client,
    Json,
    JsonObject,
    Server,
    ToolFailure,
    require,
)

FILE_COUNT = 50
MARKER_PATH = PATHS["bun"]


class Record(Protocol):
    def __call__(self, action: str, **values: Json) -> None: ...


def connection(root: Path) -> sqlite3.Connection:
    db = sqlite3.connect((root / ".rift/index").as_uri() + "?mode=ro", uri=True)
    db.execute("BEGIN")
    return db


def source_paths(root: Path, file_count: int) -> list[str]:
    """Spread the formatting edits over tracked TypeScript files admitted to storage."""
    require(file_count in (1, FILE_COUNT), "unsupported Bun edit file count")
    tracked = git(root, "ls-files", "-z", "--", "*.ts").output_bytes()
    with closing(connection(root)) as db:
        admitted = {
            name
            for (name,) in db.execute(
                "SELECT path FROM lexical_files WHERE path LIKE '%.ts'"
            )
        }
    regular = [
        name
        for name in tracked.decode().split("\0")
        if name in admitted
        and not (root / name).is_symlink()
        and (root / name).is_file()
    ]
    require(MARKER_PATH in regular, "marker file is missing from the pinned index")
    regular.remove(MARKER_PATH)
    count = file_count - 1
    require(len(regular) >= count, "too few indexed TypeScript files")
    return [regular[i * len(regular) // count] for i in range(count)] + [MARKER_PATH]


def changed_source(name: str, source: bytes, declaration_body: bytes) -> bytes:
    if name == MARKER_PATH:
        return source + declaration_body
    if source.startswith(b"#!"):
        first_end = source.find(b"\n")
        if first_end == -1:
            return source + b"\n\n"
        return source[: first_end + 1] + b"\n" + source[first_end + 1 :]
    return b"\n" + source


def digests(payload: dict[str, bytes]) -> dict[str, str]:
    return {
        name: hashlib.sha256(content).hexdigest() for name, content in payload.items()
    }


async def wait_files(
    server: Server, expected: dict[str, str], revision: str | None = None
) -> tuple[float, str]:
    """Read the revision and every touched digest in the same SQLite snapshot."""
    params = ",".join("?" for _ in expected)
    while True:
        server.check_running()
        with closing(connection(server.root)) as db:
            state = db.execute(
                "SELECT tree_revision FROM lexical_index_state"
            ).fetchone()
            if state and state[0] and (revision is None or state[0] == revision):
                actual = (
                    {
                        name: digest.hex()
                        for name, digest in db.execute(
                            f"SELECT path,digest FROM lexical_files WHERE path IN ({params})",
                            tuple(expected),
                        )
                    }
                    if expected
                    else {}
                )
                if actual == expected:
                    return time.perf_counter(), state[0]
        await asyncio.sleep(0.025)


def snapshot(root: Path, marker: str) -> JsonObject:
    with closing(connection(root)) as db:
        return {
            "revision": db.execute(
                "SELECT tree_revision FROM lexical_index_state"
            ).fetchone()[0],
            "rows": {
                table: db.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                for table in (
                    "lexical_files",
                    "lexical_documents",
                    "documentation_sources",
                )
            },
            "marker_rows": db.execute(
                "SELECT COUNT(*) FROM lexical_documents WHERE path=? AND name=?",
                (MARKER_PATH, marker),
            ).fetchone()[0],
        }


def publication_report(log: str, revision: str, written: datetime) -> JsonObject | None:
    """Join filesystem publications to successful build closes by epoch."""
    builds = {}
    published: list[Json] = []
    accepted: list[datetime] = []
    for row in lines(log):
        if row.label("trigger") != "filesystem":
            continue
        epoch = row.label("epoch")
        if (
            row.nested_name == "index.build"
            and row.closes()
            and row.label("outcome") == "ok"
        ):
            builds[epoch] = (row.label("tree_revision"), row.label("changed_count"))
        if row.is_message("index snapshot published"):
            tree, changed = builds.get(epoch, ("", ""))
            seconds = (row.time - written).total_seconds()
            published.append(
                {
                    "epoch": epoch,
                    "revision": tree,
                    "changed_count": changed,
                    "seconds": seconds,
                }
            )
            if tree == revision:
                accepted.append(row.time)
    if not accepted:
        return None
    final = accepted[-1]
    stages = stage_table(parse_stages(log), written, final)
    return {
        "publications": published,
        "final_matching_publication_seconds": (final - written).total_seconds(),
        "stages": [
            {"stage": row.stage, "seconds": row.seconds, "evidence": row.evidence}
            for row in stages
        ],
    }


async def observe_publication(
    server: Server, offset: int, revision: str, written: datetime
) -> JsonObject:
    trace = publication_report(log_since(server.log_path, offset), revision, written)
    if trace is None:
        records = await asyncio.to_thread(
            server.read_logs,
            [
                "server",
                "logs",
                "--tail",
                "all",
                "--since",
                written.isoformat(timespec="milliseconds"),
            ],
        )
        trace = publication_report(records, revision, written)
    if trace is None:
        raise AssertionError(
            "no published filesystem build matches the settled revision"
        )
    return trace


async def run_burst(
    server: Server,
    client: Client,
    record: Record,
    file_count: int,
    launched: float,
    *,
    observe_restore: bool = False,
) -> None:
    root = server.root
    before_map = await client.resource("rift://map")
    initial = before_map["revision"]
    require(isinstance(initial, str), "map has no tree revision")
    lexical_ready, _ = await wait_files(server, {}, str(initial))
    record("burst_lexical_ready", seconds=lexical_ready - launched, revision=initial)
    paths = source_paths(root, file_count)
    original = {name: (root / name).read_bytes() for name in paths}
    marker, body, symbol = declaration("bun", file_count)
    changed = {
        name: changed_source(name, content, body) for name, content in original.items()
    }
    before, after = digests(original), digests(changed)
    await wait_files(server, before, str(initial))
    initial_rows = snapshot(root, marker)
    record(
        "burst_inputs",
        original_sha256=dict(before),
        changed_sha256=dict(after),
        initial=initial_rows,
    )
    require(initial_rows["marker_rows"] == 0, "probe declaration already exists")
    offset = server.log_path.stat().st_size
    written = datetime.now(UTC)
    started = time.perf_counter()
    try:
        for name, content in changed.items():
            (root / name).write_bytes(content)
        write_seconds = time.perf_counter() - started
        record("burst_write", written_at=written.isoformat(), seconds=write_seconds)
        async with asyncio.TaskGroup() as group:
            files = group.create_task(wait_files(server, after))
            publication = group.create_task(
                wait_publication(server, offset, "filesystem")
            )
            await asyncio.wait(
                (publication, files), return_when=asyncio.FIRST_COMPLETED
            )
            # A bounded stderr stream can end before the publication. The committed
            # file digests also allow the first read to start in that case.
            start_signal = "publication" if publication.done() else "all_digests"
            publication.cancel()
            while True:
                persisted_at_start = files.done()
                try:
                    answer = await read_edit(client, symbol, marker, file_count)
                except ToolFailure as failure:
                    if (
                        failure.identity != "rift.server.read_symbol_not_found"
                        or persisted_at_start
                    ):
                        raise
                    await asyncio.sleep(0.025)
                    continue
                observed = time.perf_counter()
                ready, revision = await files
                full = re.fullmatch(
                    r"rift://revision/tree/([0-9a-f]{64})", str(answer["revision"])
                )
                require(full is not None, "facets returned an invalid tree revision")
                if full and full[1][:8] == revision:
                    if not persisted_at_start:
                        # The first read can finish before all digests commit. An
                        # independent current read confirms the complete revision.
                        confirmed = await read_edit(client, symbol, marker, file_count)
                        if confirmed["revision"] != answer["revision"]:
                            continue
                    break
        require(revision != initial, "burst left the initial revision current")
        trace = await observe_publication(server, offset, revision, written)
        changed_rows = snapshot(root, marker)
        current_map = await client.resource("rift://map")
        record(
            "burst",
            settled=changed_rows,
            map_revision=current_map["revision"],
            facets_start_signal=start_signal,
            files=len(after),
            write_seconds=write_seconds,
            all_digests_seconds=ready - started,
            facets_seconds=observed - started,
            facets=answer,
            **trace,
        )
        require(
            current_map["revision"] == revision == changed_rows["revision"],
            "changed map and index revisions differ",
        )
        require(
            changed_rows["marker_rows"] == 1,
            "changed index did not persist exactly one probe declaration",
        )
    finally:
        restore_written = datetime.now(UTC)
        restore_started = time.perf_counter()
        try:
            restore_offset = server.log_path.stat().st_size if observe_restore else 0
        finally:
            for name, content in original.items():
                (root / name).write_bytes(content)
        if observe_restore:
            record(
                "burst_restore_write",
                written_at=restore_written.isoformat(),
                seconds=time.perf_counter() - restore_started,
            )
    restored_at, _ = await wait_files(server, before, str(initial))
    convergence = {}
    if observe_restore:
        try:
            async with asyncio.timeout(5):
                await wait_publication(server, restore_offset, "filesystem")
        except TimeoutError:
            # A capped stderr stream leaves the committed publication in server records.
            pass
        convergence = await observe_publication(
            server, restore_offset, str(initial), restore_written
        )
        convergence["all_digests_seconds"] = restored_at - restore_started
    restored = snapshot(root, marker)
    restored_map = await client.resource("rift://map")
    on_disk = digests({name: (root / name).read_bytes() for name in paths})
    record(
        "burst_restore",
        restored=restored,
        disk_sha256=dict(on_disk),
        map_equal=before_map == restored_map,
        **convergence,
    )
    require(on_disk == before, "restored source digests differ")
    require(
        initial_rows == restored, "restored index counts, marker or revision differ"
    )
    require(restored_map == before_map, "restored workspace map differs")
