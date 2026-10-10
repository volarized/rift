//! Canonical ownership and logical paths for symbol identities.

use std::num::NonZeroU32;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::read::Language;

/// Maximum encoded size of a symbol identity in bytes.
pub const SYMBOL_ID_BYTES_MAX: usize = 8_192;

/// Prefix shared by canonical symbol addresses.
pub const SYMBOL_URI_PREFIX: &str = "rift://symbol/";

/// Structural wire pattern. The codec also validates exact versions and canonical spelling.
pub const SYMBOL_ID_PATTERN: &str = concat!(
    r"^rift://symbol/(?:local(?:@[a-z][a-z0-9_-]*)?/",
    r"|stdlib/[a-z][a-z0-9_-]*@[^/?#]+/",
    r"|[a-z][a-z0-9_-]*/[^/?#]+/(?:@[a-z0-9._-]+/)?[a-z0-9._-]+@[^/?#]+/)",
    r"[a-z][a-z0-9._-]*(?::[a-z][a-z0-9._-]*)?/",
    r"(?:[A-Za-z0-9._!$&'()*+,;=:@-]|%[0-9A-F]{2})+",
    r"(?:/(?:[A-Za-z0-9._!$&'()*+,;=:@-]|%[0-9A-F]{2})+)*",
    r"(?:~[1-9][0-9]*\?rev=[0-9a-f]{64})?$"
);

/// Revision query selecting an occurrence's immutable source.
const REVISION_QUERY_PREFIX: &str = "?rev=";

/// Number separator for a revision-bound occurrence.
const OCCURRENCE_SEPARATOR: char = '~';

/// Encoded width of a complete SHA-256 revision.
const REVISION_HEX_BYTES: usize = 64;

/// RFC 3986 path characters, excluding hierarchy and occurrence delimiters.
const COMPONENT_ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'_')
    .remove(b'-')
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
    .remove(b'@');

/// Defining owner of a logical symbol.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SymbolOwner {
    /// The project served by the current workspace.
    Local,
    /// An explicitly registered project in the current connection.
    NamedLocal {
        /// Accepted registration name.
        name: String,
    },
    /// One exact release from its defining registry.
    Package {
        /// Package manager or ecosystem name.
        manager: String,
        /// Canonical registry authority and optional endpoint path.
        registry: String,
        /// Canonical package name, including an npm scope when present.
        name: String,
        /// Exact release version.
        version: String,
    },
    /// Standard-library object owned by a runtime or compiler.
    Runtime {
        /// Canonical runtime or compiler name.
        runtime: String,
        /// Exact runtime or compiler version.
        version: String,
    },
}

/// Occurrence number bound to one complete immutable source digest.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SymbolOccurrence {
    number: NonZeroU32,
    revision: String,
}

impl SymbolOccurrence {
    /// Validates an occurrence number and complete SHA-256 revision spelling.
    ///
    /// # Errors
    /// Returns a violation for zero numbers or noncanonical revisions.
    pub fn new(number: u32, revision: String) -> Result<Self, SymbolIdentityViolation> {
        let number = NonZeroU32::new(number).ok_or(SymbolIdentityViolation::Occurrence)?;
        let valid_width = revision.len() == REVISION_HEX_BYTES;
        let valid_digits = revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid_width || !valid_digits {
            return Err(SymbolIdentityViolation::Revision);
        }
        Ok(Self { number, revision })
    }

    /// Returns the positive occurrence number.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number.get()
    }

    /// Returns the complete immutable source revision.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// Validated owner, language and logical hierarchy of one symbol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolIdentity {
    owner: SymbolOwner,
    language: Language,
    qualified_path: Vec<String>,
    occurrence: Option<SymbolOccurrence>,
}

impl schemars::JsonSchema for SymbolIdentity {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SymbolIdentity".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": SYMBOL_ID_BYTES_MAX,
            "pattern": SYMBOL_ID_PATTERN,
            "description": "Canonical logical symbol identity with local, registered local, package or runtime ownership. The portable codec validates exact versions, UTF-8 and canonical percent-encoding before lookup.",
            "examples": [
                "rift://symbol/local/rust/app/parser/parse",
                "rift://symbol/local@cloud/rust/rift_cloud_service/resolution/resolve_entries",
                "rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node/buffer/Buffer",
                "rift://symbol/stdlib/cpython@3.12.9/python/builtins/len"
            ]
        })
    }
}

