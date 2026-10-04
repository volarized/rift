//! Shared scaffolding for served workspace integration suites.
//!
//! Each workspace disables vector search through `hermetic_search.rs`
//! and drives the read tools through a live rmcp client.

use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rift_protocol::error::{ErrorCode, RetryDirective};
use rmcp::ServiceExt as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ReadResourceResult,
    ResourceContents,
};
use serde_json::Value;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// One served workspace: the directory that holds it, the client speaking
/// to it, and the task serving it.
pub(crate) type ServedWorkspace = (
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
);

/// Builds one workspace of `files`, optionally with LSP configuration, and
/// serves it to one client.
pub(crate) async fn served_workspace(
    files: &[(&str, &str)],
    lsp_configuration: Option<String>,
) -> TestResult<ServedWorkspace> {
    let directory = laid_out_workspace(files, lsp_configuration)?;
    let (client, server_task) = served_root(directory.path()).await?;
    Ok((directory, client, server_task))
}

/// Builds one workspace of `files`, runs `prepare` over its directory, and serves it to
/// one client: a suite installing the packages its engine runs from does it here, so the
/// server's first capture already holds them.
pub(crate) async fn served_prepared_workspace(
    files: &[(&str, &str)],
    lsp_configuration: Option<String>,
    prepare: impl FnOnce(&Path),
) -> TestResult<ServedWorkspace> {
    let directory = laid_out_workspace(files, lsp_configuration)?;
    prepare(directory.path());
    let (client, server_task) = served_root(directory.path()).await?;
    Ok((directory, client, server_task))
}

/// The same workspace, served under a root spelled relative to the process
/// working directory.
///
/// This is the spelling the CLI hands the server: `rift mcp` and
/// `rift server start` both serve the working directory, which they name
/// `.`. Every read below the root resolves against that directory
/// either way, so only the engine tier can tell the two spellings apart -
/// it is addressed in `file://` URIs, which carry no working directory.
/// A test cannot change the process directory without disturbing the
/// suites running beside it, so it spells the same relative form the long
/// way, as `..` segments down to the filesystem root and back up.
pub(crate) async fn served_relative_workspace(
    files: &[(&str, &str)],
    lsp_configuration: Option<String>,
) -> TestResult<ServedWorkspace> {
    let directory = laid_out_workspace_in(&relative_workspace_parent()?, files, lsp_configuration)?;
    let (client, server_task) = served_root(&relative_spelling(directory.path())?).await?;
    Ok((directory, client, server_task))
}

/// The directory a relative-root workspace is created in: the system temporary
/// directory when it shares the working directory's volume, and that volume's root
/// otherwise.
///
/// A relative spelling cannot leave its volume. A Windows runner checks the repository
/// out on `D:` while its temporary directory is on `C:`, and no `..` chain from the
/// test's working directory reaches `C:`. The volume root stays outside the checkout,
/// whose ignore rules the embedded `ty` engine would apply to a workspace below it.
fn relative_workspace_parent() -> TestResult<PathBuf> {
    let volume_root = |path: &Path| -> PathBuf {
        path.components()
            .take_while(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            .collect()
    };
    let current = std::env::current_dir()?;
    let temporary = std::env::temp_dir();
    if volume_root(&current) == volume_root(&temporary) {
        Ok(temporary)
    } else {
        Ok(volume_root(&current))
    }
}

/// One temporary workspace holding `files` and a `rift.toml`: tables that keep vector
/// acquisition and global HTTP off, then the LSP configuration when the suite drives one.
///
/// A table header ends where the next one begins, so the LSP configuration follows
/// unchanged and each suite still proves whatever its own table carries.
fn laid_out_workspace(
    files: &[(&str, &str)],
    lsp_configuration: Option<String>,
) -> TestResult<tempfile::TempDir> {
    laid_out_workspace_in(&std::env::temp_dir(), files, lsp_configuration)
}

/// [`laid_out_workspace`] below `parent`.
fn laid_out_workspace_in(
    parent: &Path,
    files: &[(&str, &str)],
    lsp_configuration: Option<String>,
) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir_in(parent)?;
    for (name, source) in files {
        let path = directory.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, source)?;
    }
    let mut configuration = crate::hermetic_search::HERMETIC_TABLES.to_owned();
    if !lsp_configuration
        .as_deref()
        .is_some_and(|value| value.contains("[global]"))
    {
        configuration.push_str("\n[global]\nenabled = false\n");
    }
    if let Some(lsp_configuration) = lsp_configuration {
        configuration.push('\n');
        configuration.push_str(&lsp_configuration);
    }
    fs::write(directory.path().join("rift.toml"), configuration)?;
    Ok(directory)
}

