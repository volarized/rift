use std::collections::BTreeMap;

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use syn::{File, ItemUse};

use crate::{ir, schema, validate::CodegenError};

#[derive(Default)]
struct Node<'a> {
    error: Option<&'a ir::Error>,
    children: BTreeMap<String, Node<'a>>,
}

pub(crate) fn generate(registry: &ir::Registry) -> Result<String, CodegenError> {
    let namespace = &registry.namespace;
    let mut root = Node::default();
    for error in &registry.errors {
        let mut node = &mut root;
        for part in &error.path {
            node = node.children.entry(part.clone()).or_default();
        }
        node.error = Some(error);
    }
    let modules = render_children(&root);
    let slugs = registry.errors.iter().map(|error| &error.slug);
    let fields = registry
        .errors
        .iter()
        .flat_map(|error| error.fields.iter())
        .collect::<Vec<_>>();
    let has_fields = !fields.is_empty();
    let has_required = fields.iter().any(|field| !field.optional);
    let has_display = fields.iter().any(|field| {
        matches!(
            field.field_type,
            schema::FieldType::String | schema::FieldType::Error
        ) && !matches!(
            field.role,
            Some(schema::FieldRole::Source | schema::FieldRole::Cause)
        )
    });
    let has_unsigned = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Unsigned && field.role.is_none());
    let has_integer = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Integer && field.role.is_none());
    let has_bool = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Bool && field.role.is_none());
    let has_path = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Path);
    let has_pid = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Pid);
    let has_port = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Port);
    let has_duration = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Duration);
    let has_borrow = has_bool || has_duration || has_pid || has_port;
    let has_error = fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::Error);
    let has_source = fields
        .iter()
        .any(|field| matches!(field.role, Some(schema::FieldRole::Source)));
    let mut import_paths = vec![
        ("rift_error", "BuilderCore", None),
        ("rift_error", "ErrorContext", None),
        ("rift_error", "ErrorSlug", None),
        ("rift_error", "EvidenceFor", None),
        ("rift_error", "IntoRiftError", None),
        ("rift_error", "RiftError", None),
        ("std", "marker::PhantomData", None),
    ];
    if has_fields {
        import_paths.push(("rift_error", "ErrorValue", None));
    }
    if has_required {
        import_paths.push(("rift_error", "Set", Some("SetState")));
        import_paths.push(("rift_error", "Unset", None));
    }
    if has_display {
        import_paths.push(("std", "fmt::Display", None));
    }
    if has_unsigned {
        import_paths.push(("rift_error", "IntoUnsigned", None));
    }
    if has_integer {
        import_paths.push(("rift_error", "IntoInteger", None));
    }
    if has_path {
        import_paths.push(("std", "path::Path", None));
    }
    if has_borrow {
        import_paths.push(("std", "borrow::Borrow", None));
    }
    if has_duration {
        import_paths.push(("std", "time::Duration", None));
    }
    if has_error {
        import_paths.push(("std", "error::Error", None));
    }
    if has_source {
        import_paths.push(("std", "boxed::Box", None));
    }
    let imports = imports(&import_paths)?;
    let tokens = quote! {
        pub use rift_error::{FieldSet, OptionalFieldSet};
        #(#imports)*
        #[doc(hidden)]
        pub const REGISTRY_NAMESPACE: &str = #namespace;
        #[doc(hidden)]
        pub const REGISTERED_SLUGS: &[&str] = &[#(#slugs),*];
        #modules
    };
    let file: File = syn::parse2(tokens).map_err(|error| CodegenError::Rust(error.to_string()))?;
    Ok(prettyplease::unparse(&file))
}

#[derive(Default)]
struct ImportNode {
    alias: Option<Ident>,
    children: BTreeMap<String, ImportNode>,
}

