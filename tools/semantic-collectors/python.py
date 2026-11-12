"""Collect authored Python calls without importing target modules."""

import ast
import importlib.util
import json
import re
import sys
from typing import BinaryIO

FRAME_BYTES_MAX = 8 << 20
HEADER_BYTES_MAX = 4 << 10
OWNERS = (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)


def collect(request, _read_source):
    """Collect calls from the admitted source. Custom collectors use the same arguments."""
    if request["category"] != "calls":
        return {"coverage": "unsupported", "observations": []}
    source = request["source"]
    text = source["text"]
    try:
        tree = ast.parse(text, filename=source["filename"])
    except (SyntaxError, ValueError, RecursionError):
        return {"coverage": "partial", "observations": []}

    starts = [0, *(match.end() for match in re.finditer(rb"\r\n|\r|\n", text.encode()))]

    def span(node):
        return {
            "start": starts[node.lineno - 1] + node.col_offset,
            "end": starts[node.end_lineno - 1] + node.end_col_offset,
        }

    observations = []

    def batch(coverage):
        observations.sort(key=lambda item: (item["occurrence"]["start"], item["occurrence"]["end"]))
        for index, observation in enumerate(observations):
            observation["id"] = str(index)
        return {"coverage": coverage, "observations": observations}

    pending = [(tree, None)]
    while pending:
        node, owner = pending.pop()
        if isinstance(node, ast.Call):
            if len(observations) == request["observations_max"]:
                return batch("partial")
            observations.append({
                "occurrence": span(node),
                "enclosing": owner,
                "target": None,
                "evidence": "syntax",
            })
        outer = owner
        if isinstance(node, OWNERS):
            owner = span(node)
        children = []
        for field, value in ast.iter_fields(node):
            child_owner = outer if field == "decorator_list" else owner
            if isinstance(value, ast.AST):
                children.append((value, child_owner))
            elif isinstance(value, list):
                children.extend((child, child_owner) for child in value if isinstance(child, ast.AST))
        pending.extend(reversed(children))
    return batch("complete")


def read_message(stream: BinaryIO):
    header = stream.readline(HEADER_BYTES_MAX)
    if not header:
        return None
    match = re.fullmatch(rb"Content-Length: ([0-9]{1,8})\r\n", header, re.IGNORECASE)
    if match is None or stream.readline(3) != b"\r\n":
        raise ValueError("invalid Content-Length header")
    length = int(match[1])
    if not 0 < length <= FRAME_BYTES_MAX:
        raise ValueError("invalid frame size")
    body = stream.read(length)
    if len(body) != length:
        raise EOFError("incomplete frame")
    return json.loads(body)


def send_message(stream: BinaryIO, message):
    body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode()
    if len(body) > FRAME_BYTES_MAX:
        raise ValueError("response exceeds frame bound")
    stream.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
    stream.write(body)
    stream.flush()


def serve(extract=collect, reader=None, writer=None):
    """Serve the same callable over stdio. A collector may request supporting source."""
    reader = sys.stdin.buffer if reader is None else reader
    writer = sys.stdout.buffer if writer is None else writer
    support_id = 0

    def read_source(unit):
        nonlocal support_id
        support_id += 1
        request_id = f"source-{support_id}"
        send_message(writer, {
            "jsonrpc": "2.0", "id": request_id, "method": "facts/request",
            "params": {"unit": unit},
        })
        response = read_message(reader)
        if response is None or response.get("jsonrpc") != "2.0" or response.get("id") != request_id:
            raise ValueError("invalid supporting source response")
        return response["result"]

    while (request := read_message(reader)) is not None:
        if request.get("jsonrpc") != "2.0" or request.get("method") != "extractor/extract":
            raise ValueError("invalid extraction request")
        params = request["params"]
        if params.get("version") != 1:
            raise ValueError("unsupported extraction contract version")
        batch = extract(params, read_source)
        send_message(writer, {"jsonrpc": "2.0", "id": request["id"], "result": batch})


def main():
    if len(sys.argv) == 1:
        serve()
        return
    if len(sys.argv) != 2:
        raise SystemExit("usage: python.py [COLLECTOR_MODULE]")
    spec = importlib.util.spec_from_file_location("collector", sys.argv[1])
    if spec is None or spec.loader is None:
        raise ValueError("collector module must be a Python file")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    serve(module.collect)


if __name__ == "__main__":
    main()
