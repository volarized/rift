use std::path::{Path, PathBuf};

use schemars::Schema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ProductIdentity, ServerLock};
use crate::configuration::ServerConfiguration;
use crate::identity::SymbolOwner;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LockInput {
    port: u16,
    token: String,
    pid: u32,
    identity: ProductIdentity,
    #[serde(default)]
    server: Option<ServerConfiguration>,
    #[serde(default, deserialize_with = "crate::index::warning::present")]
    root: (bool, Option<PathBuf>),
    #[serde(default, deserialize_with = "crate::index::warning::present")]
    owner: (bool, Option<SymbolOwner>),
}

impl TryFrom<LockInput> for ServerLock {
    type Error = &'static str;

    fn try_from(input: LockInput) -> Result<Self, Self::Error> {
        if input.root.0 != input.owner.0
            || (input.root.0 && (input.root.1.is_none() || input.owner.1.is_none()))
        {
            return Err("server lock requires paired workspace root and local owner");
        }
        let lock = Self {
            port: input.port,
            token: input.token,
            pid: input.pid,
            identity: input.identity,
            server: input.server,
            root: input.root.1,
            owner: input.owner.1,
        };
        if !lock.workspace_context_is_valid() {
            return Err("server lock requires paired workspace root and local owner");
        }
        Ok(lock)
    }
}

#[derive(Serialize)]
struct LockOutput<'a> {
    port: u16,
    token: &'a str,
    pid: u32,
    identity: &'a ProductIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'a ServerConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<&'a SymbolOwner>,
}

impl Serialize for ServerLock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if !self.workspace_context_is_valid() {
            return Err(serde::ser::Error::custom(
                "server lock requires paired workspace root and local owner",
            ));
        }
        LockOutput {
            port: self.port,
            token: &self.token,
            pid: self.pid,
            identity: &self.identity,
            server: self.server.as_ref(),
            root: self.root.as_deref(),
            owner: self.owner.as_ref(),
        }
        .serialize(serializer)
    }
}

pub(super) fn pair_workspace_context(schema: &mut Schema) {
    schema.ensure_object().insert(
        "allOf".to_owned(),
        json!([{
            "oneOf": [
                {"not": {"anyOf": [{"required": ["root"]}, {"required": ["owner"]}]}},
                {
                    "required": ["root", "owner"],
                    "properties": {
                        "root": {"type": "string", "minLength": 1},
                        "owner": {
                            "type": "object",
                            "properties": {"kind": {"enum": ["local", "named_local"]}}
                        }
                    }
                }
            ]
        }]),
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn legacy() -> serde_json::Value {
        json!({
            "port": 12345,
            "token": "a".repeat(super::super::SERVER_TOKEN_LENGTH),
            "pid": 42,
            "identity": {"version": "0.0.11", "schema_digest": "b".repeat(64)}
        })
    }

    #[test]
    fn workspace_context_pair_matches_schema_and_serde() {
        let schema = serde_json::to_value(schemars::schema_for!(ServerLock)).expect("lock schema");
        let validator = jsonschema::validator_for(&schema).expect("lock validator");
        for (fields, accepted) in [
            (json!({}), true),
            (
                json!({"root": "/workspace", "owner": {"kind": "local"}}),
                true,
            ),
            (
                json!({"root": "/workspace", "owner": {"kind": "named_local", "name": "cloud"}}),
                true,
            ),
            (json!({"root": "/workspace"}), false),
            (json!({"owner": {"kind": "local"}}), false),
            (json!({"root": null}), false),
            (json!({"owner": null}), false),
            (json!({"root": null, "owner": null}), false),
            (json!({"root": "/workspace", "owner": null}), false),
            (json!({"root": null, "owner": {"kind": "local"}}), false),
            (
                json!({"root": "/workspace", "owner": {"kind": "runtime", "runtime": "node", "version": "26.11.1"}}),
                false,
            ),
        ] {
            let mut wire = legacy();
            wire.as_object_mut()
                .expect("lock object")
                .extend(fields.as_object().expect("context fields").clone());
            assert_eq!(validator.is_valid(&wire), accepted, "schema: {wire}");
            let parsed = serde_json::from_value::<ServerLock>(wire.clone());
            assert_eq!(parsed.is_ok(), accepted, "serde: {wire}");
            if let Ok(lock) = parsed {
                let encoded = serde_json::to_value(&lock).expect("accepted lock serializes");
                assert_eq!(
                    serde_json::from_value::<ServerLock>(encoded).expect("lock round trip"),
                    lock
                );
            }
        }
    }

    #[test]
    fn constructed_invalid_context_cannot_serialize() {
        let legacy: ServerLock = serde_json::from_value(legacy()).expect("legacy lock");
        for lock in [
            ServerLock {
                root: Some(PathBuf::from("/workspace")),
                ..legacy.clone()
            },
            ServerLock {
                owner: Some(SymbolOwner::Local),
                ..legacy.clone()
            },
            ServerLock {
                root: Some(PathBuf::from("relative")),
                owner: Some(SymbolOwner::Local),
                ..legacy.clone()
            },
            ServerLock {
                root: Some(PathBuf::from("/workspace")),
                owner: Some(SymbolOwner::NamedLocal {
                    name: "bad/name".to_owned(),
                }),
                ..legacy
            },
        ] {
            assert!(!lock.workspace_context_is_valid());
            assert!(serde_json::to_value(lock).is_err());
        }
    }
}
