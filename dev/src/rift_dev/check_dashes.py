"""Report em dashes, en dashes, and middle dots in UTF-8 text.

`just dashes` checks the repository's tracked and unignored files. Pass paths
to check a particular file or directory, including generated files. Findings
report the path, line, column, and code point. Exit 1 indicates findings.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Iterator
from pathlib import Path

from rift_dev.commands import REPOSITORY, GitCommand

CHARACTER_NAMES = {"\u2014": "em dash", "\u2013": "en dash", "\u00b7": "middle dot"}
CHARACTER_BYTES = tuple(character.encode("utf-8") for character in CHARACTER_NAMES)
SKIP_DIRS = {
    ".git",
    ".claude",
    ".next",
    ".rift",
    "build",
    "dist",
    "node_modules",
    "target",
}
CONTEXT_CHARS = 40


def repository_files() -> Iterator[tuple[Path, Path]]:
    """Yield paths to check and their repository-relative names."""
    output = GitCommand(
        REPOSITORY, "ls-files", "-z", "--cached", "--others", "--exclude-standard"
    ).output_bytes()
    for name in sorted(set(output.split(b"\0")) - {b""}):
        relative = Path(os.fsdecode(name))
        path = REPOSITORY / relative
        if path.is_file() and not path.is_symlink():
            yield path, relative


def iter_files(targets: list[Path]) -> Iterator[tuple[Path, Path]]:
    """Yield files named by the caller, skipping build directories inside a target."""
    for target in targets:
        if target.is_file():
            yield target, target
        else:
            for directory, subdirectories, files in target.walk():
                subdirectories[:] = sorted(set(subdirectories) - SKIP_DIRS)
                for name in sorted(files):
                    path = directory / name
                    if path.is_file() and not path.is_symlink():
                        yield path, path


def scan_file(path: Path) -> list[tuple[int, int, str, str]]:
    """Return (line, column, character, context) findings with 1-based positions."""
    raw = path.read_bytes()
    if b"\0" in raw or not any(character in raw for character in CHARACTER_BYTES):
        return []
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return []
    findings: list[tuple[int, int, str, str]] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        for column, character in enumerate(line, start=1):
            if character not in CHARACTER_NAMES:
                continue
            start = max(0, column - 1 - CONTEXT_CHARS)
            end = min(len(line), column + CONTEXT_CHARS)
            context = line[start:end]
            if start > 0:
                context = "..." + context
            if end < len(line):
                context = context + "..."
            findings.append((line_number, column, character, context))
    return findings


def highlight(context: str, use_color: bool) -> str:
    """Show each banned character as a code point in the context slice."""
    for character in CHARACTER_NAMES:
        label = f"[U+{ord(character):04X}]"
        marker = f"\x1b[1;31m{label}\x1b[0m" if use_color else label
        context = context.replace(character, marker)
    return context


def main(arguments: list[str]) -> int:
    if arguments:
        targets = [Path(argument) for argument in arguments]
        missing = [target for target in targets if not target.exists()]
        if missing:
            raise FileNotFoundError(
                f"no such path: {', '.join(str(m) for m in missing)}"
            )
        files = iter_files(targets)
    else:
        files = repository_files()

    use_color = sys.stdout.isatty()
    total = 0
    dirty_files = 0
    for path, name in files:
        findings = scan_file(path)
        if not findings:
            continue
        dirty_files += 1
        total += len(findings)
        for line_number, column, character, context in findings:
            character_name = CHARACTER_NAMES[character]
            shown = highlight(context, use_color)
            print(
                f"{name}:{line_number}:{column}: "
                f"{character_name} U+{ord(character):04X}: {shown}"
            )

    if total == 0:
        print("No em dashes, en dashes, or middle dots found.")
        return 0
    print(f"\n{total} banned character(s) across {dirty_files} file(s).")
    return 1
