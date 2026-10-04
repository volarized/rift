//! Identifier joining for registered Rift error declarations.
//!
//! `__rift_error_definition!` in `rift-error` derives builder names from field names:
//! `.maybe_<field>()` for an optional field and the `<Field>State` type parameter for a
//! required one. `macro_rules!` cannot form a new identifier, so the declaration macros
//! hand their expansion to [`paste!`]. The crate depends on `proc_macro` alone, so a
//! crate that reaches `rift-error` by path builds it with no registry access.

mod expand;
mod name;

use proc_macro::TokenStream;

/// Replaces every `[< .. >]` group in its input with one joined identifier.
///
/// A group holds one or more segments, each an identifier or `identifier:camel`. The
/// segments join in order; `:camel` converts its segment from snake case to upper camel
/// case, and a raw identifier joins without its `r#` prefix. The identifier takes the
/// bracket group's span, so it resolves with the hygiene of the macro that wrote the
/// group. Every other token is emitted unchanged, once, in its original order; the
/// macro evaluates nothing and adds no control flow.
///
/// ```
/// rift_error_macros::paste! {
///     struct [<request_id:camel State>];
///     fn [<maybe_ request_id>]() -> [<request_id:camel State>] {
///         [<request_id:camel State>]
///     }
/// }
/// let _: RequestIdState = maybe_request_id();
/// ```
///
/// A group that names no identifier fails compilation at that group:
///
/// ```compile_fail
/// rift_error_macros::paste! { fn [<maybe_ "field">]() {} }
/// ```
///
/// ```compile_fail
/// rift_error_macros::paste! { struct [<field:snake State>]; }
/// ```
///
/// ```compile_fail
/// rift_error_macros::paste! { struct [<>]; }
/// ```
///
/// ```compile_fail
/// rift_error_macros::paste! { struct [<_:camel>]; }
/// ```
#[proc_macro]
pub fn paste(input: TokenStream) -> TokenStream {
    expand::expand(input).unwrap_or_else(expand::Failure::into_compile_error)
}
