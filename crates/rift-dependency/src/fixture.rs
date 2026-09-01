//! A recorded inspector for resolver tests: every answer is scripted, every question logged.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::resolver::{
    CommandFailure, CommandOutput, FileObservation, Inspector, ToolchainCommand,
};

/// An inspector answering from scripted files, directories, commands, and environment.
///
/// Every question a resolver asks lands in `asked`, so a test can assert what the
/// resolver read and, as important, what it never touched.
#[derive(Debug, Default)]
pub(crate) struct RecordedInspector {
    files: BTreeMap<PathBuf, Vec<u8>>,
    directories: BTreeSet<PathBuf>,
    commands: BTreeMap<String, Result<CommandOutput, CommandFailure>>,
    environment: BTreeMap<String, String>,
    home: Option<PathBuf>,
    /// Every question asked, rendered one line each, in order.
    pub(crate) asked: Vec<String>,
}

impl RecordedInspector {
    /// Scripts one file's content; its parent directories exist too.
    pub(crate) fn with_file(
        mut self,
        path: impl Into<PathBuf>,
        content: impl Into<Vec<u8>>,
    ) -> Self {
        let path = path.into();
        let mut ancestor = path.parent();
        while let Some(directory) = ancestor {
            self.directories.insert(directory.to_path_buf());
            ancestor = directory.parent();
        }
        self.files.insert(path, content.into());
        self
    }

    /// Scripts one directory's existence, with every ancestor.
    pub(crate) fn with_directory(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut ancestor = Some(path.as_path());
        while let Some(directory) = ancestor {
            self.directories.insert(directory.to_path_buf());
            ancestor = directory.parent();
        }
        self
    }

    /// Scripts the answer to one command, keyed by its rendered invocation.
    pub(crate) fn with_command(
        mut self,
        rendered: impl Into<String>,
        answer: Result<CommandOutput, CommandFailure>,
    ) -> Self {
        self.commands.insert(rendered.into(), answer);
        self
    }

    /// Scripts one environment variable.
    pub(crate) fn with_environment(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.environment.insert(name.into(), value.into());
        self
    }

    /// Scripts the home directory.
    pub(crate) fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// A successful run printing `stdout`, in the shape `with_command` scripts.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "scripted answers share the inspector's own result type, so a test reads one shape"
    )]
    pub(crate) fn succeeded(stdout: impl Into<String>) -> Result<CommandOutput, CommandFailure> {
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            stdout_truncated: false,
        })
    }

    /// A run that exited nonzero, printing `stderr`, in the shape `with_command` scripts.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "scripted answers share the inspector's own result type, so a test reads one shape"
    )]
    pub(crate) fn failed(stderr: impl Into<String>) -> Result<CommandOutput, CommandFailure> {
        Ok(CommandOutput {
            exit_code: Some(101),
            stdout: String::new(),
            stderr: stderr.into(),
            stdout_truncated: false,
        })
    }

    /// A program the inspector could not start.
    pub(crate) fn unavailable(program: &str) -> Result<CommandOutput, CommandFailure> {
        Err(CommandFailure {
            program: program.to_owned(),
            reason: "failed to launch: No such file or directory (os error 2)".to_owned(),
        })
    }
}

impl Inspector for RecordedInspector {
    fn read_file(&mut self, path: &Path, bytes_max: u64) -> FileObservation {
        self.asked.push(format!("read {}", path.display()));
        match self.files.get(path) {
            None => FileObservation::Absent,
            Some(bytes) if bytes.len() as u64 > bytes_max => FileObservation::OverBound {
                bytes: bytes.len() as u64,
            },
            Some(bytes) => FileObservation::Bytes(bytes.clone()),
        }
    }

    fn directory_exists(&mut self, path: &Path) -> bool {
        self.asked.push(format!("exists {}", path.display()));
        self.directories.contains(path)
    }

    fn list_directory(&mut self, path: &Path, entries_max: usize) -> Vec<String> {
        self.asked.push(format!("list {}", path.display()));
        let mut names: BTreeSet<String> = BTreeSet::new();
        for candidate in self.directories.iter().chain(self.files.keys()) {
            if candidate.parent() == Some(path)
                && let Some(name) = candidate.file_name().and_then(|name| name.to_str())
            {
                names.insert(name.to_owned());
            }
        }
        names.into_iter().take(entries_max).collect()
    }

    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure> {
        let rendered = command.rendered();
        self.asked.push(format!(
            "run {rendered} in {}",
            command.working_directory.display()
        ));
        self.commands
            .get(&rendered)
            .cloned()
            .unwrap_or_else(|| Self::unavailable(command.program))
    }

    fn environment(&mut self, name: &str) -> Option<String> {
        self.asked.push(format!("environment {name}"));
        self.environment.get(name).cloned()
    }

    fn home_directory(&mut self) -> Option<PathBuf> {
        self.asked.push("home".to_owned());
        self.home.clone()
    }
}
