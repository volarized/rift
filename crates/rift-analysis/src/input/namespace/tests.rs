use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::Language;

use super::{ExactPackageInput, ExactPackageLimits, NamespaceInput, PackageSource};

#[test]
fn aggregate_byte_overflow_refuses_even_at_u64_bound() {
    assert_eq!(
        super::counted_bytes(u64::MAX - 1, 1, u64::MAX).expect("exact bound"),
        u64::MAX
    );
    assert!(super::counted_bytes(u64::MAX, 1, u64::MAX).is_err());
}

#[test]
fn local_and_named_context_preserve_captured_paths_and_metadata() {
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
    let files = [PackageSource::new(&path, "pub fn start() {}")];
    let metadata = [PackageSource::new(
        &metadata_path,
        "[package]\nname = 'beacon'",
    )];
    let bytes =
        u64::try_from(files[0].text().len() + metadata[0].text().len()).expect("captured bytes");
    for owner in [
        SymbolOwner::Local,
        SymbolOwner::NamedLocal {
            name: "cloud".to_owned(),
        },
    ] {
        let context = NamespaceInput::project(
            &owner,
            &language,
            &files,
            &metadata,
            &[],
            ExactPackageLimits::new(2, bytes),
        )
        .expect("captured local context");
        assert_eq!(context.owner(), &owner);
        assert_eq!(context.files()[0].path(), &path);
        assert_eq!(context.context_sources()[0].text(), metadata[0].text());
        assert!(context.files()[0].source_unit().is_none());
        assert_eq!(
            rift_syntax::source_unit_for_path(context.files()[0].path())
                .expect("physical project source")
                .to_string(),
            "rift://source/project/src/lib.rs",
        );
    }
}

#[test]
fn local_context_shares_source_count_bytes_and_duplicate_path_refusals() {
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let path = ProjectPath::new("lib.rs").expect("source path");
    let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
    let files = [PackageSource::new(&path, "x")];
    let metadata = [PackageSource::new(&metadata_path, "y")];
    assert!(
        NamespaceInput::project(
            &SymbolOwner::Local,
            &language,
            &files,
            &metadata,
            &[],
            ExactPackageLimits::new(1, 2),
        )
        .is_err()
    );
    assert!(
        NamespaceInput::project(
            &SymbolOwner::Local,
            &language,
            &files,
            &metadata,
            &[],
            ExactPackageLimits::new(2, 1),
        )
        .is_err()
    );
    assert!(
        NamespaceInput::project(
            &SymbolOwner::Local,
            &language,
            &files,
            &files,
            &[],
            ExactPackageLimits::new(2, 2),
        )
        .is_err()
    );
}

#[test]
fn workspace_source_count_preserves_usize_bounds_without_allocating_inventory() {
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let context = NamespaceInput::workspace(
        &owner,
        &language,
        &[],
        &[],
        &[],
        (usize::MAX, u64::MAX, rift_syntax::SyntaxLimits::default()),
    )
    .expect("workspace bounds");
    assert_eq!(context.files_max, usize::MAX);
    assert!(context.validate_count(usize::MAX).is_ok());
    for bounds in [
        (0, 1, rift_syntax::SyntaxLimits::default()),
        (1, 0, rift_syntax::SyntaxLimits::default()),
    ] {
        assert!(NamespaceInput::workspace(&owner, &language, &[], &[], &[], bounds).is_err());
    }
}

#[test]
fn local_context_does_not_change_released_owner_admission() {
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Project { package: None }),
        SourceKind::Authored,
    )
    .expect("project origin");
    let limits = ExactPackageLimits::new(0, 0);
    assert!(ExactPackageInput::new(&SymbolOwner::Local, &language, &origin, &[], limits).is_err());
    let package = SymbolOwner::Package {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    assert!(NamespaceInput::project(&package, &language, &[], &[], &[], limits).is_err());
    let invalid = SymbolOwner::NamedLocal {
        name: "all".to_owned(),
    };
    assert!(NamespaceInput::project(&invalid, &language, &[], &[], &[], limits).is_err());
}

