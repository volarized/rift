"""Fail-closed checks over corpus answers and persisted lexical content."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import random
import re
import sqlite3
from contextlib import closing
from pathlib import Path
from urllib.parse import unquote

from rift_dev.rift_test_client import (
    Json,
    JsonObject,
    array_value,
    object_value,
    require,
    string_value,
)

SYMBOL_COUNT = 200
SYMBOL_POOL_MAX = 4000
READ_COUNT = 50
SOURCE_WARNINGS_MAX = 8
LEXICAL_UNITS_MAX = 1_000_000
LEXICAL_BYTES_MAX = 512 * 1024 * 1024
PROBE_PATH = "rift_corpus_probe.rs"
PROBE_SOURCE = "pub fn corpus_probe(){}\n"
MAP_MODULES_MAX = 100_000
# The span startup opens while it reads the manifests and lockfiles, and the warning each
# unread input is reported under.
CONTEXT_SPAN = "dependency.context"
CONTEXT_DEGRADED = "dependency context degraded"
# `[search.text] max_chunk` at its default. The corpus writes no `[search.text]` table, so
# `large_files` stays `split`: a file past this many bytes is indexed as whole-line chunks
# of at most this size, and its text past the first chunk still answers search.
TEXT_CHUNK_BYTES = 1 << 20
FILE_PREFIX = "rift://file/"
# A run of identifier bytes, which a `pattern` matches literally with no escaping.
PATTERN_TOKEN = re.compile(rb"[A-Za-z_][A-Za-z0-9_]{7,63}")
# The characters the Rust `regex` crate's syntax reserves, each escaped with a backslash
# to match itself (`regex_syntax::is_meta_character`).
PATTERN_META = re.compile(r"[\\.+*?()|\[\]{}^$#&\-~]")
# Characters one `pattern` may hold (`SEARCH_PATTERN_CHARS_MAX`).
PATTERN_CHARS_MAX = 1_024
# The record each build writes for a file it holds as text its syntax provider does not
# parse; a file it leaves out gets `file left out of the index` instead.
HELD_UNPARSED_RECORD = "file held unparsed in the index"
# The record the history task writes as each store batch starts, with `pending`, the
# commits its fill plan has not analyzed yet. A span reaches server output only when it
# closes, so this record is what shows a batch in flight.
HISTORY_BATCH_STARTED = (
    'history batch started component="history" operation="history.batch" phase="start"'
)
HISTORY_BATCH_CLOSED = re.compile(
    r"history\.batch\{[^}]*\}: rift_mcp::history: close\b"
)


def map_paths(answer: JsonObject) -> set[str]:
    """Read exact paths from the map's named fields and escaped symbol identities."""
    string_value(answer.get("revision"), "map revision")
    object_value(answer.get("pagination"), "map pagination")
    found = {
        string_value(value, "map documentation path")
        for value in array_value(answer.get("docs", []), "map documentation")
    }
    pending = [
        object_value(value, "map module")
        for value in array_value(answer.get("modules", []), "map modules")
    ]
    visited = 0
    while pending:
        module = pending.pop()
        visited += 1
        require(visited <= MAP_MODULES_MAX, "map module count exceeded 100000")
        found.add(string_value(module.get("path"), "map module path"))
        children = array_value(module.get("children", []), "map module children")
        require(
            visited + len(pending) + len(children) <= MAP_MODULES_MAX,
            "map module count exceeded 100000",
        )
        pending.extend(object_value(child, "map module") for child in children)
    symbols = list(array_value(answer.get("entry_points", []), "map entry points"))
    symbols.extend(
        object_value(value, "map hub").get("symbol")
        for value in array_value(answer.get("hubs", []), "map hubs")
    )
    for value in symbols:
        identity = string_value(value, "map symbol identity")
        prefix = "rift://symbol/"
        require(identity.startswith(prefix), f"invalid map symbol identity: {identity}")
        language, separator, tail = identity.removeprefix(prefix).partition("/")
        require(
            bool(language and separator), f"map symbol lost its language: {identity}"
        )
        encoded, separator, name = tail.rpartition("/")
        require(
            bool(encoded and separator and name),
            f"map symbol lost its path or name: {identity}",
        )
        require(
            re.search(r"%(?![0-9A-Fa-f]{2})", encoded) is None,
            f"map symbol path has an invalid percent escape: {identity}",
        )
        found.add(unquote(encoded, errors="strict"))
    return found


