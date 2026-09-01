//! `rift install claude` - generates and removes the Claude Code skill
//! teaching agents to reach for Rift's tools.
//!
//! [`generate`] is the sans-I/O core: the served tool listing in, a
//! [`GeneratedSkill`] out, deterministic and checked against the listing so
//! the skill can never name a tool the server does not serve. The rest of
//! this module is the thin filesystem shell: it resolves the target scope,
//! writes the generated files atomically, and removes them.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use rift_core::{CliCode, Error, ErrorContext, ErrorName, Fault};
use rmcp::model::Tool;
use serde_json::Value;

/// Directory name the generated skill lands in, below `.claude/skills`.
const SKILL_NAME: &str = "rift";
/// Sidecar file carrying every served tool's parameters.
const TOOLS_REFERENCE_FILE: &str = "tools.md";
/// Frontmatter `description`: Claude Code reads this to decide when to load
/// the skill, so it leads with the trigger case.
const SKILL_DESCRIPTION: &str = "Use when finding, reading, or editing code in a workspace \
    Rift serves: structured search across declarations and source text, symbol reads by exact \
    name, syntax inspection, and witnessed edits that recompute their address before writing \
    and run the workspace's configured hooks.";

/// Which agent's skill `rift install` generates. A single variant today;
/// [`generate`] and the filesystem shell below are already keyed on it, so a
/// second target is an added match arm, not a rewrite. Variants carry no
/// documentation of their own: clap renders a value's doc comment as
/// per-value help, which turns the whole command's help into its long form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum InstallTarget {
    Claude,
}

/// Where the generated skill lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InstallScope {
    /// Below the current workspace's `.claude/skills`.
    Project,
    /// Below the operator's home directory's `.claude/skills`.
    User,
}

impl fmt::Display for InstallScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Project => "project",
            Self::User => "user",
        })
    }
}

/// What a completed `rift install` command prints.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum InstallOutcome {
    /// The skill was generated and written.
    Written { scope: InstallScope, root: PathBuf },
    /// The generated skill directory was removed.
    Removed { scope: InstallScope, root: PathBuf },
}

impl fmt::Display for InstallOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Written { scope, root } => write!(
                formatter,
                "✨ wrote the rift Claude Code skill ({scope} scope) to {}",
                root.display()
            ),
            Self::Removed { scope, root } => write!(
                formatter,
                "🗑️ removed the rift Claude Code skill ({scope} scope) from {}",
                root.display()
            ),
        }
    }
}

/// Failure while running one `rift install` command.
pub(super) type InstallError = Error<InstallFault>;

/// One install-command failure.
#[derive(Debug)]
pub(super) enum InstallFault {
    /// `--user` was given and neither `HOME` nor `USERPROFILE` names a directory.
    HomeDirectoryUnresolved,
    /// The hand-authored decision table names a tool the served MCP surface lacks.
    TemplateToolMissing { name: &'static str },
    /// The generated skill could not be written.
    Write { path: PathBuf, source: io::Error },
    /// The generated skill directory could not be removed.
    Remove { path: PathBuf, source: io::Error },
}

impl Fault for InstallFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::HomeDirectoryUnresolved => ErrorName::Cli(CliCode::InstallHomeUnresolved),
            Self::TemplateToolMissing { .. } => ErrorName::Cli(CliCode::InstallTemplateMissingTool),
            Self::Write { .. } => ErrorName::Cli(CliCode::InstallWriteFailed),
            Self::Remove { .. } => ErrorName::Cli(CliCode::InstallRemoveFailed),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::HomeDirectoryUnresolved => {
                vec![ErrorContext::new("checked", "HOME, USERPROFILE")]
            }
            Self::TemplateToolMissing { name } => {
                vec![ErrorContext::new("tool", (*name).to_owned())]
            }
            Self::Write { path, .. } | Self::Remove { path, .. } => {
                vec![ErrorContext::new("path", path.display().to_string())]
            }
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HomeDirectoryUnresolved | Self::TemplateToolMissing { .. } => None,
            Self::Write { source, .. } | Self::Remove { source, .. } => Some(source),
        }
    }
}

