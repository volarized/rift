"""Typed source model for ``rift.toml``."""

from __future__ import annotations

import re
from enum import Enum
from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, RootModel, model_validator

from . import adapter, core, mcp
from .base import FieldRef

DURATION_PATTERN = r"^(?:0|[1-9][0-9]*)(?:ms|s|m|h|d)$"
BYTE_SIZE_PATTERN = r"^(?:0|[1-9][0-9]*)(?:B|KiB|MiB|GiB|TiB)$"
LANGUAGE_SELECTOR_PATTERN = (
    r"^[A-Za-z][A-Za-z0-9._-]{0,63}(?::[A-Za-z][A-Za-z0-9._-]{0,63})?$"
)
PROCESS_NAME_PATTERN = r"^[A-Za-z][A-Za-z0-9._-]{0,63}$"
ENVIRONMENT_NAME_PATTERN = r"^[A-Za-z_][A-Za-z0-9_]*$"


def _field_target(reference: FieldRef[Any, Any]) -> dict[str, str]:
    """Serialize a checked model-field reference into config-schema metadata."""

    return {"model": reference.owner.__name__, "field": reference.name}


class ConfigModel(BaseModel):
    """Closed TOML table with aliases matching the file's kebab-case keys."""

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
            examples=["rust", "sql:postgresql", "css:scss"],
        ),
    ]

    def to_language(self) -> core.Language:
        name, separator, dialect = self.root.partition(":")
        return core.Language(name=name, dialect=dialect if separator else None)


class SessionBase(str, Enum):
    """Source state from which a new MCP process starts its session."""

    HEAD = "head"


class SessionConfig(ConfigModel):
    """Initial state for each MCP session."""

    base: SessionBase = Field(
        default=SessionBase.HEAD,
        description="State imported into the session projection. `head` imports the resolved HEAD tree.",
    )


AllowedLanguages = Annotated[
    list[LanguageSelector],
    Field(json_schema_extra={"uniqueItems": True}),
]


