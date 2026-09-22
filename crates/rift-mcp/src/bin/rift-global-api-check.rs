//! Validates the published global package API contract.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = Path::new(rift_mcp::global_api::CONTRACT_PATH);
    match rift_mcp::global_api::validate(path) {
        Ok(()) => {
            println!("{} is valid", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rift-global-api-check: {error}");
            ExitCode::FAILURE
        }
    }
}
