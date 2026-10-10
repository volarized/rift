use serde::Deserialize;

use super::{PackageAnalysisWarning, ProjectPath, SourceUnitId, TextRange};

#[derive(Deserialize)]
#[serde(tag = "code", deny_unknown_fields, rename_all = "snake_case")]
pub(super) enum WarningInput {
    IdentityUnresolved {
        #[serde(default, deserialize_with = "present")]
        unit: (bool, Option<SourceUnitId>),
        #[serde(default, deserialize_with = "present")]
        range: (bool, Option<TextRange>),
        qualified_name: String,
    },
    ObjectUnavailable {
        #[serde(default, deserialize_with = "present")]
        unit: (bool, Option<SourceUnitId>),
        #[serde(default, deserialize_with = "present")]
        range: (bool, Option<TextRange>),
        qualified_name: String,
        field: String,
        bound: u64,
    },
    UnitUnavailable {
        path: ProjectPath,
        detail: String,
    },
    SourceTruncated {
        path: ProjectPath,
        dropped: u64,
    },
    PublicationTruncated {
        collection: String,
        bound: u64,
    },
}

pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<(bool, Option<T>), D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| (true, value))
}

impl TryFrom<WarningInput> for PackageAnalysisWarning {
    type Error = &'static str;

    fn try_from(value: WarningInput) -> Result<Self, Self::Error> {
        let warning = match value {
            WarningInput::IdentityUnresolved {
                unit,
                range,
                qualified_name,
            } => {
                if unit.0 != range.0 {
                    return Err("identity_unresolved requires paired source unit and range");
                }
                Self::IdentityUnresolved {
                    unit: unit.1,
                    range: range.1,
                    qualified_name,
                }
            }
            WarningInput::UnitUnavailable { path, detail } => {
                Self::UnitUnavailable { path, detail }
            }
            WarningInput::ObjectUnavailable {
                unit,
                range,
                qualified_name,
                field,
                bound,
            } => {
                if unit.0 != range.0 {
                    return Err("object_unavailable requires paired source unit and range");
                }
                Self::ObjectUnavailable {
                    unit: unit.1,
                    range: range.1,
                    qualified_name,
                    field,
                    bound,
                }
            }
            WarningInput::SourceTruncated { path, dropped } => {
                Self::SourceTruncated { path, dropped }
            }
            WarningInput::PublicationTruncated { collection, bound } => {
                Self::PublicationTruncated { collection, bound }
            }
        };
        if !warning.is_valid() {
            if matches!(warning, Self::ObjectUnavailable { .. }) {
                return Err(
                    "object_unavailable requires paired source unit and range and valid field bounds",
                );
            }
            return Err("identity_unresolved requires paired source unit and range");
        }
        Ok(warning)
    }
}

impl PackageAnalysisWarning {
    /// Checks paired source fields and enforced public field bounds.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self {
            Self::IdentityUnresolved { unit, range, .. } => {
                source_is_valid(unit.as_ref(), range.as_ref())
            }
            Self::ObjectUnavailable {
                unit,
                range,
                field,
                bound,
                ..
            } => {
                source_is_valid(unit.as_ref(), range.as_ref())
                    && !field.is_empty()
                    && field.chars().take(129).count() <= 128
                    && *bound > 0
            }
            _ => true,
        }
    }
}

