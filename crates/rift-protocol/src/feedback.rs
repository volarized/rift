//! Wire models for the `feedback` tool.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

const TEXT_BYTES_MAX: usize = 4096;
const DETAILS_MAX: usize = 2;

/// One optional detail the caller may send with a feedback report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDetail {
    /// Operating system name, such as `linux`.
    Os,
    /// Rift's package version, such as `0.0.78`.
    RiftVersion,
}

/// The report and any diagnostic fields the caller elects to send.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.79"))]
#[schemars(example = serde_json::json!({
    "text": "The nodes tool returned an empty result for a symbol it found.",
    "include_details": ["os", "rift_version"]
}))]
pub struct FeedbackParams {
    /// Report text sent verbatim. The caller MUST send nonblank text of at most 4096 UTF-8 bytes.
    #[serde(deserialize_with = "report_text")]
    #[schemars(length(min = 1, max = TEXT_BYTES_MAX), regex(pattern = r"\S"))]
    pub text: String,
    /// Diagnostic fields to send alongside `text`. Omitted or empty sends no diagnostic fields.
    #[serde(default, deserialize_with = "selected_details")]
    #[schemars(length(max = DETAILS_MAX), extend("uniqueItems" = true))]
    pub include_details: Vec<FeedbackDetail>,
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

fn selected_details<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<FeedbackDetail>, D::Error> {
    let details = Vec::<FeedbackDetail>::deserialize(deserializer)?;
    if details.len() > DETAILS_MAX
        || matches!(details.as_slice(), [first, second] if first == second)
    {
        return Err(de::Error::custom(
            "expected up to two distinct feedback details",
        ));
    }
    Ok(details)
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
    fn feedback_arguments_apply_byte_and_detail_bounds() {
        for rejected in [
            json!({"text": ""}),
            json!({"text": " \n\t "}),
            json!({"text": "λ".repeat(TEXT_BYTES_MAX / 2 + 1)}),
            json!({"text": "valid", "include_details": null}),
            json!({"text": "valid", "include_details": ["os", "os"]}),
            json!({"text": "valid", "include_details": ["os", "rift_version", "os"]}),
            json!({"text": "valid", "include_details": ["host"]}),
            json!({"text": "valid", "url": "https://example.test"}),
        ] {
            assert!(serde_json::from_value::<FeedbackParams>(rejected).is_err());
        }
        let text = "λ".repeat(TEXT_BYTES_MAX / 2);
        let largest =
            serde_json::from_value::<FeedbackParams>(json!({"text": text})).expect("at limit");
        assert_eq!(largest.text, text);
        assert!(largest.include_details.is_empty());
    }
}
