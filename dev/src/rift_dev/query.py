"""Build SPARQL requests and inspect projected terms in independent client gates."""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence

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
    """Read original declaration facts without requiring retrieval ranking."""
    value = quoted(name)
    return {
        "query": f"SELECT ?symbol ?name ?kind ?language ?path ?source ?start ?end {{ ?symbol a :Symbol; :name {value}; :kind ?kind; :language ?language; :declarations ?declaration . ?declaration :path ?path; :source ?source; :range ?range . ?range :start ?start; :end ?end . BIND({value} AS ?name) }}"
    }


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
    # Existing page default: rift_core::constants::SEARCH_RESULTS_DEFAULT.
    limit = parameters.get("limit", 20)
    page = parameters.get("page_index", 0)
    if type(limit) is not int or type(page) is not int or limit < 0 or page < 0:
        raise ValueError("query window must contain unsigned integers")
    clauses = [
        f"?hit :{key} {_input(value)} ."
        for key, value in parameters.items()
        if value is not None and key not in {"limit", "page_index"}
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
        "query": f"SELECT {projection} {{ SERVICE :search {{ {body} }} }} ORDER BY ?index LIMIT {limit} OFFSET {page * limit}"
    }


def usage_request(symbol: str) -> JsonObject:
    """Return every physical usage with source-unit identity and UTF-8 range."""
    _identity(symbol)
    return {
        "query": f"SELECT ?usage ?unit ?start ?end {{ <{symbol}> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end }} ORDER BY ?unit ?start ?end"
    }


def incoming_references_request(symbol: str) -> JsonObject:
    """Read reference endpoints and original relationship evidence."""
    _identity(symbol)
    return {
        "query": f"SELECT ?caller ?target ?derivation ?facet {{ ?caller :references <{symbol}> . ?relationship a :Relationship; :from ?caller; :to <{symbol}>; :derivation ?derivation; :facets :references . BIND(<{symbol}> AS ?target) BIND(:references AS ?facet) }} ORDER BY ?caller"
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


def _identity(value: str) -> None:
    if not value.startswith("rift://symbol/") or any(
        character in value for character in '<>"{}|^`\\\n\r\t '
    ):
        raise ValueError("expected a canonical symbol IRI returned by Rift")
