use std::collections::BTreeMap;
use std::path::Path;

use rift_protocol::configuration::McpConfiguration;

use super::{Registrations, merged};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn cli_overrides_saved_names_but_duplicate_cli_names_are_refused() {
    let saved = McpConfiguration {
        extra: BTreeMap::from([("cloud".to_owned(), "../saved".to_owned())]),
    };
    let accepted = merged(&saved, &["cloud=../cli=source".to_owned()]).expect("CLI override");
    assert_eq!(accepted.extra["cloud"], "../cli=source");
    assert_eq!(saved.extra["cloud"], "../saved");
    let refused = merged(
        &saved,
        &["cloud=../first".to_owned(), "cloud=../last".to_owned()],
    )
    .expect_err("duplicate CLI aliases must be refused before override");
    assert_eq!(refused.slug().as_str(), "rift.core.configuration_invalid");
    assert!(
        refused
            .to_string()
            .contains("CLI registrations repeat this name")
    );
}

#[test]
fn merged_budget_is_checked_before_any_root_resolution() {
    let saved = McpConfiguration::default();
    let entry = format!("cloud={}", "x".repeat(256 << 10));
    let refused = Registrations::accept(Path::new("missing-primary"), &saved, &[entry], None)
        .expect_err("aggregate input must be refused before filesystem access");
    assert_eq!(
        refused.slug().as_str(),
        "rift.core.configuration_limit_out_of_range"
    );
    assert!(!refused.to_string().contains("primary root"));
}

#[test]
fn relative_roots_use_primary_and_home_forms_use_supplied_home() -> TestResult {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let cloud = directory.path().join("cloud");
    let home = directory.path().join("home");
    let app = home.join("app=source");
    for path in [&primary, &cloud, &app] {
        std::fs::create_dir_all(path)?;
    }
    let saved = McpConfiguration {
        extra: BTreeMap::from([
            ("cloud".to_owned(), "../cloud".to_owned()),
            ("app".to_owned(), "~/app=source".to_owned()),
        ]),
    };
    let accepted = Registrations::accept(&primary, &saved, &[], Some(&home))?;
    assert_eq!(
        accepted.root("cloud"),
        Some(cloud.canonicalize()?.as_path())
    );
    assert_eq!(accepted.root("app"), Some(app.canonicalize()?.as_path()));
    assert_eq!(accepted.names().collect::<Vec<_>>(), ["app", "cloud"]);
    assert_eq!(accepted.root("absent"), None);
    let no_home = Registrations::accept(&primary, &saved, &[], None).expect_err("home required");
    assert!(
        no_home
            .to_string()
            .contains("home directory is unavailable")
    );
    Ok(())
}

#[test]
fn canonical_duplicates_and_primary_root_are_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let extra = directory.path().join("extra");
    std::fs::create_dir_all(&primary)?;
    std::fs::create_dir_all(&extra)?;
    let duplicate = McpConfiguration {
        extra: BTreeMap::from([
            ("app".to_owned(), "../extra".to_owned()),
            ("cloud".to_owned(), "../extra/.".to_owned()),
        ]),
    };
    assert!(
        Registrations::accept(&primary, &duplicate, &[], None)
            .expect_err("duplicate roots")
            .to_string()
            .contains("same canonical root")
    );
    let primary_again = McpConfiguration {
        extra: BTreeMap::from([("app".to_owned(), ".".to_owned())]),
    };
    assert!(
        Registrations::accept(&primary, &primary_again, &[], None)
            .expect_err("primary cannot be extra")
            .to_string()
            .contains("primary root")
    );
    Ok(())
}

#[test]
fn malformed_cli_paths_and_missing_or_file_roots_are_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    let saved = McpConfiguration::default();
    for entry in [
        "cloud",
        "Cloud=../cloud",
        "local=../cloud",
        "cloud=~someone/app",
        "cloud=",
    ] {
        assert!(merged(&saved, &[entry.to_owned()]).is_err(), "{entry}");
    }
    let file = directory.path().join("file");
    std::fs::write(&file, "source")?;
    for path in ["missing", "file"] {
        let entry = format!("app={path}");
        assert!(Registrations::accept(directory.path(), &saved, &[entry], None).is_err());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn accepted_symlink_root_is_fixed_across_later_changes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let link = directory.path().join("linked");
    for path in [&primary, &first, &second] {
        std::fs::create_dir_all(path)?;
    }
    std::os::unix::fs::symlink(&first, &link)?;
    let saved = McpConfiguration {
        extra: BTreeMap::from([("app".to_owned(), "../linked".to_owned())]),
    };
    let accepted = Registrations::accept(&primary, &saved, &[], None)?;
    std::fs::remove_file(&link)?;
    std::os::unix::fs::symlink(&second, &link)?;
    assert_eq!(accepted.root("app"), Some(first.canonicalize()?.as_path()));
    assert_ne!(accepted.root("app"), Some(second.canonicalize()?.as_path()));
    Ok(())
}

#[test]
fn empty_extra_projects_retain_accepted_primary_root() -> TestResult {
    let primary = tempfile::tempdir()?;
    let accepted = Registrations::accept(primary.path(), &McpConfiguration::default(), &[], None)?;
    assert_eq!(
        accepted.primary_root(),
        Some(primary.path().canonicalize()?.as_path())
    );
    assert_eq!(accepted.entries().count(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn primary_root_keeps_accepted_target_after_symlink_changes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let link = directory.path().join("primary");
    std::fs::create_dir(&first)?;
    std::fs::create_dir(&second)?;
    std::os::unix::fs::symlink(&first, &link)?;
    let accepted = Registrations::accept(&link, &McpConfiguration::default(), &[], None)?;
    let original = first.canonicalize()?;
    std::fs::remove_file(&link)?;
    std::os::unix::fs::symlink(&second, &link)?;
    assert_eq!(accepted.primary_root(), Some(original.as_path()));
    assert_ne!(
        accepted.primary_root(),
        Some(link.canonicalize()?.as_path())
    );
    Ok(())
}
