"""Serialize the Pydantic protocol model into JSON Schema and Protobuf."""

from __future__ import annotations

import argparse
import copy
import importlib.resources
import json
import re
import sys
import tempfile
import types
from dataclasses import dataclass
from enum import Enum as PythonEnum
from pathlib import Path
from typing import Any, Union, cast, get_args, get_origin

import tomli
from google.protobuf import descriptor_pb2
from grpc_tools import protoc
from jsonschema.validators import Draft202012Validator
from pydantic import TypeAdapter

from .models import adapter, config, scip, scip_api
from .models.base import (
    DEFINITIONS,
    PROTO_ENUMS,
    PROTO_MESSAGES,
    DefinitionMetadata,
    Proto,
    ProtoFieldDescriptor,
    ProtoPackage,
    proto_type_name,
    rebuild_models,
)
from .models.base import (
    Field as ModelField,
)
from .models.document import DOCUMENT
from .models.surface import Document
from .proto import serde
from .proto.model import Enum as EnumSpec
from .proto.model import EnumValue as EnumValueSpec
from .proto.model import Field as FieldSpec
from .proto.model import File as FileSpec
from .proto.model import Message as MessageSpec
from .proto.model import Rpc as RpcSpec
from .proto.model import Service as ServiceSpec

PROTOCOL = Path(__file__).resolve().parents[2]
SCALAR = {"string": "string", "integer": "int64", "number": "double", "boolean": "bool"}


@dataclass
class Definition:
    owner: str
    public: bool
    model: type[Any]
    mapping: Proto
    body: dict[str, Any]


def pydantic_schema() -> dict[str, Any]:
    rebuild_models()
    aggregate = Union[tuple(DEFINITIONS)]  # noqa: UP007 - the union members are dynamic
    return TypeAdapter(aggregate).json_schema(ref_template="#/$defs/{model}")


PYDANTIC_SCHEMA = pydantic_schema()


def definitions() -> dict[str, Definition]:
    generated_definitions = PYDANTIC_SCHEMA["$defs"]
    result: dict[str, Definition] = {}
    for model in DEFINITIONS:
        metadata: DefinitionMetadata = model.__protocol__
        body = generated_definitions[model.__name__]
        if model.__name__ in result:
            raise ValueError(f"duplicate JSON definition {model.__name__}")
        result[model.__name__] = Definition(
            metadata.namespace.owner,
            metadata.public,
            model,
            metadata.mapping,
            body,
        )
    return result


DEFINITION_MAP = definitions()


def snake(name: str) -> str:
    out: list[str] = []
    for index, char in enumerate(name):
        if index and char.isupper() and name[index - 1].islower():
            out.append("_")
        out.append(char.lower())
    return "".join(out)


def scream(name: str) -> str:
    return snake(name).upper()


def ref_name(schema: dict[str, Any]) -> str | None:
    ref = schema.get("$ref")
    if not isinstance(ref, str):
        return None
    prefix = "#/$defs/"
    if not ref.startswith(prefix):
        raise ValueError(f"unsupported JSON reference {ref}")
    return ref[len(prefix) :]


def json_named_fields(model: type[Any]) -> dict[str, ModelField[Any]]:
    """
    A model's declared fields, keyed the way JSON Schema names them.

    `__protocol_fields__` is keyed by the Python attribute, and a field whose
    name collides with a Python keyword is authored as `from_` with an alias.
    Pydantic emits the alias as the property name, so looking a property up by
    attribute misses exactly those fields and drops them out of the Protobuf —
    which is how `Relationship` lost the end the edge starts at.
    """
    declared: dict[str, ModelField[Any]] = getattr(model, "__protocol_fields__", {})
    pydantic_fields = getattr(model, "model_fields", {})
    named: dict[str, ModelField[Any]] = {}
    for attribute, field in declared.items():
        info = pydantic_fields.get(attribute)
        named[info.alias if info is not None and info.alias else attribute] = field
    return named


def real_schema(schema: dict[str, Any]) -> dict[str, Any]:
    for keyword in ["oneOf", "anyOf"]:
        branches = schema.get(keyword)
        if not isinstance(branches, list):
            continue
        real = [branch for branch in branches if branch.get("type") != "null"]
        if len(real) == 1:
            return {**real[0], "rift:proto": schema.get("rift:proto", {})}
    return schema


