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
type = "command"
id = "{id}"
kind = "{kind}"
program = "sh"
arguments = ["{script}"]
changed_paths = "none"
writes = "{writes}"
working_directory = ""
environment = {{}}
timeout = "{timeout}"
output_limit = "4kb"
failure_severity = "{failure_severity}"
guarantees = {guarantees}
determinism = "deterministic"
"#,
            id = hook.id,
            kind = hook.kind,
            script = hook.script,
            writes = hook.writes,
            timeout = hook.timeout,
            failure_severity = hook.failure_severity,
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

async fn read_beacon_source(client: &RunningService<RoleClient, ()>) -> TestResult<String> {
    let request = CallToolRequestParams::new("get_symbol")
        .with_arguments(arguments(&json!({ "name": "beacon" }))?);
    let structured = call_retrying_acceptance(client, request)
        .await?
        .structured_content
        .ok_or("get_symbol must return structured content")?;
    structured["hits"][0]["source"]["text"]
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
        },
        Hook {
            id: "tests",
            kind: "test",
            script: "tests.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: true,
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
        },
        Hook {
            id: "late-write",
            kind: "test",
            script: "late.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
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
async fn successful_transform_defines_final_edits_id_and_index_source() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "changed_paths",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
    }];
    let scripts = [("format.sh", "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\n")];

    let (first_directory, first_client, first_task) =
        served_workspace(&hooks, &scripts, &[]).await?;
    let first = replace(&first_client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(first["status"], json!("applied"));
    assert_eq!(first["summary"]["paths"], json!(["lib.rs"]));
    let edits = first["summary"]["edits"]
        .as_array()
        .ok_or("applied result must carry edits")?;
    assert!(
        edits
            .iter()
            .any(|edit| edit["text"] == json!(FORMATTED_SOURCE)),
        "final edits must carry formatter bytes: {first:#}"
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
async fn workspace_transform_reports_every_final_path_and_edit() -> TestResult {
    let hooks = [Hook {
        id: "format",
        kind: "format",
        script: "format.sh",
        writes: "workspace",
        failure_severity: "warning",
        timeout: "30s",
        guarantee: false,
    }];
    let scripts = [(
        "format.sh",
        "echo 'pub fn beacon() -> u8 { 2 }' > lib.rs\necho 'formatted note' > notes.txt\n",
    )];
    let files = [("notes.txt", "original note\n")];
    let (directory, client, server_task) = served_workspace(&hooks, &scripts, &files).await?;
    let result = replace(&client, "pub fn beacon() -> u8 { 1 }").await?;
    assert_eq!(result["status"], json!("applied"));
    assert_eq!(result["summary"]["paths"], json!(["lib.rs", "notes.txt"]));
    assert_eq!(
        result["summary"]["edits"]
            .as_array()
            .ok_or("applied result must carry edits")?
            .len(),
        2
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
        },
        Hook {
            id: "second",
            kind: "format",
            script: "second.sh",
            writes: "changed_paths",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
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
        },
        Hook {
            id: "observes-final",
            kind: "test",
            script: "observes.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: true,
        },
        Hook {
            id: "fails",
            kind: "test",
            script: "fails.sh",
            writes: "none",
            failure_severity: "error",
            timeout: "30s",
            guarantee: false,
        },
        Hook {
            id: "writes-source",
            kind: "lint",
            script: "writes.sh",
            writes: "none",
            failure_severity: "warning",
            timeout: "30s",
            guarantee: false,
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
async fn hook_permission_writes_are_restored_and_never_reported_as_byte_edits() -> TestResult {
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
            result["summary"]["edits"]
                .as_array()
                .ok_or("applied result must carry edits")?
                .len(),
            1,
            "permission state must not produce a byte edit for {id}"
        );
        assert_eq!(diagnostic(&result, id)?["severity"], json!("warning"));
        assert!(
            result["summary"]["guarantees"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "a validation hook that changes permissions must add no guarantee"
        );
        stop(client, server_task).await?;
    }
    Ok(())
}
