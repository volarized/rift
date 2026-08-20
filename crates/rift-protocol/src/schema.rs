//! Schema rules the derive attributes cannot express, attached to models
//! with `#[schemars(transform = schema::...)]`.
//!
//! Every rule is built from the vocabulary in this module: JSON Schema
//! keywords are spelled once in the private `keyword` module, model property
//! names are proven against the model structs by the `property!` macro, and
//! wire values come from serializing the model enums themselves.
//! [`nullable`] is the field-level
//! rule: it restores the `null` arm that `#[schemars(required)]` removes
//! from an `Option` field.

use crate::read::{SearchHit, SearchParams, SearchParamsTarget, SearchScope};
use schemars::Schema;
use serde::Serialize;
use serde_json::{Map, Value, json};

/// JSON Schema keywords, spelled once. No crate in the stack exports these:
/// `schemars` ships only meta-schema URIs, `jsonschema` is a validator.
mod keyword {
    pub(super) const ALL_OF: &str = "allOf";
    pub(super) const ANY_OF: &str = "anyOf";
    pub(super) const ONE_OF: &str = "oneOf";
    pub(super) const NOT: &str = "not";
    pub(super) const IF: &str = "if";
    pub(super) const THEN: &str = "then";
    pub(super) const PROPERTIES: &str = "properties";
    pub(super) const REQUIRED: &str = "required";
    pub(super) const CONST: &str = "const";
    pub(super) const ENUM: &str = "enum";
    pub(super) const TYPE: &str = "type";
    pub(super) const NULL: &str = "null";
    pub(super) const REFERENCE: &str = "$ref";
    pub(super) const DESCRIPTION: &str = "description";
    pub(super) const MAX_ITEMS: &str = "maxItems";
}

/// The serde property name of one model field, proven against the model:
/// this fails to compile when the field is renamed or removed. Serde-level
/// renames are caught by [`tests::rule_properties_exist_in_model_schemas`].
macro_rules! property {
    ($owner:ty, $field:ident) => {{
        const _: fn(&$owner) = |owner: &$owner| {
            let _ = &owner.$field;
        };
        stringify!($field)
    }};
}

/// The serde tag property of [`FileContent`](crate::read::FileContent),
/// pinned by [`tests::tagged_union_tags_exist_in_generated_schemas`].
const FILE_CONTENT_TAG: &str = "kind";
/// The tag value of its symlink variant, pinned by the same test.
const FILE_CONTENT_SYMLINK: &str = "symlink";
/// The serde tag property of [`SearchHitTarget`](crate::read::SearchHitTarget),
/// pinned by [`tests::tagged_union_tags_exist_in_generated_schemas`].
const SEARCH_HIT_TARGET_TAG: &str = "target";
/// Its node and file tag values, pinned by the same test.
const SEARCH_HIT_NODE: &str = "node";
const SEARCH_HIT_FILE: &str = "file";

/// A JSON Schema composition keyword a rule may extend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Composition {
    /// `allOf`: every clause must hold.
    All,
    /// `anyOf`: at least one clause must hold.
    Any,
}

impl Composition {
    fn keyword(self) -> &'static str {
        match self {
            Self::All => keyword::ALL_OF,
            Self::Any => keyword::ANY_OF,
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

/// A single-keyword clause, consuming its already-built subclause.
fn keyed(key: &str, value: Value) -> Value {
    let mut clause = Map::new();
    clause.insert(key.to_owned(), value);
    Value::Object(clause)
}

/// A clause satisfied when every named property is present.
fn requires(properties: &[&str]) -> Value {
    json!({ keyword::REQUIRED: properties })
}

/// A clause satisfied when `clause` is not.
fn not(clause: Value) -> Value {
    keyed(keyword::NOT, clause)
}

/// A clause satisfied when at least one of `clauses` is.
fn any_of(clauses: Vec<Value>) -> Value {
    keyed(keyword::ANY_OF, Value::Array(clauses))
}

/// A clause satisfied when exactly one of `clauses` is.
fn one_of(clauses: Vec<Value>) -> Value {
    keyed(keyword::ONE_OF, Value::Array(clauses))
}

/// A clause constraining named properties, each by its own subclause.
fn properties(entries: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (name, clause) in entries {
        map.insert(name.to_owned(), clause);
    }
    keyed(keyword::PROPERTIES, Value::Object(map))
}

/// A conditional clause: where `condition` holds, `outcome` must hold.
fn when(condition: Value, outcome: Value) -> Value {
    let mut clause = Map::new();
    clause.insert(keyword::IF.to_owned(), condition);
    clause.insert(keyword::THEN.to_owned(), outcome);
    Value::Object(clause)
}

/// A subclause pinning a property to the wire form of one model value.
fn constant<T: Serialize>(value: &T) -> Value {
    json!({ keyword::CONST: wire(value) })
}

/// A subclause admitting the wire forms of the listed model values.
fn enumeration<T: Serialize>(values: &[T]) -> Value {
    let admitted: Vec<Value> = values.iter().map(wire).collect();
    json!({ keyword::ENUM: admitted })
}

/// A subclause bounding an array property's length.
fn max_items(count: u64) -> Value {
    json!({ keyword::MAX_ITEMS: count })
}

/// The same clause, carrying a reader-facing description.
fn described(description: &str, mut clause: Value) -> Value {
    if let Some(object) = clause.as_object_mut() {
        object.insert(
            keyword::DESCRIPTION.to_owned(),
            Value::String(description.to_owned()),
        );
    }
    clause
}

/// The wire form of one model value, serialized by the model itself.
fn wire<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value)
        .unwrap_or_else(|error| unreachable!("wire models serialize to JSON values: {error}"))
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
    match object.get_mut(keyword::TYPE) {
        Some(Value::String(name)) => {
            let name = std::mem::take(name);
            object.insert(keyword::TYPE.to_owned(), json!([name, keyword::NULL]));
        }
        Some(Value::Array(names)) => {
            if !names.iter().any(|name| name == keyword::NULL) {
                names.push(json!(keyword::NULL));
            }
        }
        _ => nullable_without_type(object),
    }
}

