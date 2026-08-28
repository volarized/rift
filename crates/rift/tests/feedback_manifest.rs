//! Checks every v0.0.23 feedback report has a v0.0.24 disposition.

use std::collections::BTreeSet;

use serde::Deserialize;

#[derive(Deserialize)]
struct FeedbackManifest {
    required_record_fields: Vec<String>,
    reports: Vec<FeedbackReport>,
}

#[derive(Deserialize)]
struct FeedbackReport {
    heading: String,
    mapping: String,
    disposition: String,
    evidence: String,
}

#[test]
fn feedback_manifest_maps_all_v0_0_23_reports() {
    let manifest: FeedbackManifest =
        serde_json::from_str(include_str!("v0_0_23_feedback_manifest.json"))
            .expect("feedback manifest must be valid JSON");
    assert_eq!(
        manifest.required_record_fields,
        [
            "request_frame",
            "pre_tree",
            "post_tree",
            "result",
            "hook_runs",
            "tree_revision",
            "engine_trace",
            "package_version",
            "executable_digest",
            "schema_digest",
        ]
    );
    assert_eq!(manifest.reports.len(), 52);

    let mut headings = BTreeSet::new();
    for report in manifest.reports {
        assert!(
            headings.insert(report.heading),
            "feedback heading must be unique"
        );
        assert!(!report.mapping.is_empty());
        assert!(matches!(
            report.disposition.as_str(),
            "test" | "retention" | "configuration" | "external"
        ));
        assert!(!report.evidence.is_empty());
    }
}
