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

pub(crate) fn generate(registry: &ir::Registry) -> Result<String, CodegenError> {
    let mut root = Node::default();
    for error in &registry.errors {
        let mut node = &mut root;
        for part in &error.path {
            node = node.children.entry(part.as_str()).or_default();
        }
        node.error = Some(error);
    }

    let mut output = String::new();
    output.push_str("use rift_error::__rift_error_definition;\n\n");
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
    render_children(&mut output, &root, &registry.namespace, 0);

    syn::parse_file(&output).map_err(|error| CodegenError::Rust(error.to_string()))?;
    Ok(output)
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
