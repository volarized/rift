//! Schema document export tests.

use std::error::Error;

use rift_protocol::schema::schema_document;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn schema_document_is_deterministic() {
    let first = schema_document();
    let second = schema_document();
    assert_eq!(first, second, "two exports must render identical bytes");
}

#[test]
fn schema_document_ends_with_trailing_newline() {
    let document = schema_document();
    assert!(
        document.ends_with('\n'),
        "exported document must end with a newline"
    );
    assert!(
        !document.ends_with("\n\n"),
        "exported document must end with exactly one newline"
    );
}

#[test]
fn schema_document_parses_as_json() -> TestResult {
    let document: Value = serde_json::from_str(&schema_document())?;
    assert!(document.is_object(), "document root must be an object");
    assert_eq!(
        document.get("$schema").and_then(Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema")
    );
    Ok(())
}
