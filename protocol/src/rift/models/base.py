"""Metadata shared by the protocol's Pydantic models."""

from __future__ import annotations

import sys
from dataclasses import dataclass
from enum import Enum, IntEnum
from typing import Annotated, Any, Generic, Literal, TypeVar, cast

from pydantic import BaseModel, ConfigDict, Field, GetJsonSchemaHandler, RootModel
from pydantic.json_schema import JsonSchemaValue
from pydantic_core import CoreSchema

T = TypeVar("T")


class ProtocolRoot(RootModel[T], Generic[T]):
    model_config = ConfigDict(regex_engine="python-re")


class ClosedModel(BaseModel):
    model_config = ConfigDict(extra="forbid", regex_engine="python-re")


class ProtoModel(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)


def closed_config(schema_extra: dict[str, Any]) -> ConfigDict:
    return ConfigDict(
        extra="forbid",
        regex_engine="python-re",
        json_schema_extra=schema_extra,
    )


@dataclass(frozen=True)
class DefinitionMetadata:
    owner: str
    public: bool
    proto: dict[str, Any]
    schema_extra: dict[str, Any]


@dataclass(frozen=True)
class ProtoMetadata:
    values: dict[str, Any]


@dataclass(frozen=True)
class ProtoPackage:
    spec: dict[str, Any]
    models: tuple[type[ProtoModel], ...]
    enums: tuple[type[IntEnum], ...]
    services: list[dict[str, Any]]


DEFINITIONS: list[type[Any]] = []
PROTO_MESSAGES: list[type[ProtoModel]] = []
PROTO_ENUMS: list[type[IntEnum]] = []


def definition(
    *,
    owner: str,
    public: bool,
    proto: dict[str, Any],
    schema_extra: dict[str, Any],
):
    def decorate(model: type[T]) -> type[T]:
        metadata = DefinitionMetadata(owner, public, proto, schema_extra)
        dynamic = cast(Any, model)
        dynamic.__protocol__ = metadata
        extra = {
            **schema_extra,
            "rift:proto": proto,
            "rift:package": f"rift.{owner}",
        }

        def emit_schema(
            cls: type[Any],
            core_schema: CoreSchema,
            handler: GetJsonSchemaHandler,
        ) -> JsonSchemaValue:
            schema = handler(core_schema)
            schema.update(extra)
            return schema

        if issubclass(model, BaseModel):
            pydantic_model = cast(type[BaseModel], model)
            if issubclass(model, RootModel):
                dynamic.__get_pydantic_json_schema__ = classmethod(emit_schema)
            else:
                current = cast(
                    dict[str, Any] | None,
                    pydantic_model.model_config.get("json_schema_extra"),
                )
                pydantic_model.model_config["json_schema_extra"] = {
                    **(current or {}),
                    **extra,
                }
        elif issubclass(model, Enum):
            dynamic.__get_pydantic_json_schema__ = classmethod(emit_schema)
        DEFINITIONS.append(model)
        return model

    return decorate


def proto_message(values: dict[str, Any]):
    def decorate(model: type[T]) -> type[T]:
        typed = cast(type[ProtoModel], model)
        cast(Any, typed).__proto__ = ProtoMetadata(values)
        PROTO_MESSAGES.append(typed)
        return model

    return decorate


def proto_enum(values: dict[str, Any]):
    def decorate(model: type[T]) -> type[T]:
        typed = cast(type[IntEnum], model)
        cast(Any, typed).__proto__ = ProtoMetadata(values)
        PROTO_ENUMS.append(typed)
        return model

    return decorate


def proto_field(*, default: Any, spec: dict[str, Any]):
    return Field(
        default=default,
        description=spec.get("description"),
        json_schema_extra={"rift:proto": spec},
    )


def rebuild_models() -> None:
    for model in [*DEFINITIONS, *PROTO_MESSAGES]:
        rebuild = getattr(model, "model_rebuild", None)
        if rebuild is None:
            continue
        namespace = vars(sys.modules[model.__module__])
        rebuild(force=True, _types_namespace=namespace)


__all__ = [
    "DEFINITIONS",
    "PROTO_ENUMS",
    "PROTO_MESSAGES",
    "Annotated",
    "Any",
    "ClosedModel",
    "ConfigDict",
    "Enum",
    "Field",
    "IntEnum",
    "Literal",
    "ProtoModel",
    "ProtoPackage",
    "ProtocolRoot",
    "closed_config",
    "definition",
    "proto_enum",
    "proto_field",
    "proto_message",
    "rebuild_models",
]
