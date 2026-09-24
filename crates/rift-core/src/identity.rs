use std::fmt::{self, Write as _};
use std::num::NonZeroU64;
use std::str::FromStr;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;

use crate::constants::{
    HEX_LETTER_VALUE_OFFSET, HEX_NIBBLE_BITS, PERCENT_ESCAPE_BYTES, PERCENT_ESCAPE_HIGH_OFFSET,
    PERCENT_ESCAPE_LOW_OFFSET, PERCENT_ESCAPE_MARKER, SOURCE_RESOLVER_ID_BYTES_MAX,
    SOURCE_RESOLVER_PUNCTUATION, SOURCE_UNIT_ID_BYTES_MAX, SOURCE_UNIT_SAFE_PUNCTUATION,
    SOURCE_UNIT_SEPARATOR, SOURCE_UNIT_SEPARATOR_BYTES, SOURCE_UNIT_URI_PREFIX, SYMBOL_URI_PREFIX,
};
use crate::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, PackageIdentity, PathError, ProjectPath,
    SourcePath,
};

/// ASCII bytes percent-encoded inside the path of a `rift://` identity.
///
/// The kept characters are the RFC 3986 path set; every other byte, including each byte of a
/// multi-byte UTF-8 sequence, is `%XX`-escaped. Slashes remain path separators.
const RIFT_PATH_ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/')
    .remove(b'-');

/// ASCII bytes percent-encoded inside a symbol's final qualified-name segment.
/// Slashes in declaration names must not become project path separators.
const RIFT_SYMBOL_ESCAPE_SET: &AsciiSet = &RIFT_PATH_ESCAPE_SET.add(b'/');

/// Percent-encodes the path of a `rift://` identity, keeping the RFC 3986 path set
/// literal and escaping everything else.
///
/// The read service and the lexical index share this one function so a project path
/// is escaped identically wherever a wire identity is minted from it.
#[must_use]
pub fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, RIFT_PATH_ESCAPE_SET).to_string()
}

/// Mints the wire identity for one symbol declaration:
/// `rift://symbol/{language_segment}/{escaped_path}/{escaped_qualified_name}`.
///
/// `language_segment` is the declaring language's address spelling - `name`, or
/// `name:dialect` - as `Language::identity_segment` mints it. The read service mints this as
/// a declaration's `SymbolId`, and the lexical index mints the same spelling as a symbol
/// lexical unit's identity, so a lexical hit's identity equals the id `get_symbol` returns
/// for that declaration. The qualified name is one segment: its slashes are escaped
/// so the final literal slash always separates the project path from the name.
#[must_use]
pub fn symbol_identity(language_segment: &str, path: &str, qualified_name: &str) -> String {
    format!(
        "{SYMBOL_URI_PREFIX}{language_segment}/{}/{}",
        encode_path(path),
        utf8_percent_encode(qualified_name, RIFT_SYMBOL_ESCAPE_SET)
    )
}

/// Parsed parts of a canonical wire symbol identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbolIdentity {
    language_segment: String,
    path: String,
    qualified_name: String,
}

impl ParsedSymbolIdentity {
    /// Returns language segment from wire identity.
    #[must_use]
    pub fn language_segment(&self) -> &str {
        &self.language_segment
    }

    /// Returns the decoded project path or resolver and source-unit key.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns decoded qualified name from wire identity.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Returns canonical wire identity.
    #[must_use]
    pub fn wire_identity(&self) -> String {
        symbol_identity(
            &self.language_segment,
            self.path.as_str(),
            &self.qualified_name,
        )
    }
}

/// Failure to parse a canonical wire symbol identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolIdentityError;

impl std::fmt::Display for SymbolIdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("symbol identity is not canonical")
    }
}

impl std::error::Error for SymbolIdentityError {}

