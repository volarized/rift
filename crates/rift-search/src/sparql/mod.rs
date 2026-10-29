//! SPARQL evaluation over Rift's graph model.
//!
//! A caller states one read's facts - symbols, nodes, source units, revisions, and the edges
//! between them - as [`GraphFact`] values. [`GraphDatasetBuilder`] writes each fact as triples
//! over the fixed vocabulary under [`VOCABULARY_NAMESPACE`], within [`GraphLimits`].
//! [`GraphQuery::parse`] admits one SPARQL 1.1 `SELECT` or `ASK` query, and
//! [`GraphDataset::evaluate`] answers it as a [`GraphAnswer`].
//!
//! # Examples
//!
//! ```
//! use rift_protocol::read::{Language, SymbolId};
//! use rift_search::sparql::{
//!     GraphAnswer, GraphDataset, GraphFact, GraphLimits, GraphQuery, GraphTerm,
//!     QueryCancellation, SymbolRelation,
//! };
//!
//! let limits = GraphLimits::default();
//! let rust = Language { name: "rust".to_owned(), dialect: None };
//! let main = SymbolId("rift://symbol/local/rust/app/main".to_owned());
//! let parse = SymbolId("rift://symbol/local/rust/app/parser/parse".to_owned());
//! let mut graph = GraphDataset::builder(limits);
//! graph.insert(GraphFact::Symbol { symbol: &main, kind: "function", language: &rust })?;
//! graph.insert(GraphFact::Relation {
//!     source: &main,
//!     relation: SymbolRelation::Calls,
//!     target: &parse,
//! })?;
//! let dataset = graph.build();
//!
//! let query = GraphQuery::parse("SELECT ?callee { ?caller :kind \"function\" ; :calls ?callee }", &limits)?;
//! let GraphAnswer::Solutions(table) = dataset.evaluate(&query, &QueryCancellation::new())? else {
//!     unreachable!("a SELECT answers solutions");
//! };
//! assert_eq!(table.rows, vec![vec![Some(GraphTerm::Iri(parse.0.clone()))]]);
//! # Ok::<(), rift_error::RiftError>(())
//! ```

mod admission;
mod dataset;
mod evaluation;
mod expression;
mod limits;
mod query;
mod query_thread;
mod vocabulary;

#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod tests;

pub use dataset::{
    GraphDataset, GraphDatasetBuilder, GraphFact, HistoryEvent, NodeAddress, NodeFact,
    NodeLocation, RevisionIri, SymbolRelation,
};
pub use evaluation::{GraphAnswer, GraphTerm, QueryCancellation, SolutionTable};
pub use limits::{
    ALGEBRA_DEPTH_MAX, ANSWER_BYTES_MAX, BRACKETS_MAX, COMPUTED_BYTES_MAX, EVALUATION_TIMEOUT,
    EXPRESSION_BYTES_MAX, GraphLimits, GraphLimitsBuilder, LITERAL_BYTES_MAX, OPERATORS_MAX,
    PATTERNS_MAX, QUERY_BYTES_MAX, SOLUTIONS_MAX, TRIPLES_MAX,
};
pub use query::GraphQuery;
pub use vocabulary::{
    PropertyDefinition, VOCABULARY_NAMESPACE, VocabularyClass, VocabularyProperty,
    VocabularyResource, VocabularyValue,
};
