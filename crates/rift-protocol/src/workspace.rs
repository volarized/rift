//! Wire models for the `rift://workspace` resource.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::configuration::HookKind;
use crate::read::{Digest, Language, Pagination, ProjectPath};
use crate::schema;
use crate::search::PathPattern;

/// Source units one workspace resource page may carry, at most.
pub const WORKSPACE_SOURCE_UNITS_MAX: usize = 1_000;
/// Effective language entries one workspace resource page may carry, at most.
///
/// A page reports one entry per shipped syntax provider plus one per
/// configured `[languages.<identity>]` entry the shipped set does not
/// already name, so the bound covers
/// [`LANGUAGES_MAX`](crate::configuration::LANGUAGES_MAX) configured
/// entries and every provider this build ships.
pub const WORKSPACE_LANGUAGE_SUMMARIES_MAX: usize = 128;

const _: () = assert!(
    WORKSPACE_LANGUAGE_SUMMARIES_MAX > crate::configuration::LANGUAGES_MAX,
    "a workspace page reports every configured language entry and every shipped one"
);
/// Hook summaries one workspace resource page may carry, at most: one per
/// configured hook.
pub const WORKSPACE_HOOK_SUMMARIES_MAX: usize = crate::configuration::HOOKS_MAX;
/// Bytes one workspace LSP process key may hold, at most.
pub const WORKSPACE_LSP_PROCESS_KEY_BYTES_MAX: usize = 129;

/// One page of the effective workspace configuration and source catalog.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceResourcePage {
    /// Digest of the accepted `rift.toml` bytes this page describes.
    pub configuration_revision: Digest,
    /// Effective exact-language entries, sorted by language identity.
    #[schemars(length(max = 128))]
    pub languages: Vec<WorkspaceLanguageSummary>,
    /// Configured hooks in execution order.
    #[schemars(length(max = 32))]
    pub hooks: Vec<WorkspaceHookSummary>,
    /// Source units on this page, sorted by project path.
    #[schemars(length(max = 1_000))]
    pub source: Vec<WorkspaceSourceUnit>,
    /// Where this page sits in the source catalog.
    pub pagination: Pagination,
}

/// Effective configuration for one exact language identity.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_workspace_language_summary_empty_defaults)]
pub struct WorkspaceLanguageSummary {
    /// Exact language name and optional dialect.
    pub language: Language,
    /// Whether syntax, LSP service, and execution are enabled for matched paths.
    pub enabled: bool,
    /// Effective path patterns selecting files for this language. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub include: Vec<PathPattern>,
    /// Effective path patterns removed from this language's selection. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub exclude: Vec<PathPattern>,
    /// Whether caller-provided code may run as this exact language.
    pub execution: bool,
    /// Whether a shipped syntax provider serves this exact language.
    pub syntax: bool,
    /// Selected LSP process and its current state. Absent when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<WorkspaceLspSummary>,
}

/// Selected LSP process for one exact language.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLspSummary {
    /// Accepted process key. Inline processes use the exact language identity segment.
    #[schemars(length(min = 1, max = 129))]
    #[schemars(regex(pattern = r"^[a-z][a-z0-9._-]*(?::[a-z][a-z0-9._-]*)?$"))]
    pub process: String,
    /// Current process state.
    pub state: LspState,
}

/// Current state of one configured LSP process.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LspState {
    /// No child process is running.
    Stopped,
    /// Rift is starting the child process and initializing LSP.
    Starting,
    /// The process is analyzing workspace source.
    Analyzing,
    /// The process is ready to answer requests.
    Ready,
    /// The process ended or could not answer within its configured bounds.
    Failed,
}

/// Effective path selection for one configured hook.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_workspace_hook_summary_empty_defaults)]
pub struct WorkspaceHookSummary {
    /// Hook identity from workspace configuration.
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    /// What the hook checks or changes.
    pub kind: HookKind,
    /// Path patterns selecting initially changed files for this hook. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub include: Vec<PathPattern>,
    /// Path patterns removed from hook selection. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub exclude: Vec<PathPattern>,
}

/// One source unit in the captured workspace catalog.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSourceUnit {
    /// Project-relative source path.
    pub path: ProjectPath,
    /// Digest of the source bytes this catalog captured.
    pub digest: Digest,
    /// Exact language selected for this source. Absent when none matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
}

#[cfg(test)]
mod tests {
    use schemars::schema_for;
    use serde_json::{Value, json};

    use super::*;
    use crate::configuration::{CONFIGURATION_PATTERNS_MAX, HOOK_ID_BYTES_MAX};

    fn language() -> WorkspaceLanguageSummary {
        WorkspaceLanguageSummary {
            language: Language {
                name: "typescript".to_owned(),
                dialect: Some("tsx".to_owned()),
            },
            enabled: true,
            include: vec![PathPattern("src/**/*.tsx".to_owned())],
            exclude: vec![PathPattern("src/generated/**".to_owned())],
            execution: false,
            syntax: true,
            lsp: Some(WorkspaceLspSummary {
                process: "typescript:tsx".to_owned(),
                state: LspState::Ready,
            }),
        }
    }