fn imports(paths: &[(&str, &str, Option<&str>)]) -> Result<Vec<ItemUse>, CodegenError> {
    let mut roots = BTreeMap::<String, ImportNode>::new();
    for (namespace, path, alias) in paths {
        let mut node = roots.entry((*namespace).to_owned()).or_default();
        let parts = path.split("::").collect::<Vec<_>>();
        for part in &parts {
            node = node.children.entry((*part).to_owned()).or_default();
        }
        node.alias = alias.map(|alias| format_ident!("{alias}"));
    }
    roots
        .into_iter()
        .map(|(namespace, node)| {
            let namespace = ident(&namespace);
            let tree = import_tree(&node);
            syn::parse2(quote! { use #namespace::#tree; })
                .map_err(|error| CodegenError::Rust(error.to_string()))
        })
        .collect()
}

fn import_tree(node: &ImportNode) -> TokenStream {
    let children = node.children.iter().map(|(name, child)| {
        let name = ident(name);
        let tree = if child.children.is_empty() {
            if let Some(alias) = &child.alias {
                quote! { #name as #alias }
            } else {
                quote! { #name }
            }
        } else {
            import_tree(child)
        };
        if child.children.is_empty() {
            tree
        } else {
            quote! { #name::#tree }
        }
    });
    quote! { { #(#children),* } }
}

fn render_children(node: &Node<'_>) -> TokenStream {
    let modules = node.children.iter().map(|(name, child)| {
        let name = ident(name);
        if let Some(error) = child.error {
            render_error(error)
        } else {
            let contents = render_children(child);
            quote! {
                #[allow(missing_docs)]
                pub mod #name {
                    use super::*;
                    pub use super::{FieldSet, OptionalFieldSet};
                    #contents
                }
            }
        }
    });
    quote! { #(#modules)* }
}

fn render_error(error: &ir::Error) -> TokenStream {
    let function = ident(error.path.last().expect("validated error path"));
    let slug = &error.slug;
    let message = &error.message;
    let action = &error.action;
    let fields = &error.fields;
    let required = fields
        .iter()
        .filter(|field| !field.optional)
        .collect::<Vec<_>>();
    let state_names = (0..required.len())
        .map(|index| format_ident!("State{index}"))
        .collect::<Vec<_>>();
    let initial_states = vec![quote! { Unset }; required.len()];
    let complete_states = vec![quote! { SetState }; required.len()];
    let all_state_args = generic_args(&state_names);
    let builder_generics = generic_decl(&state_names);
    let defaults = generic_defaults(&state_names);
    let phantom = phantom_type(&state_names);
    let initial_builder = builder_type(&initial_states);
    let complete_builder = builder_type(&complete_states);

    let field_count = fields.len();
    let ambient_methods = quote! {
        pub fn with(mut self, context: ErrorContext) -> Self {
            self.core.with(context);
            self
        }
        pub fn evidence<E, O>(self, evidence: E) -> O
        where E: EvidenceFor<Self, O, EvidenceTag>
        {
            evidence.apply_evidence(self)
        }
    };
    let field_modules = fields
        .iter()
        .enumerate()
        .map(|(index, field)| render_field(error, field, index, &required, &state_names));
    let setters = fields.iter().map(|field| {
        let name = ident(&field.name);
        let field_module = ident(&field.name);
        let bound = setter_bound(field);
        let optional_setter = if field.optional {
            let maybe_name = format_ident!("maybe_{}", field.name);
            quote! {
                pub fn #maybe_name<T>(
                    self,
                    value: Option<T>,
                ) -> <#field_module::Field as #field_module::SetOptional<Self>>::Output
                where T: #bound
                {
                    <#field_module::Field as #field_module::SetOptional<Self>>::set_optional(
                        self,
                        #field_module::optional_value(value),
                    )
                }
            }
        } else {
            quote! {}
        };
        quote! {
            pub fn #name<T>(
                self,
                value: T,
            ) -> <#field_module::Field as #field_module::Set<Self>>::Output
            where T: #bound
            {
                <#field_module::Field as #field_module::Set<Self>>::set(
                    self,
                    #field_module::value(value),
                )
            }
            #optional_setter
        }
    });

    let builder_def = if state_names.is_empty() {
        quote! {
            pub struct Builder {
                pub(super) core: BuilderCore,
                pub(super) marker: PhantomData<()>,
            }
        }
    } else {
        quote! {
            pub struct Builder #defaults {
                pub(super) core: BuilderCore,
                pub(super) marker: PhantomData<#phantom>,
            }
        }
    };
    let alias_input = quote! { pub type EvidenceInput = #initial_builder; };
    let alias_output = quote! { pub type EvidenceOutput = #complete_builder; };
    let function_builder = if state_names.is_empty() {
        quote! { #function::Builder }
    } else {
        quote! { #function::Builder<#(#initial_states),*> }
    };
    let builder_impl = if state_names.is_empty() {
        quote! { impl Builder {
            #(#setters)*
            #ambient_methods
        } }
    } else {
        quote! { impl #builder_generics Builder<#(#all_state_args),*> {
            #(#setters)*
            #ambient_methods
        } }
    };
    let finish = quote! {
        fn finish(self) -> RiftError {
            self.core.finish()
        }
    };
    let terminal = if state_names.is_empty() {
        quote! {
            impl Builder {
                pub fn error(self) -> RiftError { self.finish() }
                pub fn fail<T>(self) -> Result<T, RiftError> { Err(self.finish()) }
                #finish
            }
            impl IntoRiftError for Builder {
                fn into_rift_error(self) -> RiftError { self.finish() }
            }
        }
    } else {
        quote! {
            impl Builder<#(#complete_states),*> {
                pub fn error(self) -> RiftError { self.finish() }
                pub fn fail<T>(self) -> Result<T, RiftError> { Err(self.finish()) }
                #finish
            }
            impl IntoRiftError for Builder<#(#complete_states),*> {
                fn into_rift_error(self) -> RiftError { self.finish() }
            }
        }
    };
    let function_body = quote! {
        pub fn #function() -> #function_builder {
            #function::Builder {
                core: BuilderCore::new(ErrorSlug::new(#slug), #message, #action, #field_count),
                marker: PhantomData,
            }
        }
    };
    let _ = &function_builder;
    quote! {
        #[allow(non_camel_case_types, missing_docs, unused_parens)]
        pub mod #function {
            use super::*;
            pub use super::{FieldSet, OptionalFieldSet};
            /// Stable registry identity for this error.
            pub const SLUG: ErrorSlug = ErrorSlug::new(#slug);
            pub struct EvidenceTag;
            #builder_def
            #alias_input
            #alias_output
            #(#field_modules)*
            #builder_impl
            #terminal
        }
        #function_body
    }
}

fn render_field(
    error: &ir::Error,
    field: &ir::Field,
    index: usize,
    required: &[&ir::Field],
    state_names: &[Ident],
) -> TokenStream {
    let name = ident(&field.name);
    let index = index as u32;
    let display = field.display;
    let sensitive = field.sensitive;
    let key = &field.name;
    let bound = setter_bound(field);
    let value = conversion(field);
    let optional_value = quote! { value.map(self::value) };
    let target_states = generic_args(state_names);
    let target = builder_type(&target_states);
    let input_generics = generic_decl(state_names);
    let output_states = required
        .iter()
        .enumerate()
        .map(|(position, required_field)| {
            if required_field.name == field.name {
                quote! { SetState }
            } else {
                let state = &state_names[position];
                quote! { #state }
            }
        })
        .collect::<Vec<_>>();
    let output = builder_type(&output_states);
    let output_value = builder_value_type(&output_states);
    let implementation = if state_names.is_empty() {
        quote! { impl Set<Builder> for Field {
            type Output = Builder;
            fn set(mut target: Builder, value: ErrorValue) -> Self::Output {
                target.core.set(#index as usize, #key, value, #display, #sensitive);
                target
            }
        } }
    } else {
        quote! { impl #input_generics Set<#target> for Field {
            type Output = #output;
            fn set(mut target: #target, value: ErrorValue) -> Self::Output {
                target.core.set(#index as usize, #key, value, #display, #sensitive);
                #output_value { core: target.core, marker: PhantomData }
            }
        } }
    };
    let optional_impl = if field.optional {
        if state_names.is_empty() {
            quote! { impl SetOptional<Builder> for Field {
                type Output = Builder;
                fn set_optional(mut target: Builder, value: Option<ErrorValue>) -> Self::Output {
                    target.core.set_optional(#index as usize, #key, value, #display, #sensitive);
                    target
                }
            } }
        } else {
            quote! { impl #input_generics SetOptional<#target> for Field {
                type Output = #target;
                fn set_optional(mut target: #target, value: Option<ErrorValue>) -> Self::Output {
                    target.core.set_optional(#index as usize, #key, value, #display, #sensitive);
                    target
                }
            } }
        }
    } else {
        quote! {}
    };
    let _ = error;
    quote! {
        pub mod #name {
            use super::*;
            pub use super::FieldSet as Set;
            pub use super::OptionalFieldSet as SetOptional;
            pub struct Field;
            #implementation
            #optional_impl
            pub fn value<T>(value: T) -> ErrorValue where T: #bound { #value }
            pub fn optional_value<T>(value: Option<T>) -> Option<ErrorValue> where T: #bound { #optional_value }
        }
    }
}

fn builder_type(states: &[TokenStream]) -> TokenStream {
    if states.is_empty() {
        quote! { Builder }
    } else {
        quote! { Builder<#(#states),*> }
    }
}

fn builder_value_type(states: &[TokenStream]) -> TokenStream {
    if states.is_empty() {
        quote! { Builder }
    } else {
        quote! { Builder::<#(#states),*> }
    }
}

fn generic_args(states: &[Ident]) -> Vec<TokenStream> {
    states.iter().map(|state| quote! { #state }).collect()
}

fn generic_decl(states: &[Ident]) -> TokenStream {
    if states.is_empty() {
        quote! {}
    } else {
        quote! { <#(#states),*> }
    }
}

fn generic_defaults(states: &[Ident]) -> TokenStream {
    if states.is_empty() {
        quote! {}
    } else {
        let defaults = states.iter().map(|_| quote! { Unset }).collect::<Vec<_>>();
        quote! { <#(#states = #defaults),*> }
    }
}

fn phantom_type(states: &[Ident]) -> TokenStream {
    match states {
        [] => quote! { () },
        [state] => quote! { #state },
        _ => quote! { (#(#states),*) },
    }
}

fn setter_bound(field: &ir::Field) -> TokenStream {
    match field.field_type {
        schema::FieldType::String => quote! { Display },
        schema::FieldType::Bool => quote! { Borrow<bool> },
        schema::FieldType::Integer => quote! { IntoInteger },
        schema::FieldType::Unsigned => quote! { IntoUnsigned },
        schema::FieldType::Pid => quote! { Borrow<u32> },
        schema::FieldType::Port => quote! { Borrow<u16> },
        schema::FieldType::Path => quote! { AsRef<Path> },
        schema::FieldType::Duration => quote! { Borrow<Duration> },
        schema::FieldType::Error if matches!(field.role, Some(schema::FieldRole::Source)) => {
            quote! { Into<Box<dyn Error + Send + Sync + 'static>> }
        }
        schema::FieldType::Error => quote! { Error + 'static },
        schema::FieldType::RiftError => quote! { IntoRiftError },
    }
}

fn conversion(field: &ir::Field) -> TokenStream {
    match (field.role, field.field_type, field.format) {
        (Some(schema::FieldRole::Source), _, _) => quote! { ErrorValue::source(value) },
        (Some(schema::FieldRole::Cause), _, _) => quote! { ErrorValue::cause(value) },
        (None, schema::FieldType::Path, Some(schema::FieldFormat::Display)) => {
            quote! { ErrorValue::path(value) }
        }
        (None, schema::FieldType::Duration, Some(schema::FieldFormat::Human)) => {
            quote! { ErrorValue::duration(value) }
        }
        (None, schema::FieldType::Integer, _) => quote! { ErrorValue::integer(value) },
        (None, schema::FieldType::Unsigned, _) => quote! { ErrorValue::unsigned(value) },
        (None, schema::FieldType::Pid, _) => quote! { ErrorValue::pid(value) },
        (None, schema::FieldType::Port, _) => quote! { ErrorValue::port(value) },
        (None, schema::FieldType::Bool, _) => {
            quote! { ErrorValue::bool_value(*value.borrow()) }
        }
        _ => quote! { ErrorValue::display(value) },
    }
}

fn ident(name: &str) -> Ident {
    Ident::new(name, proc_macro2::Span::call_site())
}
