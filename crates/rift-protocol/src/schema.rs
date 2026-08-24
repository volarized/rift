//! Schema rules the derive attributes cannot express, attached to models
//! with `#[schemars(transform = schema::...)]`.
//!
//! Every rule is built from the vocabulary in this module: JSON Schema
//! keywords are spelled once in the private `keyword` module, model property
//! names are proven against the model structs by the `property!` macro, and
//! wire values come from serializing the model enums themselves.

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
    pub(super) const ELSE: &str = "else";
    pub(super) const PROPERTIES: &str = "properties";
    pub(super) const REQUIRED: &str = "required";
    pub(super) const CONST: &str = "const";
    pub(super) const ENUM: &str = "enum";
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

/// The Rift extension keyword stating an accepted range schema validation
/// cannot compare itself: the bounds of a string-spelled `ByteSize` or
/// `Duration` key.
const RIFT_RANGE: &str = "rift:range";

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

/// A negative conditional clause: anywhere `condition` fails, `alternative`
/// must hold. It carries no `then` arm, so where the condition holds the
/// clause imposes nothing.
fn otherwise(condition: Value, alternative: Value) -> Value {
    let mut clause = Map::new();
    clause.insert(keyword::IF.to_owned(), condition);
    clause.insert(keyword::ELSE.to_owned(), alternative);
    Value::Object(clause)
}

/// One clause carrying several keyword facets at once, shallow-merging
/// single-keyword clauses such as [`properties`] with [`requires`].
fn merged(clauses: Vec<Value>) -> Value {
    let mut map = Map::new();
    for clause in clauses {
        if let Value::Object(facets) = clause {
            map.extend(facets);
        }
    }
    Value::Object(map)
}

/// A subclause pinning a property to the wire form of one model value.
fn constant<T: Serialize>(value: &T) -> Value {
    json!({ keyword::CONST: wire(value) })
}

/// A subclause accepting the wire forms of the listed model values.
fn enumeration<T: Serialize>(values: &[T]) -> Value {
    let accepted: Vec<Value> = values.iter().map(wire).collect();
    json!({ keyword::ENUM: accepted })
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

/// A `rift:range` value: the smallest and largest accepted spelling, both
/// serialized by the value type itself.
fn range<T: Serialize>(min: &T, max: &T) -> Value {
    json!({ "min": wire(min), "max": wire(max) })
}

/// Adds `annotation` under `key` on one named property of `schema`, for
/// extension keywords that ride a property's own clause.
fn annotate_property(schema: &mut Schema, name: &str, key: &str, annotation: Value) {
    let property = schema
        .ensure_object()
        .get_mut(keyword::PROPERTIES)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut);
    if let Some(property) = property {
        property.insert(key.to_owned(), annotation);
    }
}

