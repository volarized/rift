//! Schema rules the derive attributes cannot express, attached to models
//! with `#[schemars(transform = schema::...)]`.
//!
//! Every rule is built from the vocabulary in this module: JSON Schema
//! keywords are spelled once in the private `keyword` module, model property
//! names are proven against the model structs by the `property!` macro, and
//! wire values come from serializing the model enums themselves.

use crate::read::SearchHit;
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
    pub(super) const MAX_PROPERTIES: &str = "maxProperties";
    pub(super) const MIN_LENGTH: &str = "minLength";
    pub(super) const MAX_LENGTH: &str = "maxLength";
    pub(super) const PATTERN: &str = "pattern";
    pub(super) const PROPERTY_NAMES: &str = "propertyNames";
    pub(super) const TYPE: &str = "type";
    pub(super) const DEFAULT: &str = "default";
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

// The form a `[languages.<identity>]` table key takes: a language name, or a name and a
// dialect joined by `:`. Owned by `Language` (`crate::read::LANGUAGE_IDENTITY_PATTERN`),
// since it is the same grammar `Language`'s own wire form advertises; acceptance decodes
// the key through `Language::from_identity_segment` and refuses anything else, so the
// schema states the same form for editors reading `rift.toml` before the server does.
use crate::read::LANGUAGE_IDENTITY_PATTERN;

/// The form an `[lsp.<name>]` table key takes: one language word, with no
/// dialect. Acceptance refuses a name carrying `:`, because a process name
/// is not a language identity.
const LSP_NAME_PATTERN: &str = r"^[a-z][a-z0-9._-]*$";

/// The charset a hook `id` takes: ASCII alphanumerics, `.`, `_`, and `-`.
const HOOK_ID_PATTERN: &str = r"^[A-Za-z0-9._-]+$";

/// The Rift extension keyword stating an accepted range schema validation
/// cannot compare itself: the bounds of a string-spelled `ByteSize` or
/// `Duration` key.
const RIFT_RANGE: &str = "rift:range";

/// The serde tag property of [`SearchHitTarget`](crate::read::SearchHitTarget),
/// pinned by [`tests::tagged_union_tags_exist_in_generated_schemas`].
const SEARCH_HIT_TARGET_TAG: &str = "target";
/// Its node and file tag values, pinned by the same test.
const SEARCH_HIT_NODE: &str = "node";
const SEARCH_HIT_FILE: &str = "file";

/// Appends `clause` to the `composition` array of `schema`, creating the
/// array on first use so several rules can target the same keyword.
fn append(schema: &mut Schema, clause: Value) {
    let clauses = schema
        .ensure_object()
        .entry(keyword::ALL_OF)
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

/// Adds `annotation` under `key` on one named property of the object schema
/// `owner` carries under [`keyword::PROPERTIES`].
fn annotate_property_in(owner: &mut Map<String, Value>, name: &str, key: &str, annotation: Value) {
    let property = owner
        .get_mut(keyword::PROPERTIES)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut);
    if let Some(property) = property {
        property.insert(key.to_owned(), annotation);
    }
}

/// Adds `annotation` under `key` on one named property of `schema`, for
/// extension keywords that ride a property's own clause.
fn annotate_property(schema: &mut Schema, name: &str, key: &str, annotation: Value) {
    annotate_property_in(schema.ensure_object(), name, key, annotation);
}

/// States `default: []` on each named array property: schemars omits `default` when a
/// field's `skip_serializing_if` predicate matches its own `#[serde(default)]` value, the
/// case for every empty-collection field this rule targets (proven by
/// [`tests::schemars_omits_default_when_it_matches_skip_serializing_if`]).
fn declare_empty_array_defaults(schema: &mut Schema, names: &[&str]) {
    for name in names {
        annotate_property(schema, name, keyword::DEFAULT, json!([]));
    }
}

/// States `default: {}` on each named map property, the [`Extensions`](crate::read::Extensions)
/// form of [`declare_empty_array_defaults`].
fn declare_empty_object_defaults(schema: &mut Schema, names: &[&str]) {
    for name in names {
        annotate_property(schema, name, keyword::DEFAULT, json!({}));
    }
}

/// The object schema for one arm of a tagged union, selected by its constant `tag` value.
/// [`ChangeResult`](crate::change::ChangeResult)'s struct variants generate as inline
/// `oneOf` object schemas rather than `$defs` entries, so a variant-scoped default has
/// nowhere else to attach.
fn tagged_union_arm<'schema>(
    schema: &'schema mut Schema,
    tag: &str,
    value: &str,
) -> Option<&'schema mut Map<String, Value>> {
    schema
        .ensure_object()
        .get_mut(keyword::ONE_OF)
        .and_then(Value::as_array_mut)
        .and_then(|arms| {
            arms.iter_mut()
                .find(|arm| arm[keyword::PROPERTIES][tag][keyword::CONST] == json!(value))
        })
        .and_then(Value::as_object_mut)
}

