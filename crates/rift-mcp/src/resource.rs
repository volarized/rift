//! The resources the server publishes, and how a resource URI is read.
//!
//! One family lives here: `rift://logs`, the server's own recorded diagnostics.
//! A tool answers a question about the workspace; this answers a question about
//! the server that was supposed to answer it. The two never share a path,
//! because the case that needs the logs most is the one where the workspace
//! reads refuse.

use rift_index::{LOG_PAGE_RECORDS_MAX, LogQuery, StoredLogRecord};
use rmcp::ErrorData;
use rmcp::model::{ReadResourceResult, Resource, ResourceContents, ResourceTemplate};
use serde_json::{Map, Value, json};

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
/// The media type every log read answers in.
const LOGS_MEDIA_TYPE: &str = "application/json";
/// The levels a level-restricted read accepts, in the spelling the store holds.
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

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
            .with_mime_type(LOGS_MEDIA_TYPE),
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
            .with_mime_type(LOGS_MEDIA_TYPE),
        ResourceTemplate::new(LOGS_COMPONENT_TEMPLATE, "logs-for-component")
            .with_title("Server logs from one component")
            .with_description(
                "The recorded diagnostics one component emitted, as its spans label them: \
                 index, search, engine, change, or logs.",
            )
            .with_mime_type(LOGS_MEDIA_TYPE),
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
            return Err(ErrorData::invalid_params(
                format!(
                    "the level segment must be one of {}, not {level:?}",
                    LOG_LEVELS.join(", ")
                ),
                None,
            ));
        }
        return Ok(LogQuery::newest(page).at_level(&level));
    }
    if let Some(component) = uri.strip_prefix(LOGS_COMPONENT_PREFIX) {
        if component.is_empty() || component.contains('/') {
            return Err(ErrorData::invalid_params(
                "the component segment must be one path segment and cannot be empty",
                None,
            ));
        }
        return Ok(LogQuery::newest(page).for_component(component));
    }
    Err(ErrorData::resource_not_found(
        format!("no resource is published at {uri:?}"),
        None,
    ))
}

/// The answer one log read returns: the records it selected, newest first.
pub(crate) fn rendered_logs(uri: &str, records: &[StoredLogRecord]) -> ReadResourceResult {
    let body = json!({
        "uri": uri,
        "records": records.iter().map(rendered_record).collect::<Vec<Value>>(),
        "record_count": records.len(),
    });
    ReadResourceResult::new(vec![
        ResourceContents::text(body.to_string(), uri).with_mime_type(LOGS_MEDIA_TYPE),
    ])
}

/// The answer a read earns when the store never opened: an empty set, and the
/// reason it is empty. A refusal here would leave the caller unable to tell an
/// unrecorded run from a quiet one.
pub(crate) fn logs_unavailable(uri: &str, reason: &str) -> ReadResourceResult {
    let body = json!({
        "uri": uri,
        "records": Vec::<Value>::new(),
        "record_count": 0,
        "unavailable": reason,
    });
    ReadResourceResult::new(vec![
        ResourceContents::text(body.to_string(), uri).with_mime_type(LOGS_MEDIA_TYPE),
    ])
}

/// One stored record as the wire carries it. `fields` is embedded as the object
/// it was rendered from when it parses, and as text when it does not, so a
/// reader never has to unquote JSON out of a string.
fn rendered_record(stored: &StoredLogRecord) -> Value {
    let record = stored.record();
    let fields = serde_json::from_str::<Map<String, Value>>(record.fields())
        .map_or_else(|_| json!(record.fields()), Value::Object);
    json!({
        "identity": stored.identity(),
        "recorded_at_ms": record.recorded_at_ms(),
        "level": record.level(),
        "target": record.target(),
        "component": record.component(),
        "operation": record.operation(),
        "message": record.message(),
        "fields": fields,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        LOGS_COMPONENT_PREFIX, LOGS_LEVEL_PREFIX, LOGS_URI, declared_resources, declared_templates,
        log_query, logs_unavailable, rendered_logs,
    };
    use rift_index::{LOG_PAGE_RECORDS_MAX, LogRecord, LogStore, StoredLogRecord};
    use rmcp::model::ResourceContents;
    use serde_json::Value;

    const PAGE: u64 = 100;
    /// The same page as the read bound it becomes.
    const PAGE_RECORDS: usize = 100;

    /// The text one rendered answer carries.
    fn text(result: &rmcp::model::ReadResourceResult) -> String {
        match result.contents.first() {
            Some(ResourceContents::TextResourceContents { text, .. }) => text.clone(),
            other => unreachable!("a log read answers with text, not {other:?}"),
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
            log_query("rift://workspace", PAGE).expect_err("an unpublished URI must be refused");

        assert_eq!(refusal.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn a_page_past_the_maximum_is_bounded() {
        let query = log_query(LOGS_URI, u64::MAX).expect("the bare URI is published");

        assert_eq!(query.limit(), LOG_PAGE_RECORDS_MAX);
    }

    #[tokio::test]
    async fn a_rendered_answer_carries_the_records_as_json() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database = rift_index::WorkspaceDatabase::open(
            &directory.path().join("db"),
            rift_index::DatabasePool::new(2, 1_000),
        )
        .await
        .expect("the database opens");
        let store = LogStore::attached(database)
            .await
            .expect("the store attaches");
        store
            .append(
                &[LogRecord::new(
                    7,
                    "WARN",
                    "rift_mcp::server",
                    "index",
                    "index.reconcile",
                    "the capture disagreed",
                    "{\"epoch\":\"4\"}",
                )],
                100,
            )
            .await
            .expect("the record lands");
        let records: Vec<StoredLogRecord> = store
            .recent(&rift_index::LogQuery::newest(10))
            .await
            .expect("the read answers");

        let rendered = rendered_logs(LOGS_URI, &records);

        let body: Value = serde_json::from_str(&text(&rendered)).expect("the body is JSON");
        assert_eq!(body["record_count"], 1);
        assert_eq!(body["records"][0]["level"], "warn");
        assert_eq!(body["records"][0]["component"], "index");
        assert_eq!(body["records"][0]["fields"]["epoch"], "4");
        assert_eq!(body["records"][0]["message"], "the capture disagreed");
    }

    #[test]
    fn an_unavailable_store_answers_with_its_reason() {
        let rendered = logs_unavailable(LOGS_URI, "the log store failed to open");

        let body: Value = serde_json::from_str(&text(&rendered)).expect("the body is JSON");
        assert_eq!(body["record_count"], 0);
        assert_eq!(body["unavailable"], "the log store failed to open");
    }

    #[test]
    fn the_published_surface_names_the_log_family() {
        let resources = declared_resources();
        let templates = declared_templates();

        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, LOGS_URI);
        assert_eq!(templates.len(), 2);
        assert!(templates.iter().all(|template| {
            template.uri_template.starts_with("rift://logs/")
                && template.description.is_some()
                && template.mime_type.is_some()
        }));
    }
}
