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
use std::collections::BTreeMap;

use crate::{
    generate::{generate, generate_module as render_module},
    validate::validate,
};

/// Parses, validates, and formats one registry as Rust source.
pub fn generate_source(source: &str) -> Result<String, CodegenError> {
    let registry = schema::parse(source)?;
    let registry = validate(registry)?;
    generate(&registry)
}

/// One registry as a parent module file and one file per namespace.
///
/// A namespace is the first path segment after the registry namespace, such as
/// `analysis` in `rift.analysis.context7_malformed`. The parent declares each
/// namespace with `pub mod <namespace>;`, so the registry lays out as `<module>.rs`
/// beside `<module>/<namespace>.rs`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedModule {
    /// Registry constants and the `pub mod` declarations, in namespace order.
    pub parent: String,
    /// File content keyed by namespace, sorted by namespace.
    pub namespaces: BTreeMap<String, String>,
}

/// Parses, validates, and formats one registry as a parent module and one file per namespace.
pub fn generate_module(source: &str) -> Result<GeneratedModule, CodegenError> {
    let registry = schema::parse(source)?;
    let registry = validate(registry)?;
    let module = render_module(&registry)?;
    Ok(GeneratedModule {
        parent: module.parent,
        namespaces: module.namespaces,
    })
}

#[cfg(test)]
mod tests {
    use super::{generate_module, generate_source};

    #[test]
    fn emits_compact_semantic_declarations() {
        let source = r#"
[registry]
namespace = "rift.cloud"
schema = 1

[error.auth.token_expired]
message = "token \"expired\" for {subject} under C:\\keys"
action = "renew token for {subject}"
fields = { subject = { type = "string" }, token = { type = "bool", optional = true, sensitive = true }, internal = { type = "string", optional = true, display = false } }

[error.auth.no_token]
message = "token is missing"
action = "provide a token"
"#;
        let generated = generate_source(source).expect("generate source");
        let expected = r#"use rift_error::__rift_error_definition;

#[doc(hidden)]
pub const REGISTRY_NAMESPACE: &str = "rift.cloud";
#[doc(hidden)]
pub const REGISTERED_SLUGS: &[&str] = &[
    "rift.cloud.auth.no_token",
    "rift.cloud.auth.token_expired",
];

/// Registered errors under `rift.cloud.auth`.
pub mod auth {
    use super::__rift_error_definition;

    __rift_error_definition!(
        no_token,
        slug = "rift.cloud.auth.no_token",
        message = "token is missing",
        action = "provide a token",
        fields = {},
    );

    __rift_error_definition!(
        token_expired,
        slug = "rift.cloud.auth.token_expired",
        message = "token \"expired\" for {subject} under C:\\keys",
        action = "renew token for {subject}",
        fields = {
            internal: optional(string, hidden),
            subject: required(string),
            token: optional(bool, sensitive),
        },
    );
}
"#;
        assert_eq!(generated, expected);
        assert_eq!(
            generated,
            generate_source(source).expect("generate same source")
        );
    }

    #[test]
    fn committed_registry_files_stay_within_generated_line_bound() {
        let module = generate_module(include_str!("../../rift-error/errors.toml"))
            .expect("generate committed registry");
        for (name, file) in &module.namespaces {
            assert!(
                file.lines().count() <= 1_000,
                "generated namespace file {name} exceeds 1,000 lines"
            );
        }
        assert!(
            module.parent.lines().count() <= 1_000,
            "generated parent module exceeds 1,000 lines"
        );
    }

    #[test]
    fn module_emits_one_file_per_namespace_in_sorted_order() {
        let source = r#"
[registry]
namespace = "rift"
schema = 1

[error.zeta.late]
message = "late"
action = "retry"

[error.alpha.second]
message = "second"
action = "retry"

[error.alpha.first]
message = "first"
action = "retry"
"#;
        let module = generate_module(source).expect("generate module");
        assert_eq!(
            module
                .namespaces
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        let expected_parent = r#"#[doc(hidden)]
pub const REGISTRY_NAMESPACE: &str = "rift";
#[doc(hidden)]
pub const REGISTERED_SLUGS: &[&str] = &[
    "rift.alpha.first",
    "rift.alpha.second",
    "rift.zeta.late",
];

/// Registered errors under `rift.alpha`.
pub mod alpha;

/// Registered errors under `rift.zeta`.
pub mod zeta;
"#;
        assert_eq!(module.parent, expected_parent);
        let expected_alpha = r#"use rift_error::__rift_error_definition;

__rift_error_definition!(
    first,
    slug = "rift.alpha.first",
    message = "first",
    action = "retry",
    fields = {},
);

__rift_error_definition!(
    second,
    slug = "rift.alpha.second",
    message = "second",
    action = "retry",
    fields = {},
);
"#;
        assert_eq!(module.namespaces["alpha"], expected_alpha);
        assert_eq!(module, generate_module(source).expect("generate again"));
    }

    #[test]
    fn module_declares_an_error_directly_under_the_registry_in_the_parent() {
        let source = r#"
[registry]
namespace = "rift"
schema = 1

[error.stopped]
message = "stopped"
action = "restart"

[error.alpha.first]
message = "first"
action = "retry"
"#;
        let module = generate_module(source).expect("generate module");
        assert_eq!(
            module
                .namespaces
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["alpha"]
        );
        let expected_parent = r#"use rift_error::__rift_error_definition;

#[doc(hidden)]
pub const REGISTRY_NAMESPACE: &str = "rift";
#[doc(hidden)]
pub const REGISTERED_SLUGS: &[&str] = &[
    "rift.alpha.first",
    "rift.stopped",
];

/// Registered errors under `rift.alpha`.
pub mod alpha;

__rift_error_definition!(
    stopped,
    slug = "rift.stopped",
    message = "stopped",
    action = "restart",
    fields = {},
);
"#;
        assert_eq!(module.parent, expected_parent);
    }

    #[test]
    fn module_files_are_rustfmt_stable() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let module = generate_module(include_str!("../../rift-error/errors.toml"))
            .expect("generate committed registry");
        for (name, file) in module
            .namespaces
            .iter()
            .map(|(name, file)| (name.as_str(), file))
            .chain([("parent", &module.parent)])
        {
            let mut rustfmt = Command::new("rustfmt")
                .args(["--edition", "2024", "--emit", "stdout"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("run rustfmt");
            rustfmt
                .stdin
                .take()
                .expect("rustfmt stdin")
                .write_all(file.as_bytes())
                .expect("write generated source to rustfmt");
            let output = rustfmt.wait_with_output().expect("wait for rustfmt");
            assert!(output.status.success(), "rustfmt rejected {name}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), *file, "{name}");
        }
    }

    #[test]
    fn generated_registry_is_rustfmt_stable() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let generated = generate_source(include_str!("../../rift-error/errors.toml"))
            .expect("generate committed registry");
        let mut rustfmt = Command::new("rustfmt")
            .args(["--edition", "2024", "--emit", "stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("run rustfmt");
        rustfmt
            .stdin
            .take()
            .expect("rustfmt stdin")
            .write_all(generated.as_bytes())
            .expect("write generated source to rustfmt");
        let output = rustfmt.wait_with_output().expect("wait for rustfmt");
        assert!(output.status.success(), "rustfmt rejected generated source");
        assert_eq!(String::from_utf8_lossy(&output.stdout), generated);
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
