//! The test identity every `rift` process a test spawns carries as an OpenTelemetry
//! resource attribute, so a collector files what each process exports under its test.
//!
//! A module of its own: suites that spawn `rift` without starting a server include it
//! without the server harness.

use std::fmt::Write as _;

/// The OpenTelemetry resource attribute naming the test that spawned a `rift` process.
pub(crate) const TEST_CASE_NAME_ATTRIBUTE: &str = "test.case.name";
/// The variable the OpenTelemetry SDK's `EnvResourceDetector` reads resource attributes
/// from, as `key=value` entries separated by `,`.
const RESOURCE_ATTRIBUTES_VARIABLE: &str = "OTEL_RESOURCE_ATTRIBUTES";
/// The variable the test runner sets to `true` in every test process, so a test that
/// installs Rift's tracing in its own process exports nothing; a spawned `rift` process
/// does not inherit it and exports to the runner's collector.
const SDK_DISABLED_VARIABLE: &str = "OTEL_SDK_DISABLED";

/// The name of the running test: the nextest attempt id, which names one attempt of one
/// test in one run, or the test thread's name outside nextest.
pub(crate) fn test_case_name() -> String {
    std::env::var("NEXTEST_ATTEMPT_ID").unwrap_or_else(|_| {
        std::thread::current()
            .name()
            .unwrap_or("unnamed test")
            .to_owned()
    })
}

/// Sets `OTEL_RESOURCE_ATTRIBUTES` on one `rift` child so it carries
/// [`TEST_CASE_NAME_ATTRIBUTE`] with the [`test_case_name`]. Every process the child
/// spawns inherits the variable: a detached server's command inherits its caller's
/// environment. Entries the test process inherited stay, ahead of this one.
///
/// Call it on the test's own thread: outside nextest the name is the thread's.
pub(crate) fn with_test_case_name(
    command: &mut std::process::Command,
) -> &mut std::process::Command {
    let inherited = std::env::var(RESOURCE_ATTRIBUTES_VARIABLE).ok();
    command
        .env(
            RESOURCE_ATTRIBUTES_VARIABLE,
            resource_attributes(inherited.as_deref(), &test_case_name()),
        )
        .env_remove(SDK_DISABLED_VARIABLE)
}

/// The `OTEL_RESOURCE_ATTRIBUTES` value carrying `inherited`'s entries, then
/// [`TEST_CASE_NAME_ATTRIBUTE`] set to `test_case_name`.
///
/// `opentelemetry_sdk` 0.33 splits the value at `,`, splits each entry at its first `=`,
/// trims both sides, and decodes nothing; a later entry of the same key replaces an
/// earlier one. The value therefore writes `,`, `%`, whitespace, and control characters
/// as `%XX` of their UTF-8 bytes, and carries every other character as it is, so a nextest
/// attempt id, which holds none of them, arrives unchanged.
pub(crate) fn resource_attributes(inherited: Option<&str>, test_case_name: &str) -> String {
    let mut value = String::with_capacity(test_case_name.len());
    for character in test_case_name.chars() {
        if matches!(character, ',' | '%') || character.is_whitespace() || character.is_control() {
            let mut bytes = [0_u8; 4];
            for byte in character.encode_utf8(&mut bytes).bytes() {
                let _ = write!(value, "%{byte:02X}");
            }
        } else {
            value.push(character);
        }
    }
    match inherited.filter(|inherited| !inherited.trim().is_empty()) {
        Some(inherited) => format!("{inherited},{TEST_CASE_NAME_ATTRIBUTE}={value}"),
        None => format!("{TEST_CASE_NAME_ATTRIBUTE}={value}"),
    }
}
