"""Static contract for protocol field descriptors."""

from rift.models.base import FieldRef
from rift.models.mcp import SearchParams

field_ref: FieldRef[SearchParams, int | None] = SearchParams.limit


def instance_value(params: SearchParams) -> None:
    value: int | None = params.limit
    _ = value
