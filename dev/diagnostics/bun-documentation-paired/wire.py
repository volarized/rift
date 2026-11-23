"""Verified wire helpers extracted from the signed Bun exact-symbol fixture."""

from __future__ import annotations

import asyncio
from contextlib import closing
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import time
from urllib.parse import quote

from rift_dev.log_records import Line, fields, lines
from rift_dev.perf.edit import CONFIGURATION, LOG_FILTER, PATHS, declaration, log_since, wait_publication
from rift_dev.rift_test_client import Server

PROBE = PATHS["bun"]
STEP = {"single": 31, "burst": 50}
TREE_REVISION = re.compile(r"rift://revision/tree/[0-9a-f]{64}")
LEXICAL_PREFIX = re.compile(r"[0-9a-f]{8}")
WORKSPACE_RESOURCE = "rift://workspace?max_results=1"
DOC_PATH = "src/js/internal/serialization_buffers.ts"
DOC_TOKEN = b"aliased views"
DOC_SYMBOL = "rift://symbol/local/typescript/.%2Fsrc%2Fjs%2Finternal%2Fserialization_buffers.ts/restoreBuffers"
DOC_FILE_SHA256 = "7c69b88adafa173a023c4288559b898ec126e996fc827cf78297625f0c0d4b4d"


def require(condition: bool, detail: str) -> None:
    if not condition:
        raise ValueError(detail)

def write_json(path: Path, value: object) -> None:
    with path.open("x", encoding="utf-8") as out:
        json.dump(value, out, indent=2, ensure_ascii=False)
        out.write("\n")

def git(root: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(root), *args], timeout=45)

def safe_input(path: str) -> bool:
    parsed = Path(path)
    return (
        not parsed.is_absolute() and ".." not in parsed.parts and path.endswith(".ts")
    )

def fixture(root: Path, saved_paths: Path, lock: dict) -> dict[str, bytes]:
    staged = saved_paths.read_bytes()
    require(
        hashlib.sha256(staged).hexdigest() == lock["corpus_paths_sha256"],
        "Bun changed path list does not match the saved 50-path recipe",
    )
    paths = staged.decode().splitlines()
    require(
        len(paths) == len(set(paths)) == 50
        and paths[-1] == PROBE
        and all(map(safe_input, paths)),
        "invalid Bun shape or unexpected mutation paths",
    )
    require(
        git(root, "rev-parse", "HEAD").decode().strip() == lock["corpus_pin"],
        "wrong Bun git pin",
    )
    require(
        not git(root, "status", "--porcelain", "--untracked-files=all"),
        "the dedicated Bun checkout is dirty",
    )
    tracked = {}
    for line in git(root, "ls-tree", "-z", "HEAD", "--", *paths).split(b"\0"):
        if line:
            left, path = line.decode().split("\t", 1)
            mode, kind, oid = left.split()
            require(
                mode in {"100644", "100755"} and kind == "blob",
                "selected source is not a regular git blob",
            )
            tracked[path] = oid
    original = {}
    for path in paths:
        file = root / path
        require(
            file.is_file() and not file.is_symlink() and file.resolve() == file,
            "unsafe selected source path",
        )
        value = file.read_bytes()
        require(
            len(value) < 16 << 20, "selected source exceeds the bounded witness size"
        )
        oid = hashlib.sha1(f"blob {len(value)}\0".encode() + value).hexdigest()
        require(
            tracked.get(path) == oid,
            f"working source differs from the locked git blob: {path}",
        )
        original[path] = value
    return original

def edited(original: dict[str, bytes], recipe: str) -> dict[str, bytes]:
    _, marker_source, _ = declaration("bun", STEP[recipe])
    if recipe == "single":
        return {PROBE: original[PROBE] + marker_source}
    result = {}
    for path, data in original.items():
        if path == PROBE:
            result[path] = data + marker_source
        else:
            offset = data.index(b"\n") + 1 if data.startswith(b"#!") else 0
            result[path] = data[:offset] + b"\n" + data[offset:]
    return result

def expected_digests(source: dict[str, bytes]) -> dict[str, str]:
    return {path: hashlib.sha256(data).hexdigest() for path, data in source.items()}

