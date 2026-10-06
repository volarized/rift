//! Text of a tool failure: one section that lists the failure and each of its causes.
//!
//! ```text
//! 2 errors
//!   limit_exceeded · retry never
//!     the request crosses a limit; narrow the request
//!     limit source.files: 11 over 10
//!     error[E0308] src/lib.rs:3:9: mismatched types
//!     rift.index.workspace_too_many_files
//!   storage_failure · retry same_request
//!     the store refused
//! ```
//!
//! The failure comes first, then the causes, outermost first. Every entry writes its code and
//! retry directive, then its message. The failure adds its limit, its diagnostics, and, for a
//! registered failure, its registered identity. The message of a registered failure is the
//! registered message with its evidence, then its registered action when the registry defines
//! one. The phase is not written: `read` is the only phase. Every line is one line: control
//! characters are made visible.

use rift_protocol::error::{ErrorCause, ErrorCode, ErrorData, LimitEvidence, RetryDirective};
use rift_protocol::read::{Diagnostic, DiagnosticContext};

use super::facts::{FACT_SEPARATOR, wire_name};
use super::layout;
use crate::output::text::{TextError, TextWriter};

/// Prefix that the file identity of a diagnostic span shares.
const FILE_SCHEME: &str = "rift://file/";
/// Noun counted by the title of a failure.
const ERROR_NOUN: &str = "error";
/// Depth of an entry head under the title.
const HEAD_DEPTH: usize = 0;
/// Depth of the lines under an entry head.
const DETAIL_DEPTH: usize = 1;

/// A registered failure as its text writes it: the typed wire error and the registered identity.
pub(crate) struct RegisteredFailure<'a> {
    /// The registered identity, such as `rift.index.workspace_too_many_files`.
    pub(crate) identity: &'a str,
    /// The typed wire error.
    pub(crate) error: &'a ErrorData,
}

/// Writes the failure of a typed wire error: the title, the failure entry, and one entry per
/// cause.
pub(super) fn answer(out: &mut TextWriter, error: &ErrorData) -> Result<(), TextError> {
    failure(out, error, None)
}

/// Writes a registered failure: the entries of its typed wire error, with the registered identity
/// as the last line of the failure entry.
pub(super) fn registered(
    out: &mut TextWriter,
    registered: &RegisteredFailure<'_>,
) -> Result<(), TextError> {
    let RegisteredFailure { identity, error } = registered;
    failure(out, error, Some(identity))
}

/// Writes the title, the failure entry, and one entry per cause.
fn failure(
    out: &mut TextWriter,
    error: &ErrorData,
    identity: Option<&str>,
) -> Result<(), TextError> {
    let ErrorData {
        code,
        message,
        retry,
        phase: _,
        diagnostics,
        limit,
        causes,
    } = error;
    layout::title(out, causes.len().saturating_add(1), ERROR_NOUN, None)?;
    head_line(out, *code, *retry)?;
    layout::entry(out, DETAIL_DEPTH, message)?;
    if let Some(LimitEvidence {
        field,
        limit,
        required,
    }) = limit
    {
        layout::entry(
            out,
            DETAIL_DEPTH,
            &format!("limit {field}: {required} over {limit}"),
        )?;
    }
    for context in diagnostics {
        layout::entry(out, DETAIL_DEPTH, &diagnostic_line(context)?)?;
    }
    if let Some(identity) = identity {
        layout::entry(out, DETAIL_DEPTH, identity)?;
    }
    for ErrorCause {
        code,
        message,
        retry,
    } in causes
    {
        head_line(out, *code, *retry)?;
        layout::entry(out, DETAIL_DEPTH, message)?;
    }
    Ok(())
}

/// Writes the head of one entry: `<code> · retry <directive>`.
fn head_line(
    out: &mut TextWriter,
    code: ErrorCode,
    retry: RetryDirective,
) -> Result<(), TextError> {
    let head = format!(
        "{}{FACT_SEPARATOR}retry {}",
        wire_name(&code)?,
        wire_name(&retry)?
    );
    layout::entry(out, HEAD_DEPTH, &head)
}

/// `severity[code] file:line:column: message`, each part only when the diagnostic has it.
fn diagnostic_line(context: &DiagnosticContext) -> Result<String, TextError> {
    let DiagnosticContext {
        source: _,
        diagnostic,
        line,
        column,
        excerpt: _,
    } = context;
    let Diagnostic {
        severity,
        code,
        message,
        span,
        related: _,
        tags: _,
        reliability: _,
        continuation: _,
        extensions: _,
        language: _,
    } = diagnostic;
    let code = code.as_ref().map(|code| format!("[{code}]"));
    let place = span.as_ref().map(|span| {
        let unit = span.unit.0.as_str();
        let positions: Vec<String> = [line, column]
            .into_iter()
            .flatten()
            .map(u64::to_string)
            .collect();
        let at = if positions.is_empty() {
            String::new()
        } else {
            format!(":{}", positions.join(":"))
        };
        format!(" {}{at}", unit.strip_prefix(FILE_SCHEME).unwrap_or(unit))
    });
    Ok(format!(
        "{}{}{}: {message}",
        wire_name(severity)?,
        code.unwrap_or_default(),
        place.unwrap_or_default()
    ))
}
