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

#[test]
fn profile_and_extra_with_same_name_select_distinct_roots() -> TestResult {
    use rift_protocol::configuration::{ProfileConfiguration, WorkspaceConfiguration};
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let profile = primary.join("docs");
    let extra = directory.path().join("extra");
    std::fs::create_dir_all(&profile)?;
    std::fs::create_dir(&extra)?;
    let mut configuration = WorkspaceConfiguration::default();
    configuration
        .mcp
        .extra
        .insert("docs".to_owned(), "../extra".to_owned());
    configuration.profiles.insert(
        "docs".to_owned(),
        ProfileConfiguration {
            root: "docs".to_owned(),
            ..ProfileConfiguration::default()
        },
    );
    let accepted = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)?;
    assert_eq!(
        accepted.root(":docs"),
        Some(profile.canonicalize()?.as_path())
    );
    assert_eq!(accepted.root("docs"), Some(extra.canonicalize()?.as_path()));
    assert_eq!(accepted.entries().count(), 2);
    configuration
        .mcp
        .extra
        .insert("docs".to_owned(), "docs".to_owned());
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("one root cannot select both models");
    assert_eq!(refused.slug().as_str(), "rift.core.configuration_invalid");
    Ok(())
}

#[test]
fn extra_projects_register_their_own_profiles_without_following_their_extras() -> TestResult {
    use rift_protocol::configuration::WorkspaceConfiguration;
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let cloud = directory.path().join("cloud");
    let other = directory.path().join("other");
    for path in [
        &primary,
        &cloud.join(".git"),
        &cloud.join("docs"),
        &cloud.join("dev"),
        &other,
    ] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::write(
        cloud.join("rift.toml"),
        "[mcp.extra]\nother = '../other'\n[profiles.docs]\nroot = 'docs'\n[profiles.dev]\nroot = 'dev'\n",
    )?;
    let mut configuration = WorkspaceConfiguration::default();
    configuration
        .mcp
        .extra
        .insert("cloud".to_owned(), "../cloud".to_owned());
    let accepted = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)?;
    assert_eq!(
        accepted.names().collect::<Vec<_>>(),
        ["cloud", "cloud:dev", "cloud:docs"]
    );
    assert_eq!(
        accepted.root("cloud:docs"),
        Some(cloud.join("docs").canonicalize()?.as_path())
    );
    assert_eq!(accepted.root(":docs"), None);
    assert_eq!(accepted.root("other"), None);
    let refusal = accepted.refusal("cloud:api", "search", "scope");
    assert_eq!(refusal.slug().as_str(), "rift.mcp.parameter_invalid");
    assert!(
        refusal
            .to_string()
            .contains("local@cloud, local@cloud:dev, local@cloud:docs"),
        "{refusal}"
    );
    Ok(())
}

/// The refusal's evidence as named strings.
fn evidence(error: &rift_error::RiftError) -> BTreeMap<String, String> {
    error
        .context()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Asserts one `profiles` configuration refusal naming `name` with `detail`.
fn assert_profile_refusal(error: &rift_error::RiftError, name: &str, detail: &str) {
    assert_eq!(error.slug().as_str(), "rift.core.configuration_invalid");
    let evidence = evidence(error);
    assert_eq!(evidence.get("field").map(String::as_str), Some("profiles"));
    assert_eq!(evidence.get("name").map(String::as_str), Some(name));
    assert_eq!(evidence.get("detail").map(String::as_str), Some(detail));
}

type Fixture = (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf);

fn extra_fixture(profiles: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let cloud = directory.path().join("cloud");
    for path in [
        &primary,
        &cloud.join(".git"),
        &cloud.join("docs"),
        &directory.path().join("outside"),
    ] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::write(cloud.join("rift.toml"), profiles)?;
    Ok((directory, primary, cloud))
}

fn saved(entries: &[(&str, &str)]) -> rift_protocol::configuration::WorkspaceConfiguration {
    let mut configuration = rift_protocol::configuration::WorkspaceConfiguration::default();
    for (name, path) in entries {
        configuration
            .mcp
            .extra
            .insert((*name).to_owned(), (*path).to_owned());
    }
    configuration
}

#[test]
fn extra_profile_naming_a_registered_root_is_refused() -> TestResult {
    let (_directory, primary, _cloud) = extra_fixture("[profiles.docs]\nroot = 'docs'\n")?;
    let configuration = saved(&[("cloud", "../cloud"), ("docs", "../cloud/docs")]);
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("one root cannot be both an extra project and an extra profile");
    assert_profile_refusal(
        &refused,
        "docs",
        "profile and extra workspace name the same canonical root",
    );
    Ok(())
}

#[test]
fn extra_profile_naming_the_primary_root_is_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    std::fs::create_dir_all(&primary)?;
    std::fs::write(
        directory.path().join("rift.toml"),
        "[profiles.app]\nroot = 'primary'\n",
    )?;
    let configuration = saved(&[("parent", "..")]);
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("an extra profile cannot serve the primary root");
    assert_profile_refusal(&refused, "app", "profile names the primary project root");
    Ok(())
}

