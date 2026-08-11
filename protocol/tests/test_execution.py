"""Executable contracts for execution admission, budgets, and protocol surface."""

from unittest import TestCase

from pydantic import ValidationError

from rift.models import adapter, config, core, mcp
from rift.models.document import DOCUMENT


class ExecutionContractTests(TestCase):
    @staticmethod
    def capabilities(
        operations: list[core.AdapterOperation],
    ) -> adapter.Capabilities:
        return adapter.Capabilities(
            language=core.Language(name="python"),
            implementation="test-adapter",
            source_claims=[],
            auxiliary_claims=[],
            write_claims=[],
            kinds=[],
            operations=operations,
            fact_families=[],
            action_kinds=[],
            extension_values=[],
            limits=adapter.AdapterLimits(
                max_message_bytes=4_194_304,
                max_in_flight=4,
                max_in_flight_per_state=1,
                max_states=4,
            ),
            runtime=adapter.RuntimeRequirements(
                executes_workspace_code=True,
                spawns_subprocesses=True,
            ),
            protocol_versions=["2026-08-04"],
        )

    def test_default_configuration_admits_no_execution(self) -> None:
        execution = config.ExecutionConfig()
        language = core.Language(name="python")

        self.assertIsNone(execution.advertised_limits())
        self.assertFalse(
            execution.permits_execution(language, [core.AdapterOperation.EXECUTE])
        )
        self.assertFalse(
            execution.permits_debugging(
                language,
                [
                    core.AdapterOperation.DEBUG_START,
                    core.AdapterOperation.DEBUG_GET_FRAME,
                    core.AdapterOperation.DEBUG_STOP,
                ],
            )
        )

    def test_debug_enablement_is_a_subset_of_execution(self) -> None:
        with self.assertRaises(ValidationError):
            config.ExecutionConfig.model_validate(
                {"allow": ["python"], "debug": ["typescript"]}
            )

    def test_execution_admission_intersects_configuration_and_adapter_operations(
        self,
    ) -> None:
        execution = config.ExecutionConfig.model_validate(
            {"allow": ["python"], "debug": ["python"]}
        )
        language = core.Language(name="python")

        self.assertFalse(execution.permits_execution(language, []))
        self.assertTrue(
            execution.permits_execution(language, [core.AdapterOperation.EXECUTE])
        )
        self.assertFalse(
            execution.permits_debugging(
                language,
                [
                    core.AdapterOperation.DEBUG_START,
                    core.AdapterOperation.DEBUG_GET_FRAME,
                ],
            )
        )
        self.assertTrue(
            execution.permits_debugging(
                language,
                [
                    core.AdapterOperation.DEBUG_START,
                    core.AdapterOperation.DEBUG_GET_FRAME,
                    core.AdapterOperation.DEBUG_STOP,
                ],
            )
        )

    def test_adapter_debug_capability_is_all_or_none(self) -> None:
        with self.assertRaises(ValidationError):
            self.capabilities(
                [
                    core.AdapterOperation.DEBUG_START,
                    core.AdapterOperation.DEBUG_GET_FRAME,
                ]
            )

        complete = self.capabilities(
            [
                core.AdapterOperation.DEBUG_START,
                core.AdapterOperation.DEBUG_GET_FRAME,
                core.AdapterOperation.DEBUG_STOP,
            ]
        )
        self.assertEqual(len(complete.operations), 3)

    def test_configuration_produces_adapter_budgets_and_public_limits(self) -> None:
        execution = config.ExecutionConfig.model_validate(
            {
                "allow": ["python"],
                "debug": ["python"],
                "max_code": "4KiB",
                "max_timeout": "2s",
                "max_output": "1KiB",
                "max_concurrent": 3,
                "max_debug_sessions": 2,
                "debug_idle_timeout": "30s",
                "max_debug_frames": 8,
                "max_debug_bindings": 12,
                "max_debug_value": "256B",
            }
        )

        self.assertEqual(
            execution.execution_budget(),
            core.ExecutionBudget(timeout_ms=2_000, output_bytes=1_024),
        )
        self.assertEqual(
            execution.debug_budget(),
            core.DebugBudget(
                execution=core.ExecutionBudget(timeout_ms=2_000, output_bytes=1_024),
                frames=8,
                bindings_per_frame=12,
                value_bytes=256,
            ),
        )
        self.assertEqual(
            execution.advertised_limits(),
            mcp.ExecutionLimits(
                max_code_bytes=4_096,
                budget=core.ExecutionBudget(timeout_ms=2_000, output_bytes=1_024),
                max_concurrent=3,
                debug=mcp.DebugLimits(
                    max_sessions=2,
                    idle_timeout_ms=30_000,
                    budget=core.DebugBudget(
                        execution=core.ExecutionBudget(
                            timeout_ms=2_000, output_bytes=1_024
                        ),
                        frames=8,
                        bindings_per_frame=12,
                        value_bytes=256,
                    ),
                ),
            ),
        )

    def test_mcp_and_adapter_surfaces_keep_the_same_lifecycle(self) -> None:
        mcp_tools = {tool.name: tool.rpc.name for tool in DOCUMENT.tools}
        self.assertEqual(mcp_tools["execute"], "Execute")
        self.assertEqual(mcp_tools["debug_start"], "DebugStart")
        self.assertEqual(mcp_tools["debug_get_frame"], "DebugGetFrame")
        self.assertEqual(mcp_tools["debug_stop"], "DebugStop")

        adapter_rpcs = {
            rpc.name
            for service in adapter.ADAPTER_PACKAGE.services
            for rpc in service.rpcs
        }
        self.assertTrue(
            {"Execute", "DebugStart", "DebugGetFrame", "DebugStop"}.issubset(
                adapter_rpcs
            )
        )

    def test_debug_values_use_the_shared_bounded_text_shape(self) -> None:
        value = core.CapturedText(
            text="<large value>",
            captured_bytes=13,
            total_bytes=100,
            truncated=True,
            digest="0" * 64,
        )
        binding = core.DebugBinding(name="value", type="Object", value=value)

        self.assertTrue(binding.value.truncated)
        self.assertEqual(binding.value.total_bytes, 100)

        with self.assertRaises(ValidationError):
            core.CapturedText(
                text="complete",
                captured_bytes=8,
                total_bytes=9,
                truncated=False,
                digest="0" * 64,
            )