/// Serves the workspace at `root` to one client over an in-process duplex.
pub(crate) async fn served_root(
    root: &Path,
) -> TestResult<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let (client, server_task) = served_root_unsettled(root).await?;
    await_workspace_ready(&client).await?;
    Ok((client, server_task))
}

/// Serves `root` without waiting for initial file preparation.
pub(crate) async fn served_root_unsettled(
    root: &Path,
) -> TestResult<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    Ok((client, server_task))
}

/// MIME type of the compact text content a resource read returns first.
const RESOURCE_TEXT_MIME: &str = "text/plain";
/// MIME type of the JSON body a resource read returns second.
const RESOURCE_JSON_MIME: &str = "application/json";

/// The `(mime type, text)` of every content of a resource read; each must be text and carry
/// the requested `uri`.
fn resource_texts<'a>(
    answer: &'a ReadResourceResult,
    uri: &str,
) -> TestResult<Vec<(&'a str, &'a str)>> {
    let mut texts = Vec::with_capacity(answer.contents.len());
    for content in &answer.contents {
        let ResourceContents::TextResourceContents {
            uri: found,
            mime_type,
            text,
            ..
        } = content
        else {
            return Err(format!("{uri}: every content must be text: {content:?}").into());
        };
        if found != uri {
            return Err(format!("{uri}: a content carries uri {found}").into());
        }
        texts.push((mime_type.as_deref().unwrap_or_default(), text.as_str()));
    }
    Ok(texts)
}

/// The JSON body of a resource read.
///
/// Requires exactly one `application/json` content; when two contents arrive, the other is
/// `text/plain`. Every content carries `uri`.
pub(crate) fn resource_json(answer: &ReadResourceResult, uri: &str) -> TestResult<Value> {
    let texts = resource_texts(answer, uri)?;
    let (json, others): (Vec<_>, Vec<_>) = texts
        .iter()
        .partition(|(mime, _)| *mime == RESOURCE_JSON_MIME);
    let [(_, body)] = json.as_slice() else {
        return Err(
            format!("{uri}: want exactly one {RESOURCE_JSON_MIME} content: {texts:?}").into(),
        );
    };
    if others.len() > 1 || others.iter().any(|(mime, _)| *mime != RESOURCE_TEXT_MIME) {
        return Err(
            format!("{uri}: the other content must be {RESOURCE_TEXT_MIME}: {texts:?}").into(),
        );
    }
    Ok(serde_json::from_str(body)?)
}

/// The compact text of a resource read: its first content, which must be non-empty
/// `text/plain` carrying `uri`.
pub(crate) fn resource_text<'a>(answer: &'a ReadResourceResult, uri: &str) -> TestResult<&'a str> {
    match resource_texts(answer, uri)?.first() {
        Some((mime, text)) if *mime == RESOURCE_TEXT_MIME && !text.is_empty() => Ok(*text),
        other => Err(format!(
            "{uri}: the first content must be non-empty {RESOURCE_TEXT_MIME}: {other:?}"
        )
        .into()),
    }
}

