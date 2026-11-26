"""Collect source-backed Java and Kotlin facts with the JVM compilers."""

import argparse
from array import array
from dataclasses import dataclass
from enum import Enum
from functools import cached_property, partial
import os
from pathlib import Path
import subprocess
import tempfile
import time

from python import serve

SOURCE_SUPPORT_BYTES_MAX = 4 << 20
SOURCE_SUPPORT_COUNT_MAX = 512
OBSERVATIONS_MAX = 16_384
COMPILER_SECONDS_MAX = 24


class Language(str, Enum):
    JAVA = ".java"
    KOTLIN = ".kt"


class Category(str, Enum):
    CALLS = "calls"
    REFERENCES = "references"
    IMPORTS = "imports"


class Coverage(str, Enum):
    COMPLETE = "complete"
    PARTIAL = "partial"
    UNSUPPORTED = "unsupported"


def filename(source):
    return source["filename"].replace("\\", "/").rsplit("/", 1)[-1]


@dataclass(frozen=True)
class Source:
    unit: str
    sha256: str
    text: str
    normalized: bool

    @cached_property
    def offsets(self):
        # Compiler offsets count UTF-16 code units. Keep the original UTF-8 offsets even
        # when IntelliJ normalizes CRLF and CR to LF before the Kotlin compiler reads them.
        result = array("i", [0])
        for index, point in enumerate(self.text):
            width = len(point.encode("utf-8"))
            if self.normalized:
                if point == "\r" and self.text[index + 1:index + 2] == "\n":
                    continue
                if point == "\n" and index > 0 and self.text[index - 1] == "\r":
                    width += 1
            if ord(point) > 0xFFFF:
                result.append(-1)
            previous = result[-2] if result[-1] == -1 else result[-1]
            result.append(previous + width)
        return result

    def span(self, start, end):
        if not 0 <= start < end < len(self.offsets):
            raise ValueError("compiler returned an invalid source range")
        first, last = self.offsets[start], self.offsets[end]
        if first < 0 or last < 0:
            raise ValueError("compiler returned a split UTF-16 character")
        return {"start": first, "end": last}

    def anchor(self, start, end):
        return {"unit": self.unit, "source_sha256": self.sha256, "range": self.span(start, end)}


def batch(coverage, observations=()):
    return {"coverage": coverage, "observations": list(observations)}


def decode(output, sources, limit, partial_input):
    status, *lines = output.decode("ascii").splitlines()
    coverage = Coverage(status)
    if len(lines) > limit:
        raise ValueError("compiler exceeded its observation bound")
    selected = sources[0]
    observations = []
    for index, line in enumerate(lines):
        start, end, owner_start, owner_end, source, target_start, target_end = map(int, line.split("\t"))
        if not -1 <= source < len(sources):
            raise ValueError("compiler returned an invalid target source")
        target = None if source == -1 else sources[source].anchor(target_start, target_end)
        observations.append({
            "id": str(index),
            "occurrence": selected.span(start, end),
            "enclosing": None if owner_start == -1 else selected.span(owner_start, owner_end),
            "target": target,
            "evidence": "unresolved" if target is None else "resolution",
        })
    return batch(Coverage.PARTIAL if partial_input else coverage, observations)


