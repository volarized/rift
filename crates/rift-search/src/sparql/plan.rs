//! Acquisition and retained-page metadata produced by the query child.

use rift_protocol::read::SearchParams;
use serde::{Deserialize, Serialize};

use super::{FactRequirements, PageSelection, PreparedQuery, QueryError, QueryLimits};

/// Query metadata that the serving owner can use without parsing the statement.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueryPlan {
    statement: String,
    expanded: String,
    requirements: FactRequirements,
    page: Option<PageSelection>,
}

impl QueryPlan {
    /// Parses a statement and extracts metadata inside the query child.
    ///
    /// # Errors
    /// Returns the parser or admission failure reported by [`PreparedQuery::parse`].
    pub fn prepare(statement: String, limits: QueryLimits) -> Result<Self, QueryError> {
        let prepared = PreparedQuery::parse(&statement, limits)?;
        Ok(Self {
            statement,
            expanded: prepared.expanded(),
            page: prepared.page_selection(),
            requirements: prepared.requirements,
        })
    }

    /// Original statement, parsed again only inside the evaluation child.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// Domain facts required before this statement can report a complete answer.
    #[must_use]
    pub fn requirements(&self) -> &FactRequirements {
        &self.requirements
    }

    /// Existing retrieval operations required by local `SERVICE :search` groups.
    #[must_use]
    pub fn retrievals(&self) -> &[SearchParams] {
        &self.requirements.retrievals
    }

    /// Expanded SPARQL produced by the library parser.
    #[must_use]
    pub fn expanded(&self) -> String {
        self.expanded.clone()
    }

    /// Exact retained-page projection recognized inside the query child.
    #[must_use]
    pub fn page_selection(&self) -> Option<PageSelection> {
        self.page.clone()
    }
}
