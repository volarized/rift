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

    pub(super) const MAX_PROPERTIES: &str = "maxProperties";

    pub(super) const PATTERN: &str = "pattern";
    pub(super) const PROPERTY_NAMES: &str = "propertyNames";
    pub(super) const TYPE: &str = "type";
    pub(super) const DEFAULT: &str = "default";

    pub(super) const DEFS: &str = "$defs";
    pub(super) const REF: &str = "$ref";
    pub(super) const ITEMS: &str = "items";
    pub(super) const EXAMPLES: &str = "examples";
    pub(super) const NULL: &str = "null";
}

/// References and unions one schema walk follows before it answers with what
/// it has reached. A model may refer to itself, and a caller reaches the walk
/// by sending one value the schema refuses, so the walk is bounded.
const SCHEMA_REFERENCES_MAX: usize = 32;

/// One step of the path into a document the server refused.
///
/// A refusal names the member it stopped at, so the caller reads the shape of
/// that member rather than of the whole request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentStep<'step> {
    /// A member of an object, by the name the wire spells.
    Member(&'step str),
    /// An element of an array.
    Element,
}

/// The schema the workspace configuration declares.
///
/// One spelling for every reader: the document `rift.schema.json` publishes,
/// and the document a refused `rift.toml` is described against.
#[must_use]
pub fn configuration_schema() -> Value {
    schemars::schema_for!(crate::configuration::WorkspaceConfiguration).to_value()
}

/// The path into a refused document the deserializer stopped at.
///
/// The walk ends at the first segment that addresses neither a member nor an
/// element: a segment the schema cannot follow would misalign every segment
/// after it, and a refusal naming a member the schema never had helps nobody.
#[must_use]
pub fn document_steps(path: &serde_path_to_error::Path) -> Vec<DocumentStep<'_>> {
    let mut steps = Vec::new();
    for segment in path {
        match segment {
            serde_path_to_error::Segment::Map { key } => steps.push(DocumentStep::Member(key)),
            serde_path_to_error::Segment::Seq { .. } => steps.push(DocumentStep::Element),
            _ => break,
        }
    }
    steps
}

/// The wire spelling of the value a refusal stops at: members joined by `.`,
/// each element written `[]`. Absent for the whole document.
#[must_use]
pub fn named_member(steps: &[DocumentStep<'_>]) -> Option<String> {
    if steps.is_empty() {
        return None;
    }
    let mut name = String::new();
    for step in steps {
        match step {
            DocumentStep::Member(member) => {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(member);
            }
            DocumentStep::Element => name.push_str("[]"),
        }
    }
    Some(name)
}

/// What a schema declares at one path: the members it accepts there, and an
/// example of the value it takes.
///
/// Both are read from the schema the server serves, so a refusal states names
/// and values the caller can look up in the same document it was written
/// against.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExpectedShape {
    accepted: Vec<String>,
    example: Option<Value>,
    followed: usize,
}

impl ExpectedShape {
    /// What the addressed schema accepts, in wire spelling, sorted: an object's
    /// member names, or a closed set's values. Empty where the schema states
    /// neither.
    #[must_use]
    pub fn accepted(&self) -> &[String] {
        &self.accepted
    }

    /// An example of the value the addressed schema takes. Absent when neither
    /// the addressed schema nor any schema above it carries one.
    #[must_use]
    pub const fn example(&self) -> Option<&Value> {
        self.example.as_ref()
    }

    /// How many steps of the requested path the schema declares. A caller
    /// names the value it refused by this many steps, so a refusal never
    /// addresses a member the served schema never had.
    #[must_use]
    pub const fn followed(&self) -> usize {
        self.followed
    }
}

