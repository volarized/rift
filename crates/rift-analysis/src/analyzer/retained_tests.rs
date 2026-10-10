use super::{Records, RetainedSourceLimits};
use crate::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::{index::PACKAGE_SOURCE_BYTES_MAX, read::Language};

#[test]
fn explicit_retention_keeps_complete_source_and_current_declaration_ranges() {
    let source = format!(
        "pub fn run() {{ /*{}*/ }}",
        "x".repeat(PACKAGE_SOURCE_BYTES_MAX as usize)
    );
    let defaults = ExactPackageLimits::new(1, source.len() as u64);
    let cut = analyze(&source, defaults).expect("default analysis");
    assert!(!cut.units[0].source_complete);
    assert_eq!(cut.units[0].source.len(), PACKAGE_SOURCE_BYTES_MAX as usize);
    assert!(!cut.warnings.is_empty());
    let complete = analyze(
        &source,
        defaults
            .with_retained_source_bytes(
                u32::try_from(source.len()).expect("fixture size"),
                (source.len() * 4) as u64,
            )
            .expect("retention bounds"),
    )
    .expect("complete analysis");
    assert!(complete.warnings.is_empty());
    assert!(complete.units[0].source_complete);
    assert_eq!(complete.units[0].source, source);
    assert_eq!(complete.units[0].unit, cut.units[0].unit);
    assert_eq!(
        complete.units[0].content_digest,
        cut.units[0].content_digest
    );
    assert_eq!(complete.symbols.len(), 1);
    assert_eq!(complete.symbols[0].symbol, cut.symbols[0].symbol);
    assert_eq!(complete.symbols[0].range, cut.symbols[0].range);
    assert!(complete.symbols[0].source_complete);
    assert_eq!(complete.symbols[0].source, source);
    assert_eq!(
        complete
            .documents
            .iter()
            .find_map(|doc| doc.file_content.as_ref()),
        Some(&source)
    );
    assert_eq!(
        complete
            .documents
            .iter()
            .find_map(|doc| doc.declaration_source.as_ref()),
        Some(&source)
    );
    let schema: serde_json::Value =
        serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
            .expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("schema validator");
    let instance = serde_json::to_value(&complete).expect("publication JSON");
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "complete source publication must validate: {errors:?}"
    );
}

#[test]
fn retention_limits_refuse_zero_order_and_supported_ceiling() {
    for (record, total) in [
        (0, 1),
        (1, 0),
        (2, 1),
        (
            rift_protocol::index::PACKAGE_SOURCE_BYTES_CEILING + 1,
            u64::MAX,
        ),
    ] {
        let error = ExactPackageLimits::new(1, 1)
            .with_retained_source_bytes(record, total)
            .expect_err("invalid limits");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_retained_source_limits_invalid"
        );
    }
    ExactPackageLimits::new(1, 1)
        .with_retained_source_bytes(rift_protocol::index::PACKAGE_SOURCE_BYTES_CEILING, u64::MAX)
        .expect("supported record ceiling");
}

#[test]
fn retained_copies_are_charged_before_allocation_and_refusal_preserves_usage() {
    let mut records = Records {
        retained_source: Some(RetainedSourceLimits {
            record_bytes_max: 4,
            total_bytes_max: 8,
        }),
        ..Records::default()
    };
    let path = rift_protocol::read::ProjectPath("src/lib.rs".to_owned());
    assert_eq!(
        records
            .retained("abcd", &path, 2)
            .expect("exact limit")
            .text,
        "abcd"
    );
    let error = records.retained("x", &path, 1).err().expect("one over");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_retained_source_bytes_exceeded"
    );
    assert_eq!(records.retained_source_bytes, 8);
    assert_eq!(
        error
            .context()
            .find(|(key, _)| *key == "observed")
            .map(|(_, value)| value),
        Some("9".to_owned())
    );
}

#[test]
fn utf8_cut_and_overflow_refusal_keep_full_byte_accounting() {
    let path = rift_protocol::read::ProjectPath("src/lib.rs".to_owned());
    let mut records = Records {
        retained_source: Some(RetainedSourceLimits {
            record_bytes_max: 3,
            total_bytes_max: 4,
        }),
        ..Records::default()
    };
    let retained = records.retained("éé", &path, 2).expect("bounded UTF-8");
    assert_eq!(retained.text, "é");
    assert!(!retained.complete);
    assert_eq!(records.retained_source_bytes, 4);
    records.retained_source_bytes = u64::MAX;
    assert!(records.retained("x", &path, 1).is_err());
    assert_eq!(records.retained_source_bytes, u64::MAX);
}

#[test]
fn publication_retention_counts_file_documents_and_public_declaration_copies() {
    for (source, copies) in [("fn run() {}", 3), ("pub fn run() {}", 4)] {
        let source_bytes = source.len() as u64;
        let maximum = source_bytes * copies;
        let defaults = ExactPackageLimits::new(1, source_bytes);
        let accepted_limits = defaults
            .with_retained_source_bytes(u32::try_from(source.len()).expect("fixture size"), maximum)
            .expect("exact retention total");
        let accepted = analyze(source, accepted_limits).expect("exact total publication");
        let retained_bytes = accepted
            .units
            .iter()
            .map(|unit| unit.source.len())
            .sum::<usize>()
            + accepted
                .symbols
                .iter()
                .map(|symbol| symbol.source.len())
                .sum::<usize>()
            + accepted
                .documents
                .iter()
                .map(|document| {
                    document.file_content.as_ref().map_or(0, String::len)
                        + document.declaration_source.as_ref().map_or(0, String::len)
                })
                .sum::<usize>();
        assert_eq!(retained_bytes as u64, maximum);
        let refused_limits = defaults
            .with_retained_source_bytes(
                u32::try_from(source.len()).expect("fixture size"),
                maximum - 1,
            )
            .expect("one below exact retention total");
        let error = analyze(source, refused_limits).expect_err("retention total exceeded");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_retained_source_bytes_exceeded"
        );
    }
}

fn analyze(
    source: &str,
    limits: ExactPackageLimits,
) -> Result<rift_protocol::index::PackagePublication, rift_error::RiftError> {
    let package = super::fixture::identity();
    let owner = package.owner().expect("fixture owner");
    let language = Language {
        name: "rust".to_owned(),
        dialect: None,
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )?;
    let path = ProjectPath::new("src/lib.rs")?;
    let files = [PackageSource::new(&path, source)];
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)?;
    Ok(PackageAnalyzer::analyze(input, 1)?.into_parts().0)
}
