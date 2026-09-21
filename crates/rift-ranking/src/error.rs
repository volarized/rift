//! Registry identity for the ranking layer's refusals.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, fault_label};
use serde::Serialize;

/// What the ranking layer refused, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingViolation {
    /// A query carries no text at all.
    QueryEmpty,
    /// A query runs past the accepted byte bound.
    QueryLength,
    /// One term or quoted phrase runs past the accepted byte bound.
    QueryTermLength,
    /// A quoted phrase opens and never closes.
    QueryQuoteUnterminated,
    /// A query carries more quoted phrases than one rendered phase accepts.
    /// Unquoted terms narrow to the bound; a phrase cannot be dropped without
    /// changing what the caller asked for.
    QueryPhraseLimit,
    /// A document identity is empty.
    DocumentIdentityEmpty,
    /// One document field runs past the accepted byte bound.
    DocumentFieldLength,
    /// Fusion was handed shares that cannot carry a score.
    RankingWeightsInvalid,
    /// Fusion was handed a rank constant outside the accepted range.
    FusionConstantInvalid,
    /// Two publications state index capabilities that cannot rank together.
    CapabilitiesIncompatible,
    /// The store behind one reader refused, and its own failure rides as the
    /// cause.
    ReaderFailed,
}

impl RankingViolation {
    /// The registry identity this violation classifies as.
    const fn name(self) -> ErrorName {
        match self {
            Self::QueryEmpty
            | Self::QueryLength
            | Self::QueryTermLength
            | Self::QueryQuoteUnterminated
            | Self::QueryPhraseLimit => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::DocumentIdentityEmpty
            | Self::DocumentFieldLength
            | Self::CapabilitiesIncompatible => ErrorName::Wire(ErrorCode::InternalError),
            Self::ReaderFailed => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::RankingWeightsInvalid | Self::FusionConstantInvalid => {
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            }
        }
    }
}

