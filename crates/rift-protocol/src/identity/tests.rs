use super::{
    SYMBOL_ID_BYTES_MAX, SymbolIdentity, SymbolIdentityViolation, SymbolOccurrence, SymbolOwner,
};
use crate::read::Language;

const REVISION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn language() -> Language {
    Language::from_identity_segment("rust").expect("language fixture")
}

#[test]
fn test_canonical_owners_round_trip_without_losing_hierarchy() {
    for value in [
        "rift://symbol/local/rust/app/parser/parse",
        "rift://symbol/local@service/rust/app/parser/parse",
        "rift://symbol/cargo/crates.io/tokio@1.53.2/rust/tokio/sync/mpsc/Sender",
        "rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node/buffer/Buffer",
        "rift://symbol/pypi/pypi.org/click@8.3.3/python/click/core/Command",
        "rift://symbol/pypi/pypi.org/demo@1!2.0.post1+local/python/demo/Callable",
        "rift://symbol/stdlib/cpython@3.12.9/python/builtins/len",
        "rift://symbol/npm/artifacts.example%2Fnpm/demo@1.0.0/javascript/demo/export",
        "rift://symbol/custom/registry.example/demo@2026.10/rust/demo/export",
        "rift://symbol/local/typescript:tsx/app/components/Button",
    ] {
        let identity =
            SymbolIdentity::parse(value).unwrap_or_else(|error| panic!("{value}: {error:?}"));
        assert_eq!(identity.wire_identity(), value);
        let json = serde_json::to_string(&identity).expect("serialize identity");
        assert_eq!(
            serde_json::from_str::<SymbolIdentity>(&json).expect("deserialize identity"),
            identity
        );
        assert!(identity.occurrence().is_none());
    }
    let package = SymbolIdentity::parse(
        "rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node/buffer/Buffer",
    )
    .expect("scoped package");
    assert_eq!(
        package.owner(),
        &SymbolOwner::Package {
            manager: "npm".into(),
            registry: "npmjs.org".into(),
            name: "@types/node".into(),
            version: "26.6.4".into()
        }
    );
    assert_eq!(package.qualified_path(), ["node", "buffer", "Buffer"]);
    assert_eq!(package.language().name, "typescript");
}

#[test]
fn test_component_escaping_preserves_data_and_rejects_alternate_spellings() {
    let identity = SymbolIdentity::new(
        SymbolOwner::Local,
        language(),
        vec!["app".into(), "café/part%~2".into()],
    )
    .expect("escaped names");
    let expected = "rift://symbol/local/rust/app/caf%C3%A9%2Fpart%25%7E2";
    assert_eq!(identity.wire_identity(), expected);
    assert_eq!(
        SymbolIdentity::parse(expected).expect("canonical names"),
        identity
    );
    for value in [
        "%61pp",
        "caf%c3%a9",
        "café",
        "name%",
        "name%GG",
        "name~2",
        "name?view=latest",
        "name#1234",
    ] {
        let value = format!("rift://symbol/local/rust/app/{value}");
        assert_eq!(
            SymbolIdentity::parse(&value).err(),
            Some(SymbolIdentityViolation::Noncanonical),
            "{value}"
        );
    }
    assert_eq!(
        SymbolIdentity::parse("rift://symbol/local/rust/app/%FF").err(),
        Some(SymbolIdentityViolation::Encoding)
    );
}

#[test]
fn test_occurrences_require_complete_revisions_and_canonical_positive_numbers() {
    let occurrence = SymbolOccurrence::new(2, REVISION.into()).expect("occurrence fixture");
    let identity = SymbolIdentity::new(
        SymbolOwner::Local,
        language(),
        vec!["app".into(), "anonymous".into()],
    )
    .expect("identity fixture")
    .with_occurrence(occurrence)
    .expect("bind occurrence");
    let expected = format!("rift://symbol/local/rust/app/anonymous~2?rev={REVISION}");
    assert_eq!(identity.wire_identity(), expected);
    assert_eq!(
        SymbolIdentity::parse(&expected).expect("parse occurrence"),
        identity
    );
    assert_eq!(identity.occurrence().expect("bound occurrence").number(), 2);
    assert_eq!(
        identity.occurrence().expect("bound occurrence").revision(),
        REVISION
    );
    let literal = SymbolIdentity::new(SymbolOwner::Local, language(), vec!["anonymous~2".into()])
        .expect("literal suffix");
    assert_eq!(
        literal.wire_identity(),
        "rift://symbol/local/rust/anonymous%7E2"
    );
    let literal = literal
        .with_occurrence(
            SymbolOccurrence::new(u32::MAX, REVISION.into()).expect("maximum occurrence"),
        )
        .expect("literal name occurrence");
    assert_eq!(
        SymbolIdentity::parse(&literal.wire_identity()).expect("literal name and qualifier"),
        literal
    );
    for (suffix, violation) in [
        ("~0?rev=1234", SymbolIdentityViolation::Occurrence),
        ("~4294967296?rev=1234", SymbolIdentityViolation::Occurrence),
        ("~2?rev=1234", SymbolIdentityViolation::Revision),
        ("?rev=1234", SymbolIdentityViolation::Occurrence),
        ("~2?rev=", SymbolIdentityViolation::Revision),
        ("~2/Child?rev=1234", SymbolIdentityViolation::Occurrence),
    ] {
        assert_eq!(
            SymbolIdentity::parse(&format!("rift://symbol/local/rust/app/anonymous{suffix}")).err(),
            Some(violation)
        );
    }
    let leading_zero = format!("rift://symbol/local/rust/app/anonymous~02?rev={REVISION}");
    assert_eq!(
        SymbolIdentity::parse(&leading_zero).err(),
        Some(SymbolIdentityViolation::Noncanonical)
    );
    let extra_query =
        format!("rift://symbol/local/rust/app/anonymous~2?rev={REVISION}&rev={REVISION}");
    assert_eq!(
        SymbolIdentity::parse(&extra_query).err(),
        Some(SymbolIdentityViolation::Revision)
    );
    assert_eq!(
        SymbolOccurrence::new(2, REVISION.to_uppercase()).err(),
        Some(SymbolIdentityViolation::Revision)
    );
}

