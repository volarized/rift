//! Installed language servers in package manager layouts.

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::Path;

use rift_dependency::{ContextInputs, ToolchainCommand};
use rift_protocol::configuration::{CONFIGURATION_FILE_BYTES_MAX, CommandInput, LspPackageManager};
use rift_protocol::dependencies::DependenciesConfiguration;

/// Installed executable selected without starting a process or installing a package.
/// Local executables precede PATH. The fixed candidate list bounds each resolution.
pub(crate) fn installed_command(
    manager: LspPackageManager,
    root: &Path,
    search: Option<&OsStr>,
) -> Option<CommandInput> {
    if manager == LspPackageManager::Cargo
        && let Some(command) = cargo_command(root, search)
    {
        return Some(command);
    }
    if matches!(manager, LspPackageManager::Bun | LspPackageManager::Npm) {
        let native = native_typescript_program();
        if which::which_in(&native, None::<&OsStr>, root).is_ok() {
            return Some(CommandInput::ProgramAndArguments(vec![
                native,
                "--lsp".to_owned(),
                "--stdio".to_owned(),
            ]));
        }
    }
    let candidates = candidates(manager);
    for candidate in candidates {
        if let Some(local) = candidate.local
            && which::which_in(local, None::<&OsStr>, root).is_ok()
        {
            return Some(candidate.command(local));
        }
    }
    for candidate in candidates {
        if which::which_in(candidate.program, search, root).is_ok() {
            return Some(candidate.command(candidate.program));
        }
    }
    None
}

/// Installed server eligible for a recommendation. A pinned Cargo toolchain must
/// answer its sysroot and contain rust-analyzer. The shared dependency runner bounds
/// the single probe, honors static resolution, and starts no language server.
pub(crate) fn recommended_command(
    manager: LspPackageManager,
    root: &Path,
    search: Option<&OsStr>,
    dependencies: &DependenciesConfiguration,
) -> Option<CommandInput> {
    let command = installed_command(manager, root, search)?;
    if manager == LspPackageManager::Cargo && command.program() == "rustup" {
        let mut inputs = crate::dependency::FilesystemInputs::new(
            crate::dependency::ResolutionPolicy::from(dependencies),
        );
        installed_cargo_server(&command, root, &mut inputs)?;
    }
    Some(command)
}

fn installed_cargo_server(
    command: &CommandInput,
    root: &Path,
    inputs: &mut dyn ContextInputs,
) -> Option<()> {
    let channel = command.arguments().get(1)?;
    let probe = ToolchainCommand {
        program: "rustup",
        arguments: vec![
            "run".to_owned(),
            channel.clone(),
            "rustc".to_owned(),
            "--print".to_owned(),
            "sysroot".to_owned(),
        ],
        working_directory: root.to_path_buf(),
        environment: vec![("RUSTUP_AUTO_INSTALL", "0"), ("MISE_AUTO_INSTALL", "0")],
        environment_removed: vec!["RUSTUP_TOOLCHAIN"],
    };
    let output = inputs.run(&probe).ok()?;
    if !output.succeeded() {
        return None;
    }
    let sysroot = output.stdout.lines().next()?;
    let executable = Path::new(sysroot).join("bin").join(if cfg!(windows) {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    });
    which::which_in(executable, None::<&OsStr>, root).ok()?;
    Some(())
}

/// Manager declared by an exact language's project. A packageManager declaration
/// precedes lockfiles; a package.json without a Bun lockfile uses npm.
pub(crate) fn detect(language: &str, root: &Path) -> Option<LspPackageManager> {
    match language {
        "rust" => Some(LspPackageManager::Cargo),
        "python" => Some(LspPackageManager::Uv),
        "javascript" | "typescript" | "typescript:tsx" => {
            let declared = read_text(&root.join("package.json"))
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|package| package.get("packageManager")?.as_str().map(str::to_owned));
            match declared
                .as_deref()
                .and_then(|value| value.split('@').next())
            {
                Some("bun") => Some(LspPackageManager::Bun),
                Some("npm") => Some(LspPackageManager::Npm),
                Some(_) => None,
                _ if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() => {
                    Some(LspPackageManager::Bun)
                }
                _ => Some(LspPackageManager::Npm),
            }
        }
        _ => None,
    }
}

