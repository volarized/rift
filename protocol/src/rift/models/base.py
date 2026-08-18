"""Metadata shared by the protocol's Pydantic models."""

from __future__ import annotations

import sys
from collections.abc import Callable
from dataclasses import dataclass
from enum import Enum, IntEnum
from typing import (
    Annotated,
    Any,
    ForwardRef,
    Generic,
    Literal,
    TypeVar,
    Union,
    cast,
    get_args,
    get_origin,
    overload,
)

from google.protobuf import empty_pb2
from google.protobuf.message import Message as ProtoMessage
from pydantic import (
    BaseModel,
    ConfigDict,
    GetJsonSchemaHandler,
    RootModel,
    model_validator,
)
from pydantic import (
    Field as schema_field,
)
from pydantic._internal._model_construction import ModelMetaclass
from pydantic.json_schema import JsonSchemaValue
from pydantic_core import CoreSchema, PydanticUndefined

from ..proto.types import ProtoFieldDescriptor, proto_type_name
from .surface import Rpc, Service

ProtoEmpty = cast(Any, empty_pb2).Empty

T = TypeVar("T")
M = TypeVar("M", bound=BaseModel)


@dataclass(frozen=True)
class FieldRef(Generic[M, T]):
    """The identity of one field in one protocol model."""

    owner: type[M]
    name: str
    field: Field[T]

    @property
    def proto_name(self) -> str:
        return self.field.proto_name or self.name.rstrip("_")


@dataclass(frozen=True)
class Oneof:
    """A named Protobuf oneof declared by fields that share it."""

    name: str


@dataclass(frozen=True)
class Namespace:
    owner: str
    package: str


CORE = Namespace("core", "rift.core")
MCP = Namespace("mcp", "rift.mcp")
SCIP = Namespace("scip", "scip")
RIFT_SCIP = Namespace("scip", "rift.scip")


@dataclass(frozen=True)
class Placement:
    name: str
    number: int


@dataclass(frozen=True)
class Variant:
    """One branch of a tagged union, named by its model or by the model's name.

    A branch declared further down the module is named as a string. Nothing here
    imports it: the union needs the name to build its own annotation, and Pydantic
    resolves that name against the module when the models are rebuilt.
    """

    tag: str | None
    name: str
    number: int
    model: type[Any] | str
    proto_model: type[ProtoMessage] | None = None

    @property
    def type_name(self) -> str:
        return self.model if isinstance(self.model, str) else self.model.__name__


@dataclass(frozen=True)
class EnumValue:
    json: str
    proto: str
    number: int


