//! Rift CLI.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "rift", version, about = "agentic development toolkit")]
struct Cli;

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn test_cli_empty_invocation_is_valid() {
        assert!(Cli::try_parse_from(["rift"]).is_ok());
    }

    #[test]
    fn test_cli_help_identifies_executable() {
        let command = Cli::command();
        assert_eq!(command.get_name(), "rift");
        assert!(command.get_about().is_some());
    }

    #[test]
    fn test_cli_rejects_unknown_commands() {
        let error = Cli::try_parse_from(["rift", "server"])
            .expect_err("v0.0.1 must not expose operational commands");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
