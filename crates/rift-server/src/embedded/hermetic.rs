//! The ty system the embedded engine runs on: the OS filesystem below the served tree's
//! configuration boundary, and none of the process's own state.

use std::any::Any;
use std::io;

use ruff_db::system::walk_directory::WalkDirectoryBuilder;
use ruff_db::system::{
    DirectoryEntry, Metadata, OsSystem, Result, System, SystemPath, SystemPathBuf,
    SystemVirtualPath, WhichError, WhichResult, WritableSystem,
};
use ruff_notebook::{Notebook, NotebookError};

/// The files ty's project discovery reads configuration from, in the directory it starts
/// at and in every ancestor of it.
const PROJECT_CONFIGURATION_FILES: [&str; 2] = ["pyproject.toml", "ty.toml"];

/// A ty [`System`] over the OS filesystem that answers no environment variable, finds no
/// program, runs no command, names no user configuration directory, and holds no project
/// configuration above the served tree.
///
/// ty looks for a Python environment in the process when its options name none: the
/// environment `VIRTUAL_ENV` or `CONDA_PREFIX` names, then a `python3` or `python` on
/// `PATH`. It also adds every `PYTHONPATH` entry to its search paths and, on a
/// configuration reload, applies the user's own `ty.toml`. Each of those reads goes
/// through the system, so on this one ty's own discovery finds the tree's `.venv` or
/// nothing, and an answer depends on the served tree alone.
///
/// ty's project discovery walks from the tree root through every ancestor and settles on
/// the first `ty.toml` or `[tool.ty]` table it meets, on the first discovery and on every
/// reload alike. The system answers a `pyproject.toml` or `ty.toml` above the root as
/// absent, so the walk ends at the root, the boundary every other Rift read keeps. Every
/// other file read, metadata, directory walk, and write goes to [`OsSystem`].
#[derive(Debug, Clone)]
pub(super) struct HermeticSystem {
    os: OsSystem,
}

impl HermeticSystem {
    /// A system for the tree at `root`, which must be absolute; it is the system's
    /// current directory too.
    pub(super) fn new(root: &SystemPath) -> Self {
        Self {
            os: OsSystem::new(root),
        }
    }

    /// Whether `path` is a project configuration file in a directory above the root.
    fn is_configuration_above_the_root(&self, path: &SystemPath) -> bool {
        let root = self.os.current_directory();
        let configuration = path
            .file_name()
            .is_some_and(|name| PROJECT_CONFIGURATION_FILES.contains(&name));
        let above = path
            .parent()
            .is_some_and(|directory| directory != root && root.starts_with(directory));
        configuration && above
    }
}

/// The refusal a configuration file above the root reads as: the file is not there.
fn absent(path: &SystemPath) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{path} lies above the served tree"),
    )
}

