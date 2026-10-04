//! The resources the server publishes, and how a resource URI is read.
//!
//! One family lives here: `rift://logs`, the server's own recorded diagnostics. The workspace and
//! map answers are built here too. Every read answers two contents for the requested URI: compact
//! text first, then the JSON body.
//!
//! A tool answers a question about the workspace; this answers a question about
//! the server that was supposed to answer it. The two never share a path,
//! because the case that needs the logs most is the one where the workspace
//! reads refuse.

use rift_index::{LOG_LEVELS, LOG_PAGE_RECORDS_MAX, LogQuery, StoredLogRecord};
use rift_protocol::map::WorkspaceMap;
use rift_protocol::workspace::WorkspaceResourcePage;
use rmcp::ErrorData;
use rmcp::model::{ReadResourceResult, Resource, ResourceContents, ResourceTemplate};
use serde_json::{Value, json};

use crate::output::{LogFields, LogLine, LogsPage, resource_text};

use crate::failure::McpErrorFailExt as _;

/// The whole recorded set, newest first.
pub(crate) const LOGS_URI: &str = "rift://logs";
/// The URI prefix a level-restricted read carries.
pub(crate) const LOGS_LEVEL_PREFIX: &str = "rift://logs/level/";
/// The URI prefix a component-restricted read carries.
pub(crate) const LOGS_COMPONENT_PREFIX: &str = "rift://logs/component/";
/// The template a client expands to reach one level.
pub(crate) const LOGS_LEVEL_TEMPLATE: &str = "rift://logs/level/{level}";
/// The template a client expands to reach one component.
pub(crate) const LOGS_COMPONENT_TEMPLATE: &str = "rift://logs/component/{component}";
/// The workspace orientation snapshot.
pub(crate) const MAP_URI: &str = "rift://map";
/// The first workspace resource page.
pub(crate) const WORKSPACE_URI: &str = "rift://workspace";
/// The template a client expands to reach one workspace resource page.
pub(crate) const WORKSPACE_TEMPLATE: &str = "rift://workspace{?page_index}";
/// The query prefix selecting one workspace resource page.
const WORKSPACE_QUERY_PREFIX: &str = "rift://workspace?";
/// The media type of the JSON content of a resource read, and of every listed resource.
const RESOURCE_MEDIA_TYPE: &str = "application/json";
/// The media type of the compact text content of a resource read.
const TEXT_MEDIA_TYPE: &str = "text/plain";

