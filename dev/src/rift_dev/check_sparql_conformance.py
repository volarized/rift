"""Compare selected upstream queries through the public tool and RDFLib."""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import io
import json
from collections.abc import Generator
from contextlib import contextmanager
from importlib.metadata import version
from pathlib import Path

import rdflib
from rdflib import RDF, BNode, Graph, URIRef, Variable
from rdflib.compare import isomorphic
from rdflib.plugins.sparql import prepareQuery
from rdflib.plugins.sparql.parserutils import CompValue
from rdflib.plugins.sparql.processor import SPARQLProcessor
from rdflib.query import Result

from rift_dev.check_artifact import ARTIFACT_SECONDS, lay_out_workspace, symbol_hit
from rift_dev.commands import REPOSITORY
from rift_dev.nextest_run import retained_collector
from rift_dev.rift_test_client import (
    JsonObject,
    Server,
    ToolFailure,
    gate_deadline,
    object_value,
    require,
    retain_collector,
    verify_version,
)

FIXTURES = REPOSITORY / "dev" / "fixtures" / "sparql-query"
MF = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#"
QT = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#"
XSD_STRING = "http://www.w3.org/2001/XMLSchema#string"


@contextmanager
def original_literal_forms() -> Generator[None]:
    """Keep RDFLib's documented literal normalization disabled during comparisons."""
    previous = rdflib.NORMALIZE_LITERALS
    vars(rdflib)["NORMALIZE_LITERALS"] = False
    try:
        yield
    finally:
        vars(rdflib)["NORMALIZE_LITERALS"] = previous


def literal_setting_controls() -> None:
    previous = rdflib.NORMALIZE_LITERALS
    with original_literal_forms():
        require(
            rdflib.NORMALIZE_LITERALS is False, "literal normalization remains enabled"
        )
    require(rdflib.NORMALIZE_LITERALS == previous, "literal setting was not restored")
    try:
        with original_literal_forms():
            raise LookupError("literal setting restoration control")
    except LookupError:
        pass
    require(
        rdflib.NORMALIZE_LITERALS == previous,
        "failed comparison changed literal setting",
    )


def digest(path: Path) -> str:
    """Hash an existing artifact without copying it."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def json_term(term: dict) -> dict:
    """Encode one Rift term as a standard SPARQL JSON result term."""
    kind = term["type"]
    encoded = {"value": term["value"]}
    if kind in {"iri", "blank_node"}:
        encoded["type"] = "uri" if kind == "iri" else "bnode"
    elif kind == "literal":
        encoded["type"] = "literal"
        # SPARQL JSON omits xsd:string, as sparesults::write_json_term does.
        if term["datatype"] != XSD_STRING:
            encoded["datatype"] = term["datatype"]
    elif kind == "language_literal":
        encoded.update({"type": "literal", "xml:lang": term["language"]})
    else:
        raise AssertionError(f"unknown RDF term: {term}")
    return encoded


def parsed_json(document: dict) -> Result:
    return Result.parse(io.BytesIO(json.dumps(document).encode()), format="json")


def query_result(answer: dict) -> Result:
    """Translate the public result shape without changing term identities or rows."""
    kind = answer["result"]
    if kind == "boolean":
        return parsed_json({"head": {}, "boolean": answer["value"]})
    if kind == "bindings":
        return parsed_json(
            {
                "head": {"vars": answer["variables"]},
                "results": {
                    "bindings": [
                        {key: json_term(term) for key, term in row.items()}
                        for row in answer["bindings"]
                    ]
                },
            }
        )
    require(kind == "graph", f"unknown result form: {kind}")
    graph = Graph(store="Memory")
    for triple in answer["triples"]:
        row = parsed_json(
            {
                "head": {"vars": ["subject", "predicate", "object"]},
                "results": {
                    "bindings": [{key: json_term(term) for key, term in triple.items()}]
                },
            }
        ).bindings[0]
        graph.add(
            (
                row[Variable("subject")],
                row[Variable("predicate")],
                row[Variable("object")],
            )
        )
    result = Result("CONSTRUCT")
    result.graph = graph
    return result


def equal_results(actual: Result, expected: Result) -> bool:
    """Use RDFLib's ordered row equality or its graph isomorphism helper."""
    if actual.type in {"CONSTRUCT", "DESCRIBE"}:
        return (
            expected.type in {"CONSTRUCT", "DESCRIBE"}
            and actual.graph is not None
            and expected.graph is not None
            and isomorphic(actual.graph, expected.graph)
        )
    return actual == expected