/// Parses one canonical `rift://symbol/` identity.
///
/// # Errors
///
/// Returns [`SymbolIdentityError`] for invalid structure, language, path, encoding, or spelling.
pub fn parse_symbol_identity(value: &str) -> Result<ParsedSymbolIdentity, SymbolIdentityError> {
    const SYMBOL_ID_BYTES_MAX: usize = 8_192;

    if value.len() > SYMBOL_ID_BYTES_MAX {
        return Err(SymbolIdentityError);
    }
    let remainder = value
        .strip_prefix(SYMBOL_URI_PREFIX)
        .ok_or(SymbolIdentityError)?;
    let (language_segment, remainder) = remainder.split_once('/').ok_or(SymbolIdentityError)?;
    rift_protocol::read::Language::from_identity_segment(language_segment)
        .map_err(|_| SymbolIdentityError)?;
    let (encoded_path, encoded_name) = remainder.rsplit_once('/').ok_or(SymbolIdentityError)?;
    let path = percent_encoding::percent_decode_str(encoded_path)
        .decode_utf8()
        .map_err(|_| SymbolIdentityError)?
        .into_owned();
    let qualified_name = percent_encoding::percent_decode_str(encoded_name)
        .decode_utf8()
        .map_err(|_| SymbolIdentityError)?
        .into_owned();
    if path.is_empty() || qualified_name.is_empty() {
        return Err(SymbolIdentityError);
    }
    if ProjectPath::new(&path).is_err()
        && SourceUnitId::parse(&format!("{SOURCE_UNIT_URI_PREFIX}{encoded_path}")).is_err()
    {
        return Err(SymbolIdentityError);
    }
    let parsed = ParsedSymbolIdentity {
        language_segment: language_segment.to_owned(),
        path,
        qualified_name,
    };
    if parsed.wire_identity() != value {
        return Err(SymbolIdentityError);
    }
    Ok(parsed)
}

/// An identity value that is empty or carries a control character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdFault;

impl Fault for IdFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }
}

/// Invalid stable identity.
pub type IdError = Error<IdFault>;

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
                    return Err(Error::new(IdFault));
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
define_id!(ProviderSymbolId, "Provider-local symbol identity.");
define_id!(CompositionId, "Provider composition identity.");
define_id!(ModelId, "Resolved embedding model identity.");

/// Violated source-resolver identity rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceResolverIdViolation {
    /// Resolver identity is empty.
    Empty,
    /// Resolver identity exceeds 128 ASCII bytes.
    TooLong,
    /// Resolver identity is not canonical lowercase syntax.
    InvalidCharacter,
}

/// A source-resolver identity that broke one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceResolverIdFault {
    violation: SourceResolverIdViolation,
}

impl SourceResolverIdFault {
    /// Returns violated resolver-identity rule.
    #[must_use]
    pub const fn violation(self) -> SourceResolverIdViolation {
        self.violation
    }
}

impl Fault for SourceResolverIdFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("identity", "source_resolver"),
            ErrorContext::new("violation", crate::fault_label(&self.violation)),
        ]
    }
}

/// Invalid source-resolver identity.
pub type SourceResolverIdError = Error<SourceResolverIdFault>;