/// Runs one `rift install` command against the current directory's workspace.
///
/// # Errors
///
/// Returns [`InstallError`] when `--user` cannot resolve a home directory,
/// the served tool surface no longer carries a tool the decision table
/// names, or the generated skill could not be written or removed.
pub(super) fn run(
    target: InstallTarget,
    user: bool,
    remove: bool,
) -> Result<InstallOutcome, InstallError> {
    let InstallTarget::Claude = target;
    let scope = if user {
        InstallScope::User
    } else {
        InstallScope::Project
    };
    let scope_root = resolve_scope_root(scope)?;
    let skill_root = scope_root.join(".claude").join("skills").join(SKILL_NAME);
    if remove {
        return remove_skill(scope, skill_root);
    }
    let tools = rift_mcp::schema::tool_listing();
    let generated = generate(&tools)?;
    write_skill(scope, skill_root, &generated)
}

/// The scope's root directory, before `.claude/skills/rift` is appended.
///
/// The project scope resolves to `.` the same way `rift server` and
/// `rift mcp` resolve the workspace root: unvalidated, since the write
/// below creates whatever is missing.
fn resolve_scope_root(scope: InstallScope) -> Result<PathBuf, InstallError> {
    match scope {
        InstallScope::Project => Ok(Path::new(".").to_path_buf()),
        InstallScope::User => home_directory(&|name| std::env::var_os(name))
            .ok_or_else(|| Error::new(InstallFault::HomeDirectoryUnresolved)),
    }
}

/// The operator's home directory: `HOME` first, `USERPROFILE` (Windows)
/// second. An empty value counts as unset. The lookup is injected so a test
/// exercises the fallback without mutating the process environment.
fn home_directory(lookup: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
    set("HOME")
        .or_else(|| set("USERPROFILE"))
        .map(PathBuf::from)
}

/// The whole content of the generated skill: `SKILL.md` and its
/// `references/tools.md` sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GeneratedSkill {
    pub(super) skill_md: String,
    pub(super) tools_md: String,
}

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
/// Pure and deterministic: the same listing produces byte-identical output.
///
/// # Errors
///
/// Returns [`InstallError`] naming the missing tool when [`DECISION_TABLE`]
/// names a tool the listing does not carry - the same no-drift check the
/// exported schema document enforces on the tool surface itself.
pub(super) fn generate(tools: &[Tool]) -> Result<GeneratedSkill, InstallError> {
    for name in DECISION_TABLE
        .iter()
        .flat_map(|row| row.tools.iter().copied())
    {
        if !tools.iter().any(|tool| tool.name.as_ref() == name) {
            return Err(Error::new(InstallFault::TemplateToolMissing { name }));
        }
    }
    Ok(GeneratedSkill {
        skill_md: skill_markdown(),
        tools_md: tools_markdown(tools),
    })
}

/// Renders `SKILL.md`: frontmatter Claude Code reads to decide when to load
/// the skill, then the decision table and the reasons behind it.
fn skill_markdown() -> String {
    let mut rendered = String::new();
    rendered.push_str("---\n");
    rendered.push_str("name: rift\n");
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
/// the same way `schema_document` sorts the exported document.
fn tools_markdown(tools: &[Tool]) -> String {
    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let mut rendered = String::from("# Rift MCP tools\n\n");
    rendered.push_str(
        "Generated from the served tool surface. Regenerate with `rift install claude` \
         (or `rift install claude --user`).\n\n",
    );
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

/// Writes the generated skill under `skill_root`, replacing whatever was
/// there. Each file lands through a temp-file rename within its own
/// directory, so a reader never observes a partial write, and the rename
/// target is always a regular file - never `.claude/skills` itself, which
/// this repository symlinks to `.agents/skills`.
fn write_skill(
    scope: InstallScope,
    skill_root: PathBuf,
    generated: &GeneratedSkill,
) -> Result<InstallOutcome, InstallError> {
    let references = skill_root.join("references");
    fs::create_dir_all(&references).map_err(|source| write_error(&references, source))?;
    write_atomic(&skill_root.join("SKILL.md"), &generated.skill_md)?;
    write_atomic(&references.join(TOOLS_REFERENCE_FILE), &generated.tools_md)?;
    Ok(InstallOutcome::Written {
        scope,
        root: skill_root,
    })
}

/// Writes `content` to `path` through a sibling temp file, so a concurrent
/// reader sees either the previous content or the complete new content,
/// never a partial write.
fn write_atomic(path: &Path, content: &str) -> Result<(), InstallError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| write_error(path, source))?;
    staged
        .write_all(content.as_bytes())
        .map_err(|source| write_error(path, source))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|source| write_error(path, source))?;
    staged
        .persist(path)
        .map_err(|error| write_error(path, error.error))?;
    Ok(())
}

