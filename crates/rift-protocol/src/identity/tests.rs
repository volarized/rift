use super::{
    SYMBOL_ID_BYTES_MAX, SymbolIdentity, SymbolIdentityViolation, SymbolOccurrence, SymbolOwner,
};
use crate::read::Language;

const REVISION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn test_released_source_owner_and_path_round_trip() {
    for value in [
        "rift://source/cargo/crates.io/example@1.0.0/src/lib.rs",
        "rift://source/npm/npmjs.org/@types/node@26.6.2/fs.d.ts",
        "rift://source/npm/registry.example:8443%2Fteam%2Fapi/example@1.0.0/src/file%7E2.ts",
        "rift://source/stdlib/cpython@3.12.9/Lib/sys.py",
    ] {
        let (owner, path) =
            super::parse_released_source_identity(value).expect("canonical source owner");
        assert_eq!(
            super::released_source_identity(&owner, &path).expect("canonical source path"),
            value
        );
    }
    for invalid in [
        "rift://source/cargo/example@1.0.0/src/lib.rs",
        "rift://source/stdlib/python/Lib/sys.py",
        "rift://source/cargo/crates.io/example@1.0.0/../lib.rs",
        "rift://source/cargo/crates.io/example@1.0.0/%2Flib.rs",
        "rift://source/cargo/crates.io/example@1.0.0/%6Cib.rs",
        "rift://source/cargo/crates.io/example@1.0.0/lib.rs?rev=x",
        "rift://source/local/rust/lib.rs",
    ] {
        assert!(
            super::parse_released_source_identity(invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn test_source_unit_boundary_keeps_resolvers_paths_and_released_owners_distinct() {
    use crate::read::SourceUnitId;

    for (value, released) in [
        ("rift://source/project/src/file~2.rs", false),
        (
            "rift://source/project/registry.example/demo@1.0.0/file.rs",
            false,
        ),
        (
            "rift://source/custom/registry.example/demo@1.0.0/file.rs",
            false,
        ),
        ("rift://source/project/src/caf%C3%A9%20file.rs", false),
        ("rift://source/project/src/cafe%CC%81.rs", false),
        ("rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs", true),
        (
            "rift://source/npm/registry.example:8443%2Fnpm/@types/node@26.6.4/fs%7E2.d.ts",
            true,
        ),
        ("rift://source/stdlib/cpython@3.12.9/Lib/sys.py", true),
        (
            "rift://source/pypi/pypi.org/demo@1.0/notebook.ipynb/cell:2",
            true,
        ),
    ] {
        let (owner, _) = super::parse_source_unit_identity(value).expect("canonical source unit");
        assert_eq!(owner.is_some(), released, "{value}");
        let id = SourceUnitId::parse(value).expect("typed source unit");
        assert_eq!(id.as_str(), value);
        assert_eq!(
            serde_json::from_value::<SourceUnitId>(serde_json::json!(value))
                .expect("source unit JSON"),
            id
        );
    }
    for value in [
        "rift://source/project/",
        "rift://source/project/a//b",
        "rift://source/project/../file.rs",
        "rift://source/project/%2E/file.rs",
        "rift://source/project/src%2Flib.rs",
        "rift://source/project/src/%7E2.rs",
        "rift://source/project/src/%FF.rs",
        "rift://source/project/src/%G0.rs",
        "rift://source/project/src/%1.rs",
        "rift://source/project/src/%0A.rs",
        "rift://source/project/C:/file.rs",
        "rift://source/project/src%5Clib.rs",
        "rift://source/Project/file.rs",
        "rift://source/cargo/demo@1.0.0/lib.rs",
        "rift://source/stdlib/python/Lib/sys.py",
        "rift://source/npm/npmjs.org/demo@1.0.0/file~2.ts",
    ] {
        assert!(SourceUnitId::parse(value).is_err(), "{value}");
        assert!(
            serde_json::from_value::<SourceUnitId>(serde_json::json!(value)).is_err(),
            "{value}"
        );
    }
    let custom = SymbolOwner::Package {
        manager: "custom".into(),
        registry: "registry.example".into(),
        name: "demo".into(),
        version: "2026.10".into(),
    };
    assert!(super::released_source_identity(&custom, "file.rs").is_err());
}

#[test]
fn test_source_unit_boundary_checks_decoded_and_encoded_limits() {
    use crate::read::SourceUnitId;
    let exact_path = format!("rift://source/project/{}", "a".repeat(4096));
    assert!(SourceUnitId::parse(&exact_path).is_ok());
    assert!(SourceUnitId::parse(&format!("{exact_path}a")).is_err());
    let overhead = "rift://source/project/".len();
    let budget = SYMBOL_ID_BYTES_MAX - overhead;
    let encoded = format!("{}{}", "%25".repeat(budget / 3), "a".repeat(budget % 3));
    let exact_wire = format!("rift://source/project/{encoded}");
    assert_eq!(exact_wire.len(), SYMBOL_ID_BYTES_MAX);
    assert!(SourceUnitId::parse(&exact_wire).is_ok());
    assert_eq!(
        super::parse_source_unit_identity(&format!("{exact_wire}a")),
        Err(SymbolIdentityViolation::Length)
    );
}

#[test]
fn test_source_unit_schema_preserves_owner_boundaries_and_relative_paths() {
    use crate::read::SourceUnitId;
    let schema = serde_json::to_value(schemars::schema_for!(SourceUnitId)).expect("source schema");
    let validator = jsonschema::validator_for(&schema).expect("source pattern compiles");
    for (value, accepted) in [
        ("rift://source/project/src/file~2.rs", true),
        (
            "rift://source/custom/registry.example/demo@1.0.0/file.rs",
            true,
        ),
        (
            "rift://source/npm/registry.example:8443%2Fnpm/@types/node@26.6.4/fs%7E2.d.ts",
            true,
        ),
        ("rift://source/stdlib/cpython@3.12.9/Lib/sys.py", true),
        ("rift://source/project/src/cafe%CC%81.rs", true),
        ("rift://source/cargo/demo@1.0.0/src/lib.rs", false),
        ("rift://source/stdlib/python/3.12.9/Lib/sys.py", false),
        ("rift://source/project/src//lib.rs", false),
        ("rift://source/project/src/../lib.rs", false),
        ("rift://source/project/src%2Flib.rs", false),
        ("rift://source/project/src%2flib.rs", false),
        ("rift://source/project/src/%0A.rs", false),
        ("rift://source/project/C:/file.rs", false),
        ("rift://source/npm/npmjs.org/demo@1.0.0/file~2.ts", false),
    ] {
        assert_eq!(
            validator.is_valid(&serde_json::json!(value)),
            accepted,
            "schema: {value}"
        );
        assert_eq!(
            SourceUnitId::parse(value).is_ok(),
            accepted,
            "codec: {value}"
        );
    }
}

#[test]
fn test_source_digest_requires_full_lowercase_sha256() {
    let digest = super::SourceDigest::parse(&"a".repeat(64)).expect("full digest");
    assert_eq!(digest.as_str(), "a".repeat(64));
    assert_eq!(
        serde_json::from_str::<super::SourceDigest>(
            &serde_json::to_string(&digest).expect("serialize")
        )
        .expect("deserialize"),
        digest
    );
    for value in [
        "",
        "12345678",
        &"a".repeat(63),
        &"a".repeat(65),
        &"A".repeat(64),
        &"g".repeat(64),
    ] {
        assert!(super::SourceDigest::parse(value).is_err());
        assert!(serde_json::from_value::<super::SourceDigest>(serde_json::json!(value)).is_err());
    }
    let schema = schemars::schema_for!(super::SourceDigest);
    assert_eq!(schema.get("minLength"), Some(&serde_json::json!(64)));
    assert_eq!(schema.get("maxLength"), Some(&serde_json::json!(64)));
}

#[test]
fn test_typed_symbol_identity_conversion_preserves_wire_and_rejects_incomplete_owners() {
    let identity =
        SymbolIdentity::parse("rift://symbol/local/rust/app/parse").expect("logical identity");
    let wire = crate::read::SymbolId::from_identity(&identity);
    assert_eq!(
        SymbolIdentity::parse(wire.as_str()).expect("validated identity"),
        identity
    );
    assert_eq!(wire.into_string(), identity.wire_identity());
    for (registry, name) in [(":", "demo"), ("npmjs.org", "@scope")] {
        let owner = SymbolOwner::Package {
            manager: "npm".into(),
            registry: registry.into(),
            name: name.into(),
            version: "1.0.0".into(),
        };
        assert!(SymbolIdentity::new(owner, language(), vec!["demo".into()]).is_err());
    }
}

#[test]
fn test_released_source_full_encoded_bound() {
    let owner = SymbolOwner::Package {
        manager: "npm".to_owned(),
        registry: "registry.example".to_owned(),
        name: "example".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let prefix = "rift://source/npm/registry.example/example@1.0.0/";
    let path = "x".repeat(SYMBOL_ID_BYTES_MAX - prefix.len());
    let value = super::released_source_identity(&owner, &path).expect("encoded ceiling");
    assert_eq!(value.len(), SYMBOL_ID_BYTES_MAX);
    assert!(super::parse_released_source_identity(&value).is_ok());
    assert!(super::released_source_identity(&owner, &(path + "x")).is_err());
    assert!(super::parse_released_source_identity(&(value + "x")).is_err());
}

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
fn test_registry_endpoint_preserves_path_and_refuses_credentials_and_selectors() {
    for (input, expected) in [
        ("https://REGISTRY.example:443/", "registry.example"),
        (
            "https://registry.example/npm/releases",
            "registry.example/npm/releases",
        ),
        (
            "https://registry.example:8443/npm",
            "registry.example:8443/npm",
        ),
    ] {
        assert_eq!(
            super::canonical_registry_endpoint(input).expect("accepted endpoint"),
            expected
        );
    }
    for input in [
        "http://registry.example",
        "https://user:secret@registry.example/npm",
        "https://registry.example/npm?token=secret",
        "https://registry.example/npm#release",
        "https://registry.example/\n",
    ] {
        assert!(
            super::canonical_registry_endpoint(input).is_err(),
            "invalid endpoint"
        );
    }
    assert!(super::canonical_registry_endpoint(&"x".repeat(4097)).is_err());
}

#[test]
fn test_package_owner_requires_registry_and_runtime_has_no_registry() {
    use crate::read::{PackageIdentity, RuntimeIdentity};
    let json = serde_json::json!({"manager":"cargo", "registry":"crates.io", "name":"demo", "version":"1.0.0"});
    let package: PackageIdentity = serde_json::from_value(json.clone()).expect("package");
    assert!(
        matches!(package.owner().expect("owner"), SymbolOwner::Package { registry, .. } if registry == "crates.io")
    );
    let runtime = RuntimeIdentity {
        runtime: "cpython".into(),
        version: "3.12.9".into(),
    };
    assert!(matches!(
        runtime.owner().expect("runtime"),
        SymbolOwner::Runtime { .. }
    ));
    for registry in [
        "",
        "https://crates.io",
        "user:secret@crates.io",
        "crates.io?token=secret",
    ] {
        let mut invalid = json.clone();
        invalid["registry"] = serde_json::json!(registry);
        assert!(serde_json::from_value::<PackageIdentity>(invalid).is_err());
    }
    let mut absent = json;
    absent.as_object_mut().expect("object").remove("registry");
    assert!(serde_json::from_value::<PackageIdentity>(absent).is_err());
}

#[test]
fn test_full_endpoint_width_obeys_final_encoded_identity_bound() {
    let registry = format!(
        "registry.example/{}",
        "x".repeat(4096 - "registry.example/".len())
    );
    let owner = SymbolOwner::Package {
        manager: "cargo".into(),
        registry,
        name: "demo".into(),
        version: "1.0.0".into(),
    };
    owner.validate().expect("owner at endpoint bound");
    let base =
        SymbolIdentity::new(owner.clone(), language(), vec!["demo".into()]).expect("short path");
    let remaining = super::SYMBOL_ID_BYTES_MAX - base.wire_identity().len();
    let at = SymbolIdentity::new(
        owner.clone(),
        language(),
        vec![format!("demo{}", "x".repeat(remaining))],
    )
    .expect("final width");
    assert_eq!(at.wire_identity().len(), super::SYMBOL_ID_BYTES_MAX);
    assert!(
        SymbolIdentity::new(
            owner,
            language(),
            vec![format!("demo{}", "x".repeat(remaining + 1))]
        )
        .is_err()
    );
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
