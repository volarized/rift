//! `rift steer` - answers one Claude Code `PreToolUse` hook call from stdin.
//!
//! [`decide`] is the sans-I/O decision kernel: a parsed hook call plus
//! probed [`EnvironmentFacts`] in, a typed [`Decision`] out. It denies the
//! first `Grep` or `Glob` call in one Claude Code session, and the first
//! `Bash` call running `grep`, `rg`, or `find`, in a workspace Rift indexes
//! and version control tracks, redirecting the agent to the rift search
//! tool with the `search` arguments answering the same question; every later
//! call in that session, every call that does not qualify, and every call
//! holding a field `search` has no form for answers allow. The rest of this
//! module is the thin shell: it reads stdin bounded, probes the filesystem,
//! and prints the hook's JSON decision. It never fails - every unreadable,
//! malformed, or unexpected condition answers allow, so the hook can never
//! break the agent that triggered it.

mod bash;
mod suggestion;

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bash::{BashSearch, bash_search};
use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use serde::Deserialize;
use serde_json::{Value, json};
use suggestion::{GrepPath, GrepRequest, find_suggestion, glob_suggestion};

/// Bytes of one hook payload steer reads from stdin; a payload past this
/// bound answers allow rather than growing an unbounded buffer.
const HOOK_STDIN_BYTES_MAX: u64 = 1_048_576;

/// The path a `Grep` call without `path`, and a recursive `grep`, `rg`, or
/// `find` naming none, searches: the hook's working directory.
const CURRENT_DIRECTORY: &str = ".";

/// Highest ASCII bytes in a session id steer accepts, matching the marker
/// filename form `^[A-Za-z0-9_-]{1,128}$`.
const SESSION_ID_BYTES_MAX: usize = 128;

/// Newest per-session marker files kept under `.rift/steer/` after a denial
/// creates one; older markers are pruned.
const STEER_MARKERS_MAX: usize = 64;

/// Directory holding one marker file per steered session, below `.rift`.
const STEER_STATE_DIRECTORY: &str = "steer";

/// Environment variable naming the steering kill switch.
const STEER_ENV_VAR: &str = "RIFT_STEER";

/// Exact `RIFT_STEER` value that disables steering.
const STEER_DISABLE_VALUE: &str = "0";

/// Highest number of parent directories steer climbs from the hook's `cwd`
/// looking for `.rift/`, before giving up and answering allow.
const WORKSPACE_ROOT_WALK_DEPTH_MAX: usize = 64;

/// Runs one `rift steer` invocation: reads the hook payload from stdin,
/// decides, and returns the JSON the hook contract expects on stdout.
///
/// Never fails. Malformed stdin, an unresolvable workspace root, and every
/// filesystem error along the way all answer [`SteerOutcome::allow`]. A
/// `Bash` call running anything but a mappable `grep`, `rg`, or `find`
/// answers allow before any filesystem probe.
#[must_use]
pub(super) fn run() -> SteerOutcome {
    let Some(call) = read_stdin_bounded().and_then(|bytes| parse_hook_call(&bytes)) else {
        return SteerOutcome::allow();
    };
    let Some(tool) = call.qualifying_tool() else {
        return SteerOutcome::allow();
    };
    let session_id = call.validated_session_id();
    let cwd = call.cwd.as_deref().map(Path::new);
    let workspace_root = cwd.and_then(discover_workspace_root);
    let environment = probe_environment(workspace_root.as_deref(), session_id);
    let suggestion = match (workspace_root.as_deref(), cwd) {
        (Some(root), Some(cwd)) => {
            tool.suggestion(&call, &|path: &str| resolve_grep_path(root, cwd, path))
        }
        _ => None,
    };
    let input = KernelInput {
        tool: Some(&tool),
        suggestion,
        session_id,
    };
    match decide(&input, environment) {
        Decision::Allow => SteerOutcome::allow(),
        Decision::Deny { reason } => {
            finalize_denial(workspace_root.as_deref(), session_id, &reason)
        }
    }
}

/// What one `rift steer` invocation prints: the Claude Code `PreToolUse`
/// hook decision, as the exact JSON the hook contract reads from stdout.
#[derive(Debug)]
pub(super) struct SteerOutcome(Value);

impl SteerOutcome {
    /// The tool call passes.
    fn allow() -> Self {
        Self(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
            }
        }))
    }

    /// The tool call is cancelled; `reason` reaches the model.
    fn deny(reason: &str) -> Self {
        Self(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }))
    }
}

impl fmt::Display for SteerOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// One Claude Code `PreToolUse` hook call, the fields steer reads. Every
/// other field the real payload carries (`transcript_path`,
/// `permission_mode`, `hook_event_name`, ...) is ignored.
#[derive(Debug, Deserialize)]
struct HookCall {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
}

