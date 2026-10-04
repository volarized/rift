//! Detached process creation with explicit standard streams.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

#[cfg(not(windows))]
pub(super) use std::process::{Child, Command, Stdio};
#[cfg(windows)]
pub(super) use windows_spawn::{Child, Command, Stdio};

/// Builds a detached command inheriting this process's environment.
///
/// Explicit environment values override inherited values. Stdin and stdout are null;
/// the caller selects stderr before spawning. Windows creation transfers only the
/// selected standard streams, avoiding retained caller output pipes while the server runs.
pub(super) fn detached_command_for(
    program: impl AsRef<OsStr>,
    arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
    root: &Path,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
}

/// Spawns the command in its own process group, preserving selected streams.
#[cfg(not(windows))]
pub(super) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Spawns with only selected streams inherited, outside the caller's console.
#[cfg(windows)]
pub(super) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use windows_spawn::{CreationFlags, SpawnOptions};

    let flags = CreationFlags::DETACHED_PROCESS | CreationFlags::NEW_PROCESS_GROUP;
    command.spawn_with(SpawnOptions::new().creation_flags(flags))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Stdio, detached_command_for, spawn_detached};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const CHILD_TEST: &str = "spawn::process::tests::detached_environment_child";
    const INHERITED_VARIABLE: &str = "PATH";

    fn child_environment(root: &Path, overlay: Option<&str>) -> TestResult<String> {
        let program = std::env::current_exe()?;
        let arguments = ["--exact", CHILD_TEST, "--ignored", "--nocapture"];
        let mut command = detached_command_for(program, arguments, root);
        command.stderr(Stdio::piped());
        if let Some(value) = overlay {
            command.env(INHERITED_VARIABLE, value);
        }
        let output = spawn_detached(&mut command)?.wait_with_output()?;
        assert!(
            output.status.success(),
            "the environment child must exit successfully: {:?}",
            output.status
        );
        Ok(serde_json::from_slice(&output.stderr)?)
    }

    #[test]
    fn a_detached_child_inherits_the_callers_environment() -> TestResult {
        let directory = tempfile::tempdir()?;
        let expected = std::env::var(INHERITED_VARIABLE)?;
        assert_eq!(child_environment(directory.path(), None)?, expected);
        Ok(())
    }

    #[test]
    fn a_detached_child_environment_overlay_replaces_inherited_value() -> TestResult {
        let directory = tempfile::tempdir()?;
        let expected = "rift-detached-environment-overlay";
        assert_ne!(std::env::var(INHERITED_VARIABLE)?, expected);
        assert_eq!(
            child_environment(directory.path(), Some(expected))?,
            expected
        );
        Ok(())
    }

    #[test]
    #[ignore = "runs only as an owned child of the detached environment tests"]
    fn detached_environment_child() -> TestResult {
        let value = std::env::var(INHERITED_VARIABLE)?;
        eprintln!("{}", serde_json::to_string(&value)?);
        Ok(())
    }
}