#[test]
fn local_capture_uses_source_count_without_package_publication_capacity() {
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("rust").expect("Rust language");
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
    let files = [PackageSource::new(&path, "pub fn start() {}")];
    let metadata = [PackageSource::new(
        &metadata_path,
        "[package]\nname='beacon'",
    )];
    let limits = ExactPackageLimits::new(2, 128)
        .with_publication(rift_protocol::configuration::PackageConfiguration {
            units: 1,
            ..rift_protocol::configuration::PackageConfiguration::default()
        })
        .expect("package publication limit");
    assert!(NamespaceInput::project(&owner, &language, &files, &metadata, &[], limits).is_ok());
    let package = rift_protocol::read::PackageIdentity {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let package_owner = package.owner().expect("released package owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("released origin");
    let released = ExactPackageInput::new(&package_owner, &language, &origin, &files, limits)
        .expect("one source fits publication bound");
    assert!(released.with_framework_context(&metadata, &[]).is_err());
}

#[test]
fn module_observations_preserve_separate_conditions_and_reused_declarations() {
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("javascript").expect("JavaScript language");
    let esm_path = ProjectPath::new("index.mjs").expect("ESM path");
    let commonjs_path = ProjectPath::new("index.cjs").expect("CommonJS path");
    let declaration_path = ProjectPath::new("index.d.ts").expect("declaration path");
    let files = [
        PackageSource::new(&esm_path, "export function open() {}"),
        PackageSource::new(&commonjs_path, "exports.open = function open() {};"),
        PackageSource::new(&declaration_path, "export declare function open(): void;"),
    ];
    let declarations = [files[2]];
    let observations = [
        super::NamespaceModule::new("beacon", files[0], &declarations, &["import"]),
        super::NamespaceModule::new("beacon", files[1], &declarations, &["require"]),
    ];
    let context = NamespaceInput::project(
        &owner,
        &language,
        &files,
        &[],
        &[],
        ExactPackageLimits::new(4, 256),
    )
    .expect("captured sources")
    .with_modules(&observations)
    .expect("separate observations");
    assert_eq!(context.modules().len(), 2);
    assert_eq!(context.modules()[0].export_conditions(), &["import"]);
    assert_eq!(context.modules()[1].export_conditions(), &["require"]);
    assert_eq!(context.modules()[0].implementation().path(), &esm_path);
    assert_eq!(context.modules()[1].implementation().path(), &commonjs_path);
    assert_eq!(context.modules()[0].module(), "beacon");
    assert_eq!(
        context.modules()[0].declarations()[0].path(),
        &declaration_path
    );
    assert_eq!(
        context.modules()[1].declarations()[0].text(),
        files[2].text()
    );
}

#[test]
fn module_observations_refuse_unadmitted_or_conflicting_source_witnesses() {
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("javascript").expect("JavaScript language");
    let path = ProjectPath::new("index.js").expect("source path");
    let absent = ProjectPath::new("absent.js").expect("unadmitted path");
    let files = [PackageSource::new(&path, "export function open() {}")];
    let context = NamespaceInput::project(
        &owner,
        &language,
        &files,
        &[],
        &[],
        ExactPackageLimits::new(4, 256),
    )
    .expect("captured source");
    for source in [
        PackageSource::new(&absent, files[0].text()),
        PackageSource::new(&path, "export function changed() {}"),
    ] {
        let observations = [super::NamespaceModule::new("beacon", source, &[], &[])];
        assert!(context.with_modules(&observations).is_err());
    }
    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let unit = rift_core::SourceUnitId::for_owner(
        package.owner().expect("physical package owner"),
        path.as_str(),
    )
    .expect("physical package unit");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("physical package origin");
    let foreign = files[0]
        .with_source_unit(&unit, &origin)
        .expect("valid physical association");
    let observations = [super::NamespaceModule::new("beacon", foreign, &[], &[])];
    assert!(context.with_modules(&observations).is_err());
    let repeated = [files[0]];
    let observations = [super::NamespaceModule::new(
        "beacon",
        files[0],
        &repeated,
        &[],
    )];
    assert!(context.with_modules(&observations).is_err());
}

#[test]
fn module_observations_share_count_and_aggregate_byte_bounds() {
    let owner = SymbolOwner::Local;
    let language = Language::from_identity_segment("javascript").expect("JavaScript language");
    let path = ProjectPath::new("index.js").expect("source path");
    let files = [PackageSource::new(&path, "x")];
    let context = NamespaceInput::project(
        &owner,
        &language,
        &files,
        &[],
        &[],
        ExactPackageLimits::new(1, 64),
    )
    .expect("captured source");
    let observations = [super::NamespaceModule::new(
        "beacon",
        files[0],
        &[],
        &["import", "default"],
    )];
    assert!(context.with_modules(&observations).is_err());
    let observations = [super::NamespaceModule::new(
        "beacon",
        files[0],
        &[],
        &["import"],
    )];
    let narrow = NamespaceInput::project(
        &owner,
        &language,
        &files,
        &[],
        &[],
        ExactPackageLimits::new(1, 12),
    )
    .expect("source fits byte bound");
    assert!(narrow.with_modules(&observations).is_err());
    assert!(context.with_modules(&observations).is_ok());
    let observations = [super::NamespaceModule::new(
        "beacon",
        files[0],
        &[],
        &["import", "import"],
    )];
    assert!(context.with_modules(&observations).is_err());
}
