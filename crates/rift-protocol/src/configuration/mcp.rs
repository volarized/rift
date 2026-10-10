use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use super::{CONFIGURATION_FILE_BYTES_MAX, ConfigurationViolation};
use crate::identity::{SYMBOL_ID_BYTES_MAX, parse_local_scope};

/// The `[mcp]` table: explicit local projects registered for one connection.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfiguration {
    /// Registration names and paths. Relative paths start at the primary project root.
    /// CLI entries replace saved entries with the same name. Each project keeps its own
    /// configuration; extra registrations are not imported recursively.
    pub extra: BTreeMap<String, String>,
}

impl McpConfiguration {
    /// Checks names, path forms and the aggregate UTF-8 input byte budget before roots
    /// are resolved. This check reads neither the environment nor the filesystem.
    ///
    /// # Errors
    /// Returns the first invalid entry or an aggregate input past the configuration bound.
    pub fn validate(&self) -> Result<(), rift_error::RiftError> {
        match self.violation() {
            Some(violation) => Err(super::configuration_violation_error(&violation)),
            None => Ok(()),
        }
    }

    pub(super) fn violation(&self) -> Option<ConfigurationViolation> {
        let lengths = self.extra.iter().map(|(name, path)| {
            (
                u64::try_from(name.len()).unwrap_or(u64::MAX),
                u64::try_from(path.len()).unwrap_or(u64::MAX),
            )
        });
        if let Err(violation) = registration_bytes(lengths) {
            return Some(violation);
        }
        self.extra.iter().find_map(|(name, path)| {
            let detail = if parse_local_scope(&format!("local@{name}")).is_err() {
                "registration name is not canonical"
            } else if path.is_empty()
                || path.chars().any(char::is_control)
                || (path.starts_with('~') && path != "~" && !path.starts_with("~/"))
            {
                "registration path is empty, contains a control character, or uses unsupported home expansion"
            } else {
                return None;
            };
            Some(ConfigurationViolation::McpRegistrationInvalid {
                field: "mcp.extra",
                name: name.clone(),
                detail,
            })
        })
    }
}

pub(super) fn registration_evidence(
    field: &'static str,
    name: &str,
    detail: &'static str,
) -> Vec<(&'static str, String)> {
    vec![
        ("field", field.to_owned()),
        ("name", name.to_owned()),
        ("detail", detail.to_owned()),
    ]
}

fn registration_bytes(
    lengths: impl IntoIterator<Item = (u64, u64)>,
) -> Result<u64, ConfigurationViolation> {
    lengths.into_iter().try_fold(0_u64, |total, (name, path)| {
        let bytes = name
            .checked_add(path)
            .and_then(|entry| total.checked_add(entry));
        match bytes {
            Some(value) if value <= CONFIGURATION_FILE_BYTES_MAX => Ok(value),
            _ => Err(ConfigurationViolation::LimitOutOfRange {
                field: "mcp.extra",
                value: bytes.unwrap_or(u64::MAX),
                min: 0,
                max: CONFIGURATION_FILE_BYTES_MAX,
            }),
        }
    })
}

impl JsonSchema for McpConfiguration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "McpConfiguration".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "extra": {
                    "type": "object",
                    "default": {},
                    "description": "Explicit named local projects. Aggregate UTF-8 name and path bytes must fit the configuration document byte budget before roots are resolved.",
                    "propertyNames": {
                        "pattern": "^[a-z][a-z0-9_-]*$",
                        "maxLength": SYMBOL_ID_BYTES_MAX - "local@".len(),
                        "not": {"enum": ["local", "global", "all"]}
                    },
                    "additionalProperties": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": CONFIGURATION_FILE_BYTES_MAX,
                        "allOf": [
                            {"not": {"pattern": "[\\u0000-\\u001f\\u007f-\\u009f]"}},
                            {"anyOf": [
                                {"const": "~"},
                                {"pattern": "^~/"},
                                {"not": {"pattern": "^~"}}
                            ]}
                        ]
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
