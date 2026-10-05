use proc_macro2::TokenStream;
use quote::quote;

use super::{refused, timed, tracing_path};

fn path() -> TokenStream {
    quote!(::rift_tracing)
}

fn expanded(attribute: TokenStream, item: TokenStream) -> String {
    timed(&path(), attribute, item)
        .expect("the attribute expands")
        .to_string()
}

fn refusal(attribute: TokenStream, item: TokenStream) -> String {
    timed(&path(), attribute, item)
        .expect_err("the attribute refuses")
        .to_string()
}

#[test]
fn a_sync_function_body_becomes_the_block_of_the_short_form() {
    let expansion = expanded(
        quote!("package.analyze"),
        quote! {
            /// Analyzes one package.
            pub(crate) fn analyze(package: &Package) -> Result<Facts, AnalysisError> {
                analyze_package(package)
            }
        },
    );
    let expected = quote! {
        #[doc = r" Analyzes one package."]
        pub(crate) fn analyze(package: &Package) -> Result<Facts, AnalysisError> {
            ::rift_tracing::traced!("package.analyze", { analyze_package(package) })
        }
    };
    assert_eq!(expansion, expected.to_string());
}

#[test]
fn an_async_function_awaits_the_async_form_with_component_and_fields() {
    let expansion = expanded(
        quote!(
            "lexical.commit",
            component = "lexical",
            units = change.len()
        ),
        quote! {
            async fn commit(change: LexicalChange) -> Result<(), StorageError> {
                commit_change(change).await
            }
        },
    );
    let expected = quote! {
        async fn commit(change: LexicalChange) -> Result<(), StorageError> {
            ::rift_tracing::traced!(
                component = "lexical",
                operation = "lexical.commit",
                units = change.len(),
                async move {
                    #[allow(
                        unreachable_code,
                        clippy::diverging_sub_expression,
                        clippy::empty_loop,
                        clippy::needless_return,
                        clippy::unreachable
                    )]
                    if false {
                        let __rift_return: Result<(), StorageError> = loop {};
                        return __rift_return;
                    }
                    { commit_change(change).await }
                }
            )
            .await
        }
    };
    assert_eq!(expansion, expected.to_string());
}

#[test]
fn an_async_function_without_a_nameable_return_type_gets_no_hint() {
    for item in [
        quote!(
            async fn run() {}
        ),
        quote!(
            async fn run() -> impl Sized {}
        ),
    ] {
        let expansion = expanded(quote!("index.run"), item);
        assert!(!expansion.contains("__rift_return"), "{expansion}");
    }
}

#[test]
fn inner_attributes_stay_inside_the_body_and_a_trailing_comma_is_accepted() {
    let expansion = expanded(
        quote!("index.parse",),
        quote! {
            fn parse() {
                #![allow(unused_variables)]
                let unused = 1;
            }
        },
    );
    let expected = quote! {
        fn parse() {
            #![allow(unused_variables)]
            ::rift_tracing::traced!("index.parse", { let unused = 1; })
        }
    };
    assert_eq!(expansion, expected.to_string());
}

/// Every identifier in `tokens`, groups included.
fn identifiers(tokens: TokenStream) -> Vec<String> {
    tokens
        .into_iter()
        .flat_map(|token| match token {
            proc_macro2::TokenTree::Ident(ident) => vec![ident.to_string()],
            proc_macro2::TokenTree::Group(group) => identifiers(group.stream()),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn the_expansion_names_no_backend_library_and_no_hidden_item() {
    let expansion = timed(
        &path(),
        quote!("index.parse", component = "index"),
        quote!(
            async fn parse() -> u8 {
                1
            }
        ),
    )
    .expect("the attribute expands");
    let names = identifiers(expansion);
    assert!(names.iter().any(|name| name == "rift_tracing"), "{names:?}");
    for name in &names {
        assert!(
            !matches!(
                name.as_str(),
                "tracing" | "tracing_subscriber" | "opentelemetry"
            ),
            "{name}"
        );
        assert!(
            !name.starts_with("__") || name == "__rift_return",
            "the expansion names only the public facade: {name}"
        );
    }
}

#[test]
fn a_renamed_dependency_is_named_by_its_local_name_and_the_crate_itself_by_its_alias() {
    use proc_macro_crate::FoundCrate;

    let renamed = tracing_path(Ok(FoundCrate::Name("tracer".to_owned())))
        .expect("a found dependency has a path");
    assert_eq!(renamed.to_string(), quote!(::tracer).to_string());
    let itself = tracing_path(Ok(FoundCrate::Itself)).expect("the crate itself has a path");
    assert_eq!(itself.to_string(), quote!(::rift_tracing).to_string());
}

#[test]
fn a_missing_dependency_is_refused_with_the_package_it_looked_for() {
    let error = tracing_path(Err(proc_macro_crate::Error::CargoManifestDirNotSet))
        .expect_err("a missing dependency is refused");
    let message = error.to_string();
    assert!(message.contains("`rift-tracing`"), "{message}");
    assert!(message.contains("rift_tracing::traced!"), "{message}");
}

#[test]
fn each_malformed_attribute_is_refused_with_its_reason() {
    let item = quote!(
        fn parse() {}
    );
    let cases = [
        (quote!(), "takes the operation name first"),
        (quote!(operation), "takes the operation name first"),
        (
            quote!("index.parse", units = 1),
            "a field follows `component`",
        ),
        (
            quote!("index.parse", component = "index", component = "search"),
            "`component` comes once",
        ),
        (
            quote!(
                "index.parse",
                component = "index",
                units = 1,
                component = "x"
            ),
            "`component` comes once",
        ),
        (
            quote!("index.parse", operation = "index.parse"),
            "the operation name is the first argument",
        ),
        (quote!("index.parse"; component), "expected `,`"),
    ];
    for (attribute, reason) in cases {
        let message = refusal(attribute.clone(), item.clone());
        assert!(message.contains(reason), "{attribute}: {message}");
    }
}

#[test]
fn a_const_fn_and_an_item_without_a_body_are_refused() {
    let constant = refusal(
        quote!("index.parse"),
        quote!(
            const fn parse() -> u8 {
                1
            }
        ),
    );
    assert!(constant.contains("cannot time a `const fn`"), "{constant}");
    for item in [
        quote!(
            struct Parse;
        ),
        quote!(
            fn parse();
        ),
    ] {
        let message = refusal(quote!("index.parse"), item);
        assert!(
            message.contains("applies to a function with a body"),
            "{message}"
        );
    }
}

#[test]
fn a_refused_attribute_keeps_the_item_after_its_error() {
    let item = quote!(
        fn parse() {}
    );
    let error = syn::Error::new(proc_macro2::Span::call_site(), "refused");
    let output = refused(&error, &item).to_string();
    assert!(output.starts_with(":: core :: compile_error !"), "{output}");
    assert!(output.ends_with(&item.to_string()), "{output}");
}
