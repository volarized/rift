//! Synchronous entry for the current executable's query children.

use std::path::Path;
use std::process::ExitCode;

pub(super) fn run(
    request: &Path,
    response: &Path,
    bytes_max: usize,
    heap_bytes_max: usize,
) -> ExitCode {
    if super::allocator::limit(heap_bytes_max).is_err() {
        eprintln!("rift: query child heap limit is below its initial allocations");
        return ExitCode::FAILURE;
    }
    match rift_server::run_query_child(request, response, bytes_max) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift: {error}");
            ExitCode::FAILURE
        }
    }
}
