from __future__ import annotations

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
            "The projection contract excludes the value. `detail` states the boundary.",
        ),
    )
)
class OmissionReason(IntEnum):
    REASON_UNSPECIFIED = 0
    REASON_UNREPRESENTABLE = 1
    REASON_OUTSIDE_PROJECTION = 2


@proto_message(
    DirectMessage(
        RIFT_SCIP,
        description="One populated Rift field absent from the projected index.",
    )
)
class Omission(ProtoModel):
    field: Field[str] = proto_field(
        default=...,
        number=1,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="Core model and field path whose populated value was omitted.",
    )
    reason: Field[OmissionReason] = proto_field(
        default=...,
        number=2,
        description="Why the value has no field in the projected index.",
    )
    detail: Field[str | None] = proto_field(
        default=None,
        number=3,
        proto_type=ProtoFieldDescriptor.TYPE_STRING,
        description="The mapping boundary when `reason` is `REASON_OUTSIDE_PROJECTION`.",
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP, description="Selects the Rift snapshot to project into SCIP."
    )
)
class Request(ProtoModel):
    at: Field[core.Snapshot] = proto_field(
        default=..., number=1, description="The exact repository state to project."
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP, description="Identity and coverage emitted before index records."
    )
)
class Header(ProtoModel):
    at: Field[core.Snapshot] = proto_field(
        default=...,
        number=1,
        description="The snapshot that produced every following record.",
    )
    metadata: Field[scip.Metadata] = proto_field(
        default=...,
        number=2,
        description="The metadata field of the projected `scip.Index`.",
    )
    coverage: Field[core.SemanticCoverage] = proto_field(
        default=...,
        number=3,
        description="Availability of each Rift fact family used by the projection.",
    )
    omissions: Field[list[Omission]] = proto_field(
        default=...,
        number=4,
        description="Populated Rift fields absent from the index, sorted by field and reason.",
    )


@proto_message(
    DirectMessage(
        RIFT_SCIP, description="One ordered part of a projected `scip.Index`."
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
        description="One external symbol after the final document, in encoded-symbol order.",
    )


SCIP_API_PACKAGE = ProtoPackage(
    file=ProtoFile(
        "rift/scip.proto",
        RIFT_SCIP,
        description="The Rift server's read-only SCIP export API.",
        imports=("rift/core.proto", "scip/scip.proto"),
    ),
    models=(Omission, Request, Header, Event),
    enums=(),
    services=(
        Service(
            "Index",
            "Exports one Rift snapshot into the pinned SCIP schema.",
            (
                Rpc(
                    "Read",
                    Request,
                    Event,
                    description=(
                        "Returns one header, then documents, then external symbols. The stream order is "
                        "the canonical `scip.Index` order."
                    ),
                    response_stream=True,
                ),
            ),
        ),
    ),
)
