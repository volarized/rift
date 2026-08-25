//! Registry identity for the search tier's failures.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

/// What the search tier refused, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchViolation {
    /// A model directory does not hold one of the three files an encoder loads.
    ModelFileMissing,
    /// A model's `config.json` is not a BERT configuration this encoder serves.
    ModelConfigurationInvalid,
    /// A model's `tokenizer.json` could not be read.
    TokenizerUnreadable,
    /// A model's weights could not be read, or the architecture refused them.
    WeightsUnreadable,
    /// The encoder's forward pass failed.
    EncodeFailed,
    /// One call handed the encoder more texts than its bound allows.
    TextLimit,
}

impl SearchViolation {
    /// The registry identity this violation classifies as.
    const fn name(self) -> ErrorName {
        match self {
            Self::ModelFileMissing => ErrorName::Wire(ErrorCode::ResourceNotFound),
            Self::ModelConfigurationInvalid
            | Self::TokenizerUnreadable
            | Self::WeightsUnreadable => ErrorName::Wire(ErrorCode::ConfigurationInvalid),
            Self::EncodeFailed => ErrorName::Wire(ErrorCode::InternalError),
            Self::TextLimit => ErrorName::Wire(ErrorCode::LimitExceeded),
        }
    }
}

/// One search-tier failure, with the evidence its violation carries.
#[derive(Debug)]
pub struct SearchFault {
    violation: SearchViolation,
    subject: Option<String>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl SearchFault {
    /// Builds a failure carrying its violation alone.
    #[must_use]
    pub const fn new(violation: SearchViolation) -> Self {
        Self {
            violation,
            subject: None,
            source: None,
        }
    }

    /// Names what the failure was about: a file path, a tensor, a model.
    #[must_use]
    pub fn about(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Attaches the underlying failure this one wraps.
    #[must_use]
    pub fn caused_by(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The violation this failure reports.
    #[must_use]
    pub const fn violation(&self) -> SearchViolation {
        self.violation
    }
}

impl Fault for SearchFault {
    fn name(&self) -> ErrorName {
        self.violation.name()
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(subject) = &self.subject {
            context.push(ErrorContext::new("subject", subject.clone()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Opaque search-tier failure.
pub type SearchError = Error<SearchFault>;

#[cfg(test)]
mod tests {
    use super::{SearchError, SearchFault, SearchViolation};
    use rift_core::{ErrorCode, ErrorName, Fault};

    #[test]
    fn test_every_violation_classifies_and_labels_itself() {
        let cases = [
            (
                SearchViolation::ModelFileMissing,
                ErrorCode::ResourceNotFound,
                "model_file_missing",
            ),
            (
                SearchViolation::ModelConfigurationInvalid,
                ErrorCode::ConfigurationInvalid,
                "model_configuration_invalid",
            ),
            (
                SearchViolation::TokenizerUnreadable,
                ErrorCode::ConfigurationInvalid,
                "tokenizer_unreadable",
            ),
            (
                SearchViolation::WeightsUnreadable,
                ErrorCode::ConfigurationInvalid,
                "weights_unreadable",
            ),
            (
                SearchViolation::EncodeFailed,
                ErrorCode::InternalError,
                "encode_failed",
            ),
            (
                SearchViolation::TextLimit,
                ErrorCode::LimitExceeded,
                "text_limit",
            ),
        ];
        for (violation, code, label) in cases {
            let fault = SearchFault::new(violation);
            assert_eq!(fault.name(), ErrorName::Wire(code));
            assert_eq!(
                fault
                    .context()
                    .first()
                    .map(|entry| entry.value().to_owned()),
                Some(label.to_owned()),
                "{violation:?} must label itself from serde"
            );
        }
    }

    #[test]
    fn test_subject_and_source_ride_the_failure() {
        let cause = std::io::Error::other("weights truncated");
        let fault = SearchFault::new(SearchViolation::WeightsUnreadable)
            .about("models/bge-small/model.safetensors")
            .caused_by(cause);
        assert_eq!(fault.violation(), SearchViolation::WeightsUnreadable);
        let context = fault.context();
        assert_eq!(context.len(), 2);
        assert_eq!(context[1].value(), "models/bge-small/model.safetensors");
        let error = SearchError::new(fault);
        assert!(std::error::Error::source(&error).is_some());
        assert!(
            error.to_string().contains("weights_unreadable"),
            "the rendered failure names its violation: {error}"
        );
    }

    #[test]
    fn test_failure_without_a_subject_carries_its_violation_alone() {
        let fault = SearchFault::new(SearchViolation::TextLimit);
        assert_eq!(fault.context().len(), 1);
        assert!(Fault::source(&fault).is_none());
    }
}
