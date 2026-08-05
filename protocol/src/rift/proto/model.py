"""Compiler records consumed by the Protobuf AST adapter."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class EnumValue:
    name: str
    number: int
    description: str | None = None
    deprecated: bool = False


@dataclass
class Enum:
    name: str
    description: str | None = None
    values: list[EnumValue] = field(default_factory=list)
    allow_alias: bool = False
    reserved_numbers: list[int] = field(default_factory=list)
    reserved_names: list[str] = field(default_factory=list)


@dataclass
class Field:
    name: str
    number: int
    type: str
    description: str | None = None
    repeated: bool = False
    optional: bool = False
    oneof: str | None = None
    map_key: str | None = None
    map_value: str | None = None
    deprecated: bool = False


@dataclass
class Message:
    name: str
    description: str | None = None
    fields: list[Field] = field(default_factory=list)
    messages: list[Message] = field(default_factory=list)
    enums: list[Enum] = field(default_factory=list)
    oneofs: list[str] = field(default_factory=list)
    reserved_numbers: list[int] = field(default_factory=list)
    reserved_ranges: list[tuple[int, int]] = field(default_factory=list)
    reserved_names: list[str] = field(default_factory=list)
    section: str | None = None


@dataclass
class Rpc:
    name: str
    request: str
    response: str
    description: str | None = None
    request_stream: bool = False
    response_stream: bool = False


@dataclass
class Service:
    name: str
    description: str | None
    rpcs: list[Rpc]


@dataclass
class File:
    path: str
    package: str
    description: str | None = None
    imports: list[str] = field(default_factory=list)
    options: dict[str, str | bool] = field(default_factory=dict)
    messages: list[Message] = field(default_factory=list)
    enums: list[Enum] = field(default_factory=list)
    services: list[Service] = field(default_factory=list)
    section_option: bool = False
