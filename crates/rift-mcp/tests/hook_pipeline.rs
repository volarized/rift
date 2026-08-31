//! Proves source transforms run before source-read-only validation hooks.

mod hermetic_search;

use std::error::Error;
use std::fmt::Write as _;
use std::fs;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;
const INITIAL_SOURCE: &str = "pub fn beacon() {}\n";
const DIRECT_SOURCE: &str = "pub fn beacon() -> u8 { 1 }\n";
const FORMATTED_SOURCE: &str = "pub fn beacon() -> u8 { 2 }\n";

#[derive(Clone, Copy)]
struct Hook<'a> {
    id: &'a str,
    kind: &'a str,
    script: &'a str,
    writes: &'a str,
    failure_severity: &'a str,
    timeout: &'a str,
    guarantee: bool,
    /// The hook's `include` list, spelled as the TOML array the file carries.
    include: &'a str,
    /// The hook's `exclude` list, spelled the same way.
    exclude: &'a str,
}

impl Hook<'_> {
    /// A validation hook that runs `script` and selects every change.
    const fn validation(id: &'static str, script: &'static str) -> Hook<'static> {
        Hook {
            id,
            kind: "lint",
            script,
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        }
    }
}

fn configuration(hooks: &[Hook<'_>]) -> String {
    let mut configuration = hermetic_search::SEMANTIC_DISABLED.to_owned();
    for hook in hooks {
        let guarantees = if hook.guarantee {
            r#"[{ kind = "behavior_checked", scope = { kind = "reach", reach = "project" }, detail = "validation read the final tree" }]"#
        } else {
            "[]"
        };
        write!(
            configuration,
            r#"
[[hooks]]
id = "{id}"
kind = "{kind}"
command = ["sh", "{script}"]
changed_paths = "none"
writes = "{writes}"
working_directory = ""
environment = {{}}
timeout = "{timeout}"
output_limit = "4kb"
failure_severity = "{failure_severity}"
guarantees = {guarantees}
determinism = "deterministic"
include = {include}
exclude = {exclude}
"#,
            id = hook.id,
            kind = hook.kind,
            script = hook.script,
            writes = hook.writes,
            timeout = hook.timeout,
            failure_severity = hook.failure_severity,
            include = hook.include,
            exclude = hook.exclude,
        )
        .expect("writing to String must succeed");
    }
    configuration
}

async fn served_workspace(
    hooks: &[Hook<'_>],
    scripts: &[(&str, &str)],
    files: &[(&str, &str)],
) -> TestResult<(
    tempfile::TempDir,
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), INITIAL_SOURCE)?;
    for (path, contents) in scripts.iter().chain(files) {
        fs::write(directory.path().join(path), contents)?;
    }
    fs::write(directory.path().join("rift.toml"), configuration(hooks))?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    Ok((directory, client, server_task))
}

fn arguments(value: &Value) -> TestResult<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

async fn call_retrying_acceptance(
    client: &RunningService<RoleClient, ()>,
    params: CallToolRequestParams,
) -> TestResult<rmcp::model::CallToolResult> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        match client.call_tool(params.clone()).await {
            Ok(result) => return Ok(result),
            Err(rmcp::ServiceError::McpError(error))
                if error
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("server kept refusing a retryable hook request".into())
}

async fn replace(client: &RunningService<RoleClient, ()>, body: &str) -> TestResult<Value> {
    let request = CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
        "symbol": "rift://symbol/rust/lib.rs/beacon",
        "body": body,
    }))?);
    call_retrying_acceptance(client, request)
        .await?
        .structured_content
        .ok_or_else(|| "replace_symbol must return structured content".into())
}

async fn patch(client: &RunningService<RoleClient, ()>, body: &str) -> TestResult<Value> {
    let request =
        CallToolRequestParams::new("patch").with_arguments(arguments(&json!({ "patch": body }))?);
    call_retrying_acceptance(client, request)
        .await?
        .structured_content
        .ok_or_else(|| "patch must return structured content".into())
}

