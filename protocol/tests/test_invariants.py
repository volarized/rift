"""Executable contracts for invariants the specification pins."""

from unittest import TestCase

from pydantic import ValidationError

from rift.generate import config_schema_output
from rift.models import config, core, mcp, scip_api


def limits(**overrides: object) -> mcp.Limits:
    values: dict[str, object] = {
        "max_request_bytes": 49_152,
        "max_response_bytes": 65_536,
        "max_record_bytes": 49_152,
        "max_file_chunk_bytes": 32_768,
        "max_page_items": 200,
        "max_relation_depth": 5,
        "max_edits": 10_000,
        "max_hooks": 4,
        "execution": None,
        "max_filter_depth": 16,
        "max_request_ms": 60_000,
        "max_active_cursors": 16,
        "max_capture_items": 2_048,
        "max_capture_bytes": 8 * 1024 * 1024,
        "max_retained_capture_bytes": 32 * 1024 * 1024,
        "max_projection_dependencies": 4_096,
        "max_projection_dependency_bytes": 512 * 1024,
    }
    values.update(overrides)
    return mcp.Limits.model_validate(values)


class ProjectionIdentityTests(TestCase):
    def test_projection_identity_is_owned_by_core(self) -> None:
        self.assertNotIn("ProjectionId", {model.__name__ for model in mcp.MODELS})
        self.assertIn("ProjectionId", {model.__name__ for model in core.MODELS})

    def test_projection_identity_is_the_address(self) -> None:
        core.ProjectionId.model_validate(
            "rift://projection/prj_k2mq4vx6ntbwpj3rfd6a2zcyhe"
        )
        for bad in (
            "prj_k2mq4vx6ntbwpj3rfd6a2zcyhe",
            "rift://projection/prj_UPPER",
            "rift://projection/chg_k2mq4vx6ntbwpj3rfd6a2zcyhe",
            "",
        ):
            with self.assertRaises(ValidationError):
                core.ProjectionId.model_validate(bad)


class ScipSurfaceTests(TestCase):
    def test_scip_surface_carries_the_typed_identity(self) -> None:
        self.assertEqual(
            scip_api.Request.model_fields["projection"].annotation,
            core.ProjectionId | None,
        )
        self.assertEqual(
            scip_api.Header.model_fields["projection"].annotation,
            core.ProjectionId | None,
        )

    def test_language_coverage_is_part_of_the_export(self) -> None:
        names = {model.__name__ for model in scip_api.SCIP_API_PACKAGE.models}
        self.assertIn("LanguageCoverage", names)
        self.assertEqual(
            scip_api.Header.model_fields["coverage"].annotation,
            list[scip_api.LanguageCoverage],
        )


class OmissionTests(TestCase):
    def test_reasons_are_exactly_the_specified_set(self) -> None:
        self.assertEqual(
            {member.name: member.value for member in scip_api.OmissionReason},
            {
                "REASON_UNSPECIFIED": 0,
                "REASON_UNREPRESENTABLE": 1,
                "REASON_NO_ANALYSIS": 2,
                "REASON_UNINDEXABLE_PATH": 3,
                "REASON_TOO_LARGE": 4,
            },
        )

    def test_an_omission_names_a_field_or_a_path(self) -> None:
        scip_api.Omission.model_validate(
            {"field": "Relationship.evidence", "reason": 1, "count": 40}
        )
        scip_api.Omission.model_validate(
            {"path": "vendor/generated.rs", "reason": 4, "count": 1}
        )
        with self.assertRaises(ValidationError):
            scip_api.Omission.model_validate({"reason": 1, "count": 1})


class ConfirmationTests(TestCase):
    def test_configuration_change_is_a_confirmable_condition(self) -> None:
        self.assertEqual(
            core.ConfirmationRequirementKind.CONFIGURATION.value, "configuration"
        )

    def test_publication_failure_channels_are_closed(self) -> None:
        result = mcp.PublishResult.model_validate(
            {
                "state": {"dirty": True, "unaccepted": False},
                "conflicts": ["src/config.ts"],
                "dependency_conflicts": [],
                "unaccepted": [],
            }
        )
        self.assertEqual(result.conflicts, [core.ProjectPath("src/config.ts")])

    def test_dependency_acceptance_is_bound_to_exact_digests(self) -> None:
        conflict = {
            "path": "src/config.ts",
            "observed": "a" * 64,
            "current": "b" * 64,
        }
        params = mcp.PublishParams(
            projection="rift://projection/prj_" + "b" * 26,
            accept_dependencies=[conflict],
        )
        self.assertEqual(params.accept_dependencies[0].current, core.Digest("b" * 64))
        with self.assertRaises(ValidationError):
            mcp.PublishParams(
                projection="rift://projection/prj_" + "b" * 26,
                accept_dependencies=[conflict, conflict],
            )


