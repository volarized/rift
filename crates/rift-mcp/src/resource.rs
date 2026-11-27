//! Workspace and map resources published by the server.

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use rift_protocol::map::WorkspaceMap;
use rift_protocol::revision::Revision;
use rift_protocol::workspace::WorkspaceResourcePage;
use rmcp::ErrorData;
use rmcp::model::{ReadResourceResult, Resource, ResourceContents, ResourceTemplate};

use crate::output::{log_warnings, resource_text};

/// The workspace orientation snapshot.
pub(crate) const MAP_URI: &str = "rift://map";
/// The first workspace resource page.
pub(crate) const WORKSPACE_URI: &str = "rift://workspace";
/// The template a client expands to reach one workspace resource page.
pub(crate) const WORKSPACE_TEMPLATE: &str = "rift://workspace{?page,max_results,revision}";
/// The query prefix selecting one workspace resource page.
const WORKSPACE_QUERY_PREFIX: &str = "rift://workspace?";
/// Characters a revision IRI escapes as a workspace query value.
const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'`')
    .add(b'{')
    .add(b'}');
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
                "One page of the workspace resource, selected by 1-based `page` and \
                 `max_results`, at the `revision` an earlier page named.",
            )
            .with_mime_type(RESOURCE_MEDIA_TYPE),
    ]
}

/// Returns whether `uri` addresses the workspace resource family.
#[must_use]
pub(crate) fn is_workspace_uri(uri: &str) -> bool {
    uri == WORKSPACE_URI || uri.starts_with(WORKSPACE_QUERY_PREFIX)
}

/// The page, result count, and pinned revision one workspace resource read requests.
#[derive(Debug, PartialEq)]
pub(crate) struct WorkspacePageRequest {
    pub(crate) page: u64,
    pub(crate) max_results: u64,
    pub(crate) revision: Option<Revision>,
}

/// The URI of one workspace resource page at `revision`.
pub(crate) fn workspace_page_uri(page: u64, max_results: u64, revision: &Revision) -> String {
    format!(
        "{WORKSPACE_QUERY_PREFIX}page={page}&max_results={max_results}&revision={}",
        utf8_percent_encode(&revision.to_string(), QUERY_VALUE)
    )
}