@dataclass(frozen=True)
class Proto:
    """Typed Protobuf identity attached to one JSON Schema definition."""

    kind: Literal["empty", "enum", "message", "scalar", "union"]
    scalar_type: int | None = None
    named: bool = False
    placement: Placement | None = None
    enum_name: str | None = None
    enum_values: tuple[EnumValue, ...] = ()
    oneof: Oneof | None = None
    variants: tuple[Variant, ...] = ()

    @classmethod
    def empty(cls) -> Proto:
        return cls("empty")

    @classmethod
    def scalar(cls, scalar_type: int) -> Proto:
        return cls("scalar", scalar_type=scalar_type)

    @classmethod
    def message(
        cls,
        *,
        named: bool = True,
        placement: Placement | None = None,
    ) -> Proto:
        return cls("message", named=named, placement=placement)

    @classmethod
    def enum(
        cls,
        name: str,
        values: tuple[EnumValue, ...],
        *,
        named: bool = False,
        placement: Placement | None = None,
    ) -> Proto:
        return cls(
            "enum",
            named=named,
            placement=placement,
            enum_name=name,
            enum_values=values,
        )

    @classmethod
    def union(
        cls,
        oneof: Oneof,
        variants: tuple[Variant, ...],
        *,
        named: bool = True,
        placement: Placement | None = None,
    ) -> Proto:
        return cls(
            "union",
            named=named,
            placement=placement,
            oneof=oneof,
            variants=variants,
        )

    def schema(self, namespace: Namespace, model: type[Any]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        if self.kind == "scalar":
            if self.scalar_type is None:
                raise TypeError(f"{model.__name__} has no scalar type")
            result.update(
                scalar=(proto_type_name(self.scalar_type)),
                package=namespace.package,
            )
        elif self.named:
            result["type"] = f"{namespace.package}.{model.__name__}"
        if self.placement:
            result.update(
                field=self.placement.name,
                number=self.placement.number,
            )
        if self.kind == "enum":
            result["enum"] = self.enum_name
            result["values"] = {
                value.json: {"name": value.proto, "number": value.number}
                for value in self.enum_values
            }
        if self.kind == "union":
            if self.oneof is None:
                raise TypeError(f"{model.__name__} has no oneof")
            result["oneof"] = self.oneof.name
            result["variants"] = [
                {
                    "tag": variant.tag,
                    "field": variant.name,
                    "number": variant.number,
                    "type": (
                        cast(Any, variant.proto_model).DESCRIPTOR.full_name
                        if variant.proto_model
                        else variant.type_name
                    ),
                }
                for variant in self.variants
            ]
        return result


class Field(Generic[T]):
    """A model value on instances and a typed field reference on model classes."""

    def __init__(
        self,
        *,
        number: int | None = None,
        proto_name: str | None = None,
        proto_type: int | None = None,
        oneof: Oneof | None = None,
        deprecated: bool = False,
        default: Any = PydanticUndefined,
        **schema: Any,
    ) -> None:
        self.number = number
        self.proto_name = proto_name
        self.proto_type = proto_type
        self.oneof = oneof
        self.deprecated = deprecated
        self.owner: type[BaseModel] | None = None
        self.name: str | None = None
        schema_extra = cast(dict[str, Any], schema.pop("json_schema_extra", {}) or {})
        wire: dict[str, Any] = {}
        if number is not None:
            wire["number"] = number
        if proto_name is not None:
            wire["field"] = proto_name
        if proto_type is not None:
            wire["scalar"] = proto_type_name(proto_type)
        if wire:
            schema_extra["rift:proto"] = wire
        if schema_extra:
            schema["json_schema_extra"] = schema_extra
        self.info = schema_field(default=default, **schema)

    def bind(self, owner: type[BaseModel], name: str) -> None:
        self.owner = owner
        self.name = name

    @overload
    def __get__(self, instance: None, owner: type[M]) -> FieldRef[M, T]: ...

    @overload
    def __get__(self, instance: M, owner: type[M] | None = None) -> T: ...

    def __get__(
        self,
        instance: BaseModel | None,
        owner: type[BaseModel] | None = None,
    ) -> Any:
        if instance is None:
            if owner is None or self.name is None:
                raise AttributeError("unbound protocol field")
            return FieldRef(owner, self.name, self)
        if self.name is None:
            raise AttributeError("unbound protocol field")
        return instance.__dict__[self.name]


def field(
    *,
    number: int | None = None,
    proto_name: str | None = None,
    proto_type: int | None = None,
    oneof: Oneof | None = None,
    deprecated: bool = False,
    default: Any = PydanticUndefined,
    **schema: Any,
) -> Field[Any]:
    return Field(
        number=number,
        proto_name=proto_name,
        proto_type=proto_type,
        oneof=oneof,
        deprecated=deprecated,
        default=default,
        **schema,
    )


def _field_value_annotation(annotation: Any) -> Any:
    if isinstance(annotation, str):
        prefix = "Field["
        if annotation.startswith(prefix) and annotation.endswith("]"):
            return annotation[len(prefix) : -1]
        return annotation
    if get_origin(annotation) is Field:
        return get_args(annotation)[0]
    return annotation


class ProtocolModelMetaclass(ModelMetaclass):
    """Lower authored field descriptors into Pydantic, then restore their class view."""

    def __new__(
        mcls,
        name: str,
        bases: tuple[type[Any], ...],
        namespace: dict[str, Any],
        **kwargs: Any,
    ) -> type[Any]:
        namespace = dict(namespace)
        annotations = dict(namespace.get("__annotations__", {}))
        declared: dict[str, Field[Any]] = {}
        for field_name, value in list(namespace.items()):
            if not isinstance(value, Field):
                continue
            if field_name not in annotations:
                raise TypeError(f"{name}.{field_name} has no field annotation")
            declared[field_name] = value
            annotations[field_name] = _field_value_annotation(annotations[field_name])
            namespace[field_name] = value.info
        namespace["__annotations__"] = annotations

        model = cast(type[Any], super().__new__(mcls, name, bases, namespace, **kwargs))
        inherited: dict[str, Field[Any]] = {}
        for base in reversed(model.__mro__[1:]):
            inherited.update(getattr(base, "__protocol_fields__", {}))
        inherited.update(declared)
        model.__protocol_fields__ = inherited
        for field_name, value in declared.items():
            value.bind(model, field_name)
            setattr(model, field_name, value)
        return model


class ProtocolRoot(RootModel[T], Generic[T]):
    model_config = ConfigDict(regex_engine="python-re")


class ClosedModel(BaseModel, metaclass=ProtocolModelMetaclass):
    model_config = ConfigDict(extra="forbid", regex_engine="python-re")


class ProtoModel(BaseModel, metaclass=ProtocolModelMetaclass):
    model_config = ConfigDict(arbitrary_types_allowed=True)


def closed_config(schema_extra: dict[str, Any]) -> ConfigDict:
    return ConfigDict(
        extra="forbid",
        regex_engine="python-re",
        json_schema_extra=schema_extra,
    )


@dataclass(frozen=True)
class DefinitionMetadata:
    namespace: Namespace
    public: bool
    mapping: Proto
    schema_extra: dict[str, Any]
    model: type[Any]

    @property
    def proto(self) -> dict[str, Any]:
        return self.mapping.schema(self.namespace, self.model)


@dataclass(frozen=True)
class ProtoMetadata:
    declaration: DirectMessage | DirectEnum
    model: type[Any]

    @property
    def values(self) -> dict[str, Any]:
        return self.declaration.values(self.model)


DirectParent = type[Any] | Callable[[], type[Any]]


def _direct_parent_name(parent: DirectParent | None) -> str | None:
    if parent is None:
        return None
    model = parent if isinstance(parent, type) else parent()
    metadata = cast(Any, model).__proto__.values
    return f"{metadata['package']}.{metadata['name']}"


@dataclass(frozen=True)
class DirectMessage:
    namespace: Namespace
    name: str | None = None
    description: str | None = None
    reserved_numbers: tuple[int, ...] = ()
    reserved_ranges: tuple[tuple[int, int], ...] = ()
    reserved_names: tuple[str, ...] = ()
    section: str | None = None

    def values(self, model: type[Any]) -> dict[str, Any]:
        return {
            "package": self.namespace.package,
            "name": self.name or model.__name__,
            "parent": None,
            "description": self.description,
            "reserved_numbers": list(self.reserved_numbers),
            "reserved_ranges": list(self.reserved_ranges),
            "reserved_names": list(self.reserved_names),
            "section": self.section,
        }


@dataclass(frozen=True)
class DirectEnum:
    namespace: Namespace
    name: str | None = None
    parent: DirectParent | None = None
    description: str | None = None
    value_descriptions: tuple[str | None, ...] = ()
    allow_alias: bool = False
    reserved_numbers: tuple[int, ...] = ()
    reserved_names: tuple[str, ...] = ()

    def values(self, model: type[IntEnum]) -> dict[str, Any]:
        members = list(model.__members__.items())
        descriptions = self.value_descriptions or (None,) * len(members)
        if len(descriptions) != len(members):
            raise ValueError(f"{model.__name__} has a description-count mismatch")
        return {
            "package": self.namespace.package,
            "name": self.name or model.__name__,
            "parent": _direct_parent_name(self.parent),
            "description": self.description,
            "allow_alias": self.allow_alias,
            "reserved_numbers": list(self.reserved_numbers),
            "reserved_names": list(self.reserved_names),
            "values": [
                {"name": name, "number": member.value, "description": description}
                for (name, member), description in zip(
                    members, descriptions, strict=True
                )
            ],
        }


@dataclass(frozen=True)
class ProtoOption:
    name: str
    value: str | bool


@dataclass(frozen=True)
class ProtoFile:
    path: str
    namespace: Namespace
    description: str | None = None
    imports: tuple[str, ...] = ()
    options: tuple[ProtoOption, ...] = ()
    section_option: bool = False
    upstream_release: str | None = None
    upstream_schema: str | None = None


@dataclass(frozen=True)
class ProtoPackage:
    file: ProtoFile
    models: tuple[type[ProtoModel], ...]
    enums: tuple[type[IntEnum], ...]
    services: tuple[Service, ...] = ()


DEFINITIONS: list[type[Any]] = []
PROTO_MESSAGES: list[type[ProtoModel]] = []
PROTO_ENUMS: list[type[IntEnum]] = []


def definition(
    *,
    owner: Namespace,
    public: bool,
    proto: Proto,
    schema_extra: dict[str, Any],
):
    def decorate(model: type[T]) -> type[T]:
        metadata = DefinitionMetadata(owner, public, proto, schema_extra, model)
        dynamic = cast(Any, model)
        dynamic.__protocol__ = metadata

        def add_metadata(schema: JsonSchemaValue) -> None:
            schema.update(
                schema_extra,
                **{
                    "rift:proto": metadata.proto,
                    "rift:package": owner.package,
                },
            )

        def emit_schema(
            cls: type[Any],
            core_schema: CoreSchema,
            handler: GetJsonSchemaHandler,
        ) -> JsonSchemaValue:
            schema = handler(core_schema)
            add_metadata(schema)
            return schema

        if issubclass(model, BaseModel):
            pydantic_model = cast(type[BaseModel], model)
            if issubclass(model, RootModel):
                dynamic.__get_pydantic_json_schema__ = classmethod(emit_schema)
            else:
                current = pydantic_model.model_config.get("json_schema_extra")

                def emit_model_schema(schema: JsonSchemaValue) -> None:
                    if isinstance(current, dict):
                        schema.update(cast(dict[str, Any], current))
                    elif callable(current):
                        cast(Callable[[JsonSchemaValue], None], current)(schema)
                    add_metadata(schema)

                pydantic_model.model_config["json_schema_extra"] = emit_model_schema
        elif issubclass(model, Enum):
            dynamic.__get_pydantic_json_schema__ = classmethod(emit_schema)
        DEFINITIONS.append(model)
        return model

    return decorate


def _root_model(cls: type[Any], annotation: Any) -> type[Any]:
    """Rebuild one authored root definition as the Pydantic model it describes.

    The authored class carries a name, docstring, and optional model validators;
    Pydantic needs `RootModel[...]` parametrized with the value type. Building
    it here keeps the value type out of a string in the model files, where a
    regex would have to survive two rounds of escaping and no checker would read
    it. Validators are redecorated on the rebuilt class so scalar and union
    semantic checks cannot disappear silently.
    """

    namespace: dict[str, Any] = {
        "__doc__": cls.__doc__,
        "__module__": cls.__module__,
        "__qualname__": cls.__qualname__,
    }

    decorators = cls.__pydantic_decorators__
    unsupported = {
        "field validators": decorators.field_validators,
        "field serializers": decorators.field_serializers,
        "model serializers": decorators.model_serializers,
        "computed fields": decorators.computed_fields,
    }
    for kind, authored in unsupported.items():
        if authored:
            raise TypeError(f"{cls.__name__} root definition cannot carry {kind}")
    for name, decorator in decorators.model_validators.items():
        namespace[name] = model_validator(mode=decorator.info.mode)(decorator.func)

    return cast(
        type[Any],
        type(
            cls.__name__,
            (ProtocolRoot[annotation],),
            namespace,
        ),
    )


def _constrained(root: Any, constraints: dict[str, Any]) -> Any:
    if not constraints:
        return root
    return Annotated[root, schema_field(**constraints)]


def scalar(
    *,
    owner: Namespace,
    proto: int,
    root: Any,
    public: bool = True,
    schema_extra: dict[str, Any] | None = None,
    **constraints: Any,
):
    """One JSON scalar and the Protobuf scalar it maps to.

    The docstring is the schema description, so it is written once. `constraints`
    are the JSON Schema keywords Pydantic accepts — `pattern`, `min_length`,
    `examples`.
    """

    def decorate(cls: type[T]) -> type[T]:
        model = _root_model(cls, _constrained(root, constraints))
        return definition(
            owner=owner,
            public=public,
            proto=Proto.scalar(proto),
            schema_extra=schema_extra or {},
        )(model)

    return decorate


def mapping(
    *,
    owner: Namespace,
    root: Any,
    public: bool = True,
    placement: Placement | None = None,
    schema_extra: dict[str, Any] | None = None,
    **constraints: Any,
):
    """A JSON object keyed by a scalar, carried as a Protobuf map field."""

    def decorate(cls: type[T]) -> type[T]:
        model = _root_model(cls, _constrained(root, constraints))
        return definition(
            owner=owner,
            public=public,
            proto=Proto.message(placement=placement),
            schema_extra=schema_extra or {},
        )(model)

    return decorate


def union(
    *,
    owner: Namespace,
    oneof: str,
    variants: tuple[Variant, ...],
    discriminator: str | None = None,
    public: bool = True,
    named: bool = True,
    placement: Placement | None = None,
    schema_extra: dict[str, Any] | None = None,
):
    """A tagged union, described once by its variants.

    The Python type, the JSON `oneOf`, and the Protobuf oneof all come from
    `variants`, so a branch cannot be added to one and missed in another. A branch
    named as a string becomes a `ForwardRef`, which Pydantic resolves against the
    module when the models are rebuilt.

    Every branch is a message carrying the tag, so the union admits objects and
    nothing else and says so at its root. The `oneOf` alone leaves it unsaid, and
    a union is what a tool result is: MCP reads an advertised `outputSchema` as
    an object schema, and a client that validates one rejects the entire
    `tools/list` when the root omits `"type"`.
    """

    def decorate(cls: type[T]) -> type[T]:
        members = tuple(
            variant.model
            if isinstance(variant.model, type)
            else ForwardRef(variant.model)
            for variant in variants
        )
        annotation: Any = Union[members]  # noqa: UP007 - the members are dynamic
        if discriminator:
            annotation = Annotated[
                annotation, schema_field(discriminator=discriminator)
            ]
        model = _root_model(cls, annotation)
        return definition(
            owner=owner,
            public=public,
            proto=Proto.union(Oneof(oneof), variants, named=named, placement=placement),
            schema_extra={"type": "object", **(schema_extra or {})},
        )(model)

    return decorate


def proto_message(declaration: DirectMessage):
    def decorate(model: type[T]) -> type[T]:
        typed = cast(type[ProtoModel], model)
        cast(Any, typed).__proto__ = ProtoMetadata(declaration, typed)
        PROTO_MESSAGES.append(typed)
        return model

    return decorate


def proto_enum(declaration: DirectEnum):
    def decorate(model: type[T]) -> type[T]:
        typed = cast(type[IntEnum], model)
        cast(Any, typed).__proto__ = ProtoMetadata(declaration, typed)
        PROTO_ENUMS.append(typed)
        return model

    return decorate


def proto_field(
    *,
    default: Any = PydanticUndefined,
    number: int | None = None,
    proto_name: str | None = None,
    proto_type: int | None = None,
    oneof: Oneof | None = None,
    deprecated: bool = False,
    **schema: Any,
) -> Field[Any]:
    """Declare one protocol field from typed Python and Protobuf facts."""
    return field(
        default=default,
        number=number,
        proto_name=proto_name,
        proto_type=proto_type,
        oneof=oneof,
        deprecated=deprecated,
        **schema,
    )


def rebuild_models() -> None:
    for model in [*DEFINITIONS, *PROTO_MESSAGES]:
        rebuild = getattr(model, "model_rebuild", None)
        if rebuild is None:
            continue
        namespace = vars(sys.modules[model.__module__])
        rebuild(force=True, _types_namespace=namespace)


__all__ = [
    "CORE",
    "DEFINITIONS",
    "MCP",
    "PROTO_ENUMS",
    "PROTO_MESSAGES",
    "Annotated",
    "Any",
    "ClosedModel",
    "ConfigDict",
    "DirectEnum",
    "DirectMessage",
    "Enum",
    "EnumValue",
    "Field",
    "FieldRef",
    "IntEnum",
    "Literal",
    "Namespace",
    "Oneof",
    "Placement",
    "Proto",
    "RIFT_SCIP",
    "SCIP",
    "ProtoEmpty",
    "ProtoFieldDescriptor",
    "ProtoFile",
    "ProtoModel",
    "ProtoOption",
    "ProtoPackage",
    "ProtocolRoot",
    "Rpc",
    "Service",
    "Variant",
    "closed_config",
    "definition",
    "field",
    "mapping",
    "proto_enum",
    "proto_field",
    "proto_message",
    "proto_type_name",
    "rebuild_models",
    "scalar",
    "schema_field",
    "union",
]