impl HookCall {
    /// The call steer redirects, when it is one: a `Grep` or `Glob` call, or
    /// a `Bash` call whose command line [`bash_search`] reads as one.
    fn qualifying_tool(&self) -> Option<QualifyingTool> {
        match self.tool_name.as_deref()? {
            "Grep" => Some(QualifyingTool::Grep),
            "Glob" => Some(QualifyingTool::Glob),
            "Bash" => self
                .input_text("command")
                .and_then(bash_search)
                .map(QualifyingTool::Bash),
            _ => None,
        }
    }

    /// `tool_input.<field>`, when it is a string.
    fn input_text(&self, field: &str) -> Option<&str> {
        self.tool_input
            .as_ref()
            .and_then(|value| value.get(field))
            .and_then(Value::as_str)
    }

    /// This call's session id, validated against the marker filename form.
    /// `None` for a missing or invalid id, so no path is ever built from an
    /// unvalidated one.
    fn validated_session_id(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .filter(|id| is_valid_session_id(id))
    }
}

/// Parses one hook payload. `None` for anything that does not deserialize,
/// so a malformed call answers allow.
fn parse_hook_call(bytes: &[u8]) -> Option<HookCall> {
    serde_json::from_slice(bytes).ok()
}

/// Whether `id` is safe to use as a `.rift/steer/<id>` marker file name.
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SESSION_ID_BYTES_MAX
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The call steer redirects, and which tool a deny reason names.
#[derive(Debug, PartialEq, Eq)]
enum QualifyingTool {
    Grep,
    Glob,
    /// A `Bash` call running `grep`, `rg`, or `find`, answered like the
    /// `Grep` or `Glob` call it equals.
    Bash(BashSearch),
}

impl QualifyingTool {
    /// This tool's name, exactly as Claude Code names it.
    const fn name(&self) -> &'static str {
        match self {
            Self::Grep => "Grep",
            Self::Glob => "Glob",
            Self::Bash(_) => "Bash",
        }
    }

    /// Whether the call searches file text, rather than file names.
    const fn searches_text(&self) -> bool {
        matches!(self, Self::Grep | Self::Bash(BashSearch::Grep { .. }))
    }

    /// The serialized `search` arguments answering this call, `None` when a
    /// part of it has no `search` form. `resolve` classifies one path the
    /// call names against the workspace root.
    fn suggestion(&self, call: &HookCall, resolve: &dyn Fn(&str) -> GrepPath) -> Option<String> {
        match self {
            Self::Grep => {
                let path = call
                    .input_text("path")
                    .filter(|path| !path.is_empty())
                    .unwrap_or(CURRENT_DIRECTORY);
                GrepRequest::from_input(call.tool_input.as_ref()?)?.suggestion(&[resolve(path)])
            }
            Self::Glob => glob_suggestion(call.input_text("pattern")?),
            Self::Bash(BashSearch::Grep {
                request,
                paths,
                recursive,
            }) => {
                let paths: Vec<GrepPath> = paths.iter().map(|path| resolve(path)).collect();
                // grep without `-r` refuses a directory rather than searching it.
                let refused = !recursive && paths.iter().any(GrepPath::is_directory);
                (!refused).then(|| request.suggestion(&paths))?
            }
            Self::Bash(BashSearch::Find { directory, name }) => {
                find_suggestion(&resolve(directory), name)
            }
        }
    }
}

/// The hook call, reduced to what the decision kernel needs.
#[derive(Debug)]
struct KernelInput<'a> {
    /// `None` for any call steer does not redirect.
    tool: Option<&'a QualifyingTool>,
    /// The serialized `search` arguments answering the same question. `None`
    /// when a part of the call has no `search` form, so the call passes.
    suggestion: Option<String>,
    /// This call's session id, already validated against the marker
    /// filename form.
    session_id: Option<&'a str>,
}

/// Environment facts the kernel decides against, probed once by the shell.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independently probed fact the spec names, not a boolean mode \
              parameter"
)]
#[derive(Debug, Clone, Copy)]
struct EnvironmentFacts {
    /// The workspace root holds `.rift/db`.
    index_present: bool,
    /// The workspace root holds `.git`.
    vcs_present: bool,
    /// This session already has a `.rift/steer/<session_id>` marker.
    session_already_steered: bool,
    /// `RIFT_STEER` is exactly `"0"`.
    steering_disabled: bool,
}

/// The kernel's answer for one hook call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Allow,
    Deny { reason: String },
}

