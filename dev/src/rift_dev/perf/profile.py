"""Read the call graph printed by the macOS sample profiler."""

from __future__ import annotations

import re
from collections import Counter
from dataclasses import dataclass

FRAME = re.compile(
    r"^([\s+!:|]*?)(\d+) (.+?)(?:  \(in [^)]+\))?(?: \+ [\d,.]+)?(?:  \[.*)?$"
)
IDLE = (
    "__psynch_cvwait",
    "kevent",
    "mach_msg",
    "__semwait_signal",
    "__ulock_wait",
    "__workq_kernreturn",
    "_dispatch_",
    "__select",
    "__psynch_mutexwait",
    "start_wqthread",
    "__sigsuspend",
    "read  (in libsystem_kernel",
    "__accept",
    "poll",
    "semaphore_wait",
)


@dataclass(frozen=True)
class Frame:
    depth: int
    count: int
    name: str


def parse_sample(text: str) -> list[Frame]:
    """Read frames inside Call graph, leaving image tables and summaries out."""
    frames = []
    started = False
    for line in text.splitlines():
        if line.startswith("Call graph:"):
            started = True
            continue
        if not started:
            continue
        if line.startswith(("Total number", "Sort by")):
            break
        match = FRAME.fullmatch(line)
        if match:
            frames.append(Frame(len(match[1]), int(match[2]), match[3]))
    if not started:
        raise ValueError("expected macOS sample text with a Call graph section")
    return frames


def on_cpu(
    frames: list[Frame], under: str = "", *, self_only: bool = False
) -> tuple[int, Counter[str]]:
    """Aggregate non-idle self ticks, crediting each ancestor once per stack."""
    totals: Counter[str] = Counter()
    stack: list[Frame] = []
    children: list[int] = []
    total = 0
    # A sentinel closes every remaining frame without rescanning its descendants.
    for frame in [*frames, Frame(-1, 0, "")]:
        while stack and stack[-1].depth >= frame.depth:
            finished = stack.pop()
            own = finished.count - children.pop()
            if stack:
                children[-1] += finished.count
            names = {item.name for item in stack} | {finished.name}
            idle = finished.name.startswith(IDLE)
            selected = not under or any(under in name for name in names)
            if own <= 0 or idle or not selected or finished.name.startswith("Thread_"):
                continue
            total += own
            for name in {finished.name} if self_only else names:
                if not name.startswith("Thread_"):
                    totals[name] += own
        stack.append(frame)
        children.append(0)
    return total, totals


def merged_tree(
    frames: list[Frame], root: str, depth: int
) -> tuple[int, Counter[tuple[str, ...]]]:
    """Merge child stacks below non-overlapping occurrences of a named frame."""
    totals: Counter[tuple[str, ...]] = Counter()
    stack: list[Frame] = []
    root_depth: int | None = None
    total = 0
    for frame in frames:
        if root_depth is not None and frame.depth <= root_depth:
            root_depth = None
            stack.clear()
        if root_depth is None:
            if root not in frame.name or "closure" in frame.name:
                continue
            root_depth = frame.depth
            total += frame.count
        else:
            while stack and stack[-1].depth >= frame.depth:
                stack.pop()
            stack.append(frame)
            path = tuple(item.name for item in stack)
            if len(path) <= depth:
                totals[path] += frame.count
    return total, totals
