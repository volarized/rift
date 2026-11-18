//! Registered syntax errors.

pub(crate) use rift_error::RiftError;
use rift_error::errors;
use tree_sitter::{Node, QueryError};

pub(crate) fn position_overflow(
    syntax_element: Node<'_>,
    source: std::num::TryFromIntError,
) -> RiftError {
    errors::syntax::position_overflow()
        .syntax_element_kind(syntax_element.kind())
        .start_byte(syntax_element.start_byte())
        .end_byte(syntax_element.end_byte())
        .source(source)
        .error()
}

pub(crate) fn invalid_query(query_source: &str, source: QueryError) -> RiftError {
    errors::syntax::invalid_query()
        .line_number(source.row + 1)
        .line_text(query_source.lines().nth(source.row).unwrap_or(""))
        .source(source)
        .error()
}