/// Extends a schema that names no `type`: a reference, an enumeration, or a
/// composition-only object gains an explicit `null` arm.
fn nullable_without_type(object: &mut Map<String, Value>) {
    let null_arm = json!({ keyword::TYPE: keyword::NULL });
    if let Some(reference) = object.remove(keyword::REFERENCE) {
        object.insert(
            keyword::ANY_OF.to_owned(),
            json!([{ keyword::REFERENCE: reference }, null_arm]),
        );
        return;
    }
    if let Some(Value::Array(values)) = object.get_mut(keyword::ENUM) {
        if !values.iter().any(Value::is_null) {
            values.push(Value::Null);
        }
        return;
    }
    let description = object.remove(keyword::DESCRIPTION);
    let inner = std::mem::take(object);
    if let Some(description) = description {
        object.insert(keyword::DESCRIPTION.to_owned(), description);
    }
    object.insert(keyword::ANY_OF.to_owned(), json!([inner, null_arm]));
}

/// A symlink [`File`](crate::read::File) carries no language facts: languages
/// and regions stay empty and `semantic` stays false.
pub fn forbid_symlink_language_facts(schema: &mut Schema) {
    use crate::read::File;
    append(
        schema,
        Composition::All,
        when(
            properties(vec![(
                property!(File, content),
                properties(vec![(
                    FILE_CONTENT_TAG,
                    json!({ keyword::CONST: FILE_CONTENT_SYMLINK }),
                )]),
            )]),
            properties(vec![
                (property!(File, languages), max_items(0)),
                (property!(File, regions), max_items(0)),
                (property!(File, semantic), constant(&false)),
            ]),
        ),
    );
}

/// A [`RelationFilter`](crate::read::RelationFilter) names what it matches by:
/// an exact `kind` in one language's vocabulary, or a portable `facet`.
pub fn require_kind_or_facet(schema: &mut Schema) {
    use crate::read::RelationFilter;
    append(
        schema,
        Composition::Any,
        described(
            "Matching by exact kind, in one language's vocabulary.",
            requires(&[property!(RelationFilter, kind)]),
        ),
    );
    append(
        schema,
        Composition::Any,
        described(
            "Matching by portable facet, which reaches every served language.",
            requires(&[property!(RelationFilter, facet)]),
        ),
    );
}

/// A [`SearchHit`] carries `span` and `line` together or not at all, and node
/// and file hits always carry both.
pub fn pair_span_with_line(schema: &mut Schema) {
    let span_and_line = [property!(SearchHit, span), property!(SearchHit, line)];
    append(
        schema,
        Composition::All,
        one_of(vec![
            requires(&span_and_line),
            not(any_of(vec![
                requires(&span_and_line[..1]),
                requires(&span_and_line[1..]),
            ])),
        ]),
    );
    append(
        schema,
        Composition::All,
        when(
            properties(vec![(
                property!(SearchHit, hit),
                properties(vec![(
                    SEARCH_HIT_TARGET_TAG,
                    json!({ keyword::ENUM: [SEARCH_HIT_NODE, SEARCH_HIT_FILE] }),
                )]),
            )]),
            requires(&span_and_line),
        ),
    );
}

/// [`SearchParams`] with a `traversal` keep `target` at `symbol` or `all`,
/// and `paths` only narrow project-scoped searches.
pub fn restrict_traversal_and_paths(schema: &mut Schema) {
    append(
        schema,
        Composition::All,
        when(
            requires(&[property!(SearchParams, traversal)]),
            properties(vec![(
                property!(SearchParams, target),
                enumeration(&[SearchParamsTarget::Symbol, SearchParamsTarget::All]),
            )]),
        ),
    );
    append(
        schema,
        Composition::All,
        when(
            requires(&[property!(SearchParams, paths)]),
            properties(vec![(
                property!(SearchParams, scope),
                constant(&SearchScope::Project),
            )]),
        ),
    );
}

