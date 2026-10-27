//! RDF terms and SPARQL result forms.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One RDF term, preserving its identity or literal interpretation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RdfTerm {
    /// An absolute Internationalized Resource Identifier.
    Iri {
        /// The complete IRI.
        value: String,
    },
    /// A blank-node identity within the retained answer.
    BlankNode {
        /// The identity retained across every page of this answer.
        value: String,
    },
    /// A literal with an explicit datatype IRI.
    Literal {
        /// The lexical value.
        value: String,
        /// The datatype IRI.
        datatype: String,
    },
    /// A string literal with a language tag.
    LanguageLiteral {
        /// The lexical value.
        value: String,
        /// The normalized language tag.
        language: String,
    },
}

/// One RDF triple in a constructed or described graph.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RdfTriple {
    /// The triple's subject IRI or blank node.
    pub subject: RdfTerm,
    /// The triple's predicate IRI.
    pub predicate: RdfTerm,
    /// The triple's object term.
    pub object: RdfTerm,
}

/// The result form selected by the SPARQL statement.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryAnswer {
    /// Projected variables and rows returned by `SELECT`.
    Bindings {
        /// Projected variable names, without the leading question mark.
        variables: Vec<String>,
        /// Rows in this execution's retained order; unbound variables are absent.
        bindings: Vec<BTreeMap<String, RdfTerm>>,
    },
    /// The answer returned by `ASK` after required facts are available.
    Boolean {
        /// Whether the graph pattern has a solution.
        value: bool,
    },
    /// The triples returned by `CONSTRUCT` or `DESCRIBE`.
    Graph {
        /// Triples in this execution's retained order.
        triples: Vec<RdfTriple>,
    },
}
