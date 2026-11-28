//! Deterministic export of the served MCP tool surface.
//!
//! [`schema_document`] serializes the same tool router [`RiftMcp`] runs, so
//! the exported document and the served surface come from one definition:
//! tool names and descriptions from the `#[tool]` methods, request and
//! response schemas derived from the `rift_protocol` wire models.
//! Export orchestration lives in `rift-schema-export`; this module owns only
//! the schema for the MCP surface served by [`RiftMcp`].

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use serde_json::json;

use crate::RiftMcp;

/// One-line summary rendered at the top of the exported document.
const DOCUMENT_DESCRIPTION: &str =
    "Tools served by the Rift MCP server, with the JSON Schemas derived from the Rust wire models.";

impl RiftMcp {
    /// The tool router the server serves and [`schema_document`] exports: the routes the
    /// `#[tool]` methods declare, with no optional property advertising a `null` arm.
    ///
    /// rmcp derives every tool schema through its own `SchemaSettings::draft2020_12()`
    /// generator, which takes no transform, and schemars gives each `Option<T>` field a
    /// `null` arm. The wire models omit an absent optional field, so every served schema
    /// passes [`rift_protocol::schema::strip_optional_null_arms`] here, the one path from
    /// the declared routes to a caller.
    pub(crate) fn tool_router() -> ToolRouter<Self> {
        let mut router = Self::declared_tool_router();
        for route in router.map.values_mut() {
            let tool = &mut route.attr;
            rift_protocol::schema::strip_optional_null_arms(Arc::make_mut(&mut tool.input_schema));
            if let Some(output) = tool.output_schema.as_mut() {
                let output = Arc::make_mut(output);
                rift_protocol::schema::strip_optional_null_arms(output);
                rift_protocol::schema::strip_response_warnings(output);
                if matches!(tool.name.as_ref(), "get_symbol" | "search") {
                    // These result unions contain only objects. Keep their union constraints
                    // while declaring the root type required by earlier MCP clients.
                    output.insert("type".to_owned(), json!("object"));
                }
            }
        }
        router
    }
}

/// The symbol the examples of a server reading the global scope by default name: a trait of a
/// crates.io release the global index holds.
const GLOBAL_EXAMPLE_SYMBOL: &str =
    "rift://symbol/cargo/crates.io/serde_core/rust/serde_core/ser/Serialize";

/// The release of [`GLOBAL_EXAMPLE_SYMBOL`]'s package the pinned examples read.
const GLOBAL_EXAMPLE_REVISION: &str = "rift://revision/cargo/crates.io/serde_core@1.0.229";

/// The search example of a server reading the global scope by default. Its `SERVICE :search`
/// names no `:scope`, so the workspace's default scope selects the package it names.
const GLOBAL_EXAMPLE_SEARCH: &str = concat!(
    "SELECT ?symbol ?name { SERVICE :search { ?hit :query \"Serialize\"; :target \"symbol\"; ",
    ":packages '[{\"manager\":\"cargo\",\"registry\":\"crates.io\",\"name\":\"serde_core\"}]'^^rdf:JSON; ",
    ":symbol ?symbol; :index ?index . ?symbol :name ?name } } ORDER BY ?index LIMIT 5"
);

/// Replaces the examples of `search`, `get_symbol`, and `facets` with ones that read a package
/// release, for a server whose `[search] default_scope` is `global`.
///
/// The declared examples name a project's own symbols and files, which a server holding no
/// project cannot answer. Every replacement reads a release the global index holds, so each
/// example a caller lists is one the server answers. Other tools keep their examples.
pub(crate) fn serve_global_examples(tools: &mut [rmcp::model::Tool]) {
    for tool in tools {
        let examples = match tool.name.as_ref() {
            "search" => json!([{ "query": GLOBAL_EXAMPLE_SEARCH }]),
            "get_symbol" => json!([
                { "id": GLOBAL_EXAMPLE_SYMBOL },
                { "id": GLOBAL_EXAMPLE_SYMBOL, "revision": GLOBAL_EXAMPLE_REVISION },
                { "id": GLOBAL_EXAMPLE_SYMBOL, "include": ["history"] },
            ]),
            "facets" => json!([
                { "symbol": GLOBAL_EXAMPLE_SYMBOL },
                { "symbol": GLOBAL_EXAMPLE_SYMBOL, "revision": GLOBAL_EXAMPLE_REVISION },
            ]),
            _ => continue,
        };
        Arc::make_mut(&mut tool.input_schema).insert("examples".to_owned(), examples);
    }
}

/// Renders the document the docs site publishes: one entry per served tool
/// with its name, description, input schema, and output schema, sorted by
/// name.
///
/// Output is pretty-printed, ends with a trailing newline, and is
/// byte-identical across calls because `serde_json` stores objects as sorted
/// maps.
#[must_use]
pub fn schema_document() -> String {
    let mut tools = RiftMcp::tool_router().list_all();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools: Vec<_> = tools
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
                "output_schema": tool.output_schema,
            })
        })
        .collect();
    let document = json!({
        "description": DOCUMENT_DESCRIPTION,
        "tools": tools,
    });
    rift_protocol::schema::render_json_document(document)
}

