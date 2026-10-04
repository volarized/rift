use std::collections::{BTreeMap, BTreeSet};

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
    let mut import_paths = vec![("rift_error", "__rift_error_definition", None)];
    if has_fields {
        import_paths.push(("rift_error", "ErrorValue", None));
    }
    if has_required {
        import_paths.push(("rift_error", "Set", Some("SetState")));
    }
    if fields
        .iter()
        .any(|field| field.field_type == schema::FieldType::RiftError)
    {
        import_paths.push(("rift_error", "IntoRiftError", None));
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
            let imports = super_imports(&scope_import_names(child));
            quote! {
                #[allow(missing_docs)]
                pub mod #name {
                    #imports
                    pub use super::{FieldSet, OptionalFieldSet};
                    #contents
                }
            }
        }
    });
    quote! { #(#modules)* }
}

fn scope_import_names(node: &Node<'_>) -> BTreeSet<&'static str> {
    let mut names = node.error.map(error_import_names).unwrap_or_default();
    for child in node.children.values() {
        names.extend(scope_import_names(child));
    }
    names
}

fn error_import_names(error: &ir::Error) -> BTreeSet<&'static str> {
    let mut names = BTreeSet::from(["__rift_error_definition"]);
    if !error.fields.is_empty() {
        names.insert("ErrorValue");
    }
    if error.fields.iter().any(|field| !field.optional) {
        names.insert("SetState");
    }
    for field in &error.fields {
        add_bound_imports(&mut names, field);
    }
    names
}

fn add_bound_imports(names: &mut BTreeSet<&'static str>, field: &ir::Field) {
    match field.field_type {
        schema::FieldType::String => {
            names.insert("Display");
        }
        schema::FieldType::Bool | schema::FieldType::Pid | schema::FieldType::Port => {
            names.insert("Borrow");
        }
        schema::FieldType::Integer => {
            names.insert("IntoInteger");
        }
        schema::FieldType::Unsigned => {
            names.insert("IntoUnsigned");
        }
        schema::FieldType::Path => {
            names.insert("Path");
        }
        schema::FieldType::Duration => {
            names.extend(["Borrow", "Duration"]);
        }
        schema::FieldType::Error => {
            names.insert("Error");
            if matches!(field.role, Some(schema::FieldRole::Source)) {
                names.insert("Box");
            }
        }
        schema::FieldType::RiftError => {
            names.insert("IntoRiftError");
        }
    }
}

fn super_imports(names: &BTreeSet<&'static str>) -> TokenStream {
    let names = names.iter().map(|name| ident(name));
    quote! { use super::{#(#names),*}; }
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
    let complete_states = vec![quote! { SetState }; required.len()];
    let field_count = fields.len();
    let mut names = error_import_names(error);
    names.remove("__rift_error_definition");
    let error_imports = if names.is_empty() {
        quote! {}
    } else {
        super_imports(&names)
    };
    let field_descriptors = fields
        .iter()
        .enumerate()
        .map(|(index, field)| render_field(field, index, &required, &state_names));
    quote! {
        __rift_error_definition! {
            error #function;
            imports { #error_imports }
            metadata [#slug, #message, #action, #field_count];
            builder Builder;
            states [#(#state_names),*];
            complete [#(#complete_states),*];
            fields { #(#field_descriptors)* }
        }
    }
}

fn render_field(
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
    let conversion = conversion(field);
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
    let optional = if field.optional {
        let maybe_name = format_ident!("maybe_{}", field.name);
        quote! { #maybe_name }
    } else {
        quote! {}
    };
    let mut imports = BTreeSet::from(["Builder", "ErrorValue"]);
    if !field.optional {
        imports.insert("SetState");
    }
    add_bound_imports(&mut imports, field);
    let imports = super_imports(&imports);
    let value = ident("value");
    quote! {
        #name {
            imports { #imports }
            output [#(#output_states),*];
            index #index;
            key #key;
            flags [#display, #sensitive];
            bound [#bound];
            value #value => [#conversion];
            optional [#optional];
        }
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
