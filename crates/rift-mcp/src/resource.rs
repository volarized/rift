//! Workspace and map resources published by the server.

use rift_protocol::map::WorkspaceMap;
use rift_protocol::workspace::WorkspaceResourcePage;
use rmcp::ErrorData;
use rmcp::model::{ReadResourceResult, Resource, ResourceContents, ResourceTemplate};

use crate::output::resource_text;

use crate::failure::McpErrorFailExt as _;

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
        ResourceTemplate::new(WORKSPACE_TEMPLATE, "workspace-page")
            .with_title("Workspace page")
            .with_description(
                "One page of the workspace resource, selected by zero-based `page_index`.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
    ]
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

/// The cached workspace map as one JSON resource content.
pub(crate) fn map_answer(uri: &str, map: &WorkspaceMap) -> ReadResourceResult {
    let body = serde_json::to_string(map)
        .unwrap_or_else(|error| unreachable!("the workspace map serializes: {error}"));
    ReadResourceResult::new(vec![
        ResourceContents::text(body, uri).with_mime_type(RESOURCE_MEDIA_TYPE),
    ])
}

/// One read answer: the compact text first, then the JSON body, both for `uri`.
fn answer(uri: &str, text: String, json: String) -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type(TEXT_MEDIA_TYPE),
        ResourceContents::text(json, uri).with_mime_type(RESOURCE_MEDIA_TYPE),
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        MAP_URI, WORKSPACE_TEMPLATE, WORKSPACE_URI, declared_resources, declared_templates,
        is_workspace_uri, map_answer, rendered_workspace, workspace_page_index,
    };
    use crate::output::resource_text;
    use rift_protocol::map::WorkspaceMap;
    use rift_protocol::read::{Digest, Pagination};
    use rift_protocol::workspace::WorkspaceResourcePage;
    use rmcp::model::{ReadResourceResult, ResourceContents};
    use serde_json::{Value, json};

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
    fn map_answer_carries_typed_json_alone() {
        let map = empty_map();
        let answer = map_answer(MAP_URI, &map);
        let [
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            },
        ] = &answer.contents[..]
        else {
            panic!("map returns one JSON content: {answer:?}");
        };
        assert_eq!(uri, MAP_URI);
        assert_eq!(mime_type.as_deref(), Some("application/json"));
        assert_eq!(text, &serde_json::to_string(&map).expect("map serializes"));
    }

    #[test]
    fn the_published_surface_names_workspace_and_map() {
        let resources = declared_resources();
        let templates = declared_templates();
        assert_eq!(
            resources
                .iter()
                .map(|resource| resource.uri.as_str())
                .collect::<Vec<_>>(),
            vec![WORKSPACE_URI, MAP_URI]
        );
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].uri_template, WORKSPACE_TEMPLATE);
        assert!(templates[0].description.is_some());
        assert!(templates[0].mime_type.is_some());
    }
}
