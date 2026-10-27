//! Acquisition and retained-page metadata produced by the query child.

use rift_protocol::read::SearchParams;
use serde::{Deserialize, Serialize};

use super::{FactRequirements, PageSelection, PreparedQuery, QueryError, QueryLimits};

/// Query metadata that the serving owner can use without parsing the statement.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPlan {
    statement: String,
    expanded: String,
    requirements: FactRequirements,
    page: Option<PageSelection>,
    #[serde(deserialize_with = "required_retained_subject_selection")]
    retained_subject_selection: Option<RetainedSubjectSelection>,
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
            retained_subject_selection: prepared.retained_subject_selection,
            requirements: prepared.requirements,
        })
    }

    /// Mandatory retained binding service and the subject variable used by local facts.
    /// This metadata requires validation against the actual retained table and original subjects.
    #[must_use]
    pub fn retained_subject_selection(&self) -> Option<&RetainedSubjectSelection> {
        self.retained_subject_selection.as_ref()
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

/// A mandatory retained binding service constraining one local fact subject variable.
/// The serving owner must validate the actual saved column and original subject directory.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSubjectSelection {
    service: String,
    variable: String,
}

impl RetainedSubjectSelection {
    pub(super) fn new(
        service: &str,
        variable: &str,
        text_bytes_max: usize,
    ) -> Result<Self, QueryError> {
        if service
            .len()
            .checked_add(variable.len())
            .is_none_or(|bytes| bytes > text_bytes_max)
        {
            return Err(super::failure::bound("query_bytes_max"));
        }
        let copy = |value: &str| {
            let mut text = String::new();
            text.try_reserve_exact(value.len())
                .map_err(super::failure::invalid)?;
            text.push_str(value);
            Ok::<_, QueryError>(text)
        };
        Ok(Self {
            service: copy(service)?,
            variable: copy(variable)?,
        })
    }

    /// Original retained service IRI recognized by the child.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Subject column to validate in every stored binding row.
    #[must_use]
    pub fn variable(&self) -> &str {
        &self.variable
    }
}

fn required_retained_subject_selection<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<RetainedSubjectSelection>, D::Error> {
    Option::<RetainedSubjectSelection>::deserialize(deserializer)
}
