//! Generates the Claude Code skill from the served tool surface.
//!
//! [`generate`] is the sans-I/O core: the served tool listing in, a
//! [`GeneratedSkill`] out, deterministic and checked against the listing so
//! the skill can never name a tool the server does not serve. One decision
//! table renders two forms: [`SkillForm::Installed`] for `rift install
//! claude`, which writes below `.claude/skills` where the `rift` executable
//! already runs, and [`SkillForm::Plugin`] for the repository's Claude Code
//! plugin, whose reader may not have the executable yet and gets the
//! install instructions. [`plugin_manifest`] renders the plugin's own
//! manifest carrying this build's version.

use std::error::Error;
use std::fmt::{self, Write as _};

use rmcp::model::Tool;
use serde_json::{Value, json};

/// Skill directory name below `.claude/skills`, and the plugin's name.
pub const SKILL_NAME: &str = "rift";
/// Sidecar file carrying every served tool's parameters.
pub const TOOLS_REFERENCE_FILE: &str = "tools.md";
/// Frontmatter `description`: Claude Code reads this to decide when to load
/// the skill, so it leads with the trigger case.
pub const SKILL_DESCRIPTION: &str = "Use when finding or reading code in a workspace \
    Rift serves: structured search across declarations and source text, symbol reads by exact \
    identity, and syntax inspection.";
/// The plugin manifest's `description`: the canonical product sentence.
const PLUGIN_DESCRIPTION: &str =
    "Rift is an agentic development toolkit for reading and discovering codebases.";
/// POSIX installer command, verbatim from the docs landing page.
const INSTALL_COMMAND_POSIX: &str =
    "curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash";
/// Windows PowerShell installer command, verbatim from the docs landing page.
const INSTALL_COMMAND_POWERSHELL: &str = "irm https://volar.sh/rift/install.ps1 | iex";

/// Which delivery the generated skill serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillForm {
    /// Written below `.claude/skills` by `rift install claude`, beside a
    /// workspace the `rift` executable already serves.
    Installed,
    /// Shipped in the repository's Claude Code plugin, where the reader may
    /// not have the `rift` executable yet.
    Plugin,
}

/// The whole content of the generated skill: `SKILL.md` and its
/// `references/tools.md` sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSkill {
    /// Rendered `SKILL.md`.
    pub skill_md: String,
    /// Rendered `references/tools.md`.
    pub tools_md: String,
}

/// The hand-authored decision table names a tool the served surface lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateToolMissing {
    /// The decision-table tool name the listing does not carry.
    pub name: &'static str,
}

impl fmt::Display for TemplateToolMissing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the skill decision table names `{}` but the served surface does not carry it; \
             align the table with the tool router",
            self.name
        )
    }
}

impl Error for TemplateToolMissing {}

/// One row of the "which tool" decision table: a situation, the tools that
/// answer it, and an optional qualifier rendered after the tool names.
struct DecisionRow {
    situation: &'static str,
    tools: &'static [&'static str],
    note: Option<&'static str>,
}

/// The decision table [`skill_markdown`] renders. Every tool name here is the
/// single source [`generate`] checks against the served surface, so the
/// table and the check can never name different tools.
const DECISION_TABLE: &[DecisionRow] = &[
    DecisionRow {
        situation: "The target is unknown.",
        tools: &["search"],
        note: None,
    },
    DecisionRow {
        situation: "Every match of a regex in the project's text is needed.",
        tools: &["search"],
        note: Some("with `:pattern` in `SERVICE :search`"),
    },
    DecisionRow {
        situation: "The exact symbol identity is known.",
        tools: &["get_symbol"],
        note: None,
    },
    DecisionRow {
        situation: "A package or runtime declaration is needed.",
        tools: &["search"],
        note: Some(
            "with `:scope \"global\"` or `\"all\"` in `SERVICE :search`; read the returned symbol IRI with `get_symbol`",
        ),
    },
    DecisionRow {
        situation: "A package version the project does not use, or a package it lacks, is \
                    needed.",
        tools: &["search"],
        note: Some(
            "with `:packages` and `:scope \"global\"` or `\"all\"` in `SERVICE :search`; read the returned symbol IRI with `get_symbol`",
        ),
    },
    DecisionRow {
        situation: "The syntax structure at one position is needed.",
        tools: &["nodes"],
        note: None,
    },
    DecisionRow {
        situation: "The declarations that reference a symbol, its impact (what changes when \
                    it does), or a path between two symbols is needed.",
        tools: &["search"],
        note: Some("with SPARQL property paths"),
    },
    DecisionRow {
        situation: "The declarations two revisions, or a revision and the working tree, hold \
                    differently are needed.",
        tools: &["search"],
        note: Some("with `:change` in `SERVICE :search`"),
    },
    DecisionRow {
        situation: "The commit that made a change, found by its message, is needed.",
        tools: &["search"],
        note: Some("with `:target \"commit\"` in `SERVICE :search`"),
    },
];

