//! Which representations of a tool answer a proxy forwards.

use rmcp::model::{CallToolResult, ListToolsResult, ReadResourceResult, ResourceContents};

/// The media type of the JSON content of a resource read.
const JSON_MEDIA_TYPE: &str = "application/json";

/// The representations of answers that one proxy forwards.
///
/// A proxy holds one policy for its whole lifetime, so the surface it
/// advertises stays stable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputPolicy {
    /// Forwards `content` and `structuredContent`, lists `outputSchema`, and forwards every
    /// content of a resource read.
    #[default]
    All,
    /// Forwards `content` alone, lists no `outputSchema`, and forwards the contents of a
    /// resource read that are not JSON.
    Text,
}

impl OutputPolicy {
    /// One page of the tool listing, as this policy advertises it.
    ///
    /// `Text` removes `output_schema` from every tool; all other fields of
    /// the page and of its tools stay as received.
    #[must_use]
    pub fn select_listing(self, mut listing: ListToolsResult) -> ListToolsResult {
        match self {
            Self::All => {}
            Self::Text => {
                for tool in &mut listing.tools {
                    tool.output_schema = None;
                }
            }
        }
        listing
    }

    /// One completed tool result, as this policy forwards it.
    ///
    /// `Text` removes `structured_content`; `content`, `is_error`, and the
    /// other fields stay as received.
    #[must_use]
    pub fn select_result(self, mut result: CallToolResult) -> CallToolResult {
        match self {
            Self::All => {}
            Self::Text => result.structured_content = None,
        }
        result
    }

    /// One completed resource read, as this policy forwards it.
    ///
    /// `Text` removes the contents whose `mimeType` is `application/json`, but only when another
    /// content remains, so a resource that holds JSON alone stays readable. The other fields
    /// stay as received.
    #[must_use]
    pub fn select_resource(self, mut result: ReadResourceResult) -> ReadResourceResult {
        match self {
            Self::All => {}
            Self::Text => {
                let holds_text = result.contents.iter().any(|content| !is_json(content));
                if holds_text {
                    result.contents.retain(|content| !is_json(content));
                }
            }
        }
        result
    }
}

/// True when `content` declares the JSON media type.
fn is_json(content: &ResourceContents) -> bool {
    let (ResourceContents::TextResourceContents { mime_type, .. }
    | ResourceContents::BlobResourceContents { mime_type, .. }) = content
    else {
        return false;
    };
    mime_type
        .as_deref()
        .is_some_and(|mime_type| mime_type.eq_ignore_ascii_case(JSON_MEDIA_TYPE))
}

#[cfg(test)]
mod tests {
    use rmcp::model::{
        CallToolResult, ContentBlock, Icon, JsonObject, ListToolsResult, MetaObject,
        ReadResourceResult, ResourceContents, Tool, ToolAnnotations,
    };
    use serde_json::json;

    use super::OutputPolicy;

    fn object(value: serde_json::Value) -> JsonObject {
        serde_json::from_value(value).expect("test schema must be a JSON object")
    }

    fn meta(key: &str) -> MetaObject {
        let mut meta = MetaObject::new();
        meta.insert(key.to_owned(), json!(true));
        meta
    }

    /// A tool with every field but `output_schema`.
    fn bare_tool(name: &'static str) -> Tool {
        Tool::new(
            name,
            "describes the tool",
            object(json!({"type": "object"})),
        )
        .with_title("Title")
        .with_annotations(ToolAnnotations::with_title("Annotated"))
        .with_icons(vec![Icon::new("https://example.test/icon.png")])
        .with_meta(meta("tool"))
    }

    fn described_tool(name: &'static str) -> Tool {
        bare_tool(name).with_raw_output_schema(
            object(json!({"type": "object", "properties": {"hits": {"type": "integer"}}})).into(),
        )
    }

    fn listing(tools: Vec<Tool>) -> ListToolsResult {
        let mut listing = ListToolsResult::with_all_items(tools);
        listing.next_cursor = Some("page-2".to_owned());
        listing.meta = Some(meta("page"));
        listing
    }

    #[test]
    fn default_policy_is_all() {
        assert_eq!(OutputPolicy::default(), OutputPolicy::All);
    }

    #[test]
    fn all_leaves_a_listing_unchanged() {
        let page = listing(vec![described_tool("search"), described_tool("nodes")]);
        assert_eq!(OutputPolicy::All.select_listing(page.clone()), page);
    }

