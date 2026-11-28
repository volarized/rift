mod errors;

use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

const USAGE: &str = "usage: cargo xtask <docs | analyzer-manifest | errors <generate|check> | docs-examples <generate|check>>";
const HELP_COMMANDS: &[&[&str]] = &[
    &["--help"],
    &["config", "--help"],
    &["config", "set", "--help"],
    &["mcp", "--help"],
    &["server", "--help"],
    &["server", "logs", "--help"],
];

type TaskResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &[String]) -> TaskResult {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask manifest has no workspace parent")?;
    match arguments {
        [task] if task == "docs" => {
            export_schema(root, "--docs")?;
            export_errors(root, "generate")?;
            export_cli_help(root)?;
            capture_docs_examples(root, "generate", Command::status)
        }
        [task] if task == "analyzer-manifest" => export_schema(root, "--analyzer-manifest"),
        [task, mode] if task == "errors" && matches!(mode.as_str(), "generate" | "check") => {
            export_errors(root, mode)
        }
        [task, mode]
            if task == "docs-examples" && matches!(mode.as_str(), "generate" | "check") =>
        {
            capture_docs_examples(root, mode, Command::status)
        }
        _ => Err(USAGE.into()),
    }
}

fn cargo(root: &Path) -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root);
    command
}

fn export_schema(root: &Path, target: &str) -> TaskResult {
    let status = cargo(root)
        .args([
            "run",
            "--locked",
            "--quiet",
            "-p",
            "rift-schema-export",
            "--",
            target,
        ])
        .arg(if target == "--analyzer-manifest" {
            "."
        } else {
            "docs"
        })
        .status()?;
    if !status.success() {
        return Err(format!("schema export failed: {status}").into());
    }
    Ok(())
}

fn export_errors(root: &Path, mode: &str) -> TaskResult {
    let path = root.join("docs/public/errors.json");
    let json = errors::json()?;
    if mode == "check" {
        if std::fs::read_to_string(&path)? != json {
            return Err(
                "errors.json differs from the Rust registry; run `cargo xtask errors generate`"
                    .into(),
            );
        }
    } else {
        std::fs::create_dir_all(root.join("docs/public"))?;
        std::fs::write(path, json)?;
    }
    Ok(())
}

fn export_cli_help(root: &Path) -> TaskResult {
    let mut transcript = String::new();
    for (index, arguments) in HELP_COMMANDS.iter().enumerate() {
        let output = cargo(root)
            .args(["run", "--locked", "--quiet", "-p", "rift", "--"])
            .args(*arguments)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "rift {} failed: {}\n{}",
                arguments.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr),
            )
            .into());
        }
        if index > 0 {
            transcript.push('\n');
        }
        std::fmt::Write::write_fmt(
            &mut transcript,
            format_args!("$ rift {}\n", arguments.join(" ")),
        )?;
        transcript.push_str(&String::from_utf8(output.stdout)?);
    }
    std::fs::write(root.join("docs/public/cli-help.txt"), transcript)?;
    Ok(())
}

fn capture_docs_examples(
    root: &Path,
    mode: &str,
    capture: impl FnOnce(&mut Command) -> std::io::Result<ExitStatus>,
) -> TaskResult {
    let mut process = cargo(root);
    // A Git hook exports repository variables that would redirect fixture commands
    // from their temporary workspaces into the checkout running the hook.
    let variables = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--local-env-vars"])
        .output()?;
    if !variables.status.success() {
        return Err(format!(
            "git could not list repository variables: {}",
            variables.status
        )
        .into());
    }
    for variable in String::from_utf8(variables.stdout)?.lines() {
        process.env_remove(variable);
    }
    process.env("RIFT_DOCS_EXAMPLES", mode).args([
        "test",
        "--locked",
        "--profile",
        "dev",
        "-p",
        "rift-mcp",
        "--test",
        "docs_examples",
        "--",
        "--ignored",
        "--exact",
        "capture_docs_examples",
    ]);
    let status = capture(&mut process)?;
    if !status.success() {
        return Err(format!("docs example capture failed: {status}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::path::Path;
    use std::process::ExitStatus;

    use super::{USAGE, capture_docs_examples, export_errors, run};

    #[test]
    fn errors_export_generates_the_registry_and_checks_its_bytes() {
        let directory = tempfile::tempdir().expect("a workspace");
        let root = directory.path();
        export_errors(root, "generate").expect("generate the registry");
        export_errors(root, "check").expect("the exported registry is current");
        std::fs::write(root.join("docs/public/errors.json"), "{}\n").expect("a stale registry");
        let error = export_errors(root, "check").expect_err("a stale registry must fail");
        assert!(error.to_string().contains("differs from the Rust registry"));
    }

    #[test]
    fn docs_examples_dispatches_both_modes_to_the_capture() {
        let directory = tempfile::tempdir().expect("a workspace");
        for mode in ["generate", "check"] {
            capture_docs_examples(directory.path(), mode, |process| {
                assert_eq!(process.get_current_dir(), Some(directory.path()));
                let environment = process.get_envs().collect::<BTreeMap<_, _>>();
                assert_eq!(
                    environment.get(OsStr::new("RIFT_DOCS_EXAMPLES")),
                    Some(&Some(OsStr::new(mode)))
                );
                for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
                    assert_eq!(environment.get(OsStr::new(variable)), Some(&None));
                }
                assert!(!environment.contains_key(OsStr::new("PATH")));
                assert_eq!(
                    process.get_args().collect::<Vec<_>>(),
                    [
                        "test",
                        "--locked",
                        "--profile",
                        "dev",
                        "-p",
                        "rift-mcp",
                        "--test",
                        "docs_examples",
                        "--",
                        "--ignored",
                        "--exact",
                        "capture_docs_examples",
                    ]
                );
                Ok(ExitStatus::from_raw(0))
            })
            .expect("successful capture");
        }
    }

    #[test]
    fn docs_examples_refuses_other_modes_and_reports_capture_errors() {
        for group in ["docs-examples", "unknown"] {
            let error = run(&[group.to_owned(), "bad".to_owned()]).expect_err("invalid command");
            assert_eq!(error.to_string(), USAGE);
        }

        let root = Path::new(".");
        let error = capture_docs_examples(root, "check", |_| Ok(ExitStatus::from_raw(1 << 8)))
            .expect_err("nonzero capture exit");
        assert!(
            error
                .to_string()
                .starts_with("docs example capture failed:")
        );
        let error =
            capture_docs_examples(root, "check", |_| Err(std::io::ErrorKind::NotFound.into()))
                .expect_err("capture could not start");
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound)
        );
    }
}
