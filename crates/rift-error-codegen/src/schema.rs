use std::collections::BTreeMap;

use serde::Deserialize;

use crate::validate::CodegenError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRegistry {
    pub registry: RegistryHeader,
    pub error: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryHeader {
    pub namespace: String,
    pub schema: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawError {
    pub message: String,
    pub action: String,
    #[serde(default)]
    pub fields: BTreeMap<String, RawField>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawField {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub optional: bool,
    pub format: Option<FieldFormat>,
    pub role: Option<FieldRole>,
    #[serde(default = "default_true")]
    pub display: bool,
    #[serde(default)]
    pub sensitive: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldType {
    String,
    Bool,
    Integer,
    Unsigned,
    Pid,
    Port,
    Path,
    Duration,
    Error,
    RiftError,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldFormat {
    Display,
    Human,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldRole {
    Source,
    Cause,
}

pub(crate) fn parse(source: &str) -> Result<RawRegistry, CodegenError> {
    toml::from_str(source).map_err(|error| CodegenError::Toml(error.to_string()))
}

pub(crate) fn definitions(
    entries: BTreeMap<String, toml::Value>,
) -> Result<Vec<(Vec<String>, RawError)>, CodegenError> {
    let mut output = Vec::new();
    for (name, value) in entries {
        collect(vec![name], value, &mut output)?;
    }
    Ok(output)
}

fn collect(
    path: Vec<String>,
    value: toml::Value,
    output: &mut Vec<(Vec<String>, RawError)>,
) -> Result<(), CodegenError> {
    let table = value.as_table().ok_or_else(|| {
        CodegenError::Invalid(format!("error path {} must be a table", path.join(".")))
    })?;
    if table.contains_key("message") || table.contains_key("action") || table.contains_key("fields")
    {
        let raw = value
            .try_into::<RawError>()
            .map_err(|error| CodegenError::Invalid(format!("error {}: {error}", path.join("."))))?;
        output.push((path, raw));
        return Ok(());
    }
    for (name, child) in table.iter() {
        let mut child_path = path.clone();
        child_path.push(name.clone());
        collect(child_path, child.clone(), output)?;
    }
    Ok(())
}
