//! Registry rendering as compact `__rift_error_definition!` declarations.
//!
//! The generator writes only what each error is; `rift-error`'s definition macros
//! expand how it is implemented. Output is written as text in its final layout:
//! `prettyplease` prints macro arguments as one wrapped token stream, and `rustfmt`
//! leaves this layout unchanged.

use std::{collections::BTreeMap, fmt::Write as _};

use crate::{ir, validate::CodegenError};

const INDENT: &str = "    ";

#[derive(Default)]
struct Node<'a> {
    error: Option<&'a ir::Error>,
    children: BTreeMap<&'a str, Node<'a>>,
}

/// One registry rendered as a parent module and one file per top-level namespace.
pub(crate) struct Module {
    /// Constants plus one `pub mod <namespace>;` line per namespace.
    pub parent: String,
    /// File content keyed by namespace, the first path segment after the registry namespace.
    pub namespaces: BTreeMap<String, String>,
}

fn tree(registry: &ir::Registry) -> Node<'_> {
    let mut root = Node::default();
    for error in &registry.errors {
        let mut node = &mut root;
        for part in &error.path {
            node = node.children.entry(part.as_str()).or_default();
        }
        node.error = Some(error);
    }
    root
}

fn render_constants(output: &mut String, registry: &ir::Registry) {
    output.push_str("#[doc(hidden)]\n");
    let _ = writeln!(
        output,
        "pub const REGISTRY_NAMESPACE: &str = {};",
        literal(&registry.namespace)
    );
    output.push_str("#[doc(hidden)]\npub const REGISTERED_SLUGS: &[&str] = &[\n");
    for error in &registry.errors {
        let _ = writeln!(output, "{INDENT}{},", literal(&error.slug));
    }
    output.push_str("];\n");
}

fn parsed(output: String) -> Result<String, CodegenError> {
    syn::parse_file(&output).map_err(|error| CodegenError::Rust(error.to_string()))?;
    Ok(output)
}

pub(crate) fn generate(registry: &ir::Registry) -> Result<String, CodegenError> {
    let root = tree(registry);
    let mut output = String::new();
    output.push_str("use rift_error::__rift_error_definition;\n\n");
    render_constants(&mut output, registry);
    render_children(&mut output, &root, &registry.namespace, 0);
    parsed(output)
}

pub(crate) fn generate_module(registry: &ir::Registry) -> Result<Module, CodegenError> {
    let root = tree(registry);
    let mut parent = String::new();
    if root.children.values().any(|child| child.error.is_some()) {
        parent.push_str("use rift_error::__rift_error_definition;\n\n");
    }
    render_constants(&mut parent, registry);
    let mut namespaces = BTreeMap::new();
    for (name, child) in &root.children {
        parent.push('\n');
        if let Some(error) = child.error {
            render_error(&mut parent, error, "");
            continue;
        }
        let namespace = format!("{}.{name}", registry.namespace);
        let _ = writeln!(parent, "/// Registered errors under `{namespace}`.");
        let _ = writeln!(parent, "pub mod {name};");
        let mut file = String::from("use rift_error::__rift_error_definition;\n");
        render_children(&mut file, child, &namespace, 0);
        namespaces.insert((*name).to_owned(), parsed(file)?);
    }
    Ok(Module {
        parent: parsed(parent)?,
        namespaces,
    })
}

fn render_children(output: &mut String, node: &Node<'_>, namespace: &str, depth: usize) {
    let indent = INDENT.repeat(depth);
    for (name, child) in &node.children {
        output.push('\n');
        if let Some(error) = child.error {
            render_error(output, error, &indent);
        } else {
            let namespace = format!("{namespace}.{name}");
            let _ = writeln!(output, "{indent}/// Registered errors under `{namespace}`.");
            let _ = writeln!(output, "{indent}pub mod {name} {{");
            let _ = writeln!(
                output,
                "{indent}{INDENT}use super::__rift_error_definition;"
            );
            render_children(output, child, &namespace, depth + 1);
            let _ = writeln!(output, "{indent}}}");
        }
    }
}

fn render_error(output: &mut String, error: &ir::Error, indent: &str) {
    let name = error.path.last().expect("validated error path");
    let _ = writeln!(output, "{indent}__rift_error_definition!(");
    let _ = writeln!(output, "{indent}{INDENT}{name},");
    let _ = writeln!(output, "{indent}{INDENT}slug = {},", literal(&error.slug));
    let _ = writeln!(
        output,
        "{indent}{INDENT}message = {},",
        literal(&error.message)
    );
    let _ = writeln!(
        output,
        "{indent}{INDENT}action = {},",
        literal(&error.action)
    );
    if error.fields.is_empty() {
        let _ = writeln!(output, "{indent}{INDENT}fields = {{}},");
    } else {
        let _ = writeln!(output, "{indent}{INDENT}fields = {{");
        for field in &error.fields {
            let _ = writeln!(output, "{indent}{INDENT}{INDENT}{},", declaration(field));
        }
        let _ = writeln!(output, "{indent}{INDENT}}},");
    }
    let _ = writeln!(output, "{indent});");
}

/// Renders `name: presence(kind, flags...)` for one field.
fn declaration(field: &ir::Field) -> String {
    let presence = if field.optional {
        "optional"
    } else {
        "required"
    };
    let mut arguments = vec![field.kind.word()];
    if !field.display {
        arguments.push("hidden");
    }
    if field.sensitive {
        arguments.push("sensitive");
    }
    format!("{}: {presence}({})", field.name, arguments.join(", "))
}

/// Renders a Rust string literal; `str`'s `Debug` form escapes quotes, backslashes,
/// and control characters with Rust escape syntax.
fn literal(value: &str) -> String {
    format!("{value:?}")
}
