//! Library-owned SPARQL evaluation over immutable domain facts.

use crate::query::FactRequirement;
mod dataset;
mod failure;
mod page;
mod plan;
mod recipes;
mod requirements;
mod result;
mod services;

use std::fmt;

use oxigraph::sparql::{CancellationToken, SparqlEvaluator};
use rift_protocol::query::QueryAnswer;
use spargebra::{Query, SparqlParser};

pub use crate::query::{
    AdjacencyRequirement, FactRequirements, QueryFactFamily, QueryFacts, QueryLimits, QuerySource,
    QuerySymbol,
};
pub use dataset::{QuerySubjectResource, retained_subject_resources};
pub use page::{PageForm, PageSelection, continuation_query};
pub use plan::{QueryPlan, RetainedSubjectSelection};
pub use recipes::retrieval_query;
pub use services::{
    QueryDataset, QueryIdentities, QueryServiceData, QueryServices, RetainedResult,
};

macro_rules! vocabulary_iri {
    ($term:literal) => {
        concat!("urn:rift:vocabulary:", $term)
    };
}

/// Namespace shared by the built-in empty and `r` prefixes.
pub const VOCABULARY: &str = vocabulary_iri!("");
/// Local retrieval service reusing Rift's existing search producers.
pub const SEARCH_SERVICE: &str = vocabulary_iri!("search");
/// Namespace of retained query executions selected through `SERVICE`.
pub const RESULT_SERVICE_PREFIX: &str = "urn:rift:result:";
/// Namespace of accepted extra-project dataset names.
pub const EXTRA_PROJECTS: &str = "urn:rift:graph:extra:";
/// Named graph of the selected local publication.
pub const LOCAL_GRAPH: &str = "urn:rift:graph:local";

const PREFIXES: [(&str, &str); 6] = [
    ("", VOCABULARY),
    ("r", VOCABULARY),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("extra", EXTRA_PROJECTS),
];

/// Cancellation shared with the CPU evaluator and its lazy result iterator.
#[derive(Clone, Default)]
pub struct QueryCancellation(CancellationToken);

impl fmt::Debug for QueryCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryCancellation")
            .finish_non_exhaustive()
    }
}

impl QueryCancellation {
    /// Requests cancellation of evaluation and result iteration.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Whether evaluation has been cancelled by the serving owner.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Registered query parsing, mapping and evaluation failure.
pub type QueryError = rift_error::RiftError;

/// Parsed query and sufficient domain acquisition requirements.
#[derive(Clone, Debug)]
pub struct PreparedQuery {
    query: Query,
    requirements: FactRequirements,
    retained_subject_selection: Option<RetainedSubjectSelection>,
}

impl PreparedQuery {
    /// Parses a statement using inherited prefixes and accepted admission bounds.
    ///
    /// # Errors
    ///
    /// Returns a parser error or the crossed query/algebra bound.
    pub fn parse(statement: &str, limits: QueryLimits) -> Result<Self, QueryError> {
        if statement.len() > limits.query_bytes_max {
            return Err(failure::bound("query_bytes_max"));
        }
        let mut parser = SparqlParser::new();
        for (prefix, iri) in PREFIXES {
            parser = parser.with_prefix(prefix, iri).map_err(failure::parse)?;
        }
        let query = parser.parse_query(statement).map_err(failure::parse)?;
        let (requirements, retained_subject_selection) =
            FactRequirements::from_query(&query, limits)?;
        Ok(Self {
            query,
            requirements,
            retained_subject_selection,
        })
    }

    /// Domain facts required before this statement can report a complete answer.
    #[must_use]
    pub fn requirements(&self) -> &FactRequirements {
        &self.requirements
    }

    /// Existing retrieval operations required by local `SERVICE :search` groups.
    #[must_use]
    pub fn retrievals(&self) -> &[rift_protocol::read::SearchParams] {
        &self.requirements.retrievals
    }

    /// Returns expanded SPARQL with the effective inherited prefixes resolved.
    #[must_use]
    pub fn expanded(&self) -> String {
        self.query.to_string()
    }

    /// Evaluates complete selected facts and retains the actual result form.
    ///
    /// # Errors
    ///
    /// Returns mapping, bound, cancellation or library evaluation failures.
    pub fn evaluate(
        &self,
        facts: QueryFacts,
        limits: QueryLimits,
        cancellation: QueryCancellation,
    ) -> Result<QueryAnswer, QueryError> {
        self.evaluate_with_services(facts, limits, cancellation, QueryServices::default())
    }

    /// Evaluates selected facts with pre-acquired retrieval and retained binding services.
    ///
    /// # Errors
    ///
    /// Returns registered mapping, admission, cancellation or evaluation failures.
    pub fn evaluate_with_services(
        &self,
        facts: QueryFacts,
        limits: QueryLimits,
        cancellation: QueryCancellation,
        mut services: QueryServices,
    ) -> Result<QueryAnswer, QueryError> {
        services.validate(&self.requirements)?;
        self.requirements.validate_facts(&facts)?;
        for selected in &services.datasets {
            self.requirements.validate_facts(&selected.facts)?;
        }
        let dataset = dataset::VirtualDataset::new(facts, limits, cancellation.0.clone())?
            .with_datasets(std::mem::take(&mut services.datasets))?;
        let evaluator = services.install(
            SparqlEvaluator::new(),
            limits,
            &cancellation,
            dataset.clone(),
        )?;
        let result = evaluator
            .with_cancellation_token(cancellation.0)
            .for_query(self.query.clone())
            .on_queryable_dataset(dataset)
            .execute()
            .map_err(result::evaluation_error)?;
        result::collect(result, limits)
    }
}

impl FactRequirements {
    /// Checks that the selected publication contains every requested fact family.
    ///
    /// # Errors
    /// Returns the mapping refusal for a required family absent from the publication.
    pub fn validate_facts(&self, facts: &QueryFacts) -> Result<(), QueryError> {
        for family in &facts.unavailable_families {
            let required = match family {
                QueryFactFamily::Nodes => self.requires(FactRequirement::Nodes),
                QueryFactFamily::History => self.requires(FactRequirement::History),
                QueryFactFamily::Documentation => self.requires(FactRequirement::Documentation),
            };
            if required {
                return Err(failure::invalid(format!(
                    "selected publication does not contain required {family:?} facts"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod tests;
