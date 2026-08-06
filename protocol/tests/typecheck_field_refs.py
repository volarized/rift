"""Static contract for protocol field descriptors."""

from rift.models.base import FieldRef
from rift.models.mcp import TreeParams

field_ref: FieldRef[TreeParams, int | None] = TreeParams.limit


def instance_value(params: TreeParams) -> None:
    value: int | None = params.limit
    _ = value
