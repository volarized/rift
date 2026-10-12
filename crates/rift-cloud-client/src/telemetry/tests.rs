use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn policy(root: &Path, home: Option<&Path>, environment_disabled: bool) -> Policy {
    if let Some(home) = home {
        let mut policy = Policy::for_test(root, home);
        policy.environment_disabled = environment_disabled;
        return policy;
    }
    Policy {
        root: root.to_path_buf(),
        home: home.map(Path::to_path_buf),
        environment_disabled,
    }
}

#[tokio::test]
async fn test_disable_wins_across_all_sources_and_changes_apply_to_future_requests() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    std::fs::create_dir(home.path().join(".rift"))?;
    let project = root.path().join(WORKSPACE_CONFIGURATION_FILE);
    let user = home.path().join(USER_CONFIGURATION_FILE);
    for project_disabled in [false, true] {
        std::fs::write(
            &project,
            format!("[telemetry]\ndisabled = {project_disabled}\n"),
        )?;
        for user_disabled in [false, true] {
            std::fs::write(&user, format!("[telemetry]\ndisabled = {user_disabled}\n"))?;
            for environment_disabled in [false, true] {
                let observed = policy(root.path(), Some(home.path()), environment_disabled)
                    .disabled()
                    .await;
                assert_eq!(
                    observed,
                    project_disabled || user_disabled || environment_disabled
                );
            }
        }
    }
    std::fs::remove_file(&project)?;
    std::fs::remove_file(&user)?;
    assert!(
        !policy(root.path(), Some(home.path()), false)
            .disabled()
            .await
    );
    assert!(policy(root.path(), None, false).disabled().await);
    assert_eq!(std::fs::read_dir(home.path().join(".rift"))?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn test_invalid_or_unreadable_settings_suppress_counters() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let policy = policy(root.path(), Some(home.path()), false);
    let project = root.path().join(WORKSPACE_CONFIGURATION_FILE);
    for raw in [
        "[broken",
        "[telemetry]\ndisabled = 'false'",
        "[telemetry]\nunknown = true",
    ] {
        std::fs::write(&project, raw)?;
        assert!(policy.disabled().await);
    }
    std::fs::write(&project, "x".repeat(256 * 1024 + 1))?;
    assert!(policy.disabled().await);
    std::fs::remove_file(&project)?;
    std::fs::create_dir_all(home.path().join(USER_CONFIGURATION_FILE))?;
    assert!(policy.disabled().await);
    assert_eq!(std::fs::read_dir(home.path().join(".rift"))?.count(), 1);
    Ok(())
}

#[test]
fn test_other_configuration_tables_do_not_change_telemetry_default() -> TestResult {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    std::fs::write(
        root.path().join(WORKSPACE_CONFIGURATION_FILE),
        "[source]\nrespect_gitignore = false\n",
    )?;
    assert!(!policy(root.path(), Some(home.path()), false).read_disabled()?);
    Ok(())
}