impl System for HermeticSystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        if self.is_configuration_above_the_root(path) {
            return Err(absent(path));
        }
        self.os.path_metadata(path)
    }

    fn canonicalize_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        self.os.canonicalize_path(path)
    }

    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        self.os.is_same_file(first, second)
    }

    /// Finds nothing, so no interpreter on `PATH` becomes the Python environment.
    fn which(&self, _binary_name: &str) -> WhichResult {
        Err(WhichError::CannotFindBinaryPath)
    }

    fn read_to_string(&self, path: &SystemPath) -> Result<String> {
        if self.is_configuration_above_the_root(path) {
            return Err(absent(path));
        }
        self.os.read_to_string(path)
    }

    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        self.os.read_to_notebook(path)
    }

    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> Result<String> {
        self.os.read_virtual_path_to_string(path)
    }

    fn read_virtual_path_to_notebook(
        &self,
        path: &SystemVirtualPath,
    ) -> std::result::Result<Notebook, NotebookError> {
        self.os.read_virtual_path_to_notebook(path)
    }

    fn current_directory(&self) -> &SystemPath {
        self.os.current_directory()
    }

    /// Names none, so no user configuration joins the tree's own.
    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        None
    }

    fn cache_dir(&self) -> Option<SystemPathBuf> {
        self.os.cache_dir()
    }

    fn read_directory<'a>(
        &'a self,
        path: &SystemPath,
    ) -> Result<Box<dyn Iterator<Item = Result<DirectoryEntry>> + 'a>> {
        self.os.read_directory(path)
    }

    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        self.os.walk_directory(path)
    }

    /// Answers every variable as unset: `VIRTUAL_ENV`, `CONDA_PREFIX`, and `PYTHONPATH`
    /// among them.
    fn env_var(&self, _name: &str) -> std::result::Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        self.os.as_writable()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn dyn_clone(&self) -> Box<dyn System> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ruff_db::system::walk_directory::WalkState;

    use super::*;

    /// One code cell holding `value = 1`, as Jupyter writes a notebook.
    const NOTEBOOK: &str = r#"{"cells": [{"cell_type": "code", "execution_count": null, "id": "a1", "metadata": {}, "outputs": [], "source": ["value = 1\n"]}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;

    fn fixture() -> (tempfile::TempDir, SystemPathBuf) {
        let directory = tempfile::tempdir().expect("fixture directory");
        let root = SystemPathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("a UTF-8 temporary root");
        (directory, root)
    }

    /// `PATH` is set in every test process, and a shell stands on it on every platform.
    #[test]
    fn test_the_process_variables_and_programs_stay_unread() {
        let (_directory, root) = fixture();
        let os = OsSystem::new(&root);
        let hermetic = HermeticSystem::new(&root);

        assert!(os.env_var("PATH").is_ok(), "the process holds `PATH`");
        assert_eq!(
            hermetic.env_var("PATH"),
            Err(std::env::VarError::NotPresent)
        );

        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(os.which(shell).is_ok(), "`{shell}` stands on `PATH`");
        assert!(matches!(
            hermetic.which(shell),
            Err(WhichError::CannotFindBinaryPath)
        ));

        assert!(os.command_executor().is_some(), "the OS runs commands");
        assert!(hermetic.command_executor().is_none());
        assert_eq!(hermetic.user_config_directory(), None);
    }

    #[test]
    fn test_files_and_directories_read_through_the_os() {
        let (directory, root) = fixture();
        std::fs::write(directory.path().join("module.py"), "value = 1\n").expect("fixture file");
        std::fs::write(directory.path().join("notes.ipynb"), NOTEBOOK).expect("fixture notebook");
        let hermetic = HermeticSystem::new(&root);
        let module = root.join("module.py");
        let notebook = root.join("notes.ipynb");

        assert_eq!(hermetic.current_directory(), root.as_path());
        assert_eq!(
            hermetic.read_to_string(&module).expect("the file reads"),
            "value = 1\n"
        );
        assert!(hermetic.path_metadata(&module).is_ok());
        assert!(hermetic.is_file(&module));
        assert!(hermetic.is_directory(&root));
        assert_eq!(
            hermetic.canonicalize_path(&module).expect("canonical path"),
            OsSystem::new(&root)
                .canonicalize_path(&module)
                .expect("canonical path")
        );
        assert!(hermetic.is_same_file(&module, &module).expect("comparable"));
        let cells = hermetic
            .read_to_notebook(&notebook)
            .expect("the notebook reads");
        assert!(cells.source_code().starts_with("value = 1\n"));
        assert_eq!(
            cells.source_code(),
            OsSystem::new(&root)
                .read_to_notebook(&notebook)
                .expect("the notebook reads")
                .source_code()
        );

        let mut listed: Vec<SystemPathBuf> = hermetic
            .read_directory(&root)
            .expect("the root lists")
            .map(|entry| entry.expect("an entry").into_path())
            .collect();
        listed.sort();
        assert_eq!(listed, [module.clone(), notebook.clone()]);

        let walked = Mutex::new(Vec::new());
        hermetic.walk_directory(&root).run(|| {
            Box::new(|entry| {
                if let Ok(entry) = entry {
                    walked.lock().expect("walk record").push(entry.into_path());
                }
                WalkState::Continue
            })
        });
        let mut walked = walked.into_inner().expect("walk record");
        walked.sort();
        assert_eq!(walked, [root.clone(), module, notebook]);
    }

    /// Nothing outside the filesystem answers: no virtual document, and the OS keeps
    /// its own cache and writes.
    #[test]
    fn test_virtual_paths_stay_unread_and_writes_reach_the_os() {
        let (_directory, root) = fixture();
        let hermetic = HermeticSystem::new(&root);
        let untitled = SystemVirtualPath::new("untitled:1");

        assert!(hermetic.read_virtual_path_to_string(untitled).is_err());
        assert!(hermetic.read_virtual_path_to_notebook(untitled).is_err());
        assert_eq!(hermetic.cache_dir(), OsSystem::new(&root).cache_dir());
        assert!(hermetic.as_writable().is_some());
    }

    /// A project configuration file above the root reads as absent, while the root's own
    /// and any other file above it read through.
    #[test]
    fn test_project_configuration_above_the_root_reads_as_absent() {
        let (directory, parent) = fixture();
        for name in PROJECT_CONFIGURATION_FILES {
            std::fs::write(directory.path().join(name), "\n").expect("outer configuration");
        }
        std::fs::write(directory.path().join("notes.txt"), "outer\n").expect("outer file");
        std::fs::create_dir(directory.path().join("tree")).expect("tree directory");
        std::fs::write(directory.path().join("tree/ty.toml"), "\n").expect("tree configuration");
        let root = parent.join("tree");
        let os = OsSystem::new(&root);
        let hermetic = HermeticSystem::new(&root);

        for name in PROJECT_CONFIGURATION_FILES {
            let above = parent.join(name);
            assert!(os.read_to_string(&above).is_ok(), "{above} stands on disk");
            assert_eq!(
                hermetic
                    .read_to_string(&above)
                    .map_err(|error| error.kind()),
                Err(io::ErrorKind::NotFound)
            );
            assert!(!hermetic.is_file(&above), "{above} reads as absent");
        }
        assert!(hermetic.read_to_string(&parent.join("notes.txt")).is_ok());
        assert!(hermetic.read_to_string(&root.join("ty.toml")).is_ok());
        assert!(hermetic.is_file(&root.join("ty.toml")));
    }

    #[test]
    fn test_a_clone_and_a_downcast_keep_the_hermetic_system() {
        let (_directory, root) = fixture();
        let mut hermetic = HermeticSystem::new(&root);
        let cloned = hermetic.dyn_clone();

        assert!(cloned.as_any().is::<HermeticSystem>());
        assert_eq!(cloned.env_var("PATH"), Err(std::env::VarError::NotPresent));
        assert!(hermetic.as_any_mut().is::<HermeticSystem>());
    }
}
