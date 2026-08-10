"""Executable contracts for the typed ``rift.toml`` model."""

import json
from pathlib import Path
from unittest import TestCase

from pydantic import ValidationError

from rift.generate import (
    config_schema_output,
    validate_config_schema,
    validate_rift_toml,
)
from rift.models import adapter, config, mcp

REPOSITORY = Path(__file__).parents[2]


class RiftConfigTests(TestCase):
    def test_repository_file_is_the_typed_model(self) -> None:
        parsed = validate_rift_toml(REPOSITORY / "rift.toml")

        self.assertEqual(parsed.profile.max, mcp.ConformanceProfile.EDIT)
        self.assertEqual(parsed.validators.max_timeout.milliseconds, 300_000)
        self.assertEqual(parsed.execution.allow, [])
        self.assertEqual(parsed.execution.max_timeout.milliseconds, 30_000)

    def test_tables_are_closed(self) -> None:
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate(
                {"server": {"idle-timeout": "10m", "typo": 1}}
            )

    def test_language_selector_produces_the_protocol_type(self) -> None:
        language = config.LanguageSelector("sql:postgresql").to_language()

        self.assertEqual(language.name, "sql")
        self.assertEqual(language.dialect, "postgresql")

    def test_validator_config_holds_exact_declarations_within_its_timeout(self) -> None:
        declaration = mcp.CommandValidator.model_validate(
            {
                "id": "tests",
                "kind": "test",
                "argv": ["pytest", "-q"],
                "changed_paths": "none",
                "working_directory": "",
                "environment": {},
                "timeout_ms": 2_000,
                "output_limit_bytes": 256,
                "guarantees": [],
                "determinism": "deterministic",
            }
        )
        policy = config.ValidatorsConfig.model_validate(
            {"commands": [declaration.model_dump()], "max-timeout": "2s"}
        )

        self.assertEqual(policy.commands, [declaration])
        with self.assertRaises(ValidationError):
            config.ValidatorsConfig.model_validate(
                {
                    "commands": [{**declaration.model_dump(), "timeout_ms": 2_001}],
                    "max-timeout": "2s",
                }
            )
        too_slow = mcp.CommandValidator.model_validate(
            {**declaration.model_dump(), "timeout_ms": 2_001}
        )
        self.assertEqual(too_slow.timeout_ms, 2_001)

    def test_repository_cap_combines_with_the_advertised_adapter_limit(self) -> None:
        process = config.AdapterProcessConfig(
            command=["rift-adapter-python"], state_cap=4
        )
        advertised = adapter.AdapterLimits(
            max_message_bytes=65_536,
            max_in_flight=8,
            max_in_flight_per_state=2,
            max_states=6,
        )

        self.assertEqual(process.effective_state_cap(advertised), 4)

    def test_generated_schema_has_live_protocol_targets(self) -> None:
        content = json.dumps(config_schema_output())

        validate_config_schema(content)
