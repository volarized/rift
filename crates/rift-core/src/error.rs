use std::fmt;

/// Stable machine-readable error identity.
///
/// Every operating failure Rift raises to a user or agent resolves to one of
/// these names. The wire error surface serializes the matching
/// [`ErrorDescriptor::code`] string; the registry is the one place a code,
/// its explanation, and its default retry guidance are defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorName {
    /// Request does not match the documented form.
    InvalidRequest,
    /// Request addresses state outside what the workspace allows.
    PermissionDenied,
    /// Requested resource does not exist in the current snapshot.
    ResourceNotFound,
    /// Addressed content exists but its bytes cannot be served.
    ContentUnavailable,
    /// Cursor does not continue the request it was sent with.
    CursorInvalid,
    /// Cursor's captured results are no longer retained.
    CursorExpired,
    /// Request was cancelled before it completed.
    Cancelled,
    /// Request exceeded a declared resource limit.
    LimitExceeded,
    /// Workspace state could not be read or written.
    StorageFailure,
    /// Server failed in a way it did not classify.
    InternalError,
    /// Path cannot be addressed by this workspace.
    UnsupportedPath,
    /// Server cannot serve the request yet.
    TemporarilyUnavailable,
    /// Workspace configuration failed validation.
    ConfigurationInvalid,
    /// No configured provider serves the request.
    CapabilityUnavailable,
}

impl ErrorName {
    /// Every registered identity, in registry order.
    pub const ALL: [Self; 14] = [
        Self::InvalidRequest,
        Self::PermissionDenied,
        Self::ResourceNotFound,
        Self::ContentUnavailable,
        Self::CursorInvalid,
        Self::CursorExpired,
        Self::Cancelled,
        Self::LimitExceeded,
        Self::StorageFailure,
        Self::InternalError,
        Self::UnsupportedPath,
        Self::TemporarilyUnavailable,
        Self::ConfigurationInvalid,
        Self::CapabilityUnavailable,
    ];
}

/// Default retry guidance a descriptor carries.
///
/// The vocabulary matches the wire `retry` directive. A surface may override
/// the default per failure instance, because the same identity can be
/// transient once and permanent the next time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDirective {
    /// The same bytes will fail the same way.
    Never,
    /// The failure was transient; the same request may succeed shortly.
    SameRequest,
    /// The workspace requires a local state or configuration change first.
    OperatorAction,
}

/// Canonical user-facing error metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDescriptor {
    name: ErrorName,
    code: &'static str,
    explanation: &'static str,
    retry: RetryDirective,
    action: &'static str,
}

impl ErrorDescriptor {
    /// Returns stable symbolic identity.
    #[must_use]
    pub const fn name(self) -> ErrorName {
        self.name
    }

    /// Returns the stable wire code, unique across the registry.
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }

    /// Returns canonical explanation.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        self.explanation
    }

    /// Returns default retry guidance.
    #[must_use]
    pub const fn retry(self) -> RetryDirective {
        self.retry
    }

    /// Returns canonical suggested action.
    #[must_use]
    pub const fn action(self) -> &'static str {
        self.action
    }
}

/// Stable registry mapping error identities to canonical metadata.
#[derive(Debug, Clone, Copy)]
pub struct ErrorRegistry;

impl ErrorRegistry {
    /// Returns the complete stable registry, in [`ErrorName::ALL`] order.
    #[must_use]
    pub const fn entries() -> [ErrorDescriptor; ErrorName::ALL.len()] {
        let mut entries = [Self::descriptor(ErrorName::InvalidRequest); ErrorName::ALL.len()];
        let mut index = 0;
        while index < ErrorName::ALL.len() {
            entries[index] = Self::descriptor(ErrorName::ALL[index]);
            index += 1;
        }
        entries
    }

