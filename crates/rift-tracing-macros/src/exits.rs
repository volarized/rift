//! Registered errors leaving an inline operation through `?` or `return`.

use proc_macro2::TokenStream;
use quote::ToTokens as _;
use syn::fold::{self, Fold as _};
use syn::{Expr, Item};

/// Rewrites exits belonging to this expression's enclosing function.
pub(crate) fn instrument(path: &TokenStream, work: TokenStream) -> syn::Result<TokenStream> {
    let input: syn::ExprTuple = syn::parse2(work)?;
    if input.elems.len() != 2 {
        return Err(syn::Error::new_spanned(
            input,
            "expected operation span and work",
        ));
    }
    let mut expressions = input.elems.into_iter();
    let span = expressions.next().expect("two expressions accepted");
    let work = expressions.next().expect("two expressions accepted");
    Ok(Exits {
        path,
        span: &span,
        questions: true,
    }
    .fold_expr(work)
    .into_token_stream())
}

/// Nested functions, closures, and async blocks own their exits; try blocks own `?`.
struct Exits<'path> {
    path: &'path TokenStream,
    span: &'path Expr,
    questions: bool,
}

impl fold::Fold for Exits<'_> {
    fn fold_item(&mut self, item: Item) -> Item {
        item
    }

    fn fold_expr(&mut self, expression: Expr) -> Expr {
        let path = self.path;
        let span = self.span;
        match expression {
            Expr::Closure(_) | Expr::Async(_) | Expr::Const(_) => expression,
            Expr::TryBlock(work) => {
                let mut nested = Exits {
                    path,
                    span,
                    questions: false,
                };
                Expr::TryBlock(fold::fold_expr_try_block(&mut nested, work))
            }
            Expr::Try(exit) if self.questions => {
                let value = self.fold_expr(*exit.expr);
                let attributes = &exit.attrs;
                syn::parse_quote! {
                    #(#attributes)* {
                        match #path::__private::InlineTry::branch(#value) {
                            ::core::ops::ControlFlow::Continue(__rift_value) => __rift_value,
                            ::core::ops::ControlFlow::Break(__rift_residual) => {
                                return #path::__private::exiting(
                                    &#span,
                                    #path::__rift_registered_error!(),
                                    #path::__private::InlineResidual::from_residual(__rift_residual),
                                );
                            }
                        }
                    }
                }
            }
            Expr::Return(mut exit) => {
                let Some(value) = exit.expr.take() else {
                    return Expr::Return(exit);
                };
                let value = self.fold_expr(*value);
                let attributes = &exit.attrs;
                syn::parse_quote! {
                    #(#attributes)* {
                        return #path::__private::exiting(
                            &#span,
                            #path::__rift_registered_error!(),
                            #value,
                        );
                    }
                }
            }
            expression => fold::fold_expr(self, expression),
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    #[test]
    fn a_try_block_keeps_questions_local_and_returns_in_the_enclosing_function() {
        let expanded = super::instrument(
            &quote!(::tracer),
            quote!((owned_span, {
                let caught: Result<(), Error> = try {
                    failed()?;
                };
                let returned: Result<(), Error> = try {
                    return failed();
                };
                caught?;
            })),
        )
        .expect("valid work")
        .to_string();
        assert_eq!(expanded.matches("InlineTry").count(), 1);
        assert_eq!(expanded.matches("exiting").count(), 2);
    }

    #[test]
    fn nested_functions_closures_and_async_blocks_own_their_exits() {
        let expanded = super::instrument(
            &quote!(::tracer),
            quote!((owned_span, {
                fn nested() -> Result<(), Error> {
                    failed()?;
                    return failed();
                }
                let closure = || {
                    failed()?;
                    return failed();
                };
                let future = async {
                    failed()?;
                    return failed();
                };
                failed()?;
            })),
        )
        .expect("valid work")
        .to_string();
        assert_eq!(expanded.matches("InlineTry").count(), 1);
        assert_eq!(expanded.matches("exiting").count(), 1);
    }
}
