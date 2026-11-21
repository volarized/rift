//! Publication logging for suppressed framework interpretation.

use rift_core::{FileDigest, ProjectPath};
use rift_error::errors;
use rift_syntax::{ByteRange, SyntaxLimits, SyntaxSource};

use super::{FrameworkContext, ResolvedFramework};

#[test]
fn each_publication_logs_one_framework_refusal_per_affected_source() {
    let paths = ["first.ts", "second.ts"].map(|name| ProjectPath::new(name).expect("source path"));
    let text = "export const ordinary = 1;";
    let digest = FileDigest::of(text.as_bytes());
    let limits = SyntaxLimits::default();
    let parse = |source: SyntaxSource<'_>| {
        rift_syntax::ShippedLanguage::TypeScript
            .definition()
            .syntax_provider()
            .analyze(source, limits)
            .expect("ordinary syntax")
    };
    let framework = ResolvedFramework {
        templates: vec![ByteRange {
            start: 0,
            end: u64::MAX,
        }],
        ..ResolvedFramework::default()
    };
    let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
        .capture("warn")
        .install()
        .expect("recorder");

    let reason = errors::syntax::facts_range_invalid().error().to_string();
    for _ in 0..2 {
        let context = FrameworkContext {
            files: paths
                .iter()
                .cloned()
                .map(|path| (path, framework.clone()))
                .collect(),
            digests: paths.iter().cloned().map(|path| (path, digest)).collect(),
            ..FrameworkContext::default()
        };
        for path in &paths {
            let source = SyntaxSource { path, text };
            let ordinary = parse(source);
            let (published, warnings, calls) = context
                .apply_for_publication(source, limits, parse(source))
                .expect("publication keeps ordinary syntax");
            assert_eq!(published.facts(), ordinary.facts());
            assert_eq!(calls, 0);
            assert_eq!(warnings.len(), 1);
            let retained = context.for_source(path, &digest).expect("retained context");
            for _ in 0..2 {
                let reads = [
                    context.apply(source, limits, parse(source)),
                    retained.apply(source, limits, parse(source)),
                ];
                for read in reads {
                    let (read, read_warnings, calls) = read.expect("read keeps ordinary syntax");
                    assert_eq!(read.facts(), ordinary.facts());
                    assert_eq!(read_warnings, warnings);
                    assert_eq!(calls, 0);
                }
            }
        }

        let records = drain.queued_records();
        assert_eq!(records.len(), paths.len(), "{records:?}");
        for (record, path) in records.iter().zip(&paths) {
            assert_eq!(record.component(), "index");
            assert_eq!(record.operation(), "index.parse");
            assert_eq!(
                record.message(),
                "framework interpretation left out of a file: its syntax facts failed their checks"
            );
            let fields: serde_json::Value = serde_json::from_str(record.fields()).expect("fields");
            assert_eq!(fields["path"], path.as_str());
            assert_eq!(fields["reason"], reason);
        }
    }
}
