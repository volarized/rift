//! The attribute's arguments and its expansion, over `proc_macro2` tokens so unit tests can
//! run them outside a compiler expansion.

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens as _, quote};
use syn::parse::{Parse, ParseStream};
use syn::{AttrStyle, Expr, Ident, ItemFn, LitStr, ReturnType, Token};

/// The package the expansion calls into.
pub(crate) const TRACING_PACKAGE: &str = "rift-tracing";

/// The path the expansion names `rift-tracing` by in the consuming crate.
///
/// `rift-tracing` names itself `rift_tracing` through `extern crate self`, so the path
/// also resolves inside that crate, its unit tests, and its doctests.
pub(crate) fn tracing_path(
    found: Result<proc_macro_crate::FoundCrate, proc_macro_crate::Error>,
) -> syn::Result<TokenStream> {
    match found {
        Ok(proc_macro_crate::FoundCrate::Itself) => Ok(quote!(::rift_tracing)),
        Ok(proc_macro_crate::FoundCrate::Name(name)) => {
            let name = Ident::new(&name, Span::call_site());
            Ok(quote!(::#name))
        }
        Err(error) => Err(syn::Error::new(
            Span::call_site(),
            format!(
                "`timed` expands to `rift_tracing::traced!`, and this crate's manifest names no \
                 `{TRACING_PACKAGE}` dependency it can call: {error}"
            ),
        )),
    }
}

/// The function `item` with its body timed as the operation `attribute` names, through
/// `traced!` under `path`.
pub(crate) fn timed(
    path: &TokenStream,
    attribute: TokenStream,
    item: TokenStream,
) -> syn::Result<TokenStream> {
    let options: Options = syn::parse2(attribute)?;
    let function: ItemFn = syn::parse2(item).map_err(|error| {
        syn::Error::new(
            error.span(),
            "`timed` applies to a function with a body, such as `fn analyze() { .. }`",
        )
    })?;
    if let Some(constness) = &function.sig.constness {
        return Err(syn::Error::new_spanned(
            constness,
            "`timed` cannot time a `const fn`: the clock and the span run when the function \
             is called",
        ));
    }
    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = function;
    let (inner, outer): (Vec<_>, Vec<_>) = attrs
        .into_iter()
        .partition(|attribute| matches!(attribute.style, AttrStyle::Inner(_)));
    let arguments = options.arguments();
    let traced = if sig.asyncness.is_some() {
        let hint = return_hint(&sig.output);
        quote!(#path::traced!(#arguments async move { #hint #block }).await)
    } else {
        quote!(#path::traced!(#arguments #block))
    };
    Ok(quote! {
        #(#outer)*
        #vis #sig {
            #(#inner)*
            #traced
        }
    })
}

/// A `return` the async block never reaches that gives it the function's return type.
///
/// An async block infers its output from its own body, so a body that relies on a
/// coercion to the declared type, such as `&String` to `&str`, no longer compiles once it
/// moves into the block. The unreachable `return` states the type, as
/// `tracing-attributes` does for `#[instrument]`. A return type holding `impl Trait`
/// cannot name a local's type and gets no hint.
fn return_hint(output: &ReturnType) -> TokenStream {
    let ReturnType::Type(_, returned) = output else {
        return TokenStream::new();
    };
    let names_impl = returned
        .to_token_stream()
        .into_iter()
        .any(|token| matches!(&token, TokenTree::Ident(ident) if ident == "impl"));
    if names_impl {
        return TokenStream::new();
    }
    quote! {
        #[allow(
            unreachable_code,
            clippy::diverging_sub_expression,
            clippy::empty_loop,
            clippy::needless_return,
            clippy::unreachable
        )]
        if false {
            let __rift_return: #returned = loop {};
            return __rift_return;
        }
    }
}

/// The compile error `error` reports, followed by the unchanged `item`, so a refused
/// attribute reports its own error and no error of the code that calls the function.
pub(crate) fn refused(error: &syn::Error, item: &TokenStream) -> TokenStream {
    let error = error.to_compile_error();
    quote!(#error #item)
}

/// The attribute's arguments: the operation literal, then `component = <expr>` and its
/// fields, as `traced!`'s detailed form takes them.
struct Options {
    operation: LitStr,
    component: Option<Expr>,
    fields: Vec<(Ident, Expr)>,
}

impl Options {
    /// The arguments of the `traced!` call, up to and including the comma before the work.
    fn arguments(&self) -> TokenStream {
        let operation = &self.operation;
        let Some(component) = &self.component else {
            return quote!(#operation,);
        };
        let fields = self
            .fields
            .iter()
            .map(|(name, value)| quote!(#name = #value,));
        quote!(component = #component, operation = #operation, #(#fields)*)
    }
}

impl Parse for Options {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let operation: LitStr = input.parse().map_err(|error| {
            syn::Error::new(
                error.span(),
                "`timed` takes the operation name first, as a string literal: \
                 `#[timed(\"index.parse\")]`",
            )
        })?;
        let mut component = None;
        let mut fields: Vec<(Ident, Expr)> = Vec::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: Expr = input.parse()?;
            if name == "component" {
                if component.is_some() || !fields.is_empty() {
                    return Err(syn::Error::new_spanned(
                        name,
                        "`component` comes once, right after the operation name",
                    ));
                }
                component = Some(value);
            } else if name == "operation" {
                return Err(syn::Error::new_spanned(
                    name,
                    "the operation name is the first argument, a string literal",
                ));
            } else if component.is_none() {
                return Err(syn::Error::new_spanned(
                    name,
                    "a field follows `component`: \
                     `#[timed(\"index.parse\", component = \"index\", units = units)]`",
                ));
            } else {
                fields.push((name, value));
            }
        }
        Ok(Self {
            operation,
            component,
            fields,
        })
    }
}

#[cfg(test)]
mod tests;