/// The requested page, result count, and revision of one workspace resource read.
pub(crate) fn workspace_page(uri: &str) -> Result<WorkspacePageRequest, ErrorData> {
    let mut page = rift_protocol::read::PAGE_DEFAULT;
    let mut max_results = rift_protocol::read::MAX_RESULTS_DEFAULT;
    let mut revision = None;
    if uri == WORKSPACE_URI {
        return Ok(WorkspacePageRequest {
            page,
            max_results,
            revision,
        });
    }
    let query = uri.strip_prefix(WORKSPACE_QUERY_PREFIX).ok_or_else(|| {
        ErrorData::resource_not_found(format!("no resource is published at {uri:?}"), None)
    })?;
    let mut fields = std::collections::BTreeSet::new();
    for entry in query.split('&') {
        let (field, value) = entry
            .split_once('=')
            .ok_or_else(|| ErrorData::invalid_params("expected page=1&max_results=10", None))?;
        if !fields.insert(field) {
            return Err(ErrorData::invalid_params(
                "page fields must occur once",
                None,
            ));
        }
        if field == "revision" {
            let value = percent_decode_str(value).decode_utf8().map_err(|_| {
                ErrorData::invalid_params("revision must be a UTF-8 revision IRI", None)
            })?;
            revision = Some(
                Revision::parse(&value)
                    .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?,
            );
            continue;
        }
        let value = value.parse::<u64>().map_err(|_| {
            ErrorData::invalid_params("page and max_results must be positive integers", None)
        })?;
        match field {
            "page" => page = value,
            "max_results" => max_results = value,
            _ => {
                return Err(ErrorData::invalid_params(
                    "expected page, max_results, or revision",
                    None,
                ));
            }
        }
    }
    rift_server::accept_page(page)
        .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
    rift_server::accepted_max_results(
        max_results,
        rift_protocol::workspace::WORKSPACE_SOURCE_UNITS_MAX as u64,
    )
    .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
    Ok(WorkspacePageRequest {
        page,
        max_results,
        revision,
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
    log_warnings(&page.warnings);
    let body = serde_json::to_string(page)
        .unwrap_or_else(|error| unreachable!("workspace resource pages serialize: {error}"));
    Ok(answer(uri, resource_text(page)?, body))
}

/// The cached workspace map in text and JSON.
pub(crate) fn map_answer(uri: &str, map: &WorkspaceMap) -> Result<ReadResourceResult, ErrorData> {
    log_warnings(&map.warnings);
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

#[cfg(test)]
mod tests {
    use super::{
        MAP_URI, WORKSPACE_TEMPLATE, WORKSPACE_URI, declared_resources, declared_templates,
        is_workspace_uri, map_answer, rendered_workspace, workspace_page,
    };
    use super::{Revision, WorkspacePageRequest, workspace_page_uri};
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
    fn sample_workspace_page() -> WorkspaceResourcePage {
        WorkspaceResourcePage {
            configuration_revision: Digest("3f9a1c2e".to_owned()),
            revision: Revision::tree(&"a".repeat(64)).expect("tree revision"),
            languages: Vec::new(),
            source: Vec::new(),
            warnings: Vec::new(),
            pagination: Pagination::new(1, 10, 0),
        }
    }

    #[test]
    fn workspace_bare_uri_selects_the_first_page() {
        let page = workspace_page(WORKSPACE_URI).expect("bare workspace URI is published");
        assert_eq!((page.page, page.max_results, page.revision), (1, 10, None));
        assert!(is_workspace_uri(WORKSPACE_URI));
    }

    #[test]
    fn workspace_query_selects_its_zero_based_page() {
        let uri = "rift://workspace?page=17";

        let page = workspace_page(uri).expect("workspace page URI is published");
        assert_eq!((page.page, page.max_results), (17, 10));
        assert!(is_workspace_uri(uri));
    }

    #[test]
    fn workspace_query_refuses_missing_or_invalid_page() {
        for uri in [
            "rift://workspace?",
            "rift://workspace?page=0",
            "rift://workspace?page=",
            "rift://workspace?page=one",
            "rift://workspace?page=1&extra=2",
            "rift://workspace?page=18446744073709551616",
        ] {
            let refusal = workspace_page(uri).expect_err("invalid page must be refused");
            assert_eq!(refusal.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(refusal.message.contains("page"), "{refusal:?}");
        }
    }

    /// #743: a page names the revision later pages pin, escaped or as written.
    #[test]
    fn workspace_query_pins_a_revision() {
        let revision = Revision::parse("rift://revision/cargo/crates.io/demo@1.0.0+build")
            .expect("release revision");
        let uri = workspace_page_uri(2, 5, &revision);
        assert_eq!(
            uri,
            "rift://workspace?page=2&max_results=5&revision=rift://revision/cargo/crates.io/demo@1.0.0%2Bbuild"
        );
        let selected = workspace_page(&uri).expect("pinned page URI is published");
        assert_eq!(
            selected,
            WorkspacePageRequest {
                page: 2,
                max_results: 5,
                revision: Some(revision),
            }
        );
        let tree = format!("rift%3A%2F%2Frevision%2Ftree%2F{}", "b".repeat(64));
        let selected = workspace_page(&format!("rift://workspace?revision={tree}"))
            .expect("an escaped revision is accepted");
        assert_eq!(
            selected.revision,
            Some(Revision::tree(&"b".repeat(64)).expect("tree revision"))
        );
        for uri in [
            "rift://workspace?revision=main",
            "rift://workspace?revision=%FF",
            "rift://workspace?revision=rift://revision/tree/1&revision=rift://revision/tree/1",
        ] {
            let refusal = workspace_page(uri).expect_err("an invalid revision is refused");
            assert_eq!(
                refusal.code,
                rmcp::model::ErrorCode::INVALID_PARAMS,
                "{uri}"
            );
        }
    }

    #[test]
    fn workspace_path_is_not_a_workspace_resource_uri() {
        let uri = "rift://workspace/1";
        let refusal = workspace_page(uri).expect_err("workspace paths are not published");

        assert!(!is_workspace_uri(uri));
        assert_eq!(refusal.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn rendered_workspace_answer_carries_compact_text_then_typed_json() {
        let page = sample_workspace_page();
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
        assert_eq!(compact, "workspace\nNo source files on this page.\n");
    }

    #[test]
    fn a_workspace_page_read_names_its_uri_in_both_contents() {
        let uri = "rift://workspace?page=2";
        let mut page = sample_workspace_page();
        page.pagination = Pagination {
            page: 2,
            pages: 4,
            max_results: 10,
            total: 40,
        };

        let rendered = rendered_workspace(uri, &page).expect("the text renders");

        assert_eq!(
            contents(&rendered, uri).0,
            "workspace | page 2/4, total 40 results\nnext | rift://workspace?page=3&max_results=10&revision=rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
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
            module_edges: Vec::new(),
            packages: Vec::new(),
            recommendations: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn map_answer_carries_text_and_typed_json() {
        let map = empty_map();
        let answer = map_answer(MAP_URI, &map).expect("the map renders");
        let (compact, body) = contents(&answer, MAP_URI);
        assert_eq!(
            compact,
            "workspace map\nNo indexed files.\nsource catalog | rift://workspace\n"
        );
        assert_eq!(body, serde_json::to_string(&map).expect("map serializes"));
    }

    #[test]
    fn map_text_lists_the_dependency_context() {
        use rift_protocol::dependencies::{
            PackageAvailability, PackageContextEntry, PackageSelector,
        };
        let mut map = empty_map();
        map.packages = vec![
            PackageContextEntry::new(
                "cargo",
                "tokio",
                PackageSelector::Requirement("^1.53.1".to_owned()),
                PackageAvailability::Canonical,
            ),
            PackageContextEntry::new(
                "cargo",
                "local",
                PackageSelector::Requirement("0.4".to_owned()),
                PackageAvailability::Path,
            ),
            PackageContextEntry::new(
                "stdlib",
                "rust",
                PackageSelector::Version("1.98.1".to_owned()),
                PackageAvailability::Canonical,
            ),
        ];
        let answer = map_answer(MAP_URI, &map).expect("the map renders");
        let (compact, _) = contents(&answer, MAP_URI);
        assert_eq!(
            compact,
            "workspace map\npackages:\n\ttokio ^1.53.1 (cargo)\n\tlocal 0.4 (cargo | path)\n\
             \trust@1.98.1 (stdlib)\nNo indexed files.\nsource catalog | rift://workspace\n"
        );
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
