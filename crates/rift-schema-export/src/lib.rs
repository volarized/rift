//! Deterministic export and validation of Rift artifacts.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rift_core::{ErrorCode, ErrorDescriptor, ErrorName};
use rift_mcp::skill::{self, SkillForm};

const OUTPUT_DIR_DEFAULT: &str = "docs";
const MCP_SCHEMA_PATH: &str = "public/mcp.json";
const CONFIGURATION_SCHEMA_PATH: &str = "public/rift.schema.json";
const PACKAGE_INDEX_SCHEMA_PATH: &str = "public/package-index.schema.json";
const PLUGIN_DIR_DEFAULT: &str = "plugins/claude";
const PLUGIN_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";
const PLUGIN_SKILL_PATH: &str = "skills/rift/SKILL.md";
const PLUGIN_TOOLS_PATH: &str = "skills/rift/references/tools.md";
const USAGE: &str = "usage: rift-schema-export [--check] [--analyzer-manifest | --global-contract] [OUTPUT_DIR] [PLUGIN_DIR]";
const REGENERATE_COMMAND: &str = "just generate";

/// Why an export run could not complete.
#[derive(Debug)]
pub enum ExportError {
    /// An argument started with `-` but is not a supported flag.
    UnknownFlag {
        /// Argument as given.
        argument: String,
    },
    /// A third positional argument followed the plugin directory.
    ExtraArgument {
        /// Argument as given.
        argument: String,
    },
    /// The skill decision table names a tool the served surface lacks.
    TemplateToolMissing {
        /// Missing tool name.
        name: &'static str,
    },
    /// The analyzer manifest could not be rendered from the repository tree.
    AnalyzerManifest {
        /// Renderer failure.
        source: rift_index::ManifestError,
    },
    /// The published global contract is invalid.
    GlobalContract {
        /// Contract validation failure.
        source: rift_cloud_client::contract::ContractError,
    },
    /// The document to check could not be read.
    CheckUnreadable {
        /// Document path.
        path: PathBuf,
        /// Read failure.
        source: io::Error,
    },
    /// The committed document differs from generated content.
    CheckMismatch {
        /// Document path.
        path: PathBuf,
    },
    /// The output directory or document could not be written.
    WriteFailed {
        /// Directory or document path.
        path: PathBuf,
        /// Write failure.
        source: io::Error,
    },
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlag { argument } => write!(
                formatter,
                "unknown argument `{argument}`: expected `--check` or an output directory; {USAGE}"
            ),
            Self::ExtraArgument { argument } => write!(
                formatter,
                "unexpected extra argument `{argument}`: expected at most an output directory and a plugin directory; {USAGE}"
            ),
            Self::TemplateToolMissing { name } => write!(
                formatter,
                "the skill decision table names `{name}` but the served surface does not carry it; align the table in the skill module with the tool router"
            ),
            Self::AnalyzerManifest { source } => write!(formatter, "{source}"),
            Self::GlobalContract { source } => write!(formatter, "{source}"),
            Self::CheckUnreadable { path, source } => write!(
                formatter,
                "cannot read `{}` to check it: {source}; generate the document first with `{REGENERATE_COMMAND}`",
                path.display()
            ),
            Self::CheckMismatch { path } => write!(
                formatter,
                "`{}` does not match what this tree derives; regenerate it with `{REGENERATE_COMMAND}`",
                path.display()
            ),
            Self::WriteFailed { path, source } => write!(
                formatter,
                "cannot write `{}`: {source}; ensure the output directory is writable or pass another one ({USAGE})",
                path.display()
            ),
        }
    }
}

impl Error for ExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CheckUnreadable { source, .. } | Self::WriteFailed { source, .. } => Some(source),
            Self::AnalyzerManifest { source } => Some(source),
            Self::GlobalContract { source } => Some(source),
            Self::UnknownFlag { .. }
            | Self::ExtraArgument { .. }
            | Self::TemplateToolMissing { .. }
            | Self::CheckMismatch { .. } => None,
        }
    }
}

