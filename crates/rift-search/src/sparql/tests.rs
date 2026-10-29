use std::time::Duration;

use rift_error::{ErrorSlug, RiftError, errors};
use rift_protocol::read::{Language, NodeId, SourceUnitId, SymbolId, TextRange};

use super::{
    GraphAnswer, GraphDataset, GraphDatasetBuilder, GraphFact, GraphLimits, GraphQuery, GraphTerm,
    HistoryEvent, NodeFact, NodeLocation, QueryCancellation, RevisionIri, SymbolRelation,
};

const MAIN: &str = "rift://symbol/local/rust/app/main";
const PARSE: &str = "rift://symbol/local/rust/app/parser/parse";
const LOAD: &str = "rift://symbol/local/rust/app/config/load";
const NODE: &str =
    "rift://node/rust/05f68d7add6106deac3d04d31bbafb9b809631f459a06c3521a69808e9e54e28";
const FILE_NODE: &str =
    "rift://node/rust/1111111111111111111111111111111111111111111111111111111111111111";
const UNIT: &str = "rift://source/project/src/main.rs";
const COMMIT: &str = "rift://revision/git/0123456789abcdef0123456789abcdef01234567";
const RELEASE: &str = "rift://revision/cargo/crates.io/demo@1.0.0";
const TREE: &str =
    "rift://revision/tree/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

fn symbol(id: &str) -> SymbolId {
    SymbolId(id.to_owned())
}

fn rust() -> Language {
    Language {
        name: "rust".to_owned(),
        dialect: None,
    }
}

fn revision(iri: &str) -> RevisionIri {
    RevisionIri::parse(iri).expect("revision IRI is valid")
}

/// A dataset holding every fact kind once.
fn model(limits: GraphLimits) -> Result<GraphDataset, RiftError> {
    let (main, parse, load) = (symbol(MAIN), symbol(PARSE), symbol(LOAD));
    let (node, file_node) = (NodeId(NODE.to_owned()), NodeId(FILE_NODE.to_owned()));
    let unit = SourceUnitId(UNIT.to_owned());
    let (commit, release, tree) = (revision(COMMIT), revision(RELEASE), revision(TREE));
    let language = rust();
    let (first, second) = (
        TextRange { start: 4, end: 30 },
        TextRange { start: 40, end: 66 },
    );
    let declaration = |range| NodeFact {
        node: &node,
        kind: "function_item",
        language: &language,
        of: Some(&main),
        parent: Some(&file_node),
        location: Some(NodeLocation { unit: &unit, range }),
        source: Some("fn main() { parse(); }"),
    };
    let facts = [
        GraphFact::Symbol {
            symbol: &main,
            kind: "function",
            language: &language,
        },
        GraphFact::Node(declaration(&first)),
        GraphFact::Node(declaration(&second)),
        GraphFact::Relation {
            source: &main,
            relation: SymbolRelation::Calls,
            target: &parse,
        },
        GraphFact::Relation {
            source: &parse,
            relation: SymbolRelation::References,
            target: &load,
        },
        GraphFact::Relation {
            source: &load,
            relation: SymbolRelation::RenamedFrom,
            target: &parse,
        },
        GraphFact::Usage {
            symbol: &parse,
            node: &node,
        },
        GraphFact::History {
            symbol: &main,
            event: HistoryEvent::Introduced,
            revision: &commit,
        },
        GraphFact::History {
            symbol: &parse,
            event: HistoryEvent::Changed,
            revision: &commit,
        },
        GraphFact::History {
            symbol: &load,
            event: HistoryEvent::Removed,
            revision: &commit,
        },
        GraphFact::CoChange {
            symbol: &main,
            other: &parse,
            commit_count: 3,
        },
        GraphFact::Uses {
            revision: &tree,
            dependency: &release,
        },
    ];
    let mut graph = GraphDataset::builder(limits);
    for fact in facts {
        graph.insert(fact)?;
    }
    Ok(graph.build())
}

