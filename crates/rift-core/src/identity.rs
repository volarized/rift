use std::fmt::{self, Write as _};
use std::num::NonZeroU64;
use std::str::FromStr;

use crate::constants::{
    HEX_LETTER_VALUE_OFFSET, HEX_NIBBLE_BITS, PERCENT_ESCAPE_BYTES, PERCENT_ESCAPE_HIGH_OFFSET,
    PERCENT_ESCAPE_LOW_OFFSET, PERCENT_ESCAPE_MARKER, SOURCE_RESOLVER_ID_BYTES_MAX,
    SOURCE_RESOLVER_PUNCTUATION, SOURCE_UNIT_ID_BYTES_MAX, SOURCE_UNIT_SAFE_PUNCTUATION,
    SOURCE_UNIT_SEPARATOR, SOURCE_UNIT_SEPARATOR_BYTES, SOURCE_UNIT_URI_PREFIX,
};
use crate::{ErrorCode, ErrorDescriptor, ErrorName, PathError, SourcePath};

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
define_id!(SymbolId, "Language-qualified symbol identity.");
define_id!(ProviderId, "Provider component identity.");
define_id!(CompositionId, "Provider composition identity.");
define_id!(ModelId, "Resolved embedding model identity.");
define_id!(CursorId, "Opaque search cursor identity.");

/// Violated source-resolver identity rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceResolverIdViolation {
    /// Resolver identity is empty.
    Empty,
    /// Resolver identity exceeds 128 ASCII bytes.
    TooLong,
    /// Resolver identity is not canonical lowercase syntax.
    InvalidCharacter,
}

/// Invalid source-resolver identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceResolverIdError {
    violation: SourceResolverIdViolation,
}

impl SourceResolverIdError {
    const fn new(violation: SourceResolverIdViolation) -> Self {
        Self { violation }
    }

    /// Returns violated resolver-identity rule.
    #[must_use]
    pub const fn violation(self) -> SourceResolverIdViolation {
        self.violation
    }

    /// Returns canonical registry metadata.
    #[must_use]
    pub const fn descriptor(self) -> ErrorDescriptor {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid).descriptor()
    }
}

impl fmt::Display for SourceResolverIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.descriptor().explanation())
    }
}

impl std::error::Error for SourceResolverIdError {}

/// Stable identity of one source resolver.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceResolverId(String);

impl SourceResolverId {
    /// Validates canonical lowercase resolver identity.
    ///
    /// # Errors
    ///
    /// Returns [`SourceResolverIdError`] for empty, oversized, or invalid input.
    pub fn new(value: impl Into<String>) -> Result<Self, SourceResolverIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SourceResolverIdError::new(SourceResolverIdViolation::Empty));
        }
        if value.len() > SOURCE_RESOLVER_ID_BYTES_MAX {
            return Err(SourceResolverIdError::new(
                SourceResolverIdViolation::TooLong,
            ));
        }
        let mut bytes = value.bytes();
        if !bytes.next().is_some_and(is_resolver_first_byte)
            || !bytes.all(is_resolver_continuation_byte)
        {
            return Err(SourceResolverIdError::new(
                SourceResolverIdViolation::InvalidCharacter,
            ));
        }
        Ok(Self(value))
    }

    /// Returns canonical resolver text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const fn is_resolver_first_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
}

fn is_resolver_continuation_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || SOURCE_RESOLVER_PUNCTUATION.contains(&byte)
}

impl fmt::Display for SourceResolverId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Resolver-owned source-unit identity failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceUnitIdErrorKind {
    /// Canonical identity exceeds protocol limit.
    TooLong,
    /// Identity does not contain canonical Rift source address structure.
    InvalidAddress,
    /// Resolver segment is invalid.
    InvalidResolver(SourceResolverIdError),
    /// Unit key contains malformed percent encoding or invalid UTF-8.
    InvalidEncoding,
    /// Decoded unit key violates source-path rules.
    InvalidKey(PathError),
    /// Address is valid but not encoded in canonical form.
    NonCanonical,
}

/// Invalid resolver-owned source-unit identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceUnitIdError {
    kind: SourceUnitIdErrorKind,
}

impl SourceUnitIdError {
    const fn new(kind: SourceUnitIdErrorKind) -> Self {
        Self { kind }
    }