class RequestBudgetTests(TestCase):
    def test_request_budget_and_cursor_retention_are_advertised_and_bounded(
        self,
    ) -> None:
        limits(max_request_ms=1_000, max_active_cursors=1)
        limits(max_request_ms=3_600_000, max_active_cursors=1_024)
        for overrides in (
            {"max_request_ms": 999},
            {"max_request_ms": 3_600_001},
            {"max_active_cursors": 0},
            {"max_active_cursors": 1_025},
        ):
            with self.assertRaises(ValidationError):
                limits(**overrides)


class FactFamilyTests(TestCase):
    def test_families_are_exactly_the_provider_surface(self) -> None:
        self.assertEqual(
            {member.value for member in core.FactFamily},
            {"symbols", "nodes", "relationships", "types", "diagnostics", "history"},
        )


class NodesParamsTests(TestCase):
    def test_path_must_name_a_file(self) -> None:
        params = mcp.NodesParams.model_validate(
            {"path": "src/lib.rs", "position": 0}
        )
        self.assertEqual(params.path, core.ProjectPath("src/lib.rs"))

        with self.assertRaises(ValidationError):
            mcp.NodesParams.model_validate({"path": "", "position": 0})


class SourceDiscoveryTests(TestCase):
    def test_location_and_source_kind_are_independent(self) -> None:
        unit = core.SourceUnit.model_validate(
            {
                "id": "rift://source/src_" + "b" * 26,
                "location": {
                    "kind": "dependency",
                    "package": {
                        "manager": "cargo",
                        "name": "serde",
                        "version": "1.0.197",
                    },
                },
                "path": "src/lib.rs",
                "source_kind": "generated",
                "languages": [{"name": "rust", "dialect": None}],
                "digest": "a" * 64,
                "generator": "cargo build-script",
                "mappings": [],
            }
        )
        self.assertEqual(unit.source_kind, core.SourceKind.GENERATED)
        self.assertEqual(unit.location.root.package.name, "serde")

    def test_synthetic_declarations_have_no_source_unit(self) -> None:
        core.SymbolOrigin.model_validate(
            {"location": None, "source_kind": "synthetic", "unit": None}
        )
        with self.assertRaises(ValidationError):
            core.SourceUnit.model_validate(
                {
                    "id": "rift://source/src_" + "b" * 26,
                    "location": {"kind": "project", "package": None},
                    "path": "src/generated.rs",
                    "source_kind": "synthetic",
                    "languages": [{"name": "rust", "dialect": None}],
                    "digest": "a" * 64,
                    "generator": None,
                    "mappings": [],
                }
            )


