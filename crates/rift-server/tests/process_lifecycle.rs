//! Real-process fixtures for `EnginePool` tests: canned response
//! sequences over a real `sh` process, with no scripted engine binary.
//!
//! `EnginePool` always spawns a real process through `LspConfiguration`;
//! there is no transport to substitute in-process the way `rift-lsp`'s own
//! session tests do. Every fixture here is a plain `sh -c` script answering
//! a fixed sequence of framed JSON-RPC responses without ever reading or
//! parsing its stdin: the script does not need to know what request it is
//! answering, only how many requests it has already answered, which is
//! fixed at script-authoring time by the test itself. A fixture that must
//! die once and then serve keeps that one bit of state in a marker file on
//! disk, since a restart is a fresh process with no memory of its own.

use std::collections::BTreeMap;
use std::path::Path;

use rift_protocol::configuration::{ByteSize, CommandInput, Duration, LspConfiguration};
use rift_protocol::retry::{RestartPolicy, RetryPolicy};

/// The shell every fixture runs under, resolved through `PATH`.
const SHELL_PROGRAM: &str = "sh";

/// The `initialize` response body every fixture answers with, advertising
/// rename and prepared rename.
const INITIALIZE_BODY: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"renameProvider":{"prepareProvider":true}}}}"#;

/// One framed JSON-RPC message.
fn framed(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

/// A success answer to request `id`, carrying one text edit.
pub(crate) fn ok_response(id: u64) -> String {
    framed(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"changes":{{"file:///src/lib.rs":[{{"range":{{"start":{{"line":0,"character":3}},"end":{{"line":0,"character":9}}}},"newText":"renamed"}}]}}}}}}"#
    ))
}

/// A `null` answer to request `id`.
pub(crate) fn null_response(id: u64) -> String {
    framed(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#))
}

/// A refusal answering request `id` with `code` and `message`.
pub(crate) fn refused_response(id: u64, code: i64, message: &str) -> String {
    framed(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":"{message}"}}}}"#
    ))
}

/// One process configuration and its exact language bindings.
pub(crate) struct ProcessFixture {
    pub(crate) configuration: LspConfiguration,
    pub(crate) languages: Vec<String>,
}

impl std::ops::Deref for ProcessFixture {
    type Target = LspConfiguration;

    fn deref(&self) -> &Self::Target {
        &self.configuration
    }
}

impl std::ops::DerefMut for ProcessFixture {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.configuration
    }
}

/// Process fixture resolving `sh -c script`, bound to `languages`.
fn table(script: String, languages: &[&str]) -> ProcessFixture {
    let configuration = LspConfiguration {
        command: CommandInput::ProgramAndArguments(vec![
            SHELL_PROGRAM.to_owned(),
            "-c".to_owned(),
            script,
        ]),
        environment: BTreeMap::new(),
        initialization_options: None,
        startup_timeout: Duration::from_millis(10_000),
        request_timeout: Duration::from_millis(10_000),
        output_limit: ByteSize::from_bytes(4_096),
        retry: RetryPolicy::default(),
        restart: RestartPolicy::default(),
    };
    ProcessFixture {
        configuration,
        languages: languages
            .iter()
            .map(|&language| language.to_owned())
            .collect(),
    }
}

/// Answers `initialize`, then `responses` in order, then exits. A brief
/// lingering delay after the last response lets the handshake's own
/// `initialized` notification, and every notification the client sends
/// between requests, land before the process closes its side.
pub(crate) fn answers(responses: &[String], languages: &[&str]) -> ProcessFixture {
    let mut script = format!("printf '%s' '{}", framed(INITIALIZE_BODY));
    for response in responses {
        script.push_str(response);
    }
    script.push_str("'; sleep 0.2");
    table(script, languages)
}

/// Answers `initialize`, lingers briefly for the same reason [`answers`]
/// does, then exits without answering anything further: every later
/// request meets a closed connection.
pub(crate) fn answers_initialize_then_exits(languages: &[&str]) -> ProcessFixture {
    answers(&[], languages)
}

/// Answers `initialize` and then hangs forever, reading nothing further:
/// every later request overstays its timeout.
///
/// `exec` replaces the shell's own process image with `sleep` instead of
/// forking it as a child, so the pid a test kills is the one actually
/// holding the pipes; see `rift-lsp`'s own `process_lifecycle.rs` for the
/// orphaned-child hazard a plain trailing `sleep` carries.
pub(crate) fn answers_initialize_then_hangs(languages: &[&str]) -> ProcessFixture {
    table(
        format!(
            "printf '%s' '{}'; exec sleep 999999",
            framed(INITIALIZE_BODY)
        ),
        languages,
    )
}

/// Dies right after the handshake on its first run, then answers
/// `responses` normally on every run after: a marker file on disk is the
/// one bit of state a restart, a fresh process, cannot keep in memory.
pub(crate) fn dies_once_then_answers(
    marker: &Path,
    responses: &[String],
    languages: &[&str],
) -> ProcessFixture {
    let mut served = format!("printf '%s' '{}", framed(INITIALIZE_BODY));
    for response in responses {
        served.push_str(response);
    }
    served.push_str("'; sleep 0.2");
    let script = format!(
        "if [ -f '{marker}' ]; then {served}; else touch '{marker}'; \
         printf '%s' '{init}'; sleep 0.2; fi",
        marker = marker.display(),
        init = framed(INITIALIZE_BODY),
    );
    table(script, languages)
}

/// `configuration` with its retry budget narrowed to `attempts`, delayed by
/// one millisecond so the suite spends no time on the wait; the shape of
/// the growing wait is proven by the policy's own unit tests.
pub(crate) fn retrying(mut configuration: ProcessFixture, attempts: u64) -> ProcessFixture {
    configuration.retry = RetryPolicy {
        attempts,
        delay: Duration::from_millis(1),
        delay_limit: Duration::from_millis(1),
    };
    configuration
}

/// A launch resolving a program no `PATH` entry can ever find: every start
/// fails before a process exists.
pub(crate) fn absent_program(languages: &[&str]) -> ProcessFixture {
    let mut absent = table(String::new(), languages);
    absent.command = CommandInput::Program("rift_absent_engine".to_owned());
    absent
}

/// A launch naming an absolute executable path: refused before any process
/// exists, and refused the same way every time.
pub(crate) fn absolute_program(languages: &[&str]) -> ProcessFixture {
    let mut absolute = table(String::new(), languages);
    absolute.command = CommandInput::Program("/usr/bin/rift_absent_engine".to_owned());
    absolute
}

/// [`answers_initialize_then_exits`], parked on a FIFO until the test
/// writes to it: the handshake itself is held, not merely delayed, so a
/// test can prove every racing request is in flight before any engine
/// starts answering.
pub(crate) fn gated_then_answers(
    gate: &Path,
    responses: &[String],
    languages: &[&str],
) -> ProcessFixture {
    let mut served = format!("printf '%s' '{}", framed(INITIALIZE_BODY));
    for response in responses {
        served.push_str(response);
    }
    served.push_str("'; sleep 0.2");
    let script = format!("read -r _line < '{}'; {served}", gate.display());
    table(script, languages)
}