    /// Returns source-unit identity failure classification.
    #[must_use]
    pub const fn kind(self) -> SourceUnitIdErrorKind {
        self.kind
    }

    /// Returns canonical registry metadata.
    #[must_use]
    pub const fn descriptor(self) -> ErrorDescriptor {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid).descriptor()
    }
}

impl fmt::Display for SourceUnitIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.descriptor().explanation())
    }
}

impl std::error::Error for SourceUnitIdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            SourceUnitIdErrorKind::InvalidResolver(error) => Some(error),
            SourceUnitIdErrorKind::InvalidKey(error) => Some(error),
            SourceUnitIdErrorKind::TooLong
            | SourceUnitIdErrorKind::InvalidAddress
            | SourceUnitIdErrorKind::InvalidEncoding
            | SourceUnitIdErrorKind::NonCanonical => None,
        }
    }
}

/// Stable resolver identity plus canonical source-unit key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceUnitId {
    resolver: SourceResolverId,
    key: SourcePath,
}

impl SourceUnitId {
    /// Constructs identity from validated resolver and unit key.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnitIdError`] when canonical URI exceeds protocol limit.
    pub fn new(resolver: SourceResolverId, key: SourcePath) -> Result<Self, SourceUnitIdError> {
        let identity = Self { resolver, key };
        if identity.encoded_len() > SOURCE_UNIT_ID_BYTES_MAX {
            return Err(SourceUnitIdError::new(SourceUnitIdErrorKind::TooLong));
        }
        Ok(identity)
    }

    /// Parses canonical `rift://source/` identity.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnitIdError`] for invalid structure, coordinates, or encoding.
    pub fn parse(value: &str) -> Result<Self, SourceUnitIdError> {
        if value.len() > SOURCE_UNIT_ID_BYTES_MAX {
            return Err(SourceUnitIdError::new(SourceUnitIdErrorKind::TooLong));
        }
        let address = value
            .strip_prefix(SOURCE_UNIT_URI_PREFIX)
            .ok_or(SourceUnitIdError::new(
                SourceUnitIdErrorKind::InvalidAddress,
            ))?;
        let (resolver, encoded_key) =
            address
                .split_once(SOURCE_UNIT_SEPARATOR)
                .ok_or(SourceUnitIdError::new(
                    SourceUnitIdErrorKind::InvalidAddress,
                ))?;
        let resolver = SourceResolverId::new(resolver).map_err(|error| {
            SourceUnitIdError::new(SourceUnitIdErrorKind::InvalidResolver(error))
        })?;
        let decoded = decode_unit_key(encoded_key)?;
        let key = SourcePath::new(decoded)
            .map_err(|error| SourceUnitIdError::new(SourceUnitIdErrorKind::InvalidKey(error)))?;
        let identity = Self::new(resolver, key)?;
        if identity.to_string() != value {
            return Err(SourceUnitIdError::new(SourceUnitIdErrorKind::NonCanonical));
        }
        Ok(identity)
    }

    /// Returns resolver identity.
    #[must_use]
    pub const fn resolver(&self) -> &SourceResolverId {
        &self.resolver
    }

    /// Returns decoded canonical unit key.
    #[must_use]
    pub const fn key(&self) -> &SourcePath {
        &self.key
    }

    fn encoded_len(&self) -> usize {
        SOURCE_UNIT_URI_PREFIX.len()
            + self.resolver.as_str().len()
            + SOURCE_UNIT_SEPARATOR_BYTES
            + self
                .key
                .as_str()
                .bytes()
                .map(|byte| {
                    if is_unit_key_safe(byte) {
                        1
                    } else {
                        PERCENT_ESCAPE_BYTES
                    }
                })
                .sum::<usize>()
    }
}

impl FromStr for SourceUnitId {
    type Err = SourceUnitIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for SourceUnitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{SOURCE_UNIT_URI_PREFIX}{}{SOURCE_UNIT_SEPARATOR}",
            self.resolver
        )?;
        for byte in self.key.as_str().bytes() {
            if is_unit_key_safe(byte) {
                formatter.write_char(char::from(byte))?;
            } else {
                write!(formatter, "%{byte:02X}")?;
            }
        }
        Ok(())
    }
}