fn resolver_id_error(violation: SourceResolverIdViolation) -> SourceResolverIdError {
    Error::new(SourceResolverIdFault { violation })
}

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
            return Err(resolver_id_error(SourceResolverIdViolation::Empty));
        }
        if value.len() > SOURCE_RESOLVER_ID_BYTES_MAX {
            return Err(resolver_id_error(SourceResolverIdViolation::TooLong));
        }
        let mut bytes = value.bytes();
        if !bytes.next().is_some_and(is_resolver_first_byte)
            || !bytes.all(is_resolver_continuation_byte)
        {
            return Err(resolver_id_error(
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
#[derive(Debug, PartialEq, Eq)]
pub enum SourceUnitIdFault {
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

impl Fault for SourceUnitIdFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("identity", "source_unit")];
        match self {
            Self::TooLong => context.push(ErrorContext::new("violation", "too_long")),
            Self::InvalidAddress => {
                context.push(ErrorContext::new("violation", "invalid_address"));
            }
            Self::InvalidResolver(error) => context.extend(error.context()),
            Self::InvalidEncoding => {
                context.push(ErrorContext::new("violation", "invalid_encoding"));
            }
            Self::InvalidKey(error) => context.extend(error.context()),
            Self::NonCanonical => context.push(ErrorContext::new("violation", "non_canonical")),
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidResolver(error) => Some(error),
            Self::InvalidKey(error) => Some(error),
            Self::TooLong | Self::InvalidAddress | Self::InvalidEncoding | Self::NonCanonical => {
                None
            }
        }
    }
}

/// Invalid resolver-owned source-unit identity.
pub type SourceUnitIdError = Error<SourceUnitIdFault>;

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
            return Err(Error::new(SourceUnitIdFault::TooLong));
        }
        Ok(identity)
    }

    /// The unit of one file inside a cataloged package.
    ///
    /// The resolver is the package's manager and the key is `<name>@<version>/<path>`,
    /// so the unit renders as `rift://source/cargo/helper@0.1.0/src/lib.rs`. The
    /// dependency lane and the engine tier both mint a package file's unit here: one
    /// file has one unit wherever it is addressed from.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnitIdError`] when the manager is no resolver identity, when
    /// the key breaks the source path rules, or when the canonical URI exceeds the
    /// protocol limit.
    pub fn for_package(
        package: &PackageIdentity,
        path: &ProjectPath,
    ) -> Result<Self, SourceUnitIdError> {
        let resolver = SourceResolverId::new(package.manager.clone())
            .map_err(|error| Error::new(SourceUnitIdFault::InvalidResolver(error)))?;
        let key = SourcePath::new(format!("{}@{}/{path}", package.name, package.version))
            .map_err(|error| Error::new(SourceUnitIdFault::InvalidKey(error)))?;
        Self::new(resolver, key)
    }

    /// Parses canonical `rift://source/` identity.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnitIdError`] for invalid structure, coordinates, or encoding.
    pub fn parse(value: &str) -> Result<Self, SourceUnitIdError> {
        if value.len() > SOURCE_UNIT_ID_BYTES_MAX {
            return Err(Error::new(SourceUnitIdFault::TooLong));
        }
        let address = value
            .strip_prefix(SOURCE_UNIT_URI_PREFIX)
            .ok_or(SourceUnitIdError::new(SourceUnitIdFault::InvalidAddress))?;
        let (resolver, encoded_key) = address
            .split_once(SOURCE_UNIT_SEPARATOR)
            .ok_or(SourceUnitIdError::new(SourceUnitIdFault::InvalidAddress))?;
        let resolver = SourceResolverId::new(resolver)
            .map_err(|error| Error::new(SourceUnitIdFault::InvalidResolver(error)))?;
        let decoded = decode_unit_key(encoded_key)?;
        let key = SourcePath::new(decoded)
            .map_err(|error| Error::new(SourceUnitIdFault::InvalidKey(error)))?;
        let identity = Self::new(resolver, key)?;
        if identity.to_string() != value {
            return Err(Error::new(SourceUnitIdFault::NonCanonical));
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
                .ok_or(SourceUnitIdError::new(SourceUnitIdFault::InvalidEncoding))?;
            let low = bytes
                .get(index + PERCENT_ESCAPE_LOW_OFFSET)
                .and_then(|byte| hex_value(*byte))
                .ok_or(SourceUnitIdError::new(SourceUnitIdFault::InvalidEncoding))?;
            decoded.push((high << HEX_NIBBLE_BITS) | low);
            index += PERCENT_ESCAPE_BYTES;
        } else {
            if !is_unit_key_safe(bytes[index]) {
                return Err(SourceUnitIdError::new(SourceUnitIdFault::InvalidEncoding));
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| Error::new(SourceUnitIdFault::InvalidEncoding))
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

/// A revision of zero, which no counter mints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionFault;

impl Fault for RevisionFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }
}

