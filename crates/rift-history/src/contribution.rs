//! History Contribution conversion.

use std::collections::BTreeMap;
use std::fmt;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ExtensionKey, ExtensionValue, Extensions,
    ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability, SourceKind,
    SourceLocation, SourcePath,
};
use serde_json::json;

use crate::PathHistory;

/// Converts one path history into provider Contributions.
#[derive(Debug, Clone)]
pub struct HistoryContributionAdapter {
    provider: ProviderId,
    revision: ProviderRevision,
}

impl HistoryContributionAdapter {
    /// Creates one history adapter.
    #[must_use]
    pub const fn new(provider: ProviderId, revision: ProviderRevision) -> Self {
        Self { provider, revision }
    }

    /// Converts touching commits into revision-independent Git facts.
    ///
    /// Facts retain commit and blob identity but carry no current-tree identity
    /// anchor or association evidence.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryContributionError`] when a provider symbol, origin, or
    /// Contribution is invalid.
    pub fn convert(
        &self,
        path: &SourcePath,
        history: &PathHistory,
    ) -> Result<Vec<Contribution>, HistoryContributionError> {
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .map_err(|error| history_fact_error(error.to_string()))?;
        history
            .revisions()
            .iter()
            .map(|item| {
                let symbol =
                    ProviderSymbolId::new(format!("{}:{}", item.commit_id(), path.as_str()))
                        .map_err(|error| history_fact_error(error.to_string()))?;
                let blob = item.blob().map(|blob| {
                    json!({
                        "id": blob.blob_id(),
                        "path": blob.path(),
                    })
                });
                let namespaced = Extensions(BTreeMap::from([(
                    ExtensionKey("org.rift.history".to_owned()),
                    ExtensionValue {
                        version: 1,
                        data: json!({
                            "blob": blob,
                            "commit_id": item.commit_id(),
                            "complete": history.is_complete(),
                            "path": path.as_str(),
                            "summary": item.summary(),
                            "timestamp": item.timestamp(),
                        }),
                    },
                )]));
                Contribution::fact_builder(
                    ContributionKey::new(self.provider.clone(), self.revision, symbol),
                    SourceApplicability::Independent,
                    origin.clone(),
                )
                .namespaced(namespaced)
                .build()
                .map_err(|error| history_fact_error(error.to_string()))
            })
            .collect()
    }
}

/// Stable history Contribution conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryContributionViolation {
    /// History fact could not become a Contribution.
    InvalidFact,
}

/// Error returned by history Contribution conversion.
#[derive(Debug)]
pub struct HistoryContributionError {
    violation: HistoryContributionViolation,
    detail: String,
}

impl HistoryContributionError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> HistoryContributionViolation {
        self.violation
    }

    /// Returns failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for HistoryContributionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history Contribution conversion rejected {:?}: {}",
            self.violation, self.detail
        )
    }
}

impl std::error::Error for HistoryContributionError {}

fn history_fact_error(detail: impl Into<String>) -> HistoryContributionError {
    HistoryContributionError {
        violation: HistoryContributionViolation::InvalidFact,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rift_core::{ProviderId, ProviderRevision, SourceApplicability, SourcePath};

    use super::{HistoryContributionAdapter, HistoryContributionViolation, history_fact_error};
    use crate::{Repository, fixture};

    #[test]
    fn path_history_converts_to_unbound_namespaced_facts() {
        let directory = tempfile::tempdir().expect("directory");
        fixture::init(directory.path());
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n").expect("source");
        fixture::commit_all(directory.path(), "introduce beacon");
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() -> u8 { 7 }\n",
        )
        .expect("source");
        fixture::commit_all(directory.path(), "change beacon");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head");
        let history = repository
            .path_revisions(&head, "lib.rs", 100)
            .expect("history");
        let facts = HistoryContributionAdapter::new(
            ProviderId::new("git").expect("provider"),
            ProviderRevision::new(9).expect("revision"),
        )
        .convert(&SourcePath::new("lib.rs").expect("path"), &history)
        .expect("history facts");

        assert_eq!(facts.len(), 2);
        assert!(facts.iter().all(|fact| {
            fact.facts().is_none()
                && fact.source().is_none()
                && fact.identity_anchor().is_none()
                && fact.equivalence().is_empty()
                && fact.applicability() == SourceApplicability::Independent
        }));
        let newest = facts[0]
            .namespaced()
            .0
            .get(&rift_core::ExtensionKey("org.rift.history".to_owned()))
            .expect("history fact");
        assert_eq!(newest.data["path"], "lib.rs");
        assert_eq!(newest.data["summary"], "change beacon");
        assert_eq!(newest.data["complete"], true);
        assert!(newest.data["blob"]["id"].as_str().is_some());
        assert_eq!(newest.data["blob"]["path"], "lib.rs");
        assert_eq!(newest.data["commit_id"].as_str().map(str::len), Some(40));
        assert_eq!(facts[0].key().reference().provider().as_str(), "git");
    }

    #[test]
    fn empty_history_converts_to_empty_fact_set() {
        let directory = tempfile::tempdir().expect("directory");
        fixture::init(directory.path());
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n").expect("source");
        fixture::commit_all(directory.path(), "introduce beacon");
        let repository = Repository::open(directory.path()).expect("repository");
        let head = repository.resolve("HEAD").expect("head");
        let history = repository
            .path_revisions(&head, "absent.rs", 100)
            .expect("history");
        let facts = HistoryContributionAdapter::new(
            ProviderId::new("git").expect("provider"),
            ProviderRevision::new(1).expect("revision"),
        )
        .convert(&SourcePath::new("absent.rs").expect("path"), &history)
        .expect("history facts");

        assert!(facts.is_empty());
    }

    #[test]
    fn error_exposes_stable_violation_and_detail() {
        let error = history_fact_error("invalid history fact");
        assert_eq!(error.violation(), HistoryContributionViolation::InvalidFact);
        assert_eq!(error.detail(), "invalid history fact");
        assert!(error.to_string().contains("invalid history fact"));
        let _: &dyn std::error::Error = &error;
    }
}