/// Decides one `PreToolUse` call.
///
/// Sans-I/O: every fact this needs travels in `input` and `environment`, so
/// the decision is a pure function of its arguments. Denies only a first
/// qualifying call whose every part maps to `search`, in an indexed,
/// version-controlled workspace, with steering on and a validated session id
/// that has not steered yet; every other input answers `Allow`.
fn decide(input: &KernelInput<'_>, environment: EnvironmentFacts) -> Decision {
    let (Some(tool), Some(suggestion)) = (input.tool, input.suggestion.as_deref()) else {
        return Decision::Allow;
    };
    if input.session_id.is_none()
        || environment.steering_disabled
        || !environment.index_present
        || !environment.vcs_present
        || environment.session_already_steered
    {
        return Decision::Allow;
    }
    Decision::Deny {
        reason: deny_reason(tool, suggestion),
    }
}

/// Renders the deny reason for one qualifying call: what ran, the rift
/// search tool call that answers the same question, and the once-per-session
/// promise. Names only tools and resources the served MCP surface has
/// (`search`, `get_symbol`, `rift://map`) - proven in this module's tests
/// against [`rift_mcp::schema::tool_listing`].
fn deny_reason(tool: &QualifyingTool, suggestion: &str) -> String {
    let clause = if tool.searches_text() {
        format!("{suggestion} finds declarations and text,")
    } else {
        format!("search with paths.include {suggestion} finds files by path,")
    };
    let name = tool.name();
    format!(
        "Rift serves this workspace over MCP. Instead of {name}, call the rift search tool: \
         {clause} get_symbol answers a known name, and rift://map orients in an unfamiliar \
         tree. This redirect happens once per session: the same {name} call passes if retried."
    )
}

/// Classifies one path a call names against the workspace root, relative to
/// the hook's `cwd` unless absolute. A path holding a glob character is left
/// unmapped, since `paths.include` would read it as a glob.
fn resolve_grep_path(root: &Path, cwd: &Path, path: &str) -> GrepPath {
    let (Ok(absolute), Ok(root)) = (cwd.join(path).canonicalize(), root.canonicalize()) else {
        return GrepPath::Unmapped;
    };
    let Some(relative) = absolute
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .map(|relative| relative.replace('\\', "/"))
    else {
        return GrepPath::Unmapped;
    };
    if relative.is_empty() {
        GrepPath::Root
    } else if relative.contains(['*', '?', '[', ']', '{', '}', '!']) {
        GrepPath::Unmapped
    } else if absolute.is_dir() {
        GrepPath::Directory(relative)
    } else if absolute.is_file() {
        GrepPath::File(relative)
    } else {
        GrepPath::Unmapped
    }
}

/// Reads at most [`HOOK_STDIN_BYTES_MAX`] bytes of stdin. `None` for a read
/// failure or a payload past the bound.
fn read_stdin_bounded() -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    io::stdin()
        .lock()
        .take(HOOK_STDIN_BYTES_MAX + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() as u64 > HOOK_STDIN_BYTES_MAX {
        return None;
    }
    Some(buffer)
}

/// Walks up from `start` (inclusive) for the nearest directory holding
/// `.rift/`, bounded to [`WORKSPACE_ROOT_WALK_DEPTH_MAX`] climbs. `rift mcp`
/// and `rift server` serve their root as the literal current directory and
/// carry no walk-up of their own to reuse; this is steer's own, because the
/// hook's `cwd` may be a subdirectory of the served workspace.
fn discover_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut candidate = start.to_path_buf();
    for _ in 0..WORKSPACE_ROOT_WALK_DEPTH_MAX {
        if candidate.join(RIFT_STATE_DIRECTORY).exists() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
    None
}

/// Probes every environment fact the kernel needs. `workspace_root: None`
/// (no `.rift/` found within the walk bound) answers every filesystem fact
/// false, which already routes [`decide`] to `Allow`.
fn probe_environment(workspace_root: Option<&Path>, session_id: Option<&str>) -> EnvironmentFacts {
    let steering_disabled = read_steering_disabled(&|name| std::env::var(name).ok());
    let Some(root) = workspace_root else {
        return EnvironmentFacts {
            index_present: false,
            vcs_present: false,
            session_already_steered: false,
            steering_disabled,
        };
    };
    EnvironmentFacts {
        index_present: root
            .join(RIFT_STATE_DIRECTORY)
            .join(WORKSPACE_DATABASE_FILE_NAME)
            .exists(),
        vcs_present: root.join(".git").exists(),
        session_already_steered: session_id.is_some_and(|id| marker_path(root, id).exists()),
        steering_disabled,
    }
}

/// Whether `RIFT_STEER` is exactly [`STEER_DISABLE_VALUE`]. The lookup is
/// injected so a test exercises this without mutating the process
/// environment.
fn read_steering_disabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    lookup(STEER_ENV_VAR).as_deref() == Some(STEER_DISABLE_VALUE)
}

