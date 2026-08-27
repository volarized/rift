//! Shared publication contract for provider input adapters.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use rift_core::SourceUnitId;

use crate::ProviderPublication;

/// Provider input form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputMode {
    /// One bounded operation answer.
    Operation,
    /// One versioned provider snapshot.
    Snapshot,
    /// Directly published facts.
    Fact,
}

/// Coverage claimed by one adapter result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationCoverage {
    /// Only current request.
    Request,
    /// Listed source units.
    SourceUnits(Vec<SourceUnitId>),
    /// Captured workspace.
    Workspace,
}

/// Invalid input mode or coverage pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterViolation {
    /// Operation answer must claim request coverage.
    OperationCoverage,
    /// Snapshot and fact inputs cannot claim request coverage.
    PublishedRequestCoverage,
    /// Source-unit coverage is empty or duplicated.
    InvalidSourceUnits,
    /// Exact Contribution lies outside declared source-unit coverage.
    ContributionOutsideCoverage,
}

/// Adapter validation failure.
#[derive(Debug)]
pub struct AdapterError {
    violation: AdapterViolation,
}

impl AdapterError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> AdapterViolation {
        self.violation
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "provider adapter rejected {:?}", self.violation)
    }
}

impl Error for AdapterError {}

/// Validated adapter output using one Contribution publication contract.
#[derive(Debug)]
pub struct AdapterPublication {
    mode: ProviderInputMode,
    coverage: PublicationCoverage,
    publication: ProviderPublication,
}

impl AdapterPublication {
    /// Validates one adapter output.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] when input mode and coverage disagree.
    pub fn new(
        mode: ProviderInputMode,
        coverage: PublicationCoverage,
        publication: ProviderPublication,
    ) -> Result<Self, AdapterError> {
        match (&mode, &coverage) {
            (ProviderInputMode::Operation, PublicationCoverage::Request) => {}
            (ProviderInputMode::Operation, _) => {
                return Err(adapter_error(AdapterViolation::OperationCoverage));
            }
            (_, PublicationCoverage::Request) => {
                return Err(adapter_error(AdapterViolation::PublishedRequestCoverage));
            }
            (_, PublicationCoverage::SourceUnits(units)) => {
                let unique: BTreeSet<_> = units.iter().collect();
                if units.is_empty() || unique.len() != units.len() {
                    return Err(adapter_error(AdapterViolation::InvalidSourceUnits));
                }
                if publication.contributions().iter().any(|contribution| {
                    contribution
                        .source()
                        .is_some_and(|binding| !unique.contains(binding.unit()))
                }) {
                    return Err(adapter_error(AdapterViolation::ContributionOutsideCoverage));
                }
            }
            (_, PublicationCoverage::Workspace) => {}
        }
        Ok(Self {
            mode,
            coverage,
            publication,
        })
    }

    /// Returns provider input mode.
    #[must_use]
    pub const fn mode(&self) -> ProviderInputMode {
        self.mode
    }

    /// Returns declared coverage.
    #[must_use]
    pub const fn coverage(&self) -> &PublicationCoverage {
        &self.coverage
    }

    /// Returns immutable Contribution publication.
    #[must_use]
    pub const fn publication(&self) -> &ProviderPublication {
        &self.publication
    }

    /// Consumes wrapper and returns publication.
    #[must_use]
    pub fn into_publication(self) -> ProviderPublication {
        self.publication
    }
}

const fn adapter_error(violation: AdapterViolation) -> AdapterError {
    AdapterError { violation }
}

#[cfg(test)]
mod tests {
    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ExactKind, Language,
        PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability,
        SourceKind, SourcePath, SourceResolverId, SourceUnitId,
    };

    use super::{AdapterPublication, AdapterViolation, ProviderInputMode, PublicationCoverage};
    use crate::{ProviderPublication, PublicationLimits};

    fn provider() -> ProviderId {
        ProviderId::new("test").expect("provider")
    }

    fn publication() -> ProviderPublication {
        let contribution = Contribution::builder(
            ContributionKey::new(
                provider(),
                ProviderRevision::new(1).expect("revision"),
                ProviderSymbolId::new("item").expect("symbol"),
            ),
            SourceApplicability::Independent,
            PortableSymbolFacts::new(
                Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                "item",
                "item",
                ExactKind("rust.function".to_owned()),
            ),
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("origin"),
        )
        .build()
        .expect("contribution");
        ProviderPublication::new(
            provider(),
            ProviderRevision::new(1).expect("revision"),
            vec![contribution],
            PublicationLimits::default(),
        )
        .expect("publication")
    }

    fn unit() -> SourceUnitId {
        SourceUnitId::new(
            SourceResolverId::new("project").expect("resolver"),
            SourcePath::new("src/lib.rs").expect("path"),
        )
        .expect("unit")
    }

    #[test]
    fn every_input_mode_accepts_its_coverage() {
        let cases = [
            (ProviderInputMode::Operation, PublicationCoverage::Request),
            (ProviderInputMode::Snapshot, PublicationCoverage::Workspace),
            (
                ProviderInputMode::Fact,
                PublicationCoverage::SourceUnits(vec![unit()]),
            ),
        ];
        for (mode, coverage) in cases {
            let output =
                AdapterPublication::new(mode, coverage, publication()).expect("adapter output");
            assert_eq!(output.mode(), mode);
            assert_eq!(output.publication().contributions().len(), 1);
        }
    }

    #[test]
    fn mode_and_coverage_mismatch_is_refused() {
        let operation = AdapterPublication::new(
            ProviderInputMode::Operation,
            PublicationCoverage::Workspace,
            publication(),
        )
        .expect_err("operation coverage");
        assert_eq!(operation.violation(), AdapterViolation::OperationCoverage);

        let snapshot = AdapterPublication::new(
            ProviderInputMode::Snapshot,
            PublicationCoverage::Request,
            publication(),
        )
        .expect_err("snapshot coverage");
        assert_eq!(
            snapshot.violation(),
            AdapterViolation::PublishedRequestCoverage
        );
    }

    #[test]
    fn empty_or_duplicate_source_coverage_is_refused() {
        for units in [Vec::new(), vec![unit(), unit()]] {
            let error = AdapterPublication::new(
                ProviderInputMode::Fact,
                PublicationCoverage::SourceUnits(units),
                publication(),
            )
            .expect_err("invalid source coverage");
            assert_eq!(error.violation(), AdapterViolation::InvalidSourceUnits);
        }
    }
}
