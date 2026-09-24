//! Rift CLI.

/// Use one allocator for Rust and C allocations across Linux index rebuilds.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod install;
mod otlp;
mod progress;
mod server;
mod steer;
mod update;
use std::fmt;
use std::io::IsTerminal as _;
use std::path::Path;
use std::process::ExitCode;

#[cfg(test)]
use clap::{Command, CommandFactory};
use clap::{Parser, Subcommand};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

/// Default filter keeps dependency diagnostics out of MCP stderr.
const DEFAULT_TRACING_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info,rift_index=warn";

#[derive(Debug, Parser)]
#[command(name = "rift", version, about = "agentic development toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Serve agents over stdio MCP by proxying this workspace's rift server.
    Mcp,
    /// Manage this workspace's HTTP MCP server.
    Server {
        #[command(subcommand)]
        command: server::ServerCommand,
    },
    /// Replace current Rift binary with latest official release.
    Update,
    /// Generate or remove a coding agent's Rift skill.
    Install {
        /// Which agent's skill to generate.
        target: install::InstallTarget,
        /// Install under the operator's home directory instead of this workspace.
        #[arg(long)]
        user: bool,
        /// Delete the generated skill instead of writing it.
        #[arg(long)]
        remove: bool,
    },
    /// Answer one Claude Code `PreToolUse` hook call from stdin.
    Steer,
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

impl Cli {
    /// Whether this command owns the workspace server's log drain.
    const fn records_logs(&self) -> bool {
        matches!(
            &self.command,
            Some(CliCommand::Server {
                command: server::ServerCommand::Start {
                    foreground: true,
                    ..
                }
            })
        )
    }
}

#[cfg(test)]
fn cli_command() -> Command {
    Cli::command()
}

/// The process status a foreground server leaves with when its serving ended cleanly.
const SERVED_EXIT_STATUS: i32 = 0;
/// The process status a foreground server leaves with when its serving failed.
const FAILED_EXIT_STATUS: i32 = 1;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let serves = cli.records_logs();
    let logs = serves.then(|| rift_mcp::logs_configuration(Path::new(".")));
    let stderr = stderr_policy(serves, std::io::stderr().is_terminal());
    let (drain, otlp_export) =
        initialize_tracing(logs.as_ref().map(|logs| logs.capture.as_str()), stderr);
    let retention_records = logs.map_or(0, |logs| logs.retention_records);
    let succeeded = match run(cli, drain, retention_records).await {
        Ok(Some(outcome)) => {
            println!("{outcome}");
            true
        }
        Ok(None) => true,
        Err(error) => {
            eprint!("{}", error.rendered());
            false
        }
    };
    // Flushes buffered spans before either exit path: the normal return below drops
    // every other local first, and `process::exit` past it runs no destructor at all.
    otlp_export.shutdown();
    if serves {
        // A foreground server's index build, a lane's pass, or a lexical transaction can
        // still be running when serving ends. Returning would drop the runtime, and that
        // drop waits for every `spawn_blocking` task to return and for every running task
        // to yield, which nothing can cancel; the server's outcome is already printed, its
        // log drain stopped, and its lock document retired, so the process leaves here.
        std::process::exit(if succeeded {
            SERVED_EXIT_STATUS
        } else {
            FAILED_EXIT_STATUS
        });
    }
    if succeeded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// How much the process may write to its standard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StderrPolicy {
    /// Everything the filter admits: the stream belongs to whoever reads it.
    Unbounded,
    /// At most [`rift_mcp::SERVER_STDERR_BYTES_MAX`] bytes: the stream is
    /// the file `rift server start` handed its detached server.
    Bounded,
}

/// The policy for this invocation: a server whose stderr is not a terminal
/// is writing into a file or a pipe that outlives every reader, so it is
/// bounded; every other command, and a server an operator watches in a
/// terminal, writes freely.
fn stderr_policy(serves: bool, terminal: bool) -> StderrPolicy {
    if serves && !terminal {
        StderrPolicy::Bounded
    } else {
        StderrPolicy::Unbounded
    }
}

