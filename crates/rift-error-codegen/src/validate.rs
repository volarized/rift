use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::{ir, schema};

/// Registry parsing, validation, and Rust generation failure.
#[derive(Debug, Error)]
pub enum CodegenError {
    /// TOML input could not be deserialized into the registry schema.
    #[error("invalid registry TOML: {0}")]
    Toml(String),
    /// Registry content violates a schema rule.
    #[error("invalid registry: {0}")]
    Invalid(String),
    /// Generated tokens could not be parsed as a Rust file.
    #[error("generated Rust is invalid: {0}")]
    Rust(String),
}

pub(crate) fn validate(raw: schema::RawRegistry) -> Result<ir::Registry, CodegenError> {
    if raw.registry.schema != 1 {
        return Err(invalid(format!(
            "unsupported schema version {}",
            raw.registry.schema
        )));
    }
    let namespace = raw.registry.namespace;
    let namespace_parts = namespace.split('.').map(str::to_owned).collect::<Vec<_>>();
    if namespace_parts.is_empty() || namespace_parts.iter().any(|part| !valid_name(part)) {
        return Err(invalid(format!("invalid registry namespace {namespace:?}")));
    }

    let definitions = schema::definitions(raw.error)?;
    if definitions.is_empty() {
        return Err(invalid("registry defines no errors"));
    }
    let mut errors = Vec::with_capacity(definitions.len());
    let mut slugs = BTreeSet::new();
    for (path, raw_error) in definitions {
        if path.is_empty() || path.iter().any(|part| !valid_name(part)) {
            return Err(invalid(format!("invalid error path {}", path.join("."))));
        }
        let slug = format!("{}.{}", namespace, path.join("."));
        if !slugs.insert(slug.clone()) {
            return Err(invalid(format!("duplicate error identity {slug}")));
        }
        if raw_error.message.trim().is_empty() {
            return Err(invalid(format!("error {slug} has empty message")));
        }
        if raw_error.action.trim().is_empty() {
            return Err(invalid(format!("error {slug} has empty action")));
        }

        let mut fields = Vec::with_capacity(raw_error.fields.len());
        let mut role_counts = BTreeMap::<schema::FieldRole, usize>::new();
        let mut methods = BTreeMap::<String, String>::new();
        for (name, field) in raw_error.fields {
            if !valid_name(&name) {
                return Err(invalid(format!(
                    "error {slug} has invalid field name {name:?}"
                )));
            }
            let kind = validate_field(&slug, &name, &field)?;
            if let Some(role) = field.role {
                *role_counts.entry(role).or_default() += 1;
            }
            insert_method(&mut methods, &name, &name, &slug)?;
            if field.optional {
                let maybe = format!("maybe_{name}");
                insert_method(&mut methods, &maybe, &name, &slug)?;
            }
            fields.push(ir::Field {
                name,
                kind,
                optional: field.optional,
                display: field.display,
                sensitive: field.sensitive,
            });
        }
        for reserved in ["error", "fail", "evidence", "with", "mcp"] {
            if methods.contains_key(reserved) {
                return Err(invalid(format!(
                    "error {slug} field generates reserved method {reserved}"
                )));
            }
        }
        if role_counts
            .get(&schema::FieldRole::Source)
            .copied()
            .unwrap_or(0)
            > 1
            || role_counts
                .get(&schema::FieldRole::Cause)
                .copied()
                .unwrap_or(0)
                > 1
        {
            return Err(invalid(format!(
                "error {slug} declares duplicate source or cause roles"
            )));
        }
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        validate_placeholders(&slug, "message", &raw_error.message, &fields)?;
        validate_placeholders(&slug, "action", &raw_error.action, &fields)?;
        errors.push(ir::Error {
            path,
            slug,
            message: raw_error.message,
            action: raw_error.action,
            fields,
        });
    }
    errors.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ir::Registry { namespace, errors })
}

fn insert_method(
    methods: &mut BTreeMap<String, String>,
    method: &str,
    field: &str,
    slug: &str,
) -> Result<(), CodegenError> {
    if let Some(previous) = methods.insert(method.to_owned(), field.to_owned())
        && previous != field
    {
        return Err(invalid(format!(
            "error {slug} fields {previous} and {field} generate duplicate method {method}"
        )));
    }
    Ok(())
}