def require_data_independent(value: object) -> None:
    """Reject selected queries that could read the served workspace or a remote graph."""
    if isinstance(value, CompValue):
        require(
            value.name not in {"Graph", "ServiceGraphPattern"},
            f"case reads a graph or SERVICE: {value.name}",
        )
        if value.name == "BGP":
            require(not value["triples"], "case reads workspace triples")
        require(not dict.get(value, "datasetClause"), "case changes its dataset")
        for child in value.values():
            require_data_independent(child)
    elif isinstance(value, (list, tuple)):
        for child in value:
            require_data_independent(child)


def saved_json_result(path: Path) -> Result:
    with path.open("rb") as source:
        return Result.parse(source, format="json")


def selected_cases(selection: dict) -> list[tuple[dict, str, Result | None, Graph]]:
    """Validate pinned files and actual upstream manifest links before evaluation."""
    for asset in selection["assets"]:
        path = FIXTURES / asset["path"]
        require(digest(path) == asset["sha256"], f"changed pinned asset: {path}")
    cases = []
    for case in selection["cases"]:
        manifest = Graph(store="Memory").parse(
            FIXTURES / case["manifest"], format="turtle"
        )
        subject = URIRef(case["case"])
        require(
            (subject, RDF.type, URIRef(MF + "QueryEvaluationTest")) in manifest,
            case["id"],
        )
        action = manifest.value(subject, URIRef(MF + "action"), any=False)
        query_path = FIXTURES / case["query"]
        result_path = FIXTURES / case["result"]
        require(
            manifest.value(action, URIRef(QT + "query"), any=False)
            == URIRef(query_path.as_uri()),
            case["id"],
        )
        require(
            manifest.value(subject, URIRef(MF + "result"), any=False)
            == URIRef(result_path.as_uri()),
            case["id"],
        )
        data = manifest.value(action, URIRef(QT + "data"), any=False)
        expected_data = (
            None if case["data"] is None else URIRef((FIXTURES / case["data"]).as_uri())
        )
        require(data == expected_data, case["id"])
        graph = Graph(store="Memory")
        if case["data"] is not None:
            graph.parse(FIXTURES / case["data"])
        with result_path.open("rb") as source:
            expected = Result.parse(source, format="xml")
        cases.append((case, query_path.read_bytes().decode("utf-8"), expected, graph))
    for case in selection["supplemental"]:
        cases.append(
            (
                case,
                (FIXTURES / case["query"]).read_bytes().decode("utf-8"),
                (
                    saved_json_result(FIXTURES / case["result"])
                    if "result" in case
                    else None
                ),
                Graph(store="Memory"),
            )
        )
    require(len({case[0]["id"] for case in cases}) == len(cases), "duplicate case")
    return cases


def reference_differences(selection: dict, cases: list) -> dict[str, dict]:
    """Bind documented reference behavior to its exact version and query bytes."""
    selected = {case["id"]: statement for case, statement, _, _ in cases}
    differences = {}
    for difference in selection["reference_differences"]:
        identity = difference["id"]
        require(identity in selected and identity not in differences, identity)
        require(
            difference["oracle_version"] == selection["oracle"]["version"], identity
        )
        require(
            difference["oracle_revision"] == selection["oracle"]["revision"], identity
        )
        require(
            hashlib.sha256(selected[identity].encode()).hexdigest()
            == difference["query_sha256"],
            f"reference difference query changed: {identity}",
        )
        differences[identity] = difference
    return differences


def reference_expectation(
    case: dict,
    statement: str,
    expected: Result | None,
    graph: Graph,
    differences: dict[str, dict],
    record: dict,
) -> tuple[Result | None, bool]:
    """Keep reference failures separate so every candidate call still runs."""
    candidate = expected
    try:
        reference = reference_result(statement, graph)
        record["reference"] = serialized(reference)
        if expected is not None:
            record["expected"] = serialized(expected)
            if "manifest" in case and reference.type == "SELECT":
                require(
                    len(reference.bindings) <= 1,
                    "upstream subset requires at most one row",
                )
                require(
                    not any(
                        isinstance(term, BNode)
                        for row in reference.bindings
                        for term in row.values()
                    ),
                    "upstream subset excludes blank-node bindings",
                )
        else:
            candidate = reference
        difference = differences.get(case["id"])
        if difference is None:
            if expected is not None:
                require(
                    equal_results(reference, expected),
                    f"RDFLib differs from expected: {case['id']}",
                )
            record["reference_outcome"] = "agreement"
        else:
            if expected is None:
                raise AssertionError(
                    f"reference difference has no expected result: {case['id']}"
                )
            observed = saved_json_result(FIXTURES / difference["observed_result"])
            require(
                equal_results(reference, observed),
                f"pinned reference behavior changed: {case['id']}",
            )
            require(
                not equal_results(reference, expected),
                f"reference now matches expected: {case['id']}",
            )
            record["reference_outcome"] = "documented_difference"
            record["reference_difference"] = difference
    except Exception as error:  # noqa: BLE001 - preserve reference failure and run every candidate call.
        record["reference_outcome"] = "fail"
        record["reference_error"] = repr(error)
        return candidate, False
    return candidate, True


