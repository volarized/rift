//! History Contribution conversion.

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ExtensionKey, ExtensionValue, Extensions,
    ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability, SourceKind,
    SourceLocation, SourcePath,
};
use rift_error::{RiftError, errors};
use serde_json::json;
use std::collections::BTreeMap;

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
    /// Returns [`RiftError`] when a provider symbol, origin, or
    /// Contribution is invalid.
    pub fn convert(
        &self,
        path: &SourcePath,
        history: &PathHistory,
    ) -> Result<Vec<Contribution>, RiftError> {
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .map_err(|error| {
            errors::history::contribution_invalid()
                .detail(error.to_string())
                .error()
        })?;
        history
            .revisions()
            .iter()
            .map(|item| {
                let symbol =
                    ProviderSymbolId::new(format!("{}:{}", item.commit_id(), path.as_str()))
                        .map_err(|error| {
                            errors::history::contribution_invalid()
                                .detail(error.to_string())
                                .error()
                        })?;
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
                .map_err(|error| {
                    errors::history::contribution_invalid()
                        .detail(error.to_string())
                        .error()
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rift_core::{ProviderId, ProviderRevision, SourceApplicability, SourcePath};

    use super::{HistoryContributionAdapter, errors};
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
    fn error_exposes_registered_identity_and_detail() {
        let error = errors::history::contribution_invalid()
            .detail("invalid history fact")
            .error();
        assert_eq!(error.slug(), errors::history::contribution_invalid::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "detail" && value == "invalid history fact")
        );
        assert!(error.to_string().contains("invalid history fact"));
        let _: &dyn std::error::Error = &error;
    }
}
