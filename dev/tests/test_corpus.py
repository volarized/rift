"""Exercise corpus measurements and refusal decisions without a live server."""

from __future__ import annotations

import asyncio
import dataclasses
import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from typing import cast
from unittest.mock import AsyncMock, MagicMock, patch

from mcp.shared.exceptions import MCPError
from rift_dev.check_corpus import (
    CLEANUP_RESERVE_SECONDS,
    Corpus,
    observed,
    settled_pattern,
)
from rift_dev.commands import GitCommand
from rift_dev.corpus_assertions import (
    CONTEXT_DEGRADED,
    HELD_UNPARSED_RECORD,
    PROBE_PATH,
    PROBE_SOURCE,
    TEXT_CHUNK_BYTES,
    active_stdout,
    build_records,
    chunked_answer,
    exact_degradation,
    language_counts,
    last_line_pattern,
    lexical_breach,
    lexical_content,
    map_paths,
    named_paths,
    no_failed_builds,
    probe_units,
    records,
    sample_symbols,
    token_past_chunk,
    warnings,
)
from rift_dev.corpus_cache import (
    Measurement,
    Pin,
    git,
    measure,
    missing_history_objects,
    pins,
)
from rift_dev.rift_test_client import (
    Client,
    JsonObject,
    Server,
    gate_deadline,
    object_value,
)


