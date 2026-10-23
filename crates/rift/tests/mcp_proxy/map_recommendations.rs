//! Installed language server advice through the binary map resource.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::map::{MapRecommendation, WorkspaceMap};
use rift_protocol::search::SearchHitTarget;
use rift_protocol::search_read::SearchReadResult;
use serde_json::json;

use crate::harness::{
    StopOnDrop, TestResult, assigned_port_key, await_workspace_ready, laid_out_workspace,
    proxied_call, proxy_client, require_success, resource_json, run_rift, within,
};

type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

fn installed_server(root: &Path) -> TestResult<PathBuf> {
    let executable = root.join("node_modules/.bin/typescript-language-server");
    fs::create_dir_all(executable.parent().expect("executable parent"))?;
    let marker = root.join("engine-executed");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 91\n",
            marker.display()
        ),
    )?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    Ok(marker)
}

async fn map(client: &Client) -> TestResult<WorkspaceMap> {
    let answer = within(
        "map recommendations",
        Box::pin(client.read_resource(rmcp::model::ReadResourceRequestParams::new("rift://map"))),
    )
    .await??;
    assert_eq!(answer.contents.len(), 1, "map is one JSON resource");
    let body = resource_json(&answer, "rift://map")?;
    serde_json::from_value(body.clone())
        .map_err(|error| format!("invalid workspace map: {error}; {body}").into())
}

async fn wait_until_configured(client: &Client) -> TestResult<WorkspaceMap> {
    within("configured map recommendations", async {
        loop {
            let map = map(client).await?;
            if map.recommendations.is_empty()
                && !map.warnings.iter().any(|warning| {
                    serde_json::to_value(warning)
                        .is_ok_and(|value| value["code"] == "local_index_preparing")
                })
            {
                return Ok(map);
            }
            tokio::time::sleep(crate::PRESENCE_POLL_INTERVAL).await;
        }
    })
    .await?
}

async fn apply(root: &Path, recommendation: &MapRecommendation) -> TestResult {
    let command = recommendation.command.clone();
    // The CLI's existing regression parses native shell quoting without executing
    // the suggested program. The binary harness then applies those exact arguments.
    let parsed = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .args(["-c", &format!("set -- {command}; printf '%s\\0' \"$@\"")])
            .output()
    })
    .await??;
    require_success(&parsed, "parse map apply command")?;
    let arguments = String::from_utf8(parsed.stdout)?;
    let arguments: Vec<_> = arguments
        .split('\0')
        .filter(|value| !value.is_empty())
        .collect();
    assert_eq!(arguments.first(), Some(&"rift"), "{recommendation:?}");
    let applied = run_rift(root, &arguments[1..]).await?;
    require_success(&applied, "apply map recommendation")
}

fn recommendation(map: &WorkspaceMap, root: &str) -> MapRecommendation {
    assert_eq!(map.recommendations.len(), 1, "{map:?}");
    let recommendation = map.recommendations[0].clone();
    assert_eq!(recommendation.language.identity_segment(), "typescript");
    assert_eq!(recommendation.root, root, "{map:?}");
    recommendation
}

#[tokio::test]
async fn installed_map_advice_matches_cli_and_disappears_on_same_connection() -> TestResult {
    let directory = laid_out_workspace(
        &[
            ("package.json", "{}"),
            ("main.ts", "export const beacon = 1;\n"),
        ],
        &assigned_port_key()?,
    )?;
    let root = directory.path();
    let marker = installed_server(root)?;
    let before = fs::read(root.join("rift.toml"))?;
    let _stop = StopOnDrop::new(root);
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let initial = map(&client).await?;
    let recommendation = recommendation(&initial, ".");
    let cli = run_rift(root, &["config", "recommend"]).await?;
    require_success(&cli, "CLI recommendations")?;
    assert!(String::from_utf8(cli.stdout)?.contains(&recommendation.command));
    assert_eq!(map(&client).await?, initial, "map advice is retained");
    assert_eq!(fs::read(root.join("rift.toml"))?, before);
    assert!(!marker.exists(), "reading recommendations starts no engine");
    apply(root, &recommendation).await?;
    let configured = wait_until_configured(&client).await?;
    let serialized = serde_json::to_value(configured)?;
    assert!(serialized.get("recommendations").is_none(), "{serialized}");
    assert!(!marker.exists(), "applying configuration starts no engine");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn map_profile_advice_requires_reconnect_before_named_project_reads() -> TestResult {
    let child_configuration = format!(
        "[search.vector]\ndisabled=true\n[server]\nidle_timeout=\"60s\"\n{}",
        assigned_port_key()?
    );
    let directory = laid_out_workspace(
        &[
            (
                "docs/package.json",
                "{\"name\":\"map-profile-fixture\",\"version\":\"1.0.0\",\"types\":\"./page.ts\"}",
            ),
            ("docs/page.ts", "export function beacon() { return 1; }\n"),
            ("docs/rift.toml", &child_configuration),
        ],
        &assigned_port_key()?,
    )?;
    let root = directory.path();
    let docs = root.join("docs");
    let marker = installed_server(&docs)?;
    let _stop = StopOnDrop::new(root);
    let _docs_stop = StopOnDrop::new(&docs);
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let recommendation = recommendation(&map(&client).await?, "docs");
    assert!(recommendation.command.contains("--profile=docs"));
    apply(root, &recommendation).await?;
    wait_until_configured(&client).await?;
    let query = json!({
        "query":"beacon", "target":"symbol", "scope":"local@docs",
        "paths":{"include":["page.ts"]},
    });
    let refused = proxied_call(&client, "search", &query)
        .await
        .expect_err("the existing connection has no docs registration");
    let Some(rmcp::ServiceError::McpError(error)) = refused.downcast_ref::<rmcp::ServiceError>()
    else {
        return Err(format!("expected a named scope refusal: {refused:?}").into());
    };
    assert!(error.message.contains("scope"), "{error:?}");
    client.cancel().await?;
    let child = proxy_client(&docs).await?;
    await_workspace_ready(&child).await?;
    child.cancel().await?;
    let reconnected = proxy_client(root).await?;
    let answer = proxied_call(&reconnected, "search", &query).await?;
    let result: SearchReadResult = serde_json::from_value(answer.clone())?;
    let SearchReadResult::Results(result) = result else {
        return Err(format!("named project search unavailable: {answer}").into());
    };
    assert_eq!(result.results.len(), 1, "{answer}");
    let SearchHitTarget::Symbol { symbol } = &result.results[0].hit else {
        return Err(format!("named project symbol absent: {answer}").into());
    };
    let id = symbol
        .id
        .as_ref()
        .ok_or_else(|| format!("symbol ID absent: {answer}"))?;
    assert_eq!(
        SymbolIdentity::parse(id.as_str())?.owner(),
        &SymbolOwner::NamedLocal {
            name: "docs".to_owned()
        },
        "{answer}"
    );
    assert!(!marker.exists(), "lexical project reads start no engine");
    reconnected.cancel().await?;
    Ok(())
}
