//! Writes the exported protocol schema document to disk.
//!
//! `rift-schema-export [--check] [OUTPUT_DIR]` renders
//! [`rift_protocol::schema::schema_document`] into `OUTPUT_DIR/mcp.json`
//! (default `docs/protocol`). With `--check` it compares instead of writing,
//! so CI can prove the committed document matches the models.

use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use rift_protocol::schema;

/// Directory the schema document lands in when no argument names one.
const OUTPUT_DIR_DEFAULT: &str = "docs/protocol";

/// File name of the exported schema document inside the output directory.
const SCHEMA_FILE_NAME: &str = "mcp.json";

/// Usage line appended to argument errors.
const USAGE: &str = "usage: rift-schema-export [--check] [OUTPUT_DIR]";

/// Command to run when the committed document is out of date.
const REGENERATE_COMMAND: &str = "cargo run -p rift-protocol --bin rift-schema-export";

/// Why an export run could not complete.
#[derive(Debug)]
enum ExportError {
    /// An argument started with `-` but is not a supported flag.
    UnknownFlag { argument: String },
    /// A second positional argument followed the output directory.
    ExtraArgument { argument: String },
    /// The document to check against could not be read.
    CheckUnreadable { path: PathBuf, source: io::Error },
    /// The committed document differs from the current models.
    CheckMismatch { path: PathBuf },
    /// The output directory or document could not be written.
    WriteFailed { path: PathBuf, source: io::Error },
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
                "`{path}` does not match the current protocol models; regenerate it with \
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

/// One parsed export invocation.
struct ExportRequest {
    /// Compare the existing document instead of writing it.
    check: bool,
    /// Directory holding the schema document.
    output_dir: PathBuf,
}

fn parse_arguments<Arguments>(arguments: Arguments) -> Result<ExportRequest, ExportError>
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

fn run(request: &ExportRequest) -> Result<(), ExportError> {
    let document = schema::schema_document();
    let document_path = request.output_dir.join(SCHEMA_FILE_NAME);
    if request.check {
        let committed =
            fs::read_to_string(&document_path).map_err(|source| ExportError::CheckUnreadable {
                path: document_path.clone(),
                source,
            })?;
        if committed != document {
            return Err(ExportError::CheckMismatch {
                path: document_path,
            });
        }
        println!("{} is up to date", document_path.display());
        return Ok(());
    }
    fs::create_dir_all(&request.output_dir).map_err(|source| ExportError::WriteFailed {
        path: request.output_dir.clone(),
        source,
    })?;
    fs::write(&document_path, document).map_err(|source| ExportError::WriteFailed {
        path: document_path.clone(),
        source,
    })?;
    println!("wrote {}", document_path.display());
    Ok(())
}

fn main() -> ExitCode {
    let outcome = parse_arguments(env::args().skip(1)).and_then(|request| run(&request));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift-schema-export: {error}");
            ExitCode::FAILURE
        }
    }
}
