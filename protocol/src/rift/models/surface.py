"""Typed authoring graph for protocol entry points and documentation."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

Definition = type[Any]


@dataclass(frozen=True)
class Rpc:
    name: str
    request: Definition
    response: Definition
    description: str | None = None
    request_stream: bool = False
    response_stream: bool = False


@dataclass(frozen=True)
class Service:
    name: str
    description: str
    rpcs: tuple[Rpc, ...]


@dataclass(frozen=True)
class Tool:
    name: str
    rpc: Rpc


@dataclass(frozen=True)
class Resource:
    name: str
    description: str
    template: Definition
    uri: Definition
    link: Definition


@dataclass(frozen=True)
class Axis:
    name: str
    summary: str
    identified_by: tuple[Definition, ...] = ()
    holds: tuple[Definition, ...] = ()
    residual_of: str | None = None


@dataclass(frozen=True)
class Document:
    schema: str
    id: str
    title: str
    description: str
    entry_points_description: str
    service: Service
    tools: tuple[Tool, ...]
    resources: tuple[Resource, ...]
    resource_read: Rpc
    error: Definition
    axes_description: str
    axes: tuple[Axis, ...]


__all__ = [
    "Axis",
    "Document",
    "Resource",
    "Rpc",
    "Service",
    "Tool",
]
