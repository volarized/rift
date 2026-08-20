//! Schema rules the derive attributes cannot express, attached to models
//! with `#[schemars(transform = schema::...)]`.
//!
//! Each cross-field rule appends one composition clause to its model's
//! schema. The clauses are authored as structured [`serde_json::json!`]
//! values so every rule reads as indented data with one field per line.
//! [`nullable`] is the field-level rule: it restores the `null` arm that
//! `#[schemars(required)]` removes from an `Option` field.

use schemars::Schema;
use serde_json::{Map, Value, json};

/// A JSON Schema composition keyword a rule may extend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Composition {
    /// `allOf`: every clause must hold.
    All,
    /// `anyOf`: at least one clause must hold.
    Any,
}

impl Composition {
    /// The keyword as JSON Schema spells it — the one place the spelling lives.
    fn keyword(self) -> &'static str {
        match self {
            Self::All => "allOf",
            Self::Any => "anyOf",
        }
    }
}

/// Appends `clause` to the `composition` array of `schema`, creating the
/// array on first use so several rules can target the same keyword.
fn append(schema: &mut Schema, composition: Composition, clause: Value) {
    let clauses = schema
        .ensure_object()
        .entry(composition.keyword())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(values) = clauses {
        values.push(clause);
    }
}

/// Restores the `null` arm that `#[schemars(required)]` removes from an
/// `Option` field's schema.
///
/// The attribute keeps the field in `required` by generating the inner
/// type's schema, and that schema alone rejects the `null` the server
/// serializes for `None`. Every required-but-nullable field pairs the two:
/// `#[schemars(required, transform = schema::nullable)]`.
pub fn nullable(schema: &mut Schema) {
    let object = schema.ensure_object();
    match object.get_mut("type") {
        Some(Value::String(name)) => {
            let name = std::mem::take(name);
            object.insert("type".to_owned(), json!([name, "null"]));
        }
        Some(Value::Array(names)) => {
            if !names.iter().any(|name| name == "null") {
                names.push(json!("null"));
            }
        }
        _ => nullable_without_type(object),
    }
}

/// Extends a schema that names no `type`: a reference, an enumeration, or a
/// composition-only object gains an explicit `null` arm.
fn nullable_without_type(object: &mut Map<String, Value>) {
    if let Some(reference) = object.remove("$ref") {
        object.insert(
            "anyOf".to_owned(),
            json!([{ "$ref": reference }, { "type": "null" }]),
        );
        return;
    }
    if let Some(Value::Array(values)) = object.get_mut("enum") {
        if !values.iter().any(Value::is_null) {
            values.push(Value::Null);
        }
        return;
    }
    let description = object.remove("description");
    let inner = std::mem::take(object);
    if let Some(description) = description {
        object.insert("description".to_owned(), description);
    }
    object.insert("anyOf".to_owned(), json!([inner, { "type": "null" }]));
}

/// An [`ErrorData`](crate::error::ErrorData) carries `limit` only when
/// `code` is `limit_exceeded`; any other code forbids it.
pub fn error_limit_rides_limit_exceeded(schema: &mut Schema) {
    append(
        schema,
        Composition::All,
        json!({
            "if": {
                "properties": { "code": { "const": "limit_exceeded" } },
                "required": ["code"]
            },
            "else": { "not": { "required": ["limit"] } }
        }),
    );
}

/// A symlink [`File`](crate::read::File) carries no language facts: languages
/// and regions stay empty and `semantic` stays false.
pub fn symlink_carries_no_language_facts(schema: &mut Schema) {
    append(
        schema,
        Composition::All,
        json!({
            "if": {
                "properties": {
                    "content": { "properties": { "kind": { "const": "symlink" } } }
                }
            },
            "then": {
                "properties": {
                    "languages": { "maxItems": 0 },
                    "regions": { "maxItems": 0 },
                    "semantic": { "const": false }
                }
            }
        }),
    );
}

/// A [`RelationFilter`](crate::read::RelationFilter) names what it matches by:
/// an exact `kind` in one language's vocabulary, or a portable `facet`.
pub fn relation_filter_names_kind_or_facet(schema: &mut Schema) {
    append(
        schema,
        Composition::Any,
        json!({
            "description": "Matching by exact kind, in one language's vocabulary.",
            "required": ["kind"]
        }),
    );
    append(
        schema,
        Composition::Any,
        json!({
            "description": "Matching by portable facet, which reaches every served language.",
            "required": ["facet"]
        }),
    );
}