class SearchTraversalTests(TestCase):
    def test_search_scope_defaults_and_path_constraint(self) -> None:
        search = mcp.SearchParams(query="BaseModel")
        symbol = mcp.GetSymbolParams(name="BaseModel")
        self.assertEqual(search.scope, mcp.SearchScope.PROJECT)
        self.assertEqual(symbol.scope, mcp.SearchScope.ALL)
        with self.assertRaises(ValidationError):
            mcp.SearchParams(
                query="BaseModel",
                scope="dependencies",
                paths={"include": ["src/**"], "exclude": []},
            )

    def test_symbol_hit_can_be_source_less(self) -> None:
        symbol = {
            "target": "symbol",
            "symbol": {
                "id": "rift://symbol/python/pydantic.main.BaseModel",
                "language": {"name": "python", "dialect": None},
                "name": "BaseModel",
                "kind": "class",
                "facets": ["type"],
                "origin": {
                    "location": {
                        "kind": "dependency",
                        "package": {
                            "manager": "pypi",
                            "name": "pydantic",
                            "version": "2.8.2",
                        },
                    },
                    "source_kind": "authored",
                    "unit": None,
                },
                "container": None,
                "modifiers": [],
                "visibility": "public",
                "types": [],
                "signatures": [],
                "documentation": [],
                "extensions": {},
                "document_local": False,
            },
        }
        mcp.SearchHit.model_validate(
            {"hit": symbol, "score": 1.0, "matched_by": ["name"]}
        )

    def test_traversal_defaults_and_bounds_are_part_of_the_contract(self) -> None:
        traversal = mcp.SearchTraversal.model_validate(
            {
                "seed": "rift://symbol/python/pkg.solver.fit",
                "intent": "edit_ripple",
            }
        )
        self.assertEqual(traversal.max_hops, 1)
        self.assertEqual(traversal.max_nodes, 25)
        self.assertIsNone(traversal.direction)

        for overrides in (
            {"max_hops": 0},
            {"max_hops": 3},
            {"max_nodes": 0},
            {"max_nodes": 101},
            {"facets": []},
        ):
            with self.assertRaises(ValidationError):
                mcp.SearchTraversal.model_validate(
                    {
                        "seed": "rift://symbol/python/pkg.solver.fit",
                        "intent": "edit_ripple",
                        **overrides,
                    }
                )

    def test_search_and_hits_carry_the_graph_request_and_audit_path(self) -> None:
        search = mcp.SearchParams.model_json_schema()
        self.assertIn("traversal", search["properties"])
        self.assertIn(
            {
                "description": "Satisfied by a bounded relationship traversal.",
                "required": ["traversal"],
            },
            search["anyOf"],
        )
        hit = mcp.SearchHit.model_json_schema()
        self.assertEqual(
            set(hit["required"]),
            {"hit", "score", "matched_by"},
        )
        self.assertIn("path", hit["properties"])
        self.assertIn("distance", hit["properties"])

    def test_traversal_requires_symbol_capable_target(self) -> None:
        base = {
            "traversal": {
                "seed": "rift://symbol/python/pkg.solver.fit",
                "intent": "trace",
            }
        }
        mcp.SearchParams.model_validate(base)
        mcp.SearchParams.model_validate({**base, "target": "symbol"})
        for target in ("node", "file"):
            with self.assertRaises(ValidationError):
                mcp.SearchParams.model_validate({**base, "target": target})

    def test_graph_hit_correlates_path_distance_and_match_kind(self) -> None:
        symbol = {
            "target": "symbol",
            "symbol": {
                "id": "rift://symbol/python/pkg.solver.fit",
                "language": {"name": "python", "dialect": None},
                "name": "fit",
                "kind": "function",
                "facets": ["callable", "test"],
                "origin": {
                    "location": {"kind": "project", "package": None},
                    "source_kind": "authored",
                    "unit": "rift://source/src_" + "b" * 26,
                },
                "container": None,
                "modifiers": [],
                "visibility": None,
                "types": [],
                "signatures": [],
                "documentation": [],
                "extensions": {},
                "document_local": False,
            },
        }
        relationship = {
            "from": "rift://symbol/python/pkg.solver.caller",
            "kind": "call",
            "facets": ["calls"],
            "to": "rift://symbol/python/pkg.solver.fit",
            "evidence": [],
            "derivation": "syntax",
            "confidence": None,
            "extensions": {},
        }
        base = {
            "hit": symbol,
            "score": 1.0,
            "matched_by": ["relationship"],
            "span": {
                "unit": "rift://source/src_" + "b" * 26,
                "range": {"start": 0, "end": 3},
            },
            "line": 1,
            "path": [{"relationship": relationship, "direction": "incoming"}],
            "distance": 1,
        }
        mcp.SearchHit.model_validate(base)
        for override in (
            {"distance": None},
            {"distance": 2},
            {"matched_by": ["name"]},
        ):
            with self.assertRaises(ValidationError):
                mcp.SearchHit.model_validate({**base, **override})


class ConfigRangeTests(TestCase):
    def test_declared_ranges_match_load_time_enforcement(self) -> None:
        schema = config_schema_output()
        execution = schema["$defs"]["ExecutionConfig"]["properties"]
        ranged = {
            name: field["rift:range"]
            for name, field in execution.items()
            if "rift:range" in field
        }
        self.assertEqual(
            set(ranged),
            {
                "max_code",
                "max_timeout",
                "max_output",
            },
        )
        for name, bounds in ranged.items():
            config.ExecutionConfig.model_validate({name: bounds["max"]})
            amount, unit = self._split(bounds["max"])
            with self.assertRaises(
                ValidationError, msg=f"{name} accepts more than {bounds['max']}"
            ):
                config.ExecutionConfig.model_validate({name: f"{amount + 1}{unit}"})

    @staticmethod
    def _split(value: str) -> tuple[int, str]:
        digits = "".join(ch for ch in value if ch.isdigit())
        return int(digits), value[len(digits) :]