def lexical_state(root: Path, expected: dict[str, str]) -> str | None:
    uri = (root / ".rift" / "index").as_uri() + "?mode=ro"
    try:
        with closing(sqlite3.connect(uri, uri=True, timeout=0.1)) as db:
            db.execute("BEGIN")
            bound = ",".join("?" for _ in expected)
            saved = dict(
                db.execute(
                    f"SELECT path, lower(hex(digest)) FROM lexical_files WHERE path IN ({bound})",
                    tuple(expected),
                )
            )
            revision = db.execute(
                "SELECT tree_revision FROM lexical_index_state WHERE id = 1"
            ).fetchone()
            if revision and LEXICAL_PREFIX.fullmatch(revision[0] or "") and saved == expected:
                return revision[0]
    except sqlite3.OperationalError:
        return None
    return None

async def wait_lexical(
    server: Server, expected: dict[str, str], older: str | None = None
) -> tuple[int, str]:
    while True:
        server.check_running()
        if (revision := lexical_state(server.root, expected)) and revision != older:
            return time.perf_counter_ns(), revision
        await asyncio.sleep(0.025)

def current_revision(answer: dict, lexical_revision: str) -> str:
    revision = answer.get("revision", "")
    require(
        TREE_REVISION.fullmatch(revision) and LEXICAL_PREFIX.fullmatch(lexical_revision)
        and revision.removeprefix("rift://revision/tree/").startswith(lexical_revision),
        "the response does not name the full accepted working-tree revision",
    )
    require(
        all(warning.get("code") != "stale_index" for warning in answer.get("warnings", [])),
        "the read carries a stale-index warning",
    )
    return revision


def captured_attachment(root: Path, expected: dict[str, str], revision: str, prefix: str) -> dict:
    """Read the actual collector row in the transaction carrying the selected file digests."""
    require(current_revision({"revision": revision}, prefix) == revision, "invalid attachment tree revision")
    identity = {"source": {"kind": "project", "path": DOC_PATH}}
    source_key = json.dumps(identity, separators=(",", ":"))
    db_path = (root / ".rift" / "index").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(db_path, uri=True, timeout=0.1)) as db:
        db.execute("PRAGMA query_only = ON")
        db.execute("BEGIN")
        stamp = db.execute("SELECT tree_revision FROM lexical_index_state WHERE id = 1").fetchone()
        slots = ",".join("?" for _ in expected)
        file_digests = dict(db.execute(
            f"SELECT path, lower(hex(digest)) FROM lexical_files WHERE path IN ({slots})", tuple(expected),
        ))
        manifest = db.execute(
            "SELECT payload FROM documentation_manifest WHERE id = 1 AND length(payload) <= 4 * 1024 * 1024",
        ).fetchone()
        attached = db.execute(
            "SELECT lower(hex(digest)), payload FROM documentation_sources WHERE identity = ? AND length(payload) <= 65536",
            (source_key,),
        ).fetchone()
    require(stamp == (prefix,) and file_digests == expected,
            "documentation arrived in a different lexical tree or physical source selection")
    require(manifest is not None and attached is not None, "no bounded captured attachment or collector manifest")
    recorded_digest, payload = attached
    require(hashlib.sha256(payload.encode()).hexdigest() == recorded_digest, "attachment payload digest differs")
    source_rows = json.loads(payload)
    source = source_rows["source"]
    physical = (root / DOC_PATH).read_bytes()
    require(source["identity"] == identity and source["format"] == "attached_comment"
            and source["selection"] == "attached_comment" and source["byte_length"] == len(physical)
            and source["content_digest"] == expected[DOC_PATH] == hashlib.sha256(physical).hexdigest(),
            "the collector row does not cover the exact current authored source")
    require(physical.count(DOC_TOKEN) == 1, "missing or ambiguous selected comment spelling")
    position = physical.index(DOC_TOKEN)
    start = physical.rindex(b"/**", 0, position)
    end = physical.index(b"*/", position) + 2
    comment = physical[start:end]
    blocks = [block for block in source_rows["blocks"] if block.get("symbol") == DOC_SYMBOL]
    require(len(blocks) == 1, "no unique current comment block for the exact owner")
    block, = blocks
    require(block["source"] == identity and block["range"] == {"start": start, "end": end}
            and block["content_digest"] == hashlib.sha256(comment).hexdigest()
            and block["line"] == physical.count(b"\n", 0, start) + 1,
            "the published block did not match the authored bytes, range and line")
    doc_manifest = json.loads(manifest[0])
    require(LEXICAL_PREFIX.fullmatch(doc_manifest["documentation_revision"]), "collector revision missing")
    return {"tree_revision": revision, "selected_digests": file_digests, "collector_manifest": doc_manifest,
            "source_payload_digest": recorded_digest, "source_payload": source_rows, "comment": comment.decode()}


