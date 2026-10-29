use rift_error::{RiftError, errors};
use rift_protocol::identity::{PROFILE_SEPARATOR, SymbolIdentity, SymbolOwner};
use rift_protocol::read::{ReadWarning, SearchHit, SearchHitTarget, Symbol, SymbolId, TypeBinding};
use rift_protocol::symbol_read::GetSymbolResult;

pub(super) fn public_owner(owner: &SymbolOwner, backend: &str) -> Result<SymbolOwner, RiftError> {
    let owner = match owner {
        SymbolOwner::Local => backend_owner(backend).unwrap_or(SymbolOwner::Local),
        _ => owner.clone(),
    };
    owner.validate()?;
    Ok(owner)
}

/// The owner a backend key names: `cloud`, `:docs`, or `cloud:docs`.
fn backend_owner(backend: &str) -> Option<SymbolOwner> {
    if ["local", "global"].contains(&backend) {
        return None;
    }
    Some(match backend.split_once(PROFILE_SEPARATOR) {
        Some(("", name)) => SymbolOwner::ProfileLocal {
            name: name.to_owned(),
        },
        Some((project, name)) => SymbolOwner::NamedProfileLocal {
            project: project.to_owned(),
            name: name.to_owned(),
        },
        None => SymbolOwner::NamedLocal {
            name: backend.to_owned(),
        },
    })
}

pub(super) fn backend_key(owner: &SymbolOwner) -> Option<String> {
    match owner {
        SymbolOwner::NamedLocal { name } => Some(name.clone()),
        SymbolOwner::ProfileLocal { name } => Some(profile_key(name)),
        SymbolOwner::NamedProfileLocal { project, name } => Some(extra_profile_key(project, name)),
        _ => None,
    }
}

/// The backend key a project scope selects, absent for `local`, `global`, and `all`.
pub(super) fn scope_key(scope: &rift_protocol::read::SearchScope) -> Option<String> {
    scope.project_owner().as_ref().and_then(backend_key)
}

/// The backend key of a registered profile defined by the extra project `project`.
pub(super) fn extra_profile_key(project: &str, name: &str) -> String {
    format!("{project}{PROFILE_SEPARATOR}{name}")
}

/// The backend key of a profile defined by the primary project's configuration.
pub(super) fn profile_key(name: &str) -> String {
    format!("{PROFILE_SEPARATOR}{name}")
}

fn with_owner(identity: &SymbolIdentity, owner: SymbolOwner) -> Result<SymbolId, RiftError> {
    let translated = SymbolIdentity::new(
        owner,
        identity.language().clone(),
        identity.qualified_path().to_vec(),
    )?;
    Ok(SymbolId::from_identity(&translated))
}

pub(super) fn dispatch_id(id: &SymbolId, backend: &str) -> Result<SymbolId, RiftError> {
    let identity = SymbolIdentity::parse(id.as_str())?;
    match backend_key(identity.owner()) {
        Some(key) if key == backend => with_owner(&identity, SymbolOwner::Local),
        Some(_) => errors::server::read_invalid()
            .field("id")
            .violation("symbol owner does not match the accepted project")
            .fail(),
        None => Ok(id.clone()),
    }
}

fn qualify_id(id: &mut SymbolId, backend: &str) -> Result<(), RiftError> {
    let identity = SymbolIdentity::parse(id.as_str())?;
    if matches!(identity.owner(), SymbolOwner::Local) {
        *id = with_owner(&identity, public_owner(identity.owner(), backend)?)?;
    }
    Ok(())
}

fn qualify_optional(id: &mut Option<SymbolId>, backend: &str) -> Result<(), RiftError> {
    if let Some(id) = id {
        qualify_id(id, backend)?;
    }
    Ok(())
}

