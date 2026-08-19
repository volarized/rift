//! Rift CLI.

use clap::{Command, CommandFactory, Parser};

#[derive(Debug, Parser)]
#[command(name = "rift", version, about = "agentic development toolkit")]
struct Cli;

fn cli_command() -> Command {
    Cli::command()
}

fn main() {
    cli_command().get_matches();
}

#[cfg(test)]
mod tests {
    use super::{Cli, cli_command};
    use clap::Parser;

    #[test]
    fn test_cli_empty_invocation_is_valid() {
        assert!(Cli::try_parse_from(["rift"]).is_ok());
    }

    #[test]
    fn test_cli_help_identifies_executable() {
        let command = cli_command();
        assert_eq!(command.get_name(), "rift");
        assert!(command.get_about().is_some());
    }

    #[test]
    fn test_cli_exposes_only_metadata_flags() {
        let mut command = cli_command();
        command.build();
        let argument_ids: Vec<_> = command
            .get_arguments()
            .map(|argument| argument.get_id().as_str())
            .collect();
        assert_eq!(argument_ids, ["help", "version"]);
    }

    #[test]
    fn test_cli_rejects_unknown_commands() {
        let error = Cli::try_parse_from(["rift", "server"])
            .expect_err("v0.0.1 must not expose operational commands");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