#[test]
fn refused_extra_configuration_keeps_the_extra_and_answers_its_profiles_with_the_refusal()
-> TestResult {
    for (document, detail) in [
        (
            "[profiles.docs]\nroot = '../outside'\n",
            Some("profile root must name a folder below this project using forward slashes"),
        ),
        ("[profiles.docs\nroot = 'docs'\n", None),
    ] {
        let (_directory, primary, cloud) = extra_fixture(document)?;
        let configuration = saved(&[("cloud", "../cloud")]);
        let accepted = Registrations::accept(&primary, &configuration.mcp, &[], None)?
            .with_profiles(&primary, &configuration)?;
        assert_eq!(accepted.names().collect::<Vec<_>>(), ["cloud"]);
        assert_eq!(
            accepted.root("cloud"),
            Some(cloud.canonicalize()?.as_path())
        );
        let expected = rift_server::load_configuration(&cloud)
            .expect_err("the fixture configuration is refused");
        for scope in ["cloud:docs", "cloud:api"] {
            let refusal = accepted.refusal(scope, "search", "scope");
            assert_eq!(refusal.slug(), expected.slug(), "{refusal}");
            let evidence = evidence(&refusal);
            assert_eq!(
                evidence.get("scope").map(String::as_str),
                Some(format!("local@{scope}").as_str())
            );
            if let Some(detail) = detail {
                assert_eq!(evidence.get("detail").map(String::as_str), Some(detail));
            }
        }
        let unknown = accepted.refusal("other", "search", "scope");
        assert_eq!(unknown.slug().as_str(), "rift.mcp.parameter_invalid");
        assert_eq!(
            evidence(&unknown).get("accepted").map(String::as_str),
            Some("local@cloud")
        );
    }
    Ok(())
}

#[test]
fn profile_key_already_registered_is_refused_instead_of_replaced() -> TestResult {
    let (directory, primary, cloud) = extra_fixture("[profiles.docs]\nroot = 'docs'\n")?;
    let registrations = Registrations {
        roots: BTreeMap::from([
            ("cloud".to_owned(), cloud.canonicalize()?),
            (
                "cloud:docs".to_owned(),
                directory.path().join("outside").canonicalize()?,
            ),
        ]),
        primary: Some(primary.canonicalize()?),
        refused: BTreeMap::new(),
    };
    let refused = registrations
        .with_profiles(&primary, &saved(&[]))
        .expect_err("a profile key cannot replace a registered project");
    assert_profile_refusal(
        &refused,
        "docs",
        "profile scope names an already registered project",
    );
    Ok(())
}