/// Installs stderr tracing and optional foreground-server recording.
///
/// The two carry their own filters. Stderr keeps `RUST_LOG` or the default
/// targets, because that stream belongs to whoever started the process. The
/// store captures under the workspace's `[logs] capture` filter, so a workspace
/// can record itself at debug without an operator exporting an environment
/// variable into the process a proxy spawns detached.
///
/// The returned drain exists only for a foreground server. Other commands install no
/// recording layer and allocate no log queue.
///
/// The returned [`otlp::Export`] is a no-op handle unless the `otlp` feature is compiled
/// in and `OTEL_EXPORTER_OTLP_ENDPOINT` names a collector; the caller shuts it down
/// before the process exits either way.
fn initialize_tracing(
    capture: Option<&str>,
    stderr: StderrPolicy,
) -> (Option<rift_mcp::LogDrain>, otlp::Export) {
    let (sink, drain) = match capture {
        Some(capture) => {
            let (sink, drain) = rift_mcp::log_capture();
            let filter = EnvFilter::try_new(capture)
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_TRACING_FILTER));
            (Some(sink.with_filter(filter)), Some(drain))
        }
        None => (None, None),
    };
    let writer = match stderr {
        StderrPolicy::Unbounded => BoxMakeWriter::new(std::io::stderr),
        StderrPolicy::Bounded => BoxMakeWriter::new(rift_mcp::BoundedStderr::default()),
    };
    let (otlp_layer, otlp_export) = otlp::layer();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
                .with_writer(writer)
                .with_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_TRACING_FILTER)),
                ),
        )
        .with(sink)
        .with(otlp_layer)
        .init();
    (drain, otlp_export)
}

#[derive(Debug)]
enum CliError {
    Mcp(rift_mcp::ProxyServeError),
    Server(server::ServerCommandError),
    Update(update::UpdateError),
    Install(install::InstallError),
}

impl CliError {
    /// Returns canonical registry metadata from the wrapped failure.
    fn descriptor(&self) -> rift_core::ErrorDescriptor {
        match self {
            Self::Mcp(error) => error.descriptor(),
            Self::Server(error) => error.descriptor(),
            Self::Update(error) => error.descriptor(),
            Self::Install(error) => error.descriptor(),
        }
    }

    /// The failure as the operator reads it: the registry line, then each
    /// cause below it on its own line.
    ///
    /// The outer line names what was refused; the causes name why, down to
    /// the store's or the provider's own words. The walk is bounded by
    /// [`rift_core::CAUSE_DEPTH_MAX`].
    fn rendered(&self) -> String {
        use std::fmt::Write as _;

        let mut rendered = format!(
            "rift: error[{code}]: {self}\n",
            code = self.descriptor().code()
        );
        for cause in rift_core::causes(self) {
            let _ = writeln!(rendered, "  caused by: {cause}");
        }
        rendered
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mcp(error) => error.fmt(formatter),
            Self::Server(error) => error.fmt(formatter),
            Self::Update(error) => error.fmt(formatter),
            Self::Install(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mcp(error) => Some(error),
            Self::Server(error) => Some(error),
            Self::Update(error) => Some(error),
            Self::Install(error) => Some(error),
        }
    }
}

/// What a completed command prints.
#[derive(Debug)]
enum CliOutcome {
    Server(server::ServerOutcome),
    Update(update::UpdateOutcome),
    Install(install::InstallOutcome),
    Steer(steer::SteerOutcome),
}

impl fmt::Display for CliOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(outcome) => outcome.fmt(formatter),
            Self::Update(outcome) => outcome.fmt(formatter),
            Self::Install(outcome) => outcome.fmt(formatter),
            Self::Steer(outcome) => outcome.fmt(formatter),
        }
    }
}