/// A symbol identity outside its canonical wire form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolIdentityViolation {
    /// The encoded identity exceeds its byte limit.
    Length,
    /// The address has missing or misplaced components.
    Structure,
    /// The owner has an invalid or noncanonical spelling.
    Owner,
    /// The language does not use its accepted identity spelling.
    Language,
    /// A logical name is empty, a dot segment, or contains controls.
    QualifiedPath,
    /// Percent-decoded bytes are not UTF-8.
    Encoding,
    /// Rendering the parsed identity changes its spelling.
    Noncanonical,
    /// An occurrence number is absent, zero, or invalid.
    Occurrence,
    /// A revision is absent or is not a complete lowercase SHA-256 digest.
    Revision,
}

impl SymbolIdentity {
    /// Constructs a canonical identity from accepted ownership and logical names.
    ///
    /// # Errors
    /// Returns a violation for invalid components or an encoded identity above the byte limit.
    pub fn new(
        owner: SymbolOwner,
        language: Language,
        qualified_path: Vec<String>,
    ) -> Result<Self, SymbolIdentityViolation> {
        let identity = Self {
            owner,
            language,
            qualified_path,
            occurrence: None,
        };
        identity.validate()?;
        Ok(identity)
    }

    /// Binds this identity to an occurrence in one immutable source revision.
    ///
    /// # Errors
    /// Returns a length violation if the complete encoded identity exceeds its limit.
    pub fn with_occurrence(
        mut self,
        occurrence: SymbolOccurrence,
    ) -> Result<Self, SymbolIdentityViolation> {
        self.occurrence = Some(occurrence);
        self.validate_length()?;
        Ok(self)
    }

    /// Parses a canonical symbol address without performing lookup or I/O.
    ///
    /// Work and decoded allocation are bounded by [`SYMBOL_ID_BYTES_MAX`].
    ///
    /// # Errors
    /// Returns a violation for malformed ownership, hierarchy, qualifiers or encoding.
    pub fn parse(value: &str) -> Result<Self, SymbolIdentityViolation> {
        if value.len() > SYMBOL_ID_BYTES_MAX {
            return Err(SymbolIdentityViolation::Length);
        }
        let remainder = value
            .strip_prefix(SYMBOL_URI_PREFIX)
            .ok_or(SymbolIdentityViolation::Structure)?;
        let (address, occurrence) = parse_occurrence(remainder)?;
        let mut segments = address.split('/');
        let scope = next_segment(&mut segments)?;
        let owner = parse_owner(scope, &mut segments)?;
        let language = Language::from_identity_segment(next_segment(&mut segments)?)
            .map_err(|_| SymbolIdentityViolation::Language)?;
        let qualified_path = segments
            .map(decode_component)
            .collect::<Result<Vec<_>, _>>()?;
        let mut identity = Self::new(owner, language, qualified_path)?;
        identity.occurrence = occurrence;
        if identity.wire_identity() != value {
            return Err(SymbolIdentityViolation::Noncanonical);
        }
        Ok(identity)
    }

    /// Returns the defining owner.
    #[must_use]
    pub fn owner(&self) -> &SymbolOwner {
        &self.owner
    }

    /// Returns the semantic language and optional dialect.
    #[must_use]
    pub fn language(&self) -> &Language {
        &self.language
    }

    /// Returns decoded names in hierarchy order.
    #[must_use]
    pub fn qualified_path(&self) -> &[String] {
        &self.qualified_path
    }

    /// Returns the revision-bound occurrence when this identity requires one.
    #[must_use]
    pub fn occurrence(&self) -> Option<&SymbolOccurrence> {
        self.occurrence.as_ref()
    }

    /// Serializes the validated identity with readable hierarchy and canonical escaping.
    #[must_use]
    pub fn wire_identity(&self) -> String {
        let owner = self.owner.wire_owner();
        let language = self.language.identity_segment();
        let path = self
            .qualified_path
            .iter()
            .map(|part| encode_component(part))
            .collect::<Vec<_>>()
            .join("/");
        let mut value = format!("{SYMBOL_URI_PREFIX}{owner}/{language}/{path}");
        if let Some(occurrence) = &self.occurrence {
            value.push(OCCURRENCE_SEPARATOR);
            value.push_str(&occurrence.number().to_string());
            value.push_str(REVISION_QUERY_PREFIX);
            value.push_str(occurrence.revision());
        }
        value
    }

