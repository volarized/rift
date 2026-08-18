"""Executable contracts for execution admission, budgets, and protocol surface."""

from unittest import TestCase

from pydantic import ValidationError

from rift.models import config, core, mcp
from rift.models.document import DOCUMENT


class ExecutionContractTests(TestCase):
    def test_default_configuration_admits_no_execution(self) -> None:
        execution = config.ExecutionConfig()
        language = core.Language(name="python")

        self.assertIsNone(execution.advertised_limits())
        self.assertFalse(execution.permits_execution(language, runtime=True))

    def test_execution_admission_intersects_configuration_and_runtime(self) -> None:
        execution = config.ExecutionConfig.model_validate({"allow": ["python"]})
        language = core.Language(name="python")
        other = core.Language(name="typescript")

        self.assertFalse(execution.permits_execution(language, runtime=False))
        self.assertTrue(execution.permits_execution(language, runtime=True))
        self.assertFalse(execution.permits_execution(other, runtime=True))

    def test_configuration_produces_runtime_budgets_and_public_limits(self) -> None:
        execution = config.ExecutionConfig.model_validate(
            {
                "allow": ["python"],
                "max_code": "4KiB",
                "max_timeout": "2s",
                "max_output": "1KiB",
                "max_concurrent": 3,
            }
        )

        self.assertEqual(
            execution.execution_budget(),
            core.ExecutionBudget(timeout_ms=2_000, output_bytes=1_024),
        )
        self.assertEqual(
            execution.advertised_limits(),
            mcp.ExecutionLimits(
                max_code_bytes=4_096,
                budget=core.ExecutionBudget(timeout_ms=2_000, output_bytes=1_024),
                max_concurrent=3,
            ),
        )

    def test_execute_is_the_only_execution_tool(self) -> None:
        execution_tools = [
            tool.name for tool in DOCUMENT.tools if tool.group == "execution"
        ]
        self.assertEqual(execution_tools, ["execute"])
        mcp_tools = {tool.name: tool.rpc.name for tool in DOCUMENT.tools}
        self.assertEqual(mcp_tools["execute"], "Execute")

    def test_captured_output_uses_the_shared_bounded_text_shape(self) -> None:
        value = core.CapturedText(
            text="<large value>",
            captured_bytes=13,
            total_bytes=100,
            truncated=True,
            digest="0" * 64,
        )
        self.assertTrue(value.truncated)
        self.assertEqual(value.total_bytes, 100)

        with self.assertRaises(ValidationError):
            core.CapturedText(
                text="complete",
                captured_bytes=8,
                total_bytes=9,
                truncated=False,
                digest="0" * 64,
            )
