//! Local scopes reuse canonical owner parsing and registration validation.

use super::*;

#[test]
fn local_scope_matches_canonical_symbol_owner() {
    for scope in [
        "local",
        "local@cloud",
        "local@tools_2",
        "local@cloud-tools",
        "local:docs",
        "local:docs_2",
        "local:docs-api",
        "local@cloud:docs",
        "local@cloud-tools:docs_2",
    ] {
        let owner = parse_local_scope(scope).expect("accepted local scope");
        assert_eq!(owner.local_scope().expect("canonical local scope"), scope);
        let identity = SymbolIdentity::new(
            owner.clone(),
            Language::from_identity_segment("rust").expect("language"),
            vec!["beacon".to_owned()],
        )
        .expect("canonical logical identity");
        assert_eq!(
            SymbolIdentity::parse(&identity.wire_identity())
                .expect("canonical owner roundtrip")
                .owner(),
            &owner,
        );
        match owner {
            SymbolOwner::NamedLocal { name } | SymbolOwner::ProfileLocal { name } => {
                assert!(valid_registration_name(&name));
            }
            SymbolOwner::NamedProfileLocal { project, name } => {
                assert!(valid_registration_name(&project));
                assert!(valid_registration_name(&name));
            }
            _ => assert_eq!(scope, "local"),
        }
    }
}