    #[test]
    fn text_removes_output_schema_from_every_tool_and_keeps_the_rest() {
        let page = listing(vec![
            described_tool("search"),
            described_tool("nodes"),
            bare_tool("get_symbol"),
        ]);
        let expected = listing(vec![
            bare_tool("search"),
            bare_tool("nodes"),
            bare_tool("get_symbol"),
        ]);
        let selected = OutputPolicy::Text.select_listing(page);
        assert_eq!(selected, expected);
        assert!(
            selected
                .tools
                .iter()
                .all(|tool| tool.output_schema.is_none())
        );
        assert_eq!(selected.next_cursor.as_deref(), Some("page-2"));
        assert_eq!(selected.meta, Some(meta("page")));
    }

    #[test]
    fn text_leaves_an_empty_listing_unchanged() {
        let page = ListToolsResult::with_all_items(Vec::new());
        assert_eq!(OutputPolicy::Text.select_listing(page.clone()), page);
    }

    #[test]
    fn all_leaves_a_success_result_unchanged() {
        let result = CallToolResult::structured(json!({"results": []}));
        assert_eq!(OutputPolicy::All.select_result(result.clone()), result);
    }

    #[test]
    fn text_removes_structured_content_from_a_success_result() {
        let result = CallToolResult::structured(json!({"results": []})).with_meta(Some(meta("r")));
        let selected = OutputPolicy::Text.select_result(result.clone());
        assert_eq!(selected.structured_content, None);
        assert_eq!(selected.content, result.content);
        assert_eq!(selected.is_error, Some(false));
        assert_eq!(selected.result_type, result.result_type);
        assert_eq!(selected.meta, Some(meta("r")));
    }

    #[test]
    fn text_keeps_is_error_on_an_error_result_with_structured_content() {
        let result = CallToolResult::structured_error(json!({"code": "invalid_request"}));
        assert_eq!(OutputPolicy::All.select_result(result.clone()), result);
        let selected = OutputPolicy::Text.select_result(result.clone());
        assert_eq!(selected.is_error, Some(true));
        assert_eq!(selected.structured_content, None);
        assert_eq!(selected.content, result.content);
    }

    #[test]
    fn text_leaves_a_result_without_structured_content_unchanged() {
        let result = CallToolResult::success(vec![ContentBlock::text("plain")]);
        assert_eq!(OutputPolicy::Text.select_result(result.clone()), result);
    }

    fn content(text: &str, mime_type: &str) -> ResourceContents {
        ResourceContents::text(text, "rift://map").with_mime_type(mime_type)
    }

    /// A read with the compact text first and the JSON body second.
    fn two_content_read() -> ReadResourceResult {
        ReadResourceResult::new(vec![
            content("map 3f9a1c2e", "text/plain"),
            content("{}", "application/json"),
        ])
        .with_ttl_ms(5)
    }

    #[test]
    fn all_leaves_a_resource_read_unchanged() {
        let result = two_content_read();
        assert_eq!(OutputPolicy::All.select_resource(result.clone()), result);
    }

    #[test]
    fn text_keeps_only_the_text_content_of_a_resource_read() {
        let result = two_content_read();
        let selected = OutputPolicy::Text.select_resource(result.clone());
        assert_eq!(
            selected.contents,
            vec![content("map 3f9a1c2e", "text/plain")]
        );
        assert_eq!(selected.ttl_ms, result.ttl_ms);
        assert_eq!(selected.result_type, result.result_type);
    }

    #[test]
    fn text_leaves_a_json_only_resource_read_unchanged() {
        let result = ReadResourceResult::new(vec![content("{}", "application/json")]);
        assert_eq!(OutputPolicy::Text.select_resource(result.clone()), result);
    }

    #[test]
    fn text_leaves_an_empty_resource_read_unchanged() {
        let result = ReadResourceResult::new(Vec::new());
        assert_eq!(OutputPolicy::Text.select_resource(result.clone()), result);
    }

    #[test]
    fn text_keeps_a_content_without_a_media_type_and_a_blob() {
        let blob = ResourceContents::blob("AAAA", "rift://map");
        let plain = ResourceContents::TextResourceContents {
            uri: "rift://map".to_owned(),
            mime_type: None,
            text: "t".to_owned(),
            meta: None,
        };
        let result = ReadResourceResult::new(vec![
            content("{}", "Application/JSON"),
            blob.clone(),
            plain.clone(),
        ]);
        let selected = OutputPolicy::Text.select_resource(result);
        assert_eq!(selected.contents, vec![blob, plain]);
    }
}