    fn validate(&self) -> Result<(), SymbolIdentityViolation> {
        self.validate_input_length()?;
        self.owner.validate()?;
        Language::from_identity_segment(&self.language.identity_segment())
            .map_err(|_| SymbolIdentityViolation::Language)?;
        if self.qualified_path.is_empty()
            || self
                .qualified_path
                .iter()
                .any(|part| !valid_component(part))
        {
            return Err(SymbolIdentityViolation::QualifiedPath);
        }
        self.validate_length()
    }

    fn validate_input_length(&self) -> Result<(), SymbolIdentityViolation> {
        let owner_bytes = self.owner.input_bytes();
        let path_bytes = self.qualified_path.iter().try_fold(0usize, |total, part| {
            total
                .checked_add(part.len())?
                .checked_add(1)
                .filter(|total| *total <= SYMBOL_ID_BYTES_MAX)
        });
        let language_bytes = self
            .language
            .name
            .len()
            .saturating_add(self.language.dialect.as_ref().map_or(0, String::len));
        let total = path_bytes
            .and_then(|path| path.checked_add(owner_bytes))
            .and_then(|total| total.checked_add(language_bytes));
        if total.is_none_or(|total| total > SYMBOL_ID_BYTES_MAX) {
            return Err(SymbolIdentityViolation::Length);
        }
        Ok(())
    }

    fn validate_length(&self) -> Result<(), SymbolIdentityViolation> {
        if self.wire_identity().len() > SYMBOL_ID_BYTES_MAX {
            return Err(SymbolIdentityViolation::Length);
        }
        Ok(())
    }
}

impl SymbolOwner {
    fn input_bytes(&self) -> usize {
        match self {
            Self::Local => 0,
            Self::NamedLocal { name } => name.len(),
            Self::Package {
                manager,
                registry,
                name,
                version,
            } => [manager, registry, name, version]
                .iter()
                .fold(0usize, |total, value| total.saturating_add(value.len())),
            Self::Runtime { runtime, version } => runtime.len().saturating_add(version.len()),
        }
    }

    /// Validates an owner before it enters a source or symbol identity.
    ///
    /// # Errors
    /// Returns the violated owner or length bound.
    pub fn validate(&self) -> Result<(), SymbolIdentityViolation> {
        if self.input_bytes() > SYMBOL_ID_BYTES_MAX {
            return Err(SymbolIdentityViolation::Length);
        }
        let accepted = match self {
            Self::Local => true,
            Self::NamedLocal { name } => valid_registration_name(name),
            Self::Package {
                manager,
                registry,
                name,
                version,
            } => {
                manager.len() <= 128
                    && registry.len() <= 4096
                    && name.len() <= 4096
                    && version.len() <= 4096
                    && valid_manager(manager)
                    && valid_registry(registry)
                    && valid_package_name(manager, name)
                    && valid_version(manager, version)
            }
            Self::Runtime { runtime, version } => {
                runtime.len() <= 128
                    && version.len() <= 4096
                    && valid_word(runtime)
                    && canonical_semver(version)
            }
        };
        if !accepted {
            return Err(SymbolIdentityViolation::Owner);
        }
        Ok(())
    }

    fn wire_owner(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::NamedLocal { name } => format!("local@{name}"),
            Self::Package {
                manager,
                registry,
                name,
                version,
            } => {
                let name = name
                    .split('/')
                    .map(encode_component)
                    .collect::<Vec<_>>()
                    .join("/");
                format!(
                    "{manager}/{}/{name}@{}",
                    encode_component(registry),
                    encode_component(version)
                )
            }
            Self::Runtime { runtime, version } => {
                format!("stdlib/{runtime}@{}", encode_component(version))
            }
        }
    }
}

impl Serialize for SymbolIdentity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire_identity())
    }
}

impl<'de> Deserialize<'de> for SymbolIdentity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|violation| {
            serde::de::Error::custom(format!(
                "invalid symbol identity: {violation:?}; supply a canonical rift://symbol/ address"
            ))
        })
    }
}

