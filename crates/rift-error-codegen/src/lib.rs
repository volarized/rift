//! Typed generation for Rift error registries.
//!
//! Generated builders reject completion while required evidence is unset:
//!
//! ```
//! use rift_error::errors;
//! let _ = errors::ranking::query_length()
//!     .field("query")
//!     .subject("query")
//!     .limit(4_usize)
//!     .required(5_usize)
//!     .error();
//! ```
//!
//! Optional evidence can be supplied without changing builder state:
//!
//! ```
//! use rift_error::errors;
//! let _ = errors::ranking::query_empty()
//!     .maybe_subject(Some("query"))
//!     .error();
//! ```
//!
//! ```compile_fail
//! use rift_error::errors;
//! let _ = errors::ranking::query_length().field("query").error();
//! ```
//!
//! Evidence mappings reject fields absent from the registered error:
//!
//! ```
//! use rift_error::{errors, evidence};
//! struct Evidence { subject: String }
//! evidence! {
//!     Evidence => errors::ranking::query_empty {
//!         subject = subject,
//!     }
//! }
//! let _ = errors::ranking::query_empty().evidence(&Evidence {
//!     subject: String::from("query"),
//! }).error();
//! ```
//!
//! ```compile_fail
//! use rift_error::{errors, evidence};
//! struct Evidence;
//! evidence! {
//!     Evidence => errors::ranking::query_empty {
//!         misspelled = field,
//!     }
//! }
//! ```
//!
//! Source fields require values that implement `std::error::Error`:
//!
//! ```
//! use rift_error::{errors, evidence};
//! #[derive(Clone, Debug)]
//! struct Source;
//! impl std::fmt::Display for Source {
//!     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//!         f.write_str("disk failed")
//!     }
//! }
//! impl std::error::Error for Source {}
//! struct Evidence { source: Source }
//! evidence! {
//!     Evidence => errors::ranking::reader_failed {
//!         source => |evidence| evidence.source.clone(),
//!     }
//! }
//! let _ = errors::ranking::reader_failed()
//!     .evidence(Evidence { source: Source })
//!     .error();
//! ```
//!
//! ```compile_fail
//! use rift_error::{errors, evidence};
//! struct Evidence { source: String }
//! evidence! {
//!     Evidence => errors::ranking::reader_failed {
//!         source = source,
//!     }
//! }
//! ```

mod generate;
mod ir;
mod schema;
mod validate;

pub use crate::validate::CodegenError;
use crate::{generate::generate, validate::validate};

/// Parses, validates, and formats one registry as Rust source.
pub fn generate_source(source: &str) -> Result<String, CodegenError> {
    let registry = schema::parse(source)?;
    let registry = validate(registry)?;
    generate(&registry)
}

#[cfg(test)]
mod tests {
    use super::generate_source;

    #[test]
    fn emits_stable_nested_modules_and_required_setters() {
        let source = r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.auth.token_expired]
message = "token expired for {subject}"
action = "renew token for {subject}"
        fields = { subject = { type = "string" }, token = { type = "bool", optional = true, sensitive = true } }

[error.auth.no_token]
message = "token is missing"
action = "provide a token"
"#;
        let generated = generate_source(source).expect("generate source");
        assert!(generated.contains("rift.cloud.auth.token_expired"));
        assert!(generated.contains("REGISTERED_SLUGS"));
        assert!(generated.contains("pub fn token_expired"));
        assert!(generated.contains(
            "pub const SLUG: ErrorSlug = ErrorSlug::new(\"rift.cloud.auth.token_expired\")"
        ));
        assert!(generated.contains("pub fn maybe_token"));
        assert!(generated.contains("pub type EvidenceInput"));
        assert!(generated.contains("pub fn fail<T>"));
        assert!(generated.starts_with("pub use rift_error::{FieldSet, OptionalFieldSet};"));
        assert!(!generated.contains("::rift_error::"));
        assert!(!generated.contains("::std::"));
        assert!(!generated.contains("allow(unused_imports)"));
        assert!(generated.contains("pub fn no_token"));
        assert_eq!(
            generated,
            generate_source(source).expect("generate same source")
        );
    }

    #[test]
    fn generated_imports_are_grouped_unique_and_ordered() {
        use quote::ToTokens as _;
        use syn::Item;

        let source = r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.auth.token_expired]
message = "token expired for {subject}"
action = "renew token for {subject}"
fields = { subject = { type = "string" }, token = { type = "bool", optional = true, sensitive = true }, pid = { type = "pid" }, port = { type = "port" } }
"#;
        let generated = generate_source(source).expect("generate source");
        let file = syn::parse_file(&generated).expect("generated Rust parses");
        let imports = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Use(item) if matches!(item.vis, syn::Visibility::Inherited) => {
                    Some(item.to_token_stream().to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            imports.len(),
            2,
            "one grouped import per namespace: {imports:?}"
        );
        assert!(imports[0].starts_with("use rift_error"), "{imports:?}");
        assert!(imports[1].starts_with("use std"), "{imports:?}");
        let mut sorted = imports.clone();
        sorted.sort();
        assert_eq!(imports, sorted, "imports sort by namespace: {imports:?}");
        let mut unique = imports.clone();
        unique.dedup();
        assert_eq!(imports, unique, "imports have no duplicates: {imports:?}");
        assert!(imports[1].contains("borrow :: Borrow"), "{imports:?}");
    }

    #[test]
    fn generated_cloud_namespace_compiles_against_runtime() {
        use std::fs;
        use std::process::Command;

        let registry = r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.auth.token_expired]