impl ExportError {
    /// Returns canonical registry metadata.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        match self {
            Self::UnknownFlag { .. } | Self::ExtraArgument { .. } => {
                ErrorName::Wire(ErrorCode::InvalidRequest).descriptor()
            }
            Self::TemplateToolMissing { .. } => {
                ErrorName::Cli(rift_core::CliCode::InstallTemplateMissingTool).descriptor()
            }
            Self::CheckUnreadable { .. }
            | Self::WriteFailed { .. }
            | Self::AnalyzerManifest { .. } => {
                ErrorName::Wire(ErrorCode::StorageFailure).descriptor()
            }
            Self::CheckMismatch { .. } | Self::GlobalContract { .. } => {
                ErrorName::Cli(rift_core::CliCode::ArtifactStale).descriptor()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportTarget {
    Documents,
    AnalyzerManifest,
    GlobalContract,
}

/// One parsed export invocation.
#[derive(Debug)]
pub struct ExportRequest {
    check: bool,
    output_dir: PathBuf,
    plugin_dir: PathBuf,
    target: ExportTarget,
}

/// Parses `rift-schema-export` arguments without the program name.
///
/// # Errors
///
/// Returns [`ExportError`] for an unknown flag or extra positional argument.
pub fn parse_arguments<Arguments>(arguments: Arguments) -> Result<ExportRequest, ExportError>
where
    Arguments: IntoIterator<Item = String>,
{
    let mut check = false;
    let mut target = ExportTarget::Documents;
    let mut output_dir = None;
    let mut plugin_dir = None;
    for argument in arguments {
        if argument == "--check" {
            check = true;
        } else if argument == "--analyzer-manifest" {
            target = ExportTarget::AnalyzerManifest;
        } else if argument == "--global-contract" {
            target = ExportTarget::GlobalContract;
        } else if argument.starts_with('-') {
            return Err(ExportError::UnknownFlag { argument });
        } else if output_dir.is_none() {
            output_dir = Some(PathBuf::from(argument));
        } else if plugin_dir.is_none() {
            plugin_dir = Some(PathBuf::from(argument));
        } else {
            return Err(ExportError::ExtraArgument { argument });
        }
    }
    Ok(ExportRequest {
        check,
        output_dir: output_dir.unwrap_or_else(|| PathBuf::from(OUTPUT_DIR_DEFAULT)),
        plugin_dir: plugin_dir.unwrap_or_else(|| PathBuf::from(PLUGIN_DIR_DEFAULT)),
        target,
    })
}

/// Writes or checks generated artifacts, or validates one authored contract.
///
/// # Errors
///
/// Returns [`ExportError`] when generation, validation, reading, or writing fails.
pub fn run(request: &ExportRequest) -> Result<(), ExportError> {
    match request.target {
        ExportTarget::AnalyzerManifest => return run_analyzer_manifest(request),
        ExportTarget::GlobalContract => return validate_global_contract(request),
        ExportTarget::Documents => {}
    }
    let tools = rift_mcp::schema::tool_listing();
    let generated = skill::generate(&tools, SkillForm::Plugin)
        .map_err(|missing| ExportError::TemplateToolMissing { name: missing.name })?;
    let documents = [
        (
            request.output_dir.join(MCP_SCHEMA_PATH),
            rift_mcp::schema::schema_document(),
        ),
        (
            request.output_dir.join(CONFIGURATION_SCHEMA_PATH),
            rift_protocol::schema::configuration_schema_document(),
        ),
        (
            request.output_dir.join(PACKAGE_INDEX_SCHEMA_PATH),
            rift_protocol::schema::package_index_schema_document(),
        ),
        (
            request.plugin_dir.join(PLUGIN_MANIFEST_PATH),
            skill::plugin_manifest(),
        ),
        (
            request.plugin_dir.join(PLUGIN_SKILL_PATH),
            generated.skill_md,
        ),
        (
            request.plugin_dir.join(PLUGIN_TOOLS_PATH),
            generated.tools_md,
        ),
    ];
    if request.check {
        for (path, document) in &documents {
            check_document(path, document)?;
        }
        return Ok(());
    }
    for (path, document) in &documents {
        let parent = path.parent().unwrap_or(request.output_dir.as_path());
        fs::create_dir_all(parent).map_err(|source| ExportError::WriteFailed {
            path: parent.to_path_buf(),
            source,
        })?;
        fs::write(path, document).map_err(|source| ExportError::WriteFailed {
            path: path.clone(),
            source,
        })?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn validate_global_contract(request: &ExportRequest) -> Result<(), ExportError> {
    let path = request
        .output_dir
        .join(rift_cloud_client::contract::DOCUMENT_PATH);
    rift_cloud_client::contract::validate(&path)
        .map_err(|source| ExportError::GlobalContract { source })?;
    println!("{} is valid", path.display());
    Ok(())
}

fn run_analyzer_manifest(request: &ExportRequest) -> Result<(), ExportError> {
    let root = request.output_dir.as_path();
    let document = rift_index::render_analyzer_manifest(root)
        .map_err(|source| ExportError::AnalyzerManifest { source })?;
    let path = root.join(rift_index::analyzer_manifest_path());
    if request.check {
        return check_document(&path, &document);
    }
    fs::write(&path, &document).map_err(|source| ExportError::WriteFailed {
        path: path.clone(),
        source,
    })?;
    println!("wrote {}", path.display());
    Ok(())
}

fn check_document(path: &Path, document: &str) -> Result<(), ExportError> {
    let committed = fs::read_to_string(path).map_err(|source| ExportError::CheckUnreadable {
        path: path.to_path_buf(),
        source,
    })?;
    if committed != document {
        return Err(ExportError::CheckMismatch {
            path: path.to_path_buf(),
        });
    }
    println!("{} is up to date", path.display());
    Ok(())
}
