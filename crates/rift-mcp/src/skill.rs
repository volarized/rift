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
pub const SKILL_DESCRIPTION: &str = "Use when finding, reading, or editing code in a workspace \
    Rift serves: structured search across declarations and source text, symbol reads by exact \
    name, syntax inspection, and witnessed edits that recompute their address before writing \
    and run the workspace's configured hooks.";
/// The plugin manifest's `description`: the canonical product sentence.
const PLUGIN_DESCRIPTION: &str =
    "Rift is an agentic development toolkit for reading, discovering, and editing codebases.";
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
        situation: "The declaration name is known.",
        tools: &["get_symbol"],
        note: None,
    },
    DecisionRow {
        situation: "A dependency's public declaration is needed.",
        tools: &["get_symbol", "search"],
        note: Some("with `scope: \"dependencies\"`, or `\"all\"` to answer the project's own too"),
    },
    DecisionRow {
        situation: "The syntax structure at one position is needed.",
        tools: &["nodes"],
        note: None,
    },
    DecisionRow {
        situation: "A symbol's neighbors, its impact (who breaks when it changes), or a path \
                    between two symbols is needed.",
        tools: &["search"],
        note: Some("with a `traversal` block"),
    },
    DecisionRow {
        situation: "Code needs to change.",
        tools: &[
            "patch",
            "replace_node",
            "insert_node",
            "insert_symbol",
            "replace_symbol",
            "rename_symbol",
            "remove_node",
            "remove_symbol",
            "move_file",
        ],
        note: Some(
            "over raw file writes: the server recomputes witnesses and runs the workspace's \
             configured hooks",
        ),
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
    let _ = writeln!(rendered, "description: {SKILL_DESCRIPTION}");
    rendered.push_str("---\n\n");
    rendered.push_str("# Rift\n\n");
    rendered.push_str(
        "Rift indexes this workspace's source and serves it over MCP: structured search, \
         symbol reads by exact name, syntax inspection, and edits that carry their own \
         address so a stale one refuses instead of splicing into moved code.\n\n",
    );
    rendered.push_str(
        "Start an unfamiliar repository at `rift://map`; it names the served languages and \
         the workspace's own layout before any tool call.\n\n",
    );
    rendered.push_str("## Which tool\n\n");
    rendered.push_str("| Situation | Tool |\n");
    rendered.push_str("| --- | --- |\n");
    for row in DECISION_TABLE {
        rendered.push_str(&decision_row_markdown(row));
    }
    rendered.push('\n');
    rendered.push_str(
        "The edit tools apply through the server, which recomputes each address's witness \
         before writing and refuses when the source moved since the address was read. Prefer \
         them over writing files directly: a raw write bypasses that check and the \
         workspace's configured hooks.\n\n",
    );
    rendered.push_str("## When a call refuses\n\n");
    rendered.push_str(
        "Read `rift://logs` when a refusal alone does not say why; it carries the \
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
