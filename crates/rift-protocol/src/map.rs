//! Wire model for the `rift://map` resource: a workspace orientation snapshot computed once
//! per index publication.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{Digest, ExactKind, Language, Pagination, ProjectPath, SymbolId};
use crate::schema;

/// Directory depth a [`MapModule`] tree carries, at most. A directory deeper than this folds
/// into its depth-3 ancestor's counts and lists no [`MapModule::children`] of its own.
pub const MAP_MODULE_DEPTH_MAX: usize = 3;
/// [`WorkspaceMap::hubs`] entries one map carries, at most.
pub const MAP_HUBS_MAX: usize = 20;
/// [`WorkspaceMap::entry_points`] entries one map carries, at most.
pub const MAP_ENTRY_POINTS_MAX: usize = 50;
/// [`WorkspaceMap::docs`] entries one map carries, at most.
pub const MAP_DOCS_MAX: usize = 100;

/// Workspace orientation snapshot served by `rift://map`: per-language totals, the directory
/// tree indexed files sit under, the most-referenced symbols, where execution starts, and
/// where documentation lives. Computed once when the index publishes and served from cache
/// until the next publication.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_workspace_map_empty_defaults)]
pub struct WorkspaceMap {
    /// Digest of the indexed tree this map was computed from.
    pub revision: Digest,
    /// Per-language file and symbol counts, sorted by language spelling. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128))]
    pub languages: Vec<MapLanguage>,
    /// Workspace directories holding indexed files, in path order, each carrying counts
    /// inclusive of its descendants. A directory deeper than [`MAP_MODULE_DEPTH_MAX`] folds
    /// into its depth-3 ancestor. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1_000))]
    pub modules: Vec<MapModule>,
    /// The most-referenced symbols in the workspace, ranked by reference count descending,
    /// ties broken by symbol identity. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 20))]
    pub hubs: Vec<MapHub>,
    /// Symbols carrying the `entrypoint` facet, in path then identity order. Absent when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 50))]
    pub entry_points: Vec<SymbolId>,
    /// Markdown-language files, in path order. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 100))]
    pub docs: Vec<ProjectPath>,
    /// Always the whole map on one page.
    pub pagination: Pagination,
}

/// Files and symbols indexed for one exact language.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapLanguage {
    /// The exact language this count covers.
    pub language: Language,
    /// Indexed files selecting this language.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub files: u64,
    /// Declarations this language's provider extracted from those files.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub symbols: u64,
}

/// One workspace directory holding indexed files, with counts inclusive of every descendant.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_map_module_empty_defaults)]
pub struct MapModule {
    /// Project-relative directory path.
    pub path: ProjectPath,
    /// Indexed files under this directory, descendants included.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub files: u64,
    /// Declarations under this directory, descendants included.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub symbols: u64,
    /// Direct child directories, in path order. A directory at
    /// [`MAP_MODULE_DEPTH_MAX`] lists none; its deeper descendants are already folded into
    /// its own counts. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1_000))]
    pub children: Vec<MapModule>,
}

/// One of the workspace's most-referenced symbols.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapHub {
    /// The referenced symbol.
    pub symbol: SymbolId,
    /// What the symbol is in its provider's vocabulary, such as `trait` or `function`.
    pub kind: ExactKind,
    /// References this symbol's declaration received across the workspace.
    #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
    pub references: u64,
}

#[cfg(test)]
mod tests {
    use schemars::schema_for;
    use serde_json::{Value, json};

    use super::*;
    use crate::workspace::{WORKSPACE_LANGUAGE_SUMMARIES_MAX, WORKSPACE_SOURCE_UNITS_MAX};

    fn map() -> WorkspaceMap {
        WorkspaceMap {
            revision: Digest("3f9a1c2e".to_owned()),
            languages: vec![MapLanguage {
                language: Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                files: 214,
                symbols: 3_402,
            }],
            modules: vec![MapModule {
                path: ProjectPath("crates".to_owned()),
                files: 214,
                symbols: 3_402,
                children: vec![MapModule {
                    path: ProjectPath("crates/rift-server".to_owned()),
                    files: 18,
                    symbols: 512,
                    children: Vec::new(),
                }],
            }],
            hubs: vec![MapHub {
                symbol: SymbolId(
                    "rift://symbol/rust/crates/rift-server/src/read.rs/ReadService".to_owned(),
                ),
                kind: ExactKind("struct".to_owned()),
                references: 87,
            }],
            entry_points: vec![SymbolId(
                "rift://symbol/rust/crates/rift-cli/src/main.rs/main".to_owned(),
            )],
            docs: vec![ProjectPath("README.md".to_owned())],
            pagination: Pagination {
                page_index: 0,
                total_pages: 1,
            },
        }
    }