/// An [`ExecutionConfiguration`](crate::configuration::ExecutionConfiguration)
/// states each `ByteSize` and `Duration` ceiling as `rift:range` on its key:
/// schema validation alone cannot compare `"16kb"` against a ceiling, so
/// the server enforces the bound at load and the schema carries it for
/// readers.
pub fn declare_execution_ranges(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, Duration, EXECUTION_CODE_BYTES_MAX, EXECUTION_OUTPUT_BYTES_MAX,
        EXECUTION_TIMEOUT_MS_MAX, ExecutionConfiguration,
    };
    let ranges = [
        (
            property!(ExecutionConfiguration, max_code),
            range(
                &ByteSize::from_bytes(1),
                &ByteSize::from_bytes(EXECUTION_CODE_BYTES_MAX),
            ),
        ),
        (
            property!(ExecutionConfiguration, max_timeout),
            range(
                &Duration::from_millis(1),
                &Duration::from_millis(EXECUTION_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(ExecutionConfiguration, max_output),
            range(
                &ByteSize::from_bytes(0),
                &ByteSize::from_bytes(EXECUTION_OUTPUT_BYTES_MAX),
            ),
        ),
    ];
    for (name, accepted) in ranges {
        annotate_property(schema, name, RIFT_RANGE, accepted);
    }
}

/// A [`ServerConfiguration`](crate::configuration::ServerConfiguration)
/// states its `Duration` ceiling as `rift:range` on the key: schema
/// validation alone cannot compare `"30s"` against a ceiling, so the server
/// enforces the bound at load and the schema carries it for readers.
pub fn declare_server_ranges(schema: &mut Schema) {
    use crate::configuration::{
        Duration, SERVER_IDLE_TIMEOUT_MS_MAX, SERVER_IDLE_TIMEOUT_MS_MIN,
        SERVER_QUEUE_TIMEOUT_MS_MAX, ServerConfiguration,
    };
    annotate_property(
        schema,
        property!(ServerConfiguration, worker_queue_timeout),
        RIFT_RANGE,
        range(
            &Duration::from_millis(1),
            &Duration::from_millis(SERVER_QUEUE_TIMEOUT_MS_MAX),
        ),
    );
    annotate_property(
        schema,
        property!(ServerConfiguration, idle_timeout),
        RIFT_RANGE,
        range(
            &Duration::from_millis(SERVER_IDLE_TIMEOUT_MS_MIN),
            &Duration::from_millis(SERVER_IDLE_TIMEOUT_MS_MAX),
        ),
    );
    append(
        schema,
        Composition::All,
        described(
            "server.port and server.port_range are mutually exclusive",
            not(requires(&[
                property!(ServerConfiguration, port),
                property!(ServerConfiguration, port_range),
            ])),
        ),
    );
}

/// A [`SearchConfiguration`](crate::configuration::SearchConfiguration) states its
/// `Duration` ceiling as `rift:range` on the key: schema validation alone cannot compare
/// `"1s"` against a ceiling, so the server enforces the bound at load and the schema carries
/// it for readers.
pub fn declare_search_ranges(schema: &mut Schema) {
    use crate::configuration::{
        Duration, SEARCH_BUSY_TIMEOUT_MS_MAX, SEARCH_BUSY_TIMEOUT_MS_MIN, SearchConfiguration,
    };
    annotate_property(
        schema,
        property!(SearchConfiguration, busy_timeout),
        RIFT_RANGE,
        range(
            &Duration::from_millis(SEARCH_BUSY_TIMEOUT_MS_MIN),
            &Duration::from_millis(SEARCH_BUSY_TIMEOUT_MS_MAX),
        ),
    );
}

/// A [`TextSearchConfiguration`](crate::configuration::TextSearchConfiguration) states its
/// `ByteSize` ceiling as `rift:range` on the key: schema validation alone cannot compare
/// `"1mb"` against a ceiling, so the server enforces the bound at load and the schema carries
/// it for readers.
pub fn declare_text_ranges(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, TEXT_CHUNK_BYTES_MAX, TEXT_CHUNK_BYTES_MIN, TextSearchConfiguration,
    };
    annotate_property(
        schema,
        property!(TextSearchConfiguration, max_chunk),
        RIFT_RANGE,
        range(
            &ByteSize::from_bytes(TEXT_CHUNK_BYTES_MIN),
            &ByteSize::from_bytes(TEXT_CHUNK_BYTES_MAX),
        ),
    );
}

/// A [`CommandHook`](crate::configuration::CommandHook) states its `Duration`
/// and `ByteSize` ceilings as `rift:range` on their keys: schema validation
/// alone cannot compare `"120s"` or `"4kb"` against a ceiling, so the server
/// enforces the bound at load and the schema carries it for readers.
pub fn declare_hook_ranges(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, CommandHook, Duration, HOOK_OUTPUT_BYTES_MAX, HOOK_OUTPUT_BYTES_MIN,
        HOOK_TIMEOUT_MS_MAX,
    };
    let ranges = [
        (
            property!(CommandHook, timeout),
            range(
                &Duration::from_millis(1),
                &Duration::from_millis(HOOK_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(CommandHook, output_limit),
            range(
                &ByteSize::from_bytes(HOOK_OUTPUT_BYTES_MIN),
                &ByteSize::from_bytes(HOOK_OUTPUT_BYTES_MAX),
            ),
        ),
    ];
    for (name, accepted) in ranges {
        annotate_property(schema, name, RIFT_RANGE, accepted);
    }
}

/// An [`EngineConfiguration`](crate::configuration::EngineConfiguration)
/// states its `Duration` and `ByteSize` ceilings as `rift:range` on their
/// keys: schema validation alone cannot compare `"30s"` or `"4kb"` against
/// a ceiling, so the server enforces the bound at load and the schema
/// carries it for readers.
pub fn declare_engine_ranges(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, Duration, ENGINE_OUTPUT_BYTES_MAX, ENGINE_OUTPUT_BYTES_MIN,
        ENGINE_REQUEST_TIMEOUT_MS_MAX, ENGINE_REQUEST_TIMEOUT_MS_MIN,
        ENGINE_STARTUP_TIMEOUT_MS_MAX, ENGINE_STARTUP_TIMEOUT_MS_MIN, EngineConfiguration,
    };
    let ranges = [
        (
            property!(EngineConfiguration, startup_timeout),
            range(
                &Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_MIN),
                &Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(EngineConfiguration, request_timeout),
            range(
                &Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_MIN),
                &Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(EngineConfiguration, output_limit),
            range(
                &ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_MIN),
                &ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_MAX),
            ),
        ),
    ];
    for (name, accepted) in ranges {
        annotate_property(schema, name, RIFT_RANGE, accepted);
    }
}

