//! Launches each corpus suite through the validating Python MCP client.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The Python suite owns its tighter per-repository deadline and process cleanup.
const RUN_SECONDS_MAX: Duration = Duration::from_mins(19);
/// Python unwinds its owned server and proxy before the outer runner forces exit.
const CLEANUP_SECONDS_MAX: Duration = Duration::from_secs(30);

/// Runs one suite when requested, inheriting the complete test environment.
///
/// In particular, `LLVM_PROFILE_FILE` reaches uv, Python, and the compiled
/// `rift` child. Output goes directly to the test runner, which owns its
/// output budget. The Python harness bounds each server and joins its children.
pub(super) async fn run(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("RIFT_CORPUS_LIVE").is_none() {
        eprintln!("skipped: RIFT_CORPUS_LIVE unset");
        return Ok(());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("scripts/check_corpus.py");
    let report = std::env::var_os("RIFT_CORPUS_REPORT").map_or_else(
        || root.join(format!("target/test-results/corpus/{name}/report.json")),
        PathBuf::from,
    );
    let mut command = tokio::process::Command::new("uv");
    command
        .arg("run")
        .args(["--locked", "--python", "3.12", "--project"])
        .arg(root.join("scripts"))
        .arg("python")
        .arg(script)
        .args(["test", name, "--binary", env!("CARGO_BIN_EXE_rift")])
        .arg("--report")
        .arg(report)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let status = match tokio::time::timeout(RUN_SECONDS_MAX, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            stop_child(&mut child, CLEANUP_SECONDS_MAX).await?;
            return Err(error.into());
        }
        Err(error) => {
            stop_child(&mut child, CLEANUP_SECONDS_MAX).await?;
            return Err(format!("{name} corpus exceeded its process deadline: {error}").into());
        }
    };
    if !status.success() {
        return Err(format!("{name} corpus failed: {status}").into());
    }
    Ok(())
}

/// `uv run` forwards SIGTERM to Python, whose registered handler closes its process owners.
/// Windows taskkill terminates the selected process and its children by PID.
async fn stop_child(
    child: &mut tokio::process::Child,
    cleanup_max: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    let termination = request_termination(child);
    #[cfg(windows)]
    let termination = request_termination(child).await;
    if let Err(error) = termination {
        child.kill().await?;
        return Err(error);
    }
    match tokio::time::timeout(cleanup_max, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            child.kill().await?;
            Err(error.into())
        }
        Err(error) => {
            child.kill().await?;
            Err(format!("corpus cleanup exceeded its deadline: {error}").into())
        }
    }
}

#[cfg(unix)]
fn request_termination(child: &tokio::process::Child) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    if let Some(identity) = child.id() {
        let pid = Pid::from_raw(i32::try_from(identity)?);
        if let Err(error) = kill(pid, Signal::SIGTERM)
            && error != nix::errno::Errno::ESRCH
        {
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn request_termination(
    child: &tokio::process::Child,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(identity) = child.id() {
        let mut cleanup = tokio::process::Command::new("taskkill")
            .args(["/PID", &identity.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let status = match tokio::time::timeout(CLEANUP_SECONDS_MAX, cleanup.wait()).await {
            Ok(status) => status?,
            Err(error) => {
                cleanup.kill().await?;
                return Err(error.into());
            }
        };
        if !status.success() {
            return Err(format!("corpus process-tree termination failed: {status}").into());
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use tokio::io::AsyncReadExt as _;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const TEST_SECONDS_MAX: Duration = Duration::from_secs(2);
    const TEST_CLEANUP_SECONDS_MAX: Duration = Duration::from_millis(100);

    /// The readiness bytes follow trap installation, so no test can signal too early.
    async fn ready_child(script: &str, marker: &Path) -> TestResult<tokio::process::Child> {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", script, "corpus-child"])
            .arg(marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout was not captured"))?;
        let mut ready = [0; 5];
        tokio::time::timeout(TEST_SECONDS_MAX, stdout.read_exact(&mut ready)).await??;
        assert_eq!(&ready, b"ready");
        Ok(child)
    }

    #[tokio::test]
    async fn test_stop_waits_for_cleanup_before_returning() -> TestResult {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("stopped");
        let mut child = ready_child(
            "trap 'printf stopped > \"$1\"; exit 0' TERM; printf ready; while :; do :; done",
            &marker,
        )
        .await?;

        tokio::time::timeout(TEST_SECONDS_MAX, stop_child(&mut child, TEST_SECONDS_MAX)).await??;

        assert_eq!(std::fs::read(&marker)?, b"stopped");
        assert_eq!(child.try_wait()?.and_then(|status| status.code()), Some(0));
        assert_eq!(child.id(), None);
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_deadline_kills_and_reaps_a_child_ignoring_termination() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut child = ready_child(
            "trap '' TERM; printf ready; while :; do :; done",
            &directory.path().join("unused"),
        )
        .await?;

        let error = tokio::time::timeout(
            TEST_SECONDS_MAX,
            stop_child(&mut child, TEST_CLEANUP_SECONDS_MAX),
        )
        .await?
        .expect_err("a child ignoring termination must exceed the cleanup deadline");

        assert!(
            error
                .to_string()
                .contains("corpus cleanup exceeded its deadline")
        );
        assert_eq!(
            child.try_wait()?.and_then(|status| status.signal()),
            Some(nix::sys::signal::Signal::SIGKILL as i32)
        );
        assert_eq!(child.id(), None);
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_accepts_an_already_reaped_child() -> TestResult {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let status = tokio::time::timeout(TEST_SECONDS_MAX, child.wait()).await??;
        assert!(status.success());
        assert_eq!(child.id(), None);

        tokio::time::timeout(
            TEST_SECONDS_MAX,
            stop_child(&mut child, TEST_CLEANUP_SECONDS_MAX),
        )
        .await??;

        assert_eq!(child.try_wait()?, Some(status));
        Ok(())
    }
}