async fn read_beacon_source(client: &RunningService<RoleClient, ()>) -> TestResult<String> {
    let request = CallToolRequestParams::new("get_symbol")
        .with_arguments(arguments(&json!({ "name": "beacon" }))?);
    let structured = call_retrying_acceptance(client, request)
        .await?
        .structured_content
        .ok_or("get_symbol must return structured content")?;
    structured["hits"][0]["source"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "get_symbol must carry declaration source".into())
}

async fn refused_read(client: &RunningService<RoleClient, ()>) -> TestResult<Value> {
    let error = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({ "name": "beacon" }))?),
        )
        .await
        .expect_err("invalid hook configuration must refuse the read");
    let rmcp::ServiceError::McpError(error) = error else {
        return Err(format!("expected protocol-level McpError, got {error:?}").into());
    };
    error
        .data
        .ok_or_else(|| "wire error data must be present".into())
}

fn diagnostic<'a>(result: &'a Value, hook: &str) -> TestResult<&'a Value> {
    result["summary"]["diagnostics"]
        .as_array()
        .and_then(|diagnostics| {
            diagnostics.iter().find(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(hook))
            })
        })
        .ok_or_else(|| format!("result must carry a diagnostic for hook {hook}").into())
}

async fn stop(
    client: RunningService<RoleClient, ()>,
    server_task: tokio::task::JoinHandle<()>,
) -> TestResult {
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn transform_configuration_precedes_validation_and_format_kind_is_accepted() -> TestResult {
    let hooks = [
        Hook {
            id: "format",
            kind: "format",
            script: "format.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "tests",
            kind: "test",
            script: "tests.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: true,
            include: "[]",
            exclude: "[]",
        },
    ];
    let scripts = [("format.sh", "exit 0\n"), ("tests.sh", "exit 0\n")];
    let (_directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    assert_eq!(
        read_beacon_source(&client).await?,
        INITIAL_SOURCE.trim_end()
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn validation_before_transform_is_refused_by_writes_classification() -> TestResult {
    let hooks = [
        Hook {
            id: "tests",
            kind: "test",
            script: "tests.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "late-write",
            kind: "test",
            script: "late.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
    ];
    let scripts = [("tests.sh", "exit 0\n"), ("late.sh", "exit 0\n")];
    let (_directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    let refused = refused_read(&client).await?;
    assert_eq!(refused["code"], json!("configuration_invalid"));
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("transform") && message.contains("validation"),
        "refusal must state required hook order: {message}"
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn transform_guarantees_are_refused_as_validation_only() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "workspace",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: true,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [("format.sh", "exit 0\n")];
    let (_directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    let refused = refused_read(&client).await?;
    assert_eq!(refused["code"], json!("configuration_invalid"));
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("format")
            && message.contains("guarantees belong only to validation hooks"),
        "refusal must name transform and validation guarantee rule: {message}"
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn successful_transform_defines_final_files_id_and_index_source() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "changed_paths",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [("format.sh", "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\n")];

    let (first_directory, first_client, first_task) =
        served_workspace(&hooks, &scripts, &[]).await?;
    let first = replace(&first_client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(first["status"], json!("applied"));
    let files = first["summary"]["files"]
        .as_array()
        .ok_or("applied result must carry files")?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], json!("lib.rs"));
    assert_eq!(
        files[0]["size_bytes"],
        json!(FORMATTED_SOURCE.len()),
        "the reported size must be the formatter's own bytes: {first:#}"
    );
    assert_eq!(
        fs::read_to_string(first_directory.path().join("lib.rs"))?,
        FORMATTED_SOURCE
    );
    assert_eq!(
        read_beacon_source(&first_client).await?,
        FORMATTED_SOURCE.trim_end()
    );
    let first_id = first["summary"]["id"].clone();

    let (_second_directory, second_client, second_task) =
        served_workspace(&hooks, &scripts, &[]).await?;
    let second = replace(&second_client, "pub fn beacon() -> u8 { 7 }").await?;
    assert_eq!(
        second["summary"]["id"], first_id,
        "same captured and final trees must mint same change id"
    );

    stop(first_client, first_task).await?;
    stop(second_client, second_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_transform_reports_every_final_file() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "workspace",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [(
        "format.sh",
        "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\necho 'formatted note' > notes.txt\n",
    )];
    let files = [("notes.txt", "original note\n")];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;
    let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(result["status"], json!("applied"));
    let files = result["summary"]["files"]
        .as_array()
        .ok_or("applied result must carry files")?;
    assert_eq!(
        files
            .iter()
            .map(|file| file["path"].clone())
            .collect::<Vec<_>>(),
        [json!("lib.rs"), json!("notes.txt")],
        "a workspace transform reports every file it left changed"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("notes.txt"))?,
        "formatted note\n"
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn failed_timed_out_and_out_of_scope_transforms_restore_only_hook_writes() -> TestResult {
    let cases = [
        (
            "failed",
            "echo 'pub fn beacon() -> u8 { 9 }' > lib.rs\nexit 1\n",
            "changed_paths",
            "30s",
        ),
        (
            "timed-out",
            "echo 'pub fn beacon() -> u8 { 9 }' > lib.rs\nsleep 1\n",
            "changed_paths",
            "20ms",
        ),
        (
            "outside",
            "echo 'outside write' > notes.txt\n",
            "changed_paths",
            "30s",
        ),
    ];
    for (id, script, writes, timeout) in cases {
        let hooks = [Hook {
            id,
            kind: "format",
            script: "hook.sh",
            writes,
            failure_severity: "warning",
            timeout,
            guarantee: false,
            include: "[]",
            exclude: "[]",
        }];
        let scripts = [("hook.sh", script)];
        let files = [("notes.txt", "original note\n")];
        let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;
        let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
        assert_eq!(
            result["status"],
            json!("applied"),
            "transform failure must not refuse direct edit: {result:#}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            DIRECT_SOURCE,
            "direct edit must survive hook rollback for {id}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.txt"))?,
            "original note\n",
            "hook write outside accepted result must be restored for {id}"
        );
        assert_eq!(diagnostic(&result, id)?["severity"], json!("warning"));
        stop(client, server_task).await?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_transform_keeps_an_unclassified_direct_edit() -> TestResult {
    let hooks = [Hook {
        id: "failed",
        kind: "format",
        script: "hook.sh",
        writes: "changed_paths",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [("hook.sh", "echo 'version = 9' > Cargo.lock\nexit 1\n")];
    let files = [("Cargo.lock", "version = 4\n")];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;

    let result = patch(
        &client,
        "--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n-version = 4\n+version = 3\n",
    )
    .await?;

    assert_eq!(result["status"], json!("applied"));
    assert_eq!(result["summary"]["files"][0]["path"], json!("Cargo.lock"));
    assert_eq!(
        fs::read_to_string(directory.path().join("Cargo.lock"))?,
        "version = 3\n",
        "failed hook rollback must retain unclassified direct edit"
    );
    assert_eq!(diagnostic(&result, "failed")?["severity"], json!("warning"));
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_transform_output_is_restored() -> TestResult {
    let cases = [
        ("binary", "printf '\\377' > notes.txt\n"),
        ("oversized", "head -c 4194305 /dev/zero > notes.txt\n"),
    ];
    for (id, script) in cases {
        let hooks = [Hook {
            id,
            kind: "format",
            script: "hook.sh",
            writes: "workspace",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        }];
        let scripts = [("hook.sh", script)];
        let files = [("notes.txt", "original note\n")];
        let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;

        let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;

        assert_eq!(result["status"], json!("applied"));
        assert_eq!(
            fs::read(directory.path().join("notes.txt"))?,
            b"original note\n",
            "unavailable hook output must be restored for {id}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            DIRECT_SOURCE,
            "direct edit must survive unavailable hook output for {id}"
        );
        assert_eq!(diagnostic(&result, id)?["severity"], json!("warning"));
        stop(client, server_task).await?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_gitignore_changes_keep_original_file_membership() -> TestResult {
    let cases = [
        (
            "tighten",
            "echo 'notes.txt' > .gitignore\necho 'hook note' > notes.txt\nexit 1\n",
            "",
        ),
        ("relax", ": > .gitignore\nexit 1\n", "ignored.txt\n"),
    ];
    for (id, script, ignore) in cases {
        let hooks = [Hook {
            id,
            kind: "format",
            script: "hook.sh",
            writes: "workspace",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        }];
        let scripts = [("hook.sh", script)];
        let files = [
            (".gitignore", ignore),
            ("notes.txt", "original note\n"),
            ("ignored.txt", "must remain\n"),
        ];
        let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;

        let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;

        assert_eq!(result["status"], json!("applied"));
        assert_eq!(
            fs::read_to_string(directory.path().join(".gitignore"))?,
            ignore
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.txt"))?,
            "original note\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("ignored.txt"))?,
            "must remain\n",
            "rollback must not delete a file hidden before hook {id}"
        );
        assert_eq!(diagnostic(&result, id)?["severity"], json!("warning"));
        stop(client, server_task).await?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_input_refuses_before_direct_write() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "hook.sh",
        writes: "workspace",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [("hook.sh", "exit 0\n")];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    fs::write(directory.path().join("binary.data"), [0xff])?;

    let error = replace(&client, "pub fn beacon() -> u8 { 1 }")
        .await
        .expect_err("unavailable input must refuse change");

    assert!(error.to_string().contains("content_unavailable"));
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        INITIAL_SOURCE,
        "direct write must not land before hook input capture"
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn failed_second_transform_keeps_first_transform() -> TestResult {
    let hooks = [
        Hook {
            id: "first",
            kind: "format",
            script: "first.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "second",
            kind: "format",
            script: "second.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
    ];
    let scripts = [
        ("first.sh", "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\n"),
        (
            "second.sh",
            "echo 'pub fn beacon() -> u8 { 9 }' > lib.rs\nexit 1\n",
        ),
    ];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(result["status"], json!("applied"));
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        FORMATTED_SOURCE,
        "rollback must retain prior successful transform"
    );
    assert_eq!(diagnostic(&result, "second")?["severity"], json!("warning"));
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn validation_reads_transformed_tree_reports_failure_and_restores_writes() -> TestResult {
    let hooks = [
        Hook {
            id: "format",
            kind: "format",
            script: "format.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "observes-final",
            kind: "test",
            script: "observes.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: true,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "fails",
            kind: "test",
            script: "fails.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
        Hook {
            id: "writes-source",
            kind: "lint",
            script: "writes.sh",
            writes: "none",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
            include: "[]",
            exclude: "[]",
        },
    ];
    let scripts = [
        ("format.sh", "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\n"),
        (
            "observes.sh",
            "grep -q 'pub fn beacon() -> u8 { 2 }' lib.rs\n",
        ),
        ("fails.sh", "exit 1\n"),
        ("writes.sh", "echo 'pub fn beacon() -> u8 { 9 }' > lib.rs\n"),
    ];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(result["status"], json!("applied"));
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        FORMATTED_SOURCE,
        "validation source writes must be restored"
    );
    assert_eq!(
        result["summary"]["guarantees"],
        json!([{
            "kind": "behavior_checked",
            "scope": { "kind": "reach", "reach": "project" },
            "hook": "observes-final",
            "detail": "validation read the final tree"
        }])
    );
    assert_eq!(diagnostic(&result, "fails")?["severity"], json!("error"));
    assert_eq!(
        diagnostic(&result, "writes-source")?["severity"],
        json!("warning")
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn transform_that_erases_direct_difference_returns_unchanged() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "changed_paths",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
        include: "[]",
        exclude: "[]",
    }];
    let scripts = [("format.sh", "echo 'pub fn beacon() {}' > lib.rs\n")];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
    let result = replace(&client, "pub fn beacon() { }").await?;
    assert_eq!(result["status"], json!("unchanged"));
    assert!(
        result["summary"].is_null(),
        "unchanged result must not carry false applied summary: {result:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        INITIAL_SOURCE
    );
    stop(client, server_task).await
}

#[cfg(unix)]
#[tokio::test]
async fn hook_permission_writes_are_restored_and_never_reported_as_changed_files() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let cases = [
        ("transform-permissions", "format", "changed_paths", false),
        ("validation-permissions", "test", "none", true),
    ];
    for (id, kind, writes, guarantee) in cases {
        let hooks = [Hook {
            id,
            kind,
            script: "permissions.sh",
            writes,
            failure_severity: "warning",
            timeout: "30s",
            guarantee,
            include: "[]",
            exclude: "[]",
        }];
        let scripts = [("permissions.sh", "chmod 600 lib.rs\n")];
        let (directory, client, server_task) = served_workspace(&hooks, &scripts, &[]).await?;
        let target = directory.path().join("lib.rs");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640))?;
        let initial_mode = fs::metadata(&target)?.permissions().mode();

        let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;

        assert_eq!(result["status"], json!("applied"));
        assert_eq!(
            fs::metadata(&target)?.permissions().mode(),
            initial_mode,
            "hook permission write must be restored for {id}"
        );
        assert_eq!(
            result["summary"]["files"]
                .as_array()
                .ok_or("applied result must carry files")?
                .len(),
            1,
            "permission state must not report a second changed file for {id}"
        );
        assert_eq!(diagnostic(&result, id)?["severity"], json!("warning"));
        assert!(
            result["summary"].get("guarantees").is_none(),
            "a validation hook that changes permissions must add no guarantee, so \
             guarantees is omitted"
        );
        stop(client, server_task).await?;
    }
    Ok(())
}

/// A hook's `include` and `exclude` select it from the change's own paths.
/// One change that touches a manifest and a source file runs the hook covering
/// both exactly once, and leaves a hook covering neither out of the run.
#[cfg(unix)]
#[tokio::test]
async fn hook_path_selection_runs_a_covering_hook_once_and_skips_an_unrelated_one() -> TestResult {
    let mut rust_only = Hook::validation("rust-only", "fail.sh");
    rust_only.include = r#"["**/*.rs"]"#;
    let mut manifest_only = Hook::validation("manifest-only", "fail.sh");
    manifest_only.include = r#"["**/Cargo.toml"]"#;
    let mut generated_excluded = Hook::validation("generated-excluded", "fail.sh");
    generated_excluded.exclude = r#"["**/*.rs"]"#;
    let hooks = [rust_only, manifest_only, generated_excluded];
    let scripts = [("fail.sh", "exit 1\n")];
    let files = [("Cargo.toml", "[package]\nname = \"beacon\"\n")];
    let (_directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;

    let source_only = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(source_only["status"], json!("applied"));
    assert_eq!(
        diagnostic(&source_only, "rust-only")?["severity"],
        json!("error"),
        "a hook including the changed source runs"
    );
    assert!(
        diagnostic(&source_only, "manifest-only").is_err(),
        "a hook including only the manifest stays out of a source change: {source_only:#}"
    );
    assert!(
        diagnostic(&source_only, "generated-excluded").is_err(),
        "an exclude that covers every changed path removes the hook: {source_only:#}"
    );

    let both = patch(
        &client,
        "--- a/Cargo.toml\n\
         +++ b/Cargo.toml\n\
         @@ -1,2 +1,3 @@\n\
         \x20[package]\n\
         \x20name = \"beacon\"\n\
         +version = \"0.1.0\"\n\
         --- a/lib.rs\n\
         +++ b/lib.rs\n\
         @@ -1 +1 @@\n\
         -pub fn beacon() -> u8 { 1 }\n\
         +pub fn beacon() -> u8 { 2 }\n",
    )
    .await?;
    assert_eq!(both["status"], json!("applied"));
    let matching: Vec<&Value> = both["summary"]["diagnostics"]
        .as_array()
        .ok_or("a rust-only hook scoped to lib.rs must contribute a diagnostic")?
        .iter()
        .filter(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("rust-only"))
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "a hook runs once for a multi-file change: {both:#}"
    );
    assert_eq!(
        diagnostic(&both, "manifest-only")?["severity"],
        json!("error"),
        "the manifest path selects its own hook in the same change"
    );

    stop(client, server_task).await?;
    Ok(())
}
