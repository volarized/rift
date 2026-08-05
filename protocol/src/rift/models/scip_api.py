from __future__ import annotations

from . import core, scip
from .base import *


@proto_enum(
    {
        "package": "rift.scip",
        "name": "Reason",
        "parent": "rift.scip.Omission",
        "description": "Why a populated Rift value is absent from the SCIP index.",
        "values": [
            {
                "name": "REASON_UNSPECIFIED",
                "number": 0,
                "description": "The server did not classify the omission.",
            },
            {
                "name": "REASON_UNREPRESENTABLE",
                "number": 1,
                "description": "SCIP v0.9.0 has no field with the same meaning.",
            },
            {
                "name": "REASON_OUTSIDE_PROJECTION",
                "number": 2,
                "description": "The projection contract excludes the value. `detail` states the boundary.",
            },
        ],
    }
)
class OmissionReason(IntEnum):
    REASON_UNSPECIFIED = 0
    REASON_UNREPRESENTABLE = 1
    REASON_OUTSIDE_PROJECTION = 2


@proto_message(
    {
        "package": "rift.scip",
        "name": "Omission",
        "parent": None,
        "description": "One populated Rift field absent from the projected index.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Omission(ProtoModel):
    field: str = proto_field(
        default=...,
        spec={
            "name": "field",
            "number": 1,
            "type": "string",
            "description": "Core model and field path whose populated value was omitted.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    reason: OmissionReason = proto_field(
        default=...,
        spec={
            "name": "reason",
            "number": 2,
            "type": "rift.scip.Omission.Reason",
            "description": "Why the value has no field in the projected index.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    detail: str | None = proto_field(
        default=None,
        spec={
            "name": "detail",
            "number": 3,
            "type": "string",
            "description": "The mapping boundary when `reason` is `REASON_OUTSIDE_PROJECTION`.",
            "repeated": False,
            "optional": True,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.scip",
        "name": "Request",
        "parent": None,
        "description": "Selects the Rift snapshot to project into SCIP.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Request(ProtoModel):
    at: core.Snapshot = proto_field(
        default=...,
        spec={
            "name": "at",
            "number": 1,
            "type": "rift.core.Snapshot",
            "description": "The exact repository state to project.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.scip",
        "name": "Header",
        "parent": None,
        "description": "Identity and coverage emitted before index records.",
        "oneofs": [],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Header(ProtoModel):
    at: core.Snapshot = proto_field(
        default=...,
        spec={
            "name": "at",
            "number": 1,
            "type": "rift.core.Snapshot",
            "description": "The snapshot that produced every following record.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    metadata: scip.Metadata = proto_field(
        default=...,
        spec={
            "name": "metadata",
            "number": 2,
            "type": ".scip.Metadata",
            "description": "The metadata field of the projected `scip.Index`.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    coverage: core.SemanticCoverage = proto_field(
        default=...,
        spec={
            "name": "coverage",
            "number": 3,
            "type": "rift.core.SemanticCoverage",
            "description": "Availability of each Rift fact family used by the projection.",
            "repeated": False,
            "optional": False,
            "deprecated": False,
        },
    )
    omissions: list[Omission] = proto_field(
        default=...,
        spec={
            "name": "omissions",
            "number": 4,
            "type": "rift.scip.Omission",
            "description": "Populated Rift fields absent from the index, sorted by field and reason.",
            "repeated": True,
            "optional": False,
            "deprecated": False,
        },
    )


@proto_message(
    {
        "package": "rift.scip",
        "name": "Event",
        "parent": None,
        "description": "One ordered part of a projected `scip.Index`.",
        "oneofs": ["value"],
        "reserved_numbers": [],
        "reserved_ranges": [],
        "reserved_names": [],
        "section": None,
    }
)
class Event(ProtoModel):
    header: Header | None = proto_field(
        default=None,
        spec={
            "name": "header",
            "number": 1,
            "type": "rift.scip.Header",
            "description": "The first event. A stream contains exactly one header.",
            "repeated": False,
            "optional": False,
            "oneof": "value",
            "deprecated": False,
        },
    )
    document: scip.Document | None = proto_field(
        default=None,
        spec={
            "name": "document",
            "number": 2,
            "type": ".scip.Document",
            "description": "One index document in canonical path order.",
            "repeated": False,
            "optional": False,
            "oneof": "value",
            "deprecated": False,
        },
    )
    external_symbol: scip.SymbolInformation | None = proto_field(
        default=None,
        spec={
            "name": "external_symbol",
            "number": 3,
            "type": ".scip.SymbolInformation",
            "description": "One external symbol after the final document, in encoded-symbol order.",
            "repeated": False,
            "optional": False,
            "oneof": "value",
            "deprecated": False,
        },
    )


SCIP_API_PACKAGE = ProtoPackage(
    spec={
        "path": "rift/scip.proto",
        "package": "rift.scip",
        "description": "The Rift server's read-only SCIP projection API.",
        "imports": ["rift/core.proto", "scip/scip.proto"],
        "options": {},
        "section_option": False,
    },
    models=(Omission, Request, Header, Event),
    enums=(),
    services=[
        {
            "name": "Projection",
            "description": "Projects one Rift snapshot into the pinned SCIP schema.",
            "rpcs": [
                {
                    "name": "Read",
                    "request": "rift.scip.Request",
                    "response": "rift.scip.Event",
                    "description": "Returns one header, then documents, then external symbols. The stream order is the canonical `scip.Index` order.",
                    "request_stream": False,
                    "response_stream": True,
                }
            ],
        }
    ],
)
