"""Typed source model for ``rift.toml``."""

from __future__ import annotations

import re
from typing import Annotated, Any

from pydantic import BaseModel, ConfigDict, Field, RootModel, model_validator

from . import core, mcp
from .base import FieldRef

DURATION_PATTERN = r"^(?:0|[1-9][0-9]*)(?:ms|s|m|h|d)$"
BYTE_SIZE_PATTERN = r"^(?:0|[1-9][0-9]*)(?:B|KiB|MiB|GiB|TiB)$"
LANGUAGE_SELECTOR_PATTERN = (
    r"^[a-z][a-z0-9._-]{0,63}(?::[a-z][a-z0-9._-]{0,63})?$"
)
EMBEDDING_MODEL_PATTERN = r"^[A-Za-z0-9][A-Za-z0-9._/:-]{0,255}$"


def _field_target(reference: FieldRef[Any, Any]) -> dict[str, str]:
    """Serialize a checked model-field reference into config-schema metadata."""

    return {"model": reference.owner.__name__, "field": reference.name}


class ConfigModel(BaseModel):
    """Closed TOML table. Keys are snake_case, matching the protocol's wire names."""

    model_config = ConfigDict(
        extra="forbid",
        populate_by_name=True,
        validate_default=True,
    )


class Duration(RootModel[str]):
    """A non-negative duration with one explicit unit."""

    root: Annotated[
        str,
        Field(
            pattern=DURATION_PATTERN,
            examples=["250ms", "60s", "10m", "2h"],
        ),
    ]

    @property
    def milliseconds(self) -> int:
        match = re.fullmatch(r"([0-9]+)(ms|s|m|h|d)", self.root)
        if match is None:  # pragma: no cover - Pydantic enforces the pattern
            raise ValueError(f"invalid duration {self.root!r}")
        value, unit = match.groups()
        scale = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000, "d": 86_400_000}
        return int(value) * scale[unit]