def reference_result(statement: str, graph: Graph) -> Result:
    prepared = prepareQuery(statement)
    require_data_independent(prepared.algebra)
    return graph.query(
        prepared, processor=SPARQLProcessor(graph), use_store_provided=False
    )


def serialized(result: Result) -> str:
    value = result.serialize(
        format="turtle" if result.type in {"CONSTRUCT", "DESCRIBE"} else "json"
    )
    return value.decode() if isinstance(value, bytes) else str(value)


def comparison_controls() -> None:
    """Prove the comparison retains duplicate rows, row order and empty bindings."""
    document = {
        "head": {"vars": ["x"]},
        "results": {
            "bindings": [
                {"x": {"type": "uri", "value": "urn:one"}},
                {},
                {"x": {"type": "uri", "value": "urn:one"}},
            ]
        },
    }
    original = parsed_json(document)
    require(len(original.bindings) == 3, "comparison dropped an empty or duplicate row")
    reversed_rows = dict(
        document,
        results={
            "bindings": [
                document["results"]["bindings"][1],
                *document["results"]["bindings"][::2],
            ]
        },
    )
    require(original != parsed_json(reversed_rows), "comparison ignores row order")
    fewer = dict(document, results={"bindings": document["results"]["bindings"][:2]})
    require(original != parsed_json(fewer), "comparison ignores duplicate multiplicity")

    require(
        rdflib.NORMALIZE_LITERALS is False,
        "literal comparison would rewrite lexical forms",
    )

    def integer_result(lexical: str) -> Result:
        return parsed_json(
            {
                "head": {"vars": ["value"]},
                "results": {
                    "bindings": [
                        {
                            "value": {
                                "type": "literal",
                                "value": lexical,
                                "datatype": "http://www.w3.org/2001/XMLSchema#integer",
                            }
                        }
                    ]
                },
            }
        )

    lexical = integer_result("07")
    require(
        str(lexical.bindings[0][Variable("value")]) == "07",
        "JSON parser rewrote integer lexical form",
    )
    require(
        lexical != integer_result("7"),
        "comparison hides different integer lexical forms",
    )
    reference = reference_result(
        'SELECT ("07"^^<http://www.w3.org/2001/XMLSchema#integer> AS ?value) WHERE {}',
        Graph(store="Memory"),
    )
    require(
        str(reference.bindings[0][Variable("value")]) == "07",
        "reference processor rewrote selected literal",
    )
    require(
        equal_results(lexical, reference),
        "reference and JSON parser differ on original literal",
    )
    language = {"type": "language_literal", "value": "bonjour", "language": "fr"}
    integer = {
        "type": "literal",
        "value": "7",
        "datatype": "http://www.w3.org/2001/XMLSchema#integer",
    }
    require(
        json_term(language)
        == {"type": "literal", "value": "bonjour", "xml:lang": "fr"},
        "language tag changed",
    )
    require(json_term(integer)["datatype"] == integer["datatype"], "datatype changed")
    require(
        json_term({"type": "literal", "value": "text", "datatype": XSD_STRING})
        == {"type": "literal", "value": "text"},
        "simple string encoding changed",
    )
    for statement in (
        "SELECT * WHERE { ?s ?p ?o }",
        "SELECT * WHERE { GRAPH <urn:graph> { ?s ?p ?o } }",
        "SELECT * FROM <urn:graph> WHERE {}",
        "SELECT * WHERE { SERVICE <urn:service> {} }",
    ):
        try:
            require_data_independent(prepareQuery(statement).algebra)
        except AssertionError:
            continue
        raise AssertionError(f"scope check allowed data acquisition: {statement}")
    graph_answer = {
        "result": "graph",
        "triples": [
            {
                "subject": {"type": "blank_node", "value": name},
                "predicate": {"type": "iri", "value": "urn:value"},
                "object": integer,
            }
            for name in ("first", "second")
        ],
    }
    actual_graph = query_result(graph_answer)
    require(
        actual_graph.graph is not None and len(actual_graph.graph) == 2,
        "distinct blank nodes collapsed",
    )
    expected_graph = Result("CONSTRUCT")
    expected_graph.graph = Graph(store="Memory").parse(
        data="_:a <urn:value> 7 . _:b <urn:value> 7 .", format="turtle"
    )
    require(equal_results(actual_graph, expected_graph), "isomorphic graphs differ")
    collapsed = Result("CONSTRUCT")
    collapsed.graph = Graph(store="Memory").parse(
        data="_:a <urn:value> 7 .", format="turtle"
    )
    require(
        not equal_results(actual_graph, collapsed),
        "graph comparison collapsed blank nodes",
    )