async fn run(
    cli: Cli,
    drain: Option<rift_mcp::LogDrain>,
    retention_records: u64,
) -> Result<Option<CliOutcome>, CliError> {
    match cli.command {
        None => Ok(None),
        Some(CliCommand::Mcp) => {
            rift_mcp::serve_proxy(Path::new("."))
                .await
                .map_err(CliError::Mcp)?;
            Ok(None)
        }
        Some(CliCommand::Server { command }) => server::run(command, drain, retention_records)
            .await
            .map(|outcome| outcome.map(CliOutcome::Server))
            .map_err(CliError::Server),
        Some(CliCommand::Update) => update::update()
            .await
            .map(CliOutcome::Update)
            .map(Some)
            .map_err(CliError::Update),
        Some(CliCommand::Install {
            target,
            user,
            remove,
        }) => install::run(target, user, remove)
            .map(CliOutcome::Install)
            .map(Some)
            .map_err(CliError::Install),
        Some(CliCommand::Steer) => Ok(Some(CliOutcome::Steer(steer::run()))),
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
        let outcome = super::run(cli, None, 1_000)
            .await
            .expect("empty invocation must succeed");
        assert!(outcome.is_none());
    }

    #[test]
    fn mcp_cli_error_preserves_message_and_source() {
        let error = CliError::Mcp(rift_core::Error::new(rift_mcp::ProxyFault::UnexpectedQuit));
        assert!(
            error.to_string().contains("MCP service ended unexpectedly"),
            "{error}"
        );
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
    fn install_cli_error_preserves_message_source_and_descriptor() {
        let install = rift_core::Error::new(super::install::InstallFault::HomeDirectoryUnresolved);
        let code = install.descriptor().code();
        assert_eq!(code, "install_home_unresolved");
        let error = CliError::Install(install);
        assert_eq!(error.descriptor().code(), code);
        assert!(error.to_string().contains("USERPROFILE"), "{error}");
        assert!(error.source().is_some());
    }

    #[test]
    fn cli_error_descriptor_matches_wrapped_error() {
        let update = super::update::error_for_test();
        let update_code = update.descriptor().code();
        assert!(!update_code.is_empty());
        assert_eq!(CliError::Update(update).descriptor().code(), update_code);

        let mcp = rift_core::Error::new(rift_mcp::ProxyFault::UnexpectedQuit);
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
            ["mcp", "server", "update", "install", "steer", "help"]
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
                        auth,
                    },
            }) = parsed.command
            else {
                panic!("start must parse into the server subcommand: {parsed:?}");
            };
            assert_eq!(parsed_flag, foreground);
            assert_eq!(
                auth,
                super::server::AuthMode::Token,
                "an unflagged start checks its token"
            );
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
        assert!(matches!(
            Cli::try_parse_from(["rift", "server", "status"])
                .expect("status must parse")
                .command,
            Some(CliCommand::Server {
                command: super::server::ServerCommand::Status
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

    /// `--auth skip` serves every loopback request unchecked, so it is
    /// accepted only in the process the operator is watching: a detached
    /// start that carried it would leave an unchecked server behind.
    #[test]
    fn skipping_the_token_check_needs_a_foreground_start() {
        let refused = Cli::try_parse_from(["rift", "server", "start", "--auth", "skip"])
            .expect_err("--auth skip without --foreground must be refused");
        let rendered = refused.to_string();
        assert!(rendered.contains("--foreground"), "{rendered}");
        assert!(rendered.contains("--auth"), "{rendered}");

        let parsed =
            Cli::try_parse_from(["rift", "server", "start", "--auth", "skip", "--foreground"])
                .expect("--auth skip beside --foreground must parse");
        let Some(CliCommand::Server {
            command:
                super::server::ServerCommand::Start {
                    foreground: true,
                    auth: super::server::AuthMode::Skip,
                },
        }) = parsed.command
        else {
            panic!("the flagged start must carry both values: {parsed:?}");
        };
    }

    #[test]
    fn the_logs_command_parses_its_filters() {
        let plain = Cli::try_parse_from(["rift", "server", "logs"]).expect("logs must parse");
        let rendered = format!("{plain:?}");
        let Some(CliCommand::Server {
            command:
                super::server::ServerCommand::Logs {
                    follow,
                    tail,
                    since,
                    level,
                    component,
                },
        }) = plain.command
        else {
            panic!("logs must parse into the server subcommand: {rendered}");
        };
        assert!(!follow);
        assert_eq!(tail, super::server::TailCount::All);
        assert_eq!(since, None);
        assert_eq!(level, None);
        assert_eq!(component, None);
    }

    #[test]
    fn a_filtered_logs_read_parses_every_option() {
        let parsed = Cli::try_parse_from([
            "rift",
            "server",
            "logs",
            "-f",
            "-n",
            "20",
            "--level",
            "warn",
            "--component",
            "index",
            "--since",
            "10m",
        ])
        .expect("a filtered logs read must parse");
        let rendered = format!("{parsed:?}");
        let Some(CliCommand::Server {
            command:
                super::server::ServerCommand::Logs {
                    follow,
                    tail,
                    since,
                    level,
                    component,
                },
        }) = parsed.command
        else {
            panic!("logs must parse into the server subcommand: {rendered}");
        };
        assert!(follow);
        assert_eq!(tail, super::server::TailCount::Newest(20));
        assert_eq!(
            since,
            Some(rift_protocol::configuration::Duration::from_millis(600_000))
        );
        assert_eq!(level, Some(super::server::LogLevel::Warn));
        assert_eq!(component.as_deref(), Some("index"));
    }

    #[test]
    fn a_logs_read_refuses_values_outside_their_documented_forms() {
        for arguments in [
            ["rift", "server", "logs", "--tail", "0"].as_slice(),
            ["rift", "server", "logs", "--tail", "many"].as_slice(),
            ["rift", "server", "logs", "--level", "loud"].as_slice(),
            ["rift", "server", "logs", "--since", "10"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(arguments).is_err(),
                "{arguments:?} must be refused"
            );
        }
    }

    #[test]
    fn only_a_foreground_server_records_workspace_logs() {
        let foreground = Cli::try_parse_from(["rift", "server", "start", "--foreground"])
            .expect("foreground start must parse");
        assert!(foreground.records_logs());

        for arguments in [
            ["rift", "mcp"].as_slice(),
            ["rift", "server", "start"].as_slice(),
            ["rift", "server", "stop"].as_slice(),
            ["rift", "server", "restart"].as_slice(),
            ["rift", "server", "status"].as_slice(),
            ["rift", "server", "logs", "--follow"].as_slice(),
            ["rift", "update"].as_slice(),
            ["rift", "steer"].as_slice(),
        ] {
            let command = Cli::try_parse_from(arguments).expect("command must parse");
            assert!(
                !command.records_logs(),
                "{arguments:?} must not allocate the workspace log queue"
            );
        }
    }

    #[test]
    fn server_cli_error_preserves_registry_identity() {
        let error = CliError::Server(super::server::error_for_test());
        assert_eq!(error.descriptor().code(), "server_start_timed_out");
        assert!(error.to_string().contains("--foreground"));
        assert!(error.source().is_some());
    }

    /// The wrapped server error renders the same text as the CLI error over it, so
    /// the chain below the registry line starts at the first cause that says more.
    #[test]
    fn a_rendered_error_prints_each_cause_on_its_own_line() {
        let inner = std::io::Error::other("the disk is full");
        let election = rift_core::Error::new(rift_mcp::ElectionFault::Storage {
            operation: "publish lock document",
            path: std::path::PathBuf::from("/workspace/.rift/server.json"),
            source: inner,
        });
        let error = CliError::Server(super::server::election_error_for_test(election));

        let rendered = error.rendered();

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2, "{rendered}");
        assert!(
            lines[0].starts_with("rift: error[storage_failure]: "),
            "{rendered}"
        );
        assert!(lines[0].contains("publish lock document"), "{rendered}");
        assert_eq!(lines[1], "  caused by: the disk is full");
    }

    #[test]
    fn a_rendered_error_without_causes_is_one_line() {
        let error = CliError::Server(super::server::error_for_test());
        let rendered = error.rendered();
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn only_a_server_off_a_terminal_bounds_its_stderr() {
        assert_eq!(
            super::stderr_policy(true, false),
            super::StderrPolicy::Bounded
        );
        assert_eq!(
            super::stderr_policy(true, true),
            super::StderrPolicy::Unbounded
        );
        assert_eq!(
            super::stderr_policy(false, false),
            super::StderrPolicy::Unbounded
        );
        assert_eq!(
            super::stderr_policy(false, true),
            super::StderrPolicy::Unbounded
        );
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let error = Cli::try_parse_from(["rift", "serve"])
            .expect_err("unknown operational command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn install_command_parses_target_scope_and_removal() {
        let parsed =
            Cli::try_parse_from(["rift", "install", "claude"]).expect("install claude must parse");
        let rendered = format!("{parsed:?}");
        let Some(CliCommand::Install {
            target,
            user,
            remove,
        }) = parsed.command
        else {
            panic!("install must parse into the install subcommand: {rendered}");
        };
        assert!(matches!(target, super::install::InstallTarget::Claude));
        assert!(!user);
        assert!(!remove);

        let scoped = Cli::try_parse_from(["rift", "install", "claude", "--user", "--remove"])
            .expect("install claude --user --remove must parse");
        let scoped_rendered = format!("{scoped:?}");
        let Some(CliCommand::Install { user, remove, .. }) = scoped.command else {
            panic!("install must parse into the install subcommand: {scoped_rendered}");
        };
        assert!(user);
        assert!(remove);

        assert!(
            Cli::try_parse_from(["rift", "install", "codex"]).is_err(),
            "codex is not a served install target yet"
        );
        assert!(
            Cli::try_parse_from(["rift", "install"]).is_err(),
            "install without a target must fail"
        );
    }

    #[test]
    fn steer_command_accepts_no_extra_arguments() {
        let parsed = Cli::try_parse_from(["rift", "steer"]).expect("steer must parse");
        assert!(matches!(parsed.command, Some(CliCommand::Steer)));
        assert!(
            Cli::try_parse_from(["rift", "steer", "--session-id", "abc"]).is_err(),
            "steer reads the hook call from stdin, not flags"
        );
    }
}