fn answer(dataset: &GraphDataset, text: &str) -> Result<GraphAnswer, RiftError> {
    let query = GraphQuery::parse(text, dataset.limits())?;
    dataset.evaluate(&query, &QueryCancellation::new())
}

/// The rows of a `SELECT`, each value rendered as its IRI or lexical form.
fn rows(dataset: &GraphDataset, text: &str) -> Vec<Vec<String>> {
    let GraphAnswer::Solutions(table) = answer(dataset, text).expect("query answers") else {
        panic!("a SELECT must answer solutions: {text}");
    };
    let render = |term: Option<GraphTerm>| match term {
        Some(GraphTerm::Iri(text) | GraphTerm::BlankNode(text)) => text,
        Some(GraphTerm::Literal { value, .. }) => value,
        None => "unbound".to_owned(),
    };
    let mut rendered: Vec<Vec<String>> = table
        .rows
        .into_iter()
        .map(|row| row.into_iter().map(render).collect())
        .collect();
    rendered.sort();
    rendered
}

fn refusal(result: Result<impl std::fmt::Debug, RiftError>) -> ErrorSlug {
    result.expect_err("the call is refused").slug()
}

#[test]
fn test_graph_dataset_answers_symbol_node_and_edge_vocabulary() {
    let dataset = model(GraphLimits::default()).expect("model builds");
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?callee { ?s a :Symbol ; :kind \"function\" ; :language \"rust\" ; :calls ?callee }"
        ),
        vec![vec![PARSE.to_owned()]]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?target { <rift://symbol/local/rust/app/main> (:calls/:references) ?target }"
        ),
        vec![vec![LOAD.to_owned()]]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?symbol ?kind ?parent ?source { ?n a :Node ; :of ?symbol ; :kind ?kind ; :parent ?parent ; :source ?source }"
        ),
        vec![vec![
            MAIN.to_owned(),
            "function_item".to_owned(),
            FILE_NODE.to_owned(),
            "fn main() { parse(); }".to_owned()
        ]]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?unit ?start ?end { ?n :range ?r . ?r :unit ?unit ; :start ?start ; :end ?end . ?unit a :SourceUnit }"
        ),
        vec![
            vec![UNIT.to_owned(), "4".to_owned(), "30".to_owned()],
            vec![UNIT.to_owned(), "40".to_owned(), "66".to_owned()],
        ]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?node { ?s :usages ?node . ?node a :Node }"
        ),
        vec![vec![NODE.to_owned()]]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?from { <rift://symbol/local/rust/app/config/load> :renamedFrom ?from }"
        ),
        vec![vec![PARSE.to_owned()]]
    );
}

#[test]
fn test_graph_dataset_answers_history_and_revision_vocabulary() {
    let dataset = model(GraphLimits::default()).expect("model builds");
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?event ?s { ?s ?event ?r . ?r a :Commit, :Revision . VALUES ?event { :introducedIn :changedIn :removedIn } }"
        ),
        vec![
            vec!["urn:rift:vocabulary:changedIn".to_owned(), PARSE.to_owned()],
            vec![
                "urn:rift:vocabulary:introducedIn".to_owned(),
                MAIN.to_owned()
            ],
            vec!["urn:rift:vocabulary:removedIn".to_owned(), LOAD.to_owned()],
        ]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?other ?count { ?a :coChanges ?other . ?st rdf:subject ?a ; rdf:predicate :coChanges ; rdf:object ?other ; :commitCount ?count }"
        ),
        vec![vec![PARSE.to_owned(), "3".to_owned()]]
    );
    assert_eq!(
        rows(
            &dataset,
            "SELECT ?tree ?release { ?tree :uses ?release . ?release a :Revision FILTER NOT EXISTS { ?release a :Commit } }"
        ),
        vec![vec![TREE.to_owned(), RELEASE.to_owned()]]
    );
    assert_eq!(
        answer(&dataset, "ASK { ?s :removedIn ?r }").ok(),
        Some(GraphAnswer::Boolean(true))
    );
    assert_eq!(
        answer(&dataset, "ASK { ?s :calls ?s }").ok(),
        Some(GraphAnswer::Boolean(false))
    );
}