/// This session's marker path, below the workspace root.
fn marker_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(RIFT_STATE_DIRECTORY)
        .join(STEER_STATE_DIRECTORY)
        .join(session_id)
}

/// Turns a kernel `Deny` into the printed outcome: claims this session's
/// marker atomically, and only the caller that wins the race denies. A
/// concurrent duplicate hook call - both probing `session_already_steered:
/// false` before either has written the marker - loses the race here and
/// falls back to allow, so at most one call per session ever denies.
fn finalize_denial(root: Option<&Path>, session_id: Option<&str>, reason: &str) -> SteerOutcome {
    let (Some(root), Some(session_id)) = (root, session_id) else {
        return SteerOutcome::allow();
    };
    match claim_marker(root, session_id) {
        Ok(true) => {
            prune_markers(root);
            SteerOutcome::deny(reason)
        }
        Ok(false) | Err(_) => SteerOutcome::allow(),
    }
}

/// Atomically claims this session's marker file. `Ok(true)` means this call
/// created it and therefore owns the denial; `Ok(false)` means a concurrent
/// call already claimed it first.
fn claim_marker(root: &Path, session_id: &str) -> io::Result<bool> {
    let directory = root.join(RIFT_STATE_DIRECTORY).join(STEER_STATE_DIRECTORY);
    fs::create_dir_all(&directory)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(session_id))
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Keeps only the newest [`STEER_MARKERS_MAX`] session markers under
/// `.rift/steer/`, deleting older ones by modification time; a tie in
/// modification time breaks by file name so the prune order never depends
/// on `read_dir`'s enumeration order. Best-effort: a failure to list or
/// remove a marker is swallowed, since pruning must never turn a successful
/// denial into a failed one.
fn prune_markers(root: &Path) {
    let directory = root.join(RIFT_STATE_DIRECTORY).join(STEER_STATE_DIRECTORY);
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    let mut markers: Vec<(PathBuf, SystemTime, OsString)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            let name = entry.file_name();
            Some((entry.path(), modified, name))
        })
        .collect();
    if markers.len() <= STEER_MARKERS_MAX {
        return;
    }
    markers.sort_by(
        |(_, left_modified, left_name), (_, right_modified, right_name)| {
            left_modified
                .cmp(right_modified)
                .then_with(|| left_name.cmp(right_name))
        },
    );
    let excess = markers.len() - STEER_MARKERS_MAX;
    for (path, ..) in markers.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use serde_json::{Value, json};

    use super::suggestion::GrepPath;
    use super::suggestion::tests::served_search_call;
    use super::{
        Decision, EnvironmentFacts, HookCall, KernelInput, QualifyingTool, RIFT_STATE_DIRECTORY,
        STEER_MARKERS_MAX, STEER_STATE_DIRECTORY, WORKSPACE_ROOT_WALK_DEPTH_MAX, claim_marker,
        decide, deny_reason, discover_workspace_root, finalize_denial, is_valid_session_id,
        parse_hook_call, prune_markers, read_steering_disabled, resolve_grep_path,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Every fact set so a qualifying call denies; each test flips one.
    fn qualifying_environment() -> EnvironmentFacts {
        EnvironmentFacts {
            index_present: true,
            vcs_present: true,
            session_already_steered: false,
            steering_disabled: false,
        }
    }

    /// A `Grep` call for `TODO` whose suggestion mapped.
    fn grep_input(session_id: Option<&str>) -> KernelInput<'_> {
        KernelInput {
            tool: Some(&QualifyingTool::Grep),
            suggestion: Some(r#"{"pattern":"TODO","target":"file"}"#.to_owned()),
            session_id,
        }
    }

    /// One hook call naming `tool_name` with `tool_input`.
    fn hook_call(tool_name: &str, tool_input: &Value) -> HookCall {
        let payload = json!({"tool_name": tool_name, "tool_input": tool_input}).to_string();
        parse_hook_call(payload.as_bytes()).expect("the payload parses")
    }

    /// The suggestion one hook call maps to, its paths classified by
    /// `resolve`, proven a served `search` call when it is a text search.
    fn suggested(call: &HookCall, resolve: &dyn Fn(&str) -> GrepPath) -> Option<String> {
        let tool = call.qualifying_tool()?;
        let suggestion = tool.suggestion(call, resolve)?;
        if tool.searches_text() {
            served_search_call(&suggestion);
        }
        Some(suggestion)
    }

    /// Classifies paths the way a workspace holding `src/` and
    /// `src/lib.rs` would.
    fn fixture_path(path: &str) -> GrepPath {
        match path.trim_end_matches('/') {
            "." => GrepPath::Root,
            "src" => GrepPath::Directory("src".to_owned()),
            "src/lib.rs" => GrepPath::File("src/lib.rs".to_owned()),
            _ => GrepPath::Unmapped,
        }
    }

    #[test]
    fn a_first_qualifying_grep_call_denies() {
        let decision = decide(&grep_input(Some("session-alpha")), qualifying_environment());
        let Decision::Deny { reason } = decision else {
            panic!("a first qualifying call must deny: {decision:?}");
        };
        assert!(reason.contains("Grep"));
        assert!(reason.contains("TODO"));
    }

    #[test]
    fn a_first_qualifying_glob_call_denies() {
        let input = KernelInput {
            tool: Some(&QualifyingTool::Glob),
            suggestion: Some(r#"["**/*.rs"]"#.to_owned()),
            session_id: Some("session-alpha"),
        };
        let decision = decide(&input, qualifying_environment());
        let Decision::Deny { reason } = decision else {
            panic!("a first qualifying call must deny: {decision:?}");
        };
        assert!(reason.contains("Glob"));
        assert!(reason.contains("**/*.rs"));
    }

    #[test]
    fn a_non_qualifying_tool_answers_allow() {
        let input = KernelInput {
            tool: None,
            suggestion: None,
            session_id: Some("session-alpha"),
        };
        assert_eq!(decide(&input, qualifying_environment()), Decision::Allow);
    }

    #[test]
    fn a_call_with_no_suggestion_answers_allow() {
        let input = KernelInput {
            tool: Some(&QualifyingTool::Grep),
            suggestion: None,
            session_id: Some("session-alpha"),
        };
        assert_eq!(decide(&input, qualifying_environment()), Decision::Allow);
    }

    #[test]
    fn no_index_answers_allow() {
        let environment = EnvironmentFacts {
            index_present: false,
            ..qualifying_environment()
        };
        assert_eq!(
            decide(&grep_input(Some("session-alpha")), environment),
            Decision::Allow
        );
    }

    #[test]
    fn no_vcs_answers_allow() {
        let environment = EnvironmentFacts {
            vcs_present: false,
            ..qualifying_environment()
        };
        assert_eq!(
            decide(&grep_input(Some("session-alpha")), environment),
            Decision::Allow
        );
    }

    #[test]
    fn the_kill_switch_answers_allow() {
        let environment = EnvironmentFacts {
            steering_disabled: true,
            ..qualifying_environment()
        };
        assert_eq!(
            decide(&grep_input(Some("session-alpha")), environment),
            Decision::Allow
        );
    }

    #[test]
    fn an_already_steered_session_answers_allow() {
        let environment = EnvironmentFacts {
            session_already_steered: true,
            ..qualifying_environment()
        };
        assert_eq!(
            decide(&grep_input(Some("session-alpha")), environment),
            Decision::Allow
        );
    }

    #[test]
    fn a_missing_or_invalid_session_id_answers_allow() {
        assert_eq!(
            decide(&grep_input(None), qualifying_environment()),
            Decision::Allow
        );
    }

    #[test]
    fn malformed_stdin_parses_to_nothing() {
        assert!(parse_hook_call(b"not json").is_none());
        assert!(parse_hook_call(b"").is_none());
        assert!(parse_hook_call(br#"{"tool_name": "Grep"}"#).is_some());
    }

    #[test]
    fn session_id_validation_matches_the_marker_filename_form() {
        assert!(is_valid_session_id("abc123_-XYZ"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("has space"));
        assert!(!is_valid_session_id("has/slash"));
        assert!(!is_valid_session_id(&"a".repeat(129)));
        assert!(is_valid_session_id(&"a".repeat(128)));
    }

    #[test]
    fn steering_disabled_matches_only_the_exact_value_zero() {
        assert!(read_steering_disabled(&|_| Some("0".to_owned())));
        assert!(!read_steering_disabled(&|_| Some("false".to_owned())));
        assert!(!read_steering_disabled(&|_| Some(String::new())));
        assert!(!read_steering_disabled(&|_| None));
    }

    #[test]
    fn qualifying_tools_are_grep_glob_and_mapped_bash_commands() {
        let grep = hook_call("Grep", &json!({"pattern": "x"})).qualifying_tool();
        assert_eq!(grep, Some(QualifyingTool::Grep));
        let glob = hook_call("Glob", &json!({"pattern": "*.rs"})).qualifying_tool();
        assert_eq!(glob, Some(QualifyingTool::Glob));
        let bash = hook_call("Bash", &json!({"command": "grep -rn x ."})).qualifying_tool();
        assert_eq!(bash.as_ref().map(QualifyingTool::name), Some("Bash"));
        for (tool_name, tool_input) in [
            ("Bash", json!({"command": "grep -rn x . | head"})),
            ("Bash", json!({"command": "cargo test"})),
            ("Bash", json!({"description": "no command"})),
            ("Read", json!({"file_path": "src/lib.rs"})),
        ] {
            let call = hook_call(tool_name, &tool_input);
            assert_eq!(call.qualifying_tool(), None, "{tool_name} {tool_input}");
        }
        assert_eq!(
            parse_hook_call(b"{}")
                .expect("an empty payload parses")
                .qualifying_tool(),
            None
        );
    }

    #[test]
    fn a_grep_call_without_a_path_searches_the_hook_directory() {
        let call = hook_call("Grep", &json!({"pattern": "fn", "output_mode": "content"}));
        let asked = std::cell::RefCell::new(Vec::new());
        let resolve = |path: &str| {
            asked.borrow_mut().push(path.to_owned());
            GrepPath::Directory("crates".to_owned())
        };
        assert_eq!(
            suggested(&call, &resolve).as_deref(),
            Some(r#"{"pattern":"fn","paths":{"include":["crates/**"]}}"#)
        );
        let call = hook_call("Grep", &json!({"pattern": "fn", "path": "src"}));
        assert_eq!(
            suggested(&call, &resolve).as_deref(),
            Some(r#"{"pattern":"fn","paths":{"include":["crates/**"]},"target":"file"}"#)
        );
        let call = hook_call("Grep", &json!({"pattern": "fn", "path": ""}));
        assert!(suggested(&call, &resolve).is_some());
        assert_eq!(*asked.borrow(), vec![".", "src", "."]);
        let call = hook_call("Grep", &json!({"pattern": "fn", "-C": 2}));
        assert_eq!(suggested(&call, &resolve), None);
    }

    #[test]
    fn a_glob_call_and_a_find_command_select_files_by_path() {
        let glob = hook_call("Glob", &json!({"pattern": "**/*.rs"}));
        assert_eq!(
            suggested(&glob, &fixture_path).as_deref(),
            Some(r#"["**/*.rs"]"#)
        );
        let absolute = hook_call("Glob", &json!({"pattern": "/etc/*.conf"}));
        assert_eq!(suggested(&absolute, &fixture_path), None);
        let find = hook_call("Bash", &json!({"command": "find src -name '*.rs'"}));
        assert_eq!(
            suggested(&find, &fixture_path).as_deref(),
            Some(r#"["src/**/*.rs"]"#)
        );
        let outside = hook_call("Bash", &json!({"command": "find /etc -name '*.conf'"}));
        assert_eq!(suggested(&outside, &fixture_path), None);
    }

    #[test]
    fn bash_grep_commands_map_to_valid_search_calls() {
        let cases = [
            (
                r#"grep -rn "cfg(test)" src/ ."#,
                Some(json!({"pattern": r"cfg\(test\)"})),
            ),
            (
                r#"grep -rn "cfg(test)" src/"#,
                Some(json!({"pattern": r"cfg\(test\)", "paths": {"include": ["src/**"]}})),
            ),
            (
                r#"grep -n "fn main" src/lib.rs"#,
                Some(json!({"pattern": "fn main", "paths": {"include": ["src/lib.rs"]}})),
            ),
            (
                r#"grep -rl --include="*.rs" "TODO""#,
                Some(json!({"pattern": "TODO", "paths": {"include": ["*.rs"]}, "target": "file"})),
            ),
            (
                r#"rg -iw "say\(\"hi\"\)" src"#,
                Some(json!({
                    "pattern": r#"(?i)\b{start-half}(?:say\("hi"\))\b{end-half}"#,
                    "paths": {"include": ["src/**"]}
                })),
            ),
            (r#"grep -n "fn main" src"#, None),
            (r#"grep -rn "x" missing"#, None),
            (r"rg -g '!target' x", None),
            (r"rg -g '*.rs' -t rust x", None),
            (r"rg -t rs x", None),
            (r#"rg "tokenize(" src"#, None),
        ];
        for (command, expected) in cases {
            let call = hook_call("Bash", &json!({"command": command}));
            let suggestion = suggested(&call, &fixture_path);
            let arguments = suggestion
                .map(|suggestion| serde_json::from_str::<Value>(&suggestion))
                .transpose()
                .expect("a suggestion is JSON");
            assert_eq!(arguments, expected, "{command}");
        }
    }

    #[test]
    fn deny_reason_names_only_tools_the_served_surface_has() {
        let tools = rift_mcp::schema::tool_listing();
        for name in ["search", "get_symbol"] {
            assert!(
                tools.iter().any(|tool| tool.name.as_ref() == name),
                "the served surface must carry {name}"
            );
        }
        let grep_reason = deny_reason(&QualifyingTool::Grep, r#"{"pattern":"TODO"}"#);
        let glob_reason = deny_reason(&QualifyingTool::Glob, r#"["**/*.rs"]"#);
        for reason in [&grep_reason, &glob_reason] {
            assert!(reason.contains("search"), "{reason}");
            assert!(reason.contains("get_symbol"), "{reason}");
            assert!(reason.contains("rift://map"), "{reason}");
        }
        assert!(
            grep_reason.contains(r#"tool: {"pattern":"TODO"} finds"#),
            "{grep_reason}"
        );
        assert!(
            glob_reason.contains(r#"paths.include ["**/*.rs"]"#),
            "{glob_reason}"
        );
    }

    #[test]
    fn a_bash_deny_names_bash_and_its_suggested_call() {
        let call = hook_call("Bash", &json!({"command": r#"grep -rn "a\"b" ."#}));
        let tool = call.qualifying_tool().expect("the command maps");
        let suggestion = tool.suggestion(&call, &fixture_path);
        let input = KernelInput {
            tool: Some(&tool),
            suggestion,
            session_id: Some("session-alpha"),
        };
        let Decision::Deny { reason } = decide(&input, qualifying_environment()) else {
            panic!("a mapped Bash grep must deny");
        };
        assert!(reason.contains("Instead of Bash"), "{reason}");
        assert!(reason.contains("the same Bash call passes"), "{reason}");
        let call = reason
            .split("tool: ")
            .nth(1)
            .and_then(|rest| rest.split(" finds").next())
            .expect("the reason carries the call");
        assert_eq!(served_search_call(call), json!({"pattern": r#"a"b"#}));
    }

    #[test]
    fn resolve_grep_path_classifies_against_the_root() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::create_dir_all(root.join("src").join("inner"))?;
        fs::write(root.join("src").join("lib.rs"), b"")?;
        fs::create_dir(root.join("odd[name]"))?;
        let outside = tempfile::tempdir()?;
        assert_eq!(resolve_grep_path(root, root, "."), GrepPath::Root);
        assert_eq!(
            resolve_grep_path(root, &root.join("src"), ".."),
            GrepPath::Root
        );
        let inner = root.join("src").join("inner");
        assert_eq!(
            resolve_grep_path(root, root, inner.to_str().ok_or("utf-8 path")?),
            GrepPath::Directory("src/inner".to_owned())
        );
        assert_eq!(
            resolve_grep_path(root, &root.join("src"), "lib.rs"),
            GrepPath::File("src/lib.rs".to_owned())
        );
        assert_eq!(resolve_grep_path(root, root, "missing"), GrepPath::Unmapped);
        assert_eq!(
            resolve_grep_path(root, root, "odd[name]"),
            GrepPath::Unmapped
        );
        assert_eq!(
            resolve_grep_path(root, root, outside.path().to_str().ok_or("utf-8 path")?),
            GrepPath::Unmapped
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resolve_grep_path_reads_a_path_through_a_symlinked_root() -> TestResult {
        let directory = tempfile::tempdir()?;
        let real = directory.path().join("real");
        fs::create_dir_all(real.join("src"))?;
        let linked = directory.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked)?;
        assert_eq!(
            resolve_grep_path(&linked, &real, "src"),
            GrepPath::Directory("src".to_owned())
        );
        assert_eq!(resolve_grep_path(&real, &linked, "."), GrepPath::Root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resolve_grep_path_leaves_a_special_file_unmapped() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let socket = root.join("hook.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
        assert_eq!(
            resolve_grep_path(root, root, "hook.sock"),
            GrepPath::Unmapped
        );
        Ok(())
    }

    #[test]
    fn discover_workspace_root_walks_up_to_the_nearest_rift_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::create_dir(root.join(RIFT_STATE_DIRECTORY))?;
        let nested = root.join("crates").join("inner");
        fs::create_dir_all(&nested)?;
        assert_eq!(discover_workspace_root(&nested), Some(root.to_path_buf()));
        assert_eq!(discover_workspace_root(root), Some(root.to_path_buf()));
        Ok(())
    }

    #[test]
    fn discover_workspace_root_answers_none_when_no_ancestor_has_rift() -> TestResult {
        let directory = tempfile::tempdir()?;
        assert_eq!(discover_workspace_root(directory.path()), None);
        Ok(())
    }

    #[test]
    fn discover_workspace_root_answers_none_when_the_climb_bound_is_exhausted() -> TestResult {
        let directory = tempfile::tempdir()?;
        // Nest well past the climb bound so every parent within the bound
        // exists (no early `parent()` exhaustion) and none carries `.rift`.
        let mut nested = directory.path().to_path_buf();
        for index in 0..(WORKSPACE_ROOT_WALK_DEPTH_MAX + 4) {
            nested = nested.join(format!("d{index}"));
        }
        fs::create_dir_all(&nested)?;
        assert_eq!(discover_workspace_root(&nested), None);
        Ok(())
    }

    #[test]
    fn finalize_denial_allows_when_the_workspace_root_is_missing() {
        let outcome = finalize_denial(None, Some("session-x"), "reason");
        assert_eq!(
            outcome.0["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
    }

    #[test]
    fn finalize_denial_allows_when_the_session_id_is_missing() {
        let outcome = finalize_denial(Some(Path::new("/does-not-matter")), None, "reason");
        assert_eq!(
            outcome.0["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
    }

    #[test]
    fn finalize_denial_allows_when_a_concurrent_call_already_claimed_the_marker() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        // Simulate the race: a concurrent call already created the marker
        // before this call reaches `finalize_denial`.
        assert!(claim_marker(root, "session-race")?);

        let outcome = finalize_denial(Some(root), Some("session-race"), "reason");
        assert_eq!(
            outcome.0["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
        Ok(())
    }

    #[test]
    fn finalize_denial_allows_when_claiming_the_marker_fails() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        // A regular file where `.rift` must be a directory blocks marker
        // creation with an io error.
        fs::write(root.join(RIFT_STATE_DIRECTORY), b"not a directory")?;

        let outcome = finalize_denial(Some(root), Some("session-blocked"), "reason");
        assert_eq!(
            outcome.0["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
        Ok(())
    }

    #[test]
    fn claim_marker_propagates_an_open_failure_that_is_not_already_exists() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        // The marker's parent directory does not exist, so the open call
        // fails with a real error distinct from `AlreadyExists`.
        let result = claim_marker(root, "missing-parent/marker");
        assert!(
            result.is_err(),
            "opening under a missing directory must fail: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn prune_markers_is_a_no_op_when_the_steer_directory_does_not_exist() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        prune_markers(root);
        assert!(!root.join(RIFT_STATE_DIRECTORY).exists());
        Ok(())
    }

    #[test]
    fn claim_marker_creates_once_and_reports_the_race_loser() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        assert!(claim_marker(root, "session-1")?);
        assert!(!claim_marker(root, "session-1")?);
        assert!(
            root.join(RIFT_STATE_DIRECTORY)
                .join(STEER_STATE_DIRECTORY)
                .join("session-1")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn prune_markers_keeps_only_the_newest_sixty_four() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let steer_directory = root.join(RIFT_STATE_DIRECTORY).join(STEER_STATE_DIRECTORY);
        fs::create_dir_all(&steer_directory)?;
        let base = SystemTime::now() - Duration::from_secs(1_000);
        for index in 0..65_u64 {
            let path = steer_directory.join(format!("session-{index:03}"));
            let file = fs::File::create(&path)?;
            file.set_modified(base + Duration::from_secs(index))?;
        }
        prune_markers(root);
        let remaining: Vec<String> = fs::read_dir(&steer_directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 64);
        assert!(
            !remaining.contains(&"session-000".to_owned()),
            "the oldest marker must be pruned: {remaining:?}"
        );
        assert!(remaining.contains(&"session-064".to_owned()));
        Ok(())
    }

    #[test]
    fn prune_markers_breaks_a_modification_time_tie_by_file_name() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let steer_directory = root.join(RIFT_STATE_DIRECTORY).join(STEER_STATE_DIRECTORY);
        fs::create_dir_all(&steer_directory)?;
        let base = SystemTime::now() - Duration::from_secs(1_000);
        for name in ["session-b", "session-a"] {
            let file = fs::File::create(steer_directory.join(name))?;
            file.set_modified(base)?;
        }
        for index in 0..(STEER_MARKERS_MAX as u64 - 1) {
            let path = steer_directory.join(format!("session-newer-{index:03}"));
            let file = fs::File::create(&path)?;
            file.set_modified(base + Duration::from_secs(index + 1))?;
        }
        prune_markers(root);
        let remaining: Vec<String> = fs::read_dir(&steer_directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), STEER_MARKERS_MAX);
        assert!(
            !remaining.contains(&"session-a".to_owned()),
            "the lexicographically-first marker tied on modification time must be pruned: \
             {remaining:?}"
        );
        assert!(
            remaining.contains(&"session-b".to_owned()),
            "the lexicographically-later marker tied on modification time must survive: \
             {remaining:?}"
        );
        Ok(())
    }
}