class SchemaCompiler:
    def __init__(self, owner: str):
        self.owner = owner
        self.package = f"rift.{owner}"
        self.messages: list[MessageSpec] = []
        self.enums: list[EnumSpec] = []
        self.message_names: set[str] = set()
        self.enum_names: set[str] = set()

    def qualify(self, name: str) -> str:
        target = DEFINITION_MAP[name]
        return name if target.owner == self.owner else f"rift.{target.owner}.{name}"

    def scalar_alias(self, name: str) -> str | None:
        mapping = DEFINITION_MAP[name].mapping
        if mapping.scalar_type is None:
            return None
        return proto_type_name(mapping.scalar_type)

    def enum(
        self,
        name: str,
        schema: dict[str, Any],
        mapping: Proto,
        *,
        prefixed: bool,
    ) -> EnumSpec:
        values_meta = {value.json: value for value in mapping.enum_values}
        descriptions = schema.get("rift:enumDescriptions", {})
        values = [
            EnumValueSpec(
                name=(f"{scream(name)}_" if prefixed else "") + "UNSPECIFIED",
                number=0,
                description="The caller did not set a value.",
            )
        ]
        for value in schema["enum"]:
            if str(value) not in values_meta:
                raise ValueError(
                    f"{name} maps {list(values_meta)} but its Pydantic enum contains {schema['enum']}"
                )
            wire = values_meta[str(value)]
            values.append(
                EnumValueSpec(
                    name=wire.proto,
                    number=wire.number,
                    description=descriptions.get(str(value)),
                )
            )
        return EnumSpec(name=name, description=schema.get("description"), values=values)

    def type_of(
        self,
        schema: dict[str, Any],
        nested_enums: list[tuple[str, dict[str, Any], Proto]],
    ) -> tuple[str, bool, str | None, str | None]:
        schema = real_schema(schema)
        ref = ref_name(schema)
        target = DEFINITION_MAP[ref] if ref else None
        if target and target.mapping.kind == "enum":
            enum_name = target.mapping.enum_name
            if enum_name is None:
                raise TypeError(f"{ref} has no Protobuf enum name")
            nested_enums.append((enum_name, copy.deepcopy(target.body), target.mapping))
            return enum_name, False, None, None
        if ref:
            return self.scalar_alias(ref) or self.qualify(ref), False, None, None
        if schema.get("contentEncoding") == "base64":
            return "bytes", False, None, None
        if "enum" in schema:
            raise TypeError("an inline enum has no typed protocol definition")
        if "const" in schema:
            return (
                ("string" if isinstance(schema["const"], str) else "int64"),
                False,
                None,
                None,
            )
        if schema.get("type") == "array":
            item, _, map_key, map_value = self.type_of(
                schema.get("items", {}), nested_enums
            )
            return item, True, map_key, map_value
        if isinstance(schema.get("additionalProperties"), dict) and not schema.get(
            "properties"
        ):
            value, _, _, _ = self.type_of(schema["additionalProperties"], nested_enums)
            return "map", False, "string", value
        if schema.get("type") == "object" and not schema.get("properties"):
            return "google.protobuf.Struct", False, None, None
        return (
            SCALAR.get(schema.get("type"), "google.protobuf.Value"),
            False,
            None,
            None,
        )

    def compile_message(self, definition: Definition) -> MessageSpec:
        name = definition.model.__name__
        schema = definition.body
        if name in self.message_names:
            return next(message for message in self.messages if message.name == name)
        self.message_names.add(name)
        required = set(schema.get("required", []))
        nested_raw: list[tuple[str, dict[str, Any], Proto]] = []
        fields: list[FieldSpec] = []
        declared_fields = json_named_fields(definition.model)
        for json_name, field_schema in schema.get("properties", {}).items():
            declared = declared_fields.get(json_name)
            if declared is None or declared.number is None:
                continue
            kind, repeated, map_key, map_value = (
                (
                    proto_type_name(declared.proto_type),
                    False,
                    None,
                    None,
                )
                if declared.proto_type is not None
                else self.type_of(field_schema, nested_raw)
            )
            nullable = any(
                branch.get("type") == "null"
                for keyword in ["oneOf", "anyOf"]
                for branch in field_schema.get(keyword, [])
            )
            fields.append(
                FieldSpec(
                    name=declared.proto_name or snake(json_name).rstrip("_"),
                    number=declared.number,
                    type=kind,
                    description=field_schema.get("description"),
                    repeated=repeated,
                    optional=json_name not in required or nullable,
                    map_key=map_key,
                    map_value=map_value,
                )
            )
        if (
            not fields
            and isinstance(schema.get("additionalProperties"), dict)
            and definition.mapping.placement is not None
        ):
            value, _, _, _ = self.type_of(schema["additionalProperties"], nested_raw)
            fields.append(
                FieldSpec(
                    name=definition.mapping.placement.name,
                    number=definition.mapping.placement.number,
                    type="map",
                    map_key="string",
                    map_value=value,
                )
            )
        prefixed = len(nested_raw) > 1
        nested = [
            self.enum(enum_name, enum_schema, mapping, prefixed=prefixed)
            for enum_name, enum_schema, mapping in nested_raw
        ]
        message = MessageSpec(
            name=name,
            description=schema.get("description"),
            fields=fields,
            enums=nested,
        )
        self.messages.append(message)
        return message

    def compile_union(self, definition: Definition) -> MessageSpec:
        name = definition.model.__name__
        schema = definition.body
        if name in self.message_names:
            return next(message for message in self.messages if message.name == name)
        self.message_names.add(name)
        mapping = definition.mapping
        if mapping.oneof is None:
            raise TypeError(f"{name} has no oneof")
        oneof = mapping.oneof.name
        fields = [
            FieldSpec(
                name=variant.name,
                number=variant.number,
                type=(
                    cast(Any, variant.proto_model).DESCRIPTOR.full_name
                    if variant.proto_model
                    else variant.type_name
                ),
                oneof=oneof,
            )
            for variant in mapping.variants
        ]
        message = MessageSpec(
            name=name,
            description=schema.get("description"),
            fields=fields,
            oneofs=[oneof],
        )
        self.messages.append(message)
        return message

    def compile(self) -> FileSpec:
        for name, definition in DEFINITION_MAP.items():
            if definition.owner != self.owner:
                continue
            schema = definition.body
            mapping = definition.mapping
            if "enum" in schema:
                if mapping.kind != "enum" or not mapping.named:
                    continue
                if name not in self.enum_names:
                    self.enum_names.add(name)
                    self.enums.append(self.enum(name, schema, mapping, prefixed=True))
            elif ("oneOf" in schema or "anyOf" in schema) and mapping.kind == "union":
                if mapping.named:
                    self.compile_union(definition)
            elif schema.get("properties") is not None or isinstance(
                schema.get("additionalProperties"), dict
            ):
                if mapping.named:
                    self.compile_message(definition)
        imports = ["google/protobuf/struct.proto"]
        if any(
            isinstance(field.type, str) and "google.protobuf.Empty" in field.type
            for message in self.messages
            for field in message.fields
        ):
            imports.insert(0, "google/protobuf/empty.proto")
        if self.owner == "mcp":
            imports.insert(0, "rift/core.proto")
        services: list[ServiceSpec] = []
        if self.owner == "mcp":
            rpcs: list[RpcSpec] = []
            for rpc in DOCUMENT.service.rpcs:
                request = rpc.request.__protocol__.proto["type"]
                response = rpc.response.__protocol__.proto["type"]
                rpcs.append(
                    RpcSpec(
                        name=rpc.name,
                        request=request,
                        response=response,
                        description=rpc.description,
                        request_stream=rpc.request_stream,
                        response_stream=rpc.response_stream,
                    )
                )
            services.append(
                ServiceSpec(
                    name=DOCUMENT.service.name,
                    description=DOCUMENT.service.description,
                    rpcs=rpcs,
                )
            )
        return FileSpec(
            path=f"rift/{self.owner}.proto",
            package=self.package,
            description=(
                "Shared messages used by the MCP server and language adapters."
                if self.owner == "core"
                else "Internal messages produced when rift-mcp decodes MCP JSON values."
            ),
            imports=imports,
            messages=self.messages,
            enums=self.enums,
            services=services,
        )