class Measurements(unittest.TestCase):
    def test_git_tree_counts_symlink_bytes_and_preserves_path_bytes(self) -> None:
        source = b"100644 blob " + b"a" * 40 + b" 12\tdir/package.json\0"
        link = b"120000 blob " + b"b" * 40 + b" 4\talias\0"
        foreign = b"100755 blob " + b"c" * 40 + b" 8\tdir/deeper/line\nfile\0"
        self.assertEqual(measure(source + link + foreign), Measurement(3, 24, 1, 2, 1))

    def test_measure_rejects_incomplete_and_invalid_tree(self) -> None:
        invalid = [
            b"missing terminator",
            b"100644 blob a -1\tfile\0",
            b"100644 blob a 1\t../file\0",
            b"100600 blob a 1\tfile\0",
        ]
        for source in invalid:
            with self.subTest(source=source), self.assertRaises(ValueError):
                measure(source)

    def test_gitlink_is_not_counted_as_a_blob(self) -> None:
        self.assertEqual(
            measure(b"160000 commit a -\tsubmodule\0"), Measurement(0, 0, 0, 0, 0)
        )

    def test_pins_are_full_commits_with_expected_raw_measurements(self) -> None:
        corpus = pins()
        self.assertEqual(corpus["nextjs"].measurement.files, 31115)
        self.assertEqual(corpus["bun"].measurement.symlinks, 9)
        self.assertEqual(corpus["fastapi"].measurement.package_json, 0)
        self.assertTrue(all(len(pin.commit) == 40 for pin in corpus.values()))

    def test_pins_refuse_invalid_commit_and_boolean_count(self) -> None:
        original = Path("crates/rift/tests/corpus/pins.toml").read_text()
        variants = [
            original.replace(
                'commit = "744846f844374847c902b5e7fd59b4342a51ef99"', 'commit = "main"'
            ),
            original.replace("files = 19743", "files = true"),
            original.replace("unparsed_bytes = 9040049", "unparsed_bytes = -1"),
            original.replace("unparsed_bytes = 9040049", "unparsed_bytes = true"),
        ]
        self.assertTrue(all(variant != original for variant in variants))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pins.toml"
            for variant in variants:
                path.write_text(variant)
                with self.assertRaises(ValueError):
                    pins(path)

    def test_verified_cache_rejects_tracked_and_untracked_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            git(root, "init", "--quiet").output_bytes()
            (root / "source.rs").write_text("fn beacon() {}\n")
            git(root, "add", "source.rs").output_bytes()
            git(
                root,
                "-c",
                "user.name=Corpus",
                "-c",
                "user.email=corpus@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ).output_bytes()
            commit = git(root, "rev-parse", "HEAD").output_bytes().decode().strip()
            measured = measure(
                git(root, "ls-tree", "-r", "-l", "-z", commit).output_bytes()
            )
            pin = Pin("fixture", "owner/repository", "tag", commit, measured, "", 0, 60)
            self.assertEqual(pin.verify(root), measured)
            oversized = dataclasses.replace(
                pin, oversized_path="source.rs", oversized_bytes=15
            )
            self.assertEqual(oversized.verify(root), measured)
            for changed in (
                dataclasses.replace(oversized, oversized_path="missing.rs"),
                dataclasses.replace(oversized, oversized_bytes=16),
            ):
                with self.assertRaisesRegex(RuntimeError, "oversized path"):
                    changed.verify(root)
            unparsed = dataclasses.replace(
                pin, unparsed_path="source.rs", unparsed_bytes=15
            )
            self.assertEqual(unparsed.verify(root), measured)
            for changed in (
                dataclasses.replace(unparsed, unparsed_path="missing.rs"),
                dataclasses.replace(unparsed, unparsed_bytes=16),
            ):
                with self.assertRaisesRegex(RuntimeError, "unparsed path"):
                    changed.verify(root)
            wrong = dataclasses.replace(
                pin, measurement=dataclasses.replace(measured, bytes=0)
            )
            with self.assertRaisesRegex(RuntimeError, "remeasure"):
                wrong.verify(root)
            (root / "untracked").write_text("new")
            with self.assertRaisesRegex(RuntimeError, "checkout changed"):
                pin.verify(root)
            (root / "untracked").unlink()
            (root / "source.rs").write_text("fn changed() {}\n")
            with self.assertRaisesRegex(RuntimeError, "checkout changed"):
                pin.verify(root)


class CacheHistory(unittest.TestCase):
    def test_partial_cache_repairs_history_without_changing_the_pin(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source = root / "source"
            source.mkdir()
            git(source, "init", "--quiet").output_bytes()
            for key, value in (
                ("user.name", "Corpus"),
                ("user.email", "corpus@example.invalid"),
                ("commit.gpgsign", "false"),
                ("uploadpack.allowFilter", "true"),
            ):
                git(source, "config", key, value).output_bytes()
            (source / "source.rs").write_text("fn before() {}\n")
            git(source, "add", "source.rs").output_bytes()
            git(source, "commit", "--quiet", "-m", "before").output_bytes()
            old_blob = (
                git(source, "rev-parse", "HEAD:source.rs")
                .output_bytes()
                .decode()
                .strip()
            )
            # Fifty retained commits must end at a shallow boundary, not the root.
            for index in range(49):
                git(
                    source, "commit", "--quiet", "--allow-empty", "-m", str(index)
                ).output_bytes()
            (source / "source.rs").write_text("fn after() {}\n")
            git(source, "commit", "--quiet", "-am", "after").output_bytes()
            commit = git(source, "rev-parse", "HEAD").output_bytes().decode().strip()
            measured = measure(
                git(source, "ls-tree", "-r", "-l", "-z", commit).output_bytes()
            )
            pin = Pin("fixture", "owner/repository", "tag", commit, measured, "", 0, 60)
            with patch.dict("os.environ", {"RIFT_CORPUS_DIR": str(root / "cache")}):
                cached = pin.cache
                cached.parent.mkdir(parents=True)
                git(
                    root,
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    "--depth=50",
                    "--filter=blob:none",
                    source.as_uri(),
                    str(cached),
                ).output_bytes()
                git(cached, "checkout", "--quiet", "--detach", commit).output_bytes()
                shallow = (cached / ".git/shallow").read_bytes()
                tree = git(cached, "rev-parse", "HEAD^{tree}").output_bytes()
                self.assertEqual(missing_history_objects(cached, commit), [old_blob])
                with self.assertRaisesRegex(
                    RuntimeError, "history objects are missing"
                ):
                    pin.verify(cached)
                with self.assertRaisesRegex(
                    RuntimeError, "history objects are missing"
                ):
                    pin.checkout(root / "unavailable")
                self.assertFalse((root / "unavailable").exists())

                git(
                    cached, "remote", "set-url", "origin", (root / "absent").as_uri()
                ).output_bytes()
                with self.assertRaises(RuntimeError):
                    pin.sync()
                self.assertEqual(missing_history_objects(cached, commit), [old_blob])
                git(
                    cached, "remote", "set-url", "origin", source.as_uri()
                ).output_bytes()
                self.assertEqual(pin.sync(), cached)
                self.assertEqual(pin.verify(cached), measured)
                self.assertEqual(missing_history_objects(cached, commit), [])
                self.assertEqual(
                    git(cached, "cat-file", "blob", old_blob).output_bytes(),
                    b"fn before() {}\n",
                )
                self.assertEqual(
                    git(cached, "rev-parse", "HEAD").output_bytes().decode().strip(),
                    commit,
                )
                self.assertEqual(
                    git(cached, "rev-parse", "HEAD^{tree}").output_bytes(), tree
                )
                self.assertEqual((cached / ".git/shallow").read_bytes(), shallow)
                self.assertEqual(
                    git(cached, "rev-list", "--count", "HEAD").output_bytes(), b"50\n"
                )
                git(
                    cached, "remote", "set-url", "origin", (root / "absent").as_uri()
                ).output_bytes()
                self.assertEqual(pin.sync(), cached)

    def test_missing_object_output_rejects_invalid_records(self) -> None:
        for output in (b"not-an-object\n", b"?abcd\n", b"available object\n"):
            with (
                self.subTest(output=output),
                patch.object(GitCommand, "output_bytes", return_value=output),
                self.assertRaisesRegex(ValueError, "missing history object"),
            ):
                missing_history_objects(Path(), "a" * 40)


class SymbolSamples(unittest.TestCase):
    @staticmethod
    def hit(language: str, index: int) -> JsonObject:
        return {
            "hit": {
                "symbol": {
                    "id": f"rift://symbol/{language}/source/name_{index}",
                    "language": language,
                }
            }
        }

    def test_sample_represents_languages_and_is_stable_across_page_order(self) -> None:
        candidates = [
            self.hit(language, index)
            for language in ("javascript", "json", "rust", "typescript")
            for index in range(100)
        ]
        selected = sample_symbols(candidates, 200, 34, {"rust", "typescript"})
        self.assertEqual(
            language_counts(selected),
            {"javascript": 50, "json": 50, "rust": 50, "typescript": 50},
        )
        self.assertEqual(
            selected,
            sample_symbols(list(reversed(candidates)), 200, 34, {"rust", "typescript"}),
        )
        self.assertNotEqual(
            selected, sample_symbols(candidates, 200, 35, {"rust", "typescript"})
        )

    def test_sample_keeps_scarce_languages_and_fills_all_200_unique_positions(
        self,
    ) -> None:
        candidates = [self.hit("python", index) for index in range(300)]
        candidates.extend([self.hit("rust", 0), self.hit("json", 0)])
        selected = sample_symbols(candidates + candidates, 200, 34, {"python", "rust"})
        self.assertEqual(len(selected), 200)
        self.assertEqual(
            language_counts(selected), {"json": 1, "python": 198, "rust": 1}
        )

    def test_sample_refuses_missing_languages_and_duplicate_only_capacity(self) -> None:
        candidates = [self.hit("python", index) for index in range(200)]
        with self.assertRaisesRegex(AssertionError, "required languages"):
            sample_symbols(candidates, 200, 34, {"rust"})
        with self.assertRaisesRegex(AssertionError, "unique identities"):
            sample_symbols([candidates[0]] * 200, 200, 34, {"python"})
        with self.assertRaisesRegex(AssertionError, "4000 hits"):
            sample_symbols([candidates[0]] * 4001, 200, 34, {"python"})

    def test_one_identity_cannot_change_language_across_pages(self) -> None:
        original = self.hit("rust", 0)
        conflicting: JsonObject = {
            "hit": {
                "symbol": {
                    "id": "rift://symbol/rust/source/name_0",
                    "language": "json",
                }
            }
        }
        with self.assertRaisesRegex(AssertionError, "two languages"):
            sample_symbols([original, conflicting], 1, 34, {"rust"})


class Decisions(unittest.TestCase):
    def test_map_paths_keep_nested_names_distinct_and_decode_only_symbol_paths(
        self,
    ) -> None:
        answer: JsonObject = {
            "revision": "abcdef01",
            "pagination": {"page_index": 0, "total_pages": 1},
            "docs": ["nested/readme.md"],
            "modules": [{"path": "src", "children": [{"path": "src/nested"}]}],
            "entry_points": ["rift://symbol/rust/src/a%20b.rs/main"],
            "hubs": [{"symbol": "rift://symbol/json/data.json/name%2Fwith%2Fslashes"}],
        }
        found = map_paths(answer)
        self.assertEqual(
            found, {"nested/readme.md", "src", "src/nested", "src/a b.rs", "data.json"}
        )
        self.assertNotIn("readme.md", found)
        with (
            patch("rift_dev.corpus_assertions.MAP_MODULES_MAX", 1),
            self.assertRaisesRegex(AssertionError, "module count"),
        ):
            map_paths(answer)

    def test_map_paths_refuse_missing_map_and_malformed_symbol_addresses(self) -> None:
        with self.assertRaisesRegex(AssertionError, "map revision"):
            map_paths({})
        for identity in (
            "other://symbol/rust/file.rs/main",
            "rift://symbol/rust",
            "rift://symbol/rust/main",
            "rift://symbol/rust/source%ZZ.rs/main",
        ):
            with self.subTest(identity=identity), self.assertRaises(AssertionError):
                map_paths(
                    {
                        "revision": "abcdef01",
                        "pagination": {"page_index": 0, "total_pages": 1},
                        "entry_points": [identity],
                    }
                )

    def test_lexical_breach_requires_exact_field_maximum_and_overflow(self) -> None:
        def answer(detail: str) -> JsonObject:
            return {
                "warnings": [{"code": "lexical_ranking_unavailable", "detail": detail}]
            }

        valid = "field units_max, observed 20001, maximum 20000; resize"
        self.assertEqual(lexical_breach(answer(valid), 20000), 20001)
        for wrong in (
            valid.replace("units_max", "other"),
            valid.replace("20001", "20000"),
            valid.replace("maximum 20000", "maximum 200000"),
            "still committing tree revision 20000",
        ):
            with self.assertRaises(AssertionError):
                lexical_breach(answer(wrong), 20000)

    def test_synchronous_rebuild_requires_matching_epoch_without_completion(
        self,
    ) -> None:
        start = 'DEBUG rift_mcp::validation: index capture started component="index" operation="index.build" phase="start" epoch=7\n'
        self.assertEqual(active_stdout(start, "rebuild", "7"), start.strip())
        self.assertEqual(active_stdout(start, "rebuild", None), start.strip())
        wrong_close = 'INFO index.build{component="index" epoch=6}: rift_mcp::validation: close time.busy=1s\n'
        self.assertEqual(
            active_stdout(start + wrong_close, "rebuild", "7"), start.strip()
        )
        matching_close = wrong_close.replace("epoch=6", "epoch=7")
        for output, epoch in ((start, "8"), (start + matching_close, "7"), ("", "7")):
            with self.assertRaises(AssertionError):
                active_stdout(output, "rebuild", epoch)
        for output in (
            start.replace('component="index"', 'component="dependency"'),
            start.replace('operation="index.build"', 'operation="index.publish"'),
            start.replace('phase="start"', 'phase="complete"'),
            start.replace("epoch=7", "epoch=0"),
            start.replace("epoch=7", ""),
            start + matching_close,
            start + 'INFO index snapshot published operation="index.publish"\n',
        ):
            with self.assertRaises(AssertionError):
                active_stdout(output, "rebuild", None)

    def test_synchronous_history_requires_an_open_batch_with_pending_commits(
        self,
    ) -> None:
        span = 'history.batch{component="history" operation="history.batch"}'
        start = (
            f"DEBUG {span}: rift_mcp::history: history batch started "
            'component="history" operation="history.batch" phase="start" pending=4\n'
        )
        close = f"INFO {span}: rift_mcp::history: close time.busy=1ms time.idle=2s\n"
        analyzed = (
            f'INFO {span}:history.analyze{{component="history" operation="history.analyze"}}:'
            " rift_mcp::history: close time.busy=1s\n"
        )
        written = (
            f'INFO {span}:history.write{{component="history" operation="history.write"}}:'
            " rift_mcp::history: close time.busy=1ms\n"
        )
        self.assertEqual(active_stdout(start, "history", None), start.strip())
        self.assertEqual(
            active_stdout(start + analyzed + written, "history", None), start.strip()
        )
        self.assertEqual(
            active_stdout(start + close + start, "history", None), start.strip()
        )
        for output in (
            start + close,
            start + analyzed + close,
            start.rstrip(),
            start + close.rstrip(),
            start.replace("pending=4", "pending=0"),
            start.replace(" pending=4", ""),
            start.replace("history batch started", "history batch opened"),
            start + close + start.replace("pending=4", "pending=0"),
        ):
            with self.assertRaises(AssertionError):
                active_stdout(output, "history", None)

    def test_log_page_refuses_missing_store_and_truncation(self) -> None:
        answers: list[JsonObject] = [
            {"unavailable": "failed", "records": []},
            {"records": [{}] * 5000},
        ]
        for answer in answers:
            with self.assertRaises(AssertionError):
                records(answer)
        self.assertEqual(records({"records": []}), [])

    def test_manifest_degradation_requires_one_exact_record_per_resolver(self) -> None:
        expected = "585 of 841 package.json manifests were not read: at most 256 are read per workspace"
        npm: JsonObject = {
            "message": CONTEXT_DEGRADED,
            "fields": {"resolver": "npm", "reason": expected},
        }
        bun: JsonObject = {
            "message": CONTEXT_DEGRADED,
            "fields": {"resolver": "bun", "reason": expected},
        }
        exact_degradation([npm, bun], expected)
        for found in ([], [npm], [npm, npm], [npm, bun, bun]):
            with self.assertRaises(AssertionError):
                exact_degradation(found, expected)
        with self.assertRaises(AssertionError):
            exact_degradation([npm], None)

    def test_build_failure_and_warning_overflow_fail_closed(self) -> None:
        with self.assertRaisesRegex(AssertionError, "index build failed"):
            no_failed_builds([{"message": "index rebuild failed"}])
        with self.assertRaisesRegex(AssertionError, "source warnings exceeded"):
            warnings({"warnings": [{"code": "source_unavailable"}] * 10})

    def test_source_warning_summary_follows_eight_named_files(self) -> None:
        named: list[JsonObject] = [
            {"code": "source_unavailable", "unit": f"rift://file/{number}.rs"}
            for number in range(8)
        ]
        summary: JsonObject = {
            "code": "source_unavailable",
            "detail": "remaining files",
        }
        self.assertEqual(len(warnings({"warnings": [*named, summary]})), 9)
        self.assertEqual(len(warnings({"warnings": named})), 8)
        for source in (
            [*named, named[0]],
            [*named, summary, summary],
            [summary, *named],
            [summary],
        ):
            with self.subTest(source=source), self.assertRaises(AssertionError):
                warnings({"warnings": source})


def chunk_answer(path: str, size: int, start: int, *extra: JsonObject) -> JsonObject:
    """A `pattern` answer whose first hit is a file hit on `path`, with `extra` warnings."""
    return {
        "results": [
            {
                "hit": {"target": "file", "size": size},
                "range": {"start": start, "end": start + 13},
                "path": path,
            }
        ],
        "warnings": list(extra),
    }


class OversizedFile(unittest.TestCase):
    """The pinned oversized file answers search past its first chunk under `split`."""

    def test_the_token_is_the_first_one_absent_from_the_first_chunk(self) -> None:
        head = b"shared_token " * 80_000 + b"outer_innerToken\n"
        # `straddle_token` starts inside the first chunk and ends past it.
        padding = b"-" * (TEXT_CHUNK_BYTES - 10 - len(head))
        tail = b"shared_token innerToken straddle_token fresh_token_a fresh_token_b\n"
        data = head + padding + b"straddle_token\n" + tail
        token, offset = token_past_chunk(data)
        self.assertEqual(token, "fresh_token_a")
        self.assertEqual(offset, data.find(b"fresh_token_a"))
        self.assertGreater(offset, TEXT_CHUNK_BYTES)

    def test_a_token_may_start_exactly_where_the_first_chunk_ends(self) -> None:
        exact = b"-" * TEXT_CHUNK_BYTES + b"lateToken_two\n"
        self.assertEqual(token_past_chunk(exact), ("lateToken_two", TEXT_CHUNK_BYTES))
        short = b"-" * TEXT_CHUNK_BYTES + b"\nx lateToken_one\n"
        self.assertEqual(
            token_past_chunk(short), ("lateToken_one", TEXT_CHUNK_BYTES + 3)
        )

    def test_a_file_within_one_chunk_or_with_no_new_token_is_refused(self) -> None:
        with self.assertRaisesRegex(AssertionError, "within one"):
            token_past_chunk(b"only_token\n" * 10)
        with self.assertRaisesRegex(AssertionError, "within one"):
            token_past_chunk(b"a" * TEXT_CHUNK_BYTES)
        repeated = b"shared_token " * (TEXT_CHUNK_BYTES // 13 + 1000)
        with self.assertRaisesRegex(AssertionError, "no token first occurs"):
            token_past_chunk(repeated)

    def test_warnings_name_decoded_paths_through_unit_and_files(self) -> None:
        self.assertEqual(
            named_paths(
                {"code": "source_unavailable", "unit": "rift://file/src/a%20b.c"}
            ),
            {"src/a b.c"},
        )
        self.assertEqual(
            named_paths(
                {
                    "code": "large_file_unparsed",
                    "files": ["rift://file/big.js", "rift://file/lib/huge.ts"],
                }
            ),
            {"big.js", "lib/huge.ts"},
        )
        self.assertEqual(named_paths({"code": "results_truncated"}), set())
        with self.assertRaisesRegex(AssertionError, "invalid file identity"):
            named_paths(
                {"code": "source_unavailable", "unit": "rift://symbol/rust/a.rs/main"}
            )
        with self.assertRaises(UnicodeDecodeError):
            named_paths({"code": "large_file_unparsed", "files": ["rift://file/a%ff"]})

    def test_a_chunked_answer_holds_the_file_whole_and_returns_its_warnings(
        self,
    ) -> None:
        path = "src/sqlite3.c"
        other: JsonObject = {
            "code": "large_file_unparsed",
            "files": ["rift://file/test/huge.js"],
            "detail": "1 files are past [providers.syntax] max_file",
        }
        answer = chunk_answer(path, 9_508_000, 1_048_655, other)
        self.assertEqual(chunked_answer(answer, path, 9_508_000, 1_048_655), [])
        naming: JsonObject = {
            "code": "large_file_unparsed",
            "files": ["rift://file/test/huge.js", f"rift://file/{path}"],
        }
        missing: JsonObject = {
            "code": "source_unavailable",
            "unit": f"rift://file/{path}",
        }
        answer = chunk_answer(path, 9_508_000, 1_048_655, naming, missing)
        self.assertEqual(
            chunked_answer(answer, path, 9_508_000, 1_048_655),
            ["large_file_unparsed", "source_unavailable"],
        )
        refused: list[tuple[JsonObject, str]] = [
            ({"results": [], "warnings": []}, "no pattern hit at byte 1048655"),
            (chunk_answer("src/other.c", 9_508_000, 1_048_655), "expected a file hit"),
            (chunk_answer(path, 1_048_576, 1_048_655), "expected a file hit"),
            (chunk_answer(path, 9_508_000, 12), "first hit at byte 12"),
            (
                chunk_answer(
                    path,
                    9_508_000,
                    1_048_655,
                    {"code": "large_file_skipped", "skipped": 1, "detail": "skip"},
                ),
                "skipped a large file",
            ),
        ]
        for answer, message in refused:
            with (
                self.subTest(message=message),
                self.assertRaisesRegex(AssertionError, message),
            ):
                chunked_answer(answer, path, 9_508_000, 1_048_655)
        symbol: JsonObject = {
            "results": [
                {
                    "hit": {"target": "symbol", "size": 9_508_000},
                    "range": {"start": 1_048_655, "end": 1_048_668},
                    "path": path,
                }
            ],
        }
        with self.assertRaisesRegex(AssertionError, "expected a file hit"):
            chunked_answer(symbol, path, 9_508_000, 1_048_655)

    def test_a_preparing_answer_is_resent_until_the_trigram_index_settles(
        self,
    ) -> None:
        preparing: JsonObject = {
            "results": [],
            "warnings": [
                {
                    "code": "pattern_index_preparing",
                    "prepared": 10,
                    "total": 20,
                    "detail": "10 of 20 rows",
                }
            ],
        }
        settled = chunk_answer("big.c", 2_000_000, 1_048_700)
        client = AsyncMock(spec=Client)
        client.call.side_effect = [preparing, preparing, settled]
        request: JsonObject = {"pattern": "fresh_token", "target": "file"}
        with patch("rift_dev.check_corpus.POLL_SECONDS", 0.0):
            answer = asyncio.run(settled_pattern(cast(Client, client), request))
        self.assertEqual(answer, settled)
        self.assertEqual(client.call.await_count, 3)
        client.call.assert_awaited_with("search", request)
        client.call.side_effect = None
        client.call.return_value = preparing
        with (
            patch("rift_dev.check_corpus.POLL_SECONDS", 0.0),
            patch("rift_dev.check_corpus.OBSERVATION_SECONDS", 0.05),
            self.assertRaisesRegex(AssertionError, "never covered every stored row"),
        ):
            asyncio.run(settled_pattern(cast(Client, client), request))

    def test_the_pinned_file_is_searched_past_its_first_chunk(self) -> None:
        path = "src/big.c"
        data = b"shared_token\n" * (TEXT_CHUNK_BYTES // 13 + 1) + b"fresh_token_z\n"
        offset = data.find(b"fresh_token_z")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / path).write_bytes(data)
            pin = dataclasses.replace(
                pins()["bun"], oversized_path=path, oversized_bytes=len(data)
            )
            corpus = Corpus(pin, root / "rift", root / "report.json")
            corpus.root = root
            client = AsyncMock(spec=Client)
            client.call.return_value = chunk_answer(path, len(data), offset)
            asyncio.run(corpus.oversized(cast(Client, client)))
            client.call.assert_awaited_once_with(
                "search",
                {
                    "pattern": "fresh_token_z",
                    "target": "file",
                    "paths": {"include": [path]},
                    "limit": 1,
                },
            )
            self.assertEqual(
                object_value(corpus.actions[-1], "action")["action"], "oversized"
            )
            client.call.return_value = chunk_answer(
                path,
                len(data),
                offset,
                {"code": "source_unavailable", "unit": f"rift://file/{path}"},
            )
            with self.assertRaisesRegex(AssertionError, "named by"):
                asyncio.run(corpus.oversized(cast(Client, client)))
            (root / path).write_bytes(data + b"\n")
            with self.assertRaisesRegex(AssertionError, "pinned byte count changed"):
                asyncio.run(corpus.oversized(cast(Client, client)))
            empty = dataclasses.replace(pin, oversized_path="", oversized_bytes=0)
            client.call.reset_mock()
            unpinned = Corpus(empty, root / "rift", root / "report.json")
            asyncio.run(unpinned.oversized(cast(Client, client)))
            client.call.assert_not_awaited()


def build_record(path: str, message: str) -> JsonObject:
    """One index-build record naming `path`, as `rift://logs` answers it."""
    return {
        "level": "warn",
        "component": "index",
        "operation": "index.build",
        "message": message,
        "fields": {"path": path, "reason": "exceeds a syntax bound (source_too_large)"},
    }


class UnparsedFile(unittest.TestCase):
    """The pinned file past `max_file` answers search as text, named `large_file_unparsed`."""

    def test_the_last_line_pattern_escapes_every_reserved_character(self) -> None:
        line = b"console.log(counter); a-b~c#d&e [1]{2}^$|\\ <x> f*g+h?"
        data = b"first\r\n" + line + b"\n" + line + b"\n\n  \n"
        pattern, offset = last_line_pattern(data)
        self.assertEqual(offset, 7)
        self.assertEqual(
            pattern,
            "console\\.log\\(counter\\); a\\-b\\~c\\#d\\&e "
            "\\[1\\]\\{2\\}\\^\\$\\|\\\\ <x> f\\*g\\+h\\?",
        )
        self.assertEqual(last_line_pattern(b"only line"), ("only line", 0))

    def test_a_blank_file_or_a_line_past_the_pattern_bound_is_refused(self) -> None:
        with self.assertRaisesRegex(AssertionError, "no nonblank line"):
            last_line_pattern(b"\n \n\t\n")
        with self.assertRaisesRegex(AssertionError, "past 1024"):
            last_line_pattern(b"x" * 1_025)
        with self.assertRaisesRegex(AssertionError, "1026 characters"):
            last_line_pattern(b"x" * 1_022 + b"..")
        self.assertEqual(len(last_line_pattern(b"x" * 1_024)[0]), 1_024)

    def test_build_records_keep_the_index_build_messages_naming_the_path(self) -> None:
        path = "test/fixtures/lots.js"
        found = [
            build_record(path, HELD_UNPARSED_RECORD),
            build_record("other.js", "file left out of the index"),
            {
                **build_record(path, "index rebuild failed"),
                "operation": "index.rebuild",
            },
            build_record(path, "file left out of the index"),
        ]
        self.assertEqual(
            build_records(found, path),
            [HELD_UNPARSED_RECORD, "file left out of the index"],
        )
        self.assertEqual(build_records([], path), [])

    def test_the_pinned_file_answers_as_text_and_is_recorded_held_unparsed(
        self,
    ) -> None:
        path = "test/fixtures/lots.js"
        data = b"let counter = 0;\n" + b"for (;;) break;\n" * 8 + b"log(counter);\n"
        offset = data.find(b"log(counter);")
        unparsed: JsonObject = {
            "code": "large_file_unparsed",
            "files": [f"rift://file/{path}"],
            "detail": "1 files are past [providers.syntax] max_file",
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / path).parent.mkdir(parents=True)
            (root / path).write_bytes(data)
            pin = dataclasses.replace(
                pins()["bun"], unparsed_path=path, unparsed_bytes=len(data)
            )
            corpus = Corpus(pin, root / "rift", root / "report.json")
            corpus.root = root
            client = AsyncMock(spec=Client)
            client.call.return_value = chunk_answer(path, len(data), offset, unparsed)
            held = build_record(path, HELD_UNPARSED_RECORD)
            client.resource.return_value = {"records": [held, held]}
            asyncio.run(corpus.unparsed(cast(Client, client)))
            client.call.assert_awaited_once_with(
                "search",
                {
                    "pattern": "log\\(counter\\);",
                    "target": "file",
                    "paths": {"include": [path]},
                    "limit": 1,
                },
            )
            client.resource.assert_awaited_with("rift://logs/component/index")
            self.assertEqual(
                object_value(corpus.actions[-1], "action")["action"], "unparsed"
            )
            left_out = build_record(path, "file left out of the index")
            refusals: list[tuple[JsonObject, JsonObject, str]] = [
                (
                    chunk_answer(path, len(data), offset),
                    {"records": [held]},
                    r"named by \[\], expected large_file_unparsed",
                ),
                (
                    chunk_answer(
                        path,
                        len(data),
                        offset,
                        unparsed,
                        {"code": "source_unavailable", "unit": f"rift://file/{path}"},
                    ),
                    {"records": [held]},
                    "named by \\['large_file_unparsed', 'source_unavailable'\\]",
                ),
                (
                    chunk_answer(path, len(data), offset, unparsed),
                    {"records": [held, left_out]},
                    "expected 'file held unparsed in the index' alone",
                ),
                (
                    chunk_answer(path, len(data), offset, unparsed),
                    {"records": [left_out]},
                    "expected 'file held unparsed in the index' alone",
                ),
            ]
            for answer, page, message in refusals:
                client.call.return_value = answer
                client.resource.return_value = page
                with (
                    self.subTest(message=message),
                    self.assertRaisesRegex(AssertionError, message),
                ):
                    asyncio.run(corpus.unparsed(cast(Client, client)))
            (root / path).write_bytes(data + b"\n")
            with self.assertRaisesRegex(AssertionError, "pinned byte count changed"):
                asyncio.run(corpus.unparsed(cast(Client, client)))
            client.call.reset_mock()
            unpinned = Corpus(pins()["nextjs"], root / "rift", root / "report.json")
            asyncio.run(unpinned.unparsed(cast(Client, client)))
            client.call.assert_not_awaited()

    def test_only_bun_pins_an_unparsed_file_and_the_cache_checks_it(self) -> None:
        loaded = pins()
        self.assertEqual(
            (loaded["bun"].unparsed_path, loaded["bun"].unparsed_bytes),
            ("test/bundler/transpiler/fixtures/lots-of-for-loop.js", 9_040_049),
        )
        for name in ("nextjs", "fastapi"):
            self.assertEqual(
                (loaded[name].unparsed_path, loaded[name].unparsed_bytes), ("", 0)
            )


# The document table as `rift-index/src/lexical.rs` creates it, with the ranking columns
# `lexical_content` hashes.
DOCUMENTS_TABLE = (
    "CREATE TABLE lexical_documents(identity TEXT, path TEXT, kind TEXT, digest TEXT, "
    "byte_length INTEGER, byte_offset INTEGER, name TEXT, qualified_name TEXT, "
    "identifier_terms TEXT, signature TEXT, documentation TEXT, file_content TEXT)"
)
BEACON_ROW = (
    "INSERT INTO lexical_documents VALUES ('id','source.rs','symbol','d1',15,NULL,"
    "'beacon','beacon','beacon','fn beacon()','','fn beacon() {}')"
)


class PersistedContent(unittest.TestCase):
    def test_reads_close_connections_on_success_and_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".rift").mkdir()
            with closing(sqlite3.connect(root / ".rift/db")) as fixture:
                fixture.execute(DOCUMENTS_TABLE)
                fixture.execute(BEACON_ROW)
                fixture.commit()
            connect = sqlite3.connect
            opened: list[sqlite3.Connection] = []

            def tracked(
                database: str, *, uri: bool, timeout: float
            ) -> sqlite3.Connection:
                connection = connect(database, uri=uri, timeout=timeout)
                opened.append(connection)
                return connection

            with patch(
                "rift_dev.corpus_assertions.sqlite3.connect", side_effect=tracked
            ):
                self.assertEqual(probe_units(root), 0)
                self.assertEqual(lexical_content(root).units, 1)
                with (
                    patch("rift_dev.corpus_assertions.LEXICAL_UNITS_MAX", 0),
                    self.assertRaises(AssertionError),
                ):
                    lexical_content(root)
            self.assertEqual(len(opened), 3)
            for connection in opened:
                with self.assertRaises(sqlite3.ProgrammingError):
                    connection.execute("SELECT 1")

    def test_write_preserves_unrelated_rows_and_detects_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".rift").mkdir()
            with sqlite3.connect(root / ".rift/db") as connection:
                connection.execute(DOCUMENTS_TABLE)
                connection.execute(BEACON_ROW)
                connection.commit()
                before = lexical_content(root)
                connection.execute(
                    "INSERT INTO lexical_documents VALUES ('probe',?,'symbol','d2',24,"
                    "NULL,'corpus_probe','corpus_probe','corpus_probe',"
                    "'fn corpus_probe()','',?)",
                    (PROBE_PATH, PROBE_SOURCE),
                )
                connection.commit()
                self.assertEqual(lexical_content(root), before)
                connection.execute(
                    "UPDATE lexical_documents SET file_content='changed' "
                    "WHERE identity='id'"
                )
                connection.commit()
                self.assertNotEqual(lexical_content(root), before)

    def test_empty_or_over_budget_store_is_not_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".rift").mkdir()
            with sqlite3.connect(root / ".rift/db") as connection:
                connection.execute(DOCUMENTS_TABLE)
                connection.commit()
                with self.assertRaisesRegex(AssertionError, "empty"):
                    lexical_content(root)
                connection.execute(BEACON_ROW)
                connection.commit()
                with (
                    patch("rift_dev.corpus_assertions.LEXICAL_UNITS_MAX", 0),
                    self.assertRaisesRegex(AssertionError, "row count"),
                ):
                    lexical_content(root)


