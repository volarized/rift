//! Typed result collection under accepted retention bounds.

use oxigraph::model::Term;
use oxigraph::sparql::{QueryEvaluationError, QueryResults};
use rift_protocol::query::{QueryAnswer, RdfTerm, RdfTriple};

use super::failure;
use super::{QueryError, QueryLimits};

pub(super) fn collect(
    results: QueryResults,
    limits: QueryLimits,
) -> Result<QueryAnswer, QueryError> {
    let mut retained = 0usize;
    let answer = match results {
        QueryResults::Boolean(value) => QueryAnswer::Boolean { value },
        QueryResults::Solutions(solutions) => {
            let variables = solutions
                .variables()
                .iter()
                .map(|value| value.as_str().to_owned())
                .collect();
            let mut bindings = Vec::new();
            for solution in solutions {
                admit_item(bindings.len(), limits)?;
                let solution = solution.map_err(evaluation_error)?;
                let row = solution
                    .iter()
                    .map(|(variable, value)| (variable.as_str().to_owned(), term(value.clone())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                admit_bytes(&row, &mut retained, limits)?;
                bindings.push(row);
            }
            QueryAnswer::Bindings {
                variables,
                bindings,
            }
        }
        QueryResults::Graph(graph) => {
            let mut triples = Vec::new();
            for triple in graph {
                admit_item(triples.len(), limits)?;
                let triple = triple.map_err(evaluation_error)?;
                let triple = RdfTriple {
                    subject: term(triple.subject.into()),
                    predicate: term(triple.predicate.into()),
                    object: term(triple.object),
                };
                admit_bytes(&triple, &mut retained, limits)?;
                triples.push(triple);
            }
            QueryAnswer::Graph { triples }
        }
    };
    let bytes = serde_json::to_vec(&answer).map_err(failure::evaluation)?;
    if bytes.len() > limits.result_bytes_max {
        return Err(failure::bound("result_bytes_max"));
    }
    Ok(answer)
}

fn admit_bytes(
    value: &impl serde::Serialize,
    retained: &mut usize,
    limits: QueryLimits,
) -> Result<(), QueryError> {
    let bytes = serde_json::to_vec(value).map_err(failure::evaluation)?;
    let next = retained
        .checked_add(bytes.len())
        .ok_or_else(|| failure::bound("result_bytes_max"))?;
    if next > limits.result_bytes_max {
        return Err(failure::bound("result_bytes_max"));
    }
    *retained = next;
    Ok(())
}

fn admit_item(count: usize, limits: QueryLimits) -> Result<(), QueryError> {
    if count >= limits.result_items_max {
        Err(failure::bound("result_items_max"))
    } else {
        Ok(())
    }
}

fn term(value: Term) -> RdfTerm {
    match value {
        Term::NamedNode(value) => RdfTerm::Iri {
            value: value.into_string().into(),
        },
        Term::BlankNode(value) => RdfTerm::BlankNode {
            value: value.into_string().into(),
        },
        Term::Literal(value) => match value.language() {
            Some(language) => RdfTerm::LanguageLiteral {
                value: value.value().to_owned(),
                language: language.to_owned(),
            },
            None => RdfTerm::Literal {
                value: value.value().to_owned(),
                datatype: value.datatype().as_str().to_owned(),
            },
        },
    }
}

pub(super) fn evaluation_error(error: QueryEvaluationError) -> QueryError {
    match error {
        QueryEvaluationError::Cancelled => failure::cancelled(),
        error => failure::evaluation(error),
    }
}