/// The shape `schema` declares at `path`.
///
/// The walk resolves `$ref` against the root's `$defs` and unwraps the union
/// an optional member is spelled as. It carries the deepest value it has
/// passed, so a member that states none of its own still answers with the
/// value of what holds it. A path that leaves the schema answers with what the
/// last schema it reached declares.
#[must_use]
pub fn expected_shape(schema: &Value, path: &[DocumentStep<'_>]) -> ExpectedShape {
    let root = schema;
    let mut node = resolved(root, schema);
    let mut example = declared_value(node);
    let mut followed = 0;
    for step in path {
        let Some(reached) = declared_member(node, *step) else {
            break;
        };
        node = resolved(root, reached);
        followed += 1;
        if let Some(found) = declared_value(node).or_else(|| declared_value(reached)) {
            example = Some(found);
        }
    }
    ExpectedShape {
        accepted: accepted_members(node),
        example: example.cloned(),
        followed,
    }
}

/// The schema one step reaches, before its reference is resolved, or `None`
/// where the schema declares no such step.
fn declared_member<'schema>(
    node: &'schema Value,
    step: DocumentStep<'_>,
) -> Option<&'schema Value> {
    match step {
        DocumentStep::Member(name) => node.get(keyword::PROPERTIES)?.get(name),
        DocumentStep::Element => node.get(keyword::ITEMS),
    }
}

/// The schema `node` stands for: its `$ref` target, or the one branch of its
/// `anyOf` or `oneOf` that is not the null one, or `node` itself.
///
/// An optional member is spelled as a union of its own schema and the null
/// one, so unwrapping that union reaches the member's real shape. A union with
/// more than one branch left is a closed set or a real choice, and stays whole:
/// what it accepts is every branch, not the first.
///
/// The walk is bounded by [`SCHEMA_REFERENCES_MAX`]: a model that refers to
/// itself is a legal schema, and a caller reaches this walk by sending one bad
/// value, so an unbounded one would hand every caller a way to stall a read.
/// A schema deeper than the bound answers with the last node reached, which
/// states less than the target would and never less than nothing.
fn resolved<'schema>(root: &'schema Value, node: &'schema Value) -> &'schema Value {
    let mut node = node;
    for _ in 0..SCHEMA_REFERENCES_MAX {
        if let Some(target) = referenced(root, node) {
            node = target;
            continue;
        }
        match value_branches(node).as_slice() {
            [branch] => node = branch,
            _ => return node,
        }
    }
    node
}

/// The definition one schema's `$ref` names, where it names one this document
/// carries.
fn referenced<'schema>(root: &'schema Value, node: &'schema Value) -> Option<&'schema Value> {
    let name = node
        .get(keyword::REF)
        .and_then(Value::as_str)?
        .strip_prefix("#/$defs/")?;
    root.get(keyword::DEFS)?.get(name)
}

/// The branches of a union that declare a value, the null branch left out.
/// Empty for a schema that is no union.
fn value_branches(node: &Value) -> Vec<&Value> {
    [keyword::ANY_OF, keyword::ONE_OF]
        .into_iter()
        .filter_map(|keyword| node.get(keyword))
        .filter_map(Value::as_array)
        .flatten()
        .filter(|branch| !is_null_schema(branch))
        .collect()
}

/// Whether a branch declares the absent value alone.
fn is_null_schema(branch: &Value) -> bool {
    branch.get(keyword::TYPE).and_then(Value::as_str) == Some(keyword::NULL)
}

/// A value one schema states it takes: its first authored example, or the
/// default it holds when the key is absent. A default is a value the schema
/// accepts by construction, so it stands where no example is authored; a
/// `null` default states absence and states no value at all.
fn declared_value(node: &Value) -> Option<&Value> {
    if let Some(example) = node
        .get(keyword::EXAMPLES)
        .and_then(Value::as_array)
        .and_then(|examples| examples.first())
    {
        return Some(example);
    }
    node.get(keyword::DEFAULT).filter(|value| !value.is_null())
}

/// What one schema accepts, sorted: an object's member names, or the values of
/// a closed set, however that set is spelled. Empty where the schema states
/// neither.
fn accepted_members(node: &Value) -> Vec<String> {
    let mut accepted: Vec<String> =
        if let Some(properties) = node.get(keyword::PROPERTIES).and_then(Value::as_object) {
            properties.keys().cloned().collect()
        } else if let Some(values) = node.get(keyword::ENUM).and_then(Value::as_array) {
            string_values(values.iter())
        } else {
            string_values(
                value_branches(node)
                    .into_iter()
                    .filter_map(|branch| branch.get(keyword::CONST)),
            )
        };
    accepted.sort();
    accepted
}

