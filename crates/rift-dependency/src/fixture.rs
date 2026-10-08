//! Recorded inputs for resolver tests: every answer is scripted, every question logged.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::resolver::{
    CommandFailure, CommandOutput, ContextInputs, FileObservation, StaticInputs, ToolchainCommand,
};

/// Inputs answering from scripted files, directories, resolved paths, and commands.
///
/// Every question a resolver asks lands in `asked`, so a test can assert what the
/// resolver read and, as important, what it never touched. A path in a question is
/// spelled with `/` between its components on every platform: a resolver joins with
/// the platform separator, and on Windows `/workspace` joined with `bun.lock` reads
/// `/workspace\bun.lock`.
#[derive(Debug, Default)]
pub(crate) struct RecordedInspector {
    collection: rift_protocol::dependencies::DependenciesCollectionConfiguration,
    files: BTreeMap<PathBuf, Vec<u8>>,
    directories: BTreeSet<PathBuf>,
    commands: BTreeMap<String, Result<CommandOutput, CommandFailure>>,
    canonical: BTreeMap<PathBuf, PathBuf>,
    /// Every question asked, rendered one line each, in order.
    pub(crate) asked: Vec<String>,
}

impl RecordedInspector {
    /// Uses the accepted collection bounds while answering recorded file and directory reads.
    pub(crate) fn with_collection(
        mut self,
        collection: rift_protocol::dependencies::DependenciesCollectionConfiguration,
    ) -> Self {
        self.collection = collection;
        self
    }

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

    /// Scripts one path's resolved form; an unscripted path resolves to nothing.
    pub(crate) fn with_canonical(
        mut self,
        path: impl Into<PathBuf>,
        resolved: impl Into<PathBuf>,
    ) -> Self {
        self.canonical.insert(path.into(), resolved.into());
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

/// `path` with `/` between its components, the spelling every recorded question uses.
fn spelled(path: &Path) -> String {
    path.display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

impl StaticInputs for RecordedInspector {
    fn collection(&self) -> rift_protocol::dependencies::DependenciesCollectionConfiguration {
        self.collection
    }

    fn read_file(&mut self, path: &Path, bytes_max: u64) -> FileObservation {
        self.asked.push(format!("read {}", spelled(path)));
        match self.files.get(path) {
            None => FileObservation::Absent,
            Some(bytes) if bytes.len() as u64 > bytes_max => FileObservation::OverBound {
                bytes: bytes.len() as u64,
            },
            Some(bytes) => FileObservation::Bytes(bytes.clone()),
        }
    }

    fn canonical_path(&mut self, path: &Path) -> Option<PathBuf> {
        self.asked.push(format!("canonical {}", spelled(path)));
        self.canonical.get(path).cloned()
    }

    fn list_directory(&mut self, path: &Path, entries_max: usize) -> Vec<String> {
        self.asked.push(format!("list {}", spelled(path)));
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
}

impl ContextInputs for RecordedInspector {
    /// Answers the scripted run keyed by the rendered invocation. The environment
    /// overlay lands in `asked` after the working directory, so a test sees each
    /// variable the run carried.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure> {
        let rendered = command.rendered();
        let overlay: String = command
            .environment
            .iter()
            .map(|(name, value)| format!(" with {name}={value}"))
            .chain(
                command
                    .environment_removed
                    .iter()
                    .map(|name| format!(" without {name}")),
            )
            .collect();
        self.asked.push(format!(
            "run {rendered} in {}{overlay}",
            spelled(&command.working_directory)
        ));
        self.commands
            .get(&rendered)
            .cloned()
            .unwrap_or_else(|| Self::unavailable(command.program))
    }
}
