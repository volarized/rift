//! Writes Rift generated artifacts.

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let outcome = rift_schema_export::parse_arguments(env::args().skip(1))
        .and_then(|request| rift_schema_export::run(&request));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift-schema-export: {error}");
            ExitCode::FAILURE
        }
    }
}