/// A workspace configuration states entry caps on its language and LSP maps.
pub fn declare_workspace_contract(schema: &mut Schema) {
    use crate::configuration::{LANGUAGES_MAX, LSP_CONFIGURATIONS_MAX, WorkspaceConfiguration};
    for (name, accepted) in [
        (
            property!(WorkspaceConfiguration, languages),
            LANGUAGES_MAX as u64,
        ),
        (
            property!(WorkspaceConfiguration, lsp),
            LSP_CONFIGURATIONS_MAX as u64,
        ),
    ] {
        annotate_property(schema, name, keyword::MAX_PROPERTIES, json!(accepted));
    }
    for (name, pattern) in [
        (
            property!(WorkspaceConfiguration, languages),
            LANGUAGE_IDENTITY_PATTERN,
        ),
        (property!(WorkspaceConfiguration, lsp), LSP_NAME_PATTERN),
    ] {
        annotate_property(
            schema,
            name,
            keyword::PROPERTY_NAMES,
            json!({ keyword::PATTERN: pattern }),
        );
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
        SERVER_QUEUE_TIMEOUT_MS_MAX, SERVER_READINESS_TIMEOUT_MS_MAX,
        SERVER_READINESS_TIMEOUT_MS_MIN, ServerConfiguration,
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
    annotate_property(
        schema,
        property!(ServerConfiguration, readiness_timeout),
        RIFT_RANGE,
        range(
            &Duration::from_millis(SERVER_READINESS_TIMEOUT_MS_MIN),
            &Duration::from_millis(SERVER_READINESS_TIMEOUT_MS_MAX),
        ),
    );
    append(
        schema,
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

/// A [`SemanticSearchConfiguration`](crate::configuration::SemanticSearchConfiguration)
/// states its `Duration` bounds as `rift:range` on the key: schema validation alone cannot
/// compare `"5m"` against a ceiling, so the server enforces the bounds at load and the
/// schema carries them for readers.
pub fn declare_semantic_ranges(schema: &mut Schema) {
    use crate::configuration::{
        Duration, SEMANTIC_DOWNLOAD_TIMEOUT_MS_MAX, SEMANTIC_DOWNLOAD_TIMEOUT_MS_MIN,
        SemanticSearchConfiguration,
    };
    annotate_property(
        schema,
        property!(SemanticSearchConfiguration, download_timeout),
        RIFT_RANGE,
        range(
            &Duration::from_millis(SEMANTIC_DOWNLOAD_TIMEOUT_MS_MIN),
            &Duration::from_millis(SEMANTIC_DOWNLOAD_TIMEOUT_MS_MAX),
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

/// Declares [`CommandHook`](crate::configuration::CommandHook) schema rules.
///
/// Duration and byte-size ceilings use `rift:range`; server enforces them
/// during configuration loading. Transform hooks cannot carry guarantees.
pub fn declare_hook_contract(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, CommandHook, Duration, HOOK_ENVIRONMENT_ENTRIES_MAX, HOOK_OUTPUT_BYTES_MAX,
        HOOK_OUTPUT_BYTES_MIN, HOOK_TIMEOUT_MS_MAX, HookWrites,
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
    annotate_property(
        schema,
        property!(CommandHook, environment),
        keyword::MAX_PROPERTIES,
        json!(HOOK_ENVIRONMENT_ENTRIES_MAX),
    );
    annotate_property(
        schema,
        property!(CommandHook, id),
        keyword::PATTERN,
        json!(HOOK_ID_PATTERN),
    );
    for writes in [HookWrites::ChangedPaths, HookWrites::Workspace] {
        append(
            schema,
            when(
                properties(vec![(property!(CommandHook, writes), constant(&writes))]),
                properties(vec![(property!(CommandHook, guarantees), max_items(0))]),
            ),
        );
    }
}

/// An [`LspConfiguration`](crate::configuration::LspConfiguration)
/// states its `Duration` and `ByteSize` ceilings as `rift:range` on their
/// keys: schema validation alone cannot compare `"30s"` or `"4kb"` against
/// a ceiling, so the server enforces the bound at load and the schema
/// carries it for readers.
pub fn declare_lsp_ranges(schema: &mut Schema) {
    use crate::configuration::{
        ByteSize, Duration, LSP_ENVIRONMENT_ENTRIES_MAX, LSP_OUTPUT_BYTES_MAX,
        LSP_OUTPUT_BYTES_MIN, LSP_REQUEST_TIMEOUT_MS_MAX, LSP_REQUEST_TIMEOUT_MS_MIN,
        LSP_STARTUP_TIMEOUT_MS_MAX, LSP_STARTUP_TIMEOUT_MS_MIN, LspConfiguration,
    };
    let ranges = [
        (
            property!(LspConfiguration, startup_timeout),
            range(
                &Duration::from_millis(LSP_STARTUP_TIMEOUT_MS_MIN),
                &Duration::from_millis(LSP_STARTUP_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(LspConfiguration, request_timeout),
            range(
                &Duration::from_millis(LSP_REQUEST_TIMEOUT_MS_MIN),
                &Duration::from_millis(LSP_REQUEST_TIMEOUT_MS_MAX),
            ),
        ),
        (
            property!(LspConfiguration, output_limit),
            range(
                &ByteSize::from_bytes(LSP_OUTPUT_BYTES_MIN),
                &ByteSize::from_bytes(LSP_OUTPUT_BYTES_MAX),
            ),
        ),
    ];
    for (name, accepted) in ranges {
        annotate_property(schema, name, RIFT_RANGE, accepted);
    }
    annotate_property(
        schema,
        property!(LspConfiguration, environment),
        keyword::MAX_PROPERTIES,
        json!(LSP_ENVIRONMENT_ENTRIES_MAX),
    );
    annotate_property(
        schema,
        property!(LspConfiguration, initialization_options),
        keyword::TYPE,
        json!("object"),
    );
}

/// An [`LspConfiguration`](crate::configuration::LspConfiguration) selects
/// exactly one engine: a spawned `command`, or an `embedded` engine served
/// in process. Acceptance enforces the same rule, together with the
/// embedded exclusions the field docs state.
pub fn lsp_selects_one_engine(schema: &mut Schema) {
    use crate::configuration::LspConfiguration;
    let command = property!(LspConfiguration, command);
    let embedded = property!(LspConfiguration, embedded);
    append(
        schema,
        one_of(vec![requires(&[command]), requires(&[embedded])]),
    );
}

/// A [`RetryPolicy`](crate::retry::RetryPolicy) states its `Duration`
/// bounds as `rift:range` on their keys: schema validation alone cannot
/// compare `"250ms"` against a ceiling, so the server enforces the bounds
/// at load and the schema carries them for readers.
pub fn declare_retry_ranges(schema: &mut Schema) {
    use crate::configuration::Duration;
    use crate::retry::{
        RETRY_DELAY_LIMIT_MS_MAX, RETRY_DELAY_LIMIT_MS_MIN, RETRY_DELAY_MS_MAX, RETRY_DELAY_MS_MIN,
        RetryPolicy,
    };
    let ranges = [
        (
            property!(RetryPolicy, delay),
            range(
                &Duration::from_millis(RETRY_DELAY_MS_MIN),
                &Duration::from_millis(RETRY_DELAY_MS_MAX),
            ),
        ),
        (
            property!(RetryPolicy, delay_limit),
            range(
                &Duration::from_millis(RETRY_DELAY_LIMIT_MS_MIN),
                &Duration::from_millis(RETRY_DELAY_LIMIT_MS_MAX),
            ),
        ),
    ];
    for (name, accepted) in ranges {
        annotate_property(schema, name, RIFT_RANGE, accepted);
    }
}

/// A [`RestartPolicy`](crate::retry::RestartPolicy) states its `Duration`
/// bounds as `rift:range` on the key: schema validation alone cannot
/// compare `"5m"` against a ceiling, so the server enforces the bounds at
/// load and the schema carries them for readers.
pub fn declare_restart_ranges(schema: &mut Schema) {
    use crate::configuration::Duration;
    use crate::retry::{RESTART_WINDOW_MS_MAX, RESTART_WINDOW_MS_MIN, RestartPolicy};
    annotate_property(
        schema,
        property!(RestartPolicy, window),
        RIFT_RANGE,
        range(
            &Duration::from_millis(RESTART_WINDOW_MS_MIN),
            &Duration::from_millis(RESTART_WINDOW_MS_MAX),
        ),
    );
}

/// An [`ErrorData`](crate::error::ErrorData) carries `limit` only when
/// `code` is `limit_exceeded`; any other code forbids it.
pub fn error_limit_rides_limit_exceeded(schema: &mut Schema) {
    use crate::error::{ErrorCode, ErrorData};
    let code = property!(ErrorData, code);
    let limit = property!(ErrorData, limit);
    append(
        schema,
        otherwise(
            merged(vec![
                properties(vec![(code, constant(&ErrorCode::LimitExceeded))]),
                requires(&[code]),
            ]),
            not(requires(&[limit])),
        ),
    );
}

/// A [`SearchHit`] carries `range` and `line` together or not at all, and node
/// and file hits always carry both.
pub fn pair_range_with_line(schema: &mut Schema) {
    let range_and_line = [property!(SearchHit, range), property!(SearchHit, line)];
    append(
        schema,
        one_of(vec![
            requires(&range_and_line),
            not(any_of(vec![
                requires(&range_and_line[..1]),
                requires(&range_and_line[1..]),
            ])),
        ]),
    );
    append(
        schema,
        when(
            properties(vec![(
                property!(SearchHit, hit),
                properties(vec![(
                    SEARCH_HIT_TARGET_TAG,
                    json!({ keyword::ENUM: [SEARCH_HIT_NODE, SEARCH_HIT_FILE] }),
                )]),
            )]),
            requires(&range_and_line),
        ),
    );
}

/// A [`GetSymbolHit`](crate::read::GetSymbolHit) addresses its declaration through
/// exactly one of `path` (a project declaration) or `unit` (a dependency or standard
/// library declaration).
pub fn get_symbol_hit_addresses_one_location(schema: &mut Schema) {
    use crate::read::GetSymbolHit;
    let path = property!(GetSymbolHit, path);
    let unit = property!(GetSymbolHit, unit);
    append(schema, one_of(vec![requires(&[path]), requires(&[unit])]));
}

/// [`InsertSymbolParams`](crate::change::InsertSymbolParams) addresses exactly one
/// target, and `create_missing` combines only with `file`.
pub fn insert_symbol_addresses_one_target(schema: &mut Schema) {
    use crate::change::InsertSymbolParams;
    let anchor = property!(InsertSymbolParams, anchor);
    let file = property!(InsertSymbolParams, file);
    let create_missing = property!(InsertSymbolParams, create_missing);
    append(schema, one_of(vec![requires(&[anchor]), requires(&[file])]));
    let anchor_and_create_missing = [anchor, create_missing];
    append(
        schema,
        not(merged(vec![
            requires(&anchor_and_create_missing),
            properties(vec![(create_missing, constant(&true))]),
        ])),
    );
}

/// A [`PatchParams`](crate::change::PatchParams) states
/// [`PATCH_BYTES_MAX`](crate::change::PATCH_BYTES_MAX) as `patch`'s inline-string
/// length: the shared [`BodySource`](crate::change::BodySource) `$defs` entry carries
/// no bound of its own, so every embedding stamps the limit its own runtime check
/// enforces, as a sibling of the `$ref` the field generates.
pub fn declare_patch_body_length(schema: &mut Schema) {
    use crate::change::{PATCH_BYTES_MAX, PatchParams};
    let patch = property!(PatchParams, patch);
    annotate_property(schema, patch, keyword::MIN_LENGTH, json!(1));
    annotate_property(schema, patch, keyword::MAX_LENGTH, json!(PATCH_BYTES_MAX));
}

/// A [`ReplaceSymbolParams`](crate::change::ReplaceSymbolParams) states
/// [`BODY_BYTES_MAX`](crate::change::BODY_BYTES_MAX) as `body`'s inline-string
/// `maxLength`, the same rule [`declare_patch_body_length`] states for `patch`.
pub fn declare_replace_symbol_body_length(schema: &mut Schema) {
    use crate::change::{BODY_BYTES_MAX, ReplaceSymbolParams};
    annotate_property(
        schema,
        property!(ReplaceSymbolParams, body),
        keyword::MAX_LENGTH,
        json!(BODY_BYTES_MAX),
    );
}

/// An [`InsertSymbolParams`](crate::change::InsertSymbolParams) states
/// [`BODY_BYTES_MAX`](crate::change::BODY_BYTES_MAX) as `body`'s inline-string
/// `maxLength`, the same rule [`declare_patch_body_length`] states for `patch`.
pub fn declare_insert_symbol_body_length(schema: &mut Schema) {
    use crate::change::{BODY_BYTES_MAX, InsertSymbolParams};
    annotate_property(
        schema,
        property!(InsertSymbolParams, body),
        keyword::MAX_LENGTH,
        json!(BODY_BYTES_MAX),
    );
}

/// A [`ReplaceNodeParams`](crate::change::ReplaceNodeParams) states
/// [`BODY_BYTES_MAX`](crate::change::BODY_BYTES_MAX) as `body`'s inline-string
/// `maxLength`, the same rule [`declare_patch_body_length`] states for `patch`.
pub fn declare_replace_node_body_length(schema: &mut Schema) {
    use crate::change::{BODY_BYTES_MAX, ReplaceNodeParams};
    annotate_property(
        schema,
        property!(ReplaceNodeParams, body),
        keyword::MAX_LENGTH,
        json!(BODY_BYTES_MAX),
    );
}

/// An [`InsertNodeParams`](crate::change::InsertNodeParams) states
/// [`BODY_BYTES_MAX`](crate::change::BODY_BYTES_MAX) as `body`'s inline-string
/// `maxLength`, the same rule [`declare_patch_body_length`] states for `patch`.
pub fn declare_insert_node_body_length(schema: &mut Schema) {
    use crate::change::{BODY_BYTES_MAX, InsertNodeParams};
    annotate_property(
        schema,
        property!(InsertNodeParams, body),
        keyword::MAX_LENGTH,
        json!(BODY_BYTES_MAX),
    );
}

/// A [`Node`](crate::read::Node) states `default: []` on `facets` and `regions`, and
/// `default: {}` on `extensions`.
pub fn declare_node_empty_defaults(schema: &mut Schema) {
    use crate::read::Node;
    declare_empty_array_defaults(schema, &[property!(Node, facets), property!(Node, regions)]);
    declare_empty_object_defaults(schema, &[property!(Node, extensions)]);
}

/// A [`Symbol`](crate::read::Symbol) states `default: []` on its collection fields,
/// `default: {}` on `extensions`, and `default: false` on `document_local`.
pub fn declare_symbol_empty_defaults(schema: &mut Schema) {
    use crate::read::Symbol;
    declare_empty_array_defaults(
        schema,
        &[
            property!(Symbol, facets),
            property!(Symbol, modifiers),
            property!(Symbol, types),
            property!(Symbol, signatures),
            property!(Symbol, documentation),
        ],
    );
    declare_empty_object_defaults(schema, &[property!(Symbol, extensions)]);
    annotate_property(
        schema,
        property!(Symbol, document_local),
        keyword::DEFAULT,
        json!(false),
    );
    // `origin`'s own `#[serde(default = "default_symbol_origin", skip_serializing_if =
    // "SymbolOrigin::is_common_default")]` hits the same schemars quirk
    // `declare_empty_array_defaults` works around: the auto-embedded default is
    // suppressed because it equals what the skip predicate matches, so the common-case
    // value is stated here explicitly instead.
    annotate_property(
        schema,
        property!(Symbol, origin),
        keyword::DEFAULT,
        json!({ "location": "project", "source_kind": "authored" }),
    );
}

/// A [`Signature`](crate::read::Signature) states `default: []` on its collection fields
/// and `default: {}` on `extensions`.
pub fn declare_signature_empty_defaults(schema: &mut Schema) {
    use crate::read::Signature;
    declare_empty_array_defaults(
        schema,
        &[
            property!(Signature, links),
            property!(Signature, parameters),
            property!(Signature, returns),
            property!(Signature, type_parameters),
            property!(Signature, throws),
            property!(Signature, effects),
        ],
    );
    declare_empty_object_defaults(schema, &[property!(Signature, extensions)]);
}

/// A [`Parameter`](crate::read::Parameter) states `default: []` on `types` and
/// `default: {}` on `extensions`.
pub fn declare_parameter_empty_defaults(schema: &mut Schema) {
    use crate::read::Parameter;
    declare_empty_array_defaults(schema, &[property!(Parameter, types)]);
    declare_empty_object_defaults(schema, &[property!(Parameter, extensions)]);
}

/// A [`Relationship`](crate::read::Relationship) states `default: []` on `evidence` and
/// `default: {}` on `extensions`. `facets` carries its own `minItems: 1` and is never
/// optional, so it states no default.
pub fn declare_relationship_empty_defaults(schema: &mut Schema) {
    use crate::read::Relationship;
    declare_empty_array_defaults(schema, &[property!(Relationship, evidence)]);
    declare_empty_object_defaults(schema, &[property!(Relationship, extensions)]);
}

/// A [`TypeExpression`](crate::read::TypeExpression) states `default: {}` on `extensions`.
pub fn declare_type_expression_empty_defaults(schema: &mut Schema) {
    use crate::read::TypeExpression;
    declare_empty_object_defaults(schema, &[property!(TypeExpression, extensions)]);
}

/// A [`GetSymbolResult`](crate::read::GetSymbolResult) states `default: []` on `warnings`.
pub fn declare_get_symbol_result_empty_defaults(schema: &mut Schema) {
    use crate::read::GetSymbolResult;
    declare_empty_array_defaults(schema, &[property!(GetSymbolResult, warnings)]);
}

/// A [`NodesResult`](crate::read::NodesResult) states `default: []` on `warnings`. `nodes`
/// and `source` are the tool's own answer and stay required.
pub fn declare_nodes_result_empty_defaults(schema: &mut Schema) {
    use crate::read::NodesResult;
    declare_empty_array_defaults(schema, &[property!(NodesResult, warnings)]);
}

/// A [`SearchHit`] states `default: []` on `matched_by`.
pub fn declare_search_hit_empty_defaults(schema: &mut Schema) {
    use crate::read::SearchHit;
    declare_empty_array_defaults(schema, &[property!(SearchHit, matched_by)]);
}

/// A [`SearchResult`](crate::search::SearchResult) states `default: []` on `warnings`.
pub fn declare_search_result_empty_defaults(schema: &mut Schema) {
    use crate::search::SearchResult;
    declare_empty_array_defaults(schema, &[property!(SearchResult, warnings)]);
}

/// [`SearchHitTarget`](crate::read::SearchHitTarget)'s `file` arm states `default: []` on
/// `languages`.
pub fn declare_search_hit_target_file_empty_defaults(schema: &mut Schema) {
    const SEARCH_HIT_TARGET_LANGUAGES: &str = "languages";
    if let Some(arm) = tagged_union_arm(schema, SEARCH_HIT_TARGET_TAG, SEARCH_HIT_FILE) {
        annotate_property_in(
            arm,
            SEARCH_HIT_TARGET_LANGUAGES,
            keyword::DEFAULT,
            json!([]),
        );
    }
}

/// A [`ChangeSummary`](crate::change::ChangeSummary) states `default: []` on `diagnostics`
/// and `guarantees`. `files` carries its own `minItems: 1` and is never optional, so it
/// states no default.
pub fn declare_change_summary_empty_defaults(schema: &mut Schema) {
    use crate::change::ChangeSummary;
    declare_empty_array_defaults(
        schema,
        &[
            property!(ChangeSummary, diagnostics),
            property!(ChangeSummary, guarantees),
        ],
    );
}

/// The `refused` arm of [`ChangeResult`](crate::change::ChangeResult) states `default: []`
/// on `diagnostics`. `preconditions` carries no default: a refusal's evidence is the
/// answer, so it is never optional (proven by a same-named test in this module).
pub fn declare_change_result_empty_defaults(schema: &mut Schema) {
    const CHANGE_RESULT_TAG: &str = "status";
    const CHANGE_RESULT_REFUSED: &str = "refused";
    const REFUSED_DIAGNOSTICS: &str = "diagnostics";
    if let Some(arm) = tagged_union_arm(schema, CHANGE_RESULT_TAG, CHANGE_RESULT_REFUSED) {
        annotate_property_in(arm, REFUSED_DIAGNOSTICS, keyword::DEFAULT, json!([]));
    }
}

/// A [`Diagnostic`](crate::diagnostic::Diagnostic) states `default: []` on `related` and
/// `tags`, and `default: {}` on `extensions`.
pub fn declare_diagnostic_empty_defaults(schema: &mut Schema) {
    use crate::diagnostic::Diagnostic;
    declare_empty_array_defaults(
        schema,
        &[property!(Diagnostic, related), property!(Diagnostic, tags)],
    );
    declare_empty_object_defaults(schema, &[property!(Diagnostic, extensions)]);
}

/// An [`ErrorData`](crate::error::ErrorData) states `default: []` on `diagnostics` and
/// `causes`.
pub fn declare_error_data_empty_defaults(schema: &mut Schema) {
    use crate::error::ErrorData;
    declare_empty_array_defaults(
        schema,
        &[
            property!(ErrorData, diagnostics),
            property!(ErrorData, causes),
        ],
    );
}

/// A [`WorkspaceLanguageSummary`](crate::workspace::WorkspaceLanguageSummary) states
/// `default: []` on `include` and `exclude`.
pub fn declare_workspace_language_summary_empty_defaults(schema: &mut Schema) {
    use crate::workspace::WorkspaceLanguageSummary;
    declare_empty_array_defaults(
        schema,
        &[
            property!(WorkspaceLanguageSummary, include),
            property!(WorkspaceLanguageSummary, exclude),
        ],
    );
}

/// A [`WorkspaceHookSummary`](crate::workspace::WorkspaceHookSummary) states `default: []`
/// on `include` and `exclude`.
pub fn declare_workspace_hook_summary_empty_defaults(schema: &mut Schema) {
    use crate::workspace::WorkspaceHookSummary;
    declare_empty_array_defaults(
        schema,
        &[
            property!(WorkspaceHookSummary, include),
            property!(WorkspaceHookSummary, exclude),
        ],
    );
}

/// A [`WorkspaceMap`](crate::map::WorkspaceMap) states `default: []` on its collection
/// fields.
pub fn declare_workspace_map_empty_defaults(schema: &mut Schema) {
    use crate::map::WorkspaceMap;
    declare_empty_array_defaults(
        schema,
        &[
            property!(WorkspaceMap, languages),
            property!(WorkspaceMap, modules),
            property!(WorkspaceMap, hubs),
            property!(WorkspaceMap, entry_points),
            property!(WorkspaceMap, docs),
        ],
    );
}

/// A [`MapModule`](crate::map::MapModule) states `default: []` on `children`.
pub fn declare_map_module_empty_defaults(schema: &mut Schema) {
    use crate::map::MapModule;
    declare_empty_array_defaults(schema, &[property!(MapModule, children)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::SearchHitTarget;
    use schemars::schema_for;

    fn schema_from(value: Value) -> Schema {
        Schema::try_from(value).expect("test schema literal must be a valid schema object")
    }

    /// The advertised table-key forms and the acceptance rules they mirror
    /// must agree on every sample: a key the schema admits is a key the
    /// server accepts, and a key the schema refuses is one it refuses.
    #[test]
    fn test_table_key_patterns_match_the_forms_acceptance_enforces() {
        use crate::configuration::WorkspaceConfiguration;
        use crate::read::Language;

        let document =
            serde_json::to_value(schema_for!(WorkspaceConfiguration)).expect("schema document");
        assert_eq!(
            document["properties"]["languages"][keyword::PROPERTY_NAMES][keyword::PATTERN],
            json!(LANGUAGE_IDENTITY_PATTERN)
        );
        assert_eq!(
            document["properties"]["lsp"][keyword::PROPERTY_NAMES][keyword::PATTERN],
            json!(LSP_NAME_PATTERN)
        );
        let validator = jsonschema::validator_for(&document).expect("schema compiles");

        for key in [
            "rust",
            "typescript:tsx",
            "Rust",
            "rust:",
            ":tsx",
            "9rust",
            "rust:tsx:jsx",
        ] {
            let identity_accepted = Language::from_identity_segment(key).is_ok();
            assert_eq!(
                validator.is_valid(&json!({ "languages": { key: {} } })),
                identity_accepted,
                "the languages key pattern must admit exactly what acceptance decodes: {key}"
            );
            let command = json!({ "lsp": { key: { "command": "tool" } } });
            assert_eq!(
                validator.is_valid(&command),
                identity_accepted && !key.contains(':'),
                "the lsp key pattern must admit one language word and no dialect: {key}"
            );
        }
    }

    /// A hook `id` and an LSP `initialization_options` object are refused by
    /// acceptance, so the schema states the same two rules.
    #[test]
    fn test_hook_id_and_initialization_options_state_their_accepted_forms() {
        use crate::configuration::{CommandHook, LspConfiguration};

        let hook = serde_json::to_value(schema_for!(CommandHook)).expect("hook schema");
        assert_eq!(
            hook["properties"]["id"][keyword::PATTERN],
            json!(HOOK_ID_PATTERN)
        );
        let lsp = serde_json::to_value(schema_for!(LspConfiguration)).expect("lsp schema");
        assert_eq!(
            lsp["properties"]["initialization_options"][keyword::TYPE],
            json!("object")
        );
    }

    #[test]
    fn test_append_creates_then_extends_one_keyword() {
        let mut schema = schema_from(json!({}));
        append(&mut schema, json!({ "required": ["a"] }));
        append(&mut schema, json!({ "required": ["b"] }));
        assert_eq!(
            schema.as_value(),
            &json!({
                "allOf": [{ "required": ["a"] }, { "required": ["b"] }]
            }),
            "every rule extends one keyword, and each clause must hold"
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
    fn server_configuration_schema_states_range_on_the_readiness_timeout() {
        let schema = serde_json::to_value(schema_for!(crate::configuration::ServerConfiguration))
            .expect("schema");
        assert_eq!(
            schema["properties"]["readiness_timeout"][RIFT_RANGE],
            json!({ "min": "1s", "max": "1h" }),
            "readiness_timeout must state its accepted range"
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
    fn lsp_configuration_schema_states_ranges_on_each_bounded_key() {
        let schema = serde_json::to_value(schema_for!(crate::configuration::LspConfiguration))
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

    #[test]
    fn get_symbol_hit_addresses_one_location_states_exclusive_composition() {
        let mut schema = schema_from(json!({}));
        get_symbol_hit_addresses_one_location(&mut schema);
        assert_eq!(
            schema.as_value(),
            &json!({
                "allOf": [
                    {
                        "oneOf": [
                            { "required": ["path"] },
                            { "required": ["unit"] }
                        ]
                    }
                ]
            })
        );
    }

    /// The `property!` macro proves a field exists on the struct; this test
    /// proves serde serves it under the same name, closing the rename gap.
    #[test]
    fn rule_properties_exist_in_model_schemas() {
        let cases: [(&str, Value, &[&str]); 7] = [
            (
                "CommandHook",
                serde_json::to_value(schema_for!(crate::configuration::CommandHook))
                    .expect("schema"),
                &["timeout", "output_limit", "writes", "guarantees"],
            ),
            (
                "LspConfiguration",
                serde_json::to_value(schema_for!(crate::configuration::LspConfiguration))
                    .expect("schema"),
                &["startup_timeout", "request_timeout", "output_limit"],
            ),
            (
                "GetSymbolParams",
                serde_json::to_value(schema_for!(crate::read::GetSymbolParams)).expect("schema"),
                &["rev"],
            ),
            (
                "NodesParams",
                serde_json::to_value(schema_for!(crate::read::NodesParams)).expect("schema"),
                &["rev"],
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
                "SearchHit",
                serde_json::to_value(schema_for!(SearchHit)).expect("schema"),
                &["hit", "range", "line"],
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

    /// `PatchParams.patch` states the `BodySource` `$defs` `$ref`'s inline-string
    /// length as a sibling of `$ref`, pinned to [`crate::change::PATCH_BYTES_MAX`].
    #[test]
    fn patch_params_schema_states_patch_body_length() {
        use crate::change::PATCH_BYTES_MAX;
        let schema = serde_json::to_value(schema_for!(crate::change::PatchParams)).expect("schema");
        let patch = &schema["properties"]["patch"];
        assert_eq!(patch["minLength"], json!(1));
        assert_eq!(patch["maxLength"], json!(PATCH_BYTES_MAX));
        assert!(
            patch["$ref"].is_string(),
            "patch must keep referencing the shared BodySource $defs entry: {patch}"
        );
    }

    /// `ReplaceSymbolParams.body`, `InsertSymbolParams.body`, `ReplaceNodeParams.body`, and
    /// `InsertNodeParams.body` each state the `BodySource` `$ref`'s inline-string
    /// `maxLength` as a sibling of `$ref`, pinned to [`crate::change::BODY_BYTES_MAX`].
    #[test]
    fn body_carrying_params_schemas_state_body_length() {
        use crate::change::{
            BODY_BYTES_MAX, InsertNodeParams, InsertSymbolParams, ReplaceNodeParams,
        };
        let cases = [
            (
                "ReplaceSymbolParams",
                serde_json::to_value(schema_for!(crate::change::ReplaceSymbolParams))
                    .expect("schema"),
            ),
            (
                "InsertSymbolParams",
                serde_json::to_value(schema_for!(InsertSymbolParams)).expect("schema"),
            ),
            (
                "ReplaceNodeParams",
                serde_json::to_value(schema_for!(ReplaceNodeParams)).expect("schema"),
            ),
            (
                "InsertNodeParams",
                serde_json::to_value(schema_for!(InsertNodeParams)).expect("schema"),
            ),
        ];
        for (model, schema) in cases {
            let body = &schema["properties"]["body"];
            assert_eq!(
                body["maxLength"],
                json!(BODY_BYTES_MAX),
                "{model}.body must state the enforced maxLength"
            );
            assert!(
                body["$ref"].is_string(),
                "{model}.body must keep referencing the shared BodySource $defs entry: {body}"
            );
        }
    }

    /// `BodySource` generates exactly one `$defs` entry, shared by every embedding
    /// rather than duplicated per field.
    #[test]
    fn body_source_is_one_defs_entry_shared_across_embeddings() {
        let schema =
            serde_json::to_value(schema_for!(crate::change::ReplaceSymbolParams)).expect("schema");
        let defs = schema["$defs"].as_object().expect("schema carries $defs");
        assert!(
            defs.contains_key("BodySource"),
            "BodySource must be its own $defs entry: {defs:?}"
        );
    }

    /// The premise every `declare_*_empty_defaults` function is built on: schemars 1.2.2
    /// states no `default` keyword for a field whose own `#[serde(default)]` value matches
    /// its `skip_serializing_if` predicate - the case for every empty collection and every
    /// `false` boolean this rule targets. Quoted probe output (schemars 1.2.2, this test):
    /// `{"properties":{"document_local":{"type":"boolean"},"modifiers":{"items":{"type":"string"},"type":"array"}},"type":"object"}`
    /// - no `required`, no `default`, on either field.
    #[test]
    fn schemars_omits_default_when_it_matches_skip_serializing_if() {
        #[derive(Serialize, schemars::JsonSchema)]
        struct Probe {
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            modifiers: Vec<String>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            document_local: bool,
        }
        let schema = serde_json::to_value(schema_for!(Probe)).expect("probe schema");
        assert_eq!(schema.get(keyword::REQUIRED), None, "{schema:#}");
        assert_eq!(
            schema[keyword::PROPERTIES]["modifiers"].get(keyword::DEFAULT),
            None,
            "{schema:#}"
        );
        assert_eq!(
            schema[keyword::PROPERTIES]["document_local"].get(keyword::DEFAULT),
            None,
            "{schema:#}"
        );
    }

    /// One model's schema, and the fields on it expected to carry a stated default.
    type DefaultCase = (&'static str, Value, Vec<(&'static str, Value)>);

    /// Every field a `declare_*_empty_defaults` transform targets states the `default`
    /// [`schemars_omits_default_when_it_matches_skip_serializing_if`] proved schemars
    /// leaves out on its own, and leaves the model's `required` list.
    fn assert_default_cases(cases: Vec<DefaultCase>) {
        for (model, schema, fields) in cases {
            let required: Vec<String> = schema[keyword::REQUIRED]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect();
            for (name, expected_default) in fields {
                assert_eq!(
                    schema[keyword::PROPERTIES][name][keyword::DEFAULT],
                    expected_default,
                    "{model}.{name} must advertise default: {expected_default}: {schema:#}"
                );
                assert!(
                    !required.contains(&name.to_owned()),
                    "{model}.{name} must leave required: {schema:#}"
                );
            }
        }
    }

    #[test]
    fn read_model_empty_defaults_are_declared() {
        let array = json!([]);
        let object = json!({});
        assert_default_cases(vec![
            (
                "Node",
                serde_json::to_value(schema_for!(crate::read::Node)).expect("schema"),
                vec![
                    ("facets", array.clone()),
                    ("regions", array.clone()),
                    ("extensions", object.clone()),
                ],
            ),
            (
                "Symbol",
                serde_json::to_value(schema_for!(crate::read::Symbol)).expect("schema"),
                vec![
                    ("facets", array.clone()),
                    ("modifiers", array.clone()),
                    ("types", array.clone()),
                    ("signatures", array.clone()),
                    ("documentation", array.clone()),
                    ("extensions", object.clone()),
                    ("document_local", json!(false)),
                    (
                        "origin",
                        json!({ "location": "project", "source_kind": "authored" }),
                    ),
                ],
            ),
            (
                "Signature",
                serde_json::to_value(schema_for!(crate::read::Signature)).expect("schema"),
                vec![
                    ("links", array.clone()),
                    ("parameters", array.clone()),
                    ("returns", array.clone()),
                    ("type_parameters", array.clone()),
                    ("throws", array.clone()),
                    ("effects", array.clone()),
                    ("extensions", object.clone()),
                ],
            ),
            (
                "Parameter",
                serde_json::to_value(schema_for!(crate::read::Parameter)).expect("schema"),
                vec![("types", array.clone()), ("extensions", object.clone())],
            ),
            (
                "Relationship",
                serde_json::to_value(schema_for!(crate::read::Relationship)).expect("schema"),
                vec![("evidence", array.clone()), ("extensions", object.clone())],
            ),
            (
                "TypeExpression",
                serde_json::to_value(schema_for!(crate::read::TypeExpression)).expect("schema"),
                vec![("extensions", object)],
            ),
            (
                "GetSymbolResult",
                serde_json::to_value(schema_for!(crate::read::GetSymbolResult)).expect("schema"),
                vec![("warnings", array.clone())],
            ),
            (
                "NodesResult",
                serde_json::to_value(schema_for!(crate::read::NodesResult)).expect("schema"),
                vec![("warnings", array)],
            ),
        ]);
    }

    #[test]
    fn search_model_empty_defaults_are_declared() {
        let array = json!([]);
        assert_default_cases(vec![
            (
                "SearchHit",
                serde_json::to_value(schema_for!(SearchHit)).expect("schema"),
                vec![("matched_by", array.clone())],
            ),
            (
                "SearchResult",
                serde_json::to_value(schema_for!(crate::search::SearchResult)).expect("schema"),
                vec![("warnings", array)],
            ),
        ]);
    }

    /// `SearchHitTarget`'s `file` arm states `default: []` on `languages` and leaves it out
    /// of that arm's `required` list.
    #[test]
    fn search_hit_target_file_arm_states_languages_default() {
        let schema = serde_json::to_value(schema_for!(SearchHitTarget)).expect("schema");
        let arms = schema[keyword::ONE_OF].as_array().expect("oneOf arms");
        let file = arms
            .iter()
            .find(|arm| {
                arm[keyword::PROPERTIES][SEARCH_HIT_TARGET_TAG][keyword::CONST]
                    == json!(SEARCH_HIT_FILE)
            })
            .expect("a file arm");
        assert_eq!(
            file[keyword::PROPERTIES]["languages"][keyword::DEFAULT],
            json!([]),
            "{file:#}"
        );
        let required: Vec<String> = file[keyword::REQUIRED]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        assert!(!required.contains(&"languages".to_owned()), "{file:#}");
        assert!(required.contains(&"size".to_owned()), "{file:#}");
    }

    #[test]
    fn change_and_diagnostic_model_empty_defaults_are_declared() {
        let array = json!([]);
        let object = json!({});
        assert_default_cases(vec![
            (
                "ChangeSummary",
                serde_json::to_value(schema_for!(crate::change::ChangeSummary)).expect("schema"),
                vec![
                    ("diagnostics", array.clone()),
                    ("guarantees", array.clone()),
                ],
            ),
            (
                "Diagnostic",
                serde_json::to_value(schema_for!(crate::diagnostic::Diagnostic)).expect("schema"),
                vec![
                    ("related", array.clone()),
                    ("tags", array.clone()),
                    ("extensions", object),
                ],
            ),
            (
                "ErrorData",
                serde_json::to_value(schema_for!(crate::error::ErrorData)).expect("schema"),
                vec![("diagnostics", array.clone()), ("causes", array)],
            ),
        ]);
    }

    #[test]
    fn workspace_model_empty_defaults_are_declared() {
        let array = json!([]);
        assert_default_cases(vec![
            (
                "WorkspaceLanguageSummary",
                serde_json::to_value(schema_for!(crate::workspace::WorkspaceLanguageSummary))
                    .expect("schema"),
                vec![("include", array.clone()), ("exclude", array.clone())],
            ),
            (
                "WorkspaceHookSummary",
                serde_json::to_value(schema_for!(crate::workspace::WorkspaceHookSummary))
                    .expect("schema"),
                vec![("include", array.clone()), ("exclude", array)],
            ),
        ]);
    }

    /// `ChangeSummary.files` and `Relationship.facets` both carry `minItems: 1`, so neither
    /// is ever empty: they state no `default` and stay in `required`, the deliberate
    /// exception `declare_change_summary_empty_defaults` and
    /// `declare_relationship_empty_defaults` leave alone.
    #[test]
    fn min_length_one_collections_state_no_default_and_stay_required() {
        let cases = [
            (
                "ChangeSummary",
                serde_json::to_value(schema_for!(crate::change::ChangeSummary)).expect("schema"),
                "files",
            ),
            (
                "Relationship",
                serde_json::to_value(schema_for!(crate::read::Relationship)).expect("schema"),
                "facets",
            ),
        ];
        for (model, schema, name) in cases {
            assert_eq!(
                schema[keyword::PROPERTIES][name].get(keyword::DEFAULT),
                None,
                "{model}.{name} must never advertise a default: it is never empty: {schema:#}"
            );
            let required = schema[keyword::REQUIRED]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(
                required.contains(&json!(name)),
                "{model}.{name} must stay required: {schema:#}"
            );
        }
    }

    /// The `refused` arm of `ChangeResult` states `default: []` on `diagnostics` and
    /// leaves it out of that arm's `required` list; `preconditions` states no default and
    /// stays required, because a refusal's evidence is the answer.
    #[test]
    fn change_result_refused_arm_states_diagnostics_default_and_no_precondition_default() {
        let schema =
            serde_json::to_value(schema_for!(crate::change::ChangeResult)).expect("schema");
        let arms = schema[keyword::ONE_OF].as_array().expect("oneOf arms");
        let refused = arms
            .iter()
            .find(|arm| arm[keyword::PROPERTIES]["status"][keyword::CONST] == json!("refused"))
            .expect("a refused arm");
        assert_eq!(
            refused[keyword::PROPERTIES]["diagnostics"][keyword::DEFAULT],
            json!([]),
            "{refused:#}"
        );
        assert_eq!(
            refused[keyword::PROPERTIES]["preconditions"].get(keyword::DEFAULT),
            None,
            "{refused:#}"
        );
        let required: Vec<String> = refused[keyword::REQUIRED]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        assert!(!required.contains(&"diagnostics".to_owned()), "{refused:#}");
        assert!(
            required.contains(&"preconditions".to_owned()),
            "{refused:#}"
        );
    }
}