def enum_from_model(model: type[PythonEnum]) -> EnumSpec:
    meta = cast(Any, model).__proto__.values
    return EnumSpec(
        name=meta["name"],
        description=meta.get("description"),
        values=[EnumValueSpec(**value) for value in meta.get("values", [])],
        allow_alias=meta.get("allow_alias", False),
        reserved_numbers=meta.get("reserved_numbers", []),
        reserved_names=meta.get("reserved_names", []),
    )


def _direct_value_type(annotation: Any) -> tuple[Any, bool, bool]:
    origin = get_origin(annotation)
    if origin is list:
        return get_args(annotation)[0], True, False
    if origin in {Union, types.UnionType}:
        members = get_args(annotation)
        values = [member for member in members if member is not type(None)]
        if len(values) == 1 and len(values) != len(members):
            return values[0], False, True
    return annotation, False, False


def _direct_proto_type(
    annotation: Any,
    declared: ModelField[Any],
    package: str,
) -> str | int:
    if declared.proto_type is not None:
        return declared.proto_type
    primitive = {
        bool: ProtoFieldDescriptor.TYPE_BOOL,
        bytes: ProtoFieldDescriptor.TYPE_BYTES,
        float: ProtoFieldDescriptor.TYPE_DOUBLE,
        int: ProtoFieldDescriptor.TYPE_INT64,
        str: ProtoFieldDescriptor.TYPE_STRING,
    }
    if annotation in primitive:
        return primitive[annotation]
    direct = getattr(annotation, "__proto__", None)
    if direct is not None:
        metadata = direct.values
        qualified = (
            f"{metadata['parent']}.{metadata['name']}"
            if metadata.get("parent")
            else f"{metadata['package']}.{metadata['name']}"
        )
        return (
            f".{qualified}"
            if metadata["package"] == "scip" and package != "scip"
            else qualified
        )
    definition = getattr(annotation, "__protocol__", None)
    if definition is not None:
        mapping = definition.proto
        if "scalar" in mapping:
            return mapping["scalar"]
        if "type" in mapping:
            return mapping["type"]
    raise TypeError(f"cannot derive a Protobuf type from {annotation!r}")


