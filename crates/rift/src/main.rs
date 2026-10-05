//! Rift CLI.

/// Use one allocator for Rust and C allocations across Linux index rebuilds.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod install;
mod mcp;
mod progress;
mod server;
mod steer;
mod update;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::OnceLock;

#[cfg(test)]
use clap::{Command, CommandFactory};
use clap::{Parser, Subcommand};
use rift_mcp::McpErrorExt as _;
use rift_tracing::{StderrPolicy, TracingRuntime};

/// The checkout this binary was built from, as `build.rs` recorded it. Every server and
/// proxy this binary runs names its build through it.
const BUILD_CHECKOUT: rift_mcp::BuildCheckout =
    rift_mcp::BuildCheckout::recorded(env!("RIFT_BUILD_COMMIT"), env!("RIFT_BUILD_DIRTY"));

/// This binary's product version, as `rift --version` prints it and its servers publish it.
///
/// A dirty build whose executable cannot be read prints its commit and the dirty mark
/// without the executable's metadata, where a server refuses to start instead.
fn product_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        std::env::current_exe()
            .and_then(|executable| BUILD_CHECKOUT.product_version(&executable))
            .unwrap_or_else(|_| BUILD_CHECKOUT.version_without_stamp())
    })
}

#[derive(Debug, Parser)]
#[command(
    name = "rift",
    version = product_version(),
    about = "agentic development toolkit"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Serve agents over stdio MCP by proxying this workspace's rift server.
    Mcp {
        /// Forwards text and structured tool answers with `all`, and text alone with `text`.
        #[arg(long, value_enum, default_value_t = mcp::OutputMode::All, value_name = "MODE")]
        output: mcp::OutputMode,
    },
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
    let mut tracing_builder = TracingRuntime::builder().stderr(StderrPolicy::of_process(serves));
    if let Some(logs) = &logs {
        tracing_builder = tracing_builder.capture(&logs.capture);
    }
    let (tracing_runtime, drain) = tracing_builder.install();
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
    tracing_runtime.shutdown();
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

#[derive(Debug)]
enum CliError {
    Mcp(rift_mcp::McpFailure),
    Server(rift_error::RiftError),
    Update(rift_error::RiftError),
    Install(rift_error::RiftError),
}

impl CliError {
    /// Returns stable command code for operator output.
    fn code(&self) -> String {
        match self {
            Self::Mcp(error) => error.wire_code(),
            Self::Update(error) | Self::Server(error) | Self::Install(error) => cli_code(error),
        }
    }

    /// The failure as the operator reads it: the registry line, then each
    /// cause below it on its own line.
    ///
    /// The outer line names what was refused; the causes name why, down to
    /// the store's or the provider's own words. The walk is bounded by
    /// [`rift_error::CAUSE_DEPTH_MAX`].
    fn rendered(&self) -> String {
        use std::fmt::Write as _;

        let mut rendered = format!("rift: error[{code}]: {self}\n", code = self.code());
        for cause in rift_error::causes(self) {
            let _ = writeln!(rendered, "  caused by: {cause}");
        }
        rendered
    }
}

