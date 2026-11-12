use rift_error::errors;
use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::{CommitAuthor, ProjectPath};

use super::*;

fn commit(path: &str) -> HistoryCommit {
    HistoryCommit {
        revision: Revision::commit(&"1f2080e4".repeat(5)).expect("a commit id"),
        path: ProjectPath(path.to_owned()),
        timestamp: "2026-08-21T14:03:22+00:00".to_owned(),
        summary: None,
        author: CommitAuthor {
            name: "Alice".to_owned(),
            email: "alice@example.com".to_owned(),
        },
    }
}

fn symbol(name: &str) -> SymbolId {
    SymbolId(format!("rift://symbol/local/rust/app/{name}"))
}

fn key(path: &str, qualified_name: &str) -> DeclarationKey {
    DeclarationKey {
        path: path.to_owned(),
        qualified_name: qualified_name.to_owned(),
    }
}

/// Binds `<name>.rs:<name>` to the symbol `<name>`; every other key binds nothing.
fn by_file(key: &DeclarationKey) -> Option<SymbolId> {
    let bound = key.path.strip_suffix(".rs") == Some(key.qualified_name.as_str());
    bound.then(|| symbol(&key.qualified_name))
}

/// `count` keys `n.rs:n` for `n` in `prefix0..`.
fn keys(prefix: &str, count: usize) -> Vec<DeclarationKey> {
    (0..count)
        .map(|index| {
            let name = format!("{prefix}{index}");
            key(&format!("{name}.rs"), &name)
        })
        .collect()
}

#[test]
fn every_stored_change_answers_its_commit_edge() {
    let from = Some(ProjectPath("old.rs".to_owned()));
    let cases = [
        (SymbolVersionKind::Introduced, None),
        (SymbolVersionKind::Removed, None),
        (SymbolVersionKind::SignatureChanged, None),
        (SymbolVersionKind::BodyChanged, None),
        (SymbolVersionKind::DecoratorsChanged, None),
        (SymbolVersionKind::Moved, from.clone()),
    ];
    let edges: Vec<HistoryEdge> = cases
        .into_iter()
        .filter_map(|(change, from)| commit_edge(change, commit("a.rs"), from))
        .collect();
    assert_eq!(
        edges,
        [
            HistoryEdge::IntroducedIn {
                commit: commit("a.rs")
            },
            HistoryEdge::RemovedIn {
                commit: commit("a.rs")
            },
            HistoryEdge::ChangedIn {
                commit: commit("a.rs"),
                change: HistoryChange::SignatureChanged
            },
            HistoryEdge::ChangedIn {
                commit: commit("a.rs"),
                change: HistoryChange::BodyChanged
            },
            HistoryEdge::ChangedIn {
                commit: commit("a.rs"),
                change: HistoryChange::DecoratorsChanged
            },
            HistoryEdge::RenamedFrom {
                commit: commit("a.rs"),
                from: ProjectPath("old.rs".to_owned())
            },
        ]
    );
    assert!(
        edges
            .iter()
            .all(|edge| edge_commit(edge) == Some(&commit("a.rs")))
    );
}

#[test]
fn a_move_without_its_old_path_answers_no_edge() {
    assert_eq!(
        commit_edge(SymbolVersionKind::Moved, commit("a.rs"), None),
        None
    );
    let co_change = HistoryEdge::CoChanges {
        symbol: symbol("b"),
        commits: 1,
    };
    assert_eq!(edge_commit(&co_change), None);
}

#[test]
fn a_tally_counts_each_bound_partner_once_per_commit_and_skips_itself() -> Result<(), RiftError> {
    let mut tally = CoChangeTally::new(symbol("a"));
    let first = [
        key("a.rs", "a"),
        key("b.rs", "b"),
        key("b.rs", "b"),
        key("gone.rs", "c"),
    ];
    tally.count_commit(&first, &mut |key: &DeclarationKey| Ok(by_file(key)))?;
    tally.count_commit(
        &[key("b.rs", "b"), key("c.rs", "c")],
        &mut |key: &DeclarationKey| Ok(by_file(key)),
    )?;
    let (edges, truncation) = tally.into_edges();
    assert_eq!(
        edges,
        [
            HistoryEdge::CoChanges {
                symbol: symbol("b"),
                commits: 2
            },
            HistoryEdge::CoChanges {
                symbol: symbol("c"),
                commits: 1
            },
        ],
        "most shared commits first; the symbol itself and unbound keys count toward nothing"
    );
    assert_eq!(truncation, None);
    Ok(())
}

