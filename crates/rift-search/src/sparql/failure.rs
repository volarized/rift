//! Query failures use the workspace error registry.

use rift_error::{RiftError, errors};

pub(super) fn parse(error: impl std::fmt::Display) -> RiftError {
    errors::server::read_invalid()
        .field("query")
        .violation(error.to_string())
        .error()
}

pub(super) fn invalid(error: impl std::fmt::Display) -> RiftError {
    errors::server::read_unavailable()
        .operation("query fact projection")
        .detail(error.to_string())
        .error()
}

pub(super) fn bound(quantity: &'static str) -> RiftError {
    errors::server::read_unavailable()
        .operation("query admission")
        .detail(format!(
            "query exceeds {quantity}; narrow the query or increase the accepted bound"
        ))
        .error()
}

pub(super) fn evaluation(error: impl std::fmt::Display) -> RiftError {
    errors::server::read_unavailable()
        .operation("SPARQL evaluation")
        .detail(error.to_string())
        .error()
}

pub(super) fn cancelled() -> RiftError {
    errors::server::read_cancelled().error()
}