/// Reads map through MCP until local file preparation has completed.
pub(crate) async fn await_workspace_ready(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> TestResult<Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let answer = tokio::time::timeout_at(
            deadline,
            client.read_resource(ReadResourceRequestParams::new("rift://map".to_owned())),
        )
        .await??;
        let body = resource_json(&answer, "rift://map")?;
        let preparing = body["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|warning| warning["code"] == "local_index_preparing");
        if !preparing {
            return Ok(body);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("workspace map remained in preparation: {body}").into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Calls `search` until the lexical population pass has landed.
///
/// Workspace map readiness covers local file preparation. The lexical lane commits its
/// initial write behind that publication, so a search can still return identifier matches
/// with `lexical_ranking_unavailable` or `stale_index` while the store catches up.
///
/// # Errors
///
/// Returns an error containing the last answer after 60 polling delays and up to 60
/// follow-up requests if the store does not rank the answer. The 50 ms delays total at
/// most three seconds; request time is additional. Each request allows up to eight
/// attempts for acceptance refusals and has no elapsed-time deadline.
pub(crate) async fn search_after_population(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: &Value,
) -> TestResult<Value> {
    const SEARCH_TIER_ATTEMPTS_MAX: usize = 60;
    const SEARCH_TIER_POLL: std::time::Duration = std::time::Duration::from_millis(50);

    let query = arguments["query"].as_str().unwrap_or("<missing query>");
    let mut answer = call_retrying_acceptance(client, tool_request("search", arguments)).await?;
    for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
        let population_pending =
            answer["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|warning| {
                    matches!(
                        warning["code"].as_str(),
                        Some("lexical_ranking_unavailable" | "stale_index")
                    )
                });
        if !population_pending {
            return Ok(answer);
        }
        tokio::time::sleep(SEARCH_TIER_POLL).await;
        answer = call_retrying_acceptance(client, tool_request("search", arguments)).await?;
    }
    Err(format!(
        "the population lane never stamped the served tree for query {query}; the last answer was {answer:#}"
    )
    .into())
}

/// The path from the process working directory to `target`, as one `..`
/// segment per directory above the working directory followed by the
/// target's own segments.
fn relative_spelling(target: &Path) -> TestResult<PathBuf> {
    let named = |component: &Component<'_>| matches!(component, Component::Normal(_));
    let current = std::env::current_dir()?;
    let mut spelling = PathBuf::new();
    for _ in current.components().filter(named) {
        spelling.push("..");
    }
    spelling.extend(target.components().filter(named));
    Ok(spelling)
}

/// One tool call's request parameters from a JSON argument object.
pub(crate) fn tool_request(name: &'static str, arguments: &Value) -> CallToolRequestParams {
    let arguments = arguments
        .as_object()
        .cloned()
        .expect("tool arguments are an object");
    CallToolRequestParams::new(name).with_arguments(arguments)
}

/// Most attempts one request retries before giving up on acceptance.
const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// Calls the tool, retrying the failure the server advertises as
/// `retry: same_request`: a write to the served workspace while the server
/// runs can move the index between one request's snapshot and its acceptance.
///
/// A request refused on every attempt fails with the last failure's text, so the
/// failure names the code and the cause the server kept giving.
pub(crate) async fn call_retrying_acceptance(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    params: CallToolRequestParams,
) -> TestResult<Value> {
    let mut refused = None;
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        let result = client.call_tool(params.clone()).await?;
        if result.is_error != Some(true) {
            return result
                .structured_content
                .ok_or_else(|| "the tool must return structured content".into());
        }
        let failure = tool_failure(&result)?;
        if failure.retry != RetryDirective::SameRequest {
            return Err(format!("the tool failed: {}", failure.text).into());
        }
        refused = Some(failure.text);
    }
    Err(format!(
        "the server kept refusing a retryable request through {ACCEPTANCE_ATTEMPTS_MAX} \
         attempts; the last refusal: {}",
        refused.unwrap_or_default()
    )
    .into())
}

/// The facts of one failed tool call, read from its error text.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    /// The code of the first entry head.
    pub(crate) code: ErrorCode,
    /// Line 3 with `\n`, `\r`, `\t` and `\u{HEX}` turned back into characters.
    pub(crate) message: String,
    /// The retry directive of the first entry head.
    pub(crate) retry: RetryDirective,
    /// The whole text block, for assertions on `limit`, cause entry and diagnostic lines.
    pub(crate) text: String,
}

