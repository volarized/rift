use rift_error::errors::{ErrorDefinition, REGISTRY_NAMESPACE, registry};
use serde::Serialize;

#[derive(Serialize)]
struct ErrorRegistry {
    namespace: &'static str,
    errors: Vec<&'static ErrorDefinition>,
}

pub(crate) fn json() -> Result<String, Box<dyn std::error::Error>> {
    let registry = ErrorRegistry {
        namespace: REGISTRY_NAMESPACE,
        errors: registry().collect(),
    };
    let mut json = serde_json::to_string_pretty(&registry)?;
    json.push('\n');
    Ok(json)
}