fn parse_occurrence(
    value: &str,
) -> Result<(&str, Option<SymbolOccurrence>), SymbolIdentityViolation> {
    let Some((address, revision)) = value.split_once(REVISION_QUERY_PREFIX) else {
        return Ok((value, None));
    };
    let (address, number) = address
        .rsplit_once(OCCURRENCE_SEPARATOR)
        .ok_or(SymbolIdentityViolation::Occurrence)?;
    let number = number
        .parse::<u32>()
        .map_err(|_| SymbolIdentityViolation::Occurrence)?;
    Ok((
        address,
        Some(SymbolOccurrence::new(number, revision.to_owned())?),
    ))
}

fn parse_owner<'a>(
    scope: &str,
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<SymbolOwner, SymbolIdentityViolation> {
    match scope {
        "local" => Ok(SymbolOwner::Local),
        "stdlib" => {
            let (runtime, version) = parse_release(next_segment(segments)?)?;
            Ok(SymbolOwner::Runtime { runtime, version })
        }
        value if value.starts_with("local@") => Ok(SymbolOwner::NamedLocal {
            name: value["local@".len()..].to_owned(),
        }),
        manager => parse_package_owner(manager, segments),
    }
}

fn parse_package_owner<'a>(
    manager: &str,
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<SymbolOwner, SymbolIdentityViolation> {
    let registry = decode_component(next_segment(segments)?)?;
    let first = next_segment(segments)?;
    let (name, version) = match (manager, first.starts_with('@')) {
        ("npm", true) => {
            let (name, version) = parse_release(next_segment(segments)?)?;
            (format!("{}/{name}", decode_component(first)?), version)
        }
        _ => parse_release(first)?,
    };
    Ok(SymbolOwner::Package {
        manager: manager.to_owned(),
        registry,
        name,
        version,
    })
}

fn parse_release(value: &str) -> Result<(String, String), SymbolIdentityViolation> {
    let (name, version) = value
        .rsplit_once('@')
        .ok_or(SymbolIdentityViolation::Owner)?;
    Ok((decode_component(name)?, decode_component(version)?))
}

fn next_segment<'a>(
    segments: &mut impl Iterator<Item = &'a str>,
) -> Result<&'a str, SymbolIdentityViolation> {
    segments
        .next()
        .filter(|value| !value.is_empty())
        .ok_or(SymbolIdentityViolation::Structure)
}

fn decode_component(value: &str) -> Result<String, SymbolIdentityViolation> {
    percent_decode_str(value)
        .decode_utf8()
        .map(std::borrow::Cow::into_owned)
        .map_err(|_| SymbolIdentityViolation::Encoding)
}

fn encode_component(value: &str) -> String {
    utf8_percent_encode(value, COMPONENT_ESCAPE_SET).to_string()
}

fn valid_component(value: &str) -> bool {
    let present = !value.is_empty();
    let ordinary = ![".", ".."].contains(&value);
    let controls_absent = !value.chars().any(char::is_control);
    present && ordinary && controls_absent
}

fn valid_word(value: &str) -> bool {
    let initial_letter = value.as_bytes().first().is_some_and(u8::is_ascii_lowercase);
    let accepted_bytes = value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte));
    initial_letter && accepted_bytes
}

/// Validates the connection's canonical named-project spelling.
#[must_use]
pub fn valid_registration_name(value: &str) -> bool {
    let reserved = ["local", "global", "all"].contains(&value);
    valid_word(value) && !reserved
}

fn valid_manager(value: &str) -> bool {
    let reserved = ["local", "global", "all", "stdlib"].contains(&value);
    valid_word(value) && !reserved
}

fn valid_registry(value: &str) -> bool {
    let Ok(url) = url::Url::parse(&format!("https://{value}")) else {
        return false;
    };
    let credentials_absent = url.username().is_empty() && url.password().is_none();
    let selectors_absent = url.query().is_none() && url.fragment().is_none();
    let authority_present = url.host_str().is_some();
    let canonical = url.as_str().strip_prefix("https://").unwrap_or_default();
    let canonical = if url.path() == "/" {
        canonical.strip_suffix('/').unwrap_or(canonical)
    } else {
        canonical
    };
    credentials_absent && selectors_absent && authority_present && canonical == value
}

