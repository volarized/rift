//! The parameter wrapper every served tool takes, and the refusal a caller
//! meets when its arguments do not match the served schema.
//!
//! The wrapper stands where rmcp's own `Parameters` would, and the tool macro
//! finds it by name in the handler's signature, so the tool's input schema is
//! still the parameter model's own. What it replaces is the refusal. rmcp
//! renders a failed deserialization through serde's `Display`, which names
//! Rust types no caller can look up (`expected struct PathSelector`) and shows
//! no value the caller could send instead. Rift names the member the document
//! stopped at and prints an example of the value that member takes, both read
//! from the same schema the caller lists.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard};

use rift_error::errors;
use rift_protocol::error as wire;
use rift_protocol::read::{NodesParams, ProjectPath, SearchScope, SymbolId};
use rift_protocol::revision::Revision;
use rift_protocol::schema::{ExpectedShape, document_steps, expected_shape, named_member};
use rmcp::ErrorData;
use rmcp::handler::server::common::{FromContextPart, schema_for_input};
use rmcp::handler::server::tool::ToolCallContext;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::failure::{McpErrorExt as _, McpErrorFailExt as _, McpFailure, WireFailure};

/// The registered failure retained while the tool router handles a refused parameter.
#[derive(Clone, Default)]
pub(crate) struct ParameterFailure(Arc<Mutex<Option<McpFailure>>>);

impl ParameterFailure {
    /// Saves the first registered parameter failure for this request.
    fn record(&self, failure: McpFailure) {
        let mut stored = self.stored();
        if stored.is_none() {
            *stored = Some(failure);
        }
    }

    /// Takes the registered parameter failure, if extraction refused the request.
    pub(crate) fn take(&self) -> Option<McpFailure> {
        self.stored().take()
    }

    fn stored(&self) -> MutexGuard<'_, Option<McpFailure>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The arguments one tool call carries, deserialized into its parameter model.
pub(crate) struct Parameters<P>(pub P);

/// The MCP node selector adds a connection-local project scope to the shared read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodesToolParams {
    symbol: Option<SymbolId>,
    path: Option<ProjectPath>,
    position: Option<u64>,
    node_cursor: Option<String>,
    revision: Option<Revision>,
    #[serde(default, deserialize_with = "present_scope")]
    scope: Option<SearchScope>,
}

fn present_scope<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SearchScope>, D::Error> {
    SearchScope::deserialize(deserializer).map(Some)
}

impl NodesToolParams {
    pub(crate) fn split(self) -> (NodesParams, Option<SearchScope>) {
        let read = NodesParams {
            symbol: self.symbol,
            path: self.path,
            position: self.position,
            node_cursor: self.node_cursor,
            revision: self.revision,
        };
        (read, self.scope)
    }
}

impl JsonSchema for NodesToolParams {
    fn schema_name() -> Cow<'static, str> {
        NodesParams::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = NodesParams::json_schema(generator);
        let schema_object = schema.ensure_object();
        let properties = schema_object
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("the shared node selector has named properties");
        properties.insert(
            "scope".to_owned(),
            json!({
                "type": "string",
                "maxLength": rift_protocol::identity::SYMBOL_ID_BYTES_MAX,
                "pattern": rift_protocol::identity::LOCAL_SCOPE_PATTERN,
                "not": {"pattern": rift_protocol::identity::LOCAL_SCOPE_RESERVED_PATTERN},
                "description": "Project that owns `path`. Omitted or `local` selects the current project. The caller MAY select a registered project or profile by its local scope."
            }),
        );
        let rules = schema_object
            .get_mut("allOf")
            .and_then(Value::as_array_mut)
            .expect("the shared node selector has a selector constraint");
        rules.push(json!({
            "if": {"required": ["scope"]},
            "then": {"required": ["path", "position"]}
        }));
        let examples = schema_object
            .get_mut("examples")
            .and_then(Value::as_array_mut)
            .expect("the shared node selector has examples");
        examples.push(json!({"path": "src/config.rs", "position": 338, "scope": "local@review"}));
        schema
    }
}

/// Established project identities captured before parameter decoding.
#[derive(Clone, Debug)]
pub(crate) struct SymbolExamples(pub(crate) Vec<rift_core::SymbolId>);

impl SymbolExamples {
    fn requests(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| {
            self.0
                .iter()
                .take(2)
                .map(|id| serde_json::json!({"id": id.as_str()}).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

impl<P: JsonSchema> JsonSchema for Parameters<P> {
    fn schema_name() -> Cow<'static, str> {
        P::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        P::json_schema(generator)
    }
}

impl<S, P> FromContextPart<ToolCallContext<'_, S>> for Parameters<P>
where
    P: DeserializeOwned + JsonSchema + std::any::Any,
{
    fn from_context_part(context: &mut ToolCallContext<S>) -> Result<Self, ErrorData> {
        let tool = context.name().to_string();
        let arguments = Value::Object(context.arguments.take().unwrap_or_default());
        let refused = match serde_path_to_error::deserialize::<_, P>(arguments) {
            Ok(parameters) => return Ok(Self(parameters)),
            Err(refused) => refused,
        };
        let steps = document_steps(refused.path());
        let shape = schema_for_input::<P>().map_or_else(
            |_| ExpectedShape::default(),
            |schema| expected_shape(&Value::Object(schema.as_ref().clone()), &steps),
        );
        let field = named_member(&steps[..shape.followed()]);
        let canonical = tool == "get_symbol" && field.as_deref() == Some("id");
        let accepted = if canonical {
            Some("rift://symbol/<owner>/<language>/<qualified_path>".to_owned())
        } else {
            (!shape.accepted().is_empty()).then(|| shape.accepted().join(", "))
        };
        let example = if canonical {
            context
                .request_context()
                .extensions
                .get::<SymbolExamples>()
                .and_then(SymbolExamples::requests)
        } else {
            shape.example().map(ToString::to_string)
        };
        let failure = errors::mcp::parameter_invalid()
            .tool(tool)
            .maybe_field(field)
            .maybe_accepted(accepted)
            .maybe_example(example)
            .mcp();
        let error = failure.tool_error(wire::ErrorPhase::Read);
        if let Some(parameter_failure) = context
            .request_context()
            .extensions
            .get::<ParameterFailure>()
        {
            parameter_failure.record(failure);
        }
        error.fail()
    }
}
