"""Write the table of inherent methods of the Rust standard library.

The static pass reads `receivers/standard.rs` to decide whether a call on a slice,
an array or a reference runs a method of the standard library. The rows come from
the `std` pages rustdoc rendered for one release: every stable inherent method with
a `self` parameter. Nothing reads the toolchain of the host, at build time or here;
the command takes the folder of the pages, and the table names their release.

    uv run --locked --project dev rift-dev standard-methods \\
        "$(rustc --print sysroot)/share/doc/rust/html"

The pages are the `rust-docs` component of a toolchain. A method `std` gains after
that release is not in the table, so a call of it keeps the target of a package
trait method of that name until the table is written again. The command refuses
pages of a release before the one the table names. Run `just generate` afterwards.
"""

from __future__ import annotations

import html
import re
import textwrap
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from rift_dev.commands import REPOSITORY

TABLE = (
    REPOSITORY / "crates/rift-analysis/src/static_resolution/rust/receivers/standard.rs"
)
PAGE = re.compile(r"(?:struct|enum|union|primitive)\.(\w+)\.html")
RELEASE_FILE = re.compile(r"crates(\d+\.\d+\.\d+)\.js")
RELEASE_TEXT = re.compile(r"documentation of `std` (\d+\.\d+\.\d+)\.")
ROWS = re.compile(r"const METHODS: \[Methods; \d+\] = \[\n.*?\n\];\n", re.DOTALL)
IMPLEMENTATION = re.compile(r'(?=<section id="impl-)')
HEADER = re.compile(r'<h3 class="code-header">(.*?)</h3>', re.DOTALL)
METHOD = re.compile(
    r'<section id="method\.[\w-]+" class="method">(.*?)</section>'
    r'(.*?)(?=<section id="method\.|$)',
    re.DOTALL,
)
NAME = re.compile(r'class="fn">(\w+)</a>(.*)', re.DOTALL)
# Sections after the inherent methods of a type.
LATER_SECTIONS = (
    'id="deref-methods-',
    'id="trait-implementations"',
    'id="synthetic-implementations"',
    'id="blanket-implementations"',
)
RECEIVERS = (
    (re.compile(r"\((?:mut )?self\s*[,)]"), "&[]"),
    (re.compile(r"\(&(?:'\w+ )?self\b"), "&[Shared]"),
    (re.compile(r"\(&(?:'\w+ )?mut self\b"), "&[Unique]"),
    (re.compile(r"\(self: &mut &'\w+ \["), "&[Unique, Shared]"),
    (re.compile(r"\(self: &mut &'\w+ mut \["), "&[Unique, Unique]"),
)


@dataclass(frozen=True)
class Method:
    """One inherent method and the type that declares it."""

    owner: str
    receiver: str
    name: str


def _text(markup: str) -> str:
    return html.unescape(re.sub(r"<[^>]+>", "", markup))


def _owner(page: str, header: str) -> str | None:
    """The name of the type an implementation header of a page is for."""
    if page not in {"slice", "array"}:
        return page
    written = header.partition("where")[0].strip().removeprefix("impl")
    if written.startswith("<"):
        written = written[written.index(">") + 1 :]
    written = written.strip()
    if not written.startswith("["):
        # `Box<[T]>` and the like: no method of the slice itself.
        return None
    inner = written[1:-1].strip()
    if page == "slice" and inner == "[T; N]":
        return "slice:array"
    element = inner.rpartition(";")[0].strip() if page == "array" else inner
    return page if element == "T" else f"{page}:{element.partition('<')[0]}"


def page_methods(page: str, markup: str) -> list[Method]:
    """The stable inherent methods with a `self` parameter on one page."""
    start = markup.find('id="implementations"')
    if start < 0:
        return []
    ends = [markup.find(section, start) for section in LATER_SECTIONS]
    body = markup[start : min((end for end in ends if end > 0), default=len(markup))]
    methods = []
    for block in IMPLEMENTATION.split(body)[1:]:
        header = HEADER.search(block)
        owner = _owner(page, _text(header.group(1)) if header else "")
        for signature, after in METHOD.findall(block):
            named = NAME.search(signature)
            unstable = "stab unstable" in after.partition("docblock")[0]
            if owner is None or named is None or unstable:
                continue
            if 'class="since"' not in signature:
                continue
            parameters = _text(named.group(2))
            parameters = parameters[parameters.find("(") :]
            for pattern, receiver in RECEIVERS:
                if pattern.match(parameters):
                    methods.append(Method(owner, receiver, named.group(1)))
                    break
    return methods


def documented_methods(pages: Path) -> set[Method]:
    """The methods of every type page below the folder of the `std` crate."""
    methods: set[Method] = set()
    for path in sorted(pages.rglob("*.html")):
        page = PAGE.fullmatch(path.name)
        if page is None:
            continue
        module = path.parent.relative_to(pages).parts
        primitive = path.name.startswith("primitive.")
        owner = page.group(1) if primitive else "::".join((*module, page.group(1)))
        methods.update(page_methods(owner, path.read_text(encoding="utf-8")))
    return methods


def table_rows(methods: set[Method]) -> str:
    """The table constant: one row for the methods the same types declare."""
    owners: defaultdict[tuple[str, str], set[str]] = defaultdict(set)
    for method in methods:
        owners[(method.receiver, method.name)].add(method.owner)
    groups: defaultdict[tuple[tuple[str, ...], str], list[str]] = defaultdict(list)
    for (receiver, name), types in owners.items():
        groups[(tuple(sorted(types)), receiver)].append(name)
    order = {receiver: index for index, (_, receiver) in enumerate(RECEIVERS)}

    def names(values: tuple[str, ...] | list[str]) -> str:
        return "&[" + ", ".join(f'"{value}"' for value in sorted(values)) + "]"

    lines = [f"const METHODS: [Methods; {len(groups)}] = ["]
    for types, receiver in sorted(
        groups, key=lambda group: (group[0], order[group[1]])
    ):
        row = f"({names(types)}, {receiver}, {names(groups[(types, receiver)])}),"
        lines += textwrap.wrap(
            row,
            96,
            initial_indent="    ",
            subsequent_indent="        ",
            break_long_words=False,
            break_on_hyphens=False,
        )
    return "\n".join([*lines, "];\n"])


def _release(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in text.split("."))


def write_table(documentation: Path, table: Path = TABLE) -> str:
    """Write the rows of one documentation folder and return their release."""
    releases = [RELEASE_FILE.fullmatch(path.name) for path in documentation.iterdir()]
    found = [release.group(1) for release in releases if release is not None]
    if len(found) != 1:
        raise ValueError(f"no single release in {documentation}: {found}")
    source = table.read_text(encoding="utf-8")
    named = RELEASE_TEXT.search(source)
    if named is None or ROWS.search(source) is None:
        raise ValueError(f"{table} has no release or no rows")
    if _release(found[0]) < _release(named.group(1)):
        raise ValueError(f"std {found[0]} is before {named.group(1)} of the table")
    rows = table_rows(documented_methods(documentation / "std"))
    source = ROWS.sub(lambda _: rows, source)
    source = RELEASE_TEXT.sub(f"documentation of `std` {found[0]}.", source)
    table.write_text(source, encoding="utf-8")
    return found[0]
