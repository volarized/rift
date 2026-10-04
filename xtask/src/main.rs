use std::process::ExitCode;

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

fn run(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let command = parse_command(arguments)?;
    let root = workspace_root()?;
    execute(root, command)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Generate,
    Check,
}

fn parse_command(arguments: &[String]) -> Result<Command, &'static str> {
    match arguments {
        [group, command] if group == "errors" && command == "generate" => Ok(Command::Generate),
        [group, command] if group == "errors" && command == "check" => Ok(Command::Check),
        _ => Err("usage: cargo xtask errors <generate|check>"),
    }
}

fn workspace_root() -> Result<&'static std::path::Path, &'static str> {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask manifest has no workspace parent")
}

fn execute(root: &std::path::Path, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let source_path = root.join("crates/rift-error/errors.toml");
    let output_path = root.join("crates/rift-error/src/generated.rs");
    let source = std::fs::read_to_string(source_path)?;
    let generated = rift_error_codegen::generate_source(&source)?;

    match command {
        Command::Generate => std::fs::write(output_path, generated)?,
        Command::Check => {
            let current = std::fs::read_to_string(&output_path)?;
            if current != generated {
                return Err(
                    "generated error source is stale; run `cargo xtask errors generate`".into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Command, execute, parse_command};

    const REGISTRY: &str = r#"
[registry]
namespace = "rift"
schema = 1

[error.test.failure]
message = "failure for {field}"
action = "retry after checking {field}"
fields = { field = { type = "string" } }
"#;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("create temporary workspace");
        std::fs::create_dir_all(directory.path().join("crates/rift-error/src"))
            .expect("create generated source directory");
        std::fs::write(directory.path().join("crates/rift-error/errors.toml"), REGISTRY)
            .expect("write test registry");
        directory
    }

    #[test]
    fn accepts_only_error_generation_commands() {
        assert_eq!(
            parse_command(&["errors".to_owned(), "generate".to_owned()]),
            Ok(Command::Generate)
        );
        assert_eq!(
            parse_command(&["errors".to_owned(), "check".to_owned()]),
            Ok(Command::Check)
        );
        assert!(parse_command(&[]).is_err());
        assert!(parse_command(&["errors".to_owned(), "other".to_owned()]).is_err());
        assert!(
            parse_command(&["errors".to_owned(), "check".to_owned(), "extra".to_owned()]).is_err()
        );
    }

    #[test]
    fn generate_writes_deterministic_source_and_check_preserves_stale_source() {
        let directory = fixture();
        let root = directory.path();
        let output = root.join("crates/rift-error/src/generated.rs");

        execute(root, Command::Generate).expect("generate source");
        let generated = std::fs::read_to_string(&output).expect("read generated source");
        execute(root, Command::Check).expect("check current source");
        assert_eq!(
            std::fs::read_to_string(&output).expect("read generated source again"),
            generated,
            "check must not rewrite current source"
        );

        std::fs::write(&output, "stale").expect("write stale source");
        assert!(execute(root, Command::Check).is_err());
        assert_eq!(
            std::fs::read_to_string(&output).expect("read stale source"),
            "stale",
            "check must not rewrite stale source"
        );

        execute(root, Command::Generate).expect("replace stale source");
        assert_eq!(
            std::fs::read_to_string(&output).expect("read regenerated source"),
            generated
        );
    }
}
