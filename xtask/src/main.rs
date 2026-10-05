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

/// Every generated file, as a path under `crates/rift-error/src` and its content.
fn expected_files(
    module: rift_error_codegen::GeneratedModule,
) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    let mut files = std::collections::BTreeMap::new();
    files.insert(std::path::PathBuf::from("errors.rs"), module.parent);
    for (namespace, content) in module.namespaces {
        files.insert(
            std::path::Path::new("errors").join(format!("{namespace}.rs")),
            content,
        );
    }
    files
}

/// Every file under `directory`, as a path relative to it.
fn present_files(
    directory: &std::path::Path,
    relative: &std::path::Path,
    files: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            present_files(&entry.path(), &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn execute(root: &std::path::Path, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let source_path = root.join("crates/rift-error/errors.toml");
    let source_root = root.join("crates/rift-error/src");
    let source = std::fs::read_to_string(source_path)?;
    let expected = expected_files(rift_error_codegen::generate_module(&source)?);

    let mut present = Vec::new();
    present_files(
        &source_root.join("errors"),
        std::path::Path::new("errors"),
        &mut present,
    )?;
    let stale = present
        .into_iter()
        .filter(|path| !expected.contains_key(path))
        .collect::<Vec<_>>();

    match command {
        Command::Generate => {
            std::fs::create_dir_all(source_root.join("errors"))?;
            for (path, content) in &expected {
                std::fs::write(source_root.join(path), content)?;
            }
            for path in &stale {
                std::fs::remove_file(source_root.join(path))?;
            }
        }
        Command::Check => {
            let mut problems = Vec::new();
            for (path, content) in &expected {
                match std::fs::read_to_string(source_root.join(path)) {
                    Ok(current) if current == *content => {}
                    Ok(_) => problems.push(format!("differs: {}", path.display())),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        problems.push(format!("missing: {}", path.display()));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            problems.extend(
                stale
                    .iter()
                    .map(|path| format!("unexpected: {}", path.display())),
            );
            if !problems.is_empty() {
                return Err(format!(
                    "generated error source is stale ({}); run `cargo xtask errors generate`",
                    problems.join(", ")
                )
                .into());
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
        std::fs::write(
            directory.path().join("crates/rift-error/errors.toml"),
            REGISTRY,
        )
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

    const TWO_NAMESPACES: &str = r#"
[registry]
namespace = "rift"
schema = 1

[error.test.failure]
message = "failure for {field}"
action = "retry after checking {field}"
fields = { field = { type = "string" } }

[error.other.broken]
message = "broken"
action = "retry"
"#;

    fn source(root: &std::path::Path) -> std::path::PathBuf {
        root.join("crates/rift-error/src")
    }

    fn set_registry(root: &std::path::Path, registry: &str) {
        std::fs::write(root.join("crates/rift-error/errors.toml"), registry)
            .expect("write test registry");
    }

    #[test]
    fn generate_writes_parent_and_one_file_per_namespace_and_check_preserves_stale_source() {
        let directory = fixture();
        let root = directory.path();
        let parent = source(root).join("errors.rs");
        let file = source(root).join("errors/test.rs");

        execute(root, Command::Generate).expect("generate source");
        let generated = std::fs::read_to_string(&file).expect("read generated namespace");
        assert!(
            std::fs::read_to_string(&parent)
                .expect("read generated parent")
                .contains("pub mod test;")
        );
        execute(root, Command::Check).expect("check current source");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read generated source again"),
            generated,
            "check must not rewrite current source"
        );

        std::fs::write(&file, "stale").expect("write stale source");
        assert!(execute(root, Command::Check).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).expect("read stale source"),
            "stale",
            "check must not rewrite stale source"
        );

        execute(root, Command::Generate).expect("replace stale source");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read regenerated source"),
            generated
        );
    }

    #[test]
    fn adding_a_namespace_creates_its_file_and_check_fails_until_generated() {
        let directory = fixture();
        let root = directory.path();
        execute(root, Command::Generate).expect("generate source");
        set_registry(root, TWO_NAMESPACES);

        let message = execute(root, Command::Check)
            .expect_err("check must fail on a new namespace")
            .to_string();
        assert!(message.contains("missing: errors/other.rs"), "{message}");
        assert!(!source(root).join("errors/other.rs").exists());

        execute(root, Command::Generate).expect("generate added namespace");
        assert!(source(root).join("errors/other.rs").is_file());
        execute(root, Command::Check).expect("check after generate");
    }

    #[test]
    fn removing_a_namespace_deletes_its_file_and_check_fails_on_the_leftover() {
        let directory = fixture();
        let root = directory.path();
        set_registry(root, TWO_NAMESPACES);
        execute(root, Command::Generate).expect("generate two namespaces");
        let leftover = source(root).join("errors/other.rs");
        assert!(leftover.is_file());
        set_registry(root, REGISTRY);

        let message = execute(root, Command::Check)
            .expect_err("check must fail on a leftover file")
            .to_string();
        assert!(message.contains("unexpected: errors/other.rs"), "{message}");
        assert!(leftover.is_file(), "check must not delete files");

        execute(root, Command::Generate).expect("generate after removal");
        assert!(!leftover.exists());
        execute(root, Command::Check).expect("check after generate");
    }

    #[test]
    fn check_fails_on_a_hand_edited_file() {
        let directory = fixture();
        let root = directory.path();
        execute(root, Command::Generate).expect("generate source");
        let file = source(root).join("errors/test.rs");
        let mut edited = std::fs::read_to_string(&file).expect("read generated namespace");
        edited.push_str("// hand edit\n");
        std::fs::write(&file, edited).expect("edit generated namespace");

        let message = execute(root, Command::Check)
            .expect_err("check must fail on an edited file")
            .to_string();
        assert!(message.contains("differs: errors/test.rs"), "{message}");
    }

    #[test]
    fn check_fails_on_an_extra_file_and_generate_removes_it() {
        let directory = fixture();
        let root = directory.path();
        execute(root, Command::Generate).expect("generate source");
        let extra = source(root).join("errors/handwritten.rs");
        std::fs::write(&extra, "pub fn extra() {}\n").expect("write extra file");

        let message = execute(root, Command::Check)
            .expect_err("check must fail on an extra file")
            .to_string();
        assert!(
            message.contains("unexpected: errors/handwritten.rs"),
            "{message}"
        );

        execute(root, Command::Generate).expect("generate over extra file");
        assert!(!extra.exists());
        execute(root, Command::Check).expect("check after generate");
    }
}
