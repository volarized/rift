//! Local scopes reuse canonical owner parsing and registration validation.

use super::*;

#[test]
fn local_scope_matches_canonical_symbol_owner() {
    for scope in [
        "local",
        "local@cloud",
        "local@tools_2",
        "local@cloud-tools",
        "local#docs",
        "local#docs_2",
        "local#docs-api",
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
        if let SymbolOwner::NamedLocal { name } | SymbolOwner::ProfileLocal { name } = owner {
            assert!(valid_registration_name(&name));
        } else {
            assert_eq!(scope, "local");
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
    let profile = parse_local_scope("local#docs").expect("profile scope");
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
        "rift://symbol/local%23docs/rust/app/load"
    );
    assert_eq!(
        SymbolIdentity::parse(&identity.wire_identity()).expect("profile identity parses"),
        identity
    );
    for invalid in [
        "rift://symbol/local#docs/rust/app/load",
        "rift://symbol/local%23Docs/rust/app/load",
        "rift://symbol/local%23all/rust/app/load",
    ] {
        assert!(SymbolIdentity::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn profile_scope_refuses_reserved_names_and_accepts_exact_byte_bound() {
    for name in [
        "", "local", "global", "all", "Docs", "docs/api", "%64ocs", "docs ", "docs\n", "docs.api",
    ] {
        let scope = format!("local#{name}");
        assert!(parse_local_scope(&scope).is_err(), "{scope:?}");
        assert!(
            SymbolOwner::ProfileLocal {
                name: name.to_owned()
            }
            .local_scope()
            .is_err()
        );
    }
    let name = "x".repeat(SYMBOL_ID_BYTES_MAX - "local#".len());
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
fn profile_symbol_bound_counts_encoded_scope_separator() {
    let fixed = format!("{SYMBOL_URI_PREFIX}local%23/rust/beacon").len();
    let name = "x".repeat(SYMBOL_ID_BYTES_MAX - fixed);
    let language = Language::from_identity_segment("rust").expect("language");
    let identity = SymbolIdentity::new(
        SymbolOwner::ProfileLocal { name: name.clone() },
        language.clone(),
        vec!["beacon".to_owned()],
    )
    .expect("exact encoded bound");
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
        "encoded owner expansion counts toward the symbol bound"
    );
}