#[test]
fn a_commit_at_the_declaration_bound_counts_and_one_past_it_does_not() -> Result<(), RiftError> {
    let mut tally = CoChangeTally::new(symbol("a"));
    tally.count_commit(
        &keys("at", CO_CHANGE_COMMIT_DECLARATIONS_MAX),
        &mut |key: &DeclarationKey| Ok(by_file(key)),
    )?;
    let mut resolved = 0_usize;
    let mut counting = |key: &DeclarationKey| {
        resolved += 1;
        Ok(by_file(key))
    };
    tally.count_commit(
        &keys("past", CO_CHANGE_COMMIT_DECLARATIONS_MAX + 1),
        &mut counting,
    )?;
    assert_eq!(resolved, 0, "a commit past the bound resolves no key");
    let (edges, truncation) = tally.into_edges();
    assert_eq!(edges.len(), CO_CHANGE_SYMBOLS_MAX);
    assert_eq!(
        truncation,
        Some(CoChangeTruncation {
            commits_past_bound: 1,
            symbols_past_bound: u64::try_from(
                CO_CHANGE_COMMIT_DECLARATIONS_MAX - CO_CHANGE_SYMBOLS_MAX
            )
            .unwrap_or(u64::MAX),
        })
    );
    Ok(())
}

#[test]
fn a_tally_at_the_symbol_bound_answers_every_partner_in_symbol_order() -> Result<(), RiftError> {
    let mut tally = CoChangeTally::new(symbol("a"));
    tally.count_commit(
        &keys("p", CO_CHANGE_SYMBOLS_MAX),
        &mut |key: &DeclarationKey| Ok(by_file(key)),
    )?;
    let (edges, truncation) = tally.into_edges();
    assert_eq!(edges.len(), CO_CHANGE_SYMBOLS_MAX);
    assert_eq!(truncation, None);
    let symbols: Vec<&str> = edges
        .iter()
        .filter_map(|edge| match edge {
            HistoryEdge::CoChanges { symbol, .. } => Some(symbol.as_str()),
            _ => None,
        })
        .collect();
    let mut sorted = symbols.clone();
    sorted.sort_unstable();
    assert_eq!(symbols, sorted, "equal counts order by symbol");
    Ok(())
}

#[test]
fn a_binding_failure_ends_the_count() {
    let mut tally = CoChangeTally::new(symbol("a"));
    let mut failing = |_: &DeclarationKey| -> Result<Option<SymbolId>, RiftError> {
        Err(errors::history_store::database()
            .operation("bind")
            .detail("refused")
            .error())
    };
    let error = tally
        .count_commit(&[key("b.rs", "b")], &mut failing)
        .expect_err("the binding failure surfaces");
    assert_eq!(error.slug(), errors::history_store::database::SLUG);
}

fn entry(name: &str, registry: Option<&str>, version: Option<&str>) -> PackageContextEntry {
    let selector = match version {
        Some(version) => PackageSelector::Version(version.to_owned()),
        None => PackageSelector::Requirement("^1".to_owned()),
    };
    let mut entry =
        PackageContextEntry::new("cargo", name, selector, PackageAvailability::Canonical);
    entry.registry = registry.map(str::to_owned);
    entry
}

#[test]
fn the_served_tree_uses_each_pinned_release_once_in_release_order() -> Result<(), RiftError> {
    let workspace = Revision::tree(&"4e".repeat(32))?;
    let packages = [
        entry("serde", Some("crates.io"), Some("1.0.228")),
        entry("anyhow", Some("crates.io"), Some("1.0.99")),
        entry("serde", Some("crates.io"), Some("1.0.228")),
        entry("local", None, Some("0.1.0")),
        entry("ranged", Some("crates.io"), None),
        entry("odd", Some("crates.io"), Some("01.0")),
    ];
    let uses = revision_uses(&workspace, &packages);
    let releases: Vec<String> = uses.iter().map(|used| used.release.to_string()).collect();
    assert_eq!(
        releases,
        [
            "rift://revision/cargo/crates.io/anyhow@1.0.99",
            "rift://revision/cargo/crates.io/serde@1.0.228",
        ]
    );
    assert!(uses.iter().all(|used| used.workspace == workspace));
    Ok(())
}

#[test]
fn an_owner_whose_release_does_not_spell_back_answers_no_edge() -> Result<(), RiftError> {
    let workspace = Revision::tree(&"4e".repeat(32))?;
    let packages = [entry("bad/name", Some("crates.io"), Some("1.0.0"))];
    assert!(revision_uses(&workspace, &packages).is_empty());
    Ok(())
}