/// Returns the served tool listing `RiftMcp` runs, sorted by name.
///
/// `RiftMcp::tool_router` is `pub(crate)`, so this is the one path a
/// consumer outside this crate has to the same [`rmcp::model::Tool`] values
/// [`schema_document`] serializes. `rift install claude` reads each tool's
/// name, description, and JSON Schema straight from these typed values to
/// generate its Claude Code skill, rather than round-tripping the exported
/// document back through JSON.
#[must_use]
pub fn tool_listing() -> Vec<rmcp::model::Tool> {
    RiftMcp::tool_router().list_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each example a server reading the global scope by default lists is a request its tool
    /// accepts, and the search example's service selects the package it names under the
    /// default scope.
    #[test]
    fn global_examples_are_accepted_requests_naming_a_package_release() {
        let declared = tool_listing();
        let mut served = declared.clone();
        serve_global_examples(&mut served);
        for (declared, served) in declared.iter().zip(&served) {
            let examples = served.input_schema["examples"]
                .as_array()
                .expect("every tool lists examples");
            if !matches!(served.name.as_ref(), "search" | "get_symbol" | "facets") {
                assert_eq!(served, declared, "{} keeps its examples", served.name);
                continue;
            }
            let schema = serde_json::Value::Object(served.input_schema.as_ref().clone());
            let validator = jsonschema::validator_for(&schema).expect("input schema compiles");
            for example in examples {
                assert!(validator.is_valid(example), "{}: {example}", served.name);
                let text = example.to_string();
                assert!(
                    !text.contains("rift://symbol/local") && !text.contains("\"path\""),
                    "{}: {example}",
                    served.name
                );
            }
        }
        let query = rift_search::sparql::GraphQuery::parse(
            GLOBAL_EXAMPLE_SEARCH,
            &rift_search::sparql::GraphLimits::default(),
        )
        .expect("the search example parses")
        .with_default_scope(&rift_protocol::read::SearchScope::Global);
        let [selection] = query.retrieval_requests() else {
            panic!("the search example selects one producer");
        };
        assert_eq!(
            selection.request.scope,
            rift_protocol::read::SearchScope::Global
        );
        let [package] = selection.request.packages.as_slice() else {
            panic!("the search example names one package");
        };
        assert_eq!(
            (package.manager.as_str(), package.registry.as_deref()),
            ("cargo", Some("crates.io"))
        );
        assert!(GLOBAL_EXAMPLE_SYMBOL.contains(&format!("/{}/", package.name)));
        assert!(GLOBAL_EXAMPLE_REVISION.contains(&format!("/{}@", package.name)));
    }

    #[test]
    fn feedback_advertises_a_non_idempotent_external_write() {
        let tool = tool_listing()
            .into_iter()
            .find(|tool| tool.name == "feedback")
            .expect("feedback is advertised");
        let annotations = tool.annotations.expect("feedback has annotations");
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[test]
    fn tool_output_schemas_exclude_operator_diagnostics() {
        let document: serde_json::Value =
            serde_json::from_str(&schema_document()).expect("exported MCP document");
        for tool in document["tools"].as_array().expect("tool listing") {
            let schema = &tool["output_schema"];
            for definition in [
                "ReadWarning",
                "DocumentationWarning",
                "GlobalPageWarningCode",
            ] {
                assert!(schema["$defs"].get(definition).is_none(), "{definition}");
            }
            let mut pending = vec![schema];
            while let Some(syntax_element) = pending.pop() {
                match syntax_element {
                    serde_json::Value::Object(object) => {
                        assert!(!object.contains_key("warnings"), "{syntax_element}");
                        pending.extend(object.values());
                    }
                    serde_json::Value::Array(items) => pending.extend(items),
                    serde_json::Value::String(value) => assert_ne!(value, "warnings"),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn object_output_unions_keep_their_original_constraints() {
        let declared = RiftMcp::declared_tool_router().list_all();
        let served = tool_listing();
        for name in ["get_symbol", "search"] {
            let original = declared
                .iter()
                .find(|tool| tool.name == name)
                .expect("declared tool");
            let current = served
                .iter()
                .find(|tool| tool.name == name)
                .expect("served tool");
            let mut expected = original
                .output_schema
                .as_deref()
                .expect("declared output")
                .clone();
            rift_protocol::schema::strip_optional_null_arms(&mut expected);
            rift_protocol::schema::strip_response_warnings(&mut expected);
            expected.entry("type").or_insert(json!("object"));
            let actual = current
                .output_schema
                .as_deref()
                .expect("served output")
                .clone();
            assert_eq!(
                actual, expected,
                "{name} keeps every original union constraint"
            );
        }
    }
}