message = "token expired for {subject}"
action = "renew token for {subject}"
        fields = {
            subject = { type = "string" },
            token = { type = "bool", optional = true, sensitive = true },
            count = { type = "unsigned" },
            code = { type = "integer" },
            pid = { type = "pid" },
            port = { type = "port" },
            path = { type = "path", format = "display" },
            waited = { type = "duration", format = "human" },
            source = { type = "error", role = "source" },
            cause = { type = "rift_error", role = "cause", optional = true },
        }
"#;
        let generated = generate_source(registry).expect("generate cloud registry");
        let directory =
            std::env::temp_dir().join(format!("rift-error-codegen-cloud-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create temporary directory");
        let dependencies = std::env::current_exe()
            .expect("current test executable")
            .parent()
            .expect("test executable directory")
            .to_path_buf();
        let runtime = fs::read_dir(&dependencies)
            .expect("read test dependency directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("librift_error-") && name.ends_with(".rlib")
                    })
            })
            .expect("compiled rift-error library");
        let compile = |name: &str, generated: &str, expression: &str| {
            let source = directory.join(format!("{name}.rs"));
            let output = directory.join(format!("{name}.rlib"));
            fs::write(
                &source,
                format!(
                    "#![deny(warnings)]\npub mod cloud {{\n{generated}\n}}\npub fn downstream() -> rift_error::RiftError {{ {expression} }}\n"
                ),
            )
            .expect("write generated fixture");
            Command::new("rustc")
                .arg("--edition=2024")
                .arg("--crate-type=lib")
                .arg("-Dwarnings")
                .arg("--extern")
                .arg(format!("rift_error={}", runtime.display()))
                .arg("-L")
                .arg(format!("dependency={}", dependencies.display()))
                .arg(&source)
                .arg("-o")
                .arg(&output)
                .output()
                .expect("run rustc for generated cloud registry")
        };
        let valid = compile(
            "cloud-valid",
            &generated,
            "cloud::auth::token_expired().subject(\"access token\").count(2_usize).code(-5_i32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).maybe_cause(Some(rift_error::errors::ranking::query_empty())).maybe_token(Some(true)).error()",
        );
        assert!(
            valid.status.success(),
            "valid generated evidence did not compile:\n{}",
            String::from_utf8_lossy(&valid.stderr)
        );
        for (name, expression) in [
            (
                "cloud-bad-bool",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).maybe_token(Some(\"true\")).error()",
            ),
            (
                "cloud-bad-unsigned",
                "cloud::auth::token_expired().subject(\"token\").count(\"2\").code(-5_i32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
            ),
            (
                "cloud-bad-integer",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(5_u32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
            ),
            (
                "cloud-bad-pid",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(\"41\").port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
            ),
            (
                "cloud-bad-port",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(41_u32).port(\"8080\").path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
            ),
        ] {
            let invalid = compile(name, &generated, expression);
            assert!(!invalid.status.success(), "invalid {name} compiled");
        }
        let fieldless = generate_source(
            r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.simple.ready]
message = "ready"
action = "continue"
"#,
        )
        .expect("generate fieldless registry");
        let fieldless_valid = compile(
            "cloud-fieldless",
            &fieldless,
            "cloud::simple::ready().error()",
        );
        assert!(
            fieldless_valid.status.success(),
            "fieldless generated registry did not compile under deny(warnings):\n{}",
            String::from_utf8_lossy(&fieldless_valid.stderr)
        );
        let required_only = generate_source(
            r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.simple.named]
message = "{name} failed"
action = "retry {name}"
fields = { name = { type = "string" } }
"#,
        )
        .expect("generate required-only registry");
        let required_valid = compile(
            "cloud-required-only",
            &required_only,
            "cloud::simple::named().name(\"index\").error()",
        );
        assert!(
            required_valid.status.success(),
            "required-only generated registry did not compile under deny(warnings):\n{}",
            String::from_utf8_lossy(&required_valid.stderr)
        );
        for (name, field, value) in [
            ("cloud-pid-only", "pid", "41_u32"),
            ("cloud-port-only", "port", "8080_u16"),
        ] {
            let registry = format!(
                "[registry]\nnamespace = \"rift.cloud\"\nschema = 1\n\n[error.simple.scalar]\nmessage = \"scalar rejected\"\naction = \"supply valid scalar\"\nfields = {{ {field} = {{ type = \"{field}\" }} }}\n"
            );
            let generated = generate_source(&registry).expect("generate scalar-only registry");
            let expression = format!("cloud::simple::scalar().{field}({value}).error()");
            let scalar = compile(name, &generated, &expression);
            assert!(
                scalar.status.success(),
                "{name} generated registry did not compile under deny(warnings):\n{}",
                String::from_utf8_lossy(&scalar.stderr)
            );
        }
        let _ = fs::remove_dir_all(&directory);
    }
}
