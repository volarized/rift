//! `rift steer` - answers one Claude Code `PreToolUse` hook call from stdin.
//!
//! [`decide`] is the sans-I/O decision kernel: a parsed hook call plus
//! probed [`EnvironmentFacts`] in, a typed [`Decision`] out. It denies the
//! first `Grep` or `Glob` call in one Claude Code session, in a workspace
//! Rift indexes and version control tracks, redirecting the agent to the
//! rift search tool; every later call in that session, and every call that
//! does not qualify, answers allow. The rest of this module is the thin
//! shell: it reads stdin bounded, probes the filesystem, and prints the
//! hook's JSON decision. It never fails - every unreadable, malformed, or
//! unexpected condition answers allow, so the hook can never break the
//! agent that triggered it.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use serde::Deserialize;
use serde_json::{Value, json};

/// Bytes of one hook payload steer reads from stdin; a payload past this
/// bound answers allow rather than growing an unbounded buffer.
const HOOK_STDIN_BYTES_MAX: u64 = 1_048_576;

/// UTF-8 bytes of the caller's own pattern embedded in a deny reason; a
/// longer pattern is truncated so the reason itself stays bounded.
const DENY_PATTERN_BYTES_MAX: usize = 256;

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
/// filesystem error along the way all answer [`SteerOutcome::allow`].
#[must_use]
pub(super) fn run() -> SteerOutcome {
    let Some(call) = read_stdin_bounded().and_then(|bytes| parse_hook_call(&bytes)) else {
        return SteerOutcome::allow();
    };
    let Some(tool) = call.qualifying_tool() else {
        return SteerOutcome::allow();
    };
    let session_id = call.validated_session_id();
    let workspace_root = call
        .cwd
        .as_deref()
        .map(Path::new)
        .and_then(discover_workspace_root);
    let environment = probe_environment(workspace_root.as_deref(), session_id);
    let input = KernelInput {
        tool: Some(tool),
        pattern: call.pattern(),
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
    /// The tool this call names, when it is one steer redirects.
    fn qualifying_tool(&self) -> Option<QualifyingTool> {
        self.tool_name
            .as_deref()
            .and_then(QualifyingTool::from_name)
    }

    /// The caller's own pattern: `tool_input.pattern` for both `Grep` and
    /// `Glob`. Empty when the field is missing or not a string.
    fn pattern(&self) -> &str {
        self.tool_input
            .as_ref()
            .and_then(|value| value.get("pattern"))
            .and_then(Value::as_str)
            .unwrap_or_default()
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

/// The one tool steer redirects, and which one names in a deny reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QualifyingTool {
    Grep,
    Glob,
}

impl QualifyingTool {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "Grep" => Some(Self::Grep),
            "Glob" => Some(Self::Glob),
            _ => None,
        }
    }

    /// This tool's name, exactly as Claude Code names it.
    const fn name(self) -> &'static str {
        match self {
            Self::Grep => "Grep",
            Self::Glob => "Glob",
        }
    }
}

/// The hook call, reduced to what the decision kernel needs.
#[derive(Debug)]
struct KernelInput<'a> {
    /// `None` for any tool but a first qualifying `Grep` or `Glob` call.
    tool: Option<QualifyingTool>,
    /// The caller's own search pattern, for the deny reason.
    pattern: &'a str,
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
/// qualifying `Grep` or `Glob` call, in an indexed, version-controlled
/// workspace, with steering on and a validated session id that has not
/// steered yet; every other input answers `Allow`.
fn decide(input: &KernelInput<'_>, environment: EnvironmentFacts) -> Decision {
    let Some(tool) = input.tool else {
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
        reason: deny_reason(tool, input.pattern),
    }
}