/// An [`ErrorData`](crate::error::ErrorData) carries `limit` only when
/// `code` is `limit_exceeded`; any other code forbids it.
pub fn error_limit_rides_limit_exceeded(schema: &mut Schema) {
    use crate::error::{ErrorCode, ErrorData};
    let code = property!(ErrorData, code);
    let limit = property!(ErrorData, limit);
    append(
        schema,
        Composition::All,
        otherwise(
            merged(vec![
                properties(vec![(code, constant(&ErrorCode::LimitExceeded))]),
                requires(&[code]),
            ]),
            not(requires(&[limit])),
        ),
    );
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

/// A read names at most one alternate tree to serve from - a version-control
/// `rev` or a materialized `projection`, never both - so the answer's origin
/// is always a single tree.
fn forbid_rev_with_projection(schema: &mut Schema, rev: &str, projection: &str) {
    append(schema, Composition::All, not(requires(&[rev, projection])));
}

/// [`GetSymbolParams`](crate::read::GetSymbolParams) reads one tree: `rev`
/// and `projection` never combine.
pub fn forbid_get_symbol_rev_with_projection(schema: &mut Schema) {
    use crate::read::GetSymbolParams;
    forbid_rev_with_projection(
        schema,
        property!(GetSymbolParams, rev),
        property!(GetSymbolParams, projection),
    );
}

/// [`NodesParams`](crate::read::NodesParams) reads one tree: `rev` and
/// `projection` never combine.
pub fn forbid_nodes_rev_with_projection(schema: &mut Schema) {
    use crate::read::NodesParams;
    forbid_rev_with_projection(
        schema,
        property!(NodesParams, rev),
        property!(NodesParams, projection),
    );
}

/// [`SearchParams`] read one tree: `rev` and `projection` never combine.
pub fn forbid_search_rev_with_projection(schema: &mut Schema) {
    forbid_rev_with_projection(
        schema,
        property!(SearchParams, rev),
        property!(SearchParams, projection),
    );
}

/// [`InsertSymbolParams`](crate::change::InsertSymbolParams) addresses exactly one
/// target, and `create_missing` combines only with `file`.
pub fn insert_symbol_addresses_one_target(schema: &mut Schema) {
    use crate::change::InsertSymbolParams;
    let anchor = property!(InsertSymbolParams, anchor);
    let file = property!(InsertSymbolParams, file);
    let create_missing = property!(InsertSymbolParams, create_missing);
    append(
        schema,
        Composition::All,
        one_of(vec![requires(&[anchor]), requires(&[file])]),
    );
    let anchor_and_create_missing = [anchor, create_missing];
    append(
        schema,
        Composition::All,
        not(merged(vec![
            requires(&anchor_and_create_missing),
            properties(vec![(create_missing, constant(&true))]),
        ])),
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
        assert_eq!(
            otherwise(requires(&["code"]), not(requires(&["limit"]))),
            json!({
                "if": { "required": ["code"] },
                "else": { "not": { "required": ["limit"] } }
            })
        );
        assert_eq!(
            merged(vec![
                properties(vec![("code", constant(&"x"))]),
                requires(&["code"])
            ]),
            json!({ "properties": { "code": { "const": "x" } }, "required": ["code"] })
        );
    }

    #[test]
    fn execution_configuration_schema_states_ranges_on_each_bounded_key() {
        let schema =
            serde_json::to_value(schema_for!(crate::configuration::ExecutionConfiguration))
                .expect("schema");
        let cases = [
            ("max_code", json!({ "min": "1b", "max": "32kb" })),
            ("max_timeout", json!({ "min": "1ms", "max": "1d" })),
            ("max_output", json!({ "min": "0b", "max": "16kb" })),
        ];
        for (name, accepted) in cases {
            assert_eq!(
                schema["properties"][name][RIFT_RANGE], accepted,
                "{name} must state its accepted range"
            );
        }
    }

    #[test]
    fn server_configuration_schema_states_range_on_the_queue_timeout() {
        let schema = serde_json::to_value(schema_for!(crate::configuration::ServerConfiguration))
            .expect("schema");
        assert_eq!(
            schema["properties"]["worker_queue_timeout"][RIFT_RANGE],
            json!({ "min": "1ms", "max": "1h" }),
            "worker_queue_timeout must state its accepted range"
        );
    }

    #[test]
    fn search_configuration_schema_states_range_on_busy_timeout() {
        let schema = serde_json::to_value(schema_for!(crate::configuration::SearchConfiguration))
            .expect("schema");
        assert_eq!(
            schema["properties"]["busy_timeout"][RIFT_RANGE],
            json!({ "min": "100ms", "max": "30s" }),
            "busy_timeout must state its accepted range"
        );
    }

    #[test]
    fn text_search_configuration_schema_states_range_on_max_chunk() {
        let schema =
            serde_json::to_value(schema_for!(crate::configuration::TextSearchConfiguration))
                .expect("schema");
        assert_eq!(
            schema["properties"]["max_chunk"][RIFT_RANGE],
            json!({ "min": "1kb", "max": "16mb" }),
            "max_chunk must state its accepted range"
        );
    }

    #[test]
    fn command_hook_schema_states_ranges_on_each_bounded_key() {
        let schema =
            serde_json::to_value(schema_for!(crate::configuration::CommandHook)).expect("schema");
        let cases = [
            ("timeout", json!({ "min": "1ms", "max": "1h" })),
            ("output_limit", json!({ "min": "256b", "max": "4kb" })),
        ];
        for (name, accepted) in cases {
            assert_eq!(
                schema["properties"][name][RIFT_RANGE], accepted,
                "{name} must state its accepted range"
            );
        }
    }

    #[test]
    fn engine_configuration_schema_states_ranges_on_each_bounded_key() {
        let schema = serde_json::to_value(schema_for!(crate::configuration::EngineConfiguration))
            .expect("schema");
        let cases = [
            ("startup_timeout", json!({ "min": "1s", "max": "10m" })),
            ("request_timeout", json!({ "min": "1s", "max": "10m" })),
            ("output_limit", json!({ "min": "1kb", "max": "8mb" })),
        ];
        for (name, accepted) in cases {
            assert_eq!(
                schema["properties"][name][RIFT_RANGE], accepted,
                "{name} must state its accepted range"
            );
        }
    }

    #[test]
    fn insert_symbol_addresses_one_target_states_exclusive_composition() {
        let mut schema = schema_from(json!({}));
        insert_symbol_addresses_one_target(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({
                "allOf": [
                    {
                        "oneOf": [
                            { "required": ["anchor"] },
                            { "required": ["file"] }
                        ]
                    },
                    {
                        "not": {
                            "required": ["anchor", "create_missing"],
                            "properties": { "create_missing": { "const": true } }
                        }
                    }
                ]
            })
        );
    }

    /// The `property!` macro proves a field exists on the struct; this test
    /// proves serde serves it under the same name, closing the rename gap.
    #[test]
    fn rule_properties_exist_in_model_schemas() {
        let cases: [(&str, Value, &[&str]); 9] = [
            (
                "EngineConfiguration",
                serde_json::to_value(schema_for!(crate::configuration::EngineConfiguration))
                    .expect("schema"),
                &["startup_timeout", "request_timeout", "output_limit"],
            ),
            (
                "GetSymbolParams",
                serde_json::to_value(schema_for!(crate::read::GetSymbolParams)).expect("schema"),
                &["rev", "projection"],
            ),
            (
                "NodesParams",
                serde_json::to_value(schema_for!(crate::read::NodesParams)).expect("schema"),
                &["rev", "projection"],
            ),
            (
                "ExecutionConfiguration",
                serde_json::to_value(schema_for!(crate::configuration::ExecutionConfiguration))
                    .expect("schema"),
                &["max_code", "max_timeout", "max_output"],
            ),
            (
                "InsertSymbolParams",
                serde_json::to_value(schema_for!(crate::change::InsertSymbolParams))
                    .expect("schema"),
                &["anchor", "file", "position", "body", "create_missing"],
            ),
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
                &[
                    "query",
                    "filter",
                    "traversal",
                    "target",
                    "scope",
                    "paths",
                    "rev",
                    "projection",
                ],
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

    /// Every read-params schema forbids naming `rev` and `projection`
    /// together, in the exact clause shape the transform builds.
    #[test]
    fn read_params_schemas_forbid_rev_with_projection() {
        let cases = [
            (
                "GetSymbolParams",
                serde_json::to_value(schema_for!(crate::read::GetSymbolParams)).expect("schema"),
            ),
            (
                "NodesParams",
                serde_json::to_value(schema_for!(crate::read::NodesParams)).expect("schema"),
            ),
            (
                "SearchParams",
                serde_json::to_value(schema_for!(SearchParams)).expect("schema"),
            ),
        ];
        let forbidden = json!({ "not": { "required": ["rev", "projection"] } });
        for (model, schema) in cases {
            let clauses = schema["allOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{model} schema must carry allOf clauses"));
            assert!(
                clauses.contains(&forbidden),
                "{model} schema must forbid rev with projection: {clauses:?}"
            );
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
                if let Some(accepted) = tagged["enum"].as_array() {
                    values.extend(accepted.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
        values
    }

    /// The generator has emitted tags both as a `const` and as a one-value
    /// `enum` across schemars versions; the collector must read both forms.
    #[test]
    fn tag_values_read_const_and_enum_representations() {
        let schema = json!({
            "oneOf": [
                { "properties": { "kind": { "const": "text" } } },
                { "properties": { "kind": { "enum": ["symlink"] } } },
            ]
        });
        assert_eq!(tag_values(&schema, "kind"), vec!["text", "symlink"]);
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
