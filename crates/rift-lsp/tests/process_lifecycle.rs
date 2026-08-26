//! Real-process fixtures for process lifecycle: exit, hang, and crash.
//!
//! Each fixture is a `sh -c` script, not a compiled binary: process
//! lifecycle needs only a process that answers (or refuses to answer) one
//! fixed `initialize` response and then exits, hangs, or lingers. The
//! response bytes are computed from the body's own length, never
//! hand-counted, so the script cannot carry a stale `Content-Length`.

use std::collections::BTreeMap;
use std::time::Duration;

use rift_lsp::session::EngineLaunch;

/// The shell every fixture runs under, resolved through `PATH`.
const SHELL_PROGRAM: &str = "sh";

/// The fixed `initialize` answer every handshake-completing fixture writes,
/// advertising rename so a lifecycle test may drive one request past the
/// handshake before its fixture exits, hangs, or answers no further.
///
/// `EngineSession` allocates ids from zero, and `initialize` is always the
/// first request a fresh session sends, so `id: 0` always matches.
fn initialize_answer_frame() -> String {
    let body = r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"renameProvider":true}}}"#;
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

/// A launch resolving `sh -c script`, with bounds generous enough that a
/// test's own shorter override is what actually times it out.
fn shell_launch(script: String) -> EngineLaunch {
    EngineLaunch {
        program: SHELL_PROGRAM.to_owned(),
        arguments: vec!["-c".to_owned(), script],
        environment: BTreeMap::new(),
        initialization_options: None,
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(10),
        stderr_capture_bytes: 4_096,
    }
}

/// Never answers anything and never reads its stdin: the handshake, and any
/// later write past the pipe buffer, overstays its timeout.
///
/// `exec` replaces the shell's own process image with `sleep` instead of
/// forking it as a child: the launched process and the one a test kills
/// are the same pid, so no orphaned `sleep` survives the kill still
/// holding the pipes open. A plain trailing `sleep N` forks on this
/// shell, and the orphan's inherited stderr write end then keeps
/// `EngineSession::shutdown`'s standard-error drain from ever reading
/// end-of-file.
pub(crate) fn never_responds() -> EngineLaunch {
    shell_launch("exec sleep 999999".to_owned())
}

/// Answers `initialize` and then hangs forever, reading nothing further:
/// the handshake completes, and shutdown, a later request, or a write past
/// the pipe buffer all overstay their timeout. See [`never_responds`] for
/// why the hang is `exec`'d rather than forked.
pub(crate) fn answers_then_hangs() -> EngineLaunch {
    shell_launch(format!(
        "printf '%s' '{}'; exec sleep 999999",
        initialize_answer_frame()
    ))
}

/// Answers `initialize`, lingers briefly to let the handshake's own
/// `initialized` notification land, then exits: the connection closes
/// while the next request is outstanding, or before it is ever sent.
pub(crate) fn answers_then_exits() -> EngineLaunch {
    shell_launch(format!(
        "printf '%s' '{}'; sleep 0.2",
        initialize_answer_frame()
    ))
}

/// The fixed `shutdown` response `answers_shutdown_but_never_exits` writes
/// for request id 1: the id `EngineSession` allocates for its first
/// request after the `initialize` handshake, which is `shutdown` in every
/// test that drives this fixture straight to `EngineSession::shutdown`.
fn shutdown_answer_frame() -> String {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

/// Answers `initialize` and the following `shutdown` request, then never
/// exits: `EngineSession::shutdown` sends `exit`, waits on the child, and
/// must kill it once that wait overstays its timeout. The script never
/// reads its own stdin - the two answers are written on a fixed delay, not
/// in response to what arrives - so nothing here depends on the exact
/// bytes the session writes for `shutdown` or `exit`.
pub(crate) fn answers_shutdown_but_never_exits() -> EngineLaunch {
    shell_launch(format!(
        "printf '%s' '{}'; sleep 0.3; printf '%s' '{}'; exec sleep 999999",
        initialize_answer_frame(),
        shutdown_answer_frame()
    ))
}

/// Answers `initialize`, prints the inherited `HOME` and
/// `RIFT_ENGINE_PROBE` to standard error, then stays reachable until the
/// session ends it.
///
/// A caller reads this fixture's captured standard error through
/// `EngineSession::shutdown`, so the process must survive at least until
/// `initialized` lands, whatever that takes on the runner: a script that
/// instead exits as soon as its own last line runs is only alive for as
/// long as its own steps take, and races the session's next write on a
/// loaded machine. `while read` blocks on standard input without printing
/// anything back: the read is a shell builtin, so no subprocess is forked
/// to leak past a kill, and standard output stays untouched rather than
/// closed, matching [`answers_then_hangs`]. The loop ends, and the process
/// with it, once the session closes its side of standard input or kills
/// it outright.
pub(crate) fn reports_environment() -> EngineLaunch {
    shell_launch(format!(
        "printf '%s' '{}'; echo \"HOME=$HOME\" 1>&2; \
         echo \"RIFT_ENGINE_PROBE=$RIFT_ENGINE_PROBE\" 1>&2; while read -r _line; do :; done",
        initialize_answer_frame()
    ))
}