class LogObservation(unittest.TestCase):
    """Log observation keeps one deadline when the fixture removes poll delay."""

    def test_zero_poll_delay_observes_record_after_pending_reads(self) -> None:
        client = AsyncMock(spec=Client)
        found: list[JsonObject] = [{"message": "index rebuild failed"}]
        client.resource.side_effect = [
            {"records": []},
            {"records": []},
            {"records": found},
        ]
        with patch("rift_dev.check_corpus.POLL_SECONDS", 0.0):
            answer = asyncio.run(
                observed(cast(Client, client), "rift://logs/component/index", bool)
            )
        self.assertEqual(answer, found)
        self.assertEqual(client.resource.await_count, 3)
        client.resource.assert_awaited_with("rift://logs/component/index")

    def test_zero_poll_delay_still_times_out_when_record_is_absent(self) -> None:
        client = AsyncMock(spec=Client)
        client.resource.return_value = {"records": []}
        with (
            patch("rift_dev.check_corpus.POLL_SECONDS", 0.0),
            patch("rift_dev.check_corpus.OBSERVATION_SECONDS", 0.01),
            self.assertRaises(TimeoutError),
        ):
            asyncio.run(
                observed(cast(Client, client), "rift://logs/component/index", bool)
            )
        self.assertGreater(client.resource.await_count, 0)
        client.resource.assert_awaited_with("rift://logs/component/index")

    def test_observation_deadline_cancels_held_resource_read(self) -> None:
        client = AsyncMock(spec=Client)
        cancelled = False

        async def held_resource(_uri: str) -> JsonObject:
            nonlocal cancelled
            try:
                await asyncio.Event().wait()
            finally:
                cancelled = True
            raise AssertionError("the held resource read must be cancelled")

        client.resource.side_effect = held_resource
        with (
            patch("rift_dev.check_corpus.POLL_SECONDS", 0.0),
            patch("rift_dev.check_corpus.OBSERVATION_SECONDS", 0.01),
            self.assertRaises(TimeoutError),
        ):
            asyncio.run(
                observed(cast(Client, client), "rift://logs/component/index", bool)
            )
        self.assertTrue(cancelled)
        client.resource.assert_awaited_once_with("rift://logs/component/index")