/// One ranking-layer failure, with the evidence its violation carries.
#[derive(Debug)]
pub struct RankingFault {
    violation: RankingViolation,
    subject: Option<String>,
    limit: Option<LimitEvidence>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl RankingFault {
    /// The bare violation, carrying no further evidence.
    #[must_use]
    pub const fn new(violation: RankingViolation) -> Self {
        Self {
            violation,
            subject: None,
            limit: None,
            source: None,
        }
    }

    /// Names the value the violation is about: a parameter, a field, or a key.
    #[must_use]
    pub fn about(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Records the bound in force and what the request would have needed.
    #[must_use]
    pub fn over_limit(mut self, field: &str, limit: usize, required: usize) -> Self {
        self.limit = Some(LimitEvidence {
            field: field.to_owned(),
            limit: count(limit),
            required: count(required),
        });
        self
    }

    /// Carries the failure a store answered with, so its own classification and
    /// driver text stay on the source chain rather than being flattened into a
    /// rendered subject.
    #[must_use]
    pub fn caused_by(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The violation this failure classifies as.
    #[must_use]
    pub const fn violation(&self) -> RankingViolation {
        self.violation
    }
}

impl Fault for RankingFault {
    fn name(&self) -> ErrorName {
        self.violation.name()
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(subject) = self.subject.as_ref() {
            context.push(ErrorContext::new("subject", subject.clone()));
        }
        context
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.limit.clone()
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// One ranking-layer failure.
pub type RankingError = Error<RankingFault>;

/// Refuses one value, naming the violation and the subject it is about.
pub(crate) fn refuse(violation: RankingViolation, subject: &str) -> RankingError {
    Error::new(RankingFault::new(violation).about(subject))
}

/// Refuses one value that crossed a bound, naming the bound's field, the bound
/// in force, and what the value would have needed.
pub(crate) fn refuse_over_limit(
    violation: RankingViolation,
    subject: &str,
    field: &str,
    limit: usize,
    required: usize,
) -> RankingError {
    Error::new(
        RankingFault::new(violation)
            .about(subject)
            .over_limit(field, limit, required),
    )
}

/// Widens a bounded in-memory count into the `u64` domain limit evidence
/// carries. Every count this crate bounds fits comfortably; the fallback only
/// guards the conversion.
pub(crate) fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{RankingFault, RankingViolation, count, refuse, refuse_over_limit};
    use rift_core::{Error, ErrorCode, ErrorName, Fault as _};

    /// Every violation this crate can raise, so a new one has to name its code
    /// here before it can reach a caller.
    const EVERY_VIOLATION: [RankingViolation; 11] = [
        RankingViolation::QueryEmpty,
        RankingViolation::QueryLength,
        RankingViolation::QueryTermLength,
        RankingViolation::QueryQuoteUnterminated,
        RankingViolation::QueryPhraseLimit,
        RankingViolation::DocumentIdentityEmpty,
        RankingViolation::DocumentFieldLength,
        RankingViolation::RankingWeightsInvalid,
        RankingViolation::FusionConstantInvalid,
        RankingViolation::CapabilitiesIncompatible,
        RankingViolation::ReaderFailed,
    ];

    #[test]
    fn test_every_violation_classifies_as_the_code_its_caller_acts_on() {
        for violation in EVERY_VIOLATION {
            let name = RankingFault::new(violation).name();
            let expected = match violation {
                RankingViolation::QueryEmpty
                | RankingViolation::QueryLength
                | RankingViolation::QueryTermLength
                | RankingViolation::QueryQuoteUnterminated
                | RankingViolation::QueryPhraseLimit => ErrorCode::InvalidRequest,
                RankingViolation::DocumentIdentityEmpty
                | RankingViolation::DocumentFieldLength
                | RankingViolation::CapabilitiesIncompatible => ErrorCode::InternalError,
                RankingViolation::RankingWeightsInvalid
                | RankingViolation::FusionConstantInvalid => ErrorCode::ConfigurationInvalid,
                RankingViolation::ReaderFailed => ErrorCode::StorageFailure,
            };
            assert_eq!(
                name,
                ErrorName::Wire(expected),
                "{violation:?} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn test_a_store_failure_rides_the_source_chain_rather_than_the_subject() {
        // The contract is storage-independent, so it cannot restate a store's own
        // violation. Carrying the failure itself is what keeps the driver text
        // reachable from the refusal a caller reads.
        let store = std::io::Error::other("the database is locked");
        let fault = RankingFault::new(RankingViolation::ReaderFailed)
            .about("lexical")
            .caused_by(store);
        assert_eq!(fault.violation(), RankingViolation::ReaderFailed);
        let carried = fault.source().expect("the store failure rides along");
        assert!(
            carried.to_string().contains("the database is locked"),
            "the driver text stays reachable: {carried}"
        );
        let context: Vec<(String, String)> = fault
            .context()
            .into_iter()
            .map(|held| (held.key().to_owned(), held.value().to_owned()))
            .collect();
        assert!(context.contains(&("violation".to_owned(), "reader_failed".to_owned())));
        assert!(context.contains(&("subject".to_owned(), "lexical".to_owned())));
        assert!(
            fault.limit_evidence().is_none(),
            "a store failure carries no bound"
        );
        let rendered = Error::new(fault).to_string();
        assert!(!rendered.is_empty());
    }

    #[test]
    fn test_a_bounded_refusal_carries_the_limit_and_what_was_needed() {
        let refused = refuse_over_limit(
            RankingViolation::QueryLength,
            "query",
            "query",
            4_096,
            5_000,
        );
        let evidence = refused
            .fault()
            .limit_evidence()
            .expect("an over-limit refusal carries its bound");
        assert_eq!(evidence.field, "query");
        assert_eq!(evidence.limit, 4_096);
        assert_eq!(evidence.required, 5_000);
        assert!(
            refuse(RankingViolation::QueryEmpty, "query")
                .fault()
                .limit_evidence()
                .is_none(),
            "an empty query meets no bound, so it reports none"
        );
    }

    #[test]
    fn test_a_count_widens_into_the_domain_the_evidence_carries() {
        assert_eq!(count(0), 0);
        assert_eq!(count(4_096), 4_096);
    }
}