fn qualify_types(types: &mut [TypeBinding], backend: &str) -> Result<(), RiftError> {
    for binding in types {
        qualify_optional(&mut binding.r#type.resolved, backend)?;
    }
    Ok(())
}

fn qualify_symbol(symbol: &mut Symbol, backend: &str) -> Result<(), RiftError> {
    qualify_optional(&mut symbol.id, backend)?;
    qualify_optional(&mut symbol.container, backend)?;
    qualify_types(&mut symbol.types, backend)?;
    for signature in &mut symbol.signatures {
        for link in &mut signature.links {
            qualify_id(&mut link.symbol, backend)?;
        }
        for id in &mut signature.type_parameters {
            qualify_id(id, backend)?;
        }
        for expression in &mut signature.throws {
            qualify_optional(&mut expression.resolved, backend)?;
        }
        qualify_types(&mut signature.returns, backend)?;
        if let Some(receiver) = &mut signature.receiver {
            qualify_types(&mut receiver.types, backend)?;
        }
        for parameter in &mut signature.parameters {
            qualify_types(&mut parameter.types, backend)?;
        }
    }
    Ok(())
}

pub(super) fn qualify_warnings(
    warnings: &mut [ReadWarning],
    backend: &str,
) -> Result<(), RiftError> {
    for warning in warnings {
        match warning {
            ReadWarning::SymbolDisagreement { symbol, .. } => qualify_id(symbol, backend)?,
            ReadWarning::SymbolNotFound { alternatives, .. } => {
                for id in alternatives {
                    qualify_id(id, backend)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn qualify_hit(hit: &mut SearchHit, backend: &str) -> Result<(), RiftError> {
    match &mut hit.hit {
        SearchHitTarget::Symbol { symbol } => qualify_symbol(symbol, backend)?,
        SearchHitTarget::Documentation { documentation } => {
            qualify_optional(&mut documentation.block.symbol, backend)?;
        }
        _ => {}
    }
    if let Some(path) = &mut hit.traversal_path {
        for hop in path {
            qualify_id(&mut hop.relationship.from, backend)?;
            qualify_id(&mut hop.relationship.to, backend)?;
        }
    }
    Ok(())
}

pub(super) fn qualify_nodes(
    result: &mut rift_protocol::read::NodesResult,
    backend: &str,
) -> Result<(), RiftError> {
    for node in &mut result.nodes {
        qualify_optional(&mut node.of, backend)?;
    }
    qualify_warnings(&mut result.warnings, backend)
}

pub(super) fn qualify_search(
    result: &mut rift_protocol::search_read::SearchReadResult,
    backend: &str,
) -> Result<(), RiftError> {
    use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
    match result {
        SearchReadResult::Results(result) => {
            for hit in &mut result.results {
                qualify_hit(hit, backend)?;
            }
            qualify_warnings(&mut result.warnings, backend)
        }
        SearchReadResult::Unavailable(SearchUnavailable::Unavailable { warnings, .. }) => {
            qualify_warnings(warnings, backend)
        }
    }
}

pub(super) fn qualify_symbol_result(
    result: &mut GetSymbolResult,
    backend: &str,
) -> Result<(), RiftError> {
    match result {
        GetSymbolResult::Found {
            symbol,
            history,
            documentation,
            warnings,
            ..
        } => {
            qualify_symbol(symbol, backend)?;
            if let Some(history) = history {
                qualify_id(&mut history.symbol, backend)?;
            }
            if let Some(documentation) = documentation {
                for reference in &mut documentation.references {
                    qualify_id(&mut reference.reference.target, backend)?;
                    qualify_optional(&mut reference.documentation.block.symbol, backend)?;
                }
            }
            qualify_warnings(warnings, backend)?;
        }
        GetSymbolResult::Missing {
            symbol_not_found,
            warnings,
            ..
        } => {
            qualify_id(&mut symbol_not_found.id, backend)?;
            for alternative in &mut symbol_not_found.alternatives {
                qualify_id(&mut alternative.id, backend)?;
            }
            qualify_warnings(warnings, backend)?;
        }
        GetSymbolResult::Unavailable { id, warnings, .. } => {
            qualify_id(id, backend)?;
            qualify_warnings(warnings, backend)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        SymbolOwner, backend_key, backend_owner, dispatch_id, qualify_hit, qualify_id,
        qualify_symbol_result, scope_key,
    };
    use rift_protocol::read::{SearchHit, SymbolId};
    use rift_protocol::symbol_read::GetSymbolResult;

    #[test]
    fn named_identity_roundtrip_preserves_escaping() -> Result<(), Box<dyn std::error::Error>> {
        let original = SymbolId::parse("rift://symbol/local/rust/app/name%2Fpart%7E2")?;
        let mut qualified = original.clone();
        qualify_id(&mut qualified, "cloud")?;
        assert!(qualified.as_str().starts_with("rift://symbol/local@cloud/"));
        assert_eq!(dispatch_id(&qualified, "cloud")?, original);
        assert!(dispatch_id(&qualified, "other").is_err());
        Ok(())
    }

    #[test]
    fn profile_identity_roundtrip_keeps_extra_with_same_name_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = SymbolId::parse("rift://symbol/local/rust/app/name%2Fpart%7E2")?;
        let mut profile = original.clone();
        let mut extra = original.clone();
        qualify_id(&mut profile, ":docs")?;
        qualify_id(&mut extra, "docs")?;
        assert!(profile.as_str().starts_with("rift://symbol/local:docs/"));
        assert!(extra.as_str().starts_with("rift://symbol/local@docs/"));
        assert_ne!(profile, extra);
        assert_eq!(dispatch_id(&profile, ":docs")?, original);
        assert_eq!(dispatch_id(&extra, "docs")?, original);
        assert!(dispatch_id(&profile, "docs").is_err());
        assert!(dispatch_id(&extra, ":docs").is_err());
        assert!(dispatch_id(&profile, ":other").is_err());
        Ok(())
    }

    #[test]
    fn extra_profile_identity_roundtrip_keeps_extra_and_root_profile_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = SymbolId::parse("rift://symbol/local/rust/app/beacon")?;
        let mut extra_profile = original.clone();
        qualify_id(&mut extra_profile, "cloud:docs")?;
        assert_eq!(
            extra_profile.as_str(),
            "rift://symbol/local@cloud:docs/rust/app/beacon"
        );
        assert_eq!(dispatch_id(&extra_profile, "cloud:docs")?, original);
        for backend in ["cloud", ":docs", "cloud:api", "other:docs"] {
            assert!(dispatch_id(&extra_profile, backend).is_err(), "{backend}");
        }
        let owner = SymbolOwner::NamedProfileLocal {
            project: "cloud".to_owned(),
            name: "docs".to_owned(),
        };
        assert_eq!(backend_key(&owner).as_deref(), Some("cloud:docs"));
        assert_eq!(backend_owner("cloud:docs"), Some(owner));
        assert_eq!(backend_owner("global"), None);
        assert_eq!(
            scope_key(&rift_protocol::read::SearchScope::NamedProfileLocal {
                project: "cloud".to_owned(),
                name: "docs".to_owned(),
            })
            .as_deref(),
            Some("cloud:docs")
        );
        assert_eq!(scope_key(&rift_protocol::read::SearchScope::All), None);
        Ok(())
    }

    #[test]
    fn named_identity_keeps_package_runtime_and_raw_facts_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        for value in [
            "rift://symbol/cargo/crates.io/serde@1.0.0/rust/serde/Serialize",
            "rift://symbol/stdlib/rust@1.90.0/rust/core/Option",
        ] {
            let mut id = SymbolId::parse(value)?;
            let original = id.clone();
            qualify_id(&mut id, "cloud")?;
            assert_eq!(id, original);
            assert_eq!(dispatch_id(&id, "cloud")?, original);
        }
        let mut hit: SearchHit = serde_json::from_value(serde_json::json!({
            "hit":{"target":"symbol","symbol":{"language":"rust","name":"raw","kind":"function"}},
            "path":"lib.rs","range":{"start":0,"end":12},"line":1,"source":"fn raw() {}"
        }))?;
        let original = hit.clone();
        qualify_hit(&mut hit, "cloud")?;
        assert_eq!(hit, original);
        Ok(())
    }

    #[test]
    fn named_projects_keep_same_local_object_distinct_without_changing_physical_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let original: SearchHit = serde_json::from_value(serde_json::json!({
            "hit":{"target":"symbol","symbol":{"id":"rift://symbol/local/rust/app/load", "language":"rust","name":"load","kind":"function"}},
            "path":"lib.rs","range":{"start":0,"end":12},"line":1,"source":"fn load() {}"
        }))?;
        let mut first = original.clone();
        let mut second = original.clone();
        qualify_hit(&mut first, "cloud")?;
        qualify_hit(&mut second, "other")?;
        let (
            rift_protocol::read::SearchHitTarget::Symbol {
                symbol: first_symbol,
            },
            rift_protocol::read::SearchHitTarget::Symbol {
                symbol: second_symbol,
            },
        ) = (&first.hit, &second.hit)
        else {
            return Err("symbol hits required".into());
        };
        assert_ne!(first_symbol.id, second_symbol.id);
        assert_eq!(
            dispatch_id(first_symbol.id.as_ref().ok_or("first ID")?, "cloud")?,
            SymbolId::parse("rift://symbol/local/rust/app/load")?
        );
        assert_eq!(first.path, original.path);
        assert_eq!(second.path, original.path);
        assert_eq!(first.range, original.range);
        assert_eq!(second.range, original.range);
        assert_eq!(first.source, original.source);
        assert_eq!(second.source, original.source);
        Ok(())
    }

    #[test]
    fn named_symbol_links_change_only_typed_identities() -> Result<(), Box<dyn std::error::Error>> {
        let mut result: GetSymbolResult = serde_json::from_value(serde_json::json!({
            "outcome":"found", "symbol":{"id":"rift://symbol/local/rust/app/load", "language":"rust", "name":"load", "kind":"function", "container":"rift://symbol/local/rust/app", "documentation":[{"format":"markdown","text":"rift://symbol/local/rust/app/load"}], "signatures":[{"display":"load()", "language":"rust", "links":[{"range":{"start":0,"end":4},"symbol":"rift://symbol/local/rust/app/load"}]}]},
            "view":{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","expires_at":"2026-10-11T04:00:00Z"}, "declarations":[{"origin":{"location":"project","source_kind":"authored"},"path":"lib.rs","range":{"start":0,"end":12},"line":1,"signature_indices":[],"type_indices":[],"documentation_indices":[]}]
        }))?;
        let original = result.clone();
        qualify_symbol_result(&mut result, "cloud")?;
        let (
            GetSymbolResult::Found {
                symbol,
                view,
                declarations,
                ..
            },
            GetSymbolResult::Found {
                symbol: old_symbol,
                view: old_view,
                declarations: old_declarations,
                ..
            },
        ) = (result, original)
        else {
            return Err("found result required".into());
        };
        assert_eq!(
            symbol.id.as_ref().map(SymbolId::as_str),
            Some("rift://symbol/local@cloud/rust/app/load")
        );
        assert_eq!(
            symbol.signatures[0].links[0].symbol.as_str(),
            "rift://symbol/local@cloud/rust/app/load"
        );
        assert_eq!(symbol.documentation, old_symbol.documentation);
        assert_eq!(
            symbol.signatures[0].display,
            old_symbol.signatures[0].display
        );
        assert_eq!(view, old_view);
        assert_eq!(declarations, old_declarations);
        Ok(())
    }
}
