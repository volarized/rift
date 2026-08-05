"""Serialize the Pydantic protocol model into JSON Schema and Protobuf."""

from __future__ import annotations

import argparse
import copy
import importlib.resources
import json
import re
import sys
import tempfile
from dataclasses import dataclass
from enum import Enum as PythonEnum
from pathlib import Path
from typing import Any, Union, cast

from google.protobuf import descriptor_pb2
from grpc_tools import protoc
from jsonschema.validators import Draft202012Validator
from pydantic import TypeAdapter

from .models import adapter, scip, scip_api
from .models.base import (
    DEFINITIONS,
    PROTO_ENUMS,
    PROTO_MESSAGES,
    DefinitionMetadata,
    ProtoPackage,
    rebuild_models,
)
from .models.document import DOCUMENT_METADATA
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
        result[model.__name__] = Definition(metadata.owner, metadata.public, body)
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
        schema = DEFINITION_MAP[name].body
        proto = schema.get("rift:proto", {})
        return proto.get("scalar")

    def enum(self, name: str, schema: dict[str, Any], *, prefixed: bool) -> EnumSpec:
        meta = schema["rift:proto"]
        values_meta = meta["values"]
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
                    name=wire["name"],
                    number=wire["number"],
                    description=descriptions.get(str(value)),
                )
            )
        return EnumSpec(name=name, description=schema.get("description"), values=values)

    def type_of(
        self,
        schema: dict[str, Any],
        nested_enums: list[tuple[str, dict[str, Any]]],
    ) -> tuple[str, bool, str | None, str | None]:
        schema = real_schema(schema)
        meta = schema.get("rift:proto", {})
        if "scalar" in meta:
            return meta["scalar"], False, None, None
        ref = ref_name(schema)
        target_schema = DEFINITION_MAP[ref].body if ref else None
        if ref and ("enum" in meta or "enum" in (target_schema or {})):
            enum_schema = copy.deepcopy(DEFINITION_MAP[ref].body)
            enum_meta = meta if "enum" in meta else enum_schema["rift:proto"]
            enum_schema["rift:proto"] = enum_meta
            nested_enums.append((enum_meta["enum"], enum_schema))
            return enum_meta["enum"], False, None, None
        if ref:
            return self.scalar_alias(ref) or self.qualify(ref), False, None, None
        if schema.get("contentEncoding") == "base64":
            return "bytes", False, None, None
        if "enum" in schema:
            nested_enums.append((meta["enum"], schema))
            return meta["enum"], False, None, None
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

    def compile_message(self, name: str, schema: dict[str, Any]) -> MessageSpec:
        if name in self.message_names:
            return next(message for message in self.messages if message.name == name)
        self.message_names.add(name)
        meta = schema["rift:proto"]
        required = set(schema.get("required", []))
        nested_raw: list[tuple[str, dict[str, Any]]] = []
        fields: list[FieldSpec] = []
        for json_name, field_schema in schema.get("properties", {}).items():
            wire = field_schema.get("rift:proto", {})
            if "number" not in wire:
                continue
            kind, repeated, map_key, map_value = self.type_of(field_schema, nested_raw)
            nullable = any(
                branch.get("type") == "null"
                for keyword in ["oneOf", "anyOf"]
                for branch in field_schema.get(keyword, [])
            )
            fields.append(
                FieldSpec(
                    name=wire.get("field", snake(json_name)),
                    number=wire["number"],
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
            and "number" in meta
        ):
            value, _, _, _ = self.type_of(schema["additionalProperties"], nested_raw)
            fields.append(
                FieldSpec(
                    name=meta.get("field", "entries"),
                    number=meta["number"],
                    type="map",
                    map_key="string",
                    map_value=value,
                )
            )
        prefixed = len(nested_raw) > 1
        nested = [
            self.enum(enum_name, enum_schema, prefixed=prefixed)
            for enum_name, enum_schema in nested_raw
        ]
        message = MessageSpec(
            name=name,
            description=schema.get("description"),
            fields=fields,
            enums=nested,
            reserved_numbers=meta.get("reservedNumbers", []),
            reserved_names=meta.get("reservedNames", []),
        )
        self.messages.append(message)
        return message

    def compile_union(self, name: str, schema: dict[str, Any]) -> MessageSpec:
        if name in self.message_names:
            return next(message for message in self.messages if message.name == name)
        self.message_names.add(name)
        meta = schema["rift:proto"]
        oneof = meta["oneof"]
        variants = meta["variants"]
        fields = [
            FieldSpec(
                name=variant["field"],
                number=variant["number"],
                type=variant["type"],
                oneof=oneof,
            )
            for variant in variants
        ]
        message = MessageSpec(
            name=name,
            description=schema.get("description"),
            fields=fields,
            oneofs=[oneof],
            reserved_numbers=meta.get("reservedNumbers", []),
            reserved_names=meta.get("reservedNames", []),
        )
        self.messages.append(message)
        return message

    def compile(self) -> FileSpec:
        for name, definition in DEFINITION_MAP.items():
            if definition.owner != self.owner:
                continue
            schema = definition.body
            meta = schema.get("rift:proto", {})
            if not definition.public and not any(
                key in meta for key in ["type", "scalar"]
            ):
                continue
            if "enum" in schema:
                if "type" not in meta:
                    continue
                if name not in self.enum_names:
                    self.enum_names.add(name)
                    self.enums.append(self.enum(name, schema, prefixed=True))
            elif ("oneOf" in schema or "anyOf" in schema) and "oneof" in meta:
                self.compile_union(name, schema)
            elif schema.get("properties") is not None or isinstance(
                schema.get("additionalProperties"), dict
            ):
                if "type" in meta:
                    self.compile_message(meta["type"].split(".")[-1], schema)
        imports = ["google/protobuf/struct.proto"]
        if any(
            "google.protobuf.Empty" in field.type
            for message in self.messages
            for field in message.fields
        ):
            imports.insert(0, "google/protobuf/empty.proto")
        if self.owner == "mcp":
            imports.insert(0, "rift/core.proto")
        services: list[ServiceSpec] = []
        if self.owner == "mcp":
            entries = cast(dict[str, Any], DOCUMENT_METADATA["rift:entryPoints"])
            tools = cast(dict[str, dict[str, Any]], entries["mcp.tools"])
            members = [
                *tools.values(),
                cast(dict[str, Any], entries["mcp.resources.read"]),
            ]
            rpcs: list[RpcSpec] = []
            for member in members:
                rpc = member["rpc"]
                service, method = rpc.rsplit("/", 1)
                if service != "rift.mcp.Rift":
                    raise ValueError(f"unsupported MCP service mapping {rpc}")
                request_name = ref_name(member["params"])
                response_name = ref_name(member["result"])
                if request_name is None or response_name is None:
                    raise ValueError(f"MCP service mapping {rpc} has no message types")
                request = DEFINITION_MAP[request_name].body["rift:proto"]["type"]
                response = DEFINITION_MAP[response_name].body["rift:proto"]["type"]
                rpcs.append(
                    RpcSpec(
                        name=method,
                        request=request,
                        response=response,
                        description=member.get("description"),
                    )
                )
            services.append(
                ServiceSpec(
                    name="Rift",
                    description=(
                        "The Protobuf service exposed by the Rift server. rift-mcp maps its "
                        "JSON entry points to these methods using mcp.json metadata."
                    ),
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


def message_from_model(model: type[Any]) -> MessageSpec:
    meta = model.__proto__.values
    wire_name = (
        f"{meta['package']}.{meta['name']}"
        if not meta.get("parent")
        else f"{meta['parent']}.{meta['name']}"
    )
    fields = []
    for pydantic_field in model.model_fields.values():
        extra = pydantic_field.json_schema_extra or {}
        value = extra["rift:proto"]
        fields.append(FieldSpec(**value))
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
        oneofs=meta.get("oneofs", []),
        reserved_numbers=meta.get("reserved_numbers", []),
        reserved_ranges=meta.get("reserved_ranges", []),
        reserved_names=meta.get("reserved_names", []),
        section=meta.get("section"),
    )


def file_from_package(package: ProtoPackage) -> FileSpec:
    spec = package.spec
    description = spec.get("description")
    if not description and spec["package"] == "rift.adapter":
        description = (
            "The Rift server owns one adapter process per language and calls it over gRPC on a Unix "
            "domain socket. Different-language adapters may open the same session worktree. Each keeps "
            "its own compiler state and cache. Rift serializes source writes, then refreshes every "
            "adapter that holds the worktree."
        )
    if not description and spec["package"] == "scip":
        release = spec["upstream_release"]
        description = (
            "SCIP is a language-neutral Protobuf format for code indexes. Rift protocol version 1 "
            f"pins the schema from SCIP {release}."
        )
    return FileSpec(
        path=spec["path"],
        package=spec["package"],
        description=description,
        imports=spec.get("imports", []),
        options=spec.get("options", {}),
        messages=[message_from_model(model) for model in package.models],
        enums=[enum_from_model(model) for model in package.enums],
        services=[
            ServiceSpec(
                name=service["name"],
                description=service.get("description"),
                rpcs=[RpcSpec(**rpc) for rpc in service["rpcs"]],
            )
            for service in package.services
        ],
        section_option=spec.get("section_option", False),
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
            if owner in parents and key in {"oneOf", "anyOf"} and isinstance(value, list):
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
    stays a definition even though it is also a branch of `Edit`. So does a
    variant two unions both declare, such as the empty branch `SymbolOrigin` and
    `OperationVerifier` share: folding it into the first would leave the second
    pointing at a definition that no longer exists.
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


def schema_output() -> dict[str, Any]:
    result = copy.deepcopy(DOCUMENT_METADATA)
    result["$defs"] = copy.deepcopy(PYDANTIC_SCHEMA["$defs"])
    inline_variants(result)
    return result


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
    proto_content = {value.path: serde.serialize(value) for value in files}
    schema = validate_json_schema(json_content)
    validate_proto(proto_content, schema)

    ok = write_or_check(PROTOCOL / "mcp.json", json_content, args.check)
    for path, content in proto_content.items():
        ok = write_or_check(PROTOCOL / path, content, args.check) and ok
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
