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

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::error as wire;
use rift_protocol::schema::{ExpectedShape, document_steps, expected_shape, named_member};
use rmcp::ErrorData;
use rmcp::handler::server::common::{FromContextPart, schema_for_input};
use rmcp::handler::server::tool::ToolCallContext;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::failure::WireFailure;

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
        Err(Error::new(ParameterFault { tool, field, shape }).tool_error(wire::ErrorPhase::Read))
    }
}

/// One refused tool call: the arguments do not match the tool's served schema.
#[derive(Debug)]
struct ParameterFault {
    tool: String,
    field: Option<String>,
    shape: ExpectedShape,
}

impl Fault for ParameterFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("tool", self.tool.clone())];
        if let Some(field) = &self.field {
            context.push(ErrorContext::new("field", field.clone()));
        }
        if !self.shape.accepted().is_empty() {
            context.push(ErrorContext::new(
                "accepted",
                self.shape.accepted().join(", "),
            ));
        }
        if let Some(example) = self.shape.example() {
            context.push(ErrorContext::new("example", example.to_string()));
        }
        context
    }
}
