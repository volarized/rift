#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Flag em-dash and en-dash characters in prose and source files.

The documenter skill bans "—" (em-dash) and "–" (en-dash) everywhere a
reader sees text; the author's dash is the plain hyphen-minus. Each occurrence
prints as path:line:column with highlighted context so it can be targeted and
replaced. Exit 0 means no findings; exit 1 lists them; exit 2 is a usage error.

`just dashes` runs it over every reader-facing surface; the same command
takes explicit paths while a change is still in progress:

    uv run --script scripts/check_dashes.py [PATH ...]

With no PATH arguments the script scans the default reader-facing surfaces
relative to the current directory: docs/content, docs/src/app, crates,
README.md. Do not point it at .claude: skill files quote the banned characters
on purpose (corpus evidence stays verbatim), and the directory is skipped when
reached through a scanned parent.
"""

from __future__ import annotations

import sys
from pathlib import Path

EM_DASH = "—"
EN_DASH = "–"
DASH_NAMES = {EM_DASH: "em-dash U+2014", EN_DASH: "en-dash U+2013"}

DEFAULT_TARGETS = ("docs/content", "docs/src/app", "crates", "README.md")
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
TEXT_SUFFIXES = {
    ".css",
    ".csv",
    ".html",
    ".js",
    ".json",
    ".jsx",
    ".md",
    ".mdx",
    ".py",
    ".rs",
    ".toml",
    ".ts",
    ".tsx",
    ".txt",
    ".yaml",
    ".yml",
}
FILE_BYTES_MAX = 2_000_000
CONTEXT_CHARS = 40

ADVICE = """\
Replace each with the plain hyphen-minus: spaced ' - ' for a break, apposition,
or list-item gloss; unspaced for a range (2019-2024); or restructure with a
colon. Rules: .claude/skills/documenter/SKILL.md (Banned outright) and the
'em-dash / en-dash' row in vocabulary.csv."""


def iter_files(targets: list[Path]) -> list[Path]:
    """Expand files and directories into scannable text files, skip-list applied."""
    files: list[Path] = []
    for target in targets:
        if target.is_file():
            files.append(target)
            continue
        for path in sorted(target.rglob("*")):
            if any(part in SKIP_DIRS for part in path.parts):
                continue
            if path.is_file() and path.suffix in TEXT_SUFFIXES:
                files.append(path)
    return files


def scan_file(path: Path) -> list[tuple[int, int, str, str]]:
    """Return (line, column, dash char, context) findings; both counts 1-based."""
    raw = path.read_bytes()
    if len(raw) > FILE_BYTES_MAX:
        print(f"note: skipped {path} ({len(raw)} bytes exceeds {FILE_BYTES_MAX})")
        return []
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return []
    findings: list[tuple[int, int, str, str]] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        for column, character in enumerate(line, start=1):
            if character not in DASH_NAMES:
                continue
            start = max(0, column - 1 - CONTEXT_CHARS)
            end = min(len(line), column + CONTEXT_CHARS)
            context = line[start : end]
            if start > 0:
                context = "..." + context
            if end < len(line):
                context = context + "..."
            findings.append((line_number, column, character, context))
    return findings


def highlight(context: str, use_color: bool) -> str:
    """Mark every banned dash inside the context slice."""
    for character in DASH_NAMES:
        marker = f"\x1b[1;31m{character}\x1b[0m" if use_color else f"[{character}]"
        context = context.replace(character, marker)
    return context


def main(arguments: list[str]) -> int:
    if arguments:
        targets = [Path(argument) for argument in arguments]
        missing = [target for target in targets if not target.exists()]
        if missing:
            print(f"error: no such path: {', '.join(str(m) for m in missing)}")
            return 2
    else:
        targets = [Path(name) for name in DEFAULT_TARGETS if Path(name).exists()]
        if not targets:
            print("error: none of the default targets exist here; pass paths explicitly")
            return 2

    use_color = sys.stdout.isatty()
    total = 0
    dirty_files = 0
    for path in iter_files(targets):
        findings = scan_file(path)
        if not findings:
            continue
        dirty_files += 1
        total += len(findings)
        print(f"\n{path}: {len(findings)} finding(s)")
        for line_number, column, character, context in findings:
            name = DASH_NAMES[character]
            shown = highlight(context, use_color)
            print(f"  {path}:{line_number}:{column} {name}: {shown}")

    if total == 0:
        print("NO EM OR EN DASHES FOUND")
        return 0
    print(f"\n{total} banned dash character(s) across {dirty_files} file(s).")
    print(ADVICE)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
