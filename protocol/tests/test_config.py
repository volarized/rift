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
from rift.models import config, mcp

WORKSPACE = Path(__file__).parents[2]


class RiftConfigTests(TestCase):
    def test_workspace_file_is_the_typed_model(self) -> None:
        parsed = validate_rift_toml(WORKSPACE / "rift.toml")

        self.assertEqual(parsed.validators.commands, [])
        self.assertEqual(parsed.execution.allow, [])
        self.assertEqual(parsed.execution.max_timeout.milliseconds, 30_000)

    def test_tables_are_closed(self) -> None:
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate({"session": {"base": "head"}})

    def test_language_selector_produces_the_protocol_type(self) -> None:
        language = config.LanguageSelector("sql:postgresql").to_language()

        self.assertEqual(language.name, "sql")
        self.assertEqual(language.dialect, "postgresql")

    def test_validator_config_holds_exact_unique_declarations(self) -> None:
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
        validators = config.ValidatorsConfig.model_validate(
            {"commands": [declaration.model_dump()]}
        )

        self.assertEqual(validators.commands, [declaration])
        with self.assertRaises(ValidationError):
            config.ValidatorsConfig.model_validate(
                {"commands": [declaration.model_dump(), declaration.model_dump()]}
            )

    def test_adapter_config_does_not_expose_internal_process_limits(self) -> None:
        with self.assertRaises(ValidationError):
            config.AdapterProcessConfig.model_validate(
                {"command": ["rift-adapter-python"], "state-cap": 4}
            )

    def test_generated_schema_has_live_protocol_targets(self) -> None:
        schema = config_schema_output()
        content = json.dumps(schema)

        validate_config_schema(content)
        self.assertEqual(
            set(schema["properties"]),
            {"execution", "validation", "validators", "adapters"},
        )