#[test]
fn primary_profile_overlapping_an_extra_project_or_its_profile_is_refused() -> TestResult {
    let (_directory, primary, _cloud) = extra_fixture("[profiles.docs]\nroot = 'docs'\n")?;
    std::fs::create_dir_all(primary.join("docs/api"))?;
    let mut configuration = saved(&[("api", "docs/api")]);
    configuration.profiles.insert(
        "docs".to_owned(),
        rift_protocol::configuration::ProfileConfiguration {
            root: "docs".to_owned(),
            ..rift_protocol::configuration::ProfileConfiguration::default()
        },
    );
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("a profile folder cannot hold an extra project");
    assert_profile_refusal(
        &refused,
        "docs",
        "profile overlaps an extra project; select separate project folders",
    );
    let vendor = primary.join("vendor");
    std::fs::create_dir_all(vendor.join("app"))?;
    std::fs::write(vendor.join("rift.toml"), "[profiles.app]\nroot = 'app'\n")?;
    let mut configuration = saved(&[("vendor", "vendor")]);
    configuration.profiles.insert(
        "docs".to_owned(),
        rift_protocol::configuration::ProfileConfiguration {
            root: "vendor/app/docs".to_owned(),
            ..rift_protocol::configuration::ProfileConfiguration::default()
        },
    );
    std::fs::create_dir_all(vendor.join("app/docs"))?;
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("a profile folder cannot lie inside an extra profile");
    assert_profile_refusal(
        &refused,
        "docs",
        "profile overlaps an extra project; select separate project folders",
    );
    Ok(())
}

#[test]
fn extra_profile_scope_past_the_identity_bound_is_refused() -> TestResult {
    let bound = rift_protocol::identity::SYMBOL_ID_BYTES_MAX;
    let project = "p".repeat(bound / 2);
    let profile = "d".repeat(bound - "local@:".len() - project.len());
    let (_directory, primary, _cloud) =
        extra_fixture(&format!("[profiles.{profile}]\nroot = 'docs'\n"))?;
    let configuration = saved(&[(project.as_str(), "../cloud")]);
    let accepted = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)?;
    assert_eq!(
        accepted.names().count(),
        2,
        "the exact scope bound is accepted"
    );
    let longer = format!("{profile}d");
    let (_directory, primary, _cloud) =
        extra_fixture(&format!("[profiles.{longer}]\nroot = 'docs'\n"))?;
    let refused = Registrations::accept(&primary, &configuration.mcp, &[], None)?
        .with_profiles(&primary, &configuration)
        .expect_err("a scope past the identity bound is unaddressable");
    assert_profile_refusal(
        &refused,
        &format!("{project}:{longer}"),
        "extra project profile scope exceeds the symbol identity byte bound",
    );
    Ok(())
}

#[test]
fn cli_registrations_the_primary_also_serves_are_refused_unless_saved() -> TestResult {
    let (_directory, primary, _cloud) = extra_fixture("")?;
    std::fs::create_dir_all(primary.join("vendor/app"))?;
    let nested = McpConfiguration {
        extra: BTreeMap::from([("app".to_owned(), "vendor/app".to_owned())]),
    };
    let below = "a CLI registration below the primary root is also served by the primary project; save it in mcp.extra";
    let replaced = "a CLI registration replaces a saved project below the primary root; save the change in mcp.extra";
    for (saved, cli, detail) in [
        (McpConfiguration::default(), "app=vendor/app", Some(below)),
        (nested.clone(), "app=vendor/app", None),
        (nested.clone(), "app=vendor/app/.", Some(below)),
        (nested.clone(), "app=../outside", Some(replaced)),
        (McpConfiguration::default(), "app=../outside", None),
    ] {
        let result = Registrations::accept(&primary, &saved, &[cli.to_owned()], None);
        match detail {
            None => assert!(result.is_ok(), "{cli}"),
            Some(detail) => {
                let error = result.expect_err("a CLI registration served twice is refused");
                let evidence = evidence(&error);
                assert_eq!(evidence.get("field").map(String::as_str), Some("mcp.extra"));
                assert_eq!(evidence.get("name").map(String::as_str), Some("app"));
                assert_eq!(
                    evidence.get("detail").map(String::as_str),
                    Some(detail),
                    "{cli}"
                );
            }
        }
    }
    Ok(())
}