def singleton(answer: dict) -> dict:
    declarations = answer["declarations"]
    page = answer["pagination"]
    require(
        len(declarations) == 1
        and (page["page"], page["pages"], page["total"]) == (1, 1, 1),
        "positive singleton needs one actual declaration across all pages",
    )
    return declarations[0]

def exact(
    answer: dict, identity: str, marker: str, path_source: bytes, lexical_revision: str,
) -> dict:
    require(
        answer.get("outcome") == "found"
        and answer.get("symbol", {}).get("id") == identity
        and answer["symbol"].get("name") == marker,
        "the first request did not return its canonical marker",
    )
    current_revision(answer, lexical_revision)
    needle = f"function {marker}() {{ return {marker.rsplit('_', 1)[1]}; }}".encode()
    require(path_source.count(needle) == 1, "marker bytes must occur once")
    start = path_source.index(needle)
    source = singleton(answer)
    require(
        source.get("path") == PROBE
        and "unit" not in source
        and source.get("origin", {}).get("location") == "project"
        and source.get("origin", {}).get("source_kind") == "authored"
        and source.get("range") == {"start": start, "end": start + len(needle)}
        and source.get("line") == path_source.count(b"\n", 0, start) + 1,
        "the canonical read points outside the authored file or exact bytes",
    )
    return source["range"]

def exact_facets(
    answer: dict, identity: str, revision: str, byte_range: dict, path_source: bytes,
) -> None:
    require(answer.get("symbol") == identity, "facets names another symbol")
    require(
        current_revision(answer, revision.removeprefix("rift://revision/tree/")[:8]) == revision,
        "facets disagrees with the exact current revision",
    )
    row = singleton(answer)
    source = row["source"]
    span = source["span"]
    excerpt = path_source[byte_range["start"]:byte_range["end"]]
    require(
        type(row["declaration"]) is int
        and row["declaration"] == 0
        and row["range"] == span["range"] == byte_range
        and span["unit"] == "rift://source/project/" + quote(PROBE, safe="/")
        and source["text"].encode("utf-8") == excerpt,
        "facets does not hold declaration zero at the exact authored source range",
    )

def restore_outcome(answer: dict, error: bool, identity: str) -> None:
    missing = (
        error and answer.get("outcome") == "missing"
        and answer.get("symbol_not_found", {}).get("id") == identity
    )
    unavailable = (
        not error and answer.get("outcome") == "unavailable"
        and answer.get("reason") == "insufficient_coverage"
        and answer.get("id") == identity
    )
    require(missing or unavailable, "restored marker did not return a typed absence")
    require(
        all(warning.get("code") != "stale_index" for warning in answer.get("warnings", [])),
        "the restored read carries a stale-index warning",
    )

def request(row: Line) -> tuple[str | None, str | None]:
    root = fields(row.context)
    return root.get("tool"), root.get("req")

def verify_request(trace: str, tool: str) -> tuple[str | None, str | None]:
    records = list(lines(trace))
    starts = [row for row in records if row.is_message("tool request started")]
    ends = [row for row in records if row.is_message("tool request completed")]
    require(
        len(starts) == len(ends) == 1
        and request(starts[0]) == request(ends[0])
        and request(starts[0])[0] == tool
        and request(starts[0])[1] is not None
        and starts[0].time <= ends[0].time,
        f"{tool} does not have one complete owning request",
    )
    require(
        all(row.label("warning_code") != "stale_index" for row in records),
        "the server logged a stale-index warning during the read",
    )
    return request(starts[0])
