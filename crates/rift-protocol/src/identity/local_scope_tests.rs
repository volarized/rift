//! Local scopes reuse canonical owner parsing and registration validation.

use super::*;

#[test]
fn local_scope_matches_canonical_symbol_owner() {
    for scope in ["local", "local@cloud", "local@tools_2", "local@cloud-tools"] {
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
        if let SymbolOwner::NamedLocal { name } = owner {
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
