//! Registered documentation refusals.

use rift_error::{RiftError, errors};
use serde::Serialize;

/// The invariant a documentation input or publication violates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentationViolation {
    /// A selected source exceeds its count or byte bound.
    LimitExceeded,
    /// A source address is empty or not canonical.
    Identity,
    /// More than one source names the same content owner.
    DuplicateSource,
    /// Recorded digest differs from the supplied bytes.
    Digest,
    /// A source's package or location conflicts with its address.
    Origin,
    /// Format, extension, and media type do not agree.
    Format,
    /// A metadata range does not address its source bytes.
    Range,
    /// A collection is not in its required order.
    Order,
    /// A metadata record names a missing source, block, or declaration.
    MissingTarget,
    /// A notebook identity or selected source shape is invalid.
    Notebook,
    /// Publication revision differs from the supported revision.
    Revision,
    /// Canonical JSON encoding failed.
    Encoding,
}

pub(super) fn refused(violation: DocumentationViolation, field: &'static str) -> RiftError {
    build(violation, field, None)
}

pub(super) fn refused_by(
    violation: DocumentationViolation,
    field: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> RiftError {
    build(violation, field, Some(Box::new(source)))
}

pub(crate) fn context_value(error: &RiftError, key: &str) -> Option<String> {
    error
        .context()
        .find(|(field, _)| *field == key)
        .map(|(_, value)| value)
}

pub(crate) fn violation(error: &RiftError) -> DocumentationViolation {
    match error.slug().as_str() {
        "rift.analysis.documentation_limit_exceeded" => DocumentationViolation::LimitExceeded,
        "rift.analysis.documentation_identity_invalid" => DocumentationViolation::Identity,
        "rift.analysis.documentation_duplicate_source" => DocumentationViolation::DuplicateSource,
        "rift.analysis.documentation_digest_mismatch" => DocumentationViolation::Digest,
        "rift.analysis.documentation_origin_invalid" => DocumentationViolation::Origin,
        "rift.analysis.documentation_format_invalid" => DocumentationViolation::Format,
        "rift.analysis.documentation_range_invalid" => DocumentationViolation::Range,
        "rift.analysis.documentation_order_invalid" => DocumentationViolation::Order,
        "rift.analysis.documentation_target_missing" => DocumentationViolation::MissingTarget,
        "rift.analysis.documentation_notebook_invalid" => DocumentationViolation::Notebook,
        "rift.analysis.documentation_revision_invalid" => DocumentationViolation::Revision,
        "rift.analysis.documentation_encoding_failed" => DocumentationViolation::Encoding,
        slug => panic!("unknown documentation slug {slug}"),
    }
}

fn build(
    violation: DocumentationViolation,
    field: &'static str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> RiftError {
    match violation {
        DocumentationViolation::LimitExceeded => errors::analysis::documentation_limit_exceeded()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Identity => errors::analysis::documentation_identity_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::DuplicateSource => {
            errors::analysis::documentation_duplicate_source()
                .field(field)
                .maybe_source(source)
                .error()
        }
        DocumentationViolation::Digest => errors::analysis::documentation_digest_mismatch()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Origin => errors::analysis::documentation_origin_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Format => errors::analysis::documentation_format_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Range => errors::analysis::documentation_range_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Order => errors::analysis::documentation_order_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::MissingTarget => errors::analysis::documentation_target_missing()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Notebook => errors::analysis::documentation_notebook_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Revision => errors::analysis::documentation_revision_invalid()
            .field(field)
            .maybe_source(source)
            .error(),
        DocumentationViolation::Encoding => errors::analysis::documentation_encoding_failed()
            .field(field)
            .maybe_source(source)
            .error(),
    }
}
