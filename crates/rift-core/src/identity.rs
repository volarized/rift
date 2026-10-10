use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::Arc;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rift_error::{RiftError, errors};

use crate::constants::{
    PERCENT_ESCAPE_BYTES, SOURCE_RESOLVER_ID_BYTES_MAX, SOURCE_UNIT_ID_BYTES_MAX,
    SOURCE_UNIT_SEPARATOR, SOURCE_UNIT_SEPARATOR_BYTES, SOURCE_UNIT_URI_PREFIX, SYMBOL_URI_PREFIX,
};
use crate::{PackageIdentity, ProjectPath, SourcePath};

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

/// Invalid stable identity.
macro_rules! define_id {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Validates and constructs identity.
            ///
            /// # Errors
            ///
            /// Returns [`RiftError`] when value is empty or contains a control character.
            pub fn new(value: impl Into<String>) -> Result<Self, RiftError> {
                let value = value.into();
                if value.is_empty() || value.chars().any(char::is_control) {
                    return errors::core::identity_invalid().fail();
                }
                Ok(Self(Arc::from(value)))
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

/// Stable identity of one source resolver.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceResolverId(Arc<str>);

impl SourceResolverId {
    /// Validates canonical lowercase resolver identity.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for empty, oversized, or invalid input.
    pub fn new(value: impl Into<String>) -> Result<Self, RiftError> {
        let value = value.into();
        if value.is_empty() {
            return errors::core::resolver_id_empty()
                .identity("source_resolver")
                .fail();
        }
        if value.len() > SOURCE_RESOLVER_ID_BYTES_MAX {
            return errors::core::resolver_id_too_long()
                .identity("source_resolver")
                .fail();
        }
        if !rift_protocol::identity::source_resolver_is_valid(&value) {
            return errors::core::resolver_id_invalid_character()
                .identity("source_resolver")
                .fail();
        }
        Ok(Self(Arc::from(value)))
    }

    /// Returns canonical resolver text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceResolverId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable resolver identity plus canonical source-unit key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceUnitId {
    resolver: SourceResolverId,
    key: SourcePath,
    owner: Option<rift_protocol::identity::SymbolOwner>,
}

impl SourceUnitId {
    /// Constructs identity from validated resolver and unit key.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when canonical URI exceeds protocol limit.
    pub fn new(resolver: SourceResolverId, key: SourcePath) -> Result<Self, RiftError> {
        if matches!(resolver.as_str(), "cargo" | "npm" | "pypi" | "stdlib") {
            return errors::core::source_unit_id_invalid_address()
                .identity("source_unit")
                .fail();
        }
        if !rift_protocol::identity::source_unit_path_is_valid(key.as_str()) {
            return errors::core::source_unit_id_invalid_key()
                .identity("source_unit")
                .cause(
                    errors::core::path_empty_segment()
                        .path_kind("source")
                        .error(),
                )
                .fail();
        }
        let identity = Self {
            resolver,
            key,
            owner: None,
        };
        if identity.encoded_len() > SOURCE_UNIT_ID_BYTES_MAX {
            return errors::core::source_unit_id_too_long()
                .identity("source_unit")
                .fail();
        }
        Ok(identity)
    }

    /// The unit of one file inside its defining registry package.
    ///
    /// Registry endpoint, package namespace and exact version remain in the address;
    /// the source path counts from the package root.
    ///
    /// # Errors
    /// Returns an identity error for an invalid owner, path or encoded length.
    pub fn for_package(package: &PackageIdentity, path: &ProjectPath) -> Result<Self, RiftError> {
        let owner = package
            .owner()
            .map_err(|_| errors::core::identity_invalid().error())?;
        Self::for_owner(owner, path.as_str())
    }

    /// The unit of one file installed with an exact runtime or compiler release.
    ///
    /// # Errors
    /// Returns an identity error for an invalid owner, path or encoded length.
    pub fn for_runtime(
        runtime: &rift_protocol::read::RuntimeIdentity,
        path: &ProjectPath,
    ) -> Result<Self, RiftError> {
        let owner = runtime
            .owner()
            .map_err(|_| errors::core::identity_invalid().error())?;
        Self::for_owner(owner, path.as_str())
    }

    /// The released source unit its accepted origin and root-relative path establish.
    ///
    /// # Errors
    /// Returns an identity error when the origin does not establish a released owner.
    pub fn for_origin(
        origin: &rift_protocol::read::SourceLocation,
        path: &ProjectPath,
    ) -> Result<Self, RiftError> {
        match origin {
            rift_protocol::read::SourceLocation::Dependency { package } => {
                Self::for_package(package, path)
            }
            rift_protocol::read::SourceLocation::Stdlib {
                runtime: Some(runtime),
            } => Self::for_runtime(runtime, path),
            _ => errors::core::identity_invalid().fail(),
        }
    }

    /// Defining released owner, absent for a generic resolver unit.
    #[must_use]
    pub const fn source_owner(&self) -> Option<&rift_protocol::identity::SymbolOwner> {
        self.owner.as_ref()
    }

    fn for_owner(
        owner: rift_protocol::identity::SymbolOwner,
        path: &str,
    ) -> Result<Self, RiftError> {
        rift_protocol::identity::released_source_identity(&owner, path)
            .map_err(|_| errors::core::identity_invalid().error())?;
        let resolver = match &owner {
            rift_protocol::identity::SymbolOwner::Package { manager, .. } => {
                SourceResolverId::new(manager.clone())?
            }
            rift_protocol::identity::SymbolOwner::Runtime { .. } => {
                SourceResolverId::new("stdlib")?
            }
            _ => return errors::core::identity_invalid().fail(),
        };
        let key = SourcePath::new(path)?;
        Ok(Self {
            resolver,
            key,
            owner: Some(owner),
        })
    }

    /// Parses canonical `rift://source/` identity.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for invalid structure, coordinates, or encoding.
    pub fn parse(value: &str) -> Result<Self, RiftError> {
        if value.len() > SOURCE_UNIT_ID_BYTES_MAX {
            return errors::core::source_unit_id_too_long()
                .identity("source_unit")
                .fail();
        }
        let address = value.strip_prefix(SOURCE_UNIT_URI_PREFIX).ok_or_else(|| {
            errors::core::source_unit_id_invalid_address()
                .identity("source_unit")
                .error()
        })?;
        let (resolver, encoded_key) =
            address.split_once(SOURCE_UNIT_SEPARATOR).ok_or_else(|| {
                errors::core::source_unit_id_invalid_address()
                    .identity("source_unit")
                    .error()
            })?;
        if rift_protocol::identity::released_source_resolver_is_valid(resolver) {
            let (owner, path) = rift_protocol::identity::parse_released_source_identity(value)
                .map_err(|_| {
                    errors::core::source_unit_id_invalid_address()
                        .identity("source_unit")
                        .error()
                })?;
            return Self::for_owner(owner, &path);
        }
        let resolver = SourceResolverId::new(resolver).map_err(|cause| {
            errors::core::source_unit_id_invalid_resolver()
                .identity("source_unit")
                .cause(cause)
                .error()
        })?;
        let decoded = decode_unit_key(encoded_key)?;
        let key = SourcePath::new(decoded).map_err(|cause| {
            errors::core::source_unit_id_invalid_key()
                .identity("source_unit")
                .cause(cause)
                .error()
        })?;
        let identity = Self::new(resolver, key)?;
        if identity.to_string() != value {
            return errors::core::source_unit_id_non_canonical()
                .identity("source_unit")
                .fail();
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
        if let Some(owner) = &self.owner {
            return rift_protocol::identity::released_source_identity(owner, self.key.as_str())
                .map_or(usize::MAX, |value| value.len());
        }
        SOURCE_UNIT_URI_PREFIX.len()
            + self.resolver.as_str().len()
            + SOURCE_UNIT_SEPARATOR_BYTES
            + self
                .key
                .as_str()
                .bytes()
                .map(|byte| {
                    if rift_protocol::identity::source_unit_key_byte_is_safe(byte) {
                        1
                    } else {
                        PERCENT_ESCAPE_BYTES
                    }
                })
                .sum::<usize>()
    }
}

impl FromStr for SourceUnitId {
    type Err = RiftError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for SourceUnitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(owner) = &self.owner {
            let value = rift_protocol::identity::released_source_identity(owner, self.key.as_str())
                .map_err(|_| fmt::Error)?;
            return formatter.write_str(&value);
        }
        write!(
            formatter,
            "{SOURCE_UNIT_URI_PREFIX}{}{SOURCE_UNIT_SEPARATOR}",
            self.resolver
        )?;
        formatter.write_str(&rift_protocol::identity::encode_source_unit_key(
            self.key.as_str(),
        ))
    }
}

fn decode_unit_key(value: &str) -> Result<String, RiftError> {
    rift_protocol::identity::decode_source_unit_key(value).map_err(|_| {
        errors::core::source_unit_id_invalid_encoding()
            .identity("source_unit")
            .error()
    })
}

/// Invalid zero revision.
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
            /// Returns [`RiftError`] for zero.
            pub fn new(value: u64) -> Result<Self, RiftError> {
                NonZeroU64::new(value)
                    .map(Self)
                    .ok_or_else(|| errors::core::revision_zero().error())
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
    use rift_error::RiftError;
    use std::fmt::Write as _;
    use std::hash::{Hash as _, Hasher as _};
    use std::str::FromStr as _;

    use super::{
        CompositionId, CompositionRevision, IndexRevision, ModelId, ModelRevision, ProviderId,
        ProviderRevision, ProviderSymbolId, SourceResolverId, SourceRevision, SourceUnitId,
        SymbolId, TreeRevision, WorkspaceId, encode_path, parse_symbol_identity, symbol_identity,
    };
    use crate::constants::{
        SOURCE_RESOLVER_ID_BYTES_MAX, SOURCE_UNIT_ID_BYTES_MAX, SOURCE_UNIT_URI_PREFIX,
    };
    use crate::{PackageIdentity, ProjectPath, SourcePath};

    #[test]
    fn cloned_identity_values_share_text_and_keep_value_semantics() {
        let identity = ProviderSymbolId::new("provider.symbol").expect("valid identity");
        let clone = identity.clone();
        let later = ProviderSymbolId::new("provider.zymbol").expect("valid identity");

        assert!(std::sync::Arc::ptr_eq(&identity.0, &clone.0));
        assert_eq!(identity, clone);
        assert!(identity < later);
        assert_eq!(identity.to_string(), "provider.symbol");
        assert!(ProviderSymbolId::new("").is_err());

        let mut identity_hash = std::collections::hash_map::DefaultHasher::new();
        identity.hash(&mut identity_hash);
        let mut clone_hash = std::collections::hash_map::DefaultHasher::new();
        clone.hash(&mut clone_hash);
        assert_eq!(identity_hash.finish(), clone_hash.finish());

        let resolver = SourceResolverId::new("rift.sources.project").expect("valid resolver");
        let resolver_clone = resolver.clone();
        assert!(std::sync::Arc::ptr_eq(&resolver.0, &resolver_clone.0));
        assert_eq!(resolver, resolver_clone);
        assert_eq!(resolver.to_string(), "rift.sources.project");
        assert!(SourceResolverId::new("Rift").is_err());
    }

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
        let path = format!("cargo/crates.io/beacon@1.0.0/{}lib.rs", "a/".repeat(490));
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
            ProviderSymbolId::new("rust:item")
                .expect("identity is valid")
                .to_string(),
            "rust:item"
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
        let parsed = SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs")
            .expect("canonical source-unit identity parses");
        let expected = SourceUnitId::new(
            SourceResolverId::new("rift.sources.project").expect("valid resolver"),
            SourcePath::new("src/lib.rs").expect("valid key"),
        )
        .expect("source-unit fixture is valid");
        assert_eq!(parsed, expected);
        let invalid_resolver = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("uppercase resolver is invalid");
        assert_eq!(
            invalid_resolver.slug().as_str(),
            "rift.core.source_unit_id_invalid_resolver"
        );
        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical escape is invalid");
        assert_eq!(
            non_canonical.slug().as_str(),
            "rift.core.source_unit_id_non_canonical"
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
        let over_bound = SourceUnitId::new(
            resolver,
            SourcePath::new(over_key).expect("decoded key remains bounded"),
        )
        .expect_err("identity above the encoded bound is invalid");
        assert_eq!(
            over_bound.slug().as_str(),
            "rift.core.source_unit_id_too_long"
        );
    }

    #[test]
    fn resolver_identity_reports_registered_identity() {
        let resolver_error = SourceResolverId::new("Rift").expect_err("uppercase is invalid");
        assert_eq!(
            resolver_error.slug().as_str(),
            "rift.core.resolver_id_invalid_character"
        );
        assert_eq!(
            resolver_error.to_string(),
            "source resolver identity is not canonical lowercase syntax: identity source_resolver; \
             correct the reported field and resend the request"
        );

        let unit_error = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("invalid resolver must fail unit identity");
        assert!(std::error::Error::source(&unit_error).is_some());
        assert_eq!(
            unit_error.to_string(),
            "source-unit resolver identity is invalid: source resolver identity is not canonical lowercase syntax: identity source_resolver; correct the reported field and resend the request: identity source_unit; \
             correct the reported field and resend the request"
        );
    }

    #[test]
    fn resolver_identity_rejects_empty_and_oversized_values() {
        let empty_error = SourceResolverId::new("").expect_err("empty resolver is invalid");
        assert_eq!(empty_error.slug().as_str(), "rift.core.resolver_id_empty");

        let oversized = "a".repeat(SOURCE_RESOLVER_ID_BYTES_MAX + 1);
        let oversized_error =
            SourceResolverId::new(oversized.as_str()).expect_err("oversized resolver");
        assert_eq!(
            oversized_error.slug().as_str(),
            "rift.core.resolver_id_too_long"
        );
    }

    #[test]
    fn unit_identity_rejects_malformed_addresses() {
        let missing_prefix =
            SourceUnitId::parse("not-a-rift-source-uri").expect_err("missing prefix is invalid");
        assert_eq!(
            missing_prefix.slug().as_str(),
            "rift.core.source_unit_id_invalid_address"
        );
        let missing_separator = SourceUnitId::parse("rift://source/resolverwithoutseparator")
            .expect_err("missing separator is invalid");
        assert_eq!(
            missing_separator.slug().as_str(),
            "rift.core.source_unit_id_invalid_address"
        );

        let oversized = format!(
            "{SOURCE_UNIT_URI_PREFIX}{}",
            "a".repeat(SOURCE_UNIT_ID_BYTES_MAX)
        );
        let over_bound = SourceUnitId::parse(&oversized).expect_err("oversized address");
        assert_eq!(
            over_bound.slug().as_str(),
            "rift.core.source_unit_id_too_long"
        );
    }

    #[test]
    fn source_unit_parser_keeps_custom_keys_separate_from_released_owners() {
        for value in [
            "rift://source/project/registry.example/demo@1.0.0/file.rs",
            "rift://source/custom/registry.example/demo@1.0.0/file.rs",
            "rift://source/project/src/file~2.rs",
        ] {
            let id = SourceUnitId::parse(value).expect("custom source key");
            assert_eq!(id.to_string(), value);
            assert_eq!(
                id.key().as_str(),
                value.splitn(5, '/').nth(4).expect("source key")
            );
            assert!(id.owner.is_none());
        }
        let runtime = "rift://source/stdlib/cpython@3.12.9/Lib/sys.py";
        let id = SourceUnitId::parse(runtime).expect("runtime source owner");
        assert!(matches!(
            id.owner.as_ref(),
            Some(rift_protocol::identity::SymbolOwner::Runtime { .. })
        ));
        assert_eq!(id.to_string(), runtime);
        for value in [
            "rift://source/project/a//b",
            "rift://source/stdlib/python/Lib/sys.py",
        ] {
            assert!(SourceUnitId::parse(value).is_err(), "{value}");
        }
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
            assert_eq!(
                error.slug().as_str(),
                "rift.core.source_unit_id_invalid_encoding",
                "address={address}"
            );
        }
    }

    #[test]
    fn unit_identity_reports_decoded_key_violation_as_source() {
        let error = SourceUnitId::parse("rift://source/rift.sources.project/..")
            .expect_err("dot-segment key must be rejected");
        assert_eq!(
            error.slug().as_str(),
            "rift.core.source_unit_id_invalid_key"
        );
        assert_eq!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<RiftError>())
                .map(|source| source.slug().as_str()),
            Some("rift.core.path_dot_segment")
        );
        assert!(std::error::Error::source(&error).is_some());

        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical encoding must be rejected");
        assert_eq!(
            non_canonical.slug().as_str(),
            "rift.core.source_unit_id_non_canonical"
        );
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
        assert!(error.to_string().contains("identity is empty"));
        assert_eq!(error.slug().as_str(), "rift.core.identity_invalid");
        let _: &dyn std::error::Error = &error;
    }

    #[test]
    fn revisions_are_non_zero() {
        assert!(TreeRevision::new(0).is_err());
        assert_eq!(TreeRevision::new(7).expect("revision is non-zero").get(), 7);
    }

    #[test]
    fn revision_error_displays_and_implements_std_error() {
        let error = TreeRevision::new(0).expect_err("zero revision must be rejected");
        assert!(error.to_string().contains("revision is zero"));
        assert_eq!(error.slug().as_str(), "rift.core.revision_zero");
        let _: &dyn std::error::Error = &error;
    }

    fn context_pairs(error: &RiftError) -> Vec<(&'static str, String)> {
        error.context().collect()
    }

    #[test]
    fn source_unit_id_error_context_covers_every_kind() {
        let too_long = SourceUnitId::parse(&format!(
            "{SOURCE_UNIT_URI_PREFIX}{}",
            "a".repeat(SOURCE_UNIT_ID_BYTES_MAX)
        ))
        .expect_err("oversized address must be rejected");
        assert_eq!(
            too_long.slug().as_str(),
            "rift.core.source_unit_id_too_long"
        );
        assert_eq!(
            context_pairs(&too_long),
            vec![("identity", "source_unit".to_owned()),]
        );

        let invalid_address = SourceUnitId::parse("not-a-rift-source-uri")
            .expect_err("missing prefix must be rejected");
        assert_eq!(
            invalid_address.slug().as_str(),
            "rift.core.source_unit_id_invalid_address"
        );

        let invalid_resolver = SourceUnitId::parse("rift://source/Rift/src/lib.rs")
            .expect_err("uppercase resolver must be rejected");
        assert_eq!(
            invalid_resolver.slug().as_str(),
            "rift.core.source_unit_id_invalid_resolver"
        );
        let cause = std::error::Error::source(&invalid_resolver)
            .and_then(|source| source.downcast_ref::<RiftError>())
            .expect("invalid resolver is retained as cause");
        assert_eq!(
            cause.slug().as_str(),
            "rift.core.resolver_id_invalid_character"
        );

        let invalid_encoding = SourceUnitId::parse("rift://source/r/%G0")
            .expect_err("malformed percent escape must be rejected");
        assert_eq!(
            invalid_encoding.slug().as_str(),
            "rift.core.source_unit_id_invalid_encoding"
        );

        let invalid_key = SourceUnitId::parse("rift://source/rift.sources.project/..")
            .expect_err("dot-segment key must be rejected");
        assert_eq!(
            invalid_key.slug().as_str(),
            "rift.core.source_unit_id_invalid_key"
        );
        let cause = std::error::Error::source(&invalid_key)
            .and_then(|source| source.downcast_ref::<RiftError>())
            .expect("invalid source path is retained as cause");
        assert_eq!(cause.slug().as_str(), "rift.core.path_dot_segment");

        let non_canonical = SourceUnitId::parse("rift://source/rift.sources.project/src%2flib.rs")
            .expect_err("non-canonical encoding must be rejected");
        assert_eq!(
            non_canonical.slug().as_str(),
            "rift.core.source_unit_id_non_canonical"
        );
    }

    #[test]
    fn every_identity_family_validates_and_displays() {
        assert_eq!(
            WorkspaceId::new("workspace").expect("valid id").to_string(),
            "workspace"
        );
        assert_eq!(
            SymbolId::new("python:rift.main")
                .expect("valid id")
                .to_string(),
            "python:rift.main"
        );
        assert_eq!(
            ProviderId::new("syntax").expect("valid id").to_string(),
            "syntax"
        );
        assert_eq!(
            CompositionId::new("default").expect("valid id").to_string(),
            "default"
        );
        assert_eq!(
            ModelId::new("owner/model@revision")
                .expect("valid id")
                .to_string(),
            "owner/model@revision"
        );
    }

    #[test]
    fn every_revision_family_preserves_value() {
        assert_eq!(SourceRevision::new(1).expect("valid revision").get(), 1);
        assert_eq!(ProviderRevision::new(2).expect("valid revision").get(), 2);
        assert_eq!(
            CompositionRevision::new(3).expect("valid revision").get(),
            3
        );
        assert_eq!(IndexRevision::new(4).expect("valid revision").get(), 4);
        assert_eq!(ModelRevision::new(5).expect("valid revision").get(), 5);
    }

    fn package(manager: &str, name: &str, version: &str) -> PackageIdentity {
        PackageIdentity {
            manager: manager.to_owned(),
            registry: match manager {
                "cargo" => "crates.io",
                "npm" => "npmjs.org",
                "pypi" => "pypi.org",
                _ => "registry.example",
            }
            .to_owned(),
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
            "rift://source/cargo/crates.io/helper@0.1.0/src/lib.rs"
        );
        assert_eq!(unit.resolver().as_str(), "cargo");
        assert_eq!(unit.key().as_str(), "src/lib.rs");
    }

    #[test]
    fn package_unit_refuses_a_manager_that_is_no_resolver_identity() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        let error = SourceUnitId::for_package(&package("Cargo", "helper", "0.1.0"), &path)
            .expect_err("uppercase manager");
        assert_eq!(error.slug().as_str(), "rift.core.identity_invalid");
    }

    #[test]
    fn package_unit_refuses_a_version_that_breaks_the_source_path_rules() {
        let path = ProjectPath::new("src/lib.rs").expect("valid path");
        assert!(
            SourceUnitId::for_package(&package("cargo", "helper", "0.1.0\\beta"), &path).is_err()
        );
    }
}