/// The resources the server lists.
pub(crate) fn declared_resources() -> Vec<Resource> {
    vec![
        Resource::new(LOGS_URI, "logs")
            .with_title("Server logs")
            .with_description(
                "The server's own diagnostics, newest first: what each request, rebuild, \
                 and engine did. Read this when a tool refuses and the refusal alone does \
                 not say why.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
        Resource::new(WORKSPACE_URI, "workspace")
            .with_title("Workspace")
            .with_description(
                "The server's effective language configuration, with one page of the \
                 captured source catalog.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
        Resource::new(MAP_URI, "map")
            .with_title("Workspace map")
            .with_description(
                "Workspace orientation snapshot, computed once per index publication and \
                 served from cache. Carries language totals, the module tree, the \
                 most-referenced symbols, entry points, and documentation paths.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
    ]
}

/// The templates the server lists, one per filtered read.
pub(crate) fn declared_templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(LOGS_LEVEL_TEMPLATE, "logs-at-level")
            .with_title("Server logs at one level")
            .with_description(
                "The recorded diagnostics at one severity: trace, debug, info, warn, or \
                 error.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
        ResourceTemplate::new(LOGS_COMPONENT_TEMPLATE, "logs-for-component")
            .with_title("Server logs from one component")
            .with_description(
                "The recorded diagnostics one component emitted, as its spans label them: \
                 index, search, engine, change, or logs.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
        ResourceTemplate::new(WORKSPACE_TEMPLATE, "workspace-page")
            .with_title("Workspace page")
            .with_description(
                "One page of the workspace resource, selected by zero-based `page_index`.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
    ]
}

/// The read one log URI asks for, or the refusal that URI earns.
///
/// `page_records` is the configured page, itself bounded by
/// [`LOG_PAGE_RECORDS_MAX`] before it reaches the store.
pub(crate) fn log_query(uri: &str, page_records: u64) -> Result<LogQuery, ErrorData> {
    let page = usize::try_from(page_records).unwrap_or(LOG_PAGE_RECORDS_MAX);
    if uri == LOGS_URI {
        return Ok(LogQuery::newest(page));
    }
    if let Some(level) = uri.strip_prefix(LOGS_LEVEL_PREFIX) {
        let level = level.to_lowercase();
        if !LOG_LEVELS.contains(&level.as_str()) {
            return ErrorData::invalid_params(
                format!(
                    "the level segment must be one of {}, not {level:?}",
                    LOG_LEVELS.join(", ")
                ),
                None,
            )
            .fail();
        }
        return Ok(LogQuery::newest(page).at_level(&level));
    }
    if let Some(component) = uri.strip_prefix(LOGS_COMPONENT_PREFIX) {
        if component.is_empty() || component.contains('/') {
            return ErrorData::invalid_params(
                "the component segment must be one path segment and cannot be empty",
                None,
            )
            .fail();
        }
        return Ok(LogQuery::newest(page).for_component(component));
    }
    ErrorData::resource_not_found(format!("no resource is published at {uri:?}"), None).fail()
}

/// Returns whether `uri` addresses the workspace resource family.
#[must_use]
pub(crate) fn is_workspace_uri(uri: &str) -> bool {
    uri == WORKSPACE_URI || uri.starts_with(WORKSPACE_QUERY_PREFIX)
}

/// The zero-based page one workspace resource URI selects.
pub(crate) fn workspace_page_index(uri: &str) -> Result<u64, ErrorData> {
    if uri == WORKSPACE_URI {
        return Ok(0);
    }
    let Some(query) = uri.strip_prefix(WORKSPACE_QUERY_PREFIX) else {
        return ErrorData::resource_not_found(format!("no resource is published at {uri:?}"), None)
            .fail();
    };
    let Some(value) = query.strip_prefix("page_index=") else {
        return ErrorData::invalid_params(
            format!("`page_index` must be a zero-based integer, such as 0, not {query:?}"),
            None,
        )
        .fail();
    };
    value.parse::<u64>().map_err(|_| {
        ErrorData::invalid_params(
            format!("`page_index` must be a zero-based integer, such as 0, not {value:?}"),
            None,
        )
    })
}

/// The answer one log read returns: the records it selected, newest first.
///
/// # Errors
///
/// Returns the registered answer failure when the compact text crosses its byte limit.
pub(crate) fn rendered_logs(
    uri: &str,
    records: &[StoredLogRecord],
) -> Result<ReadResourceResult, ErrorData> {
    let page = LogsPage {
        records: records.iter().map(log_line).collect(),
        unavailable: None,
    };
    logs_answer(uri, &page)
}

/// The answer a read earns when the store never opened: an empty set, and the
/// reason it is empty. A refusal here would leave the caller unable to tell an
/// unrecorded run from a quiet one.
///
/// # Errors
///
/// Returns the registered answer failure when the compact text cannot be written.
pub(crate) fn logs_unavailable(uri: &str, reason: &str) -> Result<ReadResourceResult, ErrorData> {
    let page = LogsPage {
        records: Vec::new(),
        unavailable: Some(reason),
    };
    logs_answer(uri, &page)
}

/// The typed answer one workspace resource read returns.
///
/// # Errors
///
/// Returns the registered answer failure when the compact text crosses its byte limit.
pub(crate) fn rendered_workspace(
    uri: &str,
    page: &WorkspaceResourcePage,
) -> Result<ReadResourceResult, ErrorData> {
    let body = serde_json::to_string(page)
        .unwrap_or_else(|error| unreachable!("workspace resource pages serialize: {error}"));
    Ok(answer(uri, resource_text(page)?, body))
}

/// The typed answer one `rift://map` read returns: the cached snapshot, as text and as JSON.
///
/// # Errors
///
/// Returns the registered answer failure when the compact text crosses its byte limit.
pub(crate) fn rendered_map(uri: &str, map: &WorkspaceMap) -> Result<ReadResourceResult, ErrorData> {
    let body = serde_json::to_string(map)
        .unwrap_or_else(|error| unreachable!("the workspace map serializes: {error}"));
    Ok(answer(uri, resource_text(map)?, body))
}

/// One read answer: the compact text first, then the JSON body, both for `uri`.
fn answer(uri: &str, text: String, json: String) -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type(TEXT_MEDIA_TYPE),
        ResourceContents::text(json, uri).with_mime_type(RESOURCE_MEDIA_TYPE),
    ])
}

/// The text and the JSON body of one log page, both made from `page`.
fn logs_answer(uri: &str, page: &LogsPage<'_>) -> Result<ReadResourceResult, ErrorData> {
    Ok(answer(
        uri,
        resource_text(page)?,
        logs_json(uri, page).to_string(),
    ))
}

/// The JSON body of one log page.
fn logs_json(uri: &str, page: &LogsPage<'_>) -> Value {
    let mut body = json!({
        "uri": uri,
        "records": page.records.iter().map(record_json).collect::<Vec<Value>>(),
        "record_count": page.records.len(),
    });
    if let Some(reason) = page.unavailable {
        body["unavailable"] = json!(reason);
    }
    body
}

/// One stored record as the view the text and the JSON are both made from. `fields` is the
/// object it was rendered from when it parses, and the text when it does not, so a reader never
/// has to unquote JSON out of a string.
fn log_line(stored: &StoredLogRecord) -> LogLine<'_> {
    let record = stored.record();
    LogLine {
        identity: stored.identity(),
        recorded_at_ms: record.recorded_at_ms(),
        level: record.level(),
        target: record.target(),
        component: record.component(),
        operation: record.operation(),
        message: record.message(),
        fields: LogFields::parse(record.fields()),
    }
}

