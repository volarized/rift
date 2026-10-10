//! Local scopes reuse canonical owner parsing and registration validation.

use super::*;

#[test]
fn local_scope_matches_canonical_symbol_owner() {
    for scope in ["local", "local@cloud", "local@tools_2", "local@cloud-tools"] {
        let owner = parse_local_scope(scope).expect("accepted local scope");
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
        parse_local_scope(&overbound),
        Err(SymbolIdentityViolation::Length)
    );
}