fn cli_code(error: &rift_error::RiftError) -> String {
    match error.slug().as_str() {
        "rift.cli.server_already_serving" => "server_already_serving",
        "rift.cli.server_spawn_failed" | "rift.cli.server_start_exited" => "server_start_failed",
        "rift.cli.server_start_timed_out" => "server_start_timed_out",
        "rift.cli.server_election_unreleased"
        | "rift.cli.server_stop_request_failed"
        | "rift.cli.server_stop_refused"
        | "rift.cli.server_stop_timed_out" => "server_stop_failed",
        "rift.cli.server_logs_unavailable" => "server_logs_unavailable",
        "rift.cli.install_home_unresolved" => "install_home_unresolved",
        "rift.cli.install_template_missing_tool" => "install_template_missing_tool",
        "rift.cli.install_write_failed" => "install_write_failed",
        "rift.cli.install_remove_failed" => "install_remove_failed",
        "rift.cli.install_settings_unparsable" => "install_settings_unparsable",
        "rift.cli.update_binary_invalid" => "update_binary_invalid",
        "rift.cli.update_staging_failed" => "update_staging_failed",
        "rift.cli.update_version_invalid"
        | "rift.cli.update_release_tag_invalid"
        | "rift.cli.update_prerelease_unsupported"
        | "rift.cli.update_release_file_inspection_failed"
        | "rift.cli.update_release_file_not_regular"
        | "rift.cli.update_release_file_size_invalid"
        | "rift.cli.update_release_metadata_invalid" => "update_release_invalid",
        "rift.cli.update_checksum_mismatch"
        | "rift.cli.update_checksum_manifest_invalid"
        | "rift.cli.update_checksum_read_failed" => "update_checksum_mismatch",
        "rift.cli.update_download_failed" | "rift.cli.update_download_too_large" => {
            "update_download_failed"
        }
        "rift.cli.update_archive_file_inspection_failed"
        | "rift.cli.update_archive_file_not_regular"
        | "rift.cli.update_archive_file_size_invalid"
        | "rift.cli.update_archive_member_too_large"
        | "rift.cli.update_archive_contents_invalid"
        | "rift.cli.update_archive_extraction_failed" => "update_archive_invalid",
        "rift.cli.update_publish_copy_failed"
        | "rift.cli.update_publish_pending_cleanup"
        | "rift.cli.update_publish_parent_missing"
        | "rift.cli.update_publish_failed" => "update_publish_failed",
        "rift.cli.update_rollback_failed" | "rift.cli.update_rollback_cleanup_failed" => {
            "update_rollback_failed"
        }
        _ => {
            return rift_mcp::wire_code_for_error(error)
                .unwrap_or_else(|| error.slug().as_str().to_owned());
        }
    }
    .to_owned()
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mcp(error) => error.fmt(formatter),
            Self::Server(error) | Self::Update(error) | Self::Install(error) => {
                error.fmt(formatter)
            }
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mcp(error) => Some(error),
            Self::Server(error) | Self::Update(error) | Self::Install(error) => Some(error),
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
    drain: Option<rift_tracing::LogDrain>,
    retention_records: u64,
) -> Result<Option<CliOutcome>, CliError> {
    match cli.command {
        None => Ok(None),
        Some(CliCommand::Mcp { output }) => {
            rift_mcp::serve_proxy(Path::new("."), BUILD_CHECKOUT, output.into())
                .await
                .map_err(|error| CliError::Mcp(error.mcp()))?;
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

    use super::{Cli, CliCommand, CliError, cli_code, cli_command, mcp};
    use clap::Parser;

    #[test]
    fn version_prints_the_product_version() {
        let printed = Cli::try_parse_from(["rift", "--version"])
            .expect_err("--version prints and exits")
            .to_string();
        assert_eq!(printed.trim(), format!("rift {}", super::product_version()));
    }

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
        let error = CliError::Mcp(rift_mcp::McpFailure::new(
            rift_error::errors::mcp::proxy_unexpected_quit().error(),
        ));
        assert!(
            error.to_string().contains("MCP service ended unexpectedly"),
            "{error}"
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn update_cli_error_preserves_message_and_source() {
        let failure = super::update::error_for_test();
        assert_eq!(
            failure.slug(),
            rift_error::errors::cli::update_release_tag_invalid::SLUG
        );
        let error = CliError::Update(failure);
        assert_eq!(error.code(), "update_release_invalid");
        assert_eq!(
            error.to_string(),
            "release tag `vinvalid` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`: source unexpected character 'i' while parsing major version number; use a stable release tag of the form `vMAJOR.MINOR.PATCH`"
        );
        assert!(rift_error::causes(&error).iter().any(|cause| {
            cause == "unexpected character 'i' while parsing major version number"
        }));
        assert!(
            error
                .rendered()
                .starts_with("rift: error[update_release_invalid]: ")
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn install_cli_error_preserves_message_source_and_code() {
        let install = rift_error::errors::cli::install_home_unresolved()
            .checked("HOME, USERPROFILE")
            .error();
        let code = install.slug();
        assert_eq!(code, rift_error::errors::cli::install_home_unresolved::SLUG);
        let error = CliError::Install(install);
        assert_eq!(error.code(), "install_home_unresolved");
        assert!(error.to_string().contains("USERPROFILE"), "{error}");
        assert!(error.source().is_some());
    }

    #[test]
    fn cli_error_code_matches_wrapped_legacy_errors() {
        let update = super::update::error_for_test();
        let update_code = cli_code(&update);
        assert!(!update_code.is_empty());
        assert_eq!(CliError::Update(update).code(), update_code);

        let mcp =
            rift_mcp::McpFailure::new(rift_error::errors::mcp::proxy_unexpected_quit().error());
        let mcp_code = mcp.wire_code();
        assert!(!mcp_code.is_empty());
        assert_eq!(CliError::Mcp(mcp).code(), mcp_code);
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
        // Windows adds the hidden `__cleanup-update` subcommand, which help never lists.
        assert_eq!(
            command
                .get_subcommands()
                .filter(|subcommand| !subcommand.is_hide_set())
                .map(clap::Command::get_name)
                .collect::<Vec<_>>(),
            ["mcp", "server", "update", "install", "steer", "help"]
        );
    }

    fn parsed_output(arguments: &[&str]) -> mcp::OutputMode {
        let parsed = Cli::try_parse_from(arguments).expect("mcp must parse");
        let Some(CliCommand::Mcp { output }) = parsed.command else {
            panic!("expected the mcp command, got {:?}", parsed.command);
        };
        output
    }

    #[test]
    fn mcp_command_defaults_to_all_output() {
        assert_eq!(parsed_output(&["rift", "mcp"]), mcp::OutputMode::All);
    }

    #[test]
    fn mcp_command_accepts_each_output_spelling() {
        for (arguments, expected) in [
            (
                ["rift", "mcp", "--output=all"].as_slice(),
                mcp::OutputMode::All,
            ),
            (
                ["rift", "mcp", "--output", "all"].as_slice(),
                mcp::OutputMode::All,
            ),
            (
                ["rift", "mcp", "--output=text"].as_slice(),
                mcp::OutputMode::Text,
            ),
            (
                ["rift", "mcp", "--output", "text"].as_slice(),
                mcp::OutputMode::Text,
            ),
        ] {
            assert_eq!(parsed_output(arguments), expected, "{arguments:?}");
        }
    }

    #[test]
    fn mcp_command_rejects_unknown_output_and_other_flags() {
        for arguments in [
            ["rift", "mcp", "--output=json"].as_slice(),
            ["rift", "mcp", "--output"].as_slice(),
            ["rift", "mcp", "--blocking-queue-timeout-ms", "1250"].as_slice(),
            ["rift", "mcp", "--root", "."].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(arguments).is_err(),
                "an unknown value or flag is rejected before the proxy starts; blocking bounds live in \
                 rift.toml's [server] table, not CLI flags: {arguments:?}"
            );
        }
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
                        repository: false,
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
        assert!(Cli::try_parse_from(["rift", "server", "start", "--repository"]).is_err());
        let repository_start =
            Cli::try_parse_from(["rift", "server", "start", "--foreground", "--repository"])
                .expect("hidden repository foreground start must parse");
        assert!(matches!(
            repository_start.command,
            Some(CliCommand::Server {
                command: super::server::ServerCommand::Start {
                    foreground: true,
                    repository: true,
                    ..
                }
            })
        ));
        assert!(Cli::try_parse_from(["rift", "server", "stop", "--repository"]).is_ok());
        assert!(Cli::try_parse_from(["rift", "server", "status", "--repository"]).is_ok());
        assert!(matches!(
            Cli::try_parse_from(["rift", "server", "stop"])
                .expect("stop must parse")
                .command,
            Some(CliCommand::Server {
                command: super::server::ServerCommand::Stop { repository: false }
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
                command: super::server::ServerCommand::Status { repository: false }
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
                    repository: false,
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
        let error = CliError::Server(
            rift_error::errors::cli::server_start_timed_out()
                .waited(rift_mcp::START_WAIT_MAX)
                .error(),
        );
        assert_eq!(error.code(), "server_start_timed_out");
        assert!(error.to_string().contains("--foreground"));
        assert!(error.source().is_some());
    }

    /// The wrapped server error renders the same text as the CLI error over it, so
    /// the chain below the registry line starts at the first cause that says more.
    #[test]
    fn a_rendered_error_prints_each_cause_on_its_own_line() {
        let election = rift_error::errors::mcp::election_storage_failed()
            .operation("publish lock document")
            .path("/workspace/.rift/server.json")
            .source(std::io::Error::other("the disk is full"))
            .error();
        let error = CliError::Server(election);

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
        let error = CliError::Server(
            rift_error::errors::cli::server_stop_timed_out()
                .waited(std::time::Duration::ZERO)
                .error(),
        );
        let rendered = error.rendered();
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.ends_with('\n'));
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