fn validate_field(
    slug: &str,
    name: &str,
    field: &schema::RawField,
) -> Result<ir::FieldKind, CodegenError> {
    use schema::{FieldFormat as Format, FieldRole as Role, FieldType as Type};
    match (field.field_type, field.format) {
        (Type::Path, Some(Format::Display)) | (Type::Duration, Some(Format::Human)) => {}
        (Type::Path, _) => {
            return Err(invalid(format!(
                "error {slug} path field {name} requires format = display"
            )));
        }
        (Type::Duration, _) => {
            return Err(invalid(format!(
                "error {slug} duration field {name} requires format = human"
            )));
        }
        (_, Some(_)) => {
            return Err(invalid(format!(
                "error {slug} field {name} does not support format"
            )));
        }
        (_, None) => {}
    }
    match (field.role, field.field_type) {
        (Some(Role::Source), Type::Error) => Ok(ir::FieldKind::Source),
        (Some(Role::Cause), Type::RiftError) => Ok(ir::FieldKind::Cause),
        (Some(Role::Source), _) => Err(invalid(format!(
            "error {slug} source role requires type = error"
        ))),
        (Some(Role::Cause), _) => Err(invalid(format!(
            "error {slug} cause role requires type = rift_error"
        ))),
        (None, Type::Error | Type::RiftError) => Err(invalid(format!(
            "error {slug} field {name} requires role = source or cause"
        ))),
        (None, Type::String) => Ok(ir::FieldKind::String),
        (None, Type::Bool) => Ok(ir::FieldKind::Bool),
        (None, Type::Integer) => Ok(ir::FieldKind::Integer),
        (None, Type::Unsigned) => Ok(ir::FieldKind::Unsigned),
        (None, Type::Pid) => Ok(ir::FieldKind::Pid),
        (None, Type::Port) => Ok(ir::FieldKind::Port),
        (None, Type::Path) => Ok(ir::FieldKind::Path),
        (None, Type::Duration) => Ok(ir::FieldKind::Duration),
    }
}

fn validate_placeholders(
    slug: &str,
    member: &str,
    text: &str,
    fields: &[ir::Field],
) -> Result<(), CodegenError> {
    let declared = fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let hidden = fields
        .iter()
        .filter(|field| !field.display)
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'{' if bytes.get(cursor + 1) == Some(&b'{') => cursor += 2,
            b'}' if bytes.get(cursor + 1) == Some(&b'}') => cursor += 2,
            b'{' => {
                let rest = &text[cursor + 1..];
                let Some(end) = rest.find('}') else {
                    return Err(invalid(format!(
                        "error {slug} has malformed {member} placeholder"
                    )));
                };
                let name = &rest[..end];
                if !valid_name(name) {
                    return Err(invalid(format!(
                        "error {slug} has malformed {member} placeholder {{{name}}}"
                    )));
                }
                if !declared.contains(name) {
                    return Err(invalid(format!(
                        "error {slug} {member} references unknown field {name}"
                    )));
                }
                if hidden.contains(name) {
                    return Err(invalid(format!(
                        "error {slug} {member} references hidden field {name}"
                    )));
                }
                cursor += end + 2;
            }
            b'}' => {
                return Err(invalid(format!(
                    "error {slug} has malformed {member} placeholder"
                )));
            }
            _ => cursor += 1,
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('_') || name.ends_with('_') || name.contains("__") {
        return false;
    }
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && syn::parse_str::<syn::Ident>(name).is_ok()
}