class SourceBound(unittest.TestCase):
    """Discovery refusal remains readable after early server publication (#495)."""

    @staticmethod
    def refusal() -> JsonObject:
        return {
            "code": "limit_exceeded",
            "phase": "read",
            "limit": {"field": "source.files", "required": 20001, "limit": 20000},
            "causes": [{"message": "violation too_many_files"}],
        }

    def exercise(
        self, responses: list[JsonObject | MCPError]
    ) -> tuple[Corpus, MagicMock]:
        corpus = Corpus(pins()["nextjs"], Path("rift"), Path("report.json"))
        server = MagicMock(spec=Server)
        client = AsyncMock(spec=Client)
        client.call.side_effect = responses
        client.resource.return_value = {
            "records": [
                {
                    "message": "index rebuild failed",
                    "fields": {"error_code": "limit_exceeded"},
                }
            ]
        }
        server.connect.return_value.__aenter__.return_value = client
        with (
            patch.object(corpus, "server", return_value=server),
            patch.object(corpus, "configure") as configure,
            patch("rift_dev.check_corpus.POLL_SECONDS", 0.0),
        ):
            asyncio.run(corpus.source_bound())
        configure.assert_any_call("[source]\nfiles = 20000\n")
        configure.assert_called_with()
        server.start.assert_called_once_with()
        server.check_running.assert_called_once_with()
        server.stop.assert_called_once_with()
        server.close.assert_called_once_with()
        client.call.assert_awaited_with("get_symbol", {"name": "corpus_probe"})
        client.resource.assert_awaited_with("rift://logs/component/index")
        return corpus, server

    def test_preparing_read_reaches_exact_refusal_and_stops_live_server(self) -> None:
        corpus, _server = self.exercise(
            [
                {"warnings": [{"code": "local_index_preparing"}]},
                MCPError(-32000, "source limit", self.refusal()),
            ]
        )
        action = object_value(corpus.actions[-1], "source action")
        self.assertEqual(action["action"], "source_bound")
        self.assertEqual(action["field"], "source.files")
        self.assertEqual(action["observed"], 20001)
        self.assertEqual(action["maximum"], 20000)

    def test_wrong_refusal_code_phase_field_quantities_and_cause_fail(self) -> None:
        variants = [
            {"code": "resource_not_found"},
            {"phase": "initialize"},
            {
                "limit": {
                    "field": "source.workspace_size",
                    "required": 20001,
                    "limit": 20000,
                }
            },
            {"limit": {"field": "source.files", "required": 20000, "limit": 20000}},
            {"limit": {"field": "source.files", "required": 20001, "limit": 20001}},
            {"causes": [{"message": "another refusal"}]},
        ]
        for wrong in variants:
            with self.subTest(wrong=wrong), self.assertRaises(AssertionError):
                self.exercise(
                    [MCPError(-32000, "source limit", {**self.refusal(), **wrong})]
                )

    def test_complete_read_without_refusal_fails(self) -> None:
        with self.assertRaisesRegex(AssertionError, "returned a complete read"):
            self.exercise([{"warnings": []}])

    def test_refusal_wait_keeps_one_deadline_and_closes_on_timeout(self) -> None:
        corpus = Corpus(pins()["nextjs"], Path("rift"), Path("report.json"))
        server = MagicMock(spec=Server)
        client = AsyncMock(spec=Client)

        async def held_read(_name: str, _arguments: JsonObject) -> JsonObject:
            await asyncio.Event().wait()
            raise AssertionError("the held read must be cancelled")

        client.call.side_effect = held_read
        server.connect.return_value.__aenter__.return_value = client
        with (
            patch.object(corpus, "server", return_value=server),
            patch.object(corpus, "configure") as configure,
            patch(
                "rift_dev.check_corpus.gate_deadline",
                side_effect=lambda name, _seconds: gate_deadline(name, 0.01),
            ) as deadline,
            self.assertRaises(TimeoutError),
        ):
            asyncio.run(corpus.source_bound())
        deadline.assert_called_once_with("source.files refusal", 180.0)
        self.assertEqual(corpus.actions, [])
        server.stop.assert_not_called()
        server.close.assert_called_once_with()
        configure.assert_called_with()

    def test_stop_failure_cannot_record_passed_source_bound(self) -> None:
        corpus = Corpus(pins()["nextjs"], Path("rift"), Path("report.json"))
        server = MagicMock(spec=Server)
        server.stop.side_effect = AssertionError("server stop exceeded its deadline")
        client = AsyncMock(spec=Client)
        client.call.side_effect = MCPError(-32000, "source limit", self.refusal())
        client.resource.return_value = {
            "records": [
                {
                    "message": "index rebuild failed",
                    "fields": {"error_code": "limit_exceeded"},
                }
            ]
        }
        server.connect.return_value.__aenter__.return_value = client
        with (
            patch.object(corpus, "server", return_value=server),
            patch.object(corpus, "configure") as configure,
            self.assertRaisesRegex(AssertionError, "server stop exceeded"),
        ):
            asyncio.run(corpus.source_bound())
        self.assertEqual(corpus.actions, [])
        server.close.assert_called_once_with()
        configure.assert_called_with()


class CaseBudgets(unittest.TestCase):
    """A case stops its own actions before nextest ends the case."""

    def test_the_work_budget_reserves_cleanup_inside_the_pinned_deadline(self) -> None:
        for pin in pins().values():
            with tempfile.TemporaryDirectory() as directory:
                corpus = Corpus(
                    pin,
                    Path(directory) / "rift",
                    Path(directory) / "report.json",
                    case="stop" if pin.name == "bun" else "workspace",
                )
                budget = corpus.work_seconds()
                self.assertLess(
                    budget,
                    pin.seconds,
                    f"{pin.name}: the actions must stop before the case's deadline",
                )
                self.assertEqual(budget, pin.seconds - CLEANUP_RESERVE_SECONDS)
                self.assertGreater(
                    budget,
                    pin.seconds / 2,
                    f"{pin.name}: the reserve must not take the case's own time",
                )
