"""Protobuf descriptor types used by the authoring graph and serializer."""

from typing import Any, cast

from google.protobuf import descriptor_pb2
from google.protobuf.descriptor import FieldDescriptor as ProtoFieldDescriptor


def proto_type_name(value: int) -> str:
    """Return the `.proto` scalar spelling for an official descriptor enum value."""
    field_descriptor = cast(Any, descriptor_pb2).FieldDescriptorProto
    return field_descriptor.Type.Name(value).removeprefix("TYPE_").lower()


__all__ = ["ProtoFieldDescriptor", "proto_type_name"]
