"""Drive read tools and resources through a validating MCP client."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import cast

from dulwich.repo import Repo

from rift_dev.check_artifact import (
    CONFIGURATION,
    check_external_change,
    check_reads,
    incoming_references,
    lay_out_workspace,
    symbol_hit,
    symbol_id,
)
from rift_dev.log_records import parse_line
from rift_dev.nextest_run import retained_collector
from rift_dev.query import bindings, declaration_request, iri, literal, quoted
from rift_dev.rift_test_client import (
    Client,
    Server,
    ToolFailure,
    array_value,
    collector_line,
    gate_deadline,
    object_value,
    require,
    retain_collector,
    retained_directory,
    string_value,
    verify_version,
)
from rift_dev.trace import TEST_CASE_KEY, Collector, resource_attribute

AGENT_SECONDS = 300.0
READ_TOOLS = {"search", "get_symbol", "nodes"}
RESOURCE_URIS = {"rift://map", "rift://workspace"}


PYTHON_SOURCE = "def serve(port: int) -> int:\n    return port\n\n\ndef caller() -> int:\n    return serve(8080)\n"
PYTHON_METADATA = """[build-system]
requires=['setuptools==80.9.0']
build-backend='setuptools.build_meta'
[tool.setuptools]
py-modules=['service']
"""
PYTHON_CONFIGURATION = """
[languages.python.lsp]
embedded = "ty"
startup_timeout = "2m"
request_timeout = "2m"
retry = { attempts = 12, delay = "250ms", delay_limit = "2s" }
"""


async def check_embedded_references(client: Client, root: Path) -> None:
    """Require Python references with original RDF endpoints and declaration spans."""
    target_name = quoted("serve")
    caller_name = quoted("caller")
    language = quoted("python")
    path = quoted("service.py")
    target_lookup = string_value(declaration_request("serve").get("query"), "query")
    caller_lookup = string_value(declaration_request("caller").get("query"), "query")
    answer = await client.call(
        "search",
        {
            "query": f"SELECT ?caller ?target ?derivation ?facet ?caller_source ?caller_start ?caller_end ?caller_unit ?target_source ?target_start ?target_end ?target_unit ?target_count ?caller_count {{ {{ SELECT (COUNT(*) AS ?target_count) {{ {{ {target_lookup} }} FILTER(?language = {language}) }} }} {{ SELECT (COUNT(*) AS ?caller_count) {{ {{ {caller_lookup} }} FILTER(?language = {language}) }} }} ?target a :Symbol; :name {target_name}; :language {language}; :declarations ?target_declaration . ?target_declaration :path {path}; :source ?target_source; :unit ?target_unit; :range ?target_range . ?target_range :start ?target_start; :end ?target_end . ?caller a :Symbol; :name {caller_name}; :language {language}; :declarations ?caller_declaration; :references ?target . ?caller_declaration :path {path}; :source ?caller_source; :unit ?caller_unit; :range ?caller_range . ?caller_range :start ?caller_start; :end ?caller_end . ?relationship a :Relationship; :from ?caller; :to ?target; :derivation ?derivation; :facets :references . BIND(:references AS ?facet) }} ORDER BY ?caller"
        },
    )
    resolved = bindings(answer)
    require(len(resolved) == 1, f"engine references omitted caller: {answer}")
    relationship = resolved[0]
    require(
        literal(relationship, "target_count") == "1"
        and literal(relationship, "caller_count") == "1",
        f"expected one serve and one caller declaration: {answer}",
    )
    endpoints = {
        field: object_value(relationship.get(field), field)
        for field in ("caller", "target")
    }
    for field, endpoint in endpoints.items():
        require(
            endpoint.get("type") in {"iri", "blank_node"}
            and bool(string_value(endpoint.get("value"), field)),
            f"reference endpoint is not an RDF resource: {relationship}",
        )
    require(
        endpoints["caller"] != endpoints["target"],
        f"reference endpoints are the same: {relationship}",
    )
    require(
        iri(relationship, "derivation") == "urn:rift:vocabulary:resolution",
        f"reference was not resolved by the engine: {relationship}",
    )
    require(
        iri(relationship, "facet") == "urn:rift:vocabulary:references",
        f"reference facets differ: {relationship}",
    )
    original = PYTHON_SOURCE.encode()
    target_source, caller_source = PYTHON_SOURCE.removesuffix("\n").split("\n\n\n")
    for field, text in (("caller", caller_source), ("target", target_source)):
        expected = text.encode()
        start = original.index(expected)
        require(
            literal(relationship, f"{field}_source") == text
            and literal(relationship, f"{field}_start") == str(start)
            and literal(relationship, f"{field}_end") == str(start + len(expected))
            and iri(relationship, f"{field}_unit")
            == "rift://source/project/service.py",
            f"reference declaration changed its original source or range: {relationship}",
        )
    require(
        (root / "service.py").read_bytes() == original,
        "reference read changed source bytes",
    )


async def check_mixed_references_unavailable(client: Client) -> None:
    """Require complete relationship acquisition to refuse an unserved language."""
    seed = symbol_id(await symbol_hit(client, "serve", "python"))
    try:
        answer = await incoming_references(client, seed)
    except ToolFailure as failure:
        require(
            failure.code == "capability_unavailable"
            and failure.retry == "operator_action",
            f"mixed relationship acquisition returned a different refusal: {failure}",
        )
    else:
        raise AssertionError(
            f"unserved Rust relationships returned a complete answer: {answer}"
        )


async def check_python_references(
    binary: Path, base: Path, telemetry: Collector
) -> None:
    """Read relationship metadata from a complete served Python workspace."""
    root = base / "python"
    root.mkdir()
    with Repo.init(root):
        pass
    (root / "rift.toml").write_text(
        CONFIGURATION + "[source]\ninclude=['service.py']\n" + PYTHON_CONFIGURATION,
        encoding="utf-8",
        newline="",
    )
    (root / "service.py").write_text(PYTHON_SOURCE, encoding="utf-8", newline="")
    (root / "pyproject.toml").write_text(PYTHON_METADATA, encoding="utf-8", newline="")
    with Server(
        binary,
        root,
        base / "python-server.log",
        env={"OTEL_RESOURCE_ATTRIBUTES": resource_attribute(TEST_CASE_KEY, "agent")},
        collector=telemetry,
    ) as server:
        try:
            async with server.connect() as client:
                await check_embedded_references(client, root)
        except BaseException as error:
            for note in server.evidence():
                error.add_note(note)
            raise


async def check_resources(client: Client) -> None:
    """Read every advertised resource and assert its useful payload."""
    async with asyncio.timeout(client.call_seconds):
        listed = await client.session.list_resources()
    require(
        listed.next_cursor is None, "resource listing requires a new pagination case"
    )
    observed = {str(resource.uri) for resource in listed.resources}
    require(observed == RESOURCE_URIS, f"resource coverage differs: {observed}")
    workspace = await client.resource("rift://workspace")
    require(
        bool(array_value(workspace.get("languages"), "workspace.languages")),
        "workspace has no languages",
    )
    source = array_value(workspace.get("source"), "workspace.source")
    require(bool(source), "workspace has no source")
    object_value(workspace.get("pagination"), "workspace.pagination")
    string_value(
        workspace.get("configuration_revision"), "workspace.configuration_revision"
    )
    orientation = await client.resource("rift://map")
    require(
        bool(array_value(orientation.get("languages"), "map.languages")),
        "map has no languages",
    )
    string_value(orientation.get("revision"), "map.revision")


async def declaration_node(client: Client, name: str) -> str:
    """Prove nodes retains the captured declaration and original source."""
    search = await client.call(
        "search",
        declaration_request(name),
    )
    matches = [
        row
        for row in bindings(search)
        if literal(row, "name") == name and literal(row, "language") == "rust"
    ]
    require(len(matches) == 1, f"expected one {name} declaration: {search}")
    hit = matches[0]
    captured = object_value(search.get("view"), "search.view")
    identity = symbol_id(hit)
    answer = await client.call(
        "get_symbol",
        {
            "id": identity,
            "include": ["source"],
            "view": string_value(captured.get("id"), "view.id"),
        },
    )
    require(answer.get("outcome") == "found", f"symbol was not found: {answer}")
    require(
        object_value(answer.get("symbol"), "symbol").get("id") == identity,
        f"symbol identity changed: {answer}",
    )
    declarations = array_value(answer.get("declarations"), "symbol declarations")
    require(len(declarations) == 1, f"expected one declaration: {answer}")
    declaration = object_value(declarations[0], "declaration")
    for field in ("path", "source"):
        require(
            declaration.get(field) == literal(hit, field),
            f"symbol declaration changed {field}: {answer}",
        )
    require(
        declaration.get("range")
        == {"start": int(literal(hit, "start")), "end": int(literal(hit, "end"))},
        f"symbol declaration changed range: {answer}",
    )
    span = object_value(declaration.get("range"), "declaration.range")
    start, end = span.get("start"), span.get("end")
    require(
        type(start) is int and type(end) is int and 0 <= start < end,
        f"declaration range is invalid: {span}",
    )
    view = object_value(answer.get("view"), "symbol.view")
    require(view == captured, f"symbol changed captured view: {answer}")
    listing = await client.call(
        "nodes",
        {
            "path": string_value(declaration.get("path"), "declaration.path"),
            "position": cast(int, end) - 1,
            "view": string_value(view.get("id"), "view.id"),
        },
    )
    require(listing.get("view") == view, f"nodes changed captured view: {listing}")
    nodes = array_value(listing.get("nodes"), "nodes.nodes")
    sources = array_value(listing.get("source"), "nodes.source")
    require(len(nodes) == len(sources), f"nodes lost source excerpts: {listing}")
    matches = [
        (object_value(entry, "node"), excerpt)
        for entry, excerpt in zip(nodes, sources, strict=True)
        if object_value(entry, "node").get("symbol") == identity
        and object_value(entry, "node").get("range") == span
    ]
    require(len(matches) == 1, f"nodes omitted the declaration: {listing}")
    node, excerpt = matches[0]
    require(excerpt == declaration.get("source"), f"nodes changed source: {listing}")
    return string_value(node.get("id"), "node.id")


async def check_agent(binary: Path, version: str | None = None) -> None:
    """Exercise all tools through a real proxy over one disposable workspace."""
    async with gate_deadline("agent", AGENT_SECONDS):
        if version is not None:
            verify_version(binary, version)
        base = retained_directory("agent")
        root = base / "workspace"
        root.mkdir()
        lay_out_workspace(root)
        configuration = root / "rift.toml"
        configuration.write_text(
            configuration.read_text() + PYTHON_CONFIGURATION,
            encoding="utf-8",
            newline="",
        )
        (root / "service.py").write_text(PYTHON_SOURCE, encoding="utf-8", newline="")
        (root / "pyproject.toml").write_text(
            PYTHON_METADATA, encoding="utf-8", newline=""
        )
        with (
            retained_collector(base) as telemetry,
            Server(
                binary,
                root,
                base / "server.log",
                # The server's export is filed under the runner as its `test.case.name`.
                env={
                    "OTEL_RESOURCE_ATTRIBUTES": resource_attribute(
                        TEST_CASE_KEY, "agent"
                    )
                },
                collector=telemetry,
            ) as server,
        ):
            try:
                async with server.connect() as client:
                    await check_reads(client)
                    await check_resources(client)
                    await check_external_change(client, root)
                    await check_mixed_references_unavailable(client)
                    await declaration_node(client, "beacon_one")
                    client.require_complete(READ_TOOLS)
                server.stop()
                logs = await asyncio.to_thread(server.read_records)
                require(
                    any(parse_line(line) is not None for line in logs.splitlines()),
                    "startup recorded no logs",
                )
                await check_python_references(binary, base, telemetry)
            except BaseException as error:
                for note in server.evidence():
                    error.add_note(note)
                error.add_note(collector_line(telemetry))
                raise
            finally:
                retain_collector(base, telemetry)
            print(collector_line(telemetry), flush=True)
            print(collector_line(telemetry), flush=True)