def symbol_pool(candidates: list[JsonObject]) -> dict[str, JsonObject]:
    """Deduplicate overlapping search pages while checking each emitted language."""
    require(len(candidates) <= SYMBOL_POOL_MAX, "symbol pool exceeded 4000 hits")
    identities: dict[str, JsonObject] = {}
    for candidate in candidates:
        symbol = object_value(
            object_value(candidate.get("hit"), "search hit").get("symbol"), "symbol"
        )
        identity = string_value(symbol.get("id"), "symbol identity")
        language = string_value(symbol.get("language"), "symbol language")
        if identity in identities:
            previous = object_value(
                object_value(identities[identity].get("hit"), "search hit").get(
                    "symbol"
                ),
                "symbol",
            )
            require(
                previous.get("language") == language,
                f"one identity named two languages: {identity}",
            )
        else:
            identities[identity] = candidate
    return identities


def language_counts(candidates: list[JsonObject]) -> JsonObject:
    counts: dict[str, int] = {}
    for candidate in symbol_pool(candidates).values():
        symbol = object_value(
            object_value(candidate.get("hit"), "search hit").get("symbol"), "symbol"
        )
        language = string_value(symbol.get("language"), "symbol language")
        counts[language] = counts.get(language, 0) + 1
    return {language: count for language, count in sorted(counts.items())}


def sample_symbols(
    candidates: list[JsonObject], count: int, seed: int, required: set[str]
) -> list[JsonObject]:
    """Sample across emitted languages without losing rare languages or repeating IDs."""
    require(0 < count <= SYMBOL_COUNT, "symbol sample count must be within 1..200")
    pool = symbol_pool(candidates)
    require(len(pool) >= count, f"symbol pool has fewer than {count} unique identities")
    groups: dict[str, list[JsonObject]] = {}
    for identity in sorted(pool):
        candidate = pool[identity]
        symbol = object_value(
            object_value(candidate.get("hit"), "search hit").get("symbol"), "symbol"
        )
        language = string_value(symbol.get("language"), "symbol language")
        groups.setdefault(language, []).append(candidate)
    require(
        required.issubset(groups),
        f"symbol pool lost required languages: {sorted(required - groups.keys())}",
    )
    require(len(groups) <= count, "symbol count cannot represent every pooled language")
    generator = random.Random(seed)
    for language in sorted(groups):
        generator.shuffle(groups[language])
    selected: list[JsonObject] = []
    for _ in range(count):
        for language in sorted(groups):
            if groups[language]:
                selected.append(groups[language].pop())
            if len(selected) == count:
                return selected
    raise AssertionError("symbol pool exhausted before the requested count")


def records(answer: JsonObject) -> list[JsonObject]:
    """Require complete log pages so a missing record cannot turn into a pass."""
    require("unavailable" not in answer, f"log store unavailable: {answer}")
    values = array_value(answer.get("records"), "log records")
    require(
        len(values) < 5000, "log page reached its bound; evidence may have been lost"
    )
    return [object_value(value, "log record") for value in values]


def fields(record: JsonObject) -> JsonObject:
    return object_value(record.get("fields"), "log fields")


def number(value: object, context: str) -> int:
    """Require an integer before comparing counts or using byte offsets."""
    if type(value) is not int or value < 0:
        raise AssertionError(
            f"{context}: expected a nonnegative integer, received {value!r}"
        )
    return value