    /// Returns canonical metadata for one error identity.
    #[must_use]
    pub const fn descriptor(name: ErrorName) -> ErrorDescriptor {
        match name {
            ErrorName::InvalidRequest => ErrorDescriptor {
                name,
                code: "invalid_request",
                explanation: "the request does not match the documented form",
                retry: RetryDirective::Never,
                action: "correct the reported field and resend the request",
            },
            ErrorName::PermissionDenied => ErrorDescriptor {
                name,
                code: "permission_denied",
                explanation: "the request addresses state outside what the workspace allows",
                retry: RetryDirective::Never,
                action: "address paths inside the workspace root, without symlink components",
            },
            ErrorName::ResourceNotFound => ErrorDescriptor {
                name,
                code: "resource_not_found",
                explanation: "the requested resource does not exist in the current snapshot",
                retry: RetryDirective::OperatorAction,
                action: "search or list first, then retry with an identity that answer returned",
            },
            ErrorName::ContentUnavailable => ErrorDescriptor {
                name,
                code: "content_unavailable",
                explanation: "the addressed content exists but its bytes cannot be served",
                retry: RetryDirective::Never,
                action: "request the declaration without its body, or read a source-backed unit",
            },
            ErrorName::CursorInvalid => ErrorDescriptor {
                name,
                code: "cursor_invalid",
                explanation: "the cursor does not continue the request it was sent with",
                retry: RetryDirective::Never,
                action: "page with the cursor exactly as returned, on the request that minted it",
            },
            ErrorName::CursorExpired => ErrorDescriptor {
                name,
                code: "cursor_expired",
                explanation: "the cursor's captured results are no longer retained",
                retry: RetryDirective::Never,
                action: "restart the request from its first page",
            },
            ErrorName::Cancelled => ErrorDescriptor {
                name,
                code: "cancelled",
                explanation: "the request was cancelled before it completed",
                retry: RetryDirective::SameRequest,
                action: "resend the request if the result is still needed",
            },
            ErrorName::LimitExceeded => ErrorDescriptor {
                name,
                code: "limit_exceeded",
                explanation: "the request exceeded a declared resource limit",
                retry: RetryDirective::Never,
                action: "resize the request below the named limit, or raise that limit in the workspace configuration",
            },
            ErrorName::StorageFailure => ErrorDescriptor {
                name,
                code: "storage_failure",
                explanation: "workspace state could not be read or written",
                retry: RetryDirective::SameRequest,
                action: "check filesystem permissions and free space, then retry",
            },
            ErrorName::InternalError => ErrorDescriptor {
                name,
                code: "internal_error",
                explanation: "the server failed in a way it did not classify",
                retry: RetryDirective::SameRequest,
                action: "retry once, and report the full message if the failure repeats",
            },
            ErrorName::UnsupportedPath => ErrorDescriptor {
                name,
                code: "unsupported_path",
                explanation: "the path cannot be addressed by this workspace",
                retry: RetryDirective::Never,
                action: "use a workspace-relative path with `/` separators and no `.` or `..` components",
            },
            ErrorName::TemporarilyUnavailable => ErrorDescriptor {
                name,
                code: "temporarily_unavailable",
                explanation: "the server cannot serve this request yet",
                retry: RetryDirective::SameRequest,
                action: "resend the same request after a short delay",
            },
            ErrorName::ConfigurationInvalid => ErrorDescriptor {
                name,
                code: "configuration_invalid",
                explanation: "the workspace configuration failed validation",
                retry: RetryDirective::OperatorAction,
                action: "correct the reported configuration field, then retry",
            },
            ErrorName::CapabilityUnavailable => ErrorDescriptor {
                name,
                code: "capability_unavailable",
                explanation: "no configured provider serves this request",
                retry: RetryDirective::OperatorAction,
                action: "adjust the request to a served capability, or configure a provider that serves it",
            },
        }
    }
}

/// Typed detail attached to one operating failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorContext {
    key: &'static str,
    value: String,
}

impl ErrorContext {
    /// Constructs context with stable key and display value.
    #[must_use]
    pub fn new(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: value.into(),
        }
    }

    /// Returns stable context key.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// Returns context value.
    #[must_use]
    pub fn value(&self) -> &str {
        self.value.as_str()
    }
}