    fn page() -> WorkspaceResourcePage {
        WorkspaceResourcePage {
            configuration_revision: Digest("3f9a1c2e".to_owned()),
            languages: vec![language()],
            hooks: vec![WorkspaceHookSummary {
                id: "check".to_owned(),
                kind: HookKind::Build,
                include: vec![PathPattern("src/**".to_owned())],
                exclude: Vec::new(),
            }],
            source: vec![WorkspaceSourceUnit {
                path: ProjectPath("src/view.tsx".to_owned()),
                digest: Digest("8a4d20bc".to_owned()),
                language: Some(Language {
                    name: "typescript".to_owned(),
                    dialect: Some("tsx".to_owned()),
                }),
            }],
            pagination: Pagination {
                page_index: 0,
                total_pages: 1,
            },
        }
    }

    #[test]
    fn workspace_resource_page_serializes_its_effective_state() {
        let value = serde_json::to_value(page()).expect("workspace page serializes");

        assert_eq!(value["configuration_revision"], json!("3f9a1c2e"));
        assert_eq!(value["languages"][0]["language"]["dialect"], json!("tsx"));
        assert_eq!(
            value["languages"][0]["lsp"]["process"],
            json!("typescript:tsx")
        );
        assert_eq!(value["languages"][0]["lsp"]["state"], json!("ready"));
        assert_eq!(value["hooks"][0]["kind"], json!("build"));
        assert_eq!(value["source"][0]["path"], json!("src/view.tsx"));
        assert_eq!(value["pagination"]["total_pages"], json!(1));
    }

    #[test]
    fn absent_optional_members_are_omitted() {
        let mut language = language();
        language.lsp = None;
        let language = serde_json::to_value(language).expect("language summary serializes");
        assert!(language.get("lsp").is_none());

        let unit = WorkspaceSourceUnit {
            path: ProjectPath("justfile".to_owned()),
            digest: Digest("8a4d20bc".to_owned()),
            language: None,
        };
        let unit = serde_json::to_value(unit).expect("source unit serializes");
        assert!(unit.get("language").is_none());
    }

    #[test]
    fn lsp_states_use_snake_case_wire_spellings() {
        let cases = [
            (LspState::Stopped, "stopped"),
            (LspState::Starting, "starting"),
            (LspState::Analyzing, "analyzing"),
            (LspState::Ready, "ready"),
            (LspState::Failed, "failed"),
        ];

        for (state, spelling) in cases {
            let serialized = serde_json::to_value(state).expect("LSP state serializes");
            assert_eq!(serialized, json!(spelling));
            let deserialized: LspState =
                serde_json::from_value(json!(spelling)).expect("LSP state deserializes");
            assert_eq!(deserialized, state);
        }
    }

    #[test]
    fn schema_carries_collection_and_identity_bounds() {
        let schema = serde_json::to_value(schema_for!(WorkspaceResourcePage))
            .expect("workspace schema serializes");
        let page = &schema["properties"];
        assert_eq!(
            page["languages"]["maxItems"],
            json!(WORKSPACE_LANGUAGE_SUMMARIES_MAX)
        );
        assert_eq!(
            page["hooks"]["maxItems"],
            json!(WORKSPACE_HOOK_SUMMARIES_MAX)
        );
        assert_eq!(
            page["source"]["maxItems"],
            json!(WORKSPACE_SOURCE_UNITS_MAX)
        );

        let definitions = &schema["$defs"];
        let language = &definitions["WorkspaceLanguageSummary"]["properties"];
        assert_eq!(
            language["include"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
        assert_eq!(
            language["exclude"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
        let hook = &definitions["WorkspaceHookSummary"]["properties"];
        assert_eq!(hook["id"]["maxLength"], json!(HOOK_ID_BYTES_MAX));
        let lsp = &definitions["WorkspaceLspSummary"]["properties"]["process"];
        assert_eq!(lsp["minLength"], json!(1));
        assert_eq!(lsp["maxLength"], json!(WORKSPACE_LSP_PROCESS_KEY_BYTES_MAX));
        assert_eq!(
            lsp["pattern"],
            json!(r"^[a-z][a-z0-9._-]*(?::[a-z][a-z0-9._-]*)?$")
        );
    }

    #[test]
    fn unknown_members_are_refused() {
        let mut value = serde_json::to_value(page()).expect("workspace page serializes");
        value
            .as_object_mut()
            .expect("workspace page is an object")
            .insert("unknown".to_owned(), Value::Bool(true));

        assert!(serde_json::from_value::<WorkspaceResourcePage>(value).is_err());
    }
}
