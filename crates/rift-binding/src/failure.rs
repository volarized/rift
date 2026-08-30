//! Binding refusals: the violation catalog and its registry carrier.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

use crate::limits::ExhaustedLimit;

/// One rule a binding phase found broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingViolation {
    /// A scope id names no scope in the graph under construction.
    MissingScope,
    /// A unit id names no unit in the graph under construction.
    MissingUnit,
    /// A definition id names no definition in the graph under construction.
    MissingDefinition,
    /// Scope parents form a cycle.
    ScopeCycle,
    /// A scope's parent lies in another unit.
    ScopeUnitMismatch,
    /// A member link names a scope that is not a member scope.
    MemberScopeKind,
    /// One bound is zero.
    ZeroLimit(ExhaustedLimit),
    /// One unit exceeds a per-unit bound.
    UnitLimit(ExhaustedLimit),
    /// The graph exceeds a whole-graph bound.
    GraphLimit(ExhaustedLimit),
    /// Resolution exhausted the publication's work bound.
    PublicationWork(ExhaustedLimit),
    /// The caller cancelled resolution.
    Cancelled,
    /// A name is empty or longer than `NAME_BYTES_MAX` bytes.
    InvalidName,
    /// A name path is empty or longer than `NAME_PATH_SEGMENTS_MAX` segments.
    InvalidPath,
    /// A binding fact does not form a valid Contribution.
    InvalidContribution,
    /// The binding Contributions do not form a valid publication.
    InvalidPublication,
}

impl BindingViolation {
    /// The bound this violation names, where it names one.
    #[must_use]
    pub const fn limit(self) -> Option<ExhaustedLimit> {
        match self {
            Self::ZeroLimit(limit)
            | Self::UnitLimit(limit)
            | Self::GraphLimit(limit)
            | Self::PublicationWork(limit) => Some(limit),
            _ => None,
        }
    }

    const fn code(self) -> ErrorCode {
        match self {
            Self::UnitLimit(_) | Self::GraphLimit(_) | Self::PublicationWork(_) => {
                ErrorCode::LimitExceeded
            }
            Self::Cancelled => ErrorCode::Cancelled,
            _ => ErrorCode::InvalidRequest,
        }
    }
}

/// A binding violation with the evidence the failing phase had at hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingFault {
    violation: BindingViolation,
    detail: String,
}

impl BindingFault {
    /// Pairs one violation with its call-site evidence.
    #[must_use]
    pub fn new(violation: BindingViolation, detail: impl Into<String>) -> Self {
        Self {
            violation,
            detail: detail.into(),
        }
    }

    /// Returns the broken rule.
    #[must_use]
    pub const fn violation(&self) -> BindingViolation {
        self.violation
    }

    /// Returns the evidence recorded with the refusal.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl Fault for BindingFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(self.violation.code())
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(limit) = self.violation.limit() {
            context.push(ErrorContext::new("limit", fault_label(&limit)));
        }
        if !self.detail.is_empty() {
            context.push(ErrorContext::new("detail", self.detail.clone()));
        }
        context
    }
}

/// A binding refusal on the registry carrier.
pub type BindingError = Error<BindingFault>;

pub(crate) fn binding_error(
    violation: BindingViolation,
    detail: impl Into<String>,
) -> BindingError {
    Error::new(BindingFault::new(violation, detail))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use rift_core::{ErrorCode, ErrorName, Fault, fault_label};

    use super::{BindingFault, BindingViolation, binding_error};
    use crate::limits::ExhaustedLimit;

    const VIOLATIONS: [BindingViolation; 16] = [
        BindingViolation::MissingScope,
        BindingViolation::MissingUnit,
        BindingViolation::MissingDefinition,
        BindingViolation::ScopeCycle,
        BindingViolation::ScopeUnitMismatch,
        BindingViolation::MemberScopeKind,
        BindingViolation::ZeroLimit(ExhaustedLimit::UnitScopes),
        BindingViolation::UnitLimit(ExhaustedLimit::UnitDefinitions),
        BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes),
        BindingViolation::PublicationWork(ExhaustedLimit::PublicationWork),
        BindingViolation::Cancelled,
        BindingViolation::InvalidName,
        BindingViolation::InvalidPath,
        BindingViolation::InvalidContribution,
        BindingViolation::InvalidPublication,
        BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks),
    ];

    #[test]
    fn test_binding_fault_every_violation_renders_label_and_display() {
        for violation in VIOLATIONS {
            let error = binding_error(violation, "probe");
            let context = error.context();
            assert_eq!(context[0].key(), "violation");
            assert_eq!(context[0].value(), fault_label(&violation));
            let rendered = error.to_string();
            assert!(!rendered.is_empty(), "display renders for {violation:?}");
            assert!(
                rendered.contains("detail probe"),
                "detail rides along: {rendered}"
            );
            assert!(error.source().is_none(), "no violation wraps a source");
        }
    }

    #[test]
    fn test_binding_fault_limit_violations_classify_limit_exceeded_with_limit_context() {
        let error = binding_error(
            BindingViolation::UnitLimit(ExhaustedLimit::UnitScopes),
            String::new(),
        );
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
        let context = error.context();
        assert_eq!(context.len(), 2, "violation and limit, no empty detail");
        assert_eq!(context[1].key(), "limit");
        assert_eq!(context[1].value(), "unit_scopes");
        assert_eq!(fault_label(&error.fault().violation()), "unit_limit");
    }

    #[test]
    fn test_binding_fault_cancelled_classifies_cancelled() {
        let fault = BindingFault::new(BindingViolation::Cancelled, "after 64 items");
        assert_eq!(fault.name(), ErrorName::Wire(ErrorCode::Cancelled));
        assert_eq!(fault.detail(), "after 64 items");
        assert_eq!(fault.violation().limit(), None);
    }

    #[test]
    fn test_binding_fault_structural_violations_classify_invalid_request() {
        let fault = BindingFault::new(BindingViolation::ScopeCycle, "");
        assert_eq!(fault.name(), ErrorName::Wire(ErrorCode::InvalidRequest));
        let zero = BindingViolation::ZeroLimit(ExhaustedLimit::PathDepth);
        assert_eq!(
            BindingFault::new(zero, "").name(),
            ErrorName::Wire(ErrorCode::InvalidRequest)
        );
        assert_eq!(zero.limit(), Some(ExhaustedLimit::PathDepth));
    }
}
