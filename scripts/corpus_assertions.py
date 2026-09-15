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

from rift_test_client import (
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
        rf"\bfield units_max, observed ([0-9]+), maximum {maximum}(?:;|$)", detail
    )
    if matched is None:
        raise AssertionError(f"lexical warning lost its exact bound: {detail}")
    observed = int(matched.group(1))
    require(observed > maximum, f"lexical observed {observed} did not exceed {maximum}")
    return observed


def identity_resolved(answer: JsonObject, symbol: str) -> None:
    """Issue #264: a read identity must reach change resolution."""
    status = answer.get("status")
    require(
        status in ("applied", "refused", "unchanged"),
        f"{symbol}: unknown change outcome {answer}",
    )
    if status != "refused":
        return
    conditions = array_value(answer.get("preconditions"), "change preconditions")
    require(
        bool(conditions) or answer.get("reason") != "unmet_precondition",
        f"{symbol}: missing refused precondition",
    )
    for value in conditions:
        condition = object_value(value, "change precondition")
        missing = (
            condition.get("kind") == "target_exists"
            and condition.get("status") == "failed"
        )
        require(
            not missing,
            f"read identity cannot be addressed by replace_symbol: {symbol}: {answer}",
        )


def no_failed_builds(found: list[JsonObject]) -> None:
    failures = [
        record for record in found if record.get("message") == "index rebuild failed"
    ]
    require(not failures, f"index build failed: {failures}")


def capture_count(found: list[JsonObject], after: int) -> int:
    """Count completed source captures and publications after the recorded edit boundary."""
    recent = [
        row for row in found if number(row["identity"], "record identity") > after
    ]
    captures = [
        row
        for row in recent
        if row.get("message") == "index.build"
        and row.get("target") == "rift_server::read"
        and fields(row).get("span") == "closed"
    ]
    publications = [
        row
        for row in recent
        if row.get("operation") == "index.publish"
        and fields(row).get("trigger") == "rift_change"
    ]
    require(
        len(captures) == 1 and len(publications) == 1,
        f"one edit must capture and publish once: {recent}",
    )
    require(
        fields(captures[0]).get("changed_count") == "1",
        f"capture lost changed_count: {captures}",
    )
    required = {"files_count", "tree_revision", "outcome"}
    require(
        required.issubset(fields(captures[0])), f"capture lost named fields: {captures}"
    )
    require(fields(captures[0]).get("outcome") == "ok", f"capture failed: {captures}")
    return len(captures)


def exact_degradation(found: list[JsonObject], expected: str | None) -> None:
    reasons = [
        (
            string_value(fields(record).get("resolver"), "dependency resolver"),
            string_value(fields(record).get("reason"), "dependency reason"),
        )
        for record in found
        if record.get("message") == "dependency resolution degraded"
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


def active_operation(
    found: list[JsonObject], operation: str, after: int, pending: bool
) -> int:
    """Require entered work without a later matching completion record."""
    wanted = "index.build" if operation == "rebuild" else "get_symbol"
    started = [
        row
        for row in found
        if row.get("operation") == wanted
        and fields(row).get("phase") == "start"
        and number(row.get("identity"), "record identity") > after
    ]
    require(bool(started), f"{operation}: no start record after the request")
    start = max(started, key=lambda row: number(row.get("identity"), "record identity"))
    identity = number(start.get("identity"), "record identity")
    for row in found:
        if number(row.get("identity"), "record identity") <= identity:
            continue
        if operation == "rebuild":
            same_epoch = fields(row).get("epoch") == fields(start).get("epoch")
            closed = (
                same_epoch
                and row.get("message") == "index.build"
                and fields(row).get("span") == "closed"
            )
            completed = closed or row.get("operation") == "index.publish"
        else:
            completed = (
                row.get("operation") == wanted and fields(row).get("span") == "closed"
            )
        require(not completed, f"{operation} completed before stop: {row}")
    require(operation == "rebuild" or pending, "history request completed before stop")
    return identity


def active_stdout(output: str, operation: str, epoch: str | None) -> str:
    """Check synchronous stderr so the persisted log drain cannot hide completion."""
    require(
        output.endswith("\n"), f"{operation}: stderr ends with an incomplete record"
    )
    rows = output.splitlines()
    message = (
        "index capture started" if operation == "rebuild" else "symbol history started"
    )
    started = [index for index, row in enumerate(rows) if message in row]
    require(bool(started), f"{operation}: synchronous start record is absent")
    start = started[-1]
    epoch_pattern = rf"\bepoch={re.escape(epoch or '')}(?:[ }}]|$)"
    if operation == "rebuild":
        require(
            epoch is not None and re.search(epoch_pattern, rows[start]) is not None,
            "rebuild stderr and persisted start epochs differ",
        )
    for row in rows[start + 1 :]:
        if operation == "rebuild":
            closed = "index.build{" in row and "rift_mcp::validation: close" in row
            completed = (
                closed and re.search(epoch_pattern, row) is not None
            ) or 'operation="index.publish"' in row
        else:
            completed = (
                "get_symbol{" in row
                and 'phase="history"' in row
                and "rift_server::history: close" in row
            )
        require(not completed, f"{operation} completed before stop on stderr: {row}")
    return rows[start]


@dataclasses.dataclass(frozen=True)
class LexicalContent:
    """Exact stored rows, excluding the one path the edit changes."""

    units: int
    bytes: int
    digest: str


def lexical_content(root: Path) -> LexicalContent:
    """Hash ordered source rows through SQLite's read-only connection.

    The schema is owned by rift-index/src/lexical.rs. Diagnostics and the
    revision row change on each publication; unrelated source rows must not.
    """
    digest = hashlib.sha256()
    count = size = 0
    database = root / ".rift" / "db"
    with closing(
        sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=5.0)
    ) as connection:
        cursor = connection.execute(
            "SELECT identity,path,kind,name,byte_length,content FROM lexical_units "
            "WHERE path != ? ORDER BY identity",
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
            "SELECT COUNT(*) FROM lexical_units WHERE path = ?", (PROBE_PATH,)
        ).fetchone()
    require(row is not None, "lexical probe count returned no row")
    return number(row[0], "lexical probe units")


def change_patch(path: str, previous: str, replacement: str) -> str:
    """Build the complete one-line fixture patch, preserving its exact newline."""
    require(
        previous.count("\n") <= 1 and replacement.count("\n") <= 1,
        "corpus probe must contain at most one source line",
    )
    before = f"a/{path}" if previous else "/dev/null"
    after = f"b/{path}" if replacement else "/dev/null"
    old = "1,1" if previous else "0,0"
    new = "1,1" if replacement else "0,0"
    return (
        f"--- {before}\n+++ {after}\n@@ -{old} +{new} @@\n"
        + (f"-{previous}" if previous else "")
        + (f"+{replacement}" if replacement else "")
    )
