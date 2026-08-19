//! Rift CLI.

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
    /// Serve read-only Rust workspace context over stdio MCP.
    Mcp,
}

#[derive(Debug)]
enum CliError {
    Mcp(rift_mcp::StdioServeError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mcp(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mcp(error) => Some(error),
        }
    }
}

#[cfg(test)]
fn cli_command() -> Command {
    Cli::command()
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        None => Ok(()),
        Some(CliCommand::Mcp) => rift_mcp::serve_stdio(Path::new("."))
            .await
            .map_err(CliError::Mcp),
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

    #[test]
    fn mcp_cli_error_preserves_message_and_source() {
        let error = CliError::Mcp(rift_mcp::StdioServeError::UnexpectedQuit);
        assert_eq!(error.to_string(), "MCP service ended unexpectedly");
        assert!(error.source().is_some());
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
            ["mcp", "help"]
        );
    }

    #[test]
    fn mcp_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "mcp"]).expect("mcp must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Mcp)));
        assert!(Cli::try_parse_from(["rift", "mcp", "--root", "."]).is_err());
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let error = Cli::try_parse_from(["rift", "server"])
            .expect_err("unknown operational command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