fn read_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(CONFIGURATION_FILE_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Reuses dependency discovery's toolchain file parser. rustup run without --install
/// selects the explicit toolchain without inheriting `RUSTUP_TOOLCHAIN` or downloading it.
/// At most the configuration parent bound is inspected; a deeper pin is not selected.
fn cargo_command(root: &Path, search: Option<&OsStr>) -> Option<CommandInput> {
    which::which_in("rustup", search, root).ok()?;
    let channel = root
        .ancestors()
        .take(crate::configuration::CONFIGURATION_PARENTS_MAX)
        .find_map(|directory| {
            [("rust-toolchain.toml", false), ("rust-toolchain", true)]
                .iter()
                .find_map(|(file, legacy)| {
                    rift_dependency::rust_toolchain_channel(
                        &read_text(&directory.join(file))?,
                        *legacy,
                    )
                })
        })?;
    Some(CommandInput::ProgramAndArguments(vec![
        "rustup".to_owned(),
        "run".to_owned(),
        channel,
        "rust-analyzer".to_owned(),
    ]))
}

/// Mirrors the installed TypeScript getExePath.js platform package selection.
fn native_typescript_program() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        other => other,
    };
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("node_modules/@typescript/typescript-{platform}-{architecture}/lib/tsc{suffix}")
}

/// Command used when no installed candidate was found, so the ordinary engine start
/// reports the missing executable through its typed launch failure.
pub(crate) fn missing_command(manager: LspPackageManager) -> CommandInput {
    let candidate = &candidates(manager)[0];
    candidate.command(candidate.local.unwrap_or(candidate.program))
}

struct Candidate {
    program: &'static str,
    local: Option<&'static str>,
    arguments: &'static [&'static str],
}

impl Candidate {
    fn command(&self, program: &str) -> CommandInput {
        CommandInput::ProgramAndArguments(
            std::iter::once(program.to_owned())
                .chain(self.arguments.iter().map(|argument| (*argument).to_owned()))
                .collect(),
        )
    }
}

