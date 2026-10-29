# Selected W3C SPARQL 1.1 query evaluation cases

These files are original W3C query evaluation cases from [w3c/rdf-tests](https://github.com/w3c/rdf-tests) at commit `d5d930b03ee6b63b388d0bb28fdb29ee78a59ec0`, the revision the pinned Oxigraph repository carries as its `testsuite/rdf-tests` submodule. Every file keeps its original bytes. Each group folder keeps its whole `manifest.ttl` for review; only the cases `selection.tsv` lists run. The subset checks the features the graph query engine admits during development; it makes no W3C conformance claim.

`selection.tsv` holds one case per line: group, manifest entry, query, data, and expected result, tab separated, with `-` for a case without data. The unit test `sparql::conformance_tests` in `rift-search` loads each case's Turtle data, admits its query through `GraphQuery::parse`, evaluates it, and compares the answer with the expected result as a multiset of solutions under RDF term equality.

The selection holds every `mf:QueryEvaluationTest` of the groups `aggregates`, `bind`, `bindings`, `exists`, `functions`, `grouping`, `negation`, `project-expression` and `property-path` that:

- reads at most one Turtle file into the default graph, with no named graph;
- expects a `.srx` or `.srj` result with no blank node;
- is neither withdrawn nor unclassified.

The selection leaves out the cases whose expected results spell a numeric value in a non-canonical lexical form, such as `"1.0"^^xsd:decimal` for the evaluator's `"1"^^xsd:decimal`, since term equality compares lexical forms: `aggregates/agg-avg-02`, `aggregates/agg-avg-distinct`, `aggregates/agg-err-02`, `aggregates/agg-min-01`, `aggregates/agg-min-02`, `aggregates/agg-min-distinct`, `aggregates/agg-sum-distinct` and `functions/coalesce01`.

The upstream repository uses W3C's dual test-suite licenses, as `LICENSE.md` states. These checks use its 3-clause BSD option, whose terms `w3c-bsd-license.html` retains.
