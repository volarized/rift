use rift_error::{RiftError, errors};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::SourceDigest;
use crate::read::ProjectPath;

/// Expanded compatibility tags retained for one selected artifact.
///
/// A published wheel name compresses each interpreter, ABI, and platform it supports, and the
/// tags are their product. [`PACKAGE_ARTIFACT_BYTES_MAX`] leaves 64 bytes for each tag.
pub const PACKAGE_ARTIFACT_TAGS_MAX: usize = 1_024;
/// Aggregate UTF-8 bytes retained for one selected artifact.
pub const PACKAGE_ARTIFACT_BYTES_MAX: usize = 65_536;

/// A selected artifact field the analyzer cannot accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackageArtifactViolation {
    /// An input or expanded tag set exceeds its bound.
    Length { observed: usize, bound: usize },
    /// A compatibility tag has invalid components.
    Tag,
    /// The artifact filename is not one root-relative basename.
    Path,
}

impl From<PackageArtifactViolation> for RiftError {
    fn from(violation: PackageArtifactViolation) -> Self {
        let detail = match violation {
            PackageArtifactViolation::Length { .. } => "Length",
            PackageArtifactViolation::Tag => "Tag",
            PackageArtifactViolation::Path => "Path",
        };
        let error = errors::analysis::package_artifact_invalid()
            .field("artifact")
            .violation(detail);
        match violation {
            PackageArtifactViolation::Length { observed, bound } => {
                error.observed(observed as u64).bound(bound as u64).error()
            }
            PackageArtifactViolation::Tag | PackageArtifactViolation::Path => error.error(),
        }
    }
}

fn within(observed: usize, bound: usize) -> Result<(), PackageArtifactViolation> {
    if observed <= bound {
        return Ok(());
    }
    Err(PackageArtifactViolation::Length { observed, bound })
}

/// One expanded Python, ABI and platform compatibility tag.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "WheelTagInput", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct WheelTag {
    /// Python implementation and version required by the artifact.
    python: String,
    /// ABI required by the artifact.
    abi: String,
    /// Platform required by the artifact.
    platform: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WheelTagInput {
    python: String,
    abi: String,
    platform: String,
}

impl TryFrom<WheelTagInput> for WheelTag {
    type Error = RiftError;

    fn try_from(value: WheelTagInput) -> Result<Self, Self::Error> {
        Self::new(&value.python, &value.abi, &value.platform)
    }
}

impl WheelTag {
    /// Accepts an expanded tag and retains its lowercase spelling.
    ///
    /// # Errors
    /// Returns a violation for invalid components or an excessive byte count.
    pub fn new(python: &str, abi: &str, platform: &str) -> Result<Self, RiftError> {
        let fields = [python, abi, platform];
        for value in fields {
            within(value.len(), PACKAGE_ARTIFACT_BYTES_MAX)?;
        }
        let bytes = fields.iter().map(|value| lowercase_bytes(value)).sum();
        within(bytes, PACKAGE_ARTIFACT_BYTES_MAX)?;
        if !python_identifier_is_valid(python)
            || !tag_component_is_valid(abi)
            || !tag_component_is_valid(platform)
        {
            return Err(PackageArtifactViolation::Tag.into());
        }
        Ok(Self {
            python: python.to_lowercase(),
            abi: abi.to_lowercase(),
            platform: platform.to_lowercase(),
        })
    }

    /// Expands a compressed compatibility tag under count and byte bounds.
    ///
    /// A Python part that names no interpreter, such as `7` in `py2.7`, adds no tag. At least
    /// one part names one.
    ///
    /// # Errors
    /// Returns a violation before expansion for malformed or excessive input.
    pub fn parse(value: &str) -> Result<Vec<Self>, RiftError> {
        within(value.len(), PACKAGE_ARTIFACT_BYTES_MAX)?;
        let mut fields = value.split('-');
        let python = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        let abi = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        let platform = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        if fields.next().is_some()
            || !python.split('.').all(tag_component_is_valid)
            || !python.split('.').any(python_identifier_is_valid)
            || !abi.split('.').all(tag_component_is_valid)
            || !platform.split('.').all(tag_component_is_valid)
        {
            return Err(PackageArtifactViolation::Tag.into());
        }
        let interpreters = || {
            python
                .split('.')
                .filter(|part| python_identifier_is_valid(part))
        };
        let counts = [
            interpreters().count(),
            abi.split('.').count(),
            platform.split('.').count(),
        ];
        let count = counts.iter().try_fold(1_usize, |count, next| {
            count
                .checked_mul(*next)
                .ok_or(PackageArtifactViolation::Length {
                    observed: usize::MAX,
                    bound: PACKAGE_ARTIFACT_TAGS_MAX,
                })
        })?;
        within(count, PACKAGE_ARTIFACT_TAGS_MAX)?;
        let field_bytes = [
            interpreters().map(lowercase_bytes).sum::<usize>(),
            abi.split('.').map(lowercase_bytes).sum(),
            platform.split('.').map(lowercase_bytes).sum(),
        ];
        let bytes = field_bytes
            .iter()
            .zip(counts)
            .map(|(bytes, field_count)| bytes * (count / field_count))
            .sum();
        within(bytes, PACKAGE_ARTIFACT_BYTES_MAX)?;
        let mut tags = Vec::with_capacity(count);
        for python in interpreters() {
            for abi in abi.split('.') {
                for platform in platform.split('.') {
                    tags.push(Self::new(python, abi, platform)?);
                }
            }
        }
        tags.sort_unstable();
        tags.dedup();
        Ok(tags)
    }

