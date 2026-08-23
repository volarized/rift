//! Model of the server lock file `.rift/server.json`.
//!
//! `rift server` writes the lock after it binds its port and before it serves
//! requests, and holds an exclusive file lock on it for its lifetime. A
//! `rift mcp` proxy reads the file to reach the workspace's server; a file
//! that fails [`ServerLock::validate`] is stale, and the next starter
//! replaces it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The lowest loopback port the server may bind.
pub const SERVER_PORT_MIN: u16 = 12_000;
/// The highest loopback port the server may bind.
pub const SERVER_PORT_MAX: u16 = 13_000;
/// The lock file's name under the `.rift` state directory.
pub const SERVER_LOCK_FILE_NAME: &str = "server.json";
/// Characters a minted bearer token spells: 32 random bytes as unpadded
/// base64url.
pub const SERVER_TOKEN_LENGTH: usize = 43;

/// The spelling a minted bearer token must match.
pub const SERVER_TOKEN_PATTERN: &str = "^[A-Za-z0-9_-]{43}$";
/// The spelling a lock's `version` must match: `MAJOR.MINOR.PATCH` in
/// decimal without leading zeros.
pub const SERVER_VERSION_PATTERN: &str =
    "^(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)$";

/// One workspace's serving `rift server`, as `.rift/server.json` records it.
///
/// A reader that validates the file holds everything needed to issue a
/// request: the port and the bearer token. The `pid` and `version` fields
/// carry diagnostics - which process serves, built at which release.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerLock {
    /// Loopback TCP port the server listens on, 12000 to 13000.
    #[schemars(range(min = 12_000, max = 13_000))]
    pub port: u16,
    /// Bearer token every request to the server carries: 32 random bytes,
    /// spelled as unpadded base64url.
    #[schemars(regex(pattern = "^[A-Za-z0-9_-]{43}$"))]
    pub token: String,
    /// Process id of the serving `rift server`.
    #[schemars(range(min = 1))]
    pub pid: u32,
    /// Version of the binary that wrote the lock, as `MAJOR.MINOR.PATCH`.
    #[schemars(regex(pattern = "^(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)$"))]
    pub version: String,
}

impl ServerLock {
    /// Checks every bound the schema advertises.
    ///
    /// # Errors
    ///
    /// Returns the first [`ServerLockViolation`] in field order, so a
    /// diagnosing reader sees one defect at a time.
    pub fn validate(&self) -> Result<(), ServerLockViolation> {
        match self.violation() {
            Some(violation) => Err(violation),
            None => Ok(()),
        }
    }

