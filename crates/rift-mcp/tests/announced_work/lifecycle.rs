//! Redacted phases for the announced-work integration fixture.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Reads only the phase file the selected mock child writes. The existing count uses its own log.
pub(crate) struct LifecycleFixtureReceipt(PathBuf);

impl LifecycleFixtureReceipt {
    pub(crate) fn new(log: &Path) -> Self {
        Self(log.with_extension("log.phases"))
    }
}

impl Drop for LifecycleFixtureReceipt {
    fn drop(&mut self) {
        const PHASE_BYTES_MAX: u64 = 16 * 1024;
        let Ok(file) = std::fs::File::open(&self.0) else {
            return;
        };
        let mut phases = Vec::new();
        if file
            .take(PHASE_BYTES_MAX + 1)
            .read_to_end(&mut phases)
            .is_ok()
        {
            let limit = usize::try_from(PHASE_BYTES_MAX).expect("bounded phase receipt");
            let truncated = phases.len() > limit;
            phases.truncate(limit);
            eprintln!(
                "announced work fixture lifecycle (truncated={truncated}):\n{}",
                String::from_utf8_lossy(&phases)
            );
        }
    }
}

const PHASE_WRITER: &str = r#"lifecycle_file="${1}.phases"
lifecycle_count=0
lifecycle_phase() {
  if [ "$lifecycle_count" -lt 256 ]; then
    lifecycle_count=$((lifecycle_count + 1))
    printf 'pid=%s id=%s phase=%s status=%s\n' "$$" "$2" "$1" "$3" >> "$lifecycle_file" || :
  fi
}
lifecycle_phase started "" 0
"#;

const ORIGINAL_FRAME: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }"#;

const TRACED_FRAME: &str = r#"frame() {
  printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"
  lifecycle_frame_status=$?
  case "$1" in
    *'"method":"$/progress"'*'"kind":"begin"'*) lifecycle_phase progress_begin_frame "$id" "$lifecycle_frame_status" ;;
    *'"method":"$/progress"'*'"kind":"end"'*) lifecycle_phase progress_end_frame "$id" "$lifecycle_frame_status" ;;
    *'"capabilities":'*) lifecycle_phase initialize_response_frame "$id" "$lifecycle_frame_status" ;;
    *'"name":"caller"'*) lifecycle_phase prepare_response_frame "$id" "$lifecycle_frame_status" ;;
    *'"name":"beacon"'*) lifecycle_phase outgoing_response_frame "$id" "$lifecycle_frame_status" ;;
    *'"result":null'*) lifecycle_phase empty_response_frame "$id" "$lifecycle_frame_status" ;;
  esac
  return "$lifecycle_frame_status"
}"#;

const TRACE_INBOUND: &str = r#"  case "$body" in
    *'"method":"initialize"'*) lifecycle_phase initialize_received "$id" 0 ;;
    *'"method":"textDocument/didOpen"'*) lifecycle_phase did_open_received "$id" 0 ;;
    *'"method":"textDocument/didClose"'*) lifecycle_phase did_close_received "$id" 0 ;;
    *'"method":"$/cancelRequest"'*) lifecycle_phase cancel_received "$id" 0 ;;
    *'"method":"textDocument/prepareCallHierarchy"'*) lifecycle_phase prepare_received "$id" 0 ;;
    *'"method":"callHierarchy/outgoingCalls"'*) lifecycle_phase outgoing_received "$id" 0 ;;
    *'"method":"shutdown"'*) lifecycle_phase shutdown_received "$id" 0 ;;
    *'"method":"exit"'*) lifecycle_phase exit_received "$id" 0 ;;
  esac
"#;

fn replace_once(script: &mut String, exact: &str, replacement: &str) {
    assert_eq!(
        script.matches(exact).count(),
        1,
        "one authored fixture phase"
    );
    let index = script.find(exact).expect("exact lifecycle fixture phase");
    script.replace_range(index..index + exact.len(), replacement);
}

/// Adds built-in phase appends to the authored mock without sending a new protocol frame.
pub(crate) fn loading_engine_trace(script: &str) -> String {
    let mut result = script.to_owned();
    replace_once(&mut result, ORIGINAL_FRAME, TRACED_FRAME);
    let starts = "echo start >> \"$1\"";
    replace_once(&mut result, starts, &format!("{PHASE_WRITER}{starts}"));
    let dispatch = "  case \"$body\" in";
    replace_once(&mut result, dispatch, &format!("{TRACE_INBOUND}{dispatch}"));
    let delay = "        sleep 2";
    replace_once(
        &mut result,
        delay,
        &format!("        lifecycle_phase initial_prepare_delay \"$id\" 0\n{delay}"),
    );
    result
}
