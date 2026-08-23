//! Rift CLI.

mod server;
mod update;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

#[cfg(test)]
use clap::{Command, CommandFactory};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Default filter keeps dependency diagnostics out of MCP stderr.
const DEFAULT_TRACING_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info";

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
    /// Manage this workspace's HTTP MCP server.
    Server {
        #[command(subcommand)]
        command: server::ServerCommand,
    },
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
    initialize_tracing();
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

/// Installs process tracing with human diagnostics on stderr.
fn initialize_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_TRACING_FILTER)),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();
}

#[derive(Debug)]
enum CliError {
    Mcp(rift_mcp::StdioServeError),
    Server(server::ServerCommandError),
    Update(update::UpdateError),
}

impl CliError {
    /// Returns canonical registry metadata from the wrapped failure.
    fn descriptor(&self) -> rift_core::ErrorDescriptor {
        match self {
            Self::Mcp(error) => error.descriptor(),
            Self::Server(error) => error.descriptor(),
            Self::Update(error) => error.descriptor(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mcp(error) => error.fmt(formatter),
            Self::Server(error) => error.fmt(formatter),
            Self::Update(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mcp(error) => Some(error),
            Self::Server(error) => Some(error),
            Self::Update(error) => Some(error),
        }
    }
}

/// What a completed command prints.
#[derive(Debug)]
enum CliOutcome {
    Server(server::ServerOutcome),
    Update(update::UpdateOutcome),
}

impl fmt::Display for CliOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(outcome) => outcome.fmt(formatter),
            Self::Update(outcome) => outcome.fmt(formatter),
        }
    }
}

async fn run(cli: Cli) -> Result<Option<CliOutcome>, CliError> {
    match cli.command {
        None => Ok(None),
        Some(CliCommand::Mcp) => {
            rift_mcp::serve_stdio(Path::new("."))
                .await
                .map_err(CliError::Mcp)?;
            Ok(None)
        }
        Some(CliCommand::Server { command }) => server::run(command)
            .await
            .map(|outcome| outcome.map(CliOutcome::Server))
            .map_err(CliError::Server),
        Some(CliCommand::Update) => update::update()
            .map(CliOutcome::Update)
            .map(Some)
            .map_err(CliError::Update),
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
    fn update_outcome_prints_through_the_cli_outcome() {
        let outcome = super::CliOutcome::Update(super::update::UpdateOutcome::Current(
            semver::Version::new(0, 0, 11),
        ));
        let rendered = outcome.to_string();
        assert!(rendered.contains("latest version"), "{rendered}");
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
            ["mcp", "server", "update", "help"]
        );
    }

    #[test]
    fn mcp_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "mcp"]).expect("mcp must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Mcp)));
        assert!(
            Cli::try_parse_from(["rift", "mcp", "--blocking-queue-timeout-ms", "1250"]).is_err(),
            "blocking bounds live in rift.toml's [server] table, not CLI flags"
        );
        assert!(Cli::try_parse_from(["rift", "mcp", "--root", "."]).is_err());
    }

    #[test]
    fn update_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "update"]).expect("update must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Update)));
        assert!(Cli::try_parse_from(["rift", "update", "--version", "v0.0.2"]).is_err());
    }

    #[test]
    fn server_commands_parse_with_their_exact_surface() {
        for (arguments, foreground) in [
            (["rift", "server", "start"].as_slice(), false),
            (["rift", "server", "start", "--foreground"].as_slice(), true),
        ] {
            let parsed = Cli::try_parse_from(arguments).expect("start must parse");
            let Some(CliCommand::Server {
                command:
                    super::server::ServerCommand::Start {
                        foreground: parsed_flag,
                    },
            }) = parsed.command
            else {
                panic!("start must parse into the server subcommand: {parsed:?}");
            };
            assert_eq!(parsed_flag, foreground);
        }
        assert!(matches!(
            Cli::try_parse_from(["rift", "server", "stop"])
                .expect("stop must parse")
                .command,
            Some(CliCommand::Server {
                command: super::server::ServerCommand::Stop
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["rift", "server", "restart"])
                .expect("restart must parse")
                .command,
            Some(CliCommand::Server {
                command: super::server::ServerCommand::Restart
            })
        ));
        assert!(
            Cli::try_parse_from(["rift", "server"]).is_err(),
            "server without a subcommand must fail"
        );
        assert!(
            Cli::try_parse_from(["rift", "server", "start", "--port", "12000"]).is_err(),
            "the serving port is elected, not flagged"
        );
        assert!(
            Cli::try_parse_from(["rift", "server", "stop", "--foreground"]).is_err(),
            "--foreground belongs to start alone"
        );
    }

    #[test]
    fn server_cli_error_preserves_registry_identity() {
        let error = CliError::Server(super::server::error_for_test());
        assert_eq!(error.descriptor().code(), "server_start_timed_out");
        assert!(error.to_string().contains("--foreground"));
        assert!(error.source().is_some());
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let error = Cli::try_parse_from(["rift", "serve"])
            .expect_err("unknown operational command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