/// Generates the skill from the served tool listing.
///
/// Pure and deterministic: the same listing and form produce byte-identical
/// output.
///
/// # Errors
///
/// Returns [`TemplateToolMissing`] naming the missing tool when
/// `DECISION_TABLE` names a tool the listing does not carry - the same
/// no-drift check the exported schema document enforces on the tool surface
/// itself.
pub fn generate(tools: &[Tool], form: SkillForm) -> Result<GeneratedSkill, TemplateToolMissing> {
    for name in DECISION_TABLE
        .iter()
        .flat_map(|row| row.tools.iter().copied())
    {
        if !tools.iter().any(|tool| tool.name.as_ref() == name) {
            return Err(TemplateToolMissing { name });
        }
    }
    Ok(GeneratedSkill {
        skill_md: skill_markdown(form),
        tools_md: tools_markdown(tools, form),
    })
}

/// Renders the plugin manifest (`plugin.json`) with this build's version.
///
/// Output is pretty-printed, ends with a trailing newline, and is
/// byte-identical across calls because `serde_json` stores objects as sorted
/// maps.
#[must_use]
pub fn plugin_manifest() -> String {
    let document = json!({
        "name": SKILL_NAME,
        "displayName": "Rift",
        "version": env!("CARGO_PKG_VERSION"),
        "description": PLUGIN_DESCRIPTION,
        "author": {
            "name": "volar.sh",
            "email": "contact@volar.sh",
            "url": "https://volar.sh",
        },
    });
    let mut rendered = format!("{document:#}");
    rendered.push('\n');
    rendered
}