/// A [`SearchHit`](crate::read::SearchHit) carries `span` and `line` together
/// or not at all, and node and file hits always carry both.
pub fn search_hit_pairs_span_with_line(schema: &mut Schema) {
    append(
        schema,
        Composition::All,
        json!({
            "oneOf": [
                { "required": ["span", "line"] },
                { "not": { "anyOf": [ { "required": ["span"] }, { "required": ["line"] } ] } }
            ]
        }),
    );
    append(
        schema,
        Composition::All,
        json!({
            "if": {
                "properties": {
                    "hit": { "properties": { "target": { "enum": ["node", "file"] } } }
                }
            },
            "then": { "required": ["span", "line"] }
        }),
    );
}

/// [`SearchParams`](crate::read::SearchParams) with a `traversal` keep
/// `target` at `symbol` or `all`, and `paths` only narrow project-scoped
/// searches.
pub fn search_traversal_and_paths_stay_in_scope(schema: &mut Schema) {
    append(
        schema,
        Composition::All,
        json!({
            "if": { "required": ["traversal"] },
            "then": { "properties": { "target": { "enum": ["symbol", "all"] } } }
        }),
    );
    append(
        schema,
        Composition::All,
        json!({
            "if": { "required": ["paths"] },
            "then": { "properties": { "scope": { "const": "project" } } }
        }),
    );
}

/// [`SearchParams`](crate::read::SearchParams) ask for something: a text
/// `query`, a provider `filter`, or a bounded relationship `traversal`.
pub fn search_names_query_filter_or_traversal(schema: &mut Schema) {
    append(
        schema,
        Composition::Any,
        json!({
            "description": "Satisfied by a text query, with or without a filter alongside it.",
            "required": ["query"]
        }),
    );
    append(
        schema,
        Composition::Any,
        json!({
            "description": "Satisfied by a filter alone, for a search with no text to match.",
            "required": ["filter"]
        }),
    );
    append(
        schema,
        Composition::Any,
        json!({
            "description": "Satisfied by a bounded relationship traversal.",
            "required": ["traversal"]
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_from(value: Value) -> Schema {
        Schema::try_from(value).expect("test schema literal must be a valid schema object")
    }

    #[test]
    fn test_nullable_scalar_type_becomes_type_array() {
        let mut schema = schema_from(json!({ "type": "string", "minLength": 1 }));
        nullable(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({ "type": ["string", "null"], "minLength": 1 })
        );
    }

    #[test]
    fn test_nullable_type_array_gains_null_once() {
        let mut schema = schema_from(json!({ "type": ["string", "integer"] }));
        nullable(&mut schema);
        nullable(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({ "type": ["string", "integer", "null"] })
        );
    }

    #[test]
    fn test_nullable_reference_gains_any_of_null_arm() {
        let mut schema = schema_from(json!({
            "$ref": "#/$defs/Cursor",
            "description": "Cursor for the next page."
        }));
        nullable(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({
                "description": "Cursor for the next page.",
                "anyOf": [{ "$ref": "#/$defs/Cursor" }, { "type": "null" }]
            })
        );
    }

    #[test]
    fn test_nullable_enum_gains_null_value_once() {
        let mut schema = schema_from(json!({ "enum": ["authored", "generated"] }));
        nullable(&mut schema);
        nullable(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({ "enum": ["authored", "generated", null] })
        );
    }

    #[test]
    fn test_nullable_composition_only_schema_is_wrapped_keeping_description() {
        let mut schema = schema_from(json!({
            "description": "One of two shapes.",
            "oneOf": [{ "required": ["span"] }, { "required": ["line"] }]
        }));
        nullable(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({
                "description": "One of two shapes.",
                "anyOf": [
                    { "oneOf": [{ "required": ["span"] }, { "required": ["line"] }] },
                    { "type": "null" }
                ]
            })
        );
    }

    #[test]
    fn test_append_creates_then_extends_one_keyword() {
        let mut schema = schema_from(json!({}));
        append(&mut schema, Composition::All, json!({ "required": ["a"] }));
        append(&mut schema, Composition::All, json!({ "required": ["b"] }));
        append(&mut schema, Composition::Any, json!({ "required": ["c"] }));
        assert_eq!(
            schema.as_value(),
            &json!({
                "allOf": [{ "required": ["a"] }, { "required": ["b"] }],
                "anyOf": [{ "required": ["c"] }]
            })
        );
    }
}
