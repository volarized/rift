//! Language engines scripted in `sh`, for the walks a real engine answers only in one
//! condition: a seed it prepares no call hierarchy item at, an engine that announces no
//! work, an answer naming bytes the served file does not hold.
//!
//! Each engine reads LSP frames from standard input and answers from the request's
//! method alone, so a suite proves how the server handles that answer without installing
//! a real engine. Every script serves Rust through `[languages.rust.lsp]`.

use serde_json::json;

use crate::workspace_client::{
    ServedWorkspace, TestResult, call_retrying_acceptance, served_workspace, tool_request,
};

/// A `sh` engine serving references and call hierarchy that begins and ends its work at
/// start, so the session reads it ready once quiet, and answers every request `null`: a
/// prepare that gives no item.
pub(crate) const READY_UNPREPARED_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

#[path = "fake_engine/script.rs"]
mod script;
pub(crate) use script::SCRIPTED_CALLS_ENGINE;
/// The `PROGRESS` step of an engine that announces no work, so the session reads it
/// unconfirmed.
pub(crate) const UNANNOUNCED_WORK: &str = ":";

/// The `lib.rs` a scripted calls engine answers about: `beacon` on line 0, and `caller` on
/// lines 2 to 4, calling it on line 3.
const SCRIPTED_CALLS_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\n",
    ),
    (
        "lib.rs",
        "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
    ),
];

/// The lines a scripted calls engine names: the callee's on an outgoing answer, and the
/// call's on an incoming one.
#[derive(Clone, Copy)]
pub(crate) struct ScriptedLines {
    pub(crate) callee: u32,
    pub(crate) caller: u32,
}

/// The lines `lib.rs` holds `beacon` and its call in.
pub(crate) const SERVED_LINES: ScriptedLines = ScriptedLines {
    callee: 0,
    caller: 3,
};

/// Writes `script` into a new directory, and answers that directory, which the caller
/// holds while the engine runs, with the `[languages.rust.lsp]` table running the script.
pub(crate) fn rust_engine(script: &str) -> TestResult<(tempfile::TempDir, String)> {
    let engine = tempfile::tempdir()?;
    let path = engine.path().join("engine.sh");
    std::fs::write(&path, script)?;
    let command = serde_json::to_string(&path)?;
    let table = format!("[languages.rust.lsp]\ncommand = [\"sh\", {command}]\n");
    Ok((engine, table))
}

/// Serves [`SCRIPTED_CALLS_FILES`] under a scripted calls engine that runs `progress` at
/// start and names `lines`, once the first publication answers a search.
pub(crate) async fn scripted_calls_workspace(
    progress: &str,
    lines: ScriptedLines,
) -> TestResult<(tempfile::TempDir, ServedWorkspace)> {
    let (engine, configuration) = rust_engine(
        &SCRIPTED_CALLS_ENGINE
            .replace("PROGRESS", progress)
            .replace("CALLEE_LINE", &lines.callee.to_string())
            .replace("CALLER_LINE", &lines.caller.to_string()),
    )?;
    let served = served_workspace(SCRIPTED_CALLS_FILES, Some(configuration)).await?;
    call_retrying_acceptance(
        &served.1,
        tool_request(
            "search",
            &json!({"query": "ASK { ?symbol a :Symbol; :name 'caller' }"}),
        ),
    )
    .await?;
    Ok((engine, served))
}
