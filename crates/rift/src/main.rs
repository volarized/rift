//! Rift CLI.

mod update;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

#[cfg(test)]
use clap::{Command, CommandFactory};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rift", version, about = "agentic development toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Serve Rust workspace reads and edits over stdio MCP.
    Mcp,
    /// Replace current Rift binary with latest official release.
    Update,
    /// Delete the backup binary left behind by a Windows self-update.
    ///
    /// Windows cannot delete a running executable, so after replacement the
    /// updater spawns the new binary as a detached cleanup child that retries
    /// deleting the renamed old binary until the parent process releases it.
    /// The name must match `CLEANUP_SUBCOMMAND` in `update.rs`.
    #[cfg(windows)]
    #[command(name = "__cleanup-update", hide = true)]
    __CleanupUpdate { parent_pid: u32 },
}

#[cfg(test)]
fn cli_command() -> Command {
    Cli::command()
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(Some(outcome)) => {
            println!("{outcome}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "rift: error[{code}]: {error}",
                code = error.descriptor().code()
            );
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum CliError {
    Mcp(rift_mcp::StdioServeError),
    Update(update::UpdateError),
}

impl CliError {
    /// Returns canonical registry metadata from the wrapped failure.
    fn descriptor(&self) -> rift_core::ErrorDescriptor {
        match self {
            Self::Mcp(error) => error.descriptor(),
            Self::Update(error) => error.descriptor(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mcp(error) => error.fmt(formatter),
            Self::Update(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mcp(error) => Some(error),
            Self::Update(error) => Some(error),
        }
    }
}

async fn run(cli: Cli) -> Result<Option<update::UpdateOutcome>, CliError> {
    match cli.command {
        None => Ok(None),
        Some(CliCommand::Mcp) => {
            rift_mcp::serve_stdio(Path::new("."))
                .await
                .map_err(CliError::Mcp)?;
            Ok(None)
        }
        Some(CliCommand::Update) => update::update().map(Some).map_err(CliError::Update),
        #[cfg(windows)]
        Some(CliCommand::__CleanupUpdate { parent_pid }) => {
            let _ = parent_pid;
            update::cleanup_replaced_binary().map_err(CliError::Update)?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::{Cli, CliCommand, CliError, cli_command};
    use clap::Parser;

    #[test]
    fn empty_invocation_remains_valid() {
        assert!(Cli::try_parse_from(["rift"]).is_ok());
    }

    #[tokio::test]
    async fn empty_invocation_runs_no_command() {
        let cli = Cli::try_parse_from(["rift"]).expect("empty invocation must parse");
        let outcome = super::run(cli)
            .await
            .expect("empty invocation must succeed");
        assert!(outcome.is_none());
    }

    #[test]
    fn mcp_cli_error_preserves_message_and_source() {
        let error = CliError::Mcp(rift_mcp::StdioServeError::UnexpectedQuit);
        assert_eq!(error.to_string(), "MCP service ended unexpectedly");
        assert!(error.source().is_some());
    }

    #[test]
    fn update_cli_error_preserves_message_and_source() {
        let error = CliError::Update(super::update::error_for_test());
        assert_eq!(
            error.to_string(),
            "release tag `vinvalid` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`"
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn cli_error_descriptor_matches_wrapped_error() {
        let update = super::update::error_for_test();
        let update_code = update.descriptor().code();
        assert!(!update_code.is_empty());
        assert_eq!(CliError::Update(update).descriptor().code(), update_code);

        let mcp = rift_mcp::StdioServeError::UnexpectedQuit;
        let mcp_code = mcp.descriptor().code();
        assert!(!mcp_code.is_empty());
        assert_eq!(CliError::Mcp(mcp).descriptor().code(), mcp_code);
    }

    #[test]
    fn help_identifies_executable_and_mcp_command() {
        let mut command = cli_command();
        command.build();
        assert_eq!(command.get_name(), "rift");
        assert!(command.get_about().is_some());
        assert_eq!(
            command
                .get_subcommands()
                .map(clap::Command::get_name)
                .collect::<Vec<_>>(),
            ["mcp", "update", "help"]
        );
    }

    #[test]
    fn mcp_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "mcp"]).expect("mcp must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Mcp)));
        assert!(Cli::try_parse_from(["rift", "mcp", "--root", "."]).is_err());
    }

    #[test]
    fn update_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "update"]).expect("update must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Update)));
        assert!(Cli::try_parse_from(["rift", "update", "--version", "v0.0.2"]).is_err());
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let error = Cli::try_parse_from(["rift", "server"])
            .expect_err("unknown operational command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
