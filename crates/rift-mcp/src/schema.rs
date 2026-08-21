//! Deterministic export of the served MCP tool surface.
//!
//! [`schema_document`] serializes the same tool router [`RiftMcp`] runs, so
//! the exported document and the served surface come from one definition:
//! tool names and descriptions from the `#[tool]` methods, request and
//! response schemas derived from the `rift_protocol` wire models.
//! [`configuration_schema_document`] derives the `rift.toml` schema from the
//! same crate's configuration model. The `rift-schema-export` binary writes
//! both documents into the docs; the rest of this module is the binary's
//! logic, kept here so it is testable.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use rift_core::{ErrorCode, ErrorDescriptor, ErrorName};
use serde_json::json;

use crate::RiftMcp;

/// One-line summary rendered at the top of the exported document.
const DOCUMENT_DESCRIPTION: &str =
    "Tools served by the Rift MCP server, with the JSON Schemas derived from the Rust wire models.";

/// Directory the export lands in when no argument names one; each document
/// carries its own path below it.
const OUTPUT_DIR_DEFAULT: &str = "docs";

/// Path of the exported tool-surface document below the output directory.
const SCHEMA_DOCUMENT_PATH: &str = "protocol/mcp.json";

/// Path of the exported `rift.toml` schema below the output directory. It
/// lands in the docs site's static files, so readers fetch it from the
/// site's own origin rather than the repository host.
const CONFIGURATION_SCHEMA_PATH: &str = "public/rift.schema.json";

/// Usage line appended to argument errors.
const USAGE: &str = "usage: rift-schema-export [--check] [OUTPUT_DIR]";

/// Command to run when the committed document is out of date.
const REGENERATE_COMMAND: &str = "cargo run -p rift-mcp --bin rift-schema-export";

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
    let mut rendered = format!("{document:#}");
    rendered.push('\n');
    rendered
}

/// Renders the JSON Schema of `rift.toml`, derived from
/// [`WorkspaceConfiguration`](rift_protocol::configuration::WorkspaceConfiguration),
/// so an editor can validate the file and refuse unknown keys before the
/// server does.
///
/// Output is pretty-printed, ends with a trailing newline, and is
/// byte-identical across calls because `serde_json` stores objects as sorted
/// maps.
#[must_use]
pub fn configuration_schema_document() -> String {
    let schema = schemars::schema_for!(rift_protocol::configuration::WorkspaceConfiguration);
    let document = serde_json::to_value(&schema)
        .unwrap_or_else(|error| unreachable!("schemas serialize to JSON values: {error}"));
    let mut rendered = format!("{document:#}");
    rendered.push('\n');
    rendered
}

/// Why an export run could not complete.
#[derive(Debug)]
pub enum ExportError {
    /// An argument started with `-` but is not a supported flag.
    UnknownFlag {
        /// The argument as given.
        argument: String,
    },
    /// A second positional argument followed the output directory.
    ExtraArgument {
        /// The argument as given.
        argument: String,
    },
    /// The document to check against could not be read.
    CheckUnreadable {
        /// The document that should have been readable.
        path: PathBuf,
        /// What reading it returned.
        source: io::Error,
    },
    /// The committed document differs from the served tool surface.
    CheckMismatch {
        /// The document that is out of date.
        path: PathBuf,
    },
    /// The output directory or document could not be written.
    WriteFailed {
        /// The directory or document that could not be written.
        path: PathBuf,
        /// What writing it returned.
        source: io::Error,
    },
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlag { argument } => write!(
                formatter,
                "unknown argument `{argument}`: expected `--check` or an output directory; \
                 {USAGE}"
            ),
            Self::ExtraArgument { argument } => write!(
                formatter,
                "unexpected extra argument `{argument}`: expected at most one output \
                 directory; {USAGE}"
            ),
            Self::CheckUnreadable { path, source } => write!(
                formatter,
                "cannot read `{path}` to check it: {source}; generate the document first \
                 with `{REGENERATE_COMMAND}`",
                path = path.display()
            ),
            Self::CheckMismatch { path } => write!(
                formatter,
                "`{path}` does not match the served tool surface; regenerate it with \
                 `{REGENERATE_COMMAND}`",
                path = path.display()
            ),
            Self::WriteFailed { path, source } => write!(
                formatter,
                "cannot write `{path}`: {source}; ensure the output directory is writable \
                 or pass another one ({USAGE})",
                path = path.display()
            ),
        }
    }
}