#[test]
fn local_scope_renderer_refuses_released_invalid_and_overbound_owners() {
    for owner in [
        SymbolOwner::Runtime {
            runtime: "rust".to_owned(),
            version: "1.98.1".to_owned(),
        },
        SymbolOwner::Package {
            manager: "npm".to_owned(),
            registry: "registry.npmjs.org".to_owned(),
            name: "beacon".to_owned(),
            version: "1.0.0".to_owned(),
        },
        SymbolOwner::NamedLocal {
            name: "Cloud".to_owned(),
        },
    ] {
        assert_eq!(
            owner
                .local_scope()
                .map_err(|error| super::tests::error_violation(&error)),
            Err("Owner".to_owned())
        );
    }
    let prefix_bytes = "local@".len();
    let accepted = SymbolOwner::NamedLocal {
        name: "x".repeat(SYMBOL_ID_BYTES_MAX - prefix_bytes),
    };
    let scope = accepted.local_scope().expect("exact scope byte bound");
    assert_eq!(scope.len(), SYMBOL_ID_BYTES_MAX);
    assert_eq!(
        parse_local_scope(&scope).expect("scope roundtrip"),
        accepted
    );
    let refused = SymbolOwner::NamedLocal {
        name: "x".repeat(SYMBOL_ID_BYTES_MAX - prefix_bytes + 1),
    };
    assert_eq!(
        refused
            .local_scope()
            .map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
}

#[test]
fn invalid_local_scope_cannot_select_primary_or_release_owner() {
    for scope in [
        "",
        "all",
        "global",
        "Local",
        "local@",
        "local@local",
        "local@global",
        "local@all",
        "local@Cloud",
        "local@cloud/path",
        "local@%63loud",
        "local@cloud ",
        "local@cloud\n",
        "local@a.b",
        "stdlib/rust@1.98.1",
        "npm/registry.npmjs.org/beacon@1.0.0",
    ] {
        assert!(parse_local_scope(scope).is_err(), "{scope:?}");
    }
    let overbound = format!("local@{}", "x".repeat(SYMBOL_ID_BYTES_MAX));
    assert_eq!(
        parse_local_scope(&overbound).map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
}

#[test]
fn profile_scope_and_extra_scope_keep_same_name_distinct() {
    let profile = parse_local_scope("local:docs").expect("profile scope");
    let extra = parse_local_scope("local@docs").expect("extra scope");
    assert_ne!(profile, extra);
    assert_eq!(
        profile,
        SymbolOwner::ProfileLocal {
            name: "docs".to_owned()
        }
    );
    let identity = SymbolIdentity::new(
        profile,
        Language::from_identity_segment("rust").expect("language"),
        vec!["app".to_owned(), "load".to_owned()],
    )
    .expect("profile identity");
    assert_eq!(
        identity.wire_identity(),
        "rift://symbol/local:docs/rust/app/load"
    );
    assert_eq!(
        SymbolIdentity::parse(&identity.wire_identity()).expect("profile identity parses"),
        identity
    );
    for invalid in [
        "rift://symbol/local#docs/rust/app/load",
        "rift://symbol/local%23docs/rust/app/load",
        "rift://symbol/local%3Adocs/rust/app/load",
        "rift://symbol/local:Docs/rust/app/load",
        "rift://symbol/local:all/rust/app/load",
    ] {
        assert!(SymbolIdentity::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn profile_scope_refuses_reserved_names_and_accepts_exact_byte_bound() {
    for name in [
        "", "local", "global", "all", "Docs", "docs/api", "%64ocs", "docs ", "docs\n", "docs.api",
    ] {
        let scope = format!("local:{name}");
        assert!(parse_local_scope(&scope).is_err(), "{scope:?}");
        assert!(
            SymbolOwner::ProfileLocal {
                name: name.to_owned()
            }
            .local_scope()
            .is_err()
        );
    }
    let name = "x".repeat(SYMBOL_ID_BYTES_MAX - "local:".len());
    let owner = SymbolOwner::ProfileLocal { name: name.clone() };
    let scope = owner.local_scope().expect("exact bound accepted");
    assert_eq!(scope.len(), SYMBOL_ID_BYTES_MAX);
    assert_eq!(
        parse_local_scope(&scope).expect("exact bound parses"),
        owner
    );
    let overbound = format!("{scope}x");
    assert_eq!(
        parse_local_scope(&overbound).map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
    assert_eq!(
        SymbolOwner::ProfileLocal {
            name: format!("{name}x")
        }
        .local_scope()
        .map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
}

#[test]
fn profile_symbol_bound_counts_scope_separator() {
    let fixed = format!("{SYMBOL_URI_PREFIX}local:/rust/beacon").len();
    let name = "x".repeat(SYMBOL_ID_BYTES_MAX - fixed);
    let language = Language::from_identity_segment("rust").expect("language");
    let identity = SymbolIdentity::new(
        SymbolOwner::ProfileLocal { name: name.clone() },
        language.clone(),
        vec!["beacon".to_owned()],
    )
    .expect("exact bound");
    assert_eq!(identity.wire_identity().len(), SYMBOL_ID_BYTES_MAX);
    assert_eq!(
        SymbolIdentity::parse(&identity.wire_identity()).expect("exact bound parses"),
        identity
    );
    let overbound = SymbolIdentity::new(
        SymbolOwner::ProfileLocal {
            name: format!("{name}x"),
        },
        language,
        vec!["beacon".to_owned()],
    );
    assert!(
        overbound.is_err(),
        "owner separator counts toward the symbol bound"
    );
}

#[test]
fn extra_profile_scope_keeps_extra_primary_and_root_profile_distinct() {
    let extra_profile = parse_local_scope("local@cloud:docs").expect("extra profile scope");
    assert_eq!(
        extra_profile,
        SymbolOwner::NamedProfileLocal {
            project: "cloud".to_owned(),
            name: "docs".to_owned(),
        }
    );
    let extra = parse_local_scope("local@cloud").expect("extra scope");
    let profile = parse_local_scope("local:docs").expect("profile scope");
    assert_ne!(extra_profile, extra);
    assert_ne!(extra_profile, profile);
    let identity = SymbolIdentity::new(
        extra_profile,
        Language::from_identity_segment("rust").expect("language"),
        vec!["app".to_owned(), "load".to_owned()],
    )
    .expect("extra profile identity");
    assert_eq!(
        identity.wire_identity(),
        "rift://symbol/local@cloud:docs/rust/app/load"
    );
    assert_eq!(
        SymbolIdentity::parse(&identity.wire_identity()).expect("extra profile identity parses"),
        identity
    );
    let schema = serde_json::to_value(schemars::schema_for!(SymbolIdentity)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("identity schema compiles");
    assert!(validator.is_valid(&serde_json::json!(identity.wire_identity())));
    for invalid in [
        "rift://symbol/local@cloud#docs/rust/app/load",
        "rift://symbol/local@cloud%23docs/rust/app/load",
        "rift://symbol/local@cloud%3Adocs/rust/app/load",
        "rift://symbol/local@cloud:Docs/rust/app/load",
        "rift://symbol/local@cloud:all/rust/app/load",
        "rift://symbol/local@all:docs/rust/app/load",
        "rift://symbol/local@cloud:/rust/app/load",
        "rift://symbol/local@:docs/rust/app/load",
        "rift://symbol/local@cloud:docs:api/rust/app/load",
    ] {
        assert!(SymbolIdentity::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn extra_profile_scope_refuses_reserved_and_noncanonical_spellings() {
    for scope in [
        "local@:docs",
        "local@cloud:",
        "local@cloud:all",
        "local@global:docs",
        "local@cloud:Docs",
        "local@cloud:docs:api",
        "local@cloud#docs",
        "local@cloud%23docs",
        "local:docs@cloud",
        "local#docs",
        "local%23docs",
        "local@cloud@docs",
    ] {
        assert!(parse_local_scope(scope).is_err(), "{scope:?}");
    }
}

#[test]
fn extra_profile_scope_accepts_exact_byte_bound() {
    let fixed = "local@cloud:".len();
    let owner = SymbolOwner::NamedProfileLocal {
        project: "cloud".to_owned(),
        name: "x".repeat(SYMBOL_ID_BYTES_MAX - fixed),
    };
    let scope = owner.local_scope().expect("exact bound accepted");
    assert_eq!(scope.len(), SYMBOL_ID_BYTES_MAX);
    assert_eq!(
        parse_local_scope(&scope).expect("exact bound parses"),
        owner
    );
    assert_eq!(
        parse_local_scope(&format!("{scope}x"))
            .map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
    let overbound = SymbolOwner::NamedProfileLocal {
        project: "cloud".to_owned(),
        name: "x".repeat(SYMBOL_ID_BYTES_MAX - fixed + 1),
    };
    assert_eq!(
        overbound
            .local_scope()
            .map_err(|error| super::tests::error_violation(&error)),
        Err("Length".to_owned())
    );
    let fixed = format!("{SYMBOL_URI_PREFIX}local@cloud:/rust/beacon").len();
    let language = Language::from_identity_segment("rust").expect("language");
    let identity = SymbolIdentity::new(
        SymbolOwner::NamedProfileLocal {
            project: "cloud".to_owned(),
            name: "x".repeat(SYMBOL_ID_BYTES_MAX - fixed),
        },
        language.clone(),
        vec!["beacon".to_owned()],
    )
    .expect("exact bound");
    assert_eq!(identity.wire_identity().len(), SYMBOL_ID_BYTES_MAX);
    assert!(
        SymbolIdentity::new(
            SymbolOwner::NamedProfileLocal {
                project: "cloud".to_owned(),
                name: "x".repeat(SYMBOL_ID_BYTES_MAX - fixed + 1),
            },
            language,
            vec!["beacon".to_owned()],
        )
        .is_err(),
        "owner separator counts toward the symbol bound"
    );
}