/// The string values of a set of schema values, anything else left out.
fn string_values<'value>(values: impl Iterator<Item = &'value Value>) -> Vec<String> {
    values
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
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
/// [`SearchHit`](crate::search::SearchHit)'s struct variants generate as inline
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

/// An [`EmbeddingConfiguration`](crate::configuration::EmbeddingConfiguration) states the
/// `Duration` bounds of each arm that carries one as `rift:range` on the key: schema
/// validation alone cannot compare `"5m"` against a ceiling, so the server enforces the
/// bounds at load and the schema carries them for readers.
pub fn declare_embedding_ranges(schema: &mut Schema) {
    use crate::configuration::{
        Duration, EMBEDDING_DOWNLOAD_TIMEOUT_MS_MAX, EMBEDDING_DOWNLOAD_TIMEOUT_MS_MIN,
        EMBEDDING_REQUEST_TIMEOUT_MS_MAX, EMBEDDING_REQUEST_TIMEOUT_MS_MIN,
    };
    const EMBEDDING_KIND_TAG: &str = "kind";
    const EMBEDDING_HF: &str = "hf";
    const EMBEDDING_OPENAI_COMPATIBLE: &str = "openai_compatible";
    const EMBEDDING_DOWNLOAD_TIMEOUT: &str = "download_timeout";
    const EMBEDDING_REQUEST_TIMEOUT: &str = "request_timeout";
    if let Some(arm) = tagged_union_arm(schema, EMBEDDING_KIND_TAG, EMBEDDING_HF) {
        annotate_property_in(
            arm,
            EMBEDDING_DOWNLOAD_TIMEOUT,
            RIFT_RANGE,
            range(
                &Duration::from_millis(EMBEDDING_DOWNLOAD_TIMEOUT_MS_MIN),
                &Duration::from_millis(EMBEDDING_DOWNLOAD_TIMEOUT_MS_MAX),
            ),
        );
    }
    if let Some(arm) = tagged_union_arm(schema, EMBEDDING_KIND_TAG, EMBEDDING_OPENAI_COMPATIBLE) {
        annotate_property_in(
            arm,
            EMBEDDING_REQUEST_TIMEOUT,
            RIFT_RANGE,
            range(
                &Duration::from_millis(EMBEDDING_REQUEST_TIMEOUT_MS_MIN),
                &Duration::from_millis(EMBEDDING_REQUEST_TIMEOUT_MS_MAX),
            ),
        );
    }
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

/// A [`SourceConfiguration`](crate::source::SourceConfiguration) states its `ByteSize`
/// bound as `rift:range` on the key: schema validation alone cannot compare `"512mb"`
/// against a ceiling, so the server enforces the bound at load and the schema carries it
/// for readers.
pub fn declare_source_ranges(schema: &mut Schema) {
    use crate::configuration::ByteSize;
    use crate::source::{
        SOURCE_WORKSPACE_BYTES_MAX, SOURCE_WORKSPACE_BYTES_MIN, SourceConfiguration,
    };
    annotate_property(
        schema,
        property!(SourceConfiguration, workspace_size),
        RIFT_RANGE,
        range(
            &ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MIN),
            &ByteSize::from_bytes(SOURCE_WORKSPACE_BYTES_MAX),
        ),
    );
}

/// A [`DependenciesConfiguration`](crate::dependencies::DependenciesConfiguration) states
/// its `ByteSize` and `Duration` bounds as `rift:range` on each key: schema validation
/// alone cannot compare `"4mb"` against a ceiling, so the server enforces the bounds at
/// load and the schema carries them for readers.
pub fn declare_dependencies_ranges(schema: &mut Schema) {
    use crate::configuration::{ByteSize, Duration};
    use crate::dependencies::{
        DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX, DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN,
        DEPENDENCIES_INDEX_BYTES_MAX, DEPENDENCIES_INDEX_BYTES_MIN, DEPENDENCIES_PACKAGE_BYTES_MAX,
        DEPENDENCIES_PACKAGE_BYTES_MIN, DependenciesConfiguration,
    };
    let ranges = [
        (
            property!(DependenciesConfiguration, package_size),
            range(
                &ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_MIN),
                &ByteSize::from_bytes(DEPENDENCIES_PACKAGE_BYTES_MAX),
            ),
        ),
        (
            property!(DependenciesConfiguration, index_size),
            range(
                &ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_MIN),
                &ByteSize::from_bytes(DEPENDENCIES_INDEX_BYTES_MAX),
            ),
        ),
        (
            property!(DependenciesConfiguration, command_timeout),
            range(
                &Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_MIN),
                &Duration::from_millis(DEPENDENCIES_COMMAND_TIMEOUT_MS_MAX),
            ),
        ),
    ];
    for (name, range) in ranges {
        annotate_property(schema, name, RIFT_RANGE, range);
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
        LSP_SETTLE_DELAY_MS_MAX, LSP_SETTLE_DELAY_MS_MIN, LSP_STARTUP_TIMEOUT_MS_MAX,
        LSP_STARTUP_TIMEOUT_MS_MIN, LspConfiguration,
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
            property!(LspConfiguration, settle_delay),
            range(
                &Duration::from_millis(LSP_SETTLE_DELAY_MS_MIN),
                &Duration::from_millis(LSP_SETTLE_DELAY_MS_MAX),
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

/// A [`PackageContextEntry`](crate::dependencies::PackageContextEntry) states exactly
/// one version selector: the `version` a lockfile pins, or the `requirement` a manifest
/// declares. The context builder enforces the same rule, so a manifest-only package
/// never carries an invented resolved version.
pub fn require_one_package_context_selector(schema: &mut Schema) {
    use crate::dependencies::PackageContextEntry;
    require_one_selector(
        schema,
        property!(PackageContextEntry, version),
        property!(PackageContextEntry, requirement),
    );
}

/// A [`ConfiguredPackage`](crate::dependencies::ConfiguredPackage) states exactly one
/// version selector, the same rule [`PackageContextEntry`](crate::dependencies::PackageContextEntry)
/// carries. Acceptance enforces it before the entry reaches the context.
pub fn require_one_configured_package_selector(schema: &mut Schema) {
    use crate::dependencies::ConfiguredPackage;
    require_one_selector(
        schema,
        property!(ConfiguredPackage, version),
        property!(ConfiguredPackage, requirement),
    );
}

/// The exactly-one-selector clause both package entry models carry.
fn require_one_selector(schema: &mut Schema, version: &str, requirement: &str) {
    append(
        schema,
        one_of(vec![requires(&[version]), requires(&[requirement])]),
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

/// A [`SearchParams`](crate::search::SearchParams) selects its result set with `query`,
/// `traversal`, or `change`. The server refuses a request naming none of the three, so the
/// schema states the same rule for a validating caller.
pub fn require_search_selector(schema: &mut Schema) {
    use crate::search::SearchParams;
    append(
        schema,
        described(
            "a search selects its result set with query, traversal, or change",
            any_of(vec![
                requires(&[property!(SearchParams, query)]),
                requires(&[property!(SearchParams, traversal)]),
                requires(&[property!(SearchParams, change)]),
            ]),
        ),
    );
}

/// A [`SearchTraversal`](crate::search::SearchTraversal) names the declaration its walk
/// starts at through `seed`, and no walk rides beside `change`: a comparison names two
/// committed revisions, and the language engine lane that resolves references serves the
/// current tree alone. The server enforces both halves, so the schema states them for a
/// validating caller.
pub fn require_traversal_seed(schema: &mut Schema) {
    use crate::search::{SearchParams, SearchTraversal};
    let change = property!(SearchParams, change);
    let traversal = property!(SearchParams, traversal);
    let seed = property!(SearchTraversal, seed);
    append(
        schema,
        described(
            "a traversal starts at seed",
            properties(vec![(traversal, requires(&[seed]))]),
        ),
    );
    append(
        schema,
        described(
            "a traversal never rides beside change",
            when(requires(&[change]), not(requires(&[traversal]))),
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
            property!(WorkspaceMap, module_relationships),
            property!(WorkspaceMap, packages),
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

    /// A schema shaped like a served tool's: an optional member spelled as a
    /// union with the null branch, a closed set spelled as consts, an array of
    /// a referenced type, and an example on the root alone.
    fn addressed_schema() -> Value {
        json!({
            "type": "object",
            "examples": [{"query": "beacon"}],
            "properties": {
                "query": {"type": "string"},
                "paths": {
                    "anyOf": [{"$ref": "#/$defs/PathSelector"}, {"type": "null"}]
                },
                "target": {"$ref": "#/$defs/Target"},
                "order": {"enum": ["relevance", "path"]}
            },
            "$defs": {
                "PathSelector": {
                    "type": "object",
                    "examples": [{"include": ["src/**"]}],
                    "properties": {
                        "include": {"type": "array", "items": {"$ref": "#/$defs/Pattern"}},
                        "exclude": {"type": "array", "items": {"$ref": "#/$defs/Pattern"}}
                    }
                },
                "Pattern": {"type": "string", "examples": ["src/**"]},
                "Target": {
                    "oneOf": [
                        {"const": "symbol", "type": "string"},
                        {"const": "file", "type": "string"}
                    ]
                }
            }
        })
    }

    #[test]
    fn an_empty_path_answers_the_whole_document_shape() {
        let shape = expected_shape(&addressed_schema(), &[]);
        assert_eq!(shape.followed(), 0);
        assert_eq!(shape.accepted(), ["order", "paths", "query", "target"]);
        assert_eq!(shape.example(), Some(&json!({"query": "beacon"})));
    }

    #[test]
    fn an_optional_member_resolves_through_its_union_and_reference() {
        let shape = expected_shape(&addressed_schema(), &[DocumentStep::Member("paths")]);
        assert_eq!(shape.followed(), 1);
        assert_eq!(shape.accepted(), ["exclude", "include"]);
        assert_eq!(shape.example(), Some(&json!({"include": ["src/**"]})));
    }

    #[test]
    fn an_element_step_reaches_the_item_schema_and_its_example() {
        let shape = expected_shape(
            &addressed_schema(),
            &[
                DocumentStep::Member("paths"),
                DocumentStep::Member("include"),
                DocumentStep::Element,
            ],
        );
        assert_eq!(shape.followed(), 3);
        assert!(shape.accepted().is_empty(), "a pattern accepts no member");
        assert_eq!(shape.example(), Some(&json!("src/**")));
    }

    /// A member with no example of its own answers with the example of the
    /// value that holds it, so a refusal always shows a value to send.
    #[test]
    fn a_member_without_an_example_answers_the_one_above_it() {
        let shape = expected_shape(&addressed_schema(), &[DocumentStep::Member("query")]);
        assert_eq!(shape.followed(), 1);
        assert_eq!(shape.example(), Some(&json!({"query": "beacon"})));
    }

    /// A refusal names the value it stopped at the way the wire spells it.
    #[test]
    fn a_named_member_joins_members_by_dot_and_writes_each_element() {
        assert_eq!(named_member(&[]), None, "the whole document has no name");
        assert_eq!(
            named_member(&[DocumentStep::Member("paths")]),
            Some("paths".to_owned())
        );
        assert_eq!(
            named_member(&[
                DocumentStep::Member("paths"),
                DocumentStep::Member("include"),
                DocumentStep::Element,
            ]),
            Some("paths.include[]".to_owned())
        );
    }

    /// A path segment addressing neither a member nor an element ends the walk,
    /// because a step the schema cannot follow would misalign every step after
    /// it. serde reports an enum's variant as such a segment.
    #[test]
    fn a_segment_the_schema_cannot_follow_ends_the_path() {
        #[derive(Debug, serde::Deserialize)]
        enum Choice {
            First {
                #[expect(
                    dead_code,
                    reason = "the field exists so serde reports the variant it failed inside"
                )]
                count: u32,
            },
        }

        let refused = serde_path_to_error::deserialize::<_, Choice>(json!({
            "First": {"count": "seven"}
        }))
        .expect_err("a count that is not a number must refuse");
        let segments: Vec<String> = refused
            .path()
            .iter()
            .map(|segment| format!("{segment:?}"))
            .collect();
        assert!(
            segments.iter().any(|segment| segment.starts_with("Enum")),
            "serde reports the variant as an enum segment: {segments:?}"
        );
        assert!(
            document_steps(refused.path()).is_empty(),
            "the walk stops at the variant rather than following it"
        );
    }

    /// A model may refer to itself, and a caller reaches this walk by sending
    /// one value the schema refuses. The walk answers with what it reached
    /// rather than following the cycle.
    #[test]
    fn a_schema_that_refers_to_itself_stops_at_the_reference_bound() {
        let cyclic = json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {"$ref": "#/$defs/Node"}
            }
        });
        let shape = expected_shape(&cyclic, &[DocumentStep::Member("node")]);
        assert_eq!(shape.followed(), 1);
        assert!(shape.accepted().is_empty());
        assert_eq!(shape.example(), None);
    }

    #[test]
    fn a_closed_set_answers_its_values_however_it_is_spelled() {
        let by_const = expected_shape(&addressed_schema(), &[DocumentStep::Member("target")]);
        assert_eq!(by_const.accepted(), ["file", "symbol"]);
        let by_enum = expected_shape(&addressed_schema(), &[DocumentStep::Member("order")]);
        assert_eq!(by_enum.accepted(), ["path", "relevance"]);
    }

    /// A step the schema does not declare stops the walk, and `followed`
    /// reports the prefix it did declare: a refusal then names the member the
    /// served schema has, never one it never had.
    #[test]
    fn a_step_outside_the_schema_stops_the_walk_at_the_declared_prefix() {
        let shape = expected_shape(
            &addressed_schema(),
            &[DocumentStep::Member("paths"), DocumentStep::Element],
        );
        assert_eq!(shape.followed(), 1);
        assert_eq!(shape.accepted(), ["exclude", "include"]);
        let absent = expected_shape(&addressed_schema(), &[DocumentStep::Member("path")]);
        assert_eq!(absent.followed(), 0);
        assert_eq!(absent.accepted(), ["order", "paths", "query", "target"]);
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

    /// Acceptance takes exactly one of `command` and `embedded` per LSP
    /// table, so the schema refuses both, neither, and admits each alone.
    #[test]
    fn test_lsp_schema_selects_exactly_one_engine() {
        use crate::configuration::LspConfiguration;

        let schema = serde_json::to_value(schema_for!(LspConfiguration)).expect("lsp schema");
        let validator = jsonschema::validator_for(&schema).expect("lsp validator");
        assert!(validator.is_valid(&json!({ "command": "tool" })));
        assert!(validator.is_valid(&json!({ "embedded": "ty" })));
        assert!(!validator.is_valid(&json!({ "command": "tool", "embedded": "ty" })));
        assert!(!validator.is_valid(&json!({})));
        assert!(
            !validator.is_valid(&json!({ "embedded": "rust-analyzer" })),
            "the embedded enumeration admits only engines this build links in"
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
            when(requires(&["paths"]), json!({"maxItems": 0})),
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
    fn rule_properties_exist_in_model_schemas() {
        let cases: [(&str, Value, &[&str]); 7] = [
            (
                "PackageContextEntry",
                serde_json::to_value(schema_for!(crate::dependencies::PackageContextEntry))
                    .expect("schema"),
                &["version", "requirement"],
            ),
            (
                "ConfiguredPackage",
                serde_json::to_value(schema_for!(crate::dependencies::ConfiguredPackage))
                    .expect("schema"),
                &["version", "requirement"],
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

    #[test]
    fn change_and_diagnostic_model_empty_defaults_are_declared() {
        let array = json!([]);
        let object = json!({});
        assert_default_cases(vec![
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
        assert_default_cases(vec![(
            "WorkspaceLanguageSummary",
            serde_json::to_value(schema_for!(crate::workspace::WorkspaceLanguageSummary))
                .expect("schema"),
            vec![("include", array.clone()), ("exclude", array.clone())],
        )]);
    }

    #[test]
    fn min_length_one_collections_state_no_default_and_stay_required() {
        let cases = [(
            "Relationship",
            serde_json::to_value(schema_for!(crate::read::Relationship)).expect("schema"),
            "facets",
        )];
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
}
