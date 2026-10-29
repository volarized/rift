//! A selected subset of the W3C SPARQL 1.1 query evaluation tests.
//!
//! `tests/fixtures/sparql-w3c/selection.tsv` lists one case per line: group, manifest entry,
//! query, data, and expected result, all relative to the group's folder. Each case loads its
//! Turtle data into a dataset, admits its query through [`GraphQuery::parse`], evaluates it,
//! and compares the answer with the W3C result as a multiset of solutions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxrdf::Triple;
use oxttl::TurtleParser;
use sparesults::{QueryResultsFormat, QueryResultsParser, SliceQueryResultsParserOutput};

use super::dataset::GraphDataset;
use super::evaluation::{GraphAnswer, GraphTerm, QueryCancellation};
use super::limits::GraphLimits;
use super::query::GraphQuery;

/// Base IRI the W3C suite publishes each group's files under.
const SUITE_BASE: &str = "https://w3c.github.io/rdf-tests/sparql/sparql11";

/// One selected case.
struct Case {
    group: String,
    entry: String,
    query: PathBuf,
    data: Option<PathBuf>,
    result: PathBuf,
}

/// One solution as a variable-to-term map, comparable regardless of variable order.
type Solution = Vec<(String, GraphTerm)>;

/// An answer in the shape both sides are compared in.
#[derive(Debug, PartialEq)]
enum Expected {
    Solutions(Vec<Solution>),
    Boolean(bool),
}

fn fixture_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    Path::new(&manifest).join("tests/fixtures/sparql-w3c")
}

fn cases() -> Vec<Case> {
    let root = fixture_root();
    let selection =
        std::fs::read_to_string(root.join("selection.tsv")).expect("selection.tsv is readable");
    selection
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [group, entry, query, data, result] = fields[..] else {
                panic!("selection line must hold five tab-separated fields: {line}");
            };
            let folder = root.join(group);
            Case {
                group: group.to_owned(),
                entry: entry.to_owned(),
                query: folder.join(query),
                data: (data != "-").then(|| folder.join(data)),
                result: folder.join(result),
            }
        })
        .collect()
}

fn data_triples(case: &Case) -> Vec<Triple> {
    let Some(path) = &case.data else {
        return Vec::new();
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let base = format!("{SUITE_BASE}/{}/{name}", case.group);
    let bytes = std::fs::read(path).expect("case data is readable");
    TurtleParser::new()
        .with_base_iri(&base)
        .expect("suite base is an IRI")
        .for_slice(&bytes)
        .collect::<Result<_, _>>()
        .unwrap_or_else(|error| panic!("case {} data must parse: {error}", case.entry))
}

fn expected(case: &Case) -> Expected {
    let format = match case
        .result
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("srj") => QueryResultsFormat::Json,
        _ => QueryResultsFormat::Xml,
    };
    let bytes = std::fs::read(&case.result).expect("case result is readable");
    let parsed = QueryResultsParser::from_format(format)
        .for_slice(&bytes)
        .unwrap_or_else(|error| panic!("case {} result must parse: {error}", case.entry));
    match parsed {
        SliceQueryResultsParserOutput::Boolean(value) => Expected::Boolean(value),
        SliceQueryResultsParserOutput::Solutions(solutions) => Expected::Solutions(sorted(
            solutions
                .map(|solution| {
                    let solution = solution.expect("result solution parses");
                    solution
                        .iter()
                        .map(|(variable, term)| {
                            (variable.as_str().to_owned(), GraphTerm::from(term))
                        })
                        .collect()
                })
                .collect(),
        )),
    }
}

fn answered(answer: GraphAnswer) -> Expected {
    match answer {
        GraphAnswer::Boolean(value) => Expected::Boolean(value),
        GraphAnswer::Solutions(table) => Expected::Solutions(sorted(
            table
                .rows
                .into_iter()
                .map(|row| {
                    table
                        .variables
                        .iter()
                        .zip(row)
                        .filter_map(|(variable, term)| term.map(|term| (variable.clone(), term)))
                        .collect()
                })
                .collect(),
        )),
    }
}

/// Orders each solution by variable and the solutions themselves, keeping duplicates.
fn sorted(mut solutions: Vec<Solution>) -> Vec<Solution> {
    for solution in &mut solutions {
        solution.sort_by(|left, right| left.0.cmp(&right.0));
    }
    solutions.sort_by_cached_key(|solution| format!("{solution:?}"));
    solutions
}

fn evaluate(case: &Case) -> Result<Expected, String> {
    let limits = GraphLimits::default();
    let text = std::fs::read_to_string(&case.query).map_err(|error| error.to_string())?;
    let query = GraphQuery::parse(&text, &limits).map_err(|error| error.to_string())?;
    let dataset = GraphDataset::from_triples(data_triples(case), limits);
    let answer = dataset
        .evaluate(&query, &QueryCancellation::new())
        .map_err(|error| error.to_string())?;
    Ok(answered(answer))
}

#[test]
fn test_sparql_selected_w3c_query_evaluation_cases_match_expected_results() {
    let cases = cases();
    assert!(!cases.is_empty(), "the selection must hold cases");
    let mut failures = HashMap::new();
    for case in &cases {
        let outcome = evaluate(case);
        let expected = expected(case);
        if outcome.as_ref() != Ok(&expected) {
            failures.insert(
                format!("{}/{}", case.group, case.entry),
                format!("answered {outcome:?}, expected {expected:?}"),
            );
        }
    }
    let mut names: Vec<&String> = failures.keys().collect();
    names.sort_unstable();
    for name in &names {
        eprintln!("FAIL {name}: {}", failures[*name]);
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed: {names:?}",
        failures.len(),
        cases.len()
    );
}