fn source_is_valid(unit: Option<&SourceUnitId>, range: Option<&TextRange>) -> bool {
    match (unit, range) {
        (None, None) => true,
        (Some(unit), Some(range)) => {
            SourceUnitId::parse(unit.as_str()).is_ok() && range.start <= range.end
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::PackageAnalysisWarning;
    use serde_json::json;

    #[test]
    fn unresolved_identity_source_fields_are_paired_in_serde_and_schema() {
        assert_paired_source("identity_unresolved");
    }

    #[test]
    fn unavailable_object_source_fields_are_paired_in_serde_and_schema() {
        assert_paired_source("object_unavailable");
    }

    fn assert_paired_source(code: &str) {
        let schema = serde_json::to_value(schemars::schema_for!(PackageAnalysisWarning))
            .expect("warning schema");
        let validator = jsonschema::validator_for(&schema).expect("paired warning schema");
        for (fields, accepted) in [
            (json!({}), true),
            (
                json!({"unit":"rift://source/project/lib.rs","range":{"start":0,"end":8}}),
                true,
            ),
            (json!({"unit":null,"range":null}), true),
            (json!({"unit":"rift://source/project/lib.rs"}), false),
            (json!({"range":{"start":0,"end":8}}), false),
            (json!({"unit":null}), false),
            (json!({"range":null}), false),
            (json!({"unit":null,"range":{"start":0,"end":8}}), false),
            (
                json!({"unit":"rift://source/project/lib.rs","range":null}),
                false,
            ),
        ] {
            let mut value = json!({"code":code,"qualified_name":"beacon::start"});
            if code == "object_unavailable" {
                value["field"] = json!("name");
                value["bound"] = json!(4096);
            }
            value
                .as_object_mut()
                .expect("warning object")
                .extend(fields.as_object().expect("source fields").clone());
            assert_eq!(validator.is_valid(&value), accepted, "schema: {value}");
            let parsed = serde_json::from_value::<PackageAnalysisWarning>(value.clone());
            assert_eq!(parsed.is_ok(), accepted, "serde: {value}");
            if let Ok(warning) = parsed {
                assert!(warning.is_valid());
                let wire = serde_json::to_value(&warning).expect("warning serialization");
                assert!(validator.is_valid(&wire));
                assert_eq!(
                    serde_json::from_value::<PackageAnalysisWarning>(wire)
                        .expect("warning roundtrip"),
                    warning
                );
            }
        }
    }

    #[test]
    fn unresolved_identity_refuses_invalid_unit_and_reversed_range() {
        for value in [
            json!({"code":"identity_unresolved","qualified_name":"start","unit":"rift://source/project/../lib.rs","range":{"start":0,"end":8}}),
            json!({"code":"identity_unresolved","qualified_name":"start","unit":"rift://source/project/lib.rs","range":{"start":8,"end":0}}),
        ] {
            assert!(serde_json::from_value::<PackageAnalysisWarning>(value).is_err());
        }
    }

    #[test]
    fn unavailable_object_retains_wide_name_and_refuses_invalid_bounds_or_source() {
        let schema = serde_json::to_value(schemars::schema_for!(PackageAnalysisWarning))
            .expect("warning schema");
        let validator = jsonschema::validator_for(&schema).expect("warning bounds");
        let valid = json!({"code":"object_unavailable", "qualified_name":"n".repeat(4097),
            "field":"name", "bound":4096});
        let warning = serde_json::from_value::<PackageAnalysisWarning>(valid.clone())
            .expect("full original qualified name");
        assert!(warning.is_valid());
        assert!(validator.is_valid(&valid));
        assert_eq!(
            serde_json::to_value(&warning).expect("warning source"),
            valid
        );
        for (field, bound) in [
            (String::new(), 4096),
            ("x".repeat(129), 4096),
            ("name".to_owned(), 0),
        ] {
            let mut value = valid.clone();
            value["field"] = json!(field);
            value["bound"] = json!(bound);
            assert!(!validator.is_valid(&value));
            assert!(serde_json::from_value::<PackageAnalysisWarning>(value).is_err());
        }
        for fields in [
            json!({"unit":"rift://source/project/../lib.rs","range":{"start":0,"end":8}}),
            json!({"unit":"rift://source/project/lib.rs","range":{"start":8,"end":0}}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .expect("warning object")
                .extend(fields.as_object().expect("source fields").clone());
            assert!(serde_json::from_value::<PackageAnalysisWarning>(value).is_err());
        }
    }
}
