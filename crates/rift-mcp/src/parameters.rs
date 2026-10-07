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
use rift_protocol::schema::{ExpectedShape, document_steps, expected_shape, named_member};
use rmcp::ErrorData;
use rmcp::handler::server::common::{FromContextPart, schema_for_input};
use rmcp::handler::server::tool::ToolCallContext;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde_json::Value;

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
        let failure = errors::mcp::parameter_invalid()
            .tool(tool)
            .maybe_field(field)
            .maybe_accepted((!shape.accepted().is_empty()).then(|| shape.accepted().join(", ")))
            .maybe_example(shape.example().map(ToString::to_string))
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