def collect(options, request, read_source):
    deadline = time.monotonic() + COMPILER_SECONDS_MAX
    selected = request["source"]
    try:
        language = Language(Path(filename(selected)).suffix)
    except ValueError:
        return batch(Coverage.UNSUPPORTED)
    if language == Language.KOTLIN and options.kotlin_home is None:
        return batch(Coverage.UNSUPPORTED)
    category = Category(request["category"])
    limit = request["observations_max"]
    if not 0 <= limit <= OBSERVATIONS_MAX:
        raise ValueError("invalid observation bound")

    with tempfile.TemporaryDirectory(prefix="rift-jvm-") as directory:
        root = Path(directory).resolve()
        module = root / "module"
        paths, sources = [], []
        compiler_classpath = os.pathsep.join(str(path) for path in options.classpath)

        def admit(source):
            is_kotlin = language == Language.KOTLIN
            patched = source["unit"] in options.java_module_sources and not is_kotlin
            folder = (module if patched else root / "source") / str(len(paths))
            folder.mkdir(parents=True)
            name = filename(source)
            if ":" in name or any(ord(character) < 32 for character in name):
                raise ValueError("invalid compiler source filename")
            path = folder / name
            text = source["text"]
            compiler_text = text.replace("\r\n", "\n").replace("\r", "\n") if is_kotlin else text
            path.write_bytes(compiler_text.encode("utf-8"))
            paths.append(str(path))
            sources.append(Source(source["unit"], source["sha256"], text, is_kotlin))

        admit(selected)
        support_bytes = 0
        partial_input = False
        units = dict.fromkeys([*options.source_unit, *sorted(options.java_module_sources)])
        for unit in units:
            if unit == selected["unit"]:
                continue
            source = read_source(unit)
            if source is None:
                partial_input = True
                continue
            suffix = Path(filename(source)).suffix
            if suffix != Language.JAVA and (language != Language.KOTLIN or suffix != Language.KOTLIN):
                continue
            support_bytes += len(source["text"].encode("utf-8"))
            if support_bytes > SOURCE_SUPPORT_BYTES_MAX:
                partial_input = True
                break
            admit(source)

        if language == Language.JAVA:
            classpath = str(options.engine)
            engine = "JavacFacts"
            compiler_options = [f"{options.java_module}={module}" if module.is_dir() else ""]
        else:
            classpath = os.pathsep.join((str(options.engine), str(options.kotlin_home / "lib" / "*")))
            engine = "KotlinFacts"
            compiler_options = [
                str(options.kotlin_home), str(root / "classes"), str(options.kotlin_stdlib).lower(),
            ]

        source_list = root / "sources"
        source_list.write_text("\n".join(paths) + "\n", encoding="utf-8")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return batch(Coverage.PARTIAL)
        command = [
            options.java, "-Xmx768m", f"-Djava.io.tmpdir={root}", "-cp", classpath, engine,
            category.name, str(limit), compiler_classpath, *compiler_options, str(source_list),
        ]
        try:
            process = subprocess.run(
                command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, check=True,
                timeout=remaining,
            )
        except subprocess.TimeoutExpired:
            return batch(Coverage.PARTIAL)
        return decode(process.stdout, sources, limit, partial_input)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--java", default="java")
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--kotlin-home", type=Path)
    parser.add_argument("--kotlin-stdlib", action="store_true")
    parser.add_argument("--classpath", type=Path, action="append", default=[])
    parser.add_argument("--source-unit", action="append", default=[])
    parser.add_argument("--java-module")
    parser.add_argument("--java-module-source", action="append", default=[])
    options = parser.parse_args()
    options.java_module_sources = frozenset(options.java_module_source)
    if options.kotlin_stdlib and options.kotlin_home is None:
        parser.error("--kotlin-stdlib requires --kotlin-home")
    options.engine = options.engine.resolve(strict=True)
    if options.kotlin_home:
        options.kotlin_home = options.kotlin_home.resolve(strict=True)
    options.classpath = [path.resolve(strict=True) for path in options.classpath]
    if bool(options.java_module_sources) != bool(options.java_module):
        parser.error("--java-module and --java-module-source require each other")
    if options.java_module and not all(part.isidentifier() for part in options.java_module.split(".")):
        parser.error("invalid Java module name")
    unit_count = len(set(options.source_unit) | options.java_module_sources)
    if unit_count > SOURCE_SUPPORT_COUNT_MAX:
        parser.error("supporting source count exceeds its bound")
    serve(partial(collect, options))


if __name__ == "__main__":
    main()