/// Canonical registry authority and endpoint path from an accepted HTTPS URL.
///
/// Credentials and selectors are refused before an owner is produced. Resolver-specific
/// aliases are handled by the resolver that accepted the source.
///
/// # Errors
/// Returns an owner violation for an invalid endpoint, or a length violation at the bound.
pub fn canonical_registry_endpoint(value: &str) -> Result<String, SymbolIdentityViolation> {
    if value.is_empty() || value.len() > 4096 {
        return Err(SymbolIdentityViolation::Length);
    }
    let url = url::Url::parse(value).map_err(|_| SymbolIdentityViolation::Owner)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || value.chars().any(char::is_control)
    {
        return Err(SymbolIdentityViolation::Owner);
    }
    let endpoint = url
        .as_str()
        .strip_prefix("https://")
        .ok_or(SymbolIdentityViolation::Owner)?;
    let endpoint = if url.path() == "/" {
        endpoint.strip_suffix('/').unwrap_or(endpoint)
    } else {
        endpoint
    };
    if endpoint.len() > 4096 || !valid_registry(endpoint) {
        return Err(SymbolIdentityViolation::Owner);
    }
    Ok(endpoint.to_owned())
}

fn valid_package_name(manager: &str, value: &str) -> bool {
    match manager {
        "npm" if value.starts_with('@') => {
            let Some((scope, name)) = value[1..].split_once('/') else {
                return false;
            };
            valid_package_part(scope) && valid_package_part(name)
        }
        "pypi" => valid_package_part(value) && !value.contains(['_', '.']) && !value.contains("--"),
        _ => valid_package_part(value),
    }
}

fn valid_package_part(value: &str) -> bool {
    valid_component(value)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

fn valid_version(manager: &str, value: &str) -> bool {
    match manager {
        "cargo" | "npm" => canonical_semver(value),
        "pypi" => value
            .parse::<pep440_rs::Version>()
            .is_ok_and(|version| version.to_string() == value),
        _ => {
            value.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-+!".contains(&byte))
        }
    }
}

fn canonical_semver(value: &str) -> bool {
    semver::Version::parse(value).is_ok_and(|version| version.to_string() == value)
}

/// Canonical address of a released source file under its defining package or runtime.
///
/// # Errors
/// Returns a violation for an unresolved owner, invalid relative path or encoded length.
pub fn released_source_identity(
    owner: &SymbolOwner,
    path: &str,
) -> Result<String, SymbolIdentityViolation> {
    if owner.input_bytes().saturating_add(path.len()) > SYMBOL_ID_BYTES_MAX {
        return Err(SymbolIdentityViolation::Length);
    }
    owner.validate()?;
    if !matches!(
        owner,
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    ) {
        return Err(SymbolIdentityViolation::Owner);
    }
    if path.is_empty()
        || path
            .split('/')
            .any(|part| !valid_component(part) || part.contains('\\'))
    {
        return Err(SymbolIdentityViolation::QualifiedPath);
    }
    let path = path
        .split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/");
    let value = format!("rift://source/{}/{path}", owner.wire_owner());
    if value.len() > SYMBOL_ID_BYTES_MAX {
        return Err(SymbolIdentityViolation::Length);
    }
    Ok(value)
}

/// Defining owner and root-relative path of a canonical released source address.
///
/// # Errors
/// Returns a violation for malformed ownership, source path, encoding or spelling.
pub fn parse_released_source_identity(
    value: &str,
) -> Result<(SymbolOwner, String), SymbolIdentityViolation> {
    if value.len() > SYMBOL_ID_BYTES_MAX {
        return Err(SymbolIdentityViolation::Length);
    }
    let mut parts = value
        .strip_prefix("rift://source/")
        .ok_or(SymbolIdentityViolation::Structure)?
        .split('/');
    let scope = next_segment(&mut parts)?;
    let owner = parse_owner(scope, &mut parts)?;
    let path = parts
        .map(|part| {
            let decoded = decode_component(part)?;
            if decoded.contains('/') {
                return Err(SymbolIdentityViolation::QualifiedPath);
            }
            Ok(decoded)
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if released_source_identity(&owner, &path)? != value {
        return Err(SymbolIdentityViolation::Noncanonical);
    }
    Ok((owner, path))
}

#[cfg(test)]
mod tests;