def warnings(answer: JsonObject) -> list[JsonObject]:
    values = array_value(answer.get("warnings", []), "warnings")
    found = [object_value(value, "warning") for value in values]
    source = [
        warning for warning in found if warning.get("code") == "source_unavailable"
    ]
    named = [warning for warning in source if warning.get("unit") is not None]
    summaries = [warning for warning in source if warning.get("unit") is None]
    require(
        len(named) <= SOURCE_WARNINGS_MAX and len(summaries) <= 1,
        f"source warnings exceeded {SOURCE_WARNINGS_MAX} named and one summary: {source}",
    )
    require(
        not summaries
        or (len(named) == SOURCE_WARNINGS_MAX and source[-1] == summaries[0]),
        f"source warning summary must follow {SOURCE_WARNINGS_MAX} named entries: {source}",
    )
    return found


def token_past_chunk(data: bytes) -> tuple[str, int]:
    """The first token that occurs nowhere in the file's first chunk, and its first offset.

    A `pattern` search for it can only match text the index holds past that chunk, so its
    first hit lands exactly at the returned offset. Each candidate costs one scan of the
    first chunk, so the file's token count bounds the work.
    """
    require(
        len(data) > TEXT_CHUNK_BYTES,
        f"the file holds {len(data)} bytes, within one {TEXT_CHUNK_BYTES}-byte chunk",
    )
    for found in PATTERN_TOKEN.finditer(data, TEXT_CHUNK_BYTES):
        token = found.group()
        # An occurrence starting inside the first chunk may run past its end.
        if data.find(token, 0, TEXT_CHUNK_BYTES - 1 + len(token)) == -1:
            return token.decode("ascii"), data.find(token, TEXT_CHUNK_BYTES)
    raise AssertionError(f"no token first occurs past byte {TEXT_CHUNK_BYTES}")


def last_line_pattern(data: bytes) -> tuple[str, int]:
    """A `pattern` matching the file's last nonblank line literally, and its first offset.

    Each character the `regex` syntax reserves is escaped, so the pattern's first hit
    lands at the first occurrence of that line's text.
    """
    lines = [line for line in data.splitlines() if line.strip()]
    require(bool(lines), "the file holds no nonblank line")
    line = lines[-1].decode("utf-8")
    pattern = PATTERN_META.sub(lambda found: "\\" + found.group(), line)
    require(
        len(pattern) <= PATTERN_CHARS_MAX,
        f"the last line escapes to {len(pattern)} characters, past {PATTERN_CHARS_MAX}",
    )
    return pattern, data.find(lines[-1])


def named_paths(warning: JsonObject) -> set[str]:
    """The project paths one warning names through its `unit` or `files`."""
    identities: list[Json] = [warning["unit"]] if "unit" in warning else []
    identities.extend(array_value(warning.get("files", []), "warning files"))
    found: set[str] = set()
    for value in identities:
        identity = string_value(value, "warning file")
        require(identity.startswith(FILE_PREFIX), f"invalid file identity: {identity}")
        found.add(unquote(identity.removeprefix(FILE_PREFIX), errors="strict"))
    return found


def chunked_answer(answer: JsonObject, path: str, size: int, offset: int) -> list[str]:
    """Require a `pattern` answer to hit `path` at `offset`, the file held at full size.

    Under the default `large_files = "split"` no file is left out of the text index, so no
    warning may count one as skipped. Returns the codes of the warnings naming `path`.
    """
    hits = array_value(answer.get("results"), "pattern hits")
    require(bool(hits), f"{path}: no pattern hit at byte {offset}: {answer}")
    first = object_value(hits[0], "pattern hit")
    target = object_value(first.get("hit"), "pattern hit target")
    span = object_value(first.get("range"), "pattern hit range")
    observed = (first.get("path"), target.get("target"), target.get("size"))
    require(
        observed == (path, "file", size),
        f"expected a file hit on {path} holding {size} bytes, received {observed}",
    )
    start = number(span.get("start"), "pattern hit start")
    require(start == offset, f"{path}: first hit at byte {start}, expected {offset}")
    named: list[str] = []
    for warning in warnings(answer):
        code = string_value(warning.get("code"), "warning code")
        require(code != "large_file_skipped", f"split skipped a large file: {warning}")
        if path in named_paths(warning):
            named.append(code)
    return named


