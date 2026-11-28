//! Wire models for the `feedback` tool.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

const TEXT_BYTES_MAX: usize = 4096;

/// One diagnostic field the caller elects to send with a feedback report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackField {
    /// Sends the operating system name, such as `linux`.
    Os,
    /// Sends Rift's package version, such as `0.0.78`.
    RiftVersion,
}

/// The report and any diagnostic fields the caller elects to send.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.79"))]
#[schemars(example = serde_json::json!({
    "text": "The facets tool returned an empty result for a symbol it found.",
    "include": ["os", "rift_version"]
}))]
pub struct FeedbackParams {
    /// Report text sent verbatim. The caller MUST send nonblank text of at most 4096 UTF-8 bytes.
    #[serde(deserialize_with = "report_text")]
    #[schemars(length(min = 1, max = TEXT_BYTES_MAX), regex(pattern = r"\S"))]
    pub text: String,
    /// Diagnostic fields to send alongside `text`. Omitted sends no diagnostic field.
    #[serde(default, deserialize_with = "crate::mcp_projection::deserialize_set")]
    pub include: BTreeSet<FeedbackField>,
}

fn report_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.len() > TEXT_BYTES_MAX || text.trim().is_empty() {
        return Err(de::Error::custom(
            "expected nonblank text of at most 4096 UTF-8 bytes",
        ));
    }
    Ok(text)
}

/// Confirmation from the feedback intake.
#[derive(Debug, JsonSchema, Serialize)]
#[schemars(example = serde_json::json!({"submitted": true}))]
pub struct FeedbackResult {
    /// The intake confirmed that it stored the report.
    #[schemars(extend("const" = true))]
    pub submitted: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn feedback_arguments_apply_byte_bounds_and_typed_include() {
        for rejected in [
            json!({"text": ""}),
            json!({"text": " \n\t "}),
            json!({"text": "λ".repeat(TEXT_BYTES_MAX / 2 + 1)}),
            json!({"text": "valid", "include": null}),
            json!({"text": "valid", "include": ["os", "os"]}),
            json!({"text": "valid", "include": ["os", "host"]}),
            json!({"text": "valid", "include": {"os": true}}),
            json!({"text": "valid", "include_details": ["os"]}),
            json!({"text": "valid", "url": "https://example.test"}),
        ] {
            assert!(serde_json::from_value::<FeedbackParams>(rejected).is_err());
        }
        let text = "λ".repeat(TEXT_BYTES_MAX / 2);
        let largest =
            serde_json::from_value::<FeedbackParams>(json!({"text": text})).expect("at limit");
        assert_eq!(largest.text, text);
        assert!(largest.include.is_empty());

        let selected = serde_json::from_value::<FeedbackParams>(json!({
            "text": "valid", "include": ["rift_version", "os"]
        }))
        .expect("both diagnostic fields are accepted");
        assert_eq!(
            selected.include,
            BTreeSet::from([FeedbackField::Os, FeedbackField::RiftVersion])
        );
    }
}