    /// The first violated field contract, in declaration order.
    fn violation(&self) -> Option<ServerLockViolation> {
        match self {
            lock if !(SERVER_PORT_MIN..=SERVER_PORT_MAX).contains(&lock.port) => {
                Some(ServerLockViolation::PortOutOfRange {
                    port: lock.port,
                    min: SERVER_PORT_MIN,
                    max: SERVER_PORT_MAX,
                })
            }
            lock if !token_well_formed(&lock.token) => Some(ServerLockViolation::TokenMalformed),
            lock if lock.pid == 0 => Some(ServerLockViolation::ProcessIdZero),
            lock if !version_well_formed(&lock.version) => {
                Some(ServerLockViolation::VersionMalformed {
                    version: lock.version.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Whether a token spells 32 random bytes as unpadded base64url.
fn token_well_formed(token: &str) -> bool {
    let expected_length = token.len() == SERVER_TOKEN_LENGTH;
    let charset_accepted = token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    expected_length && charset_accepted
}

/// Whether a version spells `MAJOR.MINOR.PATCH` in decimal without leading
/// zeros.
fn version_well_formed(version: &str) -> bool {
    let mut components = 0_usize;
    let every_component_decimal = version.split('.').all(|component| {
        components += 1;
        decimal_component(component)
    });
    every_component_decimal && components == 3
}

/// Whether one version component is a decimal integer without leading zeros.
fn decimal_component(component: &str) -> bool {
    match component.as_bytes() {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

/// The first contract a lock file breaks. A reader treats any violation as
/// a stale lock, because the writer is gone or speaks another shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerLockViolation {
    /// The recorded port sits outside the loopback range the server binds.
    PortOutOfRange {
        /// The recorded port.
        port: u16,
        /// The lowest accepted port.
        min: u16,
        /// The highest accepted port.
        max: u16,
    },
    /// The token does not spell 32 random bytes as unpadded base64url. The
    /// token itself never appears in evidence, because it is a credential.
    TokenMalformed,
    /// The recorded process id is zero, which no process holds.
    ProcessIdZero,
    /// The version does not spell `MAJOR.MINOR.PATCH`.
    VersionMalformed {
        /// The rejected version.
        version: String,
    },
}

impl ServerLockViolation {
    /// The violation's evidence as stable key-value pairs, for error
    /// context.
    #[must_use]
    pub fn evidence(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::PortOutOfRange { port, min, max } => vec![
                ("port", port.to_string()),
                ("range", format!("{min}..={max}")),
            ],
            Self::TokenMalformed => vec![(
                "token",
                format!("expected {SERVER_TOKEN_LENGTH} base64url characters"),
            )],
            Self::ProcessIdZero => vec![("pid", "0".to_owned())],
            Self::VersionMalformed { version } => vec![("version", version.clone())],
        }
    }
}

#[cfg(test)]
mod tests {
    use schemars::schema_for;
    use serde_json::json;

    use super::{
        SERVER_PORT_MAX, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, SERVER_TOKEN_PATTERN,
        SERVER_VERSION_PATTERN, ServerLock, ServerLockViolation,
    };

    fn valid_lock() -> ServerLock {
        ServerLock {
            port: 12_345,
            token: "a".repeat(SERVER_TOKEN_LENGTH),
            pid: 4_242,
            version: "0.0.11".to_owned(),
        }
    }

    #[test]
    fn valid_lock_passes_validation() {
        assert_eq!(valid_lock().validate(), Ok(()));
    }

    #[test]
    fn port_outside_range_is_refused() {
        for port in [SERVER_PORT_MIN - 1, SERVER_PORT_MAX + 1] {
            let mut lock = valid_lock();
            lock.port = port;
            assert_eq!(
                lock.validate(),
                Err(ServerLockViolation::PortOutOfRange {
                    port,
                    min: SERVER_PORT_MIN,
                    max: SERVER_PORT_MAX,
                }),
                "port {port} must be refused"
            );
        }
    }

    #[test]
    fn range_boundary_ports_are_accepted() {
        for port in [SERVER_PORT_MIN, SERVER_PORT_MAX] {
            let mut lock = valid_lock();
            lock.port = port;
            assert_eq!(lock.validate(), Ok(()), "port {port} must be accepted");
        }
    }

    #[test]
    fn malformed_tokens_are_refused_without_echo() {
        let wrong_length = "a".repeat(SERVER_TOKEN_LENGTH - 1);
        let wrong_charset = format!("{}+", "a".repeat(SERVER_TOKEN_LENGTH - 1));
        for token in [String::new(), wrong_length, wrong_charset] {
            let mut lock = valid_lock();
            lock.token = token.clone();
            let violation = lock.validate().expect_err("token must be refused");
            assert_eq!(violation, ServerLockViolation::TokenMalformed);
            let evidence = violation.evidence();
            assert!(
                evidence
                    .iter()
                    .all(|(_, value)| !value.contains(&token) || token.is_empty()),
                "evidence must not echo the credential"
            );
        }
    }

    #[test]
    fn zero_pid_is_refused() {
        let mut lock = valid_lock();
        lock.pid = 0;
        assert_eq!(lock.validate(), Err(ServerLockViolation::ProcessIdZero));
    }

    #[test]
    fn malformed_versions_are_refused() {
        for version in ["", "0.0", "v0.0.11", "00.1.2", "0.0.11-rc1", "0.0.11.4"] {
            let mut lock = valid_lock();
            lock.version = version.to_owned();
            assert_eq!(
                lock.validate(),
                Err(ServerLockViolation::VersionMalformed {
                    version: version.to_owned(),
                }),
                "version {version:?} must be refused"
            );
        }
    }

    #[test]
    fn workspace_versions_are_accepted() {
        for version in ["0.0.11", "1.0.0", "10.20.30"] {
            let mut lock = valid_lock();
            lock.version = version.to_owned();
            assert_eq!(
                lock.validate(),
                Ok(()),
                "version {version} must be accepted"
            );
        }
    }

    #[test]
    fn lock_round_trips_through_json() {
        let lock = valid_lock();
        let text = serde_json::to_string(&lock).expect("lock must serialize");
        let restored: ServerLock = serde_json::from_str(&text).expect("lock must deserialize");
        assert_eq!(restored, lock);
    }

    #[test]
    fn unknown_and_missing_fields_are_refused() {
        let unknown = json!({
            "port": 12_345,
            "token": "a".repeat(SERVER_TOKEN_LENGTH),
            "pid": 4_242,
            "version": "0.0.11",
            "transport": "http",
        });
        assert!(serde_json::from_value::<ServerLock>(unknown).is_err());
        let missing = json!({ "port": 12_345 });
        assert!(serde_json::from_value::<ServerLock>(missing).is_err());
    }

    #[test]
    fn schema_advertises_the_enforced_bounds() {
        let schema = serde_json::to_value(schema_for!(ServerLock)).expect("schema serializes");
        let properties = &schema["properties"];
        assert_eq!(properties["port"]["minimum"], json!(SERVER_PORT_MIN));
        assert_eq!(properties["port"]["maximum"], json!(SERVER_PORT_MAX));
        assert_eq!(properties["token"]["pattern"], json!(SERVER_TOKEN_PATTERN));
        assert_eq!(properties["pid"]["minimum"], json!(1));
        assert_eq!(
            properties["version"]["pattern"],
            json!(SERVER_VERSION_PATTERN)
        );
    }
}