def message_from_model(model: type[Any]) -> MessageSpec:
    meta = model.__proto__.values
    wire_name = (
        f"{meta['package']}.{meta['name']}"
        if not meta.get("parent")
        else f"{meta['parent']}.{meta['name']}"
    )
    fields = []
    oneofs: list[str] = []
    declared_fields: dict[str, ModelField[Any]] = model.__protocol_fields__
    for name, declared in declared_fields.items():
        if declared.number is None:
            continue
        pydantic_field = model.model_fields[name]
        value_type, repeated, nullable = _direct_value_type(pydantic_field.annotation)
        wire_type = _direct_proto_type(value_type, declared, meta["package"])
        oneof = declared.oneof.name if declared.oneof else None
        if oneof and oneof not in oneofs:
            oneofs.append(oneof)
        fields.append(
            FieldSpec(
                name=declared.proto_name or name.rstrip("_"),
                number=declared.number,
                type=wire_type,
                description=pydantic_field.description,
                repeated=repeated,
                optional=nullable and isinstance(wire_type, int),
                oneof=oneof,
                deprecated=declared.deprecated,
            )
        )
    nested_messages = [
        message_from_model(child)
        for child in PROTO_MESSAGES
        if cast(Any, child).__proto__.values.get("parent") == wire_name
    ]
    nested_enums = [
        enum_from_model(child)
        for child in PROTO_ENUMS
        if cast(Any, child).__proto__.values.get("parent") == wire_name
    ]
    return MessageSpec(
        name=meta["name"],
        description=meta.get("description"),
        fields=fields,
        messages=nested_messages,
        enums=nested_enums,
        oneofs=oneofs,
        reserved_numbers=meta.get("reserved_numbers", []),
        reserved_ranges=meta.get("reserved_ranges", []),
        reserved_names=meta.get("reserved_names", []),
        section=meta.get("section"),
    )


def _direct_rpc_type(model: type[Any]) -> str:
    descriptor = getattr(model, "DESCRIPTOR", None)
    if descriptor is not None:
        return descriptor.full_name
    metadata = cast(Any, model).__proto__.values
    return (
        f"{metadata['parent']}.{metadata['name']}"
        if metadata.get("parent")
        else f"{metadata['package']}.{metadata['name']}"
    )


