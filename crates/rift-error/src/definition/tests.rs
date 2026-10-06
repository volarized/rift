#![allow(dead_code, unreachable_pub)]

use std::{error::Error as _, path::Path, time::Duration};

use crate::{ErrorSlug, RiftError};

mod declared {
    crate::__rift_error_definition!(
        every_kind,
        slug = "rift.test.every_kind",
        message = "{name} failed after {waited}",
        action = "retry {name}",
        fields = {
            cause: optional(cause),
            code: required(integer),
            count: required(unsigned),
            enabled: optional(bool),
            name: required(string),
            path: optional(path),
            pid: required(pid),
            port: optional(port),
            secret: optional(string, sensitive),
            source: optional(source),
            internal: optional(string, hidden),
            waited: required(duration),
        },
    );

    crate::__rift_error_definition!(
        fieldless,
        slug = "rift.test.fieldless",
        message = "ready",
        action = "continue",
        fields = {},
    );

    crate::__rift_error_definition!(
        required_only,
        slug = "rift.test.required_only",
        message = "{name} failed",
        action = "retry {name}",
        fields = { name: required(string) },
    );

    crate::__rift_error_definition!(
        optional_only,
        slug = "rift.test.optional_only",
        message = "lookup failed",
        action = "retry",
        fields = { key: optional(string), source: optional(source) },
    );

    crate::__rift_error_definition!(
        unordered,
        slug = "rift.test.unordered",
        message = "{b} then {a}",
        action = "retry",
        fields = { b: required(string), a: required(string), c: optional(string) },
    );
}

fn value(error: &RiftError, key: &str) -> Option<String> {
    error
        .context()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

fn every_kind() -> RiftError {
    declared::every_kind()
        .waited(Duration::from_millis(1500))
        .pid(41_u32)
        .name("index")
        .count(2_usize)
        .code(-5_i32)
        .maybe_path(Some(Path::new("src/lib.rs")))
        .maybe_port(Some(8080_u16))
        .maybe_enabled(Some(true))
        .maybe_secret(Some("token"))
        .maybe_internal(Some("detail"))
        .maybe_source(Some(std::io::Error::other("disk failed")))
        .maybe_cause(Some(declared::fieldless()))
        .error()
}

#[test]
fn declaration_expands_every_kind_into_its_value_conversion() {
    let error = every_kind();
    assert_eq!(error.slug(), ErrorSlug::new("rift.test.every_kind"));
    assert_eq!(declared::every_kind::SLUG, error.slug());
    assert_eq!(value(&error, "name").as_deref(), Some("index"));
    assert_eq!(value(&error, "count").as_deref(), Some("2"));
    assert_eq!(value(&error, "code").as_deref(), Some("-5"));
    assert_eq!(value(&error, "pid").as_deref(), Some("41"));
    assert_eq!(value(&error, "port").as_deref(), Some("8080"));
    assert_eq!(value(&error, "enabled").as_deref(), Some("true"));
    assert_eq!(value(&error, "path").as_deref(), Some("src/lib.rs"));
    assert_eq!(value(&error, "waited").as_deref(), Some("1.5s"));
    let keys = error
        .fields()
        .iter()
        .map(crate::ErrorContext::key)
        .collect::<Vec<_>>();
    assert!(
        keys.contains(&"source") && keys.contains(&"cause"),
        "{keys:?}"
    );
    let error = declared::optional_only()
        .maybe_source(Some(std::io::Error::other("disk failed")))
        .error();
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("disk failed")
    );
}

#[test]
fn flags_set_sensitive_and_hidden_evidence() {
    let error = every_kind();
    let field = |key: &str| {
        error
            .fields()
            .iter()
            .find(|field| field.key() == key)
            .expect("declared field is set")
    };
    assert!(field("secret").is_sensitive());
    assert!(field("secret").is_displayed());
    assert!(!field("internal").is_displayed());
    assert!(!field("internal").is_sensitive());
    assert!(field("name").is_displayed());
    assert!(!field("name").is_sensitive());
}

#[test]
fn fields_keep_declaration_order() {
    let error = declared::unordered().a("first").b("second").error();
    let keys = error
        .fields()
        .iter()
        .map(crate::ErrorContext::key)
        .collect::<Vec<_>>();
    assert_eq!(keys, ["b", "a"]);
    assert_eq!(error.message(), "second then first");
}

#[test]
fn optional_setter_clears_evidence_on_none() {
    let error = declared::optional_only()
        .key("present")
        .maybe_key(None::<&str>)
        .error();
    assert_eq!(value(&error, "key"), None);
    let error = declared::optional_only().maybe_key(Some("kept")).error();
    assert_eq!(value(&error, "key").as_deref(), Some("kept"));
}

#[test]
fn fieldless_and_required_only_declarations_complete() {
    let error = declared::fieldless().error();
    assert_eq!(error.message(), "ready");
    let result: Result<(), RiftError> = declared::required_only().name("index").fail();
    let error = result.expect_err("fail returns the registered error");
    assert_eq!(error.message(), "index failed");
}

#[test]
fn evidence_macro_maps_declared_fields() {
    struct Evidence {
        name: String,
    }

    crate::evidence! {
        Evidence => declared::required_only {
            name = name,
        }
    }

    let error = declared::required_only()
        .evidence(&Evidence {
            name: "index".to_owned(),
        })
        .error();
    assert_eq!(error.message(), "index failed");
}