/// [`SearchParams`] ask for something: a text `query`, a provider `filter`,
/// or a bounded relationship `traversal`.
pub fn require_query_filter_or_traversal(schema: &mut Schema) {
    append(
        schema,
        Composition::Any,
        described(
            "Satisfied by a text query, with or without a filter alongside it.",
            requires(&[property!(SearchParams, query)]),
        ),
    );
    append(
        schema,
        Composition::Any,
        described(
            "Satisfied by a filter alone, for a search with no text to match.",
            requires(&[property!(SearchParams, filter)]),
        ),
    );
    append(
        schema,
        Composition::Any,
        described(
            "Satisfied by a bounded relationship traversal.",
            requires(&[property!(SearchParams, traversal)]),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{File, FileContent, SearchHitTarget};
    use schemars::schema_for;

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

    #[test]
    fn test_clause_builders_spell_their_keywords() {
        assert_eq!(requires(&["query"]), json!({ "required": ["query"] }));
        assert_eq!(
            not(requires(&["limit"])),
            json!({ "not": { "required": ["limit"] } })
        );
        assert_eq!(
            when(requires(&["paths"]), max_items(0)),
            json!({ "if": { "required": ["paths"] }, "then": { "maxItems": 0 } })
        );
        assert_eq!(
            properties(vec![("semantic", constant(&false))]),
            json!({ "properties": { "semantic": { "const": false } } })
        );
        assert_eq!(
            enumeration(&[SearchScope::Project, SearchScope::All]),
            json!({ "enum": ["project", "all"] })
        );
        assert_eq!(
            described("Why.", one_of(vec![requires(&["a"])])),
            json!({ "description": "Why.", "oneOf": [{ "required": ["a"] }] })
        );
        assert_eq!(
            any_of(vec![requires(&["a"]), requires(&["b"])]),
            json!({ "anyOf": [{ "required": ["a"] }, { "required": ["b"] }] })
        );
    }

    /// The `property!` macro proves a field exists on the struct; this test
    /// proves serde serves it under the same name, closing the rename gap.
    #[test]
    fn rule_properties_exist_in_model_schemas() {
        let cases: [(&str, Value, &[&str]); 4] = [
            (
                "File",
                serde_json::to_value(schema_for!(File)).expect("schema"),
                &["content", "languages", "regions", "semantic"],
            ),
            (
                "SearchHit",
                serde_json::to_value(schema_for!(SearchHit)).expect("schema"),
                &["hit", "span", "line"],
            ),
            (
                "SearchParams",
                serde_json::to_value(schema_for!(SearchParams)).expect("schema"),
                &["query", "filter", "traversal", "target", "scope", "paths"],
            ),
            (
                "RelationFilter",
                serde_json::to_value(schema_for!(crate::read::RelationFilter)).expect("schema"),
                &["kind", "facet"],
            ),
        ];
        for (model, schema, names) in cases {
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{model} schema must carry properties"));
            for name in names {
                assert!(
                    properties.contains_key(*name),
                    "{model} schema must serve property {name}: a serde rename \
                     would silently detach the rule from the model"
                );
            }
        }
    }

    /// Collects each variant's tag value from a tagged union's generated
    /// schema, wherever the generator placed it.
    fn tag_values(schema: &Value, tag: &str) -> Vec<String> {
        let mut values = Vec::new();
        let arms = [schema.get("oneOf"), schema.get("anyOf")];
        for arm in arms.into_iter().flatten().filter_map(Value::as_array) {
            for variant in arm {
                let tagged = &variant["properties"][tag];
                if let Some(value) = tagged["const"].as_str() {
                    values.push(value.to_owned());
                }
                if let Some(admitted) = tagged["enum"].as_array() {
                    values.extend(admitted.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
        values
    }

    /// The tag constants cannot be proven by `property!`; this pins them to
    /// the generated union schemas instead.
    #[test]
    fn tagged_union_tags_exist_in_generated_schemas() {
        let file_content =
            serde_json::to_value(schema_for!(FileContent)).expect("FileContent schema");
        let content_tags = tag_values(&file_content, FILE_CONTENT_TAG);
        assert!(
            content_tags.iter().any(|tag| tag == FILE_CONTENT_SYMLINK),
            "FileContent must serve a {FILE_CONTENT_SYMLINK} variant under tag \
             {FILE_CONTENT_TAG}, got {content_tags:?}"
        );
        let hit_target =
            serde_json::to_value(schema_for!(SearchHitTarget)).expect("SearchHitTarget schema");
        let target_tags = tag_values(&hit_target, SEARCH_HIT_TARGET_TAG);
        for expected in [SEARCH_HIT_NODE, SEARCH_HIT_FILE] {
            assert!(
                target_tags.iter().any(|tag| tag == expected),
                "SearchHitTarget must serve a {expected} variant under tag \
                 {SEARCH_HIT_TARGET_TAG}, got {target_tags:?}"
            );
        }
    }
}