fn decode_unit_key(value: &str) -> Result<String, SourceUnitIdError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == PERCENT_ESCAPE_MARKER {
            let high = bytes
                .get(index + PERCENT_ESCAPE_HIGH_OFFSET)
                .and_then(|byte| hex_value(*byte))
                .ok_or(SourceUnitIdError::new(
                    SourceUnitIdErrorKind::InvalidEncoding,
                ))?;
            let low = bytes
                .get(index + PERCENT_ESCAPE_LOW_OFFSET)
                .and_then(|byte| hex_value(*byte))
                .ok_or(SourceUnitIdError::new(
                    SourceUnitIdErrorKind::InvalidEncoding,
                ))?;
            decoded.push((high << HEX_NIBBLE_BITS) | low);
            index += PERCENT_ESCAPE_BYTES;
        } else {
            if !is_unit_key_safe(bytes[index]) {
                return Err(SourceUnitIdError::new(
                    SourceUnitIdErrorKind::InvalidEncoding,
                ));
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| SourceUnitIdError::new(SourceUnitIdErrorKind::InvalidEncoding))
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + HEX_LETTER_VALUE_OFFSET),
        b'a'..=b'f' => Some(byte - b'a' + HEX_LETTER_VALUE_OFFSET),
        _ => None,
    }
}

