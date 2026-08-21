//! Registry conformance for the configuration model's fault types.
//!
//! The `rift.toml` model lives in `rift-protocol`, below this crate, so its
//! fault types cannot implement [`Fault`] where they are defined. This module
//! implements the registry trait over them, giving every configuration
//! refusal the registry's identity, explanation, and rendering.

use rift_protocol::configuration::{ConfigurationViolation, UnitParseError};

use crate::error::{ErrorContext, ErrorName, Fault, fault_label};
use rift_protocol::error::ErrorCode;

impl Fault for UnitParseError {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("value", self.value()),
            ErrorContext::new("expected", self.expected()),
        ]
    }
}

impl Fault for ConfigurationViolation {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(self))];
        context.extend(
            self.evidence()
                .into_iter()
                .map(|(key, value)| ErrorContext::new(key, value)),
        );
        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use rift_protocol::configuration::{ByteSize, Duration};

    #[test]
    fn test_unit_parse_failure_renders_through_the_registry() {
        let fault = ByteSize::parse("16KiB").expect_err("an uppercase unit must be refused");
        let error = Error::from(fault);
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("the workspace configuration failed validation")
                && message.contains("value 16KiB")
                && message.contains("16kb")
                && message.contains("correct the reported configuration field"),
            "the render must carry explanation, evidence, and action: {message}"
        );
    }

    #[test]
    fn test_configuration_violation_renders_through_the_registry() {
        let violation = ConfigurationViolation::HookExecutableAbsolute {
            id: "tests".to_owned(),
            program: "/bin/cargo".to_owned(),
        };
        let error = Error::from(violation);
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let message = error.to_string();
        assert!(
            message.contains("violation hook_executable_absolute")
                && message.contains("id tests")
                && message.contains("program /bin/cargo")
                && message.contains("correct the reported configuration field"),
            "the render must carry the serde label, the evidence, and the action: {message}"
        );
    }

    #[test]
    fn test_duration_parse_failure_carries_its_own_expected_form() {
        let fault = Duration::parse("30 s").expect_err("an inner space must be refused");
        let context = fault.context();
        let keys: Vec<&str> = context.iter().map(ErrorContext::key).collect();
        assert_eq!(keys, ["value", "expected"]);
        let expected = context[1].value();
        assert!(
            expected.contains("30s"),
            "the expected form must name 30s: {expected}"
        );
    }
}
