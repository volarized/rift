use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::identity::SourceDigest;
use rift_protocol::index::PackageArtifact;
use rift_protocol::read::{Language, PackageIdentity};

use super::{
    PACKAGE_IMPORT_BYTES_MAX, PACKAGE_IMPORT_ENTRIES_MAX, PackageImportRoot,
    PackageImportRootOrigin, validate_roots,
};
use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};

fn root(prefix: Option<&str>, modules: &[&str]) -> PackageImportRoot {
    PackageImportRoot::new(
        prefix.map(|path| ProjectPath::new(path).expect("prefix")),
        modules.iter().map(|module| (*module).to_owned()).collect(),
        PackageImportRootOrigin::Flit,
    )
    .expect("import root")
}

#[test]
fn roots_preserve_original_paths_and_selected_artifact() {
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "click".to_owned(),
        version: "8.3.3".to_owned(),
    };
    let owner = package.owner().expect("fixture owner");
    let language = Language::from_identity_segment("python").expect("language");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("origin");
    let path = ProjectPath::new("src/click/core.py").expect("source path");
    let files = [PackageSource::new(&path, "def command(): pass")];
    let roots = [root(Some("src"), &["click"])];
    let artifact = PackageArtifact::new(
        "click-8.3.3.tar.gz",
        SourceDigest::parse(&"a".repeat(64)).expect("full digest"),
        &[],
    )
    .expect("source artifact");
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 64),
    )
    .expect("input");
    assert!(input.import_roots().is_empty());
    assert!(input.artifact().is_none());
    let input = input
        .with_import_roots(&roots)
        .expect("bounded roots")
        .with_artifact(&artifact);
    assert_eq!(input.files()[0].path().as_str(), "src/click/core.py");
    assert_eq!(input.import_roots()[0].prefix(), roots[0].prefix());
    assert_eq!(input.import_roots()[0].modules(), ["click"]);
    assert_eq!(
        input.import_roots()[0].origin(),
        PackageImportRootOrigin::Flit
    );
    assert_eq!(input.artifact(), Some(&artifact));
}

#[test]
fn module_grammar_and_unique_prefixes_are_validated() {
    let unicode = root(None, &["α.module", "six"]);
    assert!(unicode.prefix().is_none());
    for modules in [
        vec![],
        vec![""],
        vec!["a..b"],
        vec!["a-b"],
        vec!["9a"],
        vec!["a", "a"],
    ] {
        assert!(
            PackageImportRoot::new(
                None,
                modules.into_iter().map(str::to_owned).collect(),
                PackageImportRootOrigin::PyModules,
            )
            .is_err()
        );
    }
    assert!(
        PackageImportRoot::new(
            Some(ProjectPath::new("").expect("root path")),
            vec!["six".to_owned()],
            PackageImportRootOrigin::Wheel,
        )
        .is_err()
    );
    assert!(validate_roots(&[root(None, &["six"]), root(None, &["click"])]).is_err());
    assert!(validate_roots(&[]).is_ok());
}

#[test]
fn module_and_root_counts_stop_at_the_aggregate_bound() {
    let modules = (0..PACKAGE_IMPORT_ENTRIES_MAX)
        .map(|index| format!("m{index}"))
        .collect::<Vec<_>>();
    let exact = PackageImportRoot::new(None, modules.clone(), PackageImportRootOrigin::Wheel)
        .expect("64 module entries");
    assert!(validate_roots(std::slice::from_ref(&exact)).is_ok());
    let mut overflow = modules;
    overflow.push("extra".to_owned());
    assert!(PackageImportRoot::new(None, overflow, PackageImportRootOrigin::Wheel).is_err());
    assert!(validate_roots(&[exact, root(Some("src"), &["extra"])]).is_err());
    let mut roots = (0..PACKAGE_IMPORT_ENTRIES_MAX)
        .map(|index| root(Some(&format!("root{index}")), &["module"]))
        .collect::<Vec<_>>();
    assert!(validate_roots(&roots).is_ok());
    roots.push(root(Some("overflow"), &["module"]));
    assert!(validate_roots(&roots).is_err());
}

#[test]
fn aggregate_bytes_are_checked_before_retaining_roots() {
    let exact = PackageImportRoot::new(
        None,
        vec!["x".repeat(PACKAGE_IMPORT_BYTES_MAX)],
        PackageImportRootOrigin::PyModules,
    )
    .expect("exact byte bound");
    assert_eq!(exact.bytes(), PACKAGE_IMPORT_BYTES_MAX);
    assert!(validate_roots(std::slice::from_ref(&exact)).is_ok());
    assert!(validate_roots(&[exact, root(Some("src"), &["a"])]).is_err());
    assert!(
        PackageImportRoot::new(
            None,
            vec!["x".repeat(PACKAGE_IMPORT_BYTES_MAX + 1)],
            PackageImportRootOrigin::PyModules,
        )
        .is_err()
    );
}
