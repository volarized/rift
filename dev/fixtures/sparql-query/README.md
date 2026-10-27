# Selected SPARQL query checks

This directory contains twelve original W3C query cases and five separate Rift cases. It is a development check, not the full W3C suite or a certification claim.

`selection.json` records the upstream commit, exact case IRIs, query/result/data paths and SHA-256 hashes. The W3C files retain their original bytes. The complete two group manifests and their parent manifest remain available for review. Only the listed cases run; the runner does not follow other manifest entries.

The selected upstream queries do not read triples, named graphs or remote services. They cover empty strings, IRI/base handling, expression errors, language tags, BIND, UUID result properties, VALUES, subqueries and GROUP_CONCAT. Their expected results contain at most one SELECT row and no selected blank nodes. Original manifest data is loaded by the reference engine, even when the query does not read it. The runner checks the parsed algebra before evaluation and rejects data reads, GRAPH, SERVICE and dataset clauses. It does not inject arbitrary RDF into Rift.

The Rift cases separately check ordered LIMIT/OFFSET, grouped duplicate rows, OPTIONAL/unbound values, typed and language literals, and CONSTRUCT blank nodes. These cases are not upstream W3C tests.

The runner uses RDFLib 7.6.0 with its Python SPARQLProcessor, a Memory store and `use_store_provided=False`. It uses the supplied Rift executable through the existing validating MCP SDK client. Each query runs with omitted language and explicit `language="sparql"`, without modifying its text. A typed unavailable answer fails the case. Every result must fit one complete transport page.

Rift terms are encoded as SPARQL Results JSON for RDFLib's public parser. This encoding preserves rows, unbound variables, datatypes, language tags and blank-node labels. It omits the xsd:string datatype as the pinned sparesults serializer does. RDFLib's SELECT equality compares variable lists and binding lists directly, preserving order and duplicate multiplicity. Graph results use RDFLib's graph isomorphism helper. RDFLib's documented `NORMALIZE_LITERALS` flag is false before fixture parsing, result parsing and reference evaluation. The previous value is restored on success and failure. Controls distinguish `"07"^^xsd:integer` from `"7"^^xsd:integer` and require the original `07` lexical form. No result normalizer or blank-node label replacement is used.

`selection.json` records two RDFLib 7.6.0 reference differences, each bound to its exact query hash and recorded reference result. Empty CONCAT keeps the unchanged official empty-string expectation. The supplemental VALUES/OPTIONAL case keeps three ordered rows, including both unmatched duplicate rows, as required by [SPARQL LeftJoin and Diff](https://www.w3.org/TR/sparql11-query/#defn_LeftJoin). Every candidate call must match its official or supplemental expected result. The remaining cases retain independent reference comparison. All seventeen queries run in both language forms. A successful check records fifteen independent agreements and two documented reference differences. Unexpected reference behavior fails the check.

The runner keeps original queries, requests, structured responses, expected/reference results and failures in `report.json`. Existing server logs, telemetry and collector counts remain beside the report. Existing process owners and cleanup bounds apply. The output directory must be new; the runner does not clear old reports or copy/build the executable.

After the test dependency group has a generated lock entry, run:

```sh
uv run --project dev --group conformance --locked python -m rift_dev.check_sparql_conformance \
  --binary /absolute/path/to/rift --version <candidate-version> \
  --output /absolute/path/to/new-report-directory
```

The separate domain/model controls compare optimized acquisition with a complete captured domain and reconstruct typed models from graph facts. Incremental/cold rebuild equality, provider acquisition, retained SERVICE, live engines and corpus performance remain separate acceptance checks.

The upstream repository uses W3C's dual test-suite licenses. These development checks use its 3-clause BSD option. `w3c/LICENSE.md` retains the original repository notice; `w3c-bsd-license.html` retains W3C's license terms. RDFLib is a test-only dependency under BSD-3-Clause.