def build_records(found: list[JsonObject], path: str) -> list[str]:
    """The messages of the index-build records naming `path`, in page order."""
    return [
        string_value(row.get("message"), "log message")
        for row in found
        if row.get("operation") == "index.build" and fields(row).get("path") == path
    ]


def lexical_breach(answer: JsonObject, maximum: int) -> int:
    """Require the exact rendered lexical bound and a count beyond that maximum."""
    found = [
        entry
        for entry in warnings(answer)
        if entry.get("code") == "lexical_ranking_unavailable"
    ]
    require(
        len(found) == 1, "lexical overflow must warn exactly once and keep publication"
    )
    detail = string_value(found[0].get("detail"), "lexical warning detail")
    matched = re.search(
        rf"lexical index received more units than its accepted limit of {maximum}: "
        r"field units_max, observed ([0-9]+); "
        r"resend the same request after a short delay$",
        detail,
    )
    if matched is None:
        raise AssertionError(f"lexical warning lost its exact bound: {detail}")
    observed = int(matched.group(1))
    require(observed > maximum, f"lexical observed {observed} did not exceed {maximum}")
    return observed


def no_failed_builds(found: list[JsonObject]) -> None:
    failures = [
        record for record in found if record.get("message") == "index rebuild failed"
    ]
    require(not failures, f"index build failed: {failures}")


def exact_degradation(found: list[JsonObject], expected: str | None) -> None:
    reasons = [
        (
            string_value(fields(record).get("resolver"), "dependency resolver"),
            string_value(fields(record).get("reason"), "dependency reason"),
        )
        for record in found
        if record.get("message") == CONTEXT_DEGRADED
    ]
    drops = sorted(
        (resolver, reason)
        for resolver, reason in reasons
        if "package.json manifests were not read" in reason
    )
    required = [] if expected is None else [("bun", expected), ("npm", expected)]
    require(
        drops == required,
        f"manifest degradation: expected {required!r}, observed {drops!r}",
    )


def active_stdout(output: str, operation: str, epoch: str | None) -> str:
    """Require synchronous start without a later matching completion record."""
    require(
        output.endswith("\n"), f"{operation}: stderr ends with an incomplete record"
    )
    if operation == "history":
        batch = open_history_batch(output)
        if batch is None:
            raise AssertionError(
                "history: no store batch with pending commits is open on stderr"
            )
        return batch
    rows = output.splitlines()
    marker = (
        'index capture started component="index" operation="index.build" phase="start"'
    )
    started = [index for index, row in enumerate(rows) if marker in row]
    require(bool(started), f"{operation}: synchronous start record is absent")
    start = started[-1]
    captured = re.search(r"\bepoch=(\d+)(?:[ }]|$)", rows[start])
    if captured is None:
        raise AssertionError("rebuild start has no epoch")
    observed_epoch = captured.group(1)
    require(
        observed_epoch != "0" and (epoch is None or observed_epoch == epoch),
        "rebuild start must name the current epoch after startup",
    )
    epoch_pattern = rf"\bepoch={re.escape(observed_epoch)}(?:[ }}]|$)"
    for row in rows[start + 1 :]:
        closed = "index.build{" in row and "rift_mcp::validation: close" in row
        completed = (
            closed and re.search(epoch_pattern, row) is not None
        ) or 'operation="index.publish"' in row
        require(not completed, f"{operation} completed before stop on stderr: {row}")
    return rows[start]


def open_history_batch(output: str) -> str | None:
    """The newest history store batch start with pending commits and no close record.

    The history task runs one batch at a time, so a close record after the newest
    start closes that batch. A batch that starts with no pending commit analyzes none.
    """
    rows = output.splitlines()
    started = [index for index, row in enumerate(rows) if HISTORY_BATCH_STARTED in row]
    if not started:
        return None
    start = started[-1]
    pending = re.search(r"\bpending=(\d+)(?: |$)", rows[start])
    if pending is None or int(pending.group(1)) == 0:
        return None
    if any(HISTORY_BATCH_CLOSED.search(row) for row in rows[start + 1 :]):
        return None
    return rows[start]