#[test]
fn test_invalid_structure_names_owners_and_versions_are_refused() {
    for value in [
        "rift://symbol/rust/src/lib.rs/Name",
        "rift://symbol/local/rust",
        "rift://symbol/local//Name",
        "rift://file/README.md",
        "rift://symbol/local@/rust/app/Name",
        "rift://symbol/local@all/rust/app/Name",
        "rift://symbol/local@Upper/rust/app/Name",
        "rift://symbol/global/registry.example/demo@1.0.0/rust/Name",
        "rift://symbol/local/rust/",
        "rift://symbol/local/rust/app//Name",
        "rift://symbol/local/rust/app/../Name",
        "rift://symbol/local/rust/app/%2E/Name",
        "rift://symbol/local/rust/app/%00",
        "rift://symbol/local/rust/app/%0A",
        "rift://symbol/local/Rust/app/Name",
        "rift://symbol/local/typescript::tsx/app/Name",
        "rift://symbol/cargo/crates.io/demo@1.0/rust/Name",
        "rift://symbol/cargo/crates.io/demo@^1.0.0/rust/Name",
        "rift://symbol/pypi/pypi.org/demo@>=1/python/Name",
        "rift://symbol/pypi/pypi.org/Demo@1.0/python/Name",
        "rift://symbol/pypi/pypi.org/demo_name@1.0/python/Name",
        "rift://symbol/npm/npmjs.org/@scope/demo/Name",
        "rift://symbol/npm/npmjs.org/@scope/name/extra@1.0.0/javascript/Name",
        "rift://symbol/npm/user:secret@registry.example/demo@1.0.0/javascript/Name",
        "rift://symbol/npm/REGISTRY.EXAMPLE/demo@1.0.0/javascript/Name",
        "rift://symbol/npm/registry.example%2F..%2Fnpm/demo@1.0.0/javascript/Name",
        "rift://symbol/npm/registry.example%3Ftoken=secret/demo@1.0.0/javascript/Name",
        "rift://symbol/npm/registry.example%23data/demo@1.0.0/javascript/Name",
        "rift://symbol/stdlib/cpython@3.12/python/Name",
    ] {
        assert!(SymbolIdentity::parse(value).is_err(), "{value}");
        assert!(
            serde_json::from_value::<SymbolIdentity>(serde_json::json!(value)).is_err(),
            "{value}"
        );
    }
}

#[test]
fn test_length_bound_applies_before_decode_and_after_encoding() {
    let prefix = "rift://symbol/local/rust/";
    let exact = format!("{prefix}{}", "a".repeat(SYMBOL_ID_BYTES_MAX - prefix.len()));
    assert!(SymbolIdentity::parse(&exact).is_ok());
    assert_eq!(
        SymbolIdentity::parse(&(exact.clone() + "a")).err(),
        Some(SymbolIdentityViolation::Length)
    );
    assert_eq!(
        SymbolIdentity::new(
            SymbolOwner::Local,
            language(),
            vec!["é".repeat(SYMBOL_ID_BYTES_MAX / 3)]
        )
        .err(),
        Some(SymbolIdentityViolation::Length)
    );
    assert_eq!(
        SymbolIdentity::new(
            SymbolOwner::Local,
            language(),
            vec![String::new(); SYMBOL_ID_BYTES_MAX + 1]
        )
        .err(),
        Some(SymbolIdentityViolation::Length)
    );
    let identity = SymbolIdentity::parse(&exact).expect("exact bound");
    assert_eq!(
        identity
            .with_occurrence(SymbolOccurrence::new(1, REVISION.into()).expect("occurrence"))
            .err(),
        Some(SymbolIdentityViolation::Length)
    );
}

#[test]
fn test_construction_and_registration_use_same_acceptance() {
    for name in ["service", "app_2", "my-app"] {
        assert!(super::valid_registration_name(name));
    }
    for name in [
        "", "local", "global", "all", "1app", "APP", "app/path", "app@name",
    ] {
        assert!(!super::valid_registration_name(name));
    }
    let named = SymbolIdentity::new(
        SymbolOwner::NamedLocal {
            name: "service".into(),
        },
        language(),
        vec!["app".into()],
    )
    .expect("named identity");
    assert_eq!(
        named.wire_identity(),
        "rift://symbol/local@service/rust/app"
    );
    assert_eq!(
        SymbolIdentity::new(SymbolOwner::Local, language(), vec![]).err(),
        Some(SymbolIdentityViolation::QualifiedPath)
    );
    let malformed_language = Language {
        name: "Rust".into(),
        dialect: None,
    };
    assert_eq!(
        SymbolIdentity::new(SymbolOwner::Local, malformed_language, vec!["Name".into()]).err(),
        Some(SymbolIdentityViolation::Language)
    );
}
