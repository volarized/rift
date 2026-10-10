use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::SourceDigest;
use crate::read::ProjectPath;

/// Expanded compatibility tags retained for one selected artifact.
pub const PACKAGE_ARTIFACT_TAGS_MAX: usize = 64;
/// Aggregate UTF-8 bytes retained for one selected artifact.
pub const PACKAGE_ARTIFACT_BYTES_MAX: usize = 65_536;

/// A selected artifact field the analyzer cannot accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageArtifactViolation {
    /// An input or expanded tag set exceeds its bound.
    Length,
    /// A compatibility tag has invalid components.
    Tag,
    /// The artifact filename is not one root-relative basename.
    Path,
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
    type Error = String;

    fn try_from(value: WheelTagInput) -> Result<Self, Self::Error> {
        Self::new(&value.python, &value.abi, &value.platform)
            .map_err(|violation| format!("invalid wheel tag: {violation:?}"))
    }
}

impl WheelTag {
    /// Accepts an expanded tag and retains its lowercase spelling.
    ///
    /// # Errors
    /// Returns a violation for invalid components or an excessive byte count.
    pub fn new(python: &str, abi: &str, platform: &str) -> Result<Self, PackageArtifactViolation> {
        if [python, abi, platform]
            .iter()
            .any(|value| value.len() > PACKAGE_ARTIFACT_BYTES_MAX)
            || [python, abi, platform]
                .iter()
                .map(|value| lowercase_bytes(value))
                .sum::<usize>()
                > PACKAGE_ARTIFACT_BYTES_MAX
        {
            return Err(PackageArtifactViolation::Length);
        }
        if !python_identifier_is_valid(python)
            || !tag_component_is_valid(abi)
            || !tag_component_is_valid(platform)
        {
            return Err(PackageArtifactViolation::Tag);
        }
        Ok(Self {
            python: python.to_lowercase(),
            abi: abi.to_lowercase(),
            platform: platform.to_lowercase(),
        })
    }

    /// Expands a compressed compatibility tag under count and byte bounds.
    ///
    /// # Errors
    /// Returns a violation before expansion for malformed or excessive input.
    pub fn parse(value: &str) -> Result<Vec<Self>, PackageArtifactViolation> {
        if value.len() > PACKAGE_ARTIFACT_BYTES_MAX {
            return Err(PackageArtifactViolation::Length);
        }
        let mut fields = value.split('-');
        let python = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        let abi = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        let platform = fields.next().ok_or(PackageArtifactViolation::Tag)?;
        if fields.next().is_some()
            || !python.split('.').all(python_identifier_is_valid)
            || !abi.split('.').all(tag_component_is_valid)
            || !platform.split('.').all(tag_component_is_valid)
        {
            return Err(PackageArtifactViolation::Tag);
        }
        let counts = [
            python.split('.').count(),
            abi.split('.').count(),
            platform.split('.').count(),
        ];
        let count = counts.iter().try_fold(1_usize, |count, next| {
            count
                .checked_mul(*next)
                .ok_or(PackageArtifactViolation::Length)
        })?;
        if count > PACKAGE_ARTIFACT_TAGS_MAX {
            return Err(PackageArtifactViolation::Length);
        }
        let bytes = [python, abi, platform]
            .iter()
            .zip(counts)
            .map(|(field, field_count)| {
                field.split('.').map(lowercase_bytes).sum::<usize>() * (count / field_count)
            })
            .sum::<usize>();
        if bytes > PACKAGE_ARTIFACT_BYTES_MAX {
            return Err(PackageArtifactViolation::Length);
        }
        let mut tags = Vec::with_capacity(count);
        for python in python.split('.') {
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
    #[schemars(length(max = 64))]
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
    type Error = String;

    fn try_from(value: PackageArtifactInput) -> Result<Self, Self::Error> {
        Self::new(&value.filename.0, value.content_digest, &value.tags)
            .map_err(|violation| format!("invalid package artifact: {violation:?}"))
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
    ) -> Result<Self, PackageArtifactViolation> {
        if tags.len() > PACKAGE_ARTIFACT_TAGS_MAX
            || filename
                .len()
                .saturating_add(content_digest.as_str().len())
                .saturating_add(tags.iter().map(WheelTag::bytes).sum::<usize>())
                > PACKAGE_ARTIFACT_BYTES_MAX
        {
            return Err(PackageArtifactViolation::Length);
        }
        if !crate::identity::source_unit_path_is_valid(filename) || filename.contains('/') {
            return Err(PackageArtifactViolation::Path);
        }
        let mut tags = tags.to_vec();
        tags.sort_unstable();
        tags.dedup();
        if filename.strip_suffix(".whl").is_some() {
            if tags != wheel_filename_tags(filename)? {
                return Err(PackageArtifactViolation::Tag);
            }
        } else if !tags.is_empty() {
            return Err(PackageArtifactViolation::Tag);
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

fn wheel_filename_tags(filename: &str) -> Result<Vec<WheelTag>, PackageArtifactViolation> {
    let stem = filename
        .strip_suffix(".whl")
        .ok_or(PackageArtifactViolation::Path)?;
    let dashes = stem.bytes().filter(|byte| *byte == b'-').count();
    if !matches!(dashes, 4 | 5) {
        return Err(PackageArtifactViolation::Path);
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
        return Err(PackageArtifactViolation::Path);
    }
    if dashes == 5 {
        let build = parts.next().ok_or(PackageArtifactViolation::Path)?;
        if !build.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return Err(PackageArtifactViolation::Path);
        }
    }
    WheelTag::parse(parts.next().ok_or(PackageArtifactViolation::Tag)?)
}

#[cfg(test)]
mod tests;
