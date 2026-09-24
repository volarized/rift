"""Exercise corpus measurements and refusal decisions without a live server."""

from __future__ import annotations

import dataclasses
import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import patch

from rift_dev.check_corpus import CLEANUP_RESERVE_SECONDS, Corpus
from rift_dev.commands import GitCommand
from rift_dev.corpus_assertions import (
    CONTEXT_DEGRADED,
    PROBE_PATH,
    PROBE_SOURCE,
    active_operation,
    active_stdout,
    exact_degradation,
    language_counts,
    lexical_breach,
    lexical_content,
    map_paths,
    no_failed_builds,
    probe_units,
    records,
    sample_symbols,
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
from rift_dev.rift_test_client import JsonObject


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
        ]
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

    def test_synchronous_history_refuses_finished_or_partial_records(self) -> None:
        start = 'DEBUG rift_server::history: symbol history started component="index" operation="get_symbol" phase="start"\n'
        self.assertEqual(active_stdout(start, "history", None), start.strip())
        close = 'DEBUG get_symbol{component="index" operation="get_symbol" phase="history"}: rift_server::history: close time.busy=1s\n'
        for output in (start + close, start.rstrip(), start + close.rstrip()):
            with self.assertRaises(AssertionError):
                active_stdout(output, "history", None)

    def test_stop_requires_started_work_and_rejects_completed_history(self) -> None:
        start: JsonObject = {
            "identity": 2,
            "operation": "get_symbol",
            "fields": {"phase": "start"},
        }
        closed: JsonObject = {
            "identity": 3,
            "operation": "get_symbol",
            "fields": {"span": "closed"},
        }
        self.assertEqual(active_operation([start], "history", 1, True), 2)
        for found, pending in (
            ([start], False),
            ([start, closed], True),
            ([start], True),
        ):
            after = 2 if found == [start] and pending else 1
            with self.assertRaises(AssertionError):
                active_operation(found, "history", after, pending)

    def test_stop_rebuild_allows_answered_stale_read_but_refuses_matching_close(
        self,
    ) -> None:
        start: JsonObject = {
            "identity": 2,
            "operation": "index.build",
            "fields": {"phase": "start", "epoch": "7"},
        }
        closed: JsonObject = {
            "identity": 3,
            "operation": "",
            "message": "index.build",
            "fields": {"span": "closed", "epoch": "7"},
        }
        self.assertEqual(active_operation([start], "rebuild", 1, False), 2)
        with self.assertRaises(AssertionError):
            active_operation([start, closed], "rebuild", 1, True)

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


# The document table as `rift-index/src/lexical.rs` creates it, with the ranking columns
# `lexical_content` hashes.
DOCUMENTS_TABLE = (
    "CREATE TABLE lexical_documents(identity TEXT, path TEXT, kind TEXT, digest TEXT, "
    "byte_length INTEGER, name TEXT, qualified_name TEXT, identifier_terms TEXT, "
    "signature TEXT, documentation TEXT, declaration_source TEXT, file_content TEXT)"
)
BEACON_ROW = (
    "INSERT INTO lexical_documents VALUES ('id','source.rs','symbol','d1',15,'beacon',"
    "'beacon','beacon','fn beacon()','','fn beacon() {}','fn beacon() {}')"
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
                    "'corpus_probe','corpus_probe','corpus_probe','fn corpus_probe()','',"
                    "?,?)",
                    (PROBE_PATH, PROBE_SOURCE, PROBE_SOURCE),
                )
                connection.commit()
                self.assertEqual(lexical_content(root), before)
                connection.execute(
                    "UPDATE lexical_documents SET declaration_source='changed' "
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