/// Invalid zero revision.
pub type RevisionError = Error<RevisionFault>;

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
                NonZeroU64::new(value)
                    .map(Self)
                    .ok_or_else(|| Error::new(RevisionFault))
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
    use std::fmt::Write as _;
    use std::str::FromStr as _;

    use super::{
        CompositionId, CompositionRevision, IndexRevision, ModelId, ModelRevision, ProviderId,
        ProviderRevision, ProviderSymbolId, SourceResolverId, SourceResolverIdViolation,
        SourceRevision, SourceUnitId, SourceUnitIdError, SourceUnitIdFault, SymbolId, TreeRevision,
        WorkspaceId, encode_path, parse_symbol_identity, symbol_identity,
    };
    use crate::constants::{
        SOURCE_RESOLVER_ID_BYTES_MAX, SOURCE_UNIT_ID_BYTES_MAX, SOURCE_UNIT_URI_PREFIX,
    };
    use crate::{PackageIdentity, PathViolation, ProjectPath, SourcePath};

    #[test]
    fn encode_path_keeps_the_rfc3986_path_set_literal_and_escapes_the_rest() {
        assert_eq!(encode_path("src/lib.rs"), "src/lib.rs");
        assert_eq!(encode_path("Rift::update"), "Rift::update");
        assert_eq!(encode_path("a b"), "a%20b");
        assert_eq!(encode_path("café"), "caf%C3%A9");
    }

    /// The escape set and the served identity patterns describe one alphabet. A byte this set
    /// keeps literal is a byte every `rift://` pattern accepts; a byte it escapes reaches the
    /// wire as `%XX`, which those patterns accept separately. Drift between the two returns an
    /// identity the server minted and its own schema refuses.
    #[test]
    fn the_escape_set_keeps_exactly_the_bytes_the_served_patterns_accept() {
        for byte in 0x20_u8..0x7f {
            let character = char::from(byte).to_string();
            let kept = encode_path(&character) == character;
            let advertised = byte.is_ascii_alphanumeric()
                || rift_protocol::read::IDENTITY_PATH_PUNCTUATION.contains(&character);
            assert_eq!(
                kept, advertised,
                "byte {byte:#04x} ({character}) is kept literal by the encoder and accepted by \
                 the served patterns, or by neither"
            );
        }
    }

    #[test]
    fn symbol_identity_pins_the_exact_wire_spelling_with_escaped_characters() {
        let identity = symbol_identity("rust", "src/café mod.rs", "Rift::separated name");
        assert_eq!(
            identity,
            "rift://symbol/rust/src/caf%C3%A9%20mod.rs/Rift::separated%20name"
        );
    }

    #[test]
    fn parse_symbol_identity_accepts_only_canonical_wire_spelling() {
        let identity = symbol_identity("rust", "src/café mod.rs", "Rift::separated name");
        let parsed = parse_symbol_identity(&identity).expect("canonical symbol identity");
        assert_eq!(parsed.language_segment(), "rust");
        assert_eq!(parsed.path(), "src/café mod.rs");
        assert_eq!(parsed.qualified_name(), "Rift::separated name");
        assert_eq!(parsed.wire_identity(), identity);
        let oversized = format!("rift://symbol/rust/src/lib.rs/{}", "x".repeat(8192));
        let error = parse_symbol_identity(&oversized).expect_err("identity exceeds byte bound");
        assert_eq!(error.to_string(), "symbol identity is not canonical");

        for invalid in [
            "invalid",
            "rift://symbol/Rust/src/lib.rs/Thing",
            "rift://symbol/rust//Thing",
            "rift://symbol/rust/src/lib.rs/",
            "rift://symbol/rust/src%2flib.rs/Thing",
            "rift://symbol/rust/src%FF.rs/Thing",
            "rift://symbol/rust/src/../lib.rs/Thing",
        ] {
            assert!(
                parse_symbol_identity(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn parse_symbol_identity_accepts_bounded_package_paths() {
        let path = format!("cargo/beacon@1.0.0/{}lib.rs", "a/".repeat(490));
        assert!(ProjectPath::new(&path).is_err());
        let identity = symbol_identity("rust", &path, "serve");
        let parsed = parse_symbol_identity(&identity).expect("package symbol identity");
        assert_eq!(parsed.path(), path);
        assert_eq!(parsed.qualified_name(), "serve");
        assert_eq!(parsed.wire_identity(), identity);

        let oversized_key = format!("cargo/{}", "x".repeat(4_097));
        assert!(parse_symbol_identity(&symbol_identity("rust", &oversized_key, "serve")).is_err());
    }

    #[test]
    fn symbol_identity_keeps_slashes_inside_the_qualified_name() {
        for (name, encoded) in [
            (
                "js/bun/test/expect.test.ts",
                "js%2Fbun%2Ftest%2Fexpect.test.ts",
            ),
            ("js%2Fbun", "js%252Fbun"),
            ("café/test", "caf%C3%A9%2Ftest"),
            ("/leading//trailing/", "%2Fleading%2F%2Ftrailing%2F"),
        ] {
            assert_eq!(
                symbol_identity("json", "test/expected-durations.json", name),
                format!("rift://symbol/json/test/expected-durations.json/{encoded}"),
                "qualified-name slashes must stay separate from path separators: {name}"
            );
        }
    }

    /// Pins a shipped rust `SymbolId` byte-for-byte: generalizing the minting
    /// function over the language segment must not move any published rust
    /// identity.
    #[test]
    fn symbol_identity_keeps_the_shipped_rust_spelling() {
        assert_eq!(
            symbol_identity("rust", "src/lib.rs", "Beacon"),
            "rift://symbol/rust/src/lib.rs/Beacon"
        );
    }

    #[test]
    fn provider_symbol_identity_rejects_empty_and_control_characters() {
        assert!(ProviderSymbolId::new("").is_err());
        assert!(ProviderSymbolId::new("rust\nitem").is_err());
        assert_eq!(
            ProviderSymbolId::new("rust:item").map(|identity| identity.to_string()),
            Ok("rust:item".to_owned())
        );
    }

    #[test]
    fn symbol_identity_files_a_dialect_segment_between_scheme_and_path() {
        assert_eq!(
            symbol_identity("typescript:tsx", "src/App.tsx", "render"),
            "rift://symbol/typescript:tsx/src/App.tsx/render"
        );
    }

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
        let invalid_resolver = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("uppercase resolver is invalid");
        assert!(matches!(
            invalid_resolver.fault(),
            SourceUnitIdFault::InvalidResolver(inner)
                if inner.fault().violation() == SourceResolverIdViolation::InvalidCharacter
        ));
        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical escape is invalid");
        assert!(matches!(
            non_canonical.fault(),
            SourceUnitIdFault::NonCanonical
        ));
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
        let over_bound = SourceUnitId::new(
            resolver,
            SourcePath::new(over_key).expect("decoded key remains bounded"),
        )
        .expect_err("identity above the encoded bound is invalid");
        assert!(matches!(over_bound.fault(), SourceUnitIdFault::TooLong));
    }

    #[test]
    fn resolver_identity_reports_stable_violation() {
        let resolver_error = SourceResolverId::new("Rift").expect_err("uppercase is invalid");
        assert_eq!(
            resolver_error.fault().violation(),
            SourceResolverIdViolation::InvalidCharacter
        );
        assert_eq!(
            resolver_error.to_string(),
            "the request does not match the documented form: \
             identity source_resolver, violation invalid_character; \
             correct the reported field and resend the request"
        );

        let unit_error = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("invalid resolver must fail unit identity");
        assert!(std::error::Error::source(&unit_error).is_some());
        assert_eq!(
            unit_error.to_string(),
            "the request does not match the documented form: \
             identity source_unit, identity source_resolver, \
             violation invalid_character; \
             correct the reported field and resend the request"
        );
    }

    #[test]
    fn resolver_identity_rejects_empty_and_oversized_values() {
        let empty_error = SourceResolverId::new("").expect_err("empty resolver is invalid");
        assert_eq!(
            empty_error.fault().violation(),
            SourceResolverIdViolation::Empty
        );

        let oversized = "a".repeat(SOURCE_RESOLVER_ID_BYTES_MAX + 1);
        let oversized_error =
            SourceResolverId::new(oversized.as_str()).expect_err("oversized resolver");
        assert_eq!(
            oversized_error.fault().violation(),
            SourceResolverIdViolation::TooLong
        );
    }

    #[test]
    fn unit_identity_rejects_malformed_addresses() {
        let missing_prefix =
            SourceUnitId::parse("not-a-rift-source-uri").expect_err("missing prefix is invalid");
        assert!(matches!(
            missing_prefix.fault(),
            SourceUnitIdFault::InvalidAddress
        ));
        let missing_separator = SourceUnitId::parse("rift://source/resolverwithoutseparator")
            .expect_err("missing separator is invalid");
        assert!(matches!(
            missing_separator.fault(),
            SourceUnitIdFault::InvalidAddress
        ));

        let oversized = format!(
            "{SOURCE_UNIT_URI_PREFIX}{}",
            "a".repeat(SOURCE_UNIT_ID_BYTES_MAX)
        );
        let over_bound = SourceUnitId::parse(&oversized).expect_err("oversized address");
        assert!(matches!(over_bound.fault(), SourceUnitIdFault::TooLong));
    }

    #[test]
    fn unit_identity_rejects_malformed_percent_escapes() {
        for address in [
            "rift://source/r/%G0",
            "rift://source/r/%1",
            "rift://source/r/%FF",
            "rift://source/r/a b",
        ] {
            let error = SourceUnitId::parse(address).expect_err("malformed escape is invalid");
            assert!(
                matches!(error.fault(), SourceUnitIdFault::InvalidEncoding),
                "{address} must classify as invalid_encoding, got {fault:?}",
                fault = error.fault()
            );
        }
    }

    #[test]
    fn unit_identity_reports_decoded_key_violation_as_source() {
        let error = SourceUnitId::parse("rift://source/rift.sources.project/..")
            .expect_err("dot-segment key must be rejected");
        assert!(matches!(
            error.fault(),
            SourceUnitIdFault::InvalidKey(key_error)
                if key_error.fault().violation() == crate::PathViolation::DotSegment
        ));
        assert!(std::error::Error::source(&error).is_some());

        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical encoding must be rejected");
        assert!(matches!(
            non_canonical.fault(),
            SourceUnitIdFault::NonCanonical
        ));
        assert!(std::error::Error::source(&non_canonical).is_none());
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
            "the request does not match the documented form; \
             correct the reported field and resend the request"
        );
        assert_eq!(error.descriptor().code(), "invalid_request");
        let _: &dyn std::error::Error = &error;
        assert!(matches!(error.fault(), super::IdFault));
    }

    #[test]
    fn revisions_are_non_zero() {
        assert!(TreeRevision::new(0).is_err());
        assert_eq!(TreeRevision::new(7).map(TreeRevision::get), Ok(7));
    }

    #[test]
    fn revision_error_displays_and_implements_std_error() {
        let error = TreeRevision::new(0).expect_err("zero revision must be rejected");
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form; \
             correct the reported field and resend the request"
        );
        assert_eq!(error.descriptor().code(), "invalid_request");
        let _: &dyn std::error::Error = &error;
        assert!(matches!(error.fault(), super::RevisionFault));
    }

    fn context_pairs(error: &SourceUnitIdError) -> Vec<(&'static str, String)> {
        error
            .context()
            .into_iter()
            .map(|entry| (entry.key(), entry.value().to_string()))
            .collect()
    }

    #[test]
    fn source_unit_id_error_context_covers_every_kind() {
        let too_long = SourceUnitId::parse(&format!(
            "{SOURCE_UNIT_URI_PREFIX}{}",
            "a".repeat(SOURCE_UNIT_ID_BYTES_MAX)
        ))
        .expect_err("oversized address must be rejected");
        assert_eq!(
            context_pairs(&too_long),
            vec![
                ("identity", "source_unit".to_string()),
                ("violation", "too_long".to_string()),
            ]
        );

        let invalid_address = SourceUnitId::parse("not-a-rift-source-uri")
            .expect_err("missing prefix must be rejected");
        assert_eq!(
            context_pairs(&invalid_address),
            vec![
                ("identity", "source_unit".to_string()),
                ("violation", "invalid_address".to_string()),
            ]
        );

        let invalid_resolver = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("uppercase resolver must be rejected");
        assert_eq!(
            context_pairs(&invalid_resolver),
            vec![
                ("identity", "source_unit".to_string()),
                ("identity", "source_resolver".to_string()),
                ("violation", "invalid_character".to_string()),
            ]
        );

        let invalid_encoding = SourceUnitId::parse("rift://source/r/%G0")
            .expect_err("malformed percent escape must be rejected");
        assert_eq!(
            context_pairs(&invalid_encoding),
            vec![
                ("identity", "source_unit".to_string()),
                ("violation", "invalid_encoding".to_string()),
            ]
        );

        let invalid_key = SourceUnitId::parse("rift://source/rift.sources.project/..")
            .expect_err("dot-segment key must be rejected");
        assert_eq!(
            context_pairs(&invalid_key),
            vec![
                ("identity", "source_unit".to_string()),
                ("path_kind", "source".to_string()),
                ("violation", "dot_segment".to_string()),
            ]
        );

        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical encoding must be rejected");
        assert_eq!(
            context_pairs(&non_canonical),
            vec![
                ("identity", "source_unit".to_string()),
                ("violation", "non_canonical".to_string()),
            ]
        );

        assert_eq!(
            too_long.to_string(),
            "the request does not match the documented form: \
             identity source_unit, violation too_long; \
             correct the reported field and resend the request"
        );
        assert_eq!(
            invalid_key.to_string(),
            "the request does not match the documented form: \
             identity source_unit, path_kind source, violation dot_segment; \
             correct the reported field and resend the request"
        );
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

    fn package(manager: &str, name: &str, version: &str) -> PackageIdentity {
        PackageIdentity {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    #[test]
    fn package_unit_spells_the_manager_resolver_and_the_name_at_version_key() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let unit = SourceUnitId::for_package(&package("cargo", "helper", "0.1.0"), &path)
            .expect("unit fits protocol bound");
        assert_eq!(
            unit.to_string(),
            "rift://source/cargo/helper@0.1.0/src/lib.rs"
        );
        assert_eq!(unit.resolver().as_str(), "cargo");
        assert_eq!(unit.key().as_str(), "helper@0.1.0/src/lib.rs");
    }

    #[test]
    fn package_unit_refuses_a_manager_that_is_no_resolver_identity() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let error = SourceUnitId::for_package(&package("Cargo", "helper", "0.1.0"), &path)
            .expect_err("uppercase manager");
        assert!(matches!(
            error.fault(),
            SourceUnitIdFault::InvalidResolver(inner)
                if inner.fault().violation() == SourceResolverIdViolation::InvalidCharacter
        ));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn package_unit_refuses_a_version_that_breaks_the_source_path_rules() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let error = SourceUnitId::for_package(&package("cargo", "helper", "0.1.0\\beta"), &path)
            .expect_err("a backslash in the version");
        assert!(matches!(
            error.fault(),
            SourceUnitIdFault::InvalidKey(inner)
                if inner.fault().violation() == PathViolation::Backslash
        ));
        assert!(std::error::Error::source(&error).is_some());
    }
}