/// Renders `SKILL.md`: frontmatter Claude Code reads to decide when to load
/// the skill, then the decision table and the reasons behind it. The plugin
/// form appends the section for a reader without the `rift` executable.
fn skill_markdown(form: SkillForm) -> String {
    let mut rendered = String::new();
    rendered.push_str("---\n");
    let _ = writeln!(rendered, "name: {SKILL_NAME}");
    let _ = writeln!(rendered, "description: {}", json!(SKILL_DESCRIPTION));
    rendered.push_str("---\n\n");
    rendered.push_str("# Rift\n\n");
    rendered.push_str(
        "Rift indexes this workspace's source and serves it over MCP: structured search, \
         symbol reads by exact identity, and syntax inspection.\n\n",
    );
    rendered.push_str("Rift writes no source: apply a change with your own edit tools.\n\n");
    rendered.push_str(
        "Read `rift://map` first for served languages, workspace layout, and language server \
         recommendations. When configuring language servers, run listed `rift config set` \
         commands from the workspace root. After creating a named profile, reconnect MCP to \
         select it in the SPARQL statement. Profiles are defined in the root `rift.toml`; \
         accepted datasets and services select extra workspaces registered through `[mcp.extra]`.\n\n",
    );
    rendered.push_str("## Which tool\n\n");
    rendered.push_str("| Situation | Tool |\n");
    rendered.push_str("| --- | --- |\n");
    for row in DECISION_TABLE {
        rendered.push_str(&decision_row_markdown(row));
    }
    rendered.push('\n');
    rendered.push_str(
        "## Query recipes\n\n\
         `search` takes required SPARQL `query` and optional `language`, default `sparql`. \
         Prefixes `:`, `r:`, `rdf:`, `rdfs:`, and `xsd:` are supplied. Replace `<symbol-id>` \
         below with an observed canonical symbol IRI.\n\n\
         - Names: `SELECT ?symbol {?symbol a :Symbol; :name \"load_config\"}`\n\
         - Usage identities: `SELECT ?usage {<symbol-id> :usages ?usage}`\n\
         - Usage locations: `SELECT * {<symbol-id> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end}`\n\
         - Distinct callers: `SELECT DISTINCT ?caller {?caller :references <symbol-id>}`\n\
         - Direct callees: `SELECT ?callee {<symbol-id> :calls ?callee}`\n\
         - Third-level callees: `SELECT ?callee {<symbol-id> :calls/:calls/:calls ?callee}`\n\
         - Substring: `SELECT ?symbol {?symbol a :Symbol; :name ?name FILTER(CONTAINS(?name, \"config\"))}`\n\
         - Regex: `SELECT ?path ?start {SERVICE :search {?hit :pattern \"TODO|FIXME\"; :target \"file\"; :path ?path; :start ?start}}`\n\
         - Count: `SELECT (COUNT(?usage) AS ?count) {<symbol-id> :usages ?usage}`\n\n\
         ### Text search\n\n\
         Find declarations by text, keeping the retrieval order. Optional fields remain \
         unbound when the selected hit does not carry them.\n\n\
         ```sparql\n\
         SELECT ?symbol ?name ?path\n\
         WHERE {\n\
           SERVICE :search {\n\
             ?hit :query \"request deadline\"; :target \"symbol\"; :index ?index .\n\
             OPTIONAL { ?hit :symbol ?symbol . OPTIONAL { ?symbol :name ?name } }\n\
             OPTIONAL { ?hit :path ?path }\n\
           }\n\
         }\n\
         ORDER BY ?index LIMIT 20 OFFSET 0\n\
         ```\n\n\
         ### Match search\n\n\
         Match a regex in selected files. Structured retrieval inputs use `rdf:JSON`.\n\n\
         ```sparql\n\
         SELECT ?path ?line ?source\n\
         WHERE {\n\
           SERVICE :search {\n\
             ?hit :pattern \"TODO|FIXME\"; :target \"file\"; :index ?index; :path ?path;\n\
                  :paths '{\"include\":[\"crates/**\"]}'^^rdf:JSON;\n\
                  :include '[\"source\"]'^^rdf:JSON .\n\
             OPTIONAL { ?hit :line ?line }\n\
             OPTIONAL { ?hit :source ?source }\n\
           }\n\
         }\n\
         ORDER BY ?index LIMIT 20 OFFSET 0\n\
         ```\n\n\
         ### Traversal\n\n\
         Select a declaration by its name and path, then follow two call edges. This \
         example starts at Rift's CLI entry point; use a name and path from the served \
         workspace. Required relationships must be available from the selected provider.\n\n\
         ```sparql\n\
         SELECT ?callee ?name ?path\n\
         WHERE {\n\
           ?seed a :Symbol; :name \"main\"; :path \"crates/rift/src/main.rs\" .\n\
           ?seed :calls/:calls ?callee .\n\
           OPTIONAL { ?callee :name ?name }\n\
           OPTIONAL { ?callee :path ?path }\n\
         }\n\
         ORDER BY ?name ?path LIMIT 20\n\
         ```\n\n\
         Set `OFFSET 20` to select the next query window. Follow returned \
         `pagination.next_query` unchanged for transport pages. Carry the returned `view` \
         with exact symbol and source reads. Missing required facts return unavailability, \
         never a false boolean or empty complete answer.\n\n",
    );
    let _ = writeln!(
        rendered,
        "The `:` vocabulary is `{}`. The local named graph is `{}`.\n",
        rift_search::sparql::VOCABULARY,
        rift_search::sparql::LOCAL_GRAPH,
    );
    rendered.push_str("## When a call refuses\n\n");
    rendered.push_str(
        "Read `rift server logs` when a refusal alone does not say why; it carries the \
         workspace's own recorded diagnostics.\n\n",
    );
    let _ = writeln!(
        rendered,
        "See [references/{TOOLS_REFERENCE_FILE}](references/{TOOLS_REFERENCE_FILE}) for every \
         served tool's parameters."
    );
    if form == SkillForm::Plugin {
        rendered.push('\n');
        rendered.push_str(&missing_executable_markdown());
    }
    rendered
}