class ExecutionConfig(ConfigModel):
    """Workspace enablement and limits for caller-provided code."""

    allow: AllowedLanguages = Field(
        default_factory=list,
        description=(
            "Exact languages enabled for execute when their adapters advertise the execute "
            "operation. Empty disables caller-provided code."
        ),
        json_schema_extra={"rift:selectsType": core.Language.__name__},
    )
    debug: AllowedLanguages = Field(
        default_factory=list,
        description=(
            "Subset of allow enabled for debugging when all three debug operations are "
            "advertised. Empty disables debugging."
        ),
        json_schema_extra={"rift:selectsType": core.Language.__name__},
    )
    max_code: ByteSize = Field(
        default=ByteSize("16KiB"),
        alias="max-code",
        description="Maximum UTF-8 bytes in one CodeBlock.source.",
        json_schema_extra={
            "rift:bounds": _field_target(core.CodeBlock.source),
            "rift:conversion": "UTF-8 byte length",
        },
    )
    max_timeout: Duration = Field(
        default=Duration("30s"),
        alias="max-timeout",
        description="Wall-clock bound applied to each execution and debugging evaluation.",
        json_schema_extra={
            "rift:bounds": _field_target(core.ExecutionBudget.timeout_ms),
            "rift:conversion": "Duration.milliseconds",
        },
    )
    max_output: ByteSize = Field(
        default=ByteSize("8KiB"),
        alias="max-output",
        description="Captured prefix bound applied separately to stdout and stderr.",
        json_schema_extra={
            "rift:bounds": _field_target(core.ExecutionBudget.output_bytes),
            "rift:conversion": "ByteSize.bytes",
        },
    )
    max_concurrent: int = Field(
        default=2,
        alias="max-concurrent",
        ge=1,
        le=64,
        description="Evaluations admitted concurrently across the workspace.",
    )
    max_debug_sessions: int = Field(
        default=2,
        alias="max-debug-sessions",
        ge=1,
        le=16,
        description="Retained debugging sessions admitted across the workspace.",
    )
    debug_idle_timeout: Duration = Field(
        default=Duration("5m"),
        alias="debug-idle-timeout",
        description="Idle time before Rift stops a debugging session and releases its lease.",
    )
    max_debug_frames: int = Field(
        default=64,
        alias="max-debug-frames",
        ge=1,
        le=256,
        description="Stack frames one failed debugging evaluation may retain.",
        json_schema_extra={"rift:bounds": _field_target(core.DebugBudget.frames)},
    )
    max_debug_bindings: int = Field(
        default=32,
        alias="max-debug-bindings",
        ge=1,
        le=128,
        description="Arguments plus locals returned for one debug frame.",
        json_schema_extra={
            "rift:bounds": _field_target(core.DebugBudget.bindings_per_frame)
        },
    )
    max_debug_value: ByteSize = Field(
        default=ByteSize("1KiB"),
        alias="max-debug-value",
        description="Captured UTF-8 bytes in each adapter-rendered debug binding value.",
        json_schema_extra={
            "rift:bounds": _field_target(core.DebugBudget.value_bytes),
            "rift:conversion": "ByteSize.bytes",
        },
    )

    @model_validator(mode="after")
    def configuration_is_well_formed(self) -> ExecutionConfig:
        allowed = [language.root for language in self.allow]
        debugged = [language.root for language in self.debug]
        if len(allowed) != len(set(allowed)):
            raise ValueError("execution.allow languages must be unique")
        if len(debugged) != len(set(debugged)):
            raise ValueError("execution.debug languages must be unique")
        if not set(debugged).issubset(allowed):
            raise ValueError("execution.debug must be a subset of execution.allow")
        if not 1 <= self.max_code.bytes <= 32768:
            raise ValueError("execution.max-code must be between 1B and 32KiB")
        if not 1 <= self.max_timeout.milliseconds <= 86_400_000:
            raise ValueError("execution.max-timeout must be between 1ms and 1d")
        if not 0 <= self.max_output.bytes <= 16384:
            raise ValueError("execution.max-output must be at most 16KiB")
        if not 1 <= self.debug_idle_timeout.milliseconds <= 86_400_000:
            raise ValueError("execution.debug-idle-timeout must be between 1ms and 1d")
        if not 0 <= self.max_debug_value.bytes <= 8192:
            raise ValueError("execution.max-debug-value must be at most 8KiB")
        return self

    @staticmethod
    def _selector(language: core.Language) -> str:
        return (
            f"{language.name}:{language.dialect}"
            if language.dialect is not None
            else language.name
        )

    def permits_execution(
        self, language: core.Language, operations: list[core.AdapterOperation]
    ) -> bool:
        """Whether workspace configuration and adapter capability admit execute."""

        allowed = {entry.root for entry in self.allow}
        return (
            self._selector(language) in allowed
            and core.AdapterOperation.EXECUTE in operations
        )

    def permits_debugging(
        self, language: core.Language, operations: list[core.AdapterOperation]
    ) -> bool:
        """Whether workspace configuration and the complete adapter operation triplet admit debugging."""

        debugged = {entry.root for entry in self.debug}
        required = {
            core.AdapterOperation.DEBUG_START,
            core.AdapterOperation.DEBUG_GET_FRAME,
            core.AdapterOperation.DEBUG_STOP,
        }
        return self._selector(language) in debugged and required.issubset(operations)

    def execution_budget(self) -> core.ExecutionBudget:
        return core.ExecutionBudget(
            timeout_ms=self.max_timeout.milliseconds,
            output_bytes=self.max_output.bytes,
        )

    def debug_budget(self) -> core.DebugBudget:
        return core.DebugBudget(
            execution=self.execution_budget(),
            frames=self.max_debug_frames,
            bindings_per_frame=self.max_debug_bindings,
            value_bytes=self.max_debug_value.bytes,
        )

    def advertised_limits(self) -> mcp.ExecutionLimits | None:
        """Public ceilings, or null when no language is enabled for execution."""

        if not self.allow:
            return None
        debug = None
        if self.debug:
            debug = mcp.DebugLimits(
                max_sessions=self.max_debug_sessions,
                idle_timeout_ms=self.debug_idle_timeout.milliseconds,
                max_frames=self.max_debug_frames,
                max_bindings_per_frame=self.max_debug_bindings,
                max_value_bytes=self.max_debug_value.bytes,
            )
        return mcp.ExecutionLimits(
            max_code_bytes=self.max_code.bytes,
            max_timeout_ms=self.max_timeout.milliseconds,
            max_output_bytes=self.max_output.bytes,
            max_concurrent=self.max_concurrent,
            debug=debug,
        )


