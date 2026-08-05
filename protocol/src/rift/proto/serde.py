"""Convert compiler records into a typed Protobuf AST and serialize it."""

from __future__ import annotations

import textwrap

from proto_schema_parser import ast
from proto_schema_parser.generator import Generator

from . import model


def _comments(description: str | None, indent: int = 0) -> list[ast.Comment]:
    if not description:
        return []
    width = max(30, 96 - indent * 2 - 3)
    comments: list[ast.Comment] = []
    for paragraph in description.splitlines():
        if not paragraph.strip():
            comments.append(ast.Comment("//"))
            continue
        comments.extend(
            ast.Comment(f"// {line}")
            for line in textwrap.wrap(
                paragraph.strip(),
                width=width,
                break_long_words=False,
                break_on_hyphens=False,
            )
        )
    return comments


def _options(deprecated: bool) -> list[ast.Option]:
    return [ast.Option("deprecated", True)] if deprecated else []


def _field(value: model.Field, *, in_oneof: bool = False) -> ast.Field | ast.MapField:
    if value.map_key:
        return ast.MapField(
            name=value.name,
            number=value.number,
            key_type=value.map_key,
            value_type=value.map_value or "google.protobuf.Value",
            options=_options(value.deprecated),
        )
    cardinality = None
    if value.repeated:
        cardinality = ast.FieldCardinality.REPEATED
    elif value.optional and not in_oneof:
        cardinality = ast.FieldCardinality.OPTIONAL
    return ast.Field(
        name=value.name,
        number=value.number,
        type=value.type,
        cardinality=cardinality,
        options=_options(value.deprecated),
    )


def _enum(value: model.Enum, indent: int = 0) -> ast.Enum:
    elements: list[ast.EnumElement] = []
    if value.allow_alias:
        elements.append(ast.Option("allow_alias", True))
    if value.reserved_numbers or value.reserved_names:
        elements.append(
            ast.EnumReserved(
                ranges=[str(number) for number in value.reserved_numbers],
                names=value.reserved_names,
            )
        )
    for member in value.values:
        elements.extend(_comments(member.description, indent + 1))
        elements.append(
            ast.EnumValue(
                name=member.name,
                number=member.number,
                options=_options(member.deprecated),
            )
        )
    return ast.Enum(name=value.name, elements=elements)


def _message(value: model.Message, indent: int = 0) -> ast.Message:
    elements: list[ast.MessageElement] = []
    if value.section:
        elements.append(ast.Option("(section)", value.section))
    if value.reserved_numbers or value.reserved_ranges or value.reserved_names:
        ranges = [str(number) for number in value.reserved_numbers]
        ranges.extend(f"{start} to {end}" for start, end in value.reserved_ranges)
        elements.append(ast.Reserved(ranges=ranges, names=value.reserved_names))
    for enum in value.enums:
        elements.extend(_comments(enum.description, indent + 1))
        elements.append(_enum(enum, indent + 1))
    for message in value.messages:
        elements.extend(_comments(message.description, indent + 1))
        elements.append(_message(message, indent + 1))
    for field in value.fields:
        if field.oneof:
            continue
        elements.extend(_comments(field.description, indent + 1))
        elements.append(_field(field))
    for name in value.oneofs:
        oneof: list[ast.OneOfElement] = []
        for field in value.fields:
            if field.oneof != name:
                continue
            oneof.extend(_comments(field.description, indent + 2))
            typed = _field(field, in_oneof=True)
            if isinstance(typed, ast.MapField):
                raise TypeError(f"map field {field.name} cannot belong to oneof {name}")
            oneof.append(typed)
        elements.append(ast.OneOf(name=name, elements=oneof))
    return ast.Message(name=value.name, elements=elements)


def _service(value: model.Service) -> ast.Service:
    elements: list[ast.ServiceElement] = []
    for rpc in value.rpcs:
        elements.extend(_comments(rpc.description, 1))
        elements.append(
            ast.Method(
                name=rpc.name,
                input_type=ast.MessageType(rpc.request, stream=rpc.request_stream),
                output_type=ast.MessageType(rpc.response, stream=rpc.response_stream),
            )
        )
    return ast.Service(name=value.name, elements=elements)


def _ast(value: model.File) -> ast.File:
    elements: list[ast.FileElement] = [
        ast.Comment(
            "// Generated from the Pydantic models under protocol/src/rift/models. Do not edit."
        ),
        *_comments(value.description),
        ast.Package(value.package),
    ]
    elements.extend(ast.Option(name, option) for name, option in value.options.items())
    elements.extend(ast.Import(name) for name in value.imports)
    if value.section_option:
        elements.append(
            ast.Extension(
                typeName="google.protobuf.MessageOptions",
                elements=[
                    ast.Field(
                        name="section",
                        number=50000,
                        type="string",
                        cardinality=ast.FieldCardinality.OPTIONAL,
                    )
                ],
            )
        )
    for enum in value.enums:
        elements.extend(_comments(enum.description))
        elements.append(_enum(enum))
    for message in value.messages:
        elements.extend(_comments(message.description))
        elements.append(_message(message))
    for service in value.services:
        elements.extend(_comments(service.description))
        elements.append(_service(service))
    return ast.File(syntax="proto3", file_elements=elements)


def serialize(value: model.File) -> str:
    """Return one complete Protobuf source file."""
    return Generator().generate(_ast(value)) + "\n"