fn invalid(message: impl Into<String>) -> CodegenError {
    CodegenError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<ir::Registry, CodegenError> {
        validate(crate::schema::parse(source)?)
    }

    const GOOD: &str = r#"
[registry]
namespace = "rift"
schema = 1

[error.ranking.query_empty]
message = "query is empty"
action = "provide query text"
"#;

    #[test]
    fn accepts_valid_registry_and_stable_slug() {
        let registry = parse(GOOD).expect("valid registry");
        assert_eq!(registry.errors[0].slug, "rift.ranking.query_empty");
    }

    #[test]
    fn rejects_unsupported_schema_invalid_namespaces_and_empty_registries() {
        let unsupported = GOOD.replace("schema = 1", "schema = 2");
        assert_eq!(
            parse(&unsupported)
                .expect_err("schema version must be supported")
                .to_string(),
            "invalid registry: unsupported schema version 2"
        );
        for namespace in ["", "_rift", "rift_", "rift__cloud", "Rift", "rift..cloud"] {
            let input = GOOD.replace(
                "namespace = \"rift\"",
                &format!("namespace = {namespace:?}"),
            );
            assert_eq!(
                parse(&input)
                    .expect_err("namespace parts must be Rust names")
                    .to_string(),
                format!("invalid registry: invalid registry namespace {namespace:?}")
            );
        }
        let empty = "[registry]\nnamespace = \"rift\"\nschema = 1\n[error]\n";
        assert_eq!(
            parse(empty)
                .expect_err("a registry must define errors")
                .to_string(),
            "invalid registry: registry defines no errors"
        );
    }

    #[test]
    fn rejects_empty_messages_actions_and_non_table_error_paths() {
        for (before, member) in [
            ("query is empty", "message"),
            ("provide query text", "action"),
        ] {
            let input = GOOD.replace(before, "   ");
            assert_eq!(
                parse(&input)
                    .expect_err("messages and actions cannot be blank")
                    .to_string(),
                format!("invalid registry: error rift.ranking.query_empty has empty {member}")
            );
        }
        let input = "[registry]\nnamespace = \"rift\"\nschema = 1\n[error]\nranking = 7\n";
        assert_eq!(
            parse(input)
                .expect_err("error paths must contain tables")
                .to_string(),
            "invalid registry: error path ranking must be a table"
        );
    }

    #[test]
    fn rejects_invalid_field_names_and_reserved_builder_methods() {
        let input =
            format!("{GOOD}\n[error.ranking.query_empty.fields.bad-name]\ntype = \"string\"\n");
        assert_eq!(
            parse(&input)
                .expect_err("field names must be Rust names")
                .to_string(),
            "invalid registry: error rift.ranking.query_empty has invalid field name \"bad-name\""
        );
        for method in ["error", "fail", "evidence", "with", "mcp"] {
            let input =
                format!("{GOOD}\n[error.ranking.query_empty.fields.{method}]\ntype = \"string\"\n");
            assert_eq!(
                parse(&input)
                    .expect_err("fields cannot replace builder methods")
                    .to_string(),
                format!(
                    "invalid registry: error rift.ranking.query_empty field generates reserved method {method}"
                )
            );
        }
    }

    #[test]
    fn rejects_missing_duration_format_and_format_on_plain_fields() {
        for (definition, detail) in [
            (
                "type = \"duration\"",
                "duration field value requires format = human",
            ),
            (
                "type = \"string\"\nformat = \"display\"",
                "field value does not support format",
            ),
        ] {
            let input = format!("{GOOD}\n[error.ranking.query_empty.fields.value]\n{definition}\n");
            assert_eq!(
                parse(&input)
                    .expect_err("formats must match their field type")
                    .to_string(),
                format!("invalid registry: error rift.ranking.query_empty {detail}")
            );
        }
    }

    #[test]
    fn accepts_escaped_braces_but_rejects_malformed_and_hidden_placeholders() {
        let escaped = GOOD.replace("query is empty", "query {{text}} is empty");
        assert_eq!(
            parse(&escaped).expect("escaped braces are literal").errors[0].message,
            "query {{text}} is empty"
        );
        for (message, detail) in [
            (
                "query {bad-name} is empty",
                "malformed message placeholder {bad-name}",
            ),
            ("query } is empty", "malformed message placeholder"),
        ] {
            let input = GOOD.replace("query is empty", message);
            assert_eq!(
                parse(&input)
                    .expect_err("placeholder syntax must be valid")
                    .to_string(),
                format!("invalid registry: error rift.ranking.query_empty has {detail}")
            );
        }
        let hidden = format!(
            "{}\n[error.ranking.query_empty.fields.internal]\ntype = \"string\"\ndisplay = false\n",
            GOOD.replace("provide query text", "retry {internal}")
        );
        assert_eq!(
            parse(&hidden)
                .expect_err("actions cannot expose hidden fields")
                .to_string(),
            "invalid registry: error rift.ranking.query_empty action references hidden field internal"
        );
    }

    #[test]
    fn rejects_unknown_placeholder() {
        let input = GOOD.replace("query is empty", "query {field} is empty");
        assert!(
            matches!(parse(&input), Err(CodegenError::Invalid(message)) if message.contains("unknown field field"))
        );
    }

    #[test]
    fn rejects_unknown_field_type_and_role() {
        let input = format!("{GOOD}\n[error.ranking.query_empty.fields.bad]\ntype = \"float\"\n");
        assert!(parse(&input).is_err());
        let input = GOOD.replace("action = \"provide query text\"", "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.source]\ntype = \"string\"\nrole = \"source\"");
        assert!(
            matches!(parse(&input), Err(CodegenError::Invalid(message)) if message.contains("source role requires type"))
        );
    }

    #[test]
    fn rejects_malformed_placeholders_and_method_collisions() {
        let malformed = GOOD.replace("query is empty", "query {field is empty");
        assert!(
            matches!(parse(&malformed), Err(CodegenError::Invalid(message)) if message.contains("malformed message placeholder"))
        );
        let collision = GOOD.replace("action = \"provide query text\"", "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.foo]\ntype = \"string\"\noptional = true\n\n[error.ranking.query_empty.fields.maybe_foo]\ntype = \"string\"");
        assert!(
            matches!(parse(&collision), Err(CodegenError::Invalid(message)) if message.contains("duplicate method maybe_foo"))
        );
    }

    #[test]
    fn rejects_duplicate_definitions_unknown_members_and_invalid_names() {
        let duplicate = format!("{GOOD}\n[error.ranking.query_empty]\nmessage = \"again\"\n");
        assert!(matches!(
            crate::schema::parse(&duplicate),
            Err(CodegenError::Toml(_))
        ));

        let unknown = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\nunknown = true",
        );
        assert!(matches!(parse(&unknown), Err(CodegenError::Invalid(_))));

        let invalid_name = GOOD.replace("query_empty", "type");
        assert!(matches!(
            parse(&invalid_name),
            Err(CodegenError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_incompatible_format_and_duplicate_roles() {
        let wrong_format = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.path]\ntype = \"path\"\nformat = \"human\"",
        );
        assert!(
            matches!(parse(&wrong_format), Err(CodegenError::Invalid(message)) if message.contains("requires format = display"))
        );

        let duplicate_roles = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.source]\ntype = \"error\"\nrole = \"source\"\n\n[error.ranking.query_empty.fields.other_source]\ntype = \"error\"\nrole = \"source\"",
        );
        assert!(
            matches!(parse(&duplicate_roles), Err(CodegenError::Invalid(message)) if message.contains("duplicate source or cause"))
        );
    }

    #[test]
    fn rejects_invalid_cause_and_untyped_error_fields() {
        let wrong_cause = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.cause]\ntype = \"string\"\nrole = \"cause\"",
        );
        assert!(matches!(
            parse(&wrong_cause),
            Err(CodegenError::Invalid(message)) if message.contains("cause role requires type = rift_error")
        ));

        let missing_role = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.cause]\ntype = \"rift_error\"",
        );
        assert!(matches!(
            parse(&missing_role),
            Err(CodegenError::Invalid(message)) if message.contains("requires role = source or cause")
        ));
    }

    #[test]
    fn rejects_unknown_role_and_format_values() {
        let unknown_role = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.detail]\ntype = \"string\"\nrole = \"wrapper\"",
        );
        assert!(matches!(
            parse(&unknown_role),
            Err(CodegenError::Invalid(message))
                if message.contains("unknown variant `wrapper`")
                    && message.contains("fields.detail.role")
        ));

        let unknown_format = GOOD.replace(
            "action = \"provide query text\"",
            "action = \"provide query text\"\n\n[error.ranking.query_empty.fields.path]\ntype = \"path\"\nformat = \"canonical\"",
        );
        assert!(matches!(
            parse(&unknown_format),
            Err(CodegenError::Invalid(message))
                if message.contains("unknown variant `canonical`")
                    && message.contains("fields.path.format")
        ));
    }
}