#[test]
fn test_graph_dataset_repeated_fact_costs_no_triples() {
    let dataset = model(GraphLimits::default()).expect("model builds");
    let count = dataset.triple_count();
    let limits = GraphLimits::builder()
        .triples_max(count)
        .build()
        .expect("nonzero");
    let exact = model(limits).expect("a model of exactly the bound builds");
    assert_eq!(
        exact.triple_count(),
        count,
        "repeated nodes and types add no triple"
    );
}

#[test]
fn test_graph_dataset_refuses_fact_past_triple_bound_without_partial_insert() {
    let limits = GraphLimits::builder()
        .triples_max(4)
        .build()
        .expect("nonzero");
    let (main, parse) = (symbol(MAIN), symbol(PARSE));
    let mut graph: GraphDatasetBuilder = GraphDataset::builder(limits);
    let language = rust();
    graph
        .insert(GraphFact::Symbol {
            symbol: &main,
            kind: "function",
            language: &language,
        })
        .expect("three triples fit");
    let relation = GraphFact::Relation {
        source: &main,
        relation: SymbolRelation::Calls,
        target: &parse,
    };
    let error = graph
        .insert(relation)
        .expect_err("two new triples pass four");
    assert_eq!(error.slug(), errors::search::graph_dataset_limit::SLUG);
    assert!(error.to_string().contains("needs 5 triples"), "{error}");
    assert_eq!(
        graph.build().triple_count(),
        3,
        "a refused fact inserts nothing"
    );
}

#[test]
fn test_graph_dataset_refuses_fact_past_literal_byte_bound() {
    let limits = GraphLimits::builder()
        .literal_bytes_max(8)
        .build()
        .expect("nonzero");
    let main = symbol(MAIN);
    let language = rust();
    let mut graph = GraphDataset::builder(limits);
    let fact = GraphFact::Symbol {
        symbol: &main,
        kind: "function",
        language: &language,
    };
    let error = graph
        .insert(fact)
        .expect_err("twelve literal bytes pass eight");
    assert_eq!(error.slug(), errors::search::graph_dataset_limit::SLUG);
    assert!(error.to_string().contains("literal bytes"), "{error}");
}

#[test]
fn test_graph_dataset_refuses_identity_outside_its_form() {
    let language = rust();
    let mut graph = GraphDataset::builder(GraphLimits::default());
    let cases = [
        SymbolId("rift://node/rust/x".to_owned()),
        SymbolId("rift://symbol/".to_owned()),
        SymbolId("not an iri".to_owned()),
    ];
    for case in &cases {
        let fact = GraphFact::Symbol {
            symbol: case,
            kind: "function",
            language: &language,
        };
        assert_eq!(
            refusal(graph.insert(fact)),
            errors::search::graph_identity_invalid::SLUG,
            "{case:?}"
        );
    }
    assert_eq!(
        refusal(RevisionIri::parse(UNIT)),
        errors::search::graph_identity_invalid::SLUG
    );
    assert!(revision(COMMIT).is_commit() && !revision(RELEASE).is_commit());
    assert_eq!(revision(TREE).as_str(), TREE);
}

#[test]
fn test_graph_dataset_evaluate_refuses_answer_past_solution_and_byte_bounds() {
    let narrow = GraphLimits::builder()
        .solutions_max(2)
        .build()
        .expect("nonzero");
    let dataset = model(narrow).expect("model builds");
    let three = "SELECT ?s { ?s a :Symbol }";
    let error = answer(&dataset, three).expect_err("three symbols pass two solutions");
    assert_eq!(error.slug(), errors::search::query_answer_limit::SLUG);
    assert!(error.to_string().contains("solutions"), "{error}");
    let limited = "SELECT ?s { ?s a :Symbol } LIMIT 2";
    assert_eq!(rows(&dataset, limited).len(), 2, "two solutions fit");

    let narrow = GraphDataset::builder(
        GraphLimits::builder()
            .answer_bytes_max(16)
            .build()
            .expect("nonzero"),
    )
    .build();
    let error = answer(&narrow, "SELECT ?x { BIND(\"0123456789abcdef\" AS ?x) }")
        .expect_err("bytes pass sixteen");
    assert!(error.to_string().contains("answer bytes"), "{error}");
}