impl Error for ExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CheckUnreadable { source, .. } | Self::WriteFailed { source, .. } => Some(source),
            Self::UnknownFlag { .. } | Self::ExtraArgument { .. } | Self::CheckMismatch { .. } => {
                None
            }
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
            Self::CheckUnreadable { .. } | Self::WriteFailed { .. } => {
                ErrorName::Wire(ErrorCode::StorageFailure).descriptor()
            }
            Self::CheckMismatch { .. } => {
                ErrorName::Cli(rift_core::CliCode::ArtifactStale).descriptor()
            }
        }
    }
}

/// One parsed export invocation.
#[derive(Debug)]
pub struct ExportRequest {
    /// Compare the existing document instead of writing it.
    check: bool,
    /// Directory holding the schema document.
    output_dir: PathBuf,
}

/// Parses `rift-schema-export` arguments, without the program name.
///
/// # Errors
///
/// Returns [`ExportError`] for an unknown flag or a second output directory.
pub fn parse_arguments<Arguments>(arguments: Arguments) -> Result<ExportRequest, ExportError>
where
    Arguments: IntoIterator<Item = String>,
{
    let mut check = false;
    let mut output_dir: Option<PathBuf> = None;
    for argument in arguments {
        if argument == "--check" {
            check = true;
        } else if argument.starts_with('-') {
            return Err(ExportError::UnknownFlag { argument });
        } else if output_dir.is_some() {
            return Err(ExportError::ExtraArgument { argument });
        } else {
            output_dir = Some(PathBuf::from(argument));
        }
    }
    Ok(ExportRequest {
        check,
        output_dir: output_dir.unwrap_or_else(|| PathBuf::from(OUTPUT_DIR_DEFAULT)),
    })
}

/// Writes both schema documents, or with `--check` proves the committed
/// copies match what the server derives.
///
/// # Errors
///
/// Returns [`ExportError`] when a document cannot be read, differs, or
/// cannot be written.
pub fn run(request: &ExportRequest) -> Result<(), ExportError> {
    let documents = [
        (SCHEMA_DOCUMENT_PATH, schema_document()),
        (CONFIGURATION_SCHEMA_PATH, configuration_schema_document()),
    ];
    if request.check {
        for (relative_path, document) in &documents {
            check_document(&request.output_dir.join(relative_path), document)?;
        }
        return Ok(());
    }
    for (relative_path, document) in &documents {
        let document_path = request.output_dir.join(relative_path);
        let parent = document_path
            .parent()
            .unwrap_or(request.output_dir.as_path());
        fs::create_dir_all(parent).map_err(|source| ExportError::WriteFailed {
            path: parent.to_path_buf(),
            source,
        })?;
        fs::write(&document_path, document).map_err(|source| ExportError::WriteFailed {
            path: document_path.clone(),
            source,
        })?;
        println!("wrote {}", document_path.display());
    }
    Ok(())
}

/// Proves one committed document matches its derived content.
fn check_document(document_path: &std::path::Path, document: &str) -> Result<(), ExportError> {
    let committed =
        fs::read_to_string(document_path).map_err(|source| ExportError::CheckUnreadable {
            path: document_path.to_path_buf(),
            source,
        })?;
    if committed != *document {
        return Err(ExportError::CheckMismatch {
            path: document_path.to_path_buf(),
        });
    }
    println!("{} is up to date", document_path.display());
    Ok(())
}