    /// Python implementation and version in this expanded tag.
    #[must_use]
    pub fn python(&self) -> &str {
        &self.python
    }

    /// ABI in this expanded tag.
    #[must_use]
    pub fn abi(&self) -> &str {
        &self.abi
    }

    /// Platform in this expanded tag.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    fn bytes(&self) -> usize {
        self.python.len() + self.abi.len() + self.platform.len()
    }
}

/// Whether a Python name follows the Unicode identifier grammar.
#[must_use]
pub fn python_identifier_is_valid(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || unicode_ident::is_xid_start(first))
        && characters.all(unicode_ident::is_xid_continue)
}

fn tag_component_is_valid(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['.', '-'])
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

fn lowercase_bytes(value: &str) -> usize {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(char::len_utf8)
        .sum()
}

/// The selected immutable package artifact and its observed compatibility tags.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(try_from = "PackageArtifactInput", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct PackageArtifact {
    /// Original archive filename, retained as one basename.
    filename: ProjectPath,
    /// Complete digest of the selected archive bytes.
    content_digest: SourceDigest,
    /// Expanded tags accepted from the selected wheel's filename and metadata.
    #[schemars(length(max = 1024))]
    tags: Vec<WheelTag>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageArtifactInput {
    filename: ProjectPath,
    content_digest: SourceDigest,
    tags: Vec<WheelTag>,
}

impl TryFrom<PackageArtifactInput> for PackageArtifact {
    type Error = RiftError;

    fn try_from(value: PackageArtifactInput) -> Result<Self, Self::Error> {
        Self::new(&value.filename.0, value.content_digest, &value.tags)
    }
}

impl PackageArtifact {
    /// Accepts captured artifact facts after enforcing aggregate bounds.
    ///
    /// # Errors
    /// Returns a violation for an invalid basename or excessive tag data.
    pub fn new(
        filename: &str,
        content_digest: SourceDigest,
        tags: &[WheelTag],
    ) -> Result<Self, RiftError> {
        within(tags.len(), PACKAGE_ARTIFACT_TAGS_MAX)?;
        let bytes = filename
            .len()
            .saturating_add(content_digest.as_str().len())
            .saturating_add(tags.iter().map(WheelTag::bytes).sum::<usize>());
        within(bytes, PACKAGE_ARTIFACT_BYTES_MAX)?;
        if !crate::identity::source_unit_path_is_valid(filename) || filename.contains('/') {
            return Err(PackageArtifactViolation::Path.into());
        }
        let mut tags = tags.to_vec();
        tags.sort_unstable();
        tags.dedup();
        if filename.strip_suffix(".whl").is_some() {
            if tags != wheel_filename_tags(filename)? {
                return Err(PackageArtifactViolation::Tag.into());
            }
        } else if !tags.is_empty() {
            return Err(PackageArtifactViolation::Tag.into());
        }
        Ok(Self {
            filename: ProjectPath(filename.to_owned()),
            content_digest,
            tags,
        })
    }

    /// Original selected archive filename.
    #[must_use]
    pub fn filename(&self) -> &ProjectPath {
        &self.filename
    }

    /// Complete digest of the selected archive bytes.
    #[must_use]
    pub fn content_digest(&self) -> &SourceDigest {
        &self.content_digest
    }

    /// Expanded compatibility tags in deterministic order.
    #[must_use]
    pub fn tags(&self) -> &[WheelTag] {
        &self.tags
    }
}

fn wheel_filename_tags(filename: &str) -> Result<Vec<WheelTag>, RiftError> {
    let stem = filename
        .strip_suffix(".whl")
        .ok_or(PackageArtifactViolation::Path)?;
    let dashes = stem.bytes().filter(|byte| *byte == b'-').count();
    if !matches!(dashes, 4 | 5) {
        return Err(PackageArtifactViolation::Path.into());
    }
    let mut parts = stem.splitn(dashes - 1, '-');
    let name = parts.next().ok_or(PackageArtifactViolation::Path)?;
    let version = parts.next().ok_or(PackageArtifactViolation::Path)?;
    if name.is_empty()
        || name.contains("__")
        || !name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
        || version.parse::<pep440_rs::Version>().is_err()
    {
        return Err(PackageArtifactViolation::Path.into());
    }
    if dashes == 5 {
        let build = parts.next().ok_or(PackageArtifactViolation::Path)?;
        if !build.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return Err(PackageArtifactViolation::Path.into());
        }
    }
    WheelTag::parse(parts.next().ok_or(PackageArtifactViolation::Tag)?)
}

#[cfg(test)]
mod tests;