fn is_unit_key_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || SOURCE_UNIT_SAFE_PUNCTUATION.contains(&byte)
}

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
    use std::error::Error as _;
    use std::fmt::Write as _;
    use std::str::FromStr as _;

    use super::{
        CompositionId, CompositionRevision, CursorId, IdError, IndexRevision, ModelId,
        ModelRevision, ProviderId, ProviderRevision, RevisionError, SourceResolverId,
        SourceResolverIdViolation, SourceRevision, SourceUnitId, SourceUnitIdError,
        SourceUnitIdErrorKind, SymbolId, TreeRevision, WorkspaceId,
    };
    use crate::SourcePath;
    use crate::constants::{
        SOURCE_RESOLVER_ID_BYTES_MAX, SOURCE_UNIT_ID_BYTES_MAX, SOURCE_UNIT_URI_PREFIX,
    };

    #[test]
    fn identities_reject_ambiguous_values() {
        let resolver =
            SourceResolverId::new("rift.sources.project").expect("valid source resolver fixture");
        let key = SourcePath::new("src/café file.rs").expect("valid source key fixture");
        let identity = SourceUnitId::new(resolver, key).expect("identity fits protocol bound");
        assert_eq!(
            identity.to_string(),
            "rift://source/rift.sources.project/src/caf%C3%A9%20file.rs"
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs"),
            SourceUnitId::new(
                SourceResolverId::new("rift.sources.project").expect("valid resolver"),
                SourcePath::new("src/lib.rs").expect("valid key"),
            )
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/Rift/src/lib.rs").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidResolver(
                SourceResolverId::new("Rift").expect_err("uppercase is invalid")
            ))
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
                .map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::NonCanonical)
        );
    }

    #[test]
    fn source_unit_identity_enforces_encoded_uri_bound() {
        let resolver = SourceResolverId::new("r").expect("valid resolver");
        let overhead = format!("{SOURCE_UNIT_URI_PREFIX}r/").len();
        let encoded_budget = SOURCE_UNIT_ID_BYTES_MAX - overhead;
        let percent_count = encoded_budget / super::PERCENT_ESCAPE_BYTES;
        let safe_count = encoded_budget % super::PERCENT_ESCAPE_BYTES;
        let exact_key = format!("{}{}", "%".repeat(percent_count), "a".repeat(safe_count));
        let exact = SourceUnitId::new(
            resolver.clone(),
            SourcePath::new(exact_key).expect("valid exact-bound key"),
        )
        .expect("encoded identity at bound is valid");
        assert_eq!(exact.to_string().len(), SOURCE_UNIT_ID_BYTES_MAX);

        let over_key = format!("{}%", exact.key());
        assert_eq!(
            SourceUnitId::new(
                resolver,
                SourcePath::new(over_key).expect("decoded key remains bounded"),
            )
            .map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::TooLong)
        );
    }

    #[test]
    fn resolver_identity_reports_stable_violation() {
        let resolver_error = SourceResolverId::new("Rift").expect_err("uppercase is invalid");
        assert_eq!(
            resolver_error.violation(),
            SourceResolverIdViolation::InvalidCharacter
        );
        assert_eq!(
            resolver_error.to_string(),
            resolver_error.descriptor().explanation()
        );

        let unit_error = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("invalid resolver must fail unit identity");
        assert!(unit_error.source().is_some());
        assert_eq!(
            unit_error.to_string(),
            unit_error.descriptor().explanation()
        );
    }

    #[test]
    fn resolver_identity_rejects_empty_and_oversized_values() {
        let empty_error = SourceResolverId::new("").expect_err("empty resolver is invalid");
        assert_eq!(empty_error.violation(), SourceResolverIdViolation::Empty);

        let oversized = "a".repeat(SOURCE_RESOLVER_ID_BYTES_MAX + 1);
        let oversized_error =
            SourceResolverId::new(oversized.as_str()).expect_err("oversized resolver");
        assert_eq!(
            oversized_error.violation(),
            SourceResolverIdViolation::TooLong
        );
    }

    #[test]
    fn unit_identity_rejects_malformed_addresses() {
        assert_eq!(
            SourceUnitId::parse("not-a-rift-source-uri").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidAddress)
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/resolverwithoutseparator")
                .map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidAddress)
        );

        let oversized = format!(
            "{SOURCE_UNIT_URI_PREFIX}{}",
            "a".repeat(SOURCE_UNIT_ID_BYTES_MAX)
        );
        assert_eq!(
            SourceUnitId::parse(&oversized).map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::TooLong)
        );
    }

    #[test]
    fn unit_identity_rejects_malformed_percent_escapes() {
        assert_eq!(
            SourceUnitId::parse("rift://source/r/%G0").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidEncoding)
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/r/%1").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidEncoding)
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/r/%FF").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidEncoding)
        );
        assert_eq!(
            SourceUnitId::parse("rift://source/r/a b").map_err(SourceUnitIdError::kind),
            Err(SourceUnitIdErrorKind::InvalidEncoding)
        );
    }

    #[test]
    fn unit_identity_reports_decoded_key_violation_as_source() {
        let error = SourceUnitId::parse("rift://source/rift.sources.project/..")
            .expect_err("dot-segment key must be rejected");
        let key_error = SourcePath::new("..").expect_err("dot segment is invalid");
        assert_eq!(error.kind(), SourceUnitIdErrorKind::InvalidKey(key_error));
        assert!(error.source().is_some());

        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical encoding must be rejected");
        assert_eq!(non_canonical.kind(), SourceUnitIdErrorKind::NonCanonical);
        assert!(non_canonical.source().is_none());
    }

    #[test]
    fn unit_identity_exposes_resolver_and_implements_from_str() {
        let identity = SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs")
            .expect("canonical identity parses");
        assert_eq!(identity.resolver().as_str(), "rift.sources.project");

        let parsed = SourceUnitId::from_str("rift://source/rift.sources.project/src/lib.rs")
            .expect("FromStr delegates to parse");
        assert_eq!(parsed, identity);
    }

    #[test]
    fn unit_identity_display_propagates_writer_failure() {
        struct FailingWriter;

        impl std::fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> std::fmt::Result {
                Err(std::fmt::Error)
            }
        }

        let resolver = SourceResolverId::new("r").expect("valid resolver");
        let key = SourcePath::new("lib.rs").expect("valid key");
        let identity = SourceUnitId::new(resolver, key).expect("identity fits protocol bound");

        let mut sink = FailingWriter;
        assert!(write!(sink, "{identity}").is_err());
    }

    #[test]
    fn id_error_displays_and_implements_std_error() {
        let error = WorkspaceId::new("").expect_err("empty value must be rejected");
        assert_eq!(
            error.to_string(),
            "identity must be non-empty and contain no control characters"
        );
        let _: &dyn std::error::Error = &error;
        assert_eq!(error, IdError);
    }

    #[test]
    fn revisions_are_non_zero() {
        assert!(TreeRevision::new(0).is_err());
        assert_eq!(TreeRevision::new(7).map(TreeRevision::get), Ok(7));
    }

    #[test]
    fn revision_error_displays_and_implements_std_error() {
        let error = TreeRevision::new(0).expect_err("zero revision must be rejected");
        assert_eq!(error.to_string(), "revision must be greater than zero");
        let _: &dyn std::error::Error = &error;
        assert_eq!(error, RevisionError);
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