/// Removes the generated skill directory, tolerating one that was never
/// written. Removes only `skill_root` - the `rift` leaf below
/// `.claude/skills` - never the `.claude/skills` directory or symlink above it.
fn remove_skill(scope: InstallScope, skill_root: PathBuf) -> Result<InstallOutcome, InstallError> {
    match fs::remove_dir_all(&skill_root) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(remove_error(&skill_root, source)),
    }
    Ok(InstallOutcome::Removed {
        scope,
        root: skill_root,
    })
}

fn write_error(path: &Path, source: io::Error) -> InstallError {
    Error::new(InstallFault::Write {
        path: path.to_owned(),
        source,
    })
}

fn remove_error(path: &Path, source: io::Error) -> InstallError {
    Error::new(InstallFault::Remove {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::{
        DECISION_TABLE, InstallFault, InstallOutcome, InstallScope, SKILL_DESCRIPTION, SKILL_NAME,
        TOOLS_REFERENCE_FILE, generate, home_directory, parameters_markdown, remove_skill,
        skill_markdown, tools_markdown, write_skill,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Highest line count `SKILL.md` may reach; Claude Code loads the sidecar on demand.
    const SKILL_MD_LINES_MAX: usize = 500;
    /// Highest combined length of the frontmatter `description` and an optional
    /// `when_to_use` (unused here) Claude Code reads before truncating.
    const FRONTMATTER_DESCRIPTION_BYTES_MAX: usize = 1_536;

    #[test]
    fn generate_is_deterministic() {
        let tools = rift_mcp::schema::tool_listing();
        let first =
            generate(&tools).expect("the served surface must carry every decision-table tool");
        let second =
            generate(&tools).expect("the served surface must carry every decision-table tool");
        assert_eq!(
            first, second,
            "generate must be byte-identical across calls"
        );
    }

    #[test]
    fn every_decision_table_tool_exists_in_the_served_surface() {
        let tools = rift_mcp::schema::tool_listing();
        generate(&tools).expect("every decision-table tool name must exist in the served surface");
    }

    #[test]
    fn generate_refuses_a_decision_table_tool_the_surface_lacks() {
        let tools: Vec<_> = rift_mcp::schema::tool_listing()
            .into_iter()
            .filter(|tool| tool.name != "search")
            .collect();
        let error = generate(&tools)
            .expect_err("a decision-table tool missing from the surface must refuse");
        assert!(matches!(
            error.fault(),
            InstallFault::TemplateToolMissing { name: "search" }
        ));
        assert_eq!(error.descriptor().code(), "install_template_missing_tool");
    }

    #[test]
    fn skill_markdown_stays_under_the_line_cap() {
        let line_count = skill_markdown().lines().count();
        assert!(
            line_count < SKILL_MD_LINES_MAX,
            "SKILL.md has {line_count} lines"
        );
    }

    #[test]
    fn frontmatter_description_stays_under_the_combined_cap() {
        assert!(SKILL_DESCRIPTION.len() <= FRONTMATTER_DESCRIPTION_BYTES_MAX);
    }

    #[test]
    fn frontmatter_is_a_leading_yaml_block_naming_rift() {
        let rendered = skill_markdown();
        let mut lines = rendered.lines();
        assert_eq!(lines.next(), Some("---"));
        assert_eq!(lines.next(), Some("name: rift"));
        let description_line = lines
            .next()
            .expect("frontmatter carries a description line");
        assert!(description_line.starts_with("description: "));
        assert_eq!(lines.next(), Some("---"));
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
        let tools = rift_mcp::schema::tool_listing();
        let rendered = tools_markdown(&tools);
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
        let tools = rift_mcp::schema::tool_listing();
        let rendered = tools_markdown(&tools);
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
        assert!(rendered.contains("- `limit` - Bound on the page size."));
    }

    #[test]
    fn parameters_markdown_names_a_tool_with_no_properties() {
        let schema = serde_json::json!({"type": "object"});
        let Value::Object(schema) = schema else {
            unreachable!("json! object literal always serializes to Value::Object")
        };
        assert_eq!(parameters_markdown(&schema), "No parameters.\n");
    }

    #[test]
    fn home_directory_prefers_home_then_userprofile_and_treats_empty_as_unset() {
        let both: HashMap<&str, OsString> = HashMap::from([
            ("HOME", OsString::from("/people/ada")),
            ("USERPROFILE", OsString::from("C:\\Users\\ada")),
        ]);
        assert_eq!(
            home_directory(&|name| both.get(name).cloned()),
            Some(PathBuf::from("/people/ada"))
        );

        let profile_only: HashMap<&str, OsString> =
            HashMap::from([("USERPROFILE", OsString::from("C:\\Users\\ada"))]);
        assert_eq!(
            home_directory(&|name| profile_only.get(name).cloned()),
            Some(PathBuf::from("C:\\Users\\ada"))
        );

        let empty_home: HashMap<&str, OsString> = HashMap::from([
            ("HOME", OsString::new()),
            ("USERPROFILE", OsString::from("C:\\Users\\ada")),
        ]);
        assert_eq!(
            home_directory(&|name| empty_home.get(name).cloned()),
            Some(PathBuf::from("C:\\Users\\ada"))
        );

        assert_eq!(home_directory(&|_| None), None);
    }

    #[test]
    fn write_skill_writes_both_files_and_reruns_byte_identical() -> TestResult {
        let directory = tempfile::tempdir()?;
        let skill_root = directory
            .path()
            .join(".claude")
            .join("skills")
            .join(SKILL_NAME);
        let generated = generate(&rift_mcp::schema::tool_listing())?;
        write_skill(InstallScope::Project, skill_root.clone(), &generated)?;
        let skill_md = fs::read_to_string(skill_root.join("SKILL.md"))?;
        let tools_md =
            fs::read_to_string(skill_root.join("references").join(TOOLS_REFERENCE_FILE))?;
        assert_eq!(skill_md, generated.skill_md);
        assert_eq!(tools_md, generated.tools_md);

        write_skill(InstallScope::Project, skill_root.clone(), &generated)?;
        assert_eq!(fs::read_to_string(skill_root.join("SKILL.md"))?, skill_md);
        Ok(())
    }

    #[test]
    fn remove_skill_tolerates_a_directory_that_was_never_written() -> TestResult {
        let directory = tempfile::tempdir()?;
        let skill_root = directory
            .path()
            .join(".claude")
            .join("skills")
            .join(SKILL_NAME);
        let outcome = remove_skill(InstallScope::Project, skill_root.clone())?;
        assert_eq!(
            outcome,
            InstallOutcome::Removed {
                scope: InstallScope::Project,
                root: skill_root,
            }
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn write_skill_writes_through_a_symlinked_skills_directory_without_replacing_it() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let real_skills = directory.path().join(".agents").join("skills");
        fs::create_dir_all(&real_skills)?;
        fs::create_dir(directory.path().join(".claude"))?;
        let claude_skills = directory.path().join(".claude").join("skills");
        std::os::unix::fs::symlink(&real_skills, &claude_skills)?;

        let skill_root = claude_skills.join(SKILL_NAME);
        let generated = generate(&rift_mcp::schema::tool_listing())?;
        write_skill(InstallScope::Project, skill_root, &generated)?;

        assert!(
            fs::symlink_metadata(&claude_skills)?
                .file_type()
                .is_symlink(),
            "the skills symlink itself must survive the write"
        );
        assert_eq!(
            fs::read_to_string(real_skills.join(SKILL_NAME).join("SKILL.md"))?,
            generated.skill_md,
            "the write must land through the symlink, in the real directory"
        );
        Ok(())
    }

    #[test]
    fn install_faults_project_their_registry_identity_context_and_source() {
        use std::error::Error as _;
        use std::io;
        use std::path::Path;

        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let write = super::write_error(Path::new("skill/SKILL.md"), denied());
        assert_eq!(write.descriptor().code(), "install_write_failed");
        assert!(write.to_string().contains("skill/SKILL.md"), "{write}");
        assert!(write.source().is_some(), "{write}");

        let remove = super::remove_error(Path::new("skill"), denied());
        assert_eq!(remove.descriptor().code(), "install_remove_failed");
        assert!(remove.to_string().contains("skill"), "{remove}");
        assert!(remove.source().is_some(), "{remove}");

        let home = super::Error::new(InstallFault::HomeDirectoryUnresolved);
        assert_eq!(home.descriptor().code(), "install_home_unresolved");
        assert!(home.to_string().contains("USERPROFILE"), "{home}");
        assert!(home.source().is_none(), "{home}");

        let template = super::Error::new(InstallFault::TemplateToolMissing { name: "search" });
        assert_eq!(
            template.descriptor().code(),
            "install_template_missing_tool"
        );
        assert!(template.to_string().contains("search"), "{template}");
        assert!(template.source().is_none(), "{template}");
    }
}
