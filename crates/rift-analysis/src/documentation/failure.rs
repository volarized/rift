//! Documentation input and publication refusals.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

/// The invariant a documentation input or publication violates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationViolation {
    /// A selected source exceeds its count or byte bound.
    LimitExceeded,
    /// A source address is empty or not canonical.
    Identity,
    /// More than one source names the same content owner.
    DuplicateSource,
    /// Recorded digest differs from the supplied bytes.
    Digest,
    /// A source's package or location conflicts with its address.
    Origin,
    /// Format, extension, and media type do not agree.
    Format,
    /// A metadata range does not address its source bytes.
    Range,
    /// A collection is not in its required order.
    Order,
    /// A metadata record names a missing source, block, or declaration.
    MissingTarget,
    /// A notebook identity or selected source shape is invalid.
    Notebook,
    /// Publication revision differs from the supported revision.
    Revision,
    /// Canonical JSON encoding failed.
    Encoding,
}

/// One typed documentation refusal with its bounded field name.
#[derive(Debug)]
pub struct DocumentationFault {
    violation: DocumentationViolation,
    field: &'static str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl DocumentationFault {
    pub(super) const fn new(violation: DocumentationViolation, field: &'static str) -> Self {
        Self {
            violation,
            field,
            source: None,
        }
    }

    pub(super) fn caused_by(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Returns the violated documentation rule.
    #[must_use]
    pub const fn violation(&self) -> DocumentationViolation {
        self.violation
    }

    /// Returns the field the caller must correct.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl Fault for DocumentationFault {
    fn name(&self) -> ErrorName {
        let code = match self.violation {
            DocumentationViolation::LimitExceeded => ErrorCode::LimitExceeded,
            DocumentationViolation::MissingTarget => ErrorCode::ResourceNotFound,
            _ => ErrorCode::InvalidRequest,
        };
        ErrorName::Wire(code)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("violation", fault_label(&self.violation)),
            ErrorContext::new("field", self.field),
        ]
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// A documentation input or publication refused at the shared boundary.
pub type DocumentationError = Error<DocumentationFault>;

pub(super) fn refused(
    violation: DocumentationViolation,
    field: &'static str,
) -> DocumentationError {
    DocumentationFault::new(violation, field).into()
}

#[cfg(test)]
mod tests {
    use rift_core::{ErrorCode, ErrorName, Fault};

    use super::{DocumentationFault, DocumentationViolation};

    #[test]
    fn documentation_fault_maps_wire_codes_and_preserves_cause() {
        for (violation, expected) in [
            (
                DocumentationViolation::LimitExceeded,
                ErrorCode::LimitExceeded,
            ),
            (
                DocumentationViolation::MissingTarget,
                ErrorCode::ResourceNotFound,
            ),
            (DocumentationViolation::Digest, ErrorCode::InvalidRequest),
        ] {
            let fault = DocumentationFault::new(violation, "source")
                .caused_by(std::io::Error::other("fixture cause"));
            assert_eq!(fault.name(), ErrorName::Wire(expected));
            assert_eq!(fault.context()[1].value(), "source");
            assert_eq!(
                Fault::source(&fault).expect("cause retained").to_string(),
                "fixture cause"
            );
        }
    }
}
