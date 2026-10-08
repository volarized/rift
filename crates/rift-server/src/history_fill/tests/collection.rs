use super::*;
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_protocol::configuration::{
    ConfigurationViolation, HISTORY_MOVE_DELETIONS_MAX, HISTORY_RELEASE_TAGS_MAX,
    HISTORY_TREE_ENTRIES_MAX, WorkspaceConfiguration,
};

fn configured_history(
    document: &str,
    environment: &ConfigurationEnvironment,
) -> TestResult<HistoryConfiguration> {
    let accepted = accept_configuration::<WorkspaceConfiguration>(Some(document), environment)?;
    accepted
        .configuration()
        .validate()
        .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
    Ok(accepted.configuration().providers.history.clone())
}

fn two_deleted_files() -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(
        root,
        "from.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn stays() {}\n",
    )?;
    write(root, "drop.rs", "pub fn dropped() {}\n")?;
    commit_all(root, "introduce travelled");
    fs::remove_file(root.join("from.rs"))?;
    fs::remove_file(root.join("drop.rs"))?;
    write(
        root,
        "to.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn arrived() {}\n",
    )?;
    commit_all(root, "move travelled");
    Ok(directory)
}

#[test]
fn test_history_configuration_and_environment_apply_collection_bounds() -> TestResult {
    let directory = two_deleted_files()?;
    let root = directory.path();
    let document = "[providers.history]\ntree_entries = 2\nrelease_tags = 2\nmove_deletions = 1\n";
    let configured = configured_history(document, &ConfigurationEnvironment::default())?;
    let record = head_record(&analysis(root, &configured)?)?;
    assert!(record.boundary);
    assert!(record.paths.is_empty());

    let environment = ConfigurationEnvironment::from_variables([
        ("RIFT_PROVIDERS_HISTORY_TREE_ENTRIES", "3"),
        ("RIFT_PROVIDERS_HISTORY_RELEASE_TAGS", "3"),
        ("RIFT_PROVIDERS_HISTORY_MOVE_DELETIONS", "2"),
    ]);
    let raised = configured_history(document, &environment)?;
    assert_eq!(raised.tree_entries, 3);
    assert_eq!(raised.release_tags, 3);
    assert_eq!(raised.move_deletions, 2);
    let record = head_record(&analysis(root, &raised)?)?;
    assert!(!record.boundary);
    assert_eq!(record.paths.len(), 3);
    assert_eq!(record.moves.len(), 1);
    assert!(
        record
            .declarations
            .iter()
            .any(|change| change.qualified_name == "travelled"
                && change.change == SymbolVersionKind::Moved)
    );

    let tight_moves = HistoryConfiguration {
        tree_entries: 3,
        ..configured.clone()
    };
    let record = head_record(&analysis(root, &tight_moves)?)?;
    assert!(!record.boundary);
    assert!(record.moves.is_empty());
    assert!(
        record
            .declarations
            .iter()
            .any(|change| change.qualified_name == "travelled"
                && change.change == SymbolVersionKind::Introduced)
    );

    git(root, &["tag", "v0.0.1", "HEAD~1"]);
    git(root, &["tag", "v0.0.2", "HEAD"]);
    git(root, &["tag", "v0.0.3", "HEAD^{tree}"]);
    let selected = |mut history: HistoryConfiguration| {
        history.strategy = HistoryStrategy::Selective;
        history.releases = vec!["v*".to_owned()];
        history
    };
    let refused = analysis(root, &selected(configured))?
        .plan(&HashMap::new())
        .expect_err("three tags exceed configured two");
    assert_eq!(
        refused.slug(),
        rift_error::errors::history::too_many_tags::SLUG
    );
    assert!(
        refused
            .context()
            .any(|(key, value)| key == "tags_max" && value == "2")
    );
    let plan = analysis(root, &selected(raised))?.plan(&HashMap::new())?;
    assert_eq!(
        plan.keep().len(),
        2,
        "the tag naming a tree counts against the bound but names no commit"
    );
    Ok(())
}

#[test]
fn test_direct_revision_read_uses_history_tree_entry_bound() -> TestResult {
    let directory = two_deleted_files()?;
    let read = |tree_entries| {
        crate::ReadService::at_revision(
            directory.path(),
            &rift_protocol::read::RevisionId("HEAD~1".to_owned()),
            rift_index::WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            HistoryConfiguration {
                tree_entries,
                ..HistoryConfiguration::default()
            },
        )
    };
    let refused = read(1).expect_err("two committed entries exceed one");
    assert_eq!(
        refused.slug(),
        rift_error::errors::history::tree_too_large::SLUG
    );
    assert!(
        refused
            .context()
            .any(|(key, value)| key == "entries_max" && value == "1")
    );
    assert_eq!(read(2)?.index().files().count(), 2);
    Ok(())
}

#[test]
fn test_history_collection_builders_accept_edges_and_refuse_outside_bounds() -> TestResult {
    let directory = two_deleted_files()?;
    for (field, maximum, position) in [
        (
            "providers.history.tree_entries",
            usize::try_from(HISTORY_TREE_ENTRIES_MAX)?,
            0,
        ),
        (
            "providers.history.release_tags",
            usize::try_from(HISTORY_RELEASE_TAGS_MAX)?,
            1,
        ),
        (
            "providers.history.move_deletions",
            usize::try_from(HISTORY_MOVE_DELETIONS_MAX)?,
            2,
        ),
    ] {
        for value in [1, maximum] {
            let mut bounds = [1; 3];
            bounds[position] = value;
            analysis(directory.path(), &HistoryConfiguration::default())?
                .with_collection_bounds(bounds[0], bounds[1], bounds[2])?;
        }
        for value in [0, maximum + 1] {
            let mut bounds = [1; 3];
            bounds[position] = value;
            let refused = analysis(directory.path(), &HistoryConfiguration::default())?
                .with_collection_bounds(bounds[0], bounds[1], bounds[2])
                .expect_err("unsupported bound");
            assert_eq!(
                refused.slug(),
                rift_error::errors::core::configuration_limit_out_of_range::SLUG
            );
            assert!(
                refused
                    .context()
                    .any(|(key, value)| key == "field" && value == field)
            );
        }
    }
    let maximum = usize::try_from(HISTORY_TREE_ENTRIES_MAX)?;
    for value in [1, maximum] {
        assert_eq!(
            rift_index::WorkspaceIndexLimits::default()
                .with_revision_tree_entries(value)?
                .revision_tree_entries_max(),
            value
        );
    }
    for value in [0, maximum + 1] {
        assert!(
            rift_index::WorkspaceIndexLimits::default()
                .with_revision_tree_entries(value)
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn test_history_collection_environment_refuses_outside_supported_bounds() -> TestResult {
    for (variable, value, field) in [
        (
            "RIFT_PROVIDERS_HISTORY_TREE_ENTRIES",
            "0",
            "providers.history.tree_entries",
        ),
        (
            "RIFT_PROVIDERS_HISTORY_RELEASE_TAGS",
            "5000001",
            "providers.history.release_tags",
        ),
        (
            "RIFT_PROVIDERS_HISTORY_MOVE_DELETIONS",
            "65537",
            "providers.history.move_deletions",
        ),
    ] {
        let environment = ConfigurationEnvironment::from_variables([(variable, value)]);
        let accepted = accept_configuration::<WorkspaceConfiguration>(None, &environment)?;
        assert!(
            matches!(accepted.configuration().validate(), Err(ConfigurationViolation::LimitOutOfRange { field: refused, .. }) if refused == field)
        );
    }
    Ok(())
}