def file_from_package(package: ProtoPackage) -> FileSpec:
    spec = package.file
    description = spec.description
    if not description and spec.namespace.package == "rift.adapter":
        description = (
            "The Rift server owns at most one configured adapter process per exact Language and calls "
            "it over gRPC on a Unix domain socket. The process can hold states for several snapshots "
            "and back them with separate runtime workers. Rift resolves revisions and supplies "
            "server-owned projections; adapters never allocate or switch them. Different-language "
            "adapters may open the same tree. Rift serializes source writes, then refreshes every "
            "adapter that holds the projection."
        )
    if not description and spec.namespace.package == "scip":
        release = spec.upstream_release
        description = (
            "SCIP is a language-neutral Protobuf format for code indexes. Rift protocol version 1 "
            f"pins the schema from SCIP {release}."
        )
    return FileSpec(
        path=spec.path,
        package=spec.namespace.package,
        description=description,
        imports=list(spec.imports),
        options={option.name: option.value for option in spec.options},
        messages=[message_from_model(model) for model in package.models],
        enums=[enum_from_model(model) for model in package.enums],
        services=[
            ServiceSpec(
                name=service.name,
                description=service.description,
                rpcs=[
                    RpcSpec(
                        name=rpc.name,
                        request=_direct_rpc_type(rpc.request),
                        response=_direct_rpc_type(rpc.response),
                        description=rpc.description,
                        request_stream=rpc.request_stream,
                        response_stream=rpc.response_stream,
                    )
                    for rpc in service.rpcs
                ],
            )
            for service in package.services
        ],
        section_option=spec.section_option,
    )