#[test]
fn test_graph_dataset_evaluate_refuses_past_deadline_and_on_cancellation() {
    let expired = GraphLimits::builder()
        .evaluation_timeout(Duration::ZERO)
        .build()
        .expect("nonzero");
    let dataset = model(expired).expect("model builds");
    let reads = "SELECT ?s { ?s ?p ?o }";
    assert_eq!(
        refusal(answer(&dataset, reads)),
        errors::search::query_deadline_exceeded::SLUG
    );
    let no_reads = "SELECT ?x { BIND(1 AS ?x) }";
    assert_eq!(
        refusal(answer(&dataset, no_reads)),
        errors::search::query_deadline_exceeded::SLUG
    );

    let dataset = model(GraphLimits::default()).expect("model builds");
    let query = GraphQuery::parse(reads, dataset.limits()).expect("query parses");
    let cancellation = QueryCancellation::new();
    cancellation.cancel();
    assert!(cancellation.is_cancelled(), "{cancellation:?}");
    assert_eq!(
        refusal(dataset.evaluate(&query, &cancellation)),
        errors::search::query_cancelled::SLUG
    );
}

#[test]
fn test_graph_dataset_evaluate_reports_evaluation_failure() {
    let dataset = model(GraphLimits::default()).expect("model builds");
    let unknown = "SELECT ?x { BIND(<urn:rift:unknown>(1) AS ?x) }";
    assert_eq!(
        refusal(answer(&dataset, unknown)),
        errors::search::query_evaluation_failed::SLUG
    );
}

/// A dataset of `callers` symbols that all call one target.
fn fan_in(callers: usize, limits: GraphLimits) -> GraphDataset {
    let target = symbol(PARSE);
    let mut graph = GraphDataset::builder(limits);
    for caller in 0..callers {
        let source = symbol(&format!("{MAIN}{caller}"));
        let fact = GraphFact::Relation {
            source: &source,
            relation: SymbolRelation::Calls,
            target: &target,
        };
        graph.insert(fact).expect("the fan-in fits");
    }
    graph.build()
}

#[test]
fn test_graph_dataset_evaluate_timer_stops_join_between_reads() {
    let timeout = Duration::from_millis(50);
    let limits = GraphLimits::builder()
        .evaluation_timeout(timeout)
        .build()
        .expect("nonzero");
    let dataset = fan_in(60, limits);
    let joined =
        "SELECT (COUNT(*) AS ?n) { ?a :calls ?t . ?b :calls ?t . ?c :calls ?t . ?d :calls ?t }";
    let started = std::time::Instant::now();
    let error = answer(&dataset, joined).expect_err("13 million joined rows outlast 50 ms");
    let elapsed = started.elapsed();
    assert_eq!(
        error.slug(),
        errors::search::query_deadline_exceeded::SLUG,
        "{error}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the timer cancels the join: elapsed={elapsed:?}"
    );
}