    #[test]
    fn workspace_map_serializes_its_effective_state() {
        let value = serde_json::to_value(map()).expect("workspace map serializes");

        assert_eq!(value["revision"], json!("3f9a1c2e"));
        assert_eq!(value["languages"][0]["language"], json!("rust"));
        assert_eq!(value["languages"][0]["files"], json!(214));
        assert_eq!(value["modules"][0]["path"], json!("crates"));
        assert_eq!(
            value["modules"][0]["children"][0]["path"],
            json!("crates/rift-server")
        );
        assert_eq!(
            value["hubs"][0]["symbol"],
            json!("rift://symbol/rust/crates/rift-server/src/read.rs/ReadService")
        );
        assert_eq!(value["hubs"][0]["kind"], json!("struct"));
        assert_eq!(
            value["entry_points"][0],
            json!("rift://symbol/rust/crates/rift-cli/src/main.rs/main")
        );
        assert_eq!(value["docs"][0], json!("README.md"));
        assert_eq!(value["pagination"]["total_pages"], json!(1));
    }

    #[test]
    fn absent_collections_are_omitted_from_the_wire() {
        let mut empty = map();
        empty.languages.clear();
        empty.modules.clear();
        empty.hubs.clear();
        empty.entry_points.clear();
        empty.docs.clear();
        let value = serde_json::to_value(&empty).expect("workspace map serializes");

        assert!(value.get("languages").is_none());
        assert!(value.get("modules").is_none());
        assert!(value.get("hubs").is_none());
        assert!(value.get("entry_points").is_none());
        assert!(value.get("docs").is_none());

        let deserialized: WorkspaceMap =
            serde_json::from_value(value).expect("omitted collections deserialize as empty");
        assert!(deserialized.languages.is_empty());
        assert!(deserialized.modules.is_empty());
    }

    #[test]
    fn a_module_with_no_children_omits_the_field() {
        let module = MapModule {
            path: ProjectPath("docs".to_owned()),
            files: 4,
            symbols: 0,
            children: Vec::new(),
        };
        let value = serde_json::to_value(module).expect("module serializes");
        assert!(value.get("children").is_none());
    }

    #[test]
    fn schema_carries_collection_bounds_matching_the_reused_and_named_constants() {
        let schema = serde_json::to_value(schema_for!(WorkspaceMap)).expect("map schema");
        let properties = &schema["properties"];
        assert_eq!(
            properties["languages"]["maxItems"],
            json!(WORKSPACE_LANGUAGE_SUMMARIES_MAX)
        );
        assert_eq!(
            properties["modules"]["maxItems"],
            json!(WORKSPACE_SOURCE_UNITS_MAX)
        );
        assert_eq!(properties["hubs"]["maxItems"], json!(MAP_HUBS_MAX));
        assert_eq!(
            properties["entry_points"]["maxItems"],
            json!(MAP_ENTRY_POINTS_MAX)
        );
        assert_eq!(properties["docs"]["maxItems"], json!(MAP_DOCS_MAX));

        let module = &schema["$defs"]["MapModule"]["properties"];
        assert_eq!(
            module["children"]["maxItems"],
            json!(WORKSPACE_SOURCE_UNITS_MAX)
        );
    }

    #[test]
    fn schema_declares_empty_array_defaults() {
        let schema = serde_json::to_value(schema_for!(WorkspaceMap)).expect("map schema");
        let properties = &schema["properties"];
        for name in ["languages", "modules", "hubs", "entry_points", "docs"] {
            assert_eq!(properties[name]["default"], json!([]), "field={name}");
        }
        let module = &schema["$defs"]["MapModule"]["properties"];
        assert_eq!(module["children"]["default"], json!([]));
    }

    #[test]
    fn unknown_members_are_refused() {
        let mut value = serde_json::to_value(map()).expect("workspace map serializes");
        value
            .as_object_mut()
            .expect("workspace map is an object")
            .insert("unknown".to_owned(), Value::Bool(true));

        assert!(serde_json::from_value::<WorkspaceMap>(value).is_err());
    }
}
