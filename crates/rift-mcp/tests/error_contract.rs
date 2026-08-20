//! Contract between the workspace error registry and the wire error surface.
//!
//! The registry in `rift-core` owns every stable code; the `ErrorCode` enum
//! in `rift-protocol` owns what the wire admits. The MCP layer is where the
//! two meet, so this suite holds them to one set of codes.

use std::collections::BTreeSet;

use rift_core::ErrorRegistry;
use rift_protocol::error::ErrorCode;
use schemars::schema_for;

#[test]
fn wire_codes_and_registry_codes_are_the_same_set() {
    let registry: BTreeSet<&str> = ErrorRegistry::entries()
        .iter()
        .map(|descriptor| descriptor.code())
        .collect();
    let schema = serde_json::to_value(schema_for!(ErrorCode)).expect("ErrorCode schema serializes");
    let wire: BTreeSet<String> = schema["oneOf"]
        .as_array()
        .expect("ErrorCode schema must list its values under oneOf")
        .iter()
        .map(|value| {
            value["const"]
                .as_str()
                .expect("wire codes are strings")
                .to_owned()
        })
        .collect();
    let registry: BTreeSet<String> = registry.iter().map(|code| (*code).to_owned()).collect();
    assert_eq!(
        registry, wire,
        "the registry and the wire ErrorCode enum must name the same codes; \
         add or remove the entry on both sides in the same change"
    );
}