/// Renders the deny reason for one qualifying call: what ran, the rift
/// search tool call that answers the same question, and the once-per-session
/// promise. Names only tools and resources the served MCP surface has
/// (`search`, `get_symbol`, `rift://map`) - proven in this module's tests
/// against [`rift_mcp::schema::tool_listing`].
fn deny_reason(tool: QualifyingTool, pattern: &str) -> String {
    let pattern = truncate_pattern(pattern);
    let clause = match tool {
        QualifyingTool::Grep => {
            format!("{{\"query\": \"{pattern}\"}} finds declarations and text,")
        }
        QualifyingTool::Glob => {
            format!("search with paths.include [\"{pattern}\"] finds files by path,")
        }
    };
    let name = tool.name();
    format!(
        "Rift serves this workspace over MCP. Instead of {name}, call the rift search tool: \
         {clause} get_symbol answers a known name, and rift://map orients in an unfamiliar \
         tree. This redirect happens once per session: the same {name} call passes if retried."
    )
}

/// Truncates `pattern` at a UTF-8 char boundary within
/// [`DENY_PATTERN_BYTES_MAX`] bytes, so a caller-supplied pattern cannot
/// grow the deny reason without bound. No character needs escaping here:
/// the whole reason rides inside a JSON string via serde at
/// [`SteerOutcome::deny`].
fn truncate_pattern(pattern: &str) -> Cow<'_, str> {
    if pattern.len() <= DENY_PATTERN_BYTES_MAX {
        return Cow::Borrowed(pattern);
    }
    let mut end = DENY_PATTERN_BYTES_MAX;
    while end > 0 && !pattern.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}...", &pattern[..end]))
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

    use serde_json::json;

    use super::{
        Decision, EnvironmentFacts, KernelInput, QualifyingTool, RIFT_STATE_DIRECTORY,
        STEER_MARKERS_MAX, STEER_STATE_DIRECTORY, WORKSPACE_ROOT_WALK_DEPTH_MAX, claim_marker,
        decide, deny_reason, discover_workspace_root, finalize_denial, is_valid_session_id,
        parse_hook_call, prune_markers, read_steering_disabled, truncate_pattern,
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

    fn grep_input(session_id: Option<&str>) -> KernelInput<'_> {
        KernelInput {
            tool: Some(QualifyingTool::Grep),
            pattern: "TODO",
            session_id,
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
            tool: Some(QualifyingTool::Glob),
            pattern: "**/*.rs",
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
            pattern: "",
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
    fn truncate_pattern_keeps_short_patterns_verbatim() {
        assert_eq!(truncate_pattern("short"), "short");
    }

    #[test]
    fn truncate_pattern_cuts_long_patterns_at_a_char_boundary() {
        let long = "é".repeat(200);
        let truncated = truncate_pattern(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= super::DENY_PATTERN_BYTES_MAX + "...".len());
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_pattern_backs_off_when_the_cut_lands_mid_character() {
        // "€" is 3 bytes; DENY_PATTERN_BYTES_MAX (256) is not a multiple of 3,
        // so the naive cut at byte 256 lands mid-character and must back off
        // to the nearest char boundary at byte 255.
        let long = "€".repeat(100);
        let truncated = truncate_pattern(&long);
        assert!(truncated.ends_with("..."));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        assert_eq!(truncated.len(), 255 + "...".len());
    }

    #[test]
    fn qualifying_tool_from_name_maps_glob() {
        assert_eq!(
            QualifyingTool::from_name("Glob"),
            Some(QualifyingTool::Glob)
        );
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
        let grep_reason = deny_reason(QualifyingTool::Grep, "TODO");
        let glob_reason = deny_reason(QualifyingTool::Glob, "**/*.rs");
        for reason in [&grep_reason, &glob_reason] {
            assert!(reason.contains("search"), "{reason}");
            assert!(reason.contains("get_symbol"), "{reason}");
            assert!(reason.contains("rift://map"), "{reason}");
        }
        assert!(
            grep_reason.contains(r#"{"query": "TODO"}"#),
            "{grep_reason}"
        );
        assert!(
            glob_reason.contains(r#"paths.include ["**/*.rs"]"#),
            "{glob_reason}"
        );
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