async def check_cases(binary: Path, release: str, output: Path) -> None:
    """Exercise a supplied executable through the existing SDK transport."""
    require(
        version("rdflib") == "7.6.0", "RDFLib version differs from the pinned oracle"
    )
    selection = json.loads((FIXTURES / "selection.json").read_text())
    comparison_controls()
    cases = selected_cases(selection)
    differences = reference_differences(selection, cases)
    output.mkdir(parents=True, exist_ok=False)
    report = {
        "binary": str(binary.resolve()),
        "sha256": digest(binary),
        "version": release,
        "selection": selection,
        "cases": [],
    }
    report_path = output / "report.json"
    async with gate_deadline("SPARQL query conformance subset", ARTIFACT_SECONDS):
        verify_version(binary, release)
        root = output / "workspace"
        root.mkdir()
        lay_out_workspace(root)
        with (
            retained_collector(output) as telemetry,
            Server(binary, root, output / "server.log", collector=telemetry) as server,
        ):
            try:
                async with server.connect() as client:
                    await symbol_hit(client, "beacon_one")
                    for case, statement, expected, graph in cases:
                        record = {"id": case["id"], "query": statement, "calls": []}
                        report["cases"].append(record)
                        try:
                            candidate, reference_valid = reference_expectation(
                                case, statement, expected, graph, differences, record
                            )
                            for explicit in (False, True):
                                request: JsonObject = {"query": statement}
                                if explicit:
                                    request["language"] = "sparql"
                                call = {"request": request}
                                record["calls"].append(call)
                                try:
                                    response = await client.call("search", request)
                                    call["response"] = response
                                    answer = object_value(
                                        response.get("answer"),
                                        f"query answer: {response}",
                                    )
                                    pagination = object_value(
                                        response.get("pagination"), "query pagination"
                                    )
                                    total_pages = pagination.get("total_pages")
                                    require(
                                        pagination["page_index"] == 0
                                        and isinstance(total_pages, int)
                                        and total_pages <= 1
                                        and pagination.get("next_query") is None,
                                        f"case is not a complete first page: {response}",
                                    )
                                    if candidate is None:
                                        raise AssertionError(
                                            f"no expected result available: {case['id']}"
                                        )
                                    require(
                                        equal_results(query_result(answer), candidate),
                                        f"Rift differs from expected result: {case['id']}",
                                    )
                                    call["outcome"] = "pass"
                                except (AssertionError, ToolFailure) as error:
                                    call["outcome"] = "fail"
                                    call["error"] = repr(error)
                            record["outcome"] = (
                                "pass"
                                if reference_valid
                                and len(record["calls"]) == 2
                                and all(
                                    call["outcome"] == "pass"
                                    for call in record["calls"]
                                )
                                else "fail"
                            )
                        except (AssertionError, ToolFailure) as error:
                            record["outcome"] = "fail"
                            record["error"] = repr(error)
                        finally:
                            record.setdefault("outcome", "interrupted")
                            report_path.write_text(json.dumps(report, indent=2) + "\n")
                server.stop()
                report["reference_agreements"] = sum(
                    case.get("reference_outcome") == "agreement"
                    and case.get("outcome") == "pass"
                    for case in report["cases"]
                )
                report["reference_differences"] = sum(
                    case.get("reference_outcome") == "documented_difference"
                    for case in report["cases"]
                )
                require(
                    sum(len(case["calls"]) for case in report["cases"])
                    == 2 * len(cases),
                    "not every selected candidate call ran",
                )
                require(
                    len(report["cases"]) == len(cases)
                    and all(case["outcome"] == "pass" for case in report["cases"]),
                    "selected query cases failed; see report.json",
                )
            except BaseException as error:
                report["error"] = repr(error)
                for note in server.evidence():
                    error.add_note(note)
                raise
            finally:
                retain_collector(output, telemetry)
                report_path.write_text(json.dumps(report, indent=2) + "\n")


async def check(binary: Path, release: str, output: Path) -> None:
    literal_setting_controls()
    with original_literal_forms():
        await check_cases(binary, release, output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(check(arguments.binary, arguments.version, arguments.output.resolve()))


if __name__ == "__main__":
    main()
