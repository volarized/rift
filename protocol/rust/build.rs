//! Generates Rust protocol types from canonical JSON Schema.

mod schema_codegen;

use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=../mcp.json");

    let schema = fs::read_to_string("../mcp.json")?;
    let output_directory = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let generated = schema_codegen::generate(&schema)?;
    fs::write(output_directory.join("read.rs"), generated.read)?;
    fs::write(output_directory.join("contracts.rs"), generated.contracts)?;
    Ok(())
}