class ByteSize(RootModel[str]):
    """A non-negative byte size using binary units where a suffix is present."""

    root: Annotated[
        str,
        Field(
            pattern=BYTE_SIZE_PATTERN,
            examples=["0B", "256MiB", "4GiB"],
        ),
    ]

    @property
    def bytes(self) -> int:
        match = re.fullmatch(r"([0-9]+)(B|KiB|MiB|GiB|TiB)", self.root)
        if match is None:  # pragma: no cover - Pydantic enforces the pattern
            raise ValueError(f"invalid byte size {self.root!r}")
        value, unit = match.groups()
        scale = {"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30, "TiB": 1 << 40}
        return int(value) * scale[unit]


class LanguageSelector(RootModel[str]):
    """The TOML spelling of the same exact language pair represented by ``core.Language``."""

    root: Annotated[
        str,
        Field(
            pattern=LANGUAGE_SELECTOR_PATTERN,
            examples=["python", "sql:postgresql", "css:scss"],
        ),
    ]

    def to_language(self) -> core.Language:
        name, separator, dialect = self.root.partition(":")
        return core.Language(name=name, dialect=dialect if separator else None)


AllowedLanguages = Annotated[
    list[LanguageSelector],
    Field(json_schema_extra={"uniqueItems": True}),
]


class ExecutionConfig(ConfigModel):
    """Workspace enablement and limits for caller-provided code."""

    allow: AllowedLanguages = Field(
        default_factory=list,
        description=(
            "Exact languages enabled for execute when a runtime serves them. Empty disables "
            "caller-provided code."
        ),
        json_schema_extra={"rift:selectsType": core.Language.__name__},
    )
    max_code: ByteSize = Field(
        default=ByteSize("16KiB"),
        description="Maximum UTF-8 bytes in one CodeBlock.source.",
        json_schema_extra={
            "rift:bounds": _field_target(core.CodeBlock.source),
            "rift:conversion": "UTF-8 byte length",
            "rift:range": {"min": "1B", "max": "32KiB"},
        },
    )
    max_timeout: Duration = Field(
        default=Duration("30s"),
        description="Wall-clock bound applied to each evaluation.",
        json_schema_extra={
            "rift:bounds": _field_target(core.ExecutionBudget.timeout_ms),
            "rift:conversion": "Duration.milliseconds",
            "rift:range": {"min": "1ms", "max": "1d"},
        },
    )
    max_output: ByteSize = Field(
        default=ByteSize("8KiB"),
        description="Captured prefix bound applied separately to stdout and stderr.",
        json_schema_extra={
            "rift:bounds": _field_target(core.ExecutionBudget.output_bytes),
            "rift:conversion": "ByteSize.bytes",
            "rift:range": {"min": "0B", "max": "16KiB"},
        },
    )
    max_concurrent: int = Field(
        default=2,
        ge=1,
        le=64,
        description="Evaluations admitted concurrently across the workspace.",
    )

    @model_validator(mode="after")
    def configuration_is_well_formed(self) -> ExecutionConfig:
        allowed = [language.root for language in self.allow]
        if len(allowed) != len(set(allowed)):
            raise ValueError("execution.allow languages must be unique")
        if not 1 <= self.max_code.bytes <= 32768:
            raise ValueError("execution.max_code must be between 1B and 32KiB")
        if not 1 <= self.max_timeout.milliseconds <= 86_400_000:
            raise ValueError("execution.max_timeout must be between 1ms and 1d")
        if not 0 <= self.max_output.bytes <= 16384:
            raise ValueError("execution.max_output must be at most 16KiB")
        return self

    @staticmethod
    def _selector(language: core.Language) -> str:
        return (
            f"{language.name}:{language.dialect}"
            if language.dialect is not None
            else language.name
        )

    def permits_execution(self, language: core.Language, runtime: bool) -> bool:
        """Whether workspace configuration and a serving runtime admit execute."""

        allowed = {entry.root for entry in self.allow}
        return self._selector(language) in allowed and runtime

    def execution_budget(self) -> core.ExecutionBudget:
        return core.ExecutionBudget(
            timeout_ms=self.max_timeout.milliseconds,
            output_bytes=self.max_output.bytes,
        )

    def advertised_limits(self) -> mcp.ExecutionLimits | None:
        """Public ceilings, or null when no language is enabled for execution."""

        if not self.allow:
            return None
        return mcp.ExecutionLimits(
            max_code_bytes=self.max_code.bytes,
            budget=self.execution_budget(),
            max_concurrent=self.max_concurrent,
        )


class HistoryProviderConfig(ConfigModel):
    """The built-in history provider, which reads the workspace's version-control history
    in place — no checkout, no subprocess."""

    enabled: bool = Field(
        default=True,
        description=(
            "Whether the history fact family is served at all. A workspace without version "
            "control reports the family `not_applicable` regardless."
        ),
    )
    max_revisions: int = Field(
        default=500,
        ge=1,
        le=100000,
        description=(
            "Most revisions the history walk crosses from the current head. Timelines and "
            "co-change coupling are computed inside this depth, and `SymbolHistory.coverage` "
            "reports partial when the walk ends before a symbol's introduction."
        ),
    )


class ProvidersConfig(ConfigModel):
    """Built-in providers. Each is on by default and configured here only to bound or
    disable it."""

    history: HistoryProviderConfig = Field(
        default_factory=HistoryProviderConfig,
        description="The version-control history provider.",
    )


class SearchConfig(ConfigModel):
    """The optional dense-ranking backend for the `search` tool. The lexical index runs
    without configuration; this table names the embedding model that adds dense ranking
    on top of it."""

    embedding: str | None = Field(
        default=None,
        pattern=EMBEDDING_MODEL_PATTERN,
        description=(
            "Identifier of the embedding model that ranks search hits alongside the "
            "lexical index. Null keeps search lexical. Vectors are stored per model, so "
            "changing this value rebuilds the dense index."
        ),
        examples=["potion-retrieval-32M"],
    )


class RiftConfig(ConfigModel):
    """Workspace behavior loaded from the workspace-root ``rift.toml``."""

    execution: ExecutionConfig = Field(default_factory=ExecutionConfig)
    providers: ProvidersConfig = Field(
        default_factory=ProvidersConfig,
        description="Bounds and switches for the built-in providers.",
    )
    search: SearchConfig = Field(
        default_factory=SearchConfig,
        description="Ranking backends for the `search` tool.",
    )
    hooks: list[mcp.Hook] = Field(
        default_factory=list,
        description="Hooks run in the changed tree each time a change applies, in list order.",
    )

    @model_validator(mode="after")
    def hook_ids_are_unique(self) -> RiftConfig:
        ids = [hook.root.id for hook in self.hooks]
        if len(ids) != len(set(ids)):
            raise ValueError("hooks ids must be unique")
        return self


__all__ = [
    "ByteSize",
    "Duration",
    "ExecutionConfig",
    "HistoryProviderConfig",
    "LanguageSelector",
    "ProvidersConfig",
    "RiftConfig",
    "SearchConfig",
]
