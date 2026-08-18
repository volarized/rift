from __future__ import annotations

from pydantic import model_validator

from . import core, scip
from .base import *


@proto_enum(
    DirectEnum(
        RIFT_SCIP,
        name="Reason",
        parent=lambda: Omission,
        description="Why a populated Rift value is absent from the SCIP index.",
        value_descriptions=(
            "The server did not classify the omission.",
            "SCIP v0.9.0 has no field with the same meaning.",
            "No provider serves the file's language, or its facts were unavailable while the export ran.",
            "SCIP cannot carry the path: a symbolic link, or a form the SCIP canonical-path rules forbid.",
            "The unit cannot fit one 4 MiB stream event on its own.",
        ),
    )
)
class OmissionReason(IntEnum):
    REASON_UNSPECIFIED = 0
    REASON_UNREPRESENTABLE = 1
    REASON_NO_ANALYSIS = 2
    REASON_UNINDEXABLE_PATH = 3
    REASON_TOO_LARGE = 4


@proto_message(
    DirectMessage(
        RIFT_SCIP,
        description=(
            "One unit of Rift data absent from the projected index: a populated field the "
            "mapping dropped, or a whole file the export could not carry."
        ),
    )
)
class Omission(ProtoModel):
    field: Field[str | None] = proto_field(
        default=None,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "Core model and field path whose populated value was omitted, such as "
            "`Relationship.evidence`. Absent when the omission covers a whole file named "
            "by `path`."
        ),
    )
    reason: Field[OmissionReason] = proto_field(
        default=...,
        number=2,
        description="Why the value has no place in the projected index.",
    )
    detail: Field[str | None] = proto_field(
        default=None,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description=(
            "What was dropped, named concretely: the meaning SCIP lacks a field for, the "
            "language with no provider, or the forbidden path form."
        ),
    )
    count: Field[int] = proto_field(
        default=...,
        number=4,
        proto_type=ProtoFieldDescriptor.TYPE_UINT64,
        description="How many populated values or files this record covers across the index.",
    )
    language: Field[core.Language | None] = proto_field(
        default=None,
        number=5,
        description=(
            "The exact `Language` whose facts were omitted. Absent when no language claims "
            "the unit, as for a path the export never analyzed."
        ),
    )
    path: Field[core.ProjectPath | None] = proto_field(
        default=None,
        number=6,
        description="The file the omission covers. Absent when `field` names the unit.",
    )

    @model_validator(mode="after")
    def names_a_unit(self) -> Omission:
        if self.field is None and self.path is None:
            raise ValueError("an omission names a field or a path")
        return self


@proto_message(
    DirectMessage(RIFT_SCIP, description="Selects the tree to export as SCIP.")
)
class Request(ProtoModel):
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        number=2,
        description=(
            "Projection to export, or absent for the workspace tree. The export reads the "
            "tree as it stands, unpublished content included, so a write landing mid-stream "
            "can make the index inconsistent; a consumer that needs a whole one reads again."
        ),
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP,
        description="Analysis coverage for one language over the whole exported tree.",
    )
)
class LanguageCoverage(ProtoModel):
    language: Field[core.Language] = proto_field(
        default=...,
        number=1,
        description="The exact `Language` the coverage is stated for.",
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        default=...,
        number=2,
        description=(
            "The weakest per-file coverage the language's providers reported for each fact "
            "family across the exported tree."
        ),
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP, description="Identity and coverage emitted before index records."
    )
)
class Header(ProtoModel):
    projection: Field[core.ProjectionId | None] = proto_field(
        default=None,
        number=2,
        description=(
            "Projection that produced every following record, or absent for the workspace "
            "tree."
        ),
    )
    metadata: Field[scip.Metadata] = proto_field(
        default=...,
        number=3,
        description=(
            "The `metadata` of the projected `scip.Index`. `project_root` is the file URI "
            "of the exported tree's directory, `text_document_encoding` is UTF-8, and "
            "`tool_info` carries the name `rift` with the server build version."
        ),
    )
    coverage: Field[list[LanguageCoverage]] = proto_field(
        default=...,
        number=4,
        description=(
            "Analysis coverage per language, sorted by language name then dialect. It "
            "states what the underlying analysis established; what the SCIP mapping then "
            "dropped is in `omissions`."
        ),
    )
    omissions: Field[list[Omission]] = proto_field(
        default=...,
        number=5,
        description=(
            "Rift data absent from the index, sorted by language, field, path, and reason."
        ),
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP,
        description=(
            "One ordered part of a projected `scip.Index`. The server emits no event larger "
            "than 4 MiB, so a consumer with stock gRPC limits can read the stream; a unit "
            "that cannot fit alone is dropped and recorded as an `Omission`."
        ),
    )
)
class Event(ProtoModel):
    header: Field[Header | None] = proto_field(
        default=None,
        number=1,
        oneof=Oneof("value"),
        description="The first event. A stream contains exactly one header.",
    )
    document: Field[scip.Document | None] = proto_field(
        default=None,
        number=2,
        oneof=Oneof("value"),
        description="One index document in canonical path order.",
    )
    external_symbol: Field[scip.SymbolInformation | None] = proto_field(
        default=None,
        number=3,
        oneof=Oneof("value"),
        description=(
            "One external symbol after the final document, in byte order of the encoded "
            "SCIP symbol string."
        ),
    )


SCIP_API_PACKAGE = ProtoPackage(
    file=ProtoFile(
        "rift/scip.proto",
        RIFT_SCIP,
        description="The Rift server's read-only SCIP export API.",
        imports=("rift/core.proto", "scip/scip.proto"),
    ),
    models=(Omission, Request, LanguageCoverage, Header, Event),
    enums=(),
    services=(
        Service(
            "Index",
            (
                "Exports one Rift tree into the pinned SCIP schema. The service is served by "
                "the workspace server at the endpoint the `ServerLock` names."
            ),
            (
                Rpc(
                    "Read",
                    Request,
                    Event,
                    description=(
                        "Returns one header, then documents, then external symbols. The stream order is "
                        "the canonical `scip.Index` order. `Read` fails with `NOT_FOUND` for a projection "
                        "the server does not hold and `INVALID_ARGUMENT` for a malformed `ProjectionId`. "
                        "A projection removed mid-stream terminates the stream with `ABORTED`; a stream "
                        "that ends with `OK` carried the complete index."
                    ),
                    response_stream=True,
                ),
            ),
        ),
    ),
)
