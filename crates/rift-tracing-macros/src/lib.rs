//! The function attribute `rift-tracing` re-exports as `rift_tracing::timed`.
//!
//! A declarative macro cannot annotate a function, so timing a whole function needs an
//! attribute. The attribute rewrites the function's body into one `rift_tracing::traced!`
//! call and adds no behavior of its own: the span, the clock, and the completion metrics
//! are `traced!`'s. It finds the consuming crate's name for `rift-tracing`, renamed or
//! not, through `proc-macro-crate`, and its expansion names no other library.

mod exits;
mod expand;

use proc_macro::TokenStream;

/// Records registered errors at inline control-flow exits from `traced!`.
#[doc(hidden)]
#[proc_macro]
pub fn __rift_traced_work(item: TokenStream) -> TokenStream {
    let item = proc_macro2::TokenStream::from(item);
    expand::tracing_path(proc_macro_crate::crate_name(expand::TRACING_PACKAGE))
        .and_then(|path| exits::instrument(&path, item))
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// Times a whole function through `rift_tracing::traced!`; the documentation of
/// `rift_tracing::timed` states its arguments and behavior.
#[proc_macro_attribute]
pub fn timed(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item = proc_macro2::TokenStream::from(item);
    expand::tracing_path(proc_macro_crate::crate_name(expand::TRACING_PACKAGE))
        .and_then(|path| expand::timed(&path, attribute.into(), item.clone()))
        .unwrap_or_else(|error| expand::refused(&error, &item))
        .into()
}
