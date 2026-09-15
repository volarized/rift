//! Launches each corpus suite through the validating Python MCP client.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The Python suite owns its tighter per-repository deadline and process cleanup.
const RUN_SECONDS_MAX: Duration = Duration::from_mins(19);
/// Python unwinds its owned server and proxy before the outer runner forces exit.
const CLEANUP_SECONDS_MAX: Duration = Duration::from_secs(30);

/// Runs one explicitly selected suite, inheriting the complete test environment.
///
/// In particular, `LLVM_PROFILE_FILE` reaches uv, Python, and the compiled
/// `rift` child. Output goes directly to the test runner, which owns its
/// output budget. The Python harness bounds each server and joins its children.
pub(super) async fn run(name: &str, case: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (root, binary) = runtime_paths(|name| std::env::var_os(name))?;
    let script = root.join("scripts/check_corpus.py");
    let supplied_report = std::env::var_os("RIFT_CORPUS_REPORT").map(PathBuf::from);
    let report = report_path(&root, name, case, supplied_report.as_deref())?;
    let mut command = tokio::process::Command::new("uv");
    command
        .arg("run")
        .args(["--locked", "--python", "3.12", "--project"])
        .arg(root.join("scripts"))
        .arg("python")
        .arg(script)
        .args(["test", name, "--binary"])
        .arg(binary)
        .args(["--case", case])
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

/// Nextest remaps runtime Cargo paths when extracting an archive into another checkout.
fn runtime_paths(
    environment: impl Fn(&str) -> Option<OsString>,
) -> Result<(PathBuf, PathBuf), std::io::Error> {
    let path = |name: &str| {
        environment(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other(format!("corpus requires runtime {name}")))
    };
    Ok((
        path("CARGO_MANIFEST_DIR")?.join("../.."),
        path("CARGO_BIN_EXE_rift")?,
    ))
}

/// Bun cases add their name before the extension so one environment cannot overwrite either report.
fn report_path(
    root: &Path,
    name: &str,
    case: &str,
    supplied: Option<&Path>,
) -> Result<PathBuf, std::io::Error> {
    let Some(path) = supplied else {
        return Ok(root.join(format!(
            "target/test-results/corpus/{name}/{case}/report.json"
        )));
    };
    if name != "bun" {
        return Ok(path.to_path_buf());
    }
    let mut filename = path
        .file_stem()
        .ok_or_else(|| std::io::Error::other("corpus report path must name a file"))?
        .to_os_string();
    filename.push(".");
    filename.push(case);
    if let Some(extension) = path.extension() {
        filename.push(".");
        filename.push(extension);
    }
    Ok(path.with_file_name(filename))
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

#[cfg(test)]
mod report_tests {
    use super::{report_path, runtime_paths};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_runtime_paths_use_the_relocated_checkout_and_binary() -> Result<(), std::io::Error> {
        let (root, binary) = runtime_paths(|name| match name {
            "CARGO_MANIFEST_DIR" => Some(OsString::from("relocated/source/crates/rift")),
            "CARGO_BIN_EXE_rift" => Some(OsString::from("extracted/target/corpus/rift")),
            _ => None,
        })?;
        assert_eq!(root, PathBuf::from("relocated/source/crates/rift/../.."));
        assert_eq!(binary, PathBuf::from("extracted/target/corpus/rift"));
        Ok(())
    }

    #[test]
    fn test_runtime_paths_refuse_missing_or_empty_values() {
        for missing in ["CARGO_MANIFEST_DIR", "CARGO_BIN_EXE_rift"] {
            for value in [None, Some(OsString::new())] {
                let error = runtime_paths(|name| {
                    if name == missing {
                        value.clone()
                    } else {
                        Some(OsString::from("present"))
                    }
                })
                .expect_err("runtime paths must be present before a corpus can start");
                assert_eq!(
                    error.to_string(),
                    format!("corpus requires runtime {missing}")
                );
            }
        }
    }

    #[test]
    fn test_bun_cases_keep_both_supplied_reports() -> Result<(), std::io::Error> {
        let root = Path::new("workspace");
        let supplied = Some(Path::new("reports/release.json"));
        assert_eq!(
            report_path(root, "bun", "workspace", supplied)?,
            PathBuf::from("reports/release.workspace.json")
        );
        assert_eq!(
            report_path(root, "bun", "stop", supplied)?,
            PathBuf::from("reports/release.stop.json")
        );
        assert_eq!(
            report_path(root, "bun", "stop", Some(Path::new("report")))?,
            PathBuf::from("report.stop")
        );
        Ok(())
    }

    #[test]
    fn test_other_repositories_keep_exact_report_path() -> Result<(), std::io::Error> {
        let supplied = Path::new("reports/exact.json");
        for name in ["fastapi", "nextjs"] {
            assert_eq!(
                report_path(Path::new("workspace"), name, "workspace", Some(supplied))?,
                supplied
            );
        }
        Ok(())
    }

    #[test]
    fn test_default_report_path_separates_cases() -> Result<(), std::io::Error> {
        for case in ["workspace", "stop"] {
            assert_eq!(
                report_path(Path::new("workspace"), "bun", case, None)?,
                PathBuf::from(format!(
                    "workspace/target/test-results/corpus/bun/{case}/report.json"
                ))
            );
        }
        Ok(())
    }

    #[test]
    fn test_bun_report_path_requires_filename() {
        assert!(report_path(Path::new("workspace"), "bun", "stop", Some(Path::new(""))).is_err());
    }
}
