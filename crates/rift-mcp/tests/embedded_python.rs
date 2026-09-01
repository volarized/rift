//! The embedded ty engine over a served Python workspace: diagnostics ride
//! an applied change, and a rename lands through the engine's references.
//!
//! No external binary and no environment gate: the engine links in with the
//! server and ty's vendored typeshed answers offline, so the suite runs in
//! every instrumented gate. The engine announces no `$/progress` work, so
//! settlement leans on the retry table alone; the fixture widens it the way
//! the tombi fixture does.

#![cfg(unix)]

mod hermetic_search;
mod workspace_client;

use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

/// The rename runs under a root spelled relative to the process working
/// directory, the spelling `rift mcp` hands the server: the embedded
/// engine is addressed in `file://` URIs, which carry no working
/// directory, so the suite is the witness that the spelling reaches it
/// resolved and answers come back in the caller's own spelling.
/// A module whose function the tests patch and rename.
const SERVICE: &str = "def serve(port: int) -> int:\n    return port\n\n\nvalue = serve(8080)\n";

/// Appends an assignment binding a string to an `int` annotation, ty's
/// `invalid-assignment` violation.
const INVALID_ASSIGNMENT_PATCH: &str = "--- a/service.py\n+++ b/service.py\n@@ -3,3 +3,4 @@\n \n \n value = serve(8080)\n+count: int = \"eight\"\n";

/// Appends one comment: a follow-up change whose engine exchange opens the
/// settled document carrying the earlier violation.
const COMMENT_PATCH: &str = "--- a/service.py\n+++ b/service.py\n@@ -4,3 +4,4 @@\n \n value = serve(8080)\n count: int = \"eight\"\n+# beacon comment\n";

/// The workspace's Python entry: the embedded ty engine, under the widened
/// retry table a no-progress engine leans on.
fn embedded_python_configuration() -> String {
    "[languages.python.lsp]\nembedded = \"ty\"\nstartup_timeout = \"2m\"\nrequest_timeout = \"2m\"\nretry = { attempts = 12, delay = \"250ms\", delay_limit = \"2s\" }\n".to_owned()
}

/// The findings one applied change carries under `code`.
fn coded_findings<'summary>(structured: &'summary Value, code: &str) -> Vec<&'summary Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!(code))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn applied_patch_carries_the_embedded_ty_diagnostic() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("service.py", SERVICE)],
        Some(embedded_python_configuration()),
    )
    .await?;

    let introduce = tool_request("patch", &json!({ "patch": INVALID_ASSIGNMENT_PATCH }));
    let structured = call_retrying_acceptance(&client, introduce).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");

    // A read returning is the proof the change published: the follow-up
    // change below then opens current bytes for its engine exchange, so the
    // asserted finding cannot race the index lane the way a diagnose right
    // behind the write can.
    let published = tool_request(
        "search",
        &json!({ "query": "eight", "target": "file", "limit": 1 }),
    );
    let answer = call_retrying_acceptance(&client, published).await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "the applied change publishes before the follow-up: {answer:#}"
    );

    let follow_up = tool_request("patch", &json!({ "patch": COMMENT_PATCH }));
    let structured = call_retrying_acceptance(&client, follow_up).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");

    let findings = coded_findings(&structured, "invalid-assignment");
    assert_eq!(
        findings.len(),
        1,
        "ty's invalid-assignment finding rides the applied change: {structured:#}"
    );
    let finding = findings[0];
    assert_eq!(finding["severity"], json!("error"), "{finding:#}");
    assert_eq!(finding["language"], json!("python"), "{finding:#}");
    assert_eq!(
        finding["span"]["unit"],
        json!("rift://file/service.py"),
        "{finding:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn rename_symbol_lands_through_the_embedded_engine() -> TestResult {
    let (directory, client, server_task) = served_relative_workspace(
        &[("service.py", SERVICE)],
        Some(embedded_python_configuration()),
    )
    .await?;

    let lookup = tool_request(
        "get_symbol",
        &json!({ "name": "serve", "language": "python" }),
    );
    let answer = call_retrying_acceptance(&client, lookup).await?;
    let symbol = answer["hits"][0]["symbol"]["id"]
        .as_str()
        .expect("the python provider declares serve")
        .to_owned();

    let rename = tool_request(
        "rename_symbol",
        &json!({ "symbol": symbol, "new_name": "handle" }),
    );
    let structured = call_retrying_acceptance(&client, rename).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");

    let renamed = std::fs::read_to_string(directory.path().join("service.py"))?;
    assert!(
        renamed.contains("def handle(port: int)") && renamed.contains("value = handle(8080)"),
        "the declaration and its call site both rename: {renamed}"
    );
    assert!(
        !renamed.contains("serve"),
        "no spelling of the old name survives: {renamed}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