/// Renders one failure line: explanation, named context, then the action.
///
/// Every surface that shows an operating failure to a person prints this
/// form, so a message always says what failed, with which values, and what
/// to do next: `explanation: key value, key value; action`.
#[must_use]
pub fn render_failure(descriptor: ErrorDescriptor, context: &[ErrorContext]) -> String {
    let mut rendered = String::from(descriptor.explanation());
    let mut entries = context.iter();
    if let Some(first) = entries.next() {
        rendered.push_str(": ");
        rendered.push_str(first.key());
        rendered.push(' ');
        rendered.push_str(first.value());
        for entry in entries {
            rendered.push_str(", ");
            rendered.push_str(entry.key());
            rendered.push(' ');
            rendered.push_str(entry.value());
        }
    }
    rendered.push_str("; ");
    rendered.push_str(descriptor.action());
    rendered
}

/// Opaque operating failure referencing canonical registry metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiftError {
    descriptor: ErrorDescriptor,
    context: Vec<ErrorContext>,
}

impl RiftError {
    /// Constructs error for registered name.
    #[must_use]
    pub fn new(name: ErrorName) -> Self {
        Self {
            descriptor: ErrorRegistry::descriptor(name),
            context: Vec::new(),
        }
    }

    /// Attaches typed failure context.
    #[must_use]
    pub fn with_context(mut self, context: ErrorContext) -> Self {
        self.context.push(context);
        self
    }

    /// Returns canonical descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> ErrorDescriptor {
        self.descriptor
    }

    /// Returns ordered typed context.
    #[must_use]
    pub fn context(&self) -> &[ErrorContext] {
        self.context.as_slice()
    }
}

impl fmt::Display for RiftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&render_failure(self.descriptor, &self.context))
    }
}

impl std::error::Error for RiftError {}

#[cfg(test)]
mod tests {
    use super::{ErrorContext, ErrorName, ErrorRegistry, RiftError, render_failure};
    use std::collections::HashSet;

    /// The code a variant must carry: its name, one word per case break,
    /// joined by underscores.
    fn snake_case(name: &str) -> String {
        let mut rendered = String::new();
        for (index, character) in name.char_indices() {
            if character.is_ascii_uppercase() && index > 0 {
                rendered.push('_');
            }
            rendered.push(character.to_ascii_lowercase());
        }
        rendered
    }

    #[test]
    fn registry_codes_are_unique_snake_case_variant_names() {
        let mut codes = HashSet::new();
        for name in ErrorName::ALL {
            let descriptor = ErrorRegistry::descriptor(name);
            assert!(
                codes.insert(descriptor.code()),
                "registry code must be unique: {code} appears twice",
                code = descriptor.code()
            );
            assert_eq!(
                descriptor.code(),
                snake_case(&format!("{name:?}")),
                "registry code must be the snake_case variant name so the wire \
                 and the registry cannot drift"
            );
        }
    }

    #[test]
    fn registry_entries_cover_every_name_in_order() {
        let entries = ErrorRegistry::entries();
        assert_eq!(entries.len(), ErrorName::ALL.len());
        for (entry, name) in entries.iter().zip(ErrorName::ALL) {
            assert_eq!(entry.name(), name);
        }
    }

    #[test]
    fn every_descriptor_explains_and_directs() {
        for name in ErrorName::ALL {
            let descriptor = ErrorRegistry::descriptor(name);
            assert!(
                !descriptor.explanation().is_empty() && !descriptor.action().is_empty(),
                "descriptor for {name:?} must carry an explanation and an action"
            );
            assert!(
                descriptor
                    .explanation()
                    .chars()
                    .next()
                    .is_some_and(char::is_lowercase)
                    && !descriptor.explanation().ends_with('.'),
                "explanation for {name:?} must be lowercase without trailing punctuation"
            );
        }
    }

    #[test]
    fn rendered_failure_names_values_and_action() {
        let error = RiftError::new(ErrorName::LimitExceeded)
            .with_context(ErrorContext::new("limit", "search.results_max 100"))
            .with_context(ErrorContext::new("required", "250"));
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: \
             limit search.results_max 100, required 250; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
    }

    #[test]
    fn rendered_failure_without_context_keeps_explanation_and_action() {
        let rendered = render_failure(ErrorRegistry::descriptor(ErrorName::CursorExpired), &[]);
        assert_eq!(
            rendered,
            "the cursor's captured results are no longer retained; \
             restart the request from its first page"
        );
    }
}
