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
        assert!(generated.contains("__rift_error_definition!"));
        assert!(generated.contains("error token_expired;"));
        assert!(generated.contains("optional[maybe_token]"));
        assert!(generated.contains("states[State0]"));
        assert!(generated.contains("complete[SetState]"));
        assert!(generated.starts_with("pub use rift_error::{FieldSet, OptionalFieldSet};"));
        assert!(!generated.contains("::rift_error::"));
        assert!(!generated.contains("::std::"));
        assert!(!generated.contains("allow(unused_imports)"));
        assert!(generated.contains("error no_token;"));
        assert_eq!(
            generated,
            generate_source(source).expect("generate same source")
        );
    }

    #[test]
    fn committed_registry_stays_within_generated_line_bound() {
        let generated = generate_source(include_str!("../../rift-error/errors.toml"))
            .expect("generate committed registry");
        assert!(
            generated.lines().count() <= 10_000,
            "generated source exceeds 10,000 lines"
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
        fn contains_glob(tree: &syn::UseTree) -> bool {
            match tree {
                syn::UseTree::Glob(_) => true,
                syn::UseTree::Group(group) => group.items.iter().any(contains_glob),
                syn::UseTree::Path(path) => contains_glob(&path.tree),
                syn::UseTree::Name(_) | syn::UseTree::Rename(_) => false,
            }
        }
        fn assert_no_wildcard_imports(items: &[Item]) {
            for item in items {
                match item {
                    Item::Use(item) => assert!(
                        !contains_glob(&item.tree),
                        "wildcard import: {}",
                        item.to_token_stream()
                    ),
                    Item::Mod(item) => {
                        if let Some((_, items)) = &item.content {
                            assert_no_wildcard_imports(items);
                        }
                    }
                    _ => {}
                }
            }
        }
        assert_no_wildcard_imports(&file.items);
    }

    #[test]
    fn generated_cloud_namespace_compiles_against_runtime() {
        use std::fs;
        use std::path::PathBuf;
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
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("runtime CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root");
        let directory = workspace_root
            .join("target")
            .join(format!("rift-error-codegen-cloud-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create temporary directory");
        let manifest = "[workspace]\n\n[package]\nname = \"generated-cloud-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nrift-error = { path = \"../../../crates/rift-error\" }\n".to_owned();
        let compile = |name: &str, generated: &str, expression: &str| {
            let package = directory.join(name);
            let source = package.join("src");
            fs::create_dir_all(&source).expect("create generated fixture source directory");
            fs::write(package.join("Cargo.toml"), &manifest)
                .expect("write generated fixture manifest");
            fs::write(
                source.join("lib.rs"),
                format!(
                    "#![deny(warnings)]\npub mod cloud {{\n{generated}\n}}\npub fn downstream() -> rift_error::RiftError {{ {expression} }}\n"
                ),
            )
            .expect("write generated fixture");
            Command::new("cargo")
                .arg("check")
                .arg("--offline")
                .arg("--manifest-path")
                .arg(package.join("Cargo.toml"))
                .arg("--target-dir")
                .arg(directory.join("target"))
                .output()
                .expect("run cargo check for generated cloud registry")
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
        for (name, expression, trait_bound) in [
            (
                "cloud-bad-bool",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).maybe_token(Some(\"true\")).error()",
                "Borrow<bool>",
            ),
            (
                "cloud-bad-unsigned",
                "cloud::auth::token_expired().subject(\"token\").count(\"2\").code(-5_i32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
                "IntoUnsigned",
            ),
            (
                "cloud-bad-integer",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(5_u32).pid(41_u32).port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
                "IntoInteger",
            ),
            (
                "cloud-bad-pid",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(\"41\").port(8080_u16).path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
                "Borrow<u32>",
            ),
            (
                "cloud-bad-port",
                "cloud::auth::token_expired().subject(\"token\").count(2_usize).code(-5_i32).pid(41_u32).port(\"8080\").path(std::path::Path::new(\"src/lib.rs\")).waited(std::time::Duration::from_secs(1)).source(std::io::Error::other(\"source\")).error()",
                "Borrow<u16>",
            ),
        ] {
            let invalid = compile(name, &generated, expression);
            let stderr = String::from_utf8_lossy(&invalid.stderr);
            assert!(
                !invalid.status.success()
                    && stderr.contains("error[E0277]")
                    && stderr.contains(trait_bound),
                "invalid {name} must fail on its evidence type, got:\n{stderr}"
            );
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
