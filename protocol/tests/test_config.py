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
        parsed = validate_rift_toml(WORKSPACE / "docs" / "rift.toml")

        self.assertEqual(parsed.hooks, [])
        self.assertEqual(parsed.execution.allow, [])
        self.assertEqual(parsed.execution.max_timeout.milliseconds, 30_000)
        self.assertIsNone(parsed.search.embedding)

    def test_tables_are_closed(self) -> None:
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate({"session": {"base": "head"}})

    def test_language_selector_produces_the_protocol_type(self) -> None:
        language = config.LanguageSelector("sql:postgresql").to_language()

        self.assertEqual(language.name, "sql")
        self.assertEqual(language.dialect, "postgresql")

    def test_hooks_hold_exact_unique_declarations(self) -> None:
        declaration = mcp.Hook.model_validate(
            {
                "type": "command",
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
        parsed = config.RiftConfig.model_validate({"hooks": [declaration.model_dump()]})

        self.assertEqual(parsed.hooks, [declaration])
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate(
                {"hooks": [declaration.model_dump(), declaration.model_dump()]}
            )

    def test_search_embedding_names_one_model(self) -> None:
        parsed = config.RiftConfig.model_validate(
            {"search": {"embedding": "potion-retrieval-32M"}}
        )
        self.assertEqual(parsed.search.embedding, "potion-retrieval-32M")
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate({"search": {"embedding": ""}})
        with self.assertRaises(ValidationError):
            config.RiftConfig.model_validate({"search": {"model": "x"}})

    def test_generated_schema_has_live_protocol_targets(self) -> None:
        schema = config_schema_output()
        content = json.dumps(schema)

        validate_config_schema(content)
        self.assertEqual(
            set(schema["properties"]),
            {"execution", "providers", "search", "hooks"},
        )
