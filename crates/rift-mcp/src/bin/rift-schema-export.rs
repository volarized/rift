//! Writes the served MCP tool surface and the `rift.toml` schema to disk.
//!
//! `rift-schema-export [--check] [OUTPUT_DIR]` renders
//! [`rift_mcp::schema::schema_document`] into `OUTPUT_DIR/mcp.json` and
//! [`rift_mcp::schema::configuration_schema_document`] into
//! `OUTPUT_DIR/rift.schema.json` (default `docs/protocol`). With `--check`
//! it compares instead of writing, so CI can prove the committed documents
//! match what the server derives.

use std::env;
use std::process::ExitCode;

use rift_mcp::schema;

fn main() -> ExitCode {
    let outcome =
        schema::parse_arguments(env::args().skip(1)).and_then(|request| schema::run(&request));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift-schema-export: {error}");
            ExitCode::FAILURE
        }
    }
}