RequiredLanguages = Annotated[
    list[LanguageSelector],
    Field(min_length=1, json_schema_extra={"uniqueItems": True}),
]


class ValidationConfig(ConfigModel):
    """Adapter validation reports required for session changes and integration."""

    require: Literal["available"] | RequiredLanguages = Field(
        default="available",
        description=(
            "`available` requires every affected adapter that advertises validation; an explicit "
            "list requires those exact core.Language pairs."
        ),
        json_schema_extra={"rift:selectsType": core.Language.__name__},
    )

    @model_validator(mode="after")
    def languages_are_unique(self) -> ValidationConfig:
        if isinstance(self.require, list):
            values = [language.root for language in self.require]
            if len(values) != len(set(values)):
                raise ValueError("validation.require languages must be unique")
        return self


class ValidatorsConfig(ConfigModel):
    """Repository-declared command checks requested before integration."""

    commands: list[mcp.CommandValidator] = Field(
        default_factory=list,
        description="Exact command declarations, in execution order.",
    )
    carry: list[core.ProjectPath] = Field(
        default_factory=list,
        description="Ignored project paths copied into each disposable validation workspace.",
        json_schema_extra={"uniqueItems": True},
    )

    @model_validator(mode="after")
    def entries_are_well_formed(self) -> ValidatorsConfig:
        ids = [command.id for command in self.commands]
        if len(ids) != len(set(ids)):
            raise ValueError("validators.commands ids must be unique")
        carried = [path.root for path in self.carry]
        if len(carried) != len(set(carried)):
            raise ValueError("validators.carry paths must be unique")
        return self


class AdapterProcessConfig(ConfigModel):
    """How Rift starts one configured adapter process."""

    command: list[str] = Field(
        min_length=1,
        description="Executable followed by literal arguments used to start the adapter process.",
    )
    env: dict[str, str] = Field(
        default_factory=dict,
        description="Environment additions supplied to the adapter process.",
        json_schema_extra={"propertyNames": {"pattern": ENVIRONMENT_NAME_PATTERN}},
    )

    @model_validator(mode="after")
    def process_is_well_formed(self) -> AdapterProcessConfig:
        if any(not argument for argument in self.command):
            raise ValueError("adapter command arguments must be non-empty")
        invalid = [
            name
            for name in self.env
            if re.fullmatch(ENVIRONMENT_NAME_PATTERN, name) is None
        ]
        if invalid:
            raise ValueError(f"invalid adapter environment names: {invalid!r}")
        return self


class RiftConfig(ConfigModel):
    """Workspace behavior loaded from the physical root's ``rift.toml``."""

    session: SessionConfig = Field(default_factory=SessionConfig)
    execution: ExecutionConfig = Field(default_factory=ExecutionConfig)
    validation: ValidationConfig = Field(default_factory=ValidationConfig)
    validators: ValidatorsConfig = Field(default_factory=ValidatorsConfig)
    adapters: dict[str, AdapterProcessConfig] = Field(
        default_factory=dict,
        description=(
            "Configured adapter processes keyed by repository-local process name. Each process "
            "answers Describe with its exact core.Language and adapter capabilities."
        ),
        json_schema_extra={
            "propertyNames": {"pattern": PROCESS_NAME_PATTERN},
            "rift:describesAs": adapter.Capabilities.__name__,
        },
    )

    @model_validator(mode="after")
    def process_names_are_valid(self) -> RiftConfig:
        invalid = [
            name
            for name in self.adapters
            if re.fullmatch(PROCESS_NAME_PATTERN, name) is None
        ]
        if invalid:
            raise ValueError(f"invalid adapter process names: {invalid!r}")
        return self


__all__ = [
    "AdapterProcessConfig",
    "ByteSize",
    "Duration",
    "ExecutionConfig",
    "LanguageSelector",
    "RiftConfig",
    "SessionBase",
    "SessionConfig",
    "ValidationConfig",
    "ValidatorsConfig",
]