/// One record of the view as the JSON wire carries it.
fn record_json(line: &LogLine<'_>) -> Value {
    let LogLine {
        identity,
        recorded_at_ms,
        level,
        target,
        component,
        operation,
        message,
        fields,
    } = line;
    json!({
        "identity": identity,
        "recorded_at_ms": recorded_at_ms,
        "level": level,
        "target": target,
        "component": component,
        "operation": operation,
        "message": message,
        "fields": fields.to_json(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        LOGS_COMPONENT_PREFIX, LOGS_LEVEL_PREFIX, LOGS_URI, MAP_URI, WORKSPACE_TEMPLATE,
        WORKSPACE_URI, declared_resources, declared_templates, is_workspace_uri, log_query,
        logs_unavailable, rendered_logs, rendered_map, rendered_workspace, workspace_page_index,
    };
    use crate::output::resource_text;
    use rift_index::{LOG_PAGE_RECORDS_MAX, LogRecord, LogStore, StoredLogRecord};
    use rift_protocol::map::WorkspaceMap;
    use rift_protocol::read::{Digest, Pagination};
    use rift_protocol::workspace::WorkspaceResourcePage;
    use rmcp::model::{ReadResourceResult, ResourceContents};
    use serde_json::{Value, json};

    const PAGE: u64 = 100;
    /// The same page as the read bound it becomes.
    const PAGE_RECORDS: usize = 100;

    /// The two contents one rendered answer carries, as compact text and as JSON.
    ///
    /// Asserts the order, the media types, and that both are for `uri`.
    fn contents(result: &ReadResourceResult, uri: &str) -> (String, String) {
        let [
            ResourceContents::TextResourceContents {
                uri: text_uri,
                mime_type: text_mime,
                text: compact,
                ..
            },
            ResourceContents::TextResourceContents {
                uri: json_uri,
                mime_type: json_mime,
                text: json,
                ..
            },
        ] = &result.contents[..]
        else {
            unreachable!("a resource read answers two text contents: {result:?}");
        };
        assert_eq!((text_uri.as_str(), json_uri.as_str()), (uri, uri));
        assert_eq!(text_mime.as_deref(), Some("text/plain"));
        assert_eq!(json_mime.as_deref(), Some("application/json"));
        (compact.clone(), json.clone())
    }

    /// Empty first workspace page under one accepted configuration revision.
    fn workspace_page() -> WorkspaceResourcePage {
        WorkspaceResourcePage {
            configuration_revision: Digest("3f9a1c2e".to_owned()),
            languages: Vec::new(),
            source: Vec::new(),
            warnings: Vec::new(),
            pagination: Pagination {
                page_index: 0,
                total_pages: 0,
            },
        }
    }

    #[test]
    fn the_whole_set_is_read_at_the_bare_uri() {
        let query = log_query(LOGS_URI, PAGE).expect("the bare URI is published");

        assert_eq!(query.level(), None);
        assert_eq!(query.component(), None);
        assert_eq!(query.limit(), PAGE_RECORDS);
    }

    #[test]
    fn a_level_uri_restricts_the_read() {
        let query = log_query(&format!("{LOGS_LEVEL_PREFIX}WARN"), PAGE)
            .expect("a known level is published");

        assert_eq!(query.level(), Some("warn"));
    }

    #[test]
    fn an_unknown_level_is_refused() {
        let refusal = log_query(&format!("{LOGS_LEVEL_PREFIX}loud"), PAGE)
            .expect_err("an unknown level must be refused");

        assert!(refusal.message.contains("level segment"), "{refusal:?}");
    }

    #[test]
    fn a_component_uri_restricts_the_read() {
        let query = log_query(&format!("{LOGS_COMPONENT_PREFIX}index"), PAGE)
            .expect("a component is published");

        assert_eq!(query.component(), Some("index"));
    }

    #[test]
    fn a_multi_segment_component_is_refused() {
        let refusal = log_query(&format!("{LOGS_COMPONENT_PREFIX}index/deeper"), PAGE)
            .expect_err("a multi-segment component must be refused");

        assert!(refusal.message.contains("one path segment"), "{refusal:?}");
    }

    #[test]
    fn an_unpublished_uri_is_refused_as_not_found() {
        let refusal =
            log_query("rift://missing", PAGE).expect_err("an unpublished URI must be refused");

        assert_eq!(refusal.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
        assert!(refusal.message.contains("rift://missing"), "{refusal:?}");
    }

    #[test]
    fn a_page_past_the_maximum_is_bounded() {
        let query = log_query(LOGS_URI, u64::MAX).expect("the bare URI is published");

        assert_eq!(query.limit(), LOG_PAGE_RECORDS_MAX);
    }

    /// Two stored records, the older one with object fields and the newer one with text.
    async fn stored_records() -> Vec<StoredLogRecord> {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database = rift_index::WorkspaceDatabase::open(
            &directory.path().join("db"),
            rift_index::DatabasePool::new(2, 1_000),
        )
        .await
        .expect("the database opens");
        let store = LogStore::attached(database);
        store
            .append(
                &[
                    LogRecord::new(
                        7,
                        "WARN",
                        "rift_mcp::server",
                        "index",
                        "index.reconcile",
                        "the capture disagreed",
                        "{\"epoch\":\"4\"}",
                    ),
                    LogRecord::new(
                        1_791_110_527_120,
                        "INFO",
                        "rift_mcp::server",
                        "",
                        "",
                        "",
                        "not an object",
                    ),
                ],
                100,
            )
            .await
            .expect("the records land");
        store
            .recent(&rift_index::LogQuery::newest(10))
            .await
            .expect("the read answers")
    }

    #[tokio::test]
    async fn a_rendered_answer_carries_the_records_as_json() {
        let records = stored_records().await;

        let rendered = rendered_logs(LOGS_URI, &records).expect("the text renders");

        let (_, json_body) = contents(&rendered, LOGS_URI);
        let body: Value = serde_json::from_str(&json_body).expect("the body is JSON");
        assert_eq!(body["record_count"], 2);
        assert_eq!(body["records"][1]["level"], "warn");
        assert_eq!(body["records"][1]["component"], "index");
        assert_eq!(body["records"][1]["fields"]["epoch"], "4");
        assert_eq!(body["records"][1]["message"], "the capture disagreed");
        assert_eq!(body["records"][0]["fields"], "not an object");
    }

    #[tokio::test]
    async fn the_logs_json_is_the_body_the_read_returned_before_the_text_was_added() {
        let records = stored_records().await;

        let rendered = rendered_logs(LOGS_URI, &records).expect("the text renders");

        let expected = json!({
            "uri": LOGS_URI,
            "records": [
                {
                    "identity": 2,
                    "recorded_at_ms": 1_791_110_527_120_i64,
                    "level": "info",
                    "target": "rift_mcp::server",
                    "component": "",
                    "operation": "",
                    "message": "",
                    "fields": "not an object",
                },
                {
                    "identity": 1,
                    "recorded_at_ms": 7,
                    "level": "warn",
                    "target": "rift_mcp::server",
                    "component": "index",
                    "operation": "index.reconcile",
                    "message": "the capture disagreed",
                    "fields": {"epoch": "4"},
                },
            ],
            "record_count": 2,
        });
        assert_eq!(contents(&rendered, LOGS_URI).1, expected.to_string());
    }

    #[tokio::test]
    async fn the_logs_text_states_the_records_the_json_carries() {
        let records = stored_records().await;

        let rendered = rendered_logs(LOGS_URI, &records).expect("the text renders");

        assert_eq!(
            contents(&rendered, LOGS_URI).0,
            "2 records\n\
             \x20\x202026-10-04 10:42:07.120 info - - · not an object\n\
             \x20\x201970-01-01 00:00:00.007 warn index index.reconcile: the capture disagreed · epoch 4\n"
        );
    }

    #[tokio::test]
    async fn a_level_read_echoes_its_uri_in_both_contents() {
        let records = stored_records().await;
        let uri = format!("{LOGS_LEVEL_PREFIX}warn");

        let rendered = rendered_logs(&uri, &records).expect("the text renders");

        let (compact, json_body) = contents(&rendered, &uri);
        assert!(compact.starts_with("2 records\n"), "{compact}");
        assert!(json_body.contains("rift://logs/level/warn"), "{json_body}");
    }

    #[test]
    fn an_unavailable_store_answers_with_its_reason() {
        let rendered =
            logs_unavailable(LOGS_URI, "the log store failed to open").expect("the text renders");

        let (compact, json_body) = contents(&rendered, LOGS_URI);
        let body: Value = serde_json::from_str(&json_body).expect("the body is JSON");
        assert_eq!(body["record_count"], 0);
        assert_eq!(body["unavailable"], "the log store failed to open");
        assert_eq!(
            json_body,
            json!({
                "uri": LOGS_URI,
                "records": [],
                "record_count": 0,
                "unavailable": "the log store failed to open",
            })
            .to_string()
        );
        assert_eq!(
            compact,
            "0 records\n1 warning\n\tunavailable: the log store failed to open\n"
        );
    }

    #[test]
    fn an_empty_record_set_answers_the_header_and_the_empty_body() {
        let rendered = rendered_logs(LOGS_URI, &[]).expect("the text renders");

        let (compact, json_body) = contents(&rendered, LOGS_URI);
        assert_eq!(compact, "0 records\n");
        assert_eq!(
            json_body,
            json!({"uri": LOGS_URI, "records": [], "record_count": 0}).to_string()
        );
    }

    #[test]
    fn workspace_bare_uri_selects_the_first_page() {
        let page = workspace_page_index(WORKSPACE_URI).expect("bare workspace URI is published");
        assert_eq!(page, 0);
        assert!(is_workspace_uri(WORKSPACE_URI));
    }

    #[test]
    fn workspace_query_selects_its_zero_based_page() {
        let uri = "rift://workspace?page_index=17";

        let page = workspace_page_index(uri).expect("workspace page URI is published");
        assert_eq!(page, 17);
        assert!(is_workspace_uri(uri));
    }

    #[test]
    fn workspace_query_refuses_missing_or_invalid_page_index() {
        for uri in [
            "rift://workspace?",
            "rift://workspace?page=1",
            "rift://workspace?page_index=",
            "rift://workspace?page_index=one",
            "rift://workspace?page_index=1&extra=2",
            "rift://workspace?page_index=18446744073709551616",
        ] {
            let refusal = workspace_page_index(uri).expect_err("invalid page must be refused");
            assert_eq!(refusal.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(refusal.message.contains("page_index"), "{refusal:?}");
        }
    }

    #[test]
    fn workspace_path_is_not_a_workspace_resource_uri() {
        let uri = "rift://workspace/1";
        let refusal = workspace_page_index(uri).expect_err("workspace paths are not published");

        assert!(!is_workspace_uri(uri));
        assert_eq!(refusal.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn rendered_workspace_answer_carries_compact_text_then_typed_json() {
        let page = workspace_page();
        let rendered = rendered_workspace(WORKSPACE_URI, &page).expect("the text renders");

        let (compact, json_body) = contents(&rendered, WORKSPACE_URI);
        let body: Value = serde_json::from_str(&json_body).expect("the body is JSON");
        assert_eq!(
            json_body,
            serde_json::to_string(&page).expect("page serializes")
        );
        assert_eq!(body["configuration_revision"], "3f9a1c2e");
        assert_eq!(body["languages"], json!([]));
        assert_eq!(body["source"], json!([]));
        assert_eq!(compact, resource_text(&page).expect("page renders"));
        assert_eq!(compact, "workspace 3f9a1c2e\n");
    }

    #[test]
    fn a_workspace_page_read_names_its_uri_in_both_contents() {
        let uri = "rift://workspace?page_index=2";
        let mut page = workspace_page();
        page.pagination = Pagination {
            page_index: 2,
            total_pages: 4,
        };

        let rendered = rendered_workspace(uri, &page).expect("the text renders");

        assert_eq!(
            contents(&rendered, uri).0,
            "workspace 3f9a1c2e · page 3/4\n"
        );
    }

    /// Empty map under one revision, computed by nothing this test builds.
    fn empty_map() -> WorkspaceMap {
        WorkspaceMap {
            revision: Digest("3f9a1c2e".to_owned()),
            languages: Vec::new(),
            modules: Vec::new(),
            hubs: Vec::new(),
            entry_points: Vec::new(),
            docs: Vec::new(),
            module_relationships: Vec::new(),
            packages: Vec::new(),
            warnings: Vec::new(),
            pagination: Pagination {
                page_index: 0,
                total_pages: 1,
            },
        }
    }

    #[test]
    fn rendered_map_answer_carries_compact_text_then_typed_json() {
        let map = empty_map();
        let rendered = rendered_map(MAP_URI, &map).expect("the text renders");

        let (compact, json_body) = contents(&rendered, MAP_URI);
        let body: Value = serde_json::from_str(&json_body).expect("the body is JSON");
        assert_eq!(
            json_body,
            serde_json::to_string(&map).expect("map serializes")
        );
        assert_eq!(body["revision"], "3f9a1c2e");
        assert!(
            body.get("languages").is_none(),
            "empty collections are omitted"
        );
        assert_eq!(compact, resource_text(&map).expect("map renders"));
        assert_eq!(compact, "map 3f9a1c2e\n");
    }

    #[test]
    fn the_published_surface_names_the_log_family() {
        let resources = declared_resources();
        let templates = declared_templates();

        assert_eq!(resources.len(), 3);
        assert_eq!(resources[0].uri, LOGS_URI);
        assert_eq!(resources[1].uri, WORKSPACE_URI);
        assert!(resources[1].description.is_some());
        assert_eq!(resources[2].uri, MAP_URI);
        assert!(resources[2].description.is_some());
        assert_eq!(templates.len(), 3);
        assert!(templates[..2].iter().all(|template| {
            template.uri_template.starts_with("rift://logs/")
                && template.description.is_some()
                && template.mime_type.is_some()
        }));
        assert_eq!(templates[2].uri_template, WORKSPACE_TEMPLATE);
        assert!(templates[2].description.is_some());
        assert!(templates[2].mime_type.is_some());
    }
}