def union_parents(definitions: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    """Every tagged union in the schema, mapped to the branch list it holds."""
    parents: dict[str, list[dict[str, Any]]] = {}
    for name, body in definitions.items():
        if "oneof" not in body.get("rift:proto", {}):
            continue
        for keyword in ["oneOf", "anyOf"]:
            branches = body.get(keyword)
            if isinstance(branches, list):
                parents[name] = branches
                break
    return parents


def named_in_axes(document: dict[str, Any]) -> set[str]:
    """
    Definitions the axis declaration names. `rift:axes` carries them as bare
    strings rather than references, so nothing else in this module would see
    them, and a group that identifies itself by a type Rift had folded away
    would identify itself by nothing.
    """
    groups = document.get("rift:axes", {}).get("groups", [])
    return {
        name
        for group in groups
        for key in ["identifiedBy", "holds"]
        for name in group.get(key, [])
    }


def external_uses(document: dict[str, Any], parents: dict[str, Any]) -> set[str]:
    """
    Definitions reached from somewhere other than the union branch that declares
    them. A union's own branch list and its `discriminator` mapping both point at
    every variant by construction, so counting either would make every variant
    look shared and inline nothing.
    """
    used: set[str] = set()

    def walk(node: Any, owner: str | None) -> None:
        if isinstance(node, list):
            for value in node:
                walk(value, owner)
            return
        if not isinstance(node, dict):
            return
        target = node.get("$ref")
        if isinstance(target, str) and target.startswith("#/$defs/"):
            used.add(target[len("#/$defs/") :])
        for key, value in node.items():
            if key == "discriminator":
                continue
            if (
                owner in parents
                and key in {"oneOf", "anyOf"}
                and isinstance(value, list)
            ):
                for branch in value:
                    # A bare `$ref` here is the variant's declaration; anything
                    # else in the branch is an ordinary reference.
                    if isinstance(branch, dict) and set(branch) == {"$ref"}:
                        continue
                    walk(branch, owner)
                continue
            walk(value, owner)

    for name, body in document["$defs"].items():
        walk(body, name)
    for key, value in document.items():
        if key != "$defs":
            walk(value, None)
    return used


def inline_variants(document: dict[str, Any]) -> None:
    """
    Fold each single-use union variant into the union that declares it.

    Pydantic emits one model per branch of a discriminated union, and every one
    of those lands in `$defs` under a name it invented — `FileContentRegular`,
    `EditSetGitlink`. Nothing outside the union refers to most of them, so the
    names are surface a client has to learn for no reading of the protocol they
    make possible: the branch is already identified by its own tag. Folding them
    in leaves the union as the one named type, with a tag and the fields that
    tag carries.

    A variant something else refers to keeps its name, because inlining it would
    delete a reference target. `FileChange` holds `TextEdit` values, so `TextEdit`
    stays a definition even though it is also a branch of `Edit`. A variant two
    unions both declare also stays named: folding it into the first would leave
    the second pointing at a definition that no longer exists.
    """
    definitions = document["$defs"]
    parents = union_parents(definitions)

    declared: dict[str, int] = {}
    for branches in parents.values():
        for branch in branches:
            if isinstance(branch, dict) and set(branch) == {"$ref"}:
                target = ref_name(branch)
                if target is not None:
                    declared[target] = declared.get(target, 0) + 1

    shared = external_uses(document, parents) | named_in_axes(document)
    shared |= {name for name, count in declared.items() if count > 1}

    for name, branches in parents.items():
        inlined = False
        for index, branch in enumerate(branches):
            if not isinstance(branch, dict) or set(branch) != {"$ref"}:
                continue
            target = ref_name(branch)
            if target is None or target in shared or target not in definitions:
                continue
            branches[index] = definitions.pop(target)
            inlined = True
        # The mapping names every branch by `$ref`, so it cannot survive one of
        # them losing its address. Each branch keeps the `const` tag the mapping
        # was built from, which is what a validator reads anyway.
        if inlined:
            definitions[name].pop("discriminator", None)


def _schema_ref(model: type[Any]) -> dict[str, str]:
    return {"$ref": f"#/$defs/{model.__name__}"}


def document_metadata(document: Document) -> dict[str, Any]:
    service_name = f"rift.mcp.{document.service.name}"
    groups = {
        group.name: {"title": group.title, "summary": group.summary}
        for group in document.tool_groups
    }
    declared = {tool.group for tool in document.tools} - set(groups)
    if declared:
        raise ValueError(f"tools name undeclared groups: {sorted(declared)}")
    tools = {
        tool.name: {
            "rpc": f"{service_name}/{tool.rpc.name}",
            "group": tool.group,
            "description": tool.rpc.description,
            "params": _schema_ref(tool.rpc.request),
            "result": _schema_ref(tool.rpc.response),
        }
        for tool in document.tools
    }
    resources = {
        resource.name: {
            "description": resource.description,
            "template": _schema_ref(resource.template),
            "uri": _schema_ref(resource.uri),
            "link": _schema_ref(resource.link),
        }
        for resource in document.resources
    }
    connect = next(rpc for rpc in document.service.rpcs if rpc.name == "Connect")
    resource_read = document.resource_read
    entry_points = {
        "description": document.entry_points_description,
        "grpc.connection": {
            "rpc": f"{service_name}/{connect.name}",
            "description": connect.description,
            "params": _schema_ref(connect.request),
            "event": _schema_ref(connect.response),
        },
        "mcp.toolGroups": groups,
        "mcp.tools": tools,
        "mcp.resources": resources,
        "mcp.error": _schema_ref(document.error),
        "mcp.resources.read": {
            "rpc": f"{service_name}/{resource_read.name}",
            "params": _schema_ref(resource_read.request),
            "result": _schema_ref(resource_read.response),
        },
    }
    groups = []
    for axis in document.axes:
        group: dict[str, Any] = {"name": axis.name, "summary": axis.summary}
        if axis.identified_by:
            group["identifiedBy"] = [model.__name__ for model in axis.identified_by]
        if axis.holds:
            group["holds"] = [model.__name__ for model in axis.holds]
        if axis.residual_of:
            group["residualOf"] = axis.residual_of
        groups.append(group)
    return {
        "$schema": document.schema,
        "$id": document.id,
        "title": document.title,
        "description": document.description,
        "rift:entryPoints": entry_points,
        "rift:axes": {"description": document.axes_description, "groups": groups},
    }


def schema_output() -> dict[str, Any]:
    result = document_metadata(DOCUMENT)
    result["$defs"] = copy.deepcopy(PYDANTIC_SCHEMA["$defs"])
    inline_variants(result)
    return result


def config_schema_output() -> dict[str, Any]:
    """JSON Schema for the TOML-compatible value authored in ``rift.toml``."""

    body = config.RiftConfig.model_json_schema(
        by_alias=True,
        ref_template="#/$defs/{model}",
    )
    body.update(
        {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://volar.sh/rift/protocol/rift.schema.json",
            "title": "Rift repository configuration",
            "description": (
                "The typed rift.toml file. Configuration reuses protocol enums and paths, and "
                "carries machine-checked links to the fields it bounds."
            ),
        }
    )
    return body


def validate_json_schema(content: str) -> dict[str, Any]:
    schema = json.loads(content)
    Draft202012Validator.check_schema(schema)
    definitions = schema.get("$defs")
    if not isinstance(definitions, dict):
        raise TypeError("mcp.json has no $defs object")
    definitions = cast(dict[str, Any], definitions)

    numbered = numbered_collisions(list(definitions))
    if numbered:
        raise ValueError(
            f"mcp.json definitions have generated numeric suffixes: {numbered!r}"
        )

    for name, definition in definitions.items():
        if not isinstance(definition, dict):
            raise TypeError(f"$defs/{name} is not an object")
        if name.startswith("Scip"):
            raise ValueError(f"$defs/{name} exposes the separate SCIP API through MCP")
        package = definition.get("rift:package")
        if package not in {"rift.core", "rift.mcp"}:
            raise ValueError(f"$defs/{name} has invalid rift:package {package!r}")
        proto = definition.get("rift:proto")
        if not isinstance(proto, dict):
            raise TypeError(f"$defs/{name} has no Protobuf mapping")
        mapped = proto.get("type")
        if isinstance(mapped, str) and mapped.startswith(("rift.scip.", "scip.")):
            raise ValueError(f"$defs/{name} exposes the separate SCIP API through MCP")
    return schema


def validate_config_schema(content: str) -> dict[str, Any]:
    schema = json.loads(content)
    Draft202012Validator.check_schema(schema)
    definitions = schema.get("$defs")
    if not isinstance(definitions, dict):
        raise TypeError("rift.schema.json has no $defs object")

    linked_fields = {"rift:bounds", "rift:prefixOf"}
    linked_types = {"rift:selectsType", "rift:describesAs"}
    protocol_models = {
        model.__name__: model for model in [*DEFINITIONS, *PROTO_MESSAGES, *PROTO_ENUMS]
    }

    def walk(node: Any, path: str) -> None:
        if isinstance(node, list):
            for index, value in enumerate(node):
                walk(value, f"{path}[{index}]")
            return
        if not isinstance(node, dict):
            return
        for keyword in linked_fields:
            target = node.get(keyword)
            if target is None:
                continue
            if not isinstance(target, dict):
                raise TypeError(f"{path}.{keyword} is not a field target")
            model_name = target.get("model")
            field_name = target.get("field")
            model = protocol_models.get(model_name)
            declared = getattr(model, "__protocol_fields__", {}) if model else {}
            if model is None or field_name not in declared:
                raise ValueError(f"{path}.{keyword} targets missing field {target!r}")
        for keyword in linked_types:
            target = node.get(keyword)
            if target is not None and target not in protocol_models:
                raise ValueError(f"{path}.{keyword} targets missing type {target!r}")
        for key, value in node.items():
            if key not in {"examples", "default"}:
                walk(value, f"{path}.{key}")

    walk(schema, "rift.schema.json")
    return schema


def validate_rift_toml(path: Path) -> config.RiftConfig:
    """Parse the repository's real config through the same model as its schema."""

    try:
        value = tomli.loads(path.read_text())
    except tomli.TOMLDecodeError as error:
        raise ValueError(f"invalid {path.name}: {error}") from error
    return config.RiftConfig.model_validate(value)


def descriptor_names(
    package: str,
    parent: str,
    messages: list[Any],
) -> set[str]:
    names: set[str] = set()
    for message in messages:
        name = ".".join(part for part in [package, parent, message.name] if part)
        names.add(name)
        names.update(
            descriptor_names(
                package,
                ".".join(part for part in [parent, message.name] if part),
                message.nested_type,
            )
        )
        names.update(f"{name}.{enum.name}" for enum in message.enum_type)
    return names


def descriptor_definition_names(messages: list[Any], enums: list[Any]) -> list[str]:
    names = [value.name for value in [*messages, *enums]]
    for message in messages:
        names.extend(
            descriptor_definition_names(message.nested_type, message.enum_type)
        )
    return names


def numbered_collisions(names: list[str]) -> list[str]:
    declared = set(names)
    collisions: list[str] = []
    for name in names:
        match = re.fullmatch(r"(.+?)([0-9]+)", name)
        if match and match.group(1) in declared:
            collisions.append(name)
    return collisions


def validate_proto(files: dict[str, str], schema: dict[str, Any]) -> None:
    with tempfile.TemporaryDirectory(prefix="rift-protocol-") as directory:
        root = Path(directory)
        for path, content in files.items():
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)

        descriptor_path = root / "protocol.pb"
        bundled = importlib.resources.files("grpc_tools") / "_proto"
        arguments = [
            "grpc_tools.protoc",
            f"-I{root}",
            f"-I{bundled}",
            f"--descriptor_set_out={descriptor_path}",
            "--include_imports",
            *files,
        ]
        if protoc.main(arguments) != 0:
            raise ValueError("generated Protobuf does not compile")

        descriptors = descriptor_pb2.FileDescriptorSet()  # ty: ignore[unresolved-attribute]
        descriptors.ParseFromString(descriptor_path.read_bytes())

    generated = {
        descriptor.name: descriptor.package
        for descriptor in descriptors.file
        if descriptor.name in files
    }
    expected = {
        "rift/core.proto": "rift.core",
        "rift/mcp.proto": "rift.mcp",
        "rift/adapter.proto": "rift.adapter",
        "rift/scip.proto": "rift.scip",
        "scip/scip.proto": "scip",
    }
    if generated != expected:
        raise ValueError(f"generated Protobuf packages differ: {generated!r}")

    for descriptor in descriptors.file:
        if descriptor.name not in files:
            continue
        numbered = numbered_collisions(
            descriptor_definition_names(descriptor.message_type, descriptor.enum_type)
        )
        if numbered:
            raise ValueError(
                f"{descriptor.name} definitions have generated numeric suffixes: {numbered!r}"
            )
        if descriptor.name == "rift/mcp.proto":
            leaked = [
                value.name
                for value in [*descriptor.message_type, *descriptor.enum_type]
                if value.name.startswith("Scip")
            ]
            if leaked:
                raise ValueError(f"rift/mcp.proto exposes SCIP types: {leaked!r}")
            if any(
                dependency in {"rift/scip.proto", "scip/scip.proto"}
                for dependency in descriptor.dependency
            ):
                raise ValueError("rift/mcp.proto imports the separate SCIP API")
        if descriptor.name == "rift/scip.proto":
            prefixed = [
                value.name
                for value in [*descriptor.message_type, *descriptor.enum_type]
                if value.name.startswith("Scip")
            ]
            if prefixed:
                raise ValueError(
                    f"rift/scip.proto repeats its namespace in type names: {prefixed!r}"
                )

    names: set[str] = set()
    for descriptor in descriptors.file:
        names.update(descriptor_names(descriptor.package, "", descriptor.message_type))
        names.update(
            f"{descriptor.package}.{enum.name}" for enum in descriptor.enum_type
        )

    # Every mapping in the document, not only the ones at the top of a
    # definition: an inlined union branch keeps the Protobuf type it compiles to,
    # and dropping it out of this check is how the two drift apart unnoticed.
    def mappings(node: Any, name: str) -> None:
        if isinstance(node, list):
            for value in node:
                mappings(value, name)
            return
        if not isinstance(node, dict):
            return
        mapped = node.get("rift:proto", {}).get("type")
        if (
            isinstance(mapped, str)
            and mapped.startswith("rift.")
            and mapped not in names
        ):
            raise ValueError(f"$defs/{name} maps to missing Protobuf type {mapped}")
        for value in node.values():
            mappings(value, name)

    for name, definition in schema["$defs"].items():
        mappings(definition, name)


def write_or_check(path: Path, content: str, check: bool) -> bool:
    if check:
        if not path.exists() or path.read_text() != content:
            print(f"out of date: {path.relative_to(PROTOCOL.parent)}", file=sys.stderr)
            return False
        return True
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    files = [
        SchemaCompiler("core").compile(),
        SchemaCompiler("mcp").compile(),
        file_from_package(adapter.ADAPTER_PACKAGE),
        file_from_package(scip_api.SCIP_API_PACKAGE),
        file_from_package(scip.SCIP_PACKAGE),
    ]
    json_content = json.dumps(schema_output(), indent=2) + "\n"
    config_content = json.dumps(config_schema_output(), indent=2) + "\n"
    proto_content = {value.path: serde.serialize(value) for value in files}
    schema = validate_json_schema(json_content)
    validate_config_schema(config_content)
    validate_rift_toml(PROTOCOL.parent / "rift.toml")
    validate_proto(proto_content, schema)

    ok = write_or_check(PROTOCOL / "mcp.json", json_content, args.check)
    ok = (
        write_or_check(PROTOCOL / "rift.schema.json", config_content, args.check) and ok
    )
    for path, content in proto_content.items():
        ok = write_or_check(PROTOCOL / path, content, args.check) and ok
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
