"""Validate command options and corpus configuration before starting processes."""

from pathlib import Path
from typing import Annotated, Literal, Self

from pydantic import BaseModel, ConfigDict, Field, model_validator

CorpusName = Literal["bun", "nextjs", "fastapi"]
CorpusCase = Literal["workspace", "stop", "churn"]


class BinaryOptions(BaseModel):
    """A supplied binary or a Cargo target, with optional result output."""

    model_config = ConfigDict(extra="forbid", frozen=True)
    binary: Path | None = None
    target: str | None = None
    version: str | None = None
    junit: Path | None = None

    @model_validator(mode="after")
    def select_binary(self) -> Self:
        if self.binary is not None and self.target is not None:
            raise ValueError("--target selects a build and cannot accompany --binary")
        return self


class CorpusOptions(BaseModel):
    """A corpus command and its required inputs."""

    model_config = ConfigDict(extra="forbid", frozen=True)
    command: Literal["sync", "measure", "test"]
    name: CorpusName | None = None
    binary: Path | None = None
    report: Path | None = None
    case: CorpusCase = "workspace"

    @model_validator(mode="after")
    def test_inputs(self) -> Self:
        if self.command == "test":
            if self.name is None or self.binary is None:
                raise ValueError("test requires one corpus name and --binary")
            if self.case == "stop" and self.name != "bun":
                raise ValueError("only bun has a stop case")
            if self.case == "churn" and self.name != "nextjs":
                raise ValueError("only nextjs has a churn case")
        return self


Nonnegative = Annotated[int, Field(strict=True, ge=0)]


class CorpusPin(BaseModel):
    """The recorded bounds and measurements of one immutable Git tree."""

    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)
    repository: Annotated[str, Field(pattern=r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")]
    tag: str
    commit: Annotated[str, Field(pattern=r"^[0-9a-f]{40}$")]
    files: Nonnegative
    bytes: Nonnegative
    symlinks: Nonnegative
    depth: Nonnegative
    package_json: Nonnegative
    oversized_path: str
    oversized_bytes: Nonnegative
    seconds: Annotated[int, Field(gt=0)]


class CorpusPins(BaseModel):
    """The three repositories required by the integration suite."""

    model_config = ConfigDict(extra="forbid", frozen=True)
    bun: CorpusPin
    nextjs: CorpusPin
    fastapi: CorpusPin