#[test]
fn test_graph_dataset_evaluate_refuses_string_past_expression_bound() {
    let limits = GraphLimits::builder()
        .expression_bytes_max(64)
        .build()
        .expect("nonzero");
    let dataset = GraphDataset::builder(limits).build();
    let doubling = "SELECT ?d { BIND(\"0123456789abcdef\" AS ?a) BIND(CONCAT(?a, ?a) AS ?b) BIND(CONCAT(?b, ?b) AS ?c) BIND(CONCAT(?c, ?c) AS ?d) }";
    let replace = "SELECT ?x { BIND(REPLACE(\"aaaa\", \"a\", \"bbbbbbbbbbbbbbbbbbbb\") AS ?x) }";
    let ucase = "SELECT ?x { BIND(UCASE(\"aaaaaaaaaaaaaaaaaaaaaaa\") AS ?x) }";
    let grouped = "SELECT (GROUP_CONCAT(?v) AS ?x) { VALUES ?v { \"0123456789\" \"0123456789\" \"0123456789\" \"0123456789\" \"0123456789\" \"0123456789\" } }";
    for text in [doubling, replace, ucase, grouped] {
        let error = answer(&dataset, text).expect_err("the computed string passes 64 bytes");
        assert_eq!(
            error.slug(),
            errors::search::query_answer_limit::SLUG,
            "{text}"
        );
        assert!(error.to_string().contains("expression bytes"), "{error}");
    }
    let fits = "SELECT ?x ?y { BIND(CONCAT(\"ab\", \"cd\") AS ?x) { SELECT (GROUP_CONCAT(DISTINCT ?v; SEPARATOR=\"|\") AS ?y) { VALUES ?v { \"\" \"b\" \"\" } } } }";
    assert_eq!(
        rows(&dataset, fits),
        vec![vec!["abcd".to_owned(), "|b".to_owned()]]
    );
}

#[test]
fn test_graph_query_parse_refuses_nesting_past_bound_and_survives_it() {
    let limits = GraphLimits::default();
    let nested = |depth: usize| {
        let (open, close) = ("(".repeat(depth - 2), ")".repeat(depth - 2));
        format!("SELECT ?x {{ BIND({open}1{close} AS ?x) }}")
    };
    let deepest = nested(limits.nesting_depth_max());
    let dataset = GraphDataset::builder(limits).build();
    assert_eq!(rows(&dataset, &deepest), vec![vec!["1".to_owned()]]);
    let error = GraphQuery::parse(&nested(limits.nesting_depth_max() + 1), &limits)
        .expect_err("one level too deep");
    assert_eq!(error.slug(), errors::search::query_nesting_limit::SLUG);
    let shallow = GraphLimits::builder()
        .nesting_depth_max(2)
        .build()
        .expect("nonzero");
    let quoted =
        "ASK { FILTER(\"((\" != '''[[''' && <urn:a(b)> != <urn:c> && 1 < 2 && 2 > 1) } # (((";
    assert!(
        GraphQuery::parse(quoted, &shallow).is_ok(),
        "brackets in literals, IRIs and comments count nothing"
    );
}

#[test]
fn test_graph_query_parse_survives_nesting_and_operators_at_their_bounds() {
    let limits = GraphLimits::default();
    let open = "(".repeat(limits.nesting_depth_max() - 2);
    let close = ")".repeat(limits.nesting_depth_max() - 2);
    let chain = "+1".repeat(limits.operators_max());
    let text = format!("SELECT ?x {{ BIND({open}1{chain}{close} AS ?x) }}");
    let dataset = GraphDataset::builder(limits).build();
    let expected = (limits.operators_max() + 1).to_string();
    assert_eq!(rows(&dataset, &text), vec![vec![expected]]);
    let past = format!("SELECT ?x {{ BIND(1{chain}+1 AS ?x) }}");
    let error = GraphQuery::parse(&past, &limits).expect_err("one operator too many");
    assert_eq!(error.slug(), errors::search::query_operator_limit::SLUG);
}

#[test]
fn test_graph_query_parse_refuses_inline_rows_joining_past_solution_bound() {
    let limits = GraphLimits::builder()
        .solutions_max(100)
        .build()
        .expect("nonzero");
    let block = |name: &str| {
        format!(
            "VALUES (?{name} ?t) {{ {} }}",
            (0..11)
                .map(|row| format!("({row} 0)"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let fits = format!("SELECT * {{ {} }}", block("a"));
    assert!(
        GraphQuery::parse(&fits, &limits).is_ok(),
        "eleven inline rows fit"
    );
    let joined = format!("SELECT * {{ {} {} }}", block("a"), block("b"));
    let error = GraphQuery::parse(&joined, &limits).expect_err("121 joined inline rows pass 100");
    assert_eq!(error.slug(), errors::search::query_answer_limit::SLUG);
    assert!(error.to_string().contains("joined VALUES rows"), "{error}");
}
