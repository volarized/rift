"""Build SPARQL requests and inspect projected terms in independent client gates."""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from urllib.parse import quote

from rift_dev.rift_test_client import (
    Json,
    JsonObject,
    array_value,
    object_value,
    string_value,
    tail_text,
)

_INPUT_FIELDS = frozenset(
    {
        "view",
        "query",
        "pattern",
        "scope",
        "target",
        "order",
        "packages",
        "paths",
        "include",
        "limit",
        "page_index",
        "rev",
        "revision",
        "change",
    }
)
_OUTPUT_FIELDS = frozenset(
    {
        "hit",
        "symbol",
        "target",
        "size",
        "name",
        "kind",
        "language",
        "path",
        "unit",
        "start",
        "end",
        "line",
        "source",
        "score",
        "index",
    }
)


def quoted(value: str) -> str:
    """Render a SPARQL string with JSON's compatible string escaping."""
    return json.dumps(value, ensure_ascii=False, allow_nan=False)


def name_request(name: str) -> JsonObject:
    """Select canonical identities by an exact authored name."""
    return {"query": f"SELECT ?symbol {{ ?symbol a :Symbol; :name {quoted(name)} }}"}


def declaration_request(name: str) -> JsonObject:
    """Select a named symbol and its item node from the captured graph."""
    value = quoted(name)
    return {
        "query": f"""SELECT ?symbol ?name ?language ?unit ?source ?start ?end {{
          ?symbol a :Symbol; :name {value}; :name ?name; :language ?language .
          ?node :of ?symbol; :index 0; :source ?source;
                :range [ :unit ?unit; :start ?start; :end ?end ] .
        }}"""
    }


def project_source_unit(path: str) -> str:
    """Encode a project path with the existing protocol source-unit alphabet.

    >>> project_source_unit("packages/@scope/name a%#?.rs")
    'rift://source/project/packages/@scope/name%20a%25%23%3F.rs'
    >>> project_source_unit("café~.json")
    'rift://source/project/caf%C3%A9~.json'
    """
    return "rift://source/project/" + quote(path, safe="/!$&'()*+,;=:@")


def retrieval_request(
    parameters: Mapping[str, Json],
    columns: Sequence[str] = ("symbol", "name", "path", "unit", "start", "end"),
) -> JsonObject:
    """Invoke existing retrieval through standard SERVICE syntax and explicit projection."""
    unknown = set(parameters) - _INPUT_FIELDS
    if unknown:
        raise ValueError(f"unknown retrieval inputs: {sorted(unknown)}")
    unknown = set(columns) - _OUTPUT_FIELDS
    if unknown:
        raise ValueError(f"unknown query columns: {sorted(unknown)}")
    clauses = [
        f"?hit :{'rev' if key == 'revision' else key} {_input(value)} ."
        for key, value in parameters.items()
        if value is not None
    ]
    clauses.append("?hit :index ?index .")
    symbol_columns = {"name", "kind", "language"}.intersection(columns)
    if "symbol" in columns or symbol_columns:
        fields = " ".join(
            f"OPTIONAL {{ ?symbol :{column} ?{column} }}"
            for column in sorted(symbol_columns)
        )
        clauses.append(f"OPTIONAL {{ ?hit :symbol ?symbol . {fields} }}")
    target_columns = {"target", "size"}.intersection(columns)
    if target_columns:
        fields = " ".join(
            f"OPTIONAL {{ ?target_record :{column} ?{column} }}"
            for column in sorted(target_columns)
        )
        clauses.append(f"OPTIONAL {{ ?hit :hit ?target_record . {fields} }}")
    for column in columns:
        if column not in {
            "hit",
            "symbol",
            "name",
            "kind",
            "language",
            "index",
            "target",
            "size",
        }:
            clauses.append(f"OPTIONAL {{ ?hit :{column} ?{column} }}")
    projection = " ".join(f"?{column}" for column in columns)
    body = " ".join(clauses)
    return {
        "query": f"SELECT {projection} {{ SERVICE :search {{ {body} }} }} ORDER BY ?index"
    }


def usage_request(symbol: str) -> JsonObject:
    """Return every physical usage with source-unit identity and UTF-8 range."""
    _identity(symbol)
    return {
        "query": f"SELECT ?usage ?unit ?start ?end {{ <{symbol}> :usages ?usage . ?usage :range [ :unit ?unit; :start ?start; :end ?end ] }} ORDER BY ?unit ?start ?end"
    }


def incoming_references_request(symbol: str) -> JsonObject:
    """Select distinct enclosing symbols that refer to a canonical target."""
    _identity(symbol)
    return {
        "query": f"SELECT DISTINCT ?caller ?name {{ ?caller :references <{symbol}>; :name ?name }} ORDER BY ?name"
    }


def bindings(answer: JsonObject) -> list[JsonObject]:
    """Require the actual SELECT result branch before inspecting its rows."""
    if not isinstance(answer.get("answer"), dict):
        raise _bindings_error(answer)
    result = object_value(answer.get("answer"), "query answer")
    if result.get("result") != "bindings":
        raise _bindings_error(answer)
    return [
        object_value(row, "query binding")
        for row in array_value(result.get("bindings"), "query bindings")
    ]


def _bindings_error(answer: JsonObject) -> AssertionError:
    response = tail_text(json.dumps(answer, ensure_ascii=False, allow_nan=False))
    return AssertionError(
        f"expected SELECT bindings: outcome={answer.get('outcome')!r}, "
        f"reason={answer.get('reason')!r}; response={response}"
    )


def iri(row: JsonObject, field: str) -> str:
    """Require a projected IRI without discarding its RDF term kind."""
    value = object_value(row.get(field), field)
    if value.get("type") != "iri":
        raise AssertionError(f"{field}: expected an IRI: {value!r}")
    return string_value(value.get("value"), field)


def optional_iri(row: JsonObject, field: str) -> str | None:
    """Keep unestablished records distinct from canonical identities."""
    value = row.get(field)
    if value is None or object_value(value, field).get("type") == "blank_node":
        return None
    return iri(row, field)


def literal(row: JsonObject, field: str) -> str:
    """Read a projected literal while its datatype remains in the validated answer."""
    value = object_value(row.get(field), field)
    if value.get("type") not in {"literal", "language_literal"}:
        raise AssertionError(f"{field}: expected a literal: {value!r}")
    result = value.get("value")
    if not isinstance(result, str):
        raise TypeError(f"{field}: expected a literal lexical value")
    return result


def _input(value: Json) -> str:
    if isinstance(value, str):
        return quoted(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    return quoted(json.dumps(value, ensure_ascii=False, allow_nan=False)) + "^^rdf:JSON"


def _identity(value: str, prefix: str = "rift://symbol/") -> None:
    if not value.startswith(prefix) or any(
        character in value for character in '<>"{}|^`\\\n\r\t '
    ):
        raise ValueError(
            "expected a canonical symbol IRI returned by Rift"
            if prefix == "rift://symbol/"
            else "expected an IRI returned by Rift"
        )
