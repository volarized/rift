use std::fmt;

/// Stable machine-readable error identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorName {
    /// Request exceeded declared resource limit.
    LimitExceeded,
    /// Workspace configuration failed validation.
    InvalidConfiguration,
    /// Requested source unit does not exist in snapshot.
    SourceUnitNotFound,
    /// Published index cannot serve requested snapshot.
    IndexUnavailable,
    /// Model artifacts are unavailable or invalid.
    ModelUnavailable,
    /// Server instance cannot accept work.
    ServerUnavailable,
}

/// Retry guidance shared by every transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// Retrying unchanged request cannot succeed.
    Never,
    /// Retry may succeed after operator or background state changes.
    AfterStateChange,
    /// Retry may succeed after a bounded delay.
    AfterDelay,
}

/// Canonical user-facing error metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDescriptor {
    name: ErrorName,
    code: u16,
    explanation: &'static str,
    retry: RetryPolicy,
    action: &'static str,
}

impl ErrorDescriptor {
    crate::property! {
        /// Returns stable symbolic identity.
        pub const fn name(self) -> ErrorName = self.name;
    }

    crate::property! {
        /// Returns stable numeric code.
        pub const fn code(self) -> u16 = self.code;
    }

    crate::property! {
        /// Returns canonical explanation.
        pub const fn explanation(self) -> &'static str = self.explanation;
    }

    crate::property! {
        /// Returns retry guidance.
        pub const fn retry(self) -> RetryPolicy = self.retry;
    }

    crate::property! {
        /// Returns canonical suggested action.
        pub const fn action(self) -> &'static str = self.action;
    }
}

/// Stable registry mapping error identities to canonical metadata.
#[derive(Debug, Clone, Copy)]
pub struct ErrorRegistry;

impl ErrorRegistry {
    const ENTRIES: &'static [ErrorDescriptor] = &[
        Self::descriptor(ErrorName::LimitExceeded),
        Self::descriptor(ErrorName::InvalidConfiguration),
        Self::descriptor(ErrorName::SourceUnitNotFound),
        Self::descriptor(ErrorName::IndexUnavailable),
        Self::descriptor(ErrorName::ModelUnavailable),
        Self::descriptor(ErrorName::ServerUnavailable),
    ];

    /// Returns complete stable registry.
    #[must_use]
    pub const fn entries() -> &'static [ErrorDescriptor] {
        Self::ENTRIES
    }

    /// Returns canonical metadata for one error identity.
    #[must_use]
    pub const fn descriptor(name: ErrorName) -> ErrorDescriptor {
        match name {
            ErrorName::LimitExceeded => ErrorDescriptor {
                name,
                code: 1_001,
                explanation: "request exceeded a declared resource limit",
                retry: RetryPolicy::Never,
                action: "narrow request scope or raise configured limit",
            },
            ErrorName::InvalidConfiguration => ErrorDescriptor {
                name,
                code: 1_002,
                explanation: "workspace configuration is invalid",
                retry: RetryPolicy::AfterStateChange,
                action: "correct reported configuration fields and retry",
            },
            ErrorName::SourceUnitNotFound => ErrorDescriptor {
                name,
                code: 2_001,
                explanation: "source unit does not exist in request snapshot",
                retry: RetryPolicy::AfterStateChange,
                action: "refresh workspace state and use a current source unit identity",
            },
            ErrorName::IndexUnavailable => ErrorDescriptor {
                name,
                code: 3_001,
                explanation: "published index cannot serve request snapshot",
                retry: RetryPolicy::AfterDelay,
                action: "wait for index publication or inspect workspace index state",
            },
            ErrorName::ModelUnavailable => ErrorDescriptor {
                name,
                code: 4_001,
                explanation: "embedding model is unavailable or invalid",
                retry: RetryPolicy::AfterStateChange,
                action: "verify model source, revision, artifacts, and credentials",
            },
            ErrorName::ServerUnavailable => ErrorDescriptor {
                name,
                code: 5_001,
                explanation: "workspace server cannot accept work",
                retry: RetryPolicy::AfterDelay,
                action: "retry after server recovery or run rift doctor",
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

    crate::property! {
        /// Returns stable context key.
        pub const fn key(&self) -> &'static str = self.key;
    }

    crate::property! {
        /// Returns context value.
        pub fn value(&self) -> &str = self.value.as_str();
    }
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

    crate::property! {
        /// Returns canonical descriptor.
        pub const fn descriptor(&self) -> ErrorDescriptor = self.descriptor;
    }

    crate::property! {
        /// Returns ordered typed context.
        pub fn context(&self) -> &[ErrorContext] = self.context.as_slice();
    }
}

impl fmt::Display for RiftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.descriptor.explanation())
    }
}

impl std::error::Error for RiftError {}

#[cfg(test)]
mod tests {
    use super::{ErrorContext, ErrorName, ErrorRegistry, RiftError};
    use std::collections::HashSet;

    #[test]
    fn registry_fields_are_complete_and_unique() {
        let mut names = HashSet::new();
        let mut codes = HashSet::new();

        for descriptor in ErrorRegistry::entries() {
            assert!(names.insert(descriptor.name()));
            assert!(codes.insert(descriptor.code()));
            assert!(!descriptor.explanation().is_empty());
            assert!(!descriptor.action().is_empty());
            let _retry = descriptor.retry();
        }
    }

    #[test]
    fn error_reuses_registry_text_and_retains_context() {
        let error = RiftError::new(ErrorName::SourceUnitNotFound)
            .with_context(ErrorContext::new("source_unit", "resolver://missing"));

        assert_eq!(error.to_string(), error.descriptor().explanation());
        assert_eq!(error.context()[0].key(), "source_unit");
        assert_eq!(error.context()[0].value(), "resolver://missing");
    }

    #[test]
    fn every_error_name_resolves_to_matching_descriptor() {
        for name in [
            ErrorName::LimitExceeded,
            ErrorName::InvalidConfiguration,
            ErrorName::SourceUnitNotFound,
            ErrorName::IndexUnavailable,
            ErrorName::ModelUnavailable,
            ErrorName::ServerUnavailable,
        ] {
            assert_eq!(RiftError::new(name).descriptor().name(), name);
        }
    }
}
