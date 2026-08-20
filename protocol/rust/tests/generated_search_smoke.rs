//! Search DTO generation smoke test.

use rift_protocol::generated::read::{SearchParams, SearchResult};
use schemars::schema_for;

#[test]
fn search_roots_generate_executable_schemas() {
    let params = schema_for!(SearchParams);
    let result = schema_for!(SearchResult);
    assert!(params.get("properties").is_some());
    assert!(result.get("properties").is_some());
}
