"""Executable contracts for invariants the specification pins."""

from unittest import TestCase

from pydantic import ValidationError

from rift.generate import config_schema_output
from rift.models import adapter, config, core, mcp, scip_api


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
        "max_rewrite_expansions": 1_000,
        "execution": None,
        "max_filter_depth": 16,
        "max_request_ms": 60_000,
        "max_active_cursors": 16,
    }
    values.update(overrides)
    return mcp.Limits.model_validate(values)


class SessionIdentityTests(TestCase):
    def test_session_identity_is_owned_by_core(self) -> None:
        self.assertNotIn("SessionId", {model.__name__ for model in mcp.MODELS})
        self.assertIn("SessionId", {model.__name__ for model in core.MODELS})

    def test_session_identity_shape(self) -> None:
        core.SessionId.model_validate("ses_k2mq4vx6ntbwpj3rfd6a2zcyhe")
        for bad in ("ses_UPPER", "ses_short", "chg_k2mq4vx6ntbwpj3rfd6a2zcyhe", ""):
            with self.assertRaises(ValidationError):
                core.SessionId.model_validate(bad)

    def test_scip_surface_carries_the_typed_identity(self) -> None:
        self.assertIs(
            scip_api.Request.model_fields["session"].annotation, core.SessionId
        )
        self.assertIs(
            scip_api.Header.model_fields["session"].annotation, core.SessionId
        )


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
                "unaccepted": [],
            }
        )
        self.assertEqual(result.conflicts, [core.ProjectPath("src/config.ts")])


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


class AdapterLimitTests(TestCase):
    @staticmethod
    def fixture() -> dict[str, int]:
        return {
            "max_message_bytes": 4_194_304,
            "max_in_flight": 4,
            "max_in_flight_per_state": 1,
            "max_states": 4,
        }

    def test_declared_capacities_have_floors(self) -> None:
        adapter.AdapterLimits.model_validate(self.fixture())
        for field in ("max_in_flight", "max_in_flight_per_state", "max_states"):
            with self.assertRaises(ValidationError):
                adapter.AdapterLimits.model_validate({**self.fixture(), field: 0})


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
                "debug_idle_timeout",
                "max_debug_value",
            },
        )
        for name, bounds in ranged.items():
            config.ExecutionConfig.model_validate({name: bounds["max"]})
            amount, unit = self._split(bounds["max"])
            with self.assertRaises(
                ValidationError, msg=f"{name} accepts more than {bounds['max']}"
            ):
                config.ExecutionConfig.model_validate(
                    {name: f"{amount + 1}{unit}"}
                )

    @staticmethod
    def _split(value: str) -> tuple[int, str]:
        digits = "".join(ch for ch in value if ch.isdigit())
        return int(digits), value[len(digits) :]


class ScipSurfaceTests(TestCase):
    def test_language_coverage_is_part_of_the_export(self) -> None:
        names = {model.__name__ for model in scip_api.SCIP_API_PACKAGE.models}
        self.assertIn("LanguageCoverage", names)
        self.assertEqual(
            scip_api.Header.model_fields["coverage"].annotation,
            list[scip_api.LanguageCoverage],
        )