fn candidates(manager: LspPackageManager) -> &'static [Candidate] {
    const CARGO: &[Candidate] = &[Candidate {
        program: "rust-analyzer",
        local: None,
        arguments: &[],
    }];
    const NODE: &[Candidate] = &[Candidate {
        program: "typescript-language-server",
        local: Some(if cfg!(windows) {
            "node_modules/.bin/typescript-language-server.cmd"
        } else {
            "node_modules/.bin/typescript-language-server"
        }),
        arguments: &["--stdio"],
    }];
    const UV: &[Candidate] = &[
        Candidate {
            program: "ty",
            local: Some(if cfg!(windows) {
                ".venv/Scripts/ty.exe"
            } else {
                ".venv/bin/ty"
            }),
            arguments: &["server"],
        },
        Candidate {
            program: "pyright-langserver",
            local: Some(if cfg!(windows) {
                ".venv/Scripts/pyright-langserver.exe"
            } else {
                ".venv/bin/pyright-langserver"
            }),
            arguments: &["--stdio"],
        },
    ];
    match manager {
        LspPackageManager::Cargo => CARGO,
        LspPackageManager::Bun | LspPackageManager::Npm => NODE,
        LspPackageManager::Uv => UV,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn install(root: &Path, relative: &str) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let file = root.join(relative);
        std::fs::create_dir_all(file.parent().ok_or("executable has no parent")?)?;
        std::fs::write(&file, "#!/bin/sh\nexit 87\n")?;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_servers_precede_path_without_starting_them() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        install(root.path(), "node_modules/.bin/typescript-language-server")?;
        install(root.path(), "bin/typescript-language-server")?;
        let search = root.path().join("bin");
        for manager in [LspPackageManager::Bun, LspPackageManager::Npm] {
            let command = installed_command(manager, root.path(), Some(search.as_os_str()))
                .ok_or("missing server")?;
            assert_eq!(
                command.program(),
                "node_modules/.bin/typescript-language-server"
            );
            assert_eq!(command.arguments(), ["--stdio"]);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn uv_uses_installed_ty_then_pyright_then_path() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let search = root.path().join("bin");
        install(root.path(), "bin/ty")?;
        install(root.path(), ".venv/bin/pyright-langserver")?;
        let command =
            installed_command(LspPackageManager::Uv, root.path(), Some(search.as_os_str()))
                .ok_or("missing pyright")?;
        assert_eq!(command.program(), ".venv/bin/pyright-langserver");
        assert_eq!(command.arguments(), ["--stdio"]);
        install(root.path(), ".venv/bin/ty")?;
        let command =
            installed_command(LspPackageManager::Uv, root.path(), Some(search.as_os_str()))
                .ok_or("missing ty")?;
        assert_eq!(command.program(), ".venv/bin/ty");
        assert_eq!(command.arguments(), ["server"]);
        Ok(())
    }

    #[test]
    fn missing_servers_keep_fixed_names_and_arguments() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        for manager in [
            LspPackageManager::Cargo,
            LspPackageManager::Bun,
            LspPackageManager::Npm,
            LspPackageManager::Uv,
        ] {
            assert!(installed_command(manager, root.path(), Some(OsStr::new(""))).is_none());
            let command = missing_command(manager);
            assert!(!command.program().is_empty());
        }
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn native_typescript_precedes_javascript_server() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        install(root.path(), "node_modules/.bin/typescript-language-server")?;
        let native = native_typescript_program();
        install(root.path(), &native)?;
        for manager in [LspPackageManager::Bun, LspPackageManager::Npm] {
            let command = installed_command(manager, root.path(), Some(OsStr::new("")))
                .ok_or("missing native server")?;
            assert_eq!(command.program(), native);
            assert_eq!(command.arguments(), ["--lsp", "--stdio"]);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cargo_preserves_exact_toolchain_channel_without_running_rustup()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        install(root.path(), "bin/rustup")?;
        let search = root.path().join("bin");
        std::fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = 'nightly-2026-10-20'\n",
        )?;
        let command = installed_command(
            LspPackageManager::Cargo,
            root.path(),
            Some(search.as_os_str()),
        )
        .ok_or("missing rustup")?;
        assert_eq!(command.program(), "rustup");
        assert_eq!(
            command.arguments(),
            ["run", "nightly-2026-10-20", "rust-analyzer"]
        );
        Ok(())
    }

    #[test]
    fn declared_node_manager_precedes_lockfiles() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        assert_eq!(detect("rust", root.path()), Some(LspPackageManager::Cargo));
        assert_eq!(detect("python", root.path()), Some(LspPackageManager::Uv));
        assert_eq!(detect("go", root.path()), None);
        assert_eq!(
            detect("typescript", root.path()),
            Some(LspPackageManager::Npm)
        );
        std::fs::write(root.path().join("bun.lockb"), "lock")?;
        assert_eq!(
            detect("typescript:tsx", root.path()),
            Some(LspPackageManager::Bun)
        );
        std::fs::write(
            root.path().join("package.json"),
            r#"{"packageManager":"npm@11.0.0"}"#,
        )?;
        assert_eq!(
            detect("javascript", root.path()),
            Some(LspPackageManager::Npm)
        );
        std::fs::write(
            root.path().join("package.json"),
            r#"{"packageManager":"bun@1.3.0"}"#,
        )?;
        assert_eq!(
            detect("javascript", root.path()),
            Some(LspPackageManager::Bun)
        );
        std::fs::write(
            root.path().join("package.json"),
            r#"{"packageManager":"pnpm@10.0.0"}"#,
        )?;
        assert_eq!(detect("javascript", root.path()), None);
        std::fs::write(root.path().join("package.json"), "{")?;
        assert_eq!(
            detect("javascript", root.path()),
            Some(LspPackageManager::Bun)
        );
        Ok(())
    }
    #[derive(Default)]
    struct CargoProbe {
        output: Option<rift_dependency::CommandOutput>,
        asked: Vec<ToolchainCommand>,
    }

    impl rift_dependency::StaticInputs for CargoProbe {
        fn read_file(&mut self, _path: &Path, _bytes_max: u64) -> rift_dependency::FileObservation {
            rift_dependency::FileObservation::Absent
        }
    }

    impl ContextInputs for CargoProbe {
        fn run(
            &mut self,
            command: &ToolchainCommand,
        ) -> Result<rift_dependency::CommandOutput, rift_dependency::CommandFailure> {
            self.asked.push(command.clone());
            self.output
                .clone()
                .ok_or_else(|| rift_dependency::CommandFailure {
                    program: command.program.to_owned(),
                    reason: "toolchain is not installed".to_owned(),
                })
        }
    }

    #[cfg(unix)]
    #[test]
    fn cargo_recommendations_require_server_in_the_selected_installed_toolchain()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let command = CommandInput::ProgramAndArguments(vec![
            "rustup".to_owned(),
            "run".to_owned(),
            "nightly-1900-01-01".to_owned(),
            "rust-analyzer".to_owned(),
        ]);
        let mut inputs = CargoProbe::default();
        assert!(installed_cargo_server(&command, root.path(), &mut inputs).is_none());
        assert_eq!(inputs.asked.len(), 1);
        assert_eq!(
            inputs.asked[0].arguments,
            ["run", "nightly-1900-01-01", "rustc", "--print", "sysroot"]
        );
        assert!(
            !inputs.asked[0]
                .arguments
                .iter()
                .any(|argument| argument == "--install")
        );
        assert!(
            inputs.asked[0]
                .environment
                .contains(&("RUSTUP_AUTO_INSTALL", "0"))
        );
        inputs.output = Some(rift_dependency::CommandOutput {
            exit_code: Some(0),
            stdout: format!("{}\n", root.path().join("toolchain").display()),
            stderr: String::new(),
            stdout_truncated: false,
        });
        assert!(installed_cargo_server(&command, root.path(), &mut inputs).is_none());
        install(root.path(), "toolchain/bin/rust-analyzer")?;
        assert!(installed_cargo_server(&command, root.path(), &mut inputs).is_some());
        Ok(())
    }
}
