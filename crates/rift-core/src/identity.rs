use std::fmt;
use std::num::NonZeroU64;

/// Invalid stable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdError;

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("identity must be non-empty and contain no control characters")
    }
}

impl std::error::Error for IdError {}

macro_rules! define_id {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates and constructs identity.
            ///
            /// # Errors
            ///
            /// Returns [`IdError`] when value is empty or contains a control character.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.is_empty() || value.chars().any(char::is_control) {
                    return Err(IdError);
                }
                Ok(Self(value))
            }

            /// Returns canonical identity text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

define_id!(WorkspaceId, "Canonical workspace identity.");
define_id!(SourceUnitId, "Resolver-owned source unit identity.");
define_id!(SymbolId, "Language-qualified symbol identity.");
define_id!(ProviderId, "Provider component identity.");
define_id!(CompositionId, "Provider composition identity.");
define_id!(ModelId, "Resolved embedding model identity.");
define_id!(CursorId, "Opaque search cursor identity.");

/// Invalid zero revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionError;

impl fmt::Display for RevisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("revision must be greater than zero")
    }
}

impl std::error::Error for RevisionError {}

macro_rules! define_revision {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Constructs a non-zero revision.
            ///
            /// # Errors
            ///
            /// Returns [`RevisionError`] for zero.
            pub fn new(value: u64) -> Result<Self, RevisionError> {
                NonZeroU64::new(value).map(Self).ok_or(RevisionError)
            }

            /// Returns revision number.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

define_revision!(TreeRevision, "Resolved project tree revision.");
define_revision!(SourceRevision, "Source catalog revision.");
define_revision!(ProviderRevision, "Provider fact revision.");
define_revision!(CompositionRevision, "Provider composition revision.");
define_revision!(IndexRevision, "Published index revision.");
define_revision!(ModelRevision, "Resolved model revision.");

#[cfg(test)]
mod tests {
    use super::{
        CompositionId, CompositionRevision, CursorId, IndexRevision, ModelId, ModelRevision,
        ProviderId, ProviderRevision, SourceRevision, SourceUnitId, SymbolId, TreeRevision,
        WorkspaceId,
    };

    #[test]
    fn identities_reject_ambiguous_values() {
        assert!(SourceUnitId::new("").is_err());
        assert!(SourceUnitId::new("src\nunit").is_err());
        assert_eq!(
            SourceUnitId::new("resolver://src/lib.rs").map(|id| id.to_string()),
            Ok(String::from("resolver://src/lib.rs"))
        );
    }

    #[test]
    fn revisions_are_non_zero() {
        assert!(TreeRevision::new(0).is_err());
        assert_eq!(TreeRevision::new(7).map(TreeRevision::get), Ok(7));
    }

    #[test]
    fn every_identity_family_validates_and_displays() {
        assert_eq!(
            WorkspaceId::new("workspace").map(|id| id.to_string()),
            Ok("workspace".into())
        );
        assert_eq!(
            SymbolId::new("python:rift.main").map(|id| id.to_string()),
            Ok("python:rift.main".into())
        );
        assert_eq!(
            ProviderId::new("syntax").map(|id| id.to_string()),
            Ok("syntax".into())
        );
        assert_eq!(
            CompositionId::new("default").map(|id| id.to_string()),
            Ok("default".into())
        );
        assert_eq!(
            ModelId::new("owner/model@revision").map(|id| id.to_string()),
            Ok("owner/model@revision".into())
        );
        assert_eq!(
            CursorId::new("cursor").map(|id| id.to_string()),
            Ok("cursor".into())
        );
    }

    #[test]
    fn every_revision_family_preserves_value() {
        assert_eq!(SourceRevision::new(1).map(SourceRevision::get), Ok(1));
        assert_eq!(ProviderRevision::new(2).map(ProviderRevision::get), Ok(2));
        assert_eq!(
            CompositionRevision::new(3).map(CompositionRevision::get),
            Ok(3)
        );
        assert_eq!(IndexRevision::new(4).map(IndexRevision::get), Ok(4));
        assert_eq!(ModelRevision::new(5).map(ModelRevision::get), Ok(5));
    }
}