@dataclasses.dataclass(frozen=True)
class LexicalContent:
    """Exact stored rows, excluding the probe path."""

    units: int
    bytes: int
    digest: str


def lexical_content(root: Path) -> LexicalContent:
    """Hash every ranking column of the ordered document rows.

    The schema is owned by rift-index/src/lexical.rs, whose fifth migration replaced
    `lexical_units` with `lexical_documents`. Diagnostics and the revision row change on
    each publication; unrelated document rows must not.
    """
    digest = hashlib.sha256()
    count = size = 0
    database = root / ".rift" / "db"
    with closing(
        sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=5.0)
    ) as connection:
        cursor = connection.execute(
            "SELECT identity,path,kind,digest,byte_length,byte_offset,name,qualified_name,"
            "identifier_terms,signature,documentation,file_content "
            "FROM lexical_documents WHERE path != ? ORDER BY identity",
            (PROBE_PATH,),
        )
        for row in cursor:
            count += 1
            encoded = json.dumps(
                row, ensure_ascii=False, separators=(",", ":")
            ).encode()
            size += len(encoded)
            require(
                count <= LEXICAL_UNITS_MAX,
                "lexical row count exceeded configured maximum",
            )
            require(
                size <= LEXICAL_BYTES_MAX * 4,
                "lexical row bytes exceeded bounded content and identities",
            )
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)
    require(count > 0, "lexical content is empty; the index did not publish")
    return LexicalContent(count, size, digest.hexdigest())


def database_bytes(root: Path) -> JsonObject:
    return {
        name: path.stat().st_size
        for name in ("db", "db-wal", "db-shm")
        if (path := root / ".rift" / name).is_file()
    }


def probe_units(root: Path) -> int:
    """Count only the probe's persisted units after the lexical lane commits."""
    database = root / ".rift" / "db"
    with closing(
        sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=5.0)
    ) as connection:
        row = connection.execute(
            "SELECT COUNT(*) FROM lexical_documents WHERE path = ?", (PROBE_PATH,)
        ).fetchone()
    require(row is not None, "lexical probe count returned no row")
    return number(row[0], "lexical probe units")


def churn_answer(
    tool: str, answer: JsonObject, sources: list[str], identity: str | None
) -> tuple[str, str]:
    """Require one probe declaration with source from a complete authored revision."""
    warnings(answer)
    if tool == "nodes":
        nodes = array_value(answer.get("nodes"), "probe nodes")
        excerpts = array_value(answer.get("source"), "probe node sources")
        require(len(nodes) == len(excerpts), "nodes lost matching source excerpts")
        matches = [
            (object_value(node, "probe node"), excerpt)
            for node, excerpt in zip(nodes, excerpts, strict=True)
            if object_value(node, "probe node").get("symbol") == identity
        ]
        require(len(matches) == 1, "nodes lost probe declaration")
        hit, excerpt = matches[0]
        found = string_value(hit.get("symbol"), "probe identity")
    else:
        rows = array_value(
            answer.get("results" if tool == "search" else "hits"), "probe hits"
        )
        require(len(rows) == 1, f"{tool} lost probe declaration")
        hit = object_value(rows[0], "probe hit")
        owner = object_value(hit.get("hit"), "search hit") if tool == "search" else hit
        symbol = object_value(owner.get("symbol"), "probe symbol")
        found = string_value(symbol.get("id"), "probe identity")
        require(hit.get("path") == PROBE_PATH, f"{tool} returned another probe path")
        excerpt = hit.get("source")
    require(identity is None or found == identity, f"{tool} changed probe identity")
    source = string_value(excerpt, "probe source")
    require(
        source in sources,
        f"{tool} returned source outside the expected revisions: {source}",
    )
    span = object_value(hit.get("range"), "probe range")
    require(
        span.get("start") == 0 and span.get("end") == len(source.encode()),
        f"{tool} probe range disagrees with source",
    )
    return found, source