/// The failure a call completed with: the call must return `Ok` with an error result.
pub(crate) fn failed_call(
    outcome: Result<CallToolResult, rmcp::ServiceError>,
) -> TestResult<ToolFailure> {
    match outcome {
        Ok(result) => tool_failure(&result),
        Err(error) => Err(format!("the call must complete with an error result: {error:?}").into()),
    }
}

/// Reads the failure of an error result.
///
/// Requires `is_error == Some(true)`, no structured content, and exactly one text block.
/// Line 1 is `N error(s)`; line 2 is the first entry head `  <code> · retry <directive>`;
/// line 3 is its message at 4 spaces, on one line.
pub(crate) fn tool_failure(result: &CallToolResult) -> TestResult<ToolFailure> {
    if result.is_error != Some(true) {
        return Err(format!("is_error must be Some(true), got {:?}", result.is_error).into());
    }
    if result.structured_content.is_some() {
        return Err("an error result must carry no structured content".into());
    }
    let [block] = result.content.as_slice() else {
        return Err(format!("content must be one block, got {:?}", result.content).into());
    };
    let text = block
        .as_text()
        .ok_or("the content block must be text")?
        .text
        .clone();
    let mut lines = text.lines();
    let title = lines.next().ok_or("the error text is empty")?;
    if !failure_title(title) {
        return Err(format!("line 1 must be `N error(s)`: {title}").into());
    }
    let head = lines
        .next()
        .ok_or_else(|| format!("the error text has no entry head: {text}"))?;
    let message = lines
        .next()
        .and_then(|line| line.strip_prefix(FAILURE_LINE_INDENT))
        .ok_or_else(|| format!("line 3 must be the message at two levels: {text}"))?;
    let (code, retry) = failure_head(head)?;
    Ok(ToolFailure {
        code,
        message: unescaped_line(message),
        retry,
        text,
    })
}

/// Text of one indent level of an answer text.
const INDENT_UNIT: &str = "\t";
/// Indent of the lines under an entry head of a failure text: two levels.
const FAILURE_LINE_INDENT: &str = "\t\t";

/// Whether `line` is the title of a failure: `N error` or `N errors`.
fn failure_title(line: &str) -> bool {
    line.strip_suffix(" errors")
        .or_else(|| line.strip_suffix(" error"))
        .is_some_and(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The code and retry directive of the entry head `<code> · retry <directive>`, one level deep.
fn failure_head(line: &str) -> TestResult<(ErrorCode, RetryDirective)> {
    let (code, retry) = line
        .strip_prefix(INDENT_UNIT)
        .filter(|rest| !rest.starts_with(INDENT_UNIT))
        .and_then(|rest| rest.split_once(" · retry "))
        .ok_or_else(|| {
            format!("line 2 must be `<code> · retry <directive>` at one level: {line}")
        })?;
    Ok((
        serde_json::from_value(Value::String(code.to_owned()))?,
        serde_json::from_value(Value::String(retry.to_owned()))?,
    ))
}

/// Turns `\n`, `\r`, `\t` and `\u{HEX}` back into characters; any other backslash stays.
fn unescaped_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some((before, after)) = rest.split_once('\\') {
        out.push_str(before);
        if let Some((character, tail)) = escape_at(after) {
            out.push(character);
            rest = tail;
        } else {
            out.push('\\');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The character the escape at the start of `after` stands for, and the text past it.
fn escape_at(after: &str) -> Option<(char, &str)> {
    if let Some(tail) = after.strip_prefix('n') {
        return Some(('\n', tail));
    }
    if let Some(tail) = after.strip_prefix('r') {
        return Some(('\r', tail));
    }
    if let Some(tail) = after.strip_prefix('t') {
        return Some(('\t', tail));
    }
    let (digits, tail) = after.strip_prefix("u{")?.split_once('}')?;
    let character = char::from_u32(u32::from_str_radix(digits, 16).ok()?)?;
    Some((character, tail))
}