/// The plugin form's section for a reader without the `rift` executable.
fn missing_executable_markdown() -> String {
    let mut rendered = String::from("## Without the rift CLI\n\n");
    rendered.push_str(
        "The plugin starts its MCP server by running `rift mcp`, so the `rift` executable \
         must be on `PATH`. When Claude Code lists no rift tools, install it:\n\n",
    );
    let _ = writeln!(rendered, "- Linux and macOS: `{INSTALL_COMMAND_POSIX}`");
    let _ = writeln!(
        rendered,
        "- Windows PowerShell: `{INSTALL_COMMAND_POWERSHELL}`"
    );
    rendered.push('\n');
    rendered.push_str(
        "The installer downloads the latest verified release for your platform and installs \
         it for the current user. Reconnect with `/mcp` after installing.\n\n",
    );
    rendered.push_str(
        "A workspace that already carries a `rift` entry in its own `.mcp.json` (written by \
         `rift install claude`) runs a second proxy beside the plugin's; both reach the same \
         workspace server. Keep one: remove the project entry or uninstall the plugin.\n",
    );
    rendered
}

/// One decision-table row as a markdown table line.
fn decision_row_markdown(row: &DecisionRow) -> String {
    let tools = row
        .tools
        .iter()
        .map(|tool| format!("`{tool}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let cell = match row.note {
        Some(note) => format!("{tools} ({note})"),
        None => tools,
    };
    format!("| {} | {cell} |\n", row.situation)
}

/// Renders `references/tools.md`: one section per served tool, name-sorted
/// the same way `schema_document` sorts the exported document. The
/// regeneration note names the path that owns the reader's copy: the
/// installed form is rewritten by `rift install claude`, while the plugin
/// form ships with the plugin and updates with it.
fn tools_markdown(tools: &[Tool], form: SkillForm) -> String {
    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let mut rendered = String::from("# Rift MCP tools\n\n");
    rendered.push_str(match form {
        SkillForm::Installed => {
            "Generated from the served tool surface. Regenerate with `rift install claude` \
             (or `rift install claude --user`).\n\n"
        }
        SkillForm::Plugin => "Generated from the served tool surface.\n\n",
    });
    for tool in sorted {
        rendered.push_str(&tool_section_markdown(tool));
    }
    rendered
}

/// One tool's section: its name, its served description verbatim, and its
/// top-level parameters.
fn tool_section_markdown(tool: &Tool) -> String {
    let description = tool.description.as_deref().unwrap_or_default();
    format!(
        "## {name}\n\n{description}\n\n{parameters}\n",
        name = tool.name,
        parameters = parameters_markdown(&tool.input_schema)
    )
}

/// The tool's top-level parameters as a bullet list: name, whether required,
/// and the first line of the schema's own description.
fn parameters_markdown(input_schema: &serde_json::Map<String, Value>) -> String {
    let Some(properties) = input_schema
        .get("properties")
        .and_then(|value| value.as_object())
    else {
        return "No parameters.\n".to_owned();
    };
    let required: Vec<&str> = input_schema
        .get("required")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .collect();
    let mut rendered = String::from("Parameters:\n\n");
    for (name, schema) in properties {
        let marker = if required.contains(&name.as_str()) {
            " (required)"
        } else {
            ""
        };
        let gloss = schema
            .get("description")
            .and_then(|value| value.as_str())
            .map(first_sentence)
            .unwrap_or_default();
        let _ = writeln!(rendered, "- `{name}`{marker} - {gloss}");
    }
    rendered
}

/// The first sentence of one schema description, its source line wraps joined. Field doc
/// comments wrap prose across lines mid-sentence, so a first-line cut truncates the gloss.
fn first_sentence(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.find(". ") {
        Some(end) => joined[..=end].to_owned(),
        None => joined,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        DECISION_TABLE, GeneratedSkill, SKILL_DESCRIPTION, SkillForm, TemplateToolMissing,
        generate, parameters_markdown, plugin_manifest, skill_markdown, tools_markdown,
    };

    /// Highest line count `SKILL.md` may reach; Claude Code loads the sidecar on demand.
    const SKILL_MD_LINES_MAX: usize = 500;
    /// Highest combined length of the frontmatter `description` and an optional
    /// `when_to_use` (unused here) Claude Code reads before truncating.
    const FRONTMATTER_DESCRIPTION_BYTES_MAX: usize = 1_536;

    fn generated(form: SkillForm) -> GeneratedSkill {
        let tools = crate::schema::tool_listing();
        generate(&tools, form).expect("the served surface must carry every decision-table tool")
    }

    #[test]
    fn generate_is_deterministic_per_form() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            assert_eq!(
                generated(form),
                generated(form),
                "generate must be byte-identical across calls"
            );
        }
    }

    #[test]
    fn generate_refuses_a_decision_table_tool_the_surface_lacks() {
        let tools: Vec<_> = crate::schema::tool_listing()
            .into_iter()
            .filter(|tool| tool.name != "search")
            .collect();
        let error = generate(&tools, SkillForm::Installed)
            .expect_err("a decision-table tool missing from the surface must refuse");
        assert_eq!(error, TemplateToolMissing { name: "search" });
        assert!(error.to_string().contains("`search`"));
    }

    #[test]
    fn skill_markdown_stays_under_the_line_cap_in_both_forms() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let line_count = skill_markdown(form).lines().count();
            assert!(
                line_count < SKILL_MD_LINES_MAX,
                "SKILL.md has {line_count} lines"
            );
        }
    }

    #[test]
    fn frontmatter_description_stays_under_the_combined_cap() {
        assert!(SKILL_DESCRIPTION.len() <= FRONTMATTER_DESCRIPTION_BYTES_MAX);
    }

    #[test]
    fn frontmatter_is_a_leading_yaml_block_naming_rift() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            let mut lines = rendered.lines();
            assert_eq!(lines.next(), Some("---"));
            assert_eq!(lines.next(), Some("name: rift"));
            let description_line = lines
                .next()
                .expect("frontmatter carries a description line");
            assert!(description_line.starts_with("description: "));
            assert_eq!(lines.next(), Some("---"));
        }
    }

    #[test]
    fn plugin_form_carries_the_install_section_and_installed_form_does_not() {
        let plugin = skill_markdown(SkillForm::Plugin);
        assert!(plugin.contains("## Without the rift CLI"));
        assert!(plugin.contains("https://volar.sh/rift/install.sh"));
        assert!(plugin.contains("https://volar.sh/rift/install.ps1"));
        let installed = skill_markdown(SkillForm::Installed);
        assert!(!installed.contains("## Without the rift CLI"));
    }

    #[test]
    fn opening_prose_sends_a_change_back_to_the_agents_own_tools() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            assert!(
                rendered
                    .contains("Rift writes no source: apply a change with your own edit tools."),
                "{rendered}"
            );
        }
    }

    #[test]
    fn decision_table_routes_a_package_the_project_lacks_to_packages() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            assert!(
                rendered.contains("`:packages`") && rendered.contains("`SERVICE :search`"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn decision_table_reads_known_ids_and_discovers_package_names() {
        let known = DECISION_TABLE
            .iter()
            .find(|row| row.situation == "The exact symbol identity is known.")
            .expect("known identities must have a read route");
        assert_eq!(known.tools, &["get_symbol"]);
        assert!(known.note.is_none());
        for row in DECISION_TABLE
            .iter()
            .filter(|row| row.situation.contains("package") || row.situation.contains("runtime"))
        {
            assert_eq!(row.tools, &["search"]);
            let note = row.note.expect("discovery must select its search scope");
            assert!(note.contains("scope"));
            assert!(note.contains("returned symbol IRI with `get_symbol`"));
        }
    }

    #[test]
    fn decision_table_routes_a_revision_comparison_to_search() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            assert!(
                rendered.contains(
                    "| The declarations two revisions, or a revision and the working tree, hold \
                     differently are needed. | `search` (with `:change` in `SERVICE :search`) |"
                ),
                "{rendered}"
            );
        }
    }

    #[test]
    fn decision_table_routes_a_commit_message_to_search() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            assert!(
                rendered.contains(
                    "| The commit that made a change, found by its message, is needed. | \
                     `search` (with `:target \"commit\"` in `SERVICE :search`) |"
                ),
                "{rendered}"
            );
        }
    }

    #[test]
    fn decision_table_routes_a_regex_to_search_with_pattern() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            assert!(
                rendered.contains(
                    "| Every match of a regex in the project's text is needed. | `search` (with \
                     `:pattern` in `SERVICE :search`) |"
                ),
                "{rendered}"
            );
        }
    }

    #[test]
    fn decision_table_lists_only_real_tool_names() {
        assert!(!DECISION_TABLE.is_empty());
        for row in DECISION_TABLE {
            assert!(!row.tools.is_empty(), "{}", row.situation);
        }
    }

    #[test]
    fn tools_markdown_carries_every_tools_own_description_verbatim() {
        let tools = crate::schema::tool_listing();
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = tools_markdown(&tools, form);
            for tool in &tools {
                assert!(
                    rendered.contains(&format!("## {}", tool.name)),
                    "missing heading for {}",
                    tool.name
                );
                if let Some(description) = &tool.description {
                    assert!(
                        rendered.contains(description.as_ref()),
                        "missing verbatim description for {}",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn regeneration_note_names_the_owning_path_per_form() {
        let tools = crate::schema::tool_listing();
        let installed = tools_markdown(&tools, SkillForm::Installed);
        assert!(installed.contains("Regenerate with `rift install claude`"));
        let plugin = tools_markdown(&tools, SkillForm::Plugin);
        assert!(!plugin.contains("rift install claude"));
    }

    #[test]
    fn first_sentence_joins_wrapped_lines_before_cutting() {
        let wrapped = "Optional hit fields to attach: `source`, `history`. Omitted defaults to\n`[\"source\"]`; an explicit empty list carries neither.";
        assert_eq!(
            super::first_sentence(wrapped),
            "Optional hit fields to attach: `source`, `history`.",
        );
        assert_eq!(super::first_sentence("One sentence."), "One sentence.");
    }

    #[test]
    fn parameter_glosses_end_at_a_sentence_boundary() {
        let tools = crate::schema::tool_listing();
        let rendered = tools_markdown(&tools, SkillForm::Installed);
        for line in rendered.lines().filter(|line| line.starts_with("- `")) {
            assert!(
                line.ends_with('.'),
                "a parameter gloss must be a whole sentence: {line}"
            );
        }
    }

    #[test]
    fn parameters_markdown_marks_required_fields_and_first_description_line() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The lookup name.\nMore detail on a second line."
                },
                "limit": {"type": "integer", "description": "Bound on the page size."}
            },
            "required": ["name"]
        });
        let Value::Object(schema) = schema else {
            unreachable!("json! object literal always serializes to Value::Object")
        };
        let rendered = parameters_markdown(&schema);
        assert!(rendered.contains("- `name` (required) - The lookup name."));
        assert!(!rendered.contains("More detail on a second line."));
    }

    #[test]
    fn parameters_markdown_answers_no_parameters_without_properties() {
        let schema = serde_json::Map::new();
        assert_eq!(parameters_markdown(&schema), "No parameters.\n");
    }

    #[test]
    fn plugin_manifest_carries_this_builds_version() {
        let manifest = plugin_manifest();
        let document: Value =
            serde_json::from_str(&manifest).expect("the manifest must parse as JSON");
        assert_eq!(document["name"], "rift");
        assert_eq!(document["version"], env!("CARGO_PKG_VERSION"));
        assert!(manifest.ends_with('\n'));
        assert_eq!(manifest, plugin_manifest());
    }

    fn prepared_examples(
        form: SkillForm,
        limits: rift_search::query::QueryLimits,
    ) -> Result<Vec<rift_search::sparql::PreparedQuery>, rift_error::RiftError> {
        let rendered = skill_markdown(form);
        rendered
            .split("```sparql\n")
            .skip(1)
            .map(|block| {
                let (statement, _) = block
                    .split_once("\n```")
                    .expect("each rendered SPARQL example closes its fence");
                rift_search::sparql::PreparedQuery::parse(statement, limits)
            })
            .collect()
    }

    fn example_service(
        query: &rift_search::sparql::PreparedQuery,
        hits: Vec<rift_protocol::read::SearchHit>,
    ) -> rift_search::sparql::QueryServices {
        rift_search::sparql::QueryServices {
            retrievals: vec![rift_search::sparql::QueryServiceData {
                request: query.retrievals()[0].clone(),
                targets: vec![None; hits.len()],
                result: rift_protocol::read::SearchResult {
                    view: None,
                    results: hits,
                    pagination: rift_protocol::read::Pagination {
                        page_index: 0,
                        total_pages: 1,
                    },
                    warnings: Vec::new(),
                },
            }],
            bindings: Vec::new(),
            datasets: Vec::new(),
        }
    }

    fn example_bindings(
        answer: rift_protocol::query::QueryAnswer,
    ) -> Vec<std::collections::BTreeMap<String, rift_protocol::query::RdfTerm>> {
        match answer {
            rift_protocol::query::QueryAnswer::Bindings { bindings, .. } => bindings,
            other => panic!("rendered SELECT example returned {other:?}"),
        }
    }

    fn example_symbol(
        name: &str,
        start: u64,
    ) -> Result<rift_protocol::query::QuerySymbol, serde_json::Error> {
        let symbol: rift_protocol::read::Symbol = serde_json::from_value(serde_json::json!({
            "id": format!("rift://symbol/local/rust/fixture/{name}"),
            "name": name, "kind": "function", "language": "rust"
        }))?;
        let declaration = rift_protocol::symbol_read::SymbolDeclaration {
            origin: symbol.origin.clone(),
            path: Some(serde_json::from_value(serde_json::json!(
                "crates/rift/src/main.rs"
            ))?),
            unit: None,
            range: rift_protocol::read::TextRange {
                start,
                end: start + 1,
            },
            line: 1,
            node: None,
            source: None,
            source_complete: false,
            signature_indices: Vec::new(),
            type_indices: Vec::new(),
            documentation_indices: Vec::new(),
        };
        Ok(rift_protocol::query::QuerySymbol {
            symbol,
            declarations: vec![declaration],
        })
    }

    fn example_relationship(
        from: &str,
        to: &str,
        start: u64,
    ) -> Result<rift_protocol::read::Relationship, serde_json::Error> {
        serde_json::from_value(serde_json::json!({
            "from": format!("rift://symbol/local/rust/fixture/{from}"),
            "to": format!("rift://symbol/local/rust/fixture/{to}"),
            "kind": "call", "facets": ["calls"], "derivation": "resolution",
            "occurrence": {
                "unit": "rift://source/project/crates/rift/src/main.rs",
                "range": { "start": start, "end": start + 1 }
            }
        }))
    }

    fn evaluate_text_example(
        query: &rift_search::sparql::PreparedQuery,
        limits: rift_search::query::QueryLimits,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hits = vec![
            serde_json::from_value(serde_json::json!({
                "hit": { "target": "symbol", "symbol": {
                    "id": "rift://symbol/local/rust/fixture/deadline",
                    "name": "request_deadline", "kind": "function", "language": "rust"
                } },
                "path": "crates/fixture.rs", "score": 0.8
            }))?,
            serde_json::from_value(serde_json::json!({
                "hit": { "target": "symbol", "symbol": {
                    "id": "rift://symbol/local/rust/fixture/timeout",
                    "name": "request_timeout", "kind": "function", "language": "rust"
                } },
                "score": 0.5
            }))?,
        ];
        let rows = example_bindings(query.evaluate_with_services(
            rift_search::query::QueryFacts::default(),
            limits,
            rift_search::sparql::QueryCancellation::default(),
            example_service(query, hits),
        )?);
        assert_eq!(rows.len(), 2);
        assert!(
            matches!(&rows[0]["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "request_deadline")
        );
        assert!(rows[0].contains_key("path"));
        assert!(!rows[1].contains_key("path"));
        Ok(())
    }

    fn evaluate_pattern_example(
        query: &rift_search::sparql::PreparedQuery,
        limits: rift_search::query::QueryLimits,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hits = vec![
            serde_json::from_value(serde_json::json!({
                "hit": { "target": "file", "size": 40, "languages": [] },
                "path": "crates/fixture.rs", "line": 1, "source": "// TODO: check deadline"
            }))?,
            serde_json::from_value(serde_json::json!({
                "hit": { "target": "file", "size": 40, "languages": [] },
                "path": "crates/other.rs"
            }))?,
        ];
        let rows = example_bindings(query.evaluate_with_services(
            rift_search::query::QueryFacts::default(),
            limits,
            rift_search::sparql::QueryCancellation::default(),
            example_service(query, hits),
        )?);
        assert_eq!(rows.len(), 2);
        assert!(
            matches!(&rows[0]["source"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "// TODO: check deadline")
        );
        assert!(rows[0].contains_key("line"));
        assert!(!rows[1].contains_key("source"));
        assert!(!rows[1].contains_key("line"));
        Ok(())
    }

    fn evaluate_traversal_example(
        query: &rift_search::sparql::PreparedQuery,
        limits: rift_search::query::QueryLimits,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let facts = rift_search::query::QueryFacts {
            symbols: vec![
                example_symbol("main", 0)?,
                example_symbol("middle", 2)?,
                example_symbol("leaf", 4)?,
            ],
            relationships: vec![
                example_relationship("main", "middle", 0)?,
                example_relationship("middle", "leaf", 2)?,
            ],
            ..rift_search::query::QueryFacts::default()
        };
        let rows = example_bindings(query.evaluate(
            facts,
            limits,
            rift_search::sparql::QueryCancellation::default(),
        )?);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["callee"],
            rift_protocol::query::RdfTerm::Iri {
                value: "rift://symbol/local/rust/fixture/leaf".to_owned(),
            }
        );
        assert!(
            matches!(&rows[0]["name"], rift_protocol::query::RdfTerm::Literal { value, .. } if value == "leaf")
        );
        Ok(())
    }

    #[test]
    fn rendered_sparql_examples_evaluate_original_rows_and_two_call_edges()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = rift_search::query::QueryLimits::try_from(
            &rift_protocol::query::QueryConfiguration::default(),
        )?;
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let queries = prepared_examples(form, limits)?;
            assert_eq!(queries.len(), 3);
            evaluate_text_example(&queries[0], limits)?;
            evaluate_pattern_example(&queries[1], limits)?;
            evaluate_traversal_example(&queries[2], limits)?;
        }
        Ok(())
    }

    #[test]
    fn rendered_sparql_examples_use_served_prefixes_and_retrieval_metadata()
    -> Result<(), rift_error::RiftError> {
        let limits = rift_search::query::QueryLimits::try_from(
            &rift_protocol::query::QueryConfiguration::default(),
        )?;
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let rendered = skill_markdown(form);
            let queries = rendered
                .split("```sparql\n")
                .skip(1)
                .map(|block| {
                    let (statement, _) = block
                        .split_once("\n```")
                        .expect("each rendered SPARQL example closes its fence");
                    rift_search::sparql::PreparedQuery::parse(statement, limits)
                })
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(queries.len(), 3);
            assert_eq!(queries[0].retrievals().len(), 1);
            assert!(queries[0].retrievals()[0].query.is_some());
            assert!(queries[0].retrievals()[0].pattern.is_none());
            assert_eq!(queries[1].retrievals().len(), 1);
            assert!(queries[1].retrievals()[0].pattern.is_some());
            assert!(queries[1].retrievals()[0].paths.is_some());
            assert!(queries[2].retrievals().is_empty());
            assert!(
                queries[2]
                    .requirements()
                    .facets
                    .contains(&rift_protocol::read::RelationshipFacet::Calls)
            );
        }
        Ok(())
    }

    #[test]
    fn rendered_prose_carries_no_banned_dashes() {
        for form in [SkillForm::Installed, SkillForm::Plugin] {
            let generated = generated(form);
            for content in [&generated.skill_md, &generated.tools_md] {
                assert!(
                    !content.contains('\u{2014}') && !content.contains('\u{2013}'),
                    "generated skill prose must spell every dash as the plain hyphen"
                );
            }
        }
    }
}
