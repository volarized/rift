//! Canonical JSON: the one rendering a digest is taken over.
//!
//! Two publications of the same facts must hash alike on every supported platform, so
//! the bytes a digest covers are [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785)
//! canonical JSON: object members sorted by their UTF-16 code units, no insignificant
//! whitespace, and one shortest rendering per number. `serde_json_canonicalizer` is the
//! RFC's own scheme implemented over `serde`, so a model renders straight from its
//! `Serialize` implementation with no intermediate value.

use serde::Serialize;

/// Renders `value` as RFC 8785 canonical JSON.
///
/// # Errors
///
/// Returns the serializer's error when `value`'s `Serialize` implementation fails, or
/// when it holds a map whose keys are not strings.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json_canonicalizer::to_string(value)
}

#[cfg(test)]
mod tests {
    use super::canonical_json;
    use serde_json::json;

    /// The RFC's own worked example: members sort by their UTF-16 code units, not by
    /// their bytes, and no insignificant whitespace survives.
    #[test]
    fn test_canonical_json_sorts_members_and_drops_whitespace() {
        let value = json!({ "b": false, "c": 120, "a": "Hello!" });
        assert_eq!(
            canonical_json(&value).expect("a JSON value canonicalizes"),
            r#"{"a":"Hello!","b":false,"c":120}"#
        );
    }

    /// Nested objects sort at every depth, so a record's digest does not depend on the
    /// order its fields were built in.
    #[test]
    fn test_canonical_json_sorts_nested_members() {
        let value = json!({ "outer": { "z": 1, "a": { "y": 2, "b": 3 } } });
        assert_eq!(
            canonical_json(&value).expect("a JSON value canonicalizes"),
            r#"{"outer":{"a":{"b":3,"y":2},"z":1}}"#
        );
    }

    /// A non-ASCII string keeps its own characters: the scheme escapes what JSON must
    /// escape and nothing else.
    #[test]
    fn test_canonical_json_keeps_non_ascii_characters_literal() {
        let value = json!({ "name": "caf\u{e9}" });
        assert_eq!(
            canonical_json(&value).expect("a JSON value canonicalizes"),
            "{\"name\":\"caf\u{e9}\"}"
        );
    }
}
