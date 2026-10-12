use std::path::Path;

use super::*;

const DEFAULT: &str = "[build-system]\nrequires=['pdm-backend']\nbuild-backend='pdm.backend'\n";
const SOURCE: &str = "def application():\n    pass\n";
const SETUPTOOLS: &str = "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['main','helper']\n";

#[test]
fn selected_metadata_and_context_metadata_require_same_captured_root_proof() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", SETUPTOOLS);
    write(directory.path(), "main.py", SOURCE);
    write(directory.path(), "helper.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("metadata path");
    let main = ProjectPath::new("main.py").expect("main path");
    let helper = ProjectPath::new("helper.py").expect("helper path");
    let metadata = PackageSource::new(&metadata_path, SETUPTOOLS);
    let files = [
        PackageSource::new(&main, SOURCE),
        PackageSource::new(&helper, SOURCE),
    ];
    let paths = observations(directory.path(), metadata, &files);
    let owner = rift_protocol::identity::SymbolOwner::Local;
    let language = rift_syntax::ShippedLanguage::Python.language();
    let context = [metadata];
    let selected = [files[0], files[1], metadata];
    let mut expected = None;
    for (files, context) in [(&files[..], &context[..]), (&selected[..], &[][..])] {
        let input = NamespaceInput::workspace(
            &owner,
            &language,
            files,
            context,
            &[],
            (32, 16 * 1024 * 1024, SyntaxLimits::DEFAULT),
        )
        .expect("captured input");
        assert!(import_roots(&input).is_empty());
        let input = input
            .with_build_paths(&paths)
            .expect("captured path observations");
        let roots = import_roots(&input);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].modules(), ["main", "helper"]);
        assert_eq!(roots[0].origin(), PackageImportRootOrigin::PyModules);
        assert!(roots[0].prefix().is_none());
        let projected = super::super::python_module("main.py", &roots);
        assert_eq!(projected, Some(vec!["main".to_owned()]));
        if let Some(expected) = &expected {
            assert_eq!(&projected, expected);
        } else {
            expected = Some(projected);
        }
    }
}

#[test]
fn explicit_setuptools_modules_use_captured_files_without_directory_inventory() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", SETUPTOOLS);
    write(directory.path(), "main.py", SOURCE);
    write(directory.path(), "helper.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let main = ProjectPath::new("main.py").expect("path");
    let helper = ProjectPath::new("helper.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, SETUPTOOLS);
    let files = [
        PackageSource::new(&main, SOURCE),
        PackageSource::new(&helper, SOURCE),
    ];
    let paths = observations(directory.path(), metadata, &files);
    let root =
        captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("explicit modules");
    assert_eq!(root.modules(), ["main", "helper"]);
    assert_eq!(root.origin(), PackageImportRootOrigin::PyModules);
    assert!(root.prefix().is_none());
    assert_eq!(
        super::super::python_module("main.py", &[root]),
        Some(vec!["main".to_owned()])
    );
    assert!(!paths.contains_key(&ObservationPath("src".to_owned())));
    for missing in [
        "",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "main.py",
        "helper.py",
    ] {
        let mut incomplete = paths.clone();
        incomplete.remove(&ObservationPath(missing.to_owned()));
        assert!(
            captured_root(metadata, &files, SyntaxLimits::DEFAULT, &incomplete).is_none(),
            "{missing}"
        );
    }
    assert!(captured_root(metadata, &files[..1], SyntaxLimits::DEFAULT, &paths).is_none());
    for filename in ["setup.py", "setup.cfg"] {
        write(directory.path(), filename, "# custom configuration");
        let paths = observations(directory.path(), metadata, &files);
        assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
        std::fs::remove_file(directory.path().join(filename)).expect("remove configuration");
    }
}

#[test]
fn explicit_setuptools_package_directory_preserves_original_paths() {
    let directory = tempfile::tempdir().expect("directory");
    let text = format!("{SETUPTOOLS}package-dir={{''='src'}}\npackages=[]\n");
    write(directory.path(), "nested/pyproject.toml", &text);
    write(directory.path(), "nested/src/main.py", SOURCE);
    write(directory.path(), "nested/src/helper.py", SOURCE);
    let metadata_path = ProjectPath::new("nested/pyproject.toml").expect("path");
    let main = ProjectPath::new("nested/src/main.py").expect("path");
    let helper = ProjectPath::new("nested/src/helper.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, &text);
    let files = [
        PackageSource::new(&main, SOURCE),
        PackageSource::new(&helper, SOURCE),
    ];
    let paths = observations(directory.path(), metadata, &files);
    let root =
        captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("explicit directory");
    assert_eq!(root.prefix().expect("prefix").as_str(), "nested/src");
    assert_eq!(files[0].path().as_str(), "nested/src/main.py");
    assert_eq!(
        super::super::python_module(main.as_str(), &[root]),
        Some(vec!["main".to_owned()])
    );
}

#[test]
fn unsupported_setuptools_configuration_never_supplies_roots() {
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    for field in [
        "cmdclass={build_py='custom.Builder'}",
        "dynamic={version={attr='main.version'}}",
        "ext-modules=[]",
        "packages={find={where=['src']}}",
        "packages=['other']",
        "package-dir={'other'='src'}",
        "package-dir={''='../src'}",
    ] {
        let text = format!("{SETUPTOOLS}{field}\n");
        assert!(
            build_paths(
                PackageSource::new(&metadata_path, &text),
                &[],
                SyntaxLimits::DEFAULT
            )
            .is_none(),
            "{field}"
        );
    }
    for text in [
        SETUPTOOLS.replace("setuptools==80.9.0", "setuptools>=61"),
        SETUPTOOLS.replace(
            "requires=['setuptools==80.9.0']",
            "requires=['setuptools==80.9.0','plugin']",
        ),
        format!("{SETUPTOOLS}[project]\ndynamic=['version']\n"),
        SETUPTOOLS.replace(
            "build-backend='setuptools.build_meta'",
            "build-backend='setuptools.build_meta'\nbackend-path=['.']",
        ),
        SETUPTOOLS.replace("['main','helper']", "['main','main']"),
        SETUPTOOLS.replace("['main','helper']", "['nested.main']"),
    ] {
        assert!(
            build_paths(
                PackageSource::new(&metadata_path, &text),
                &[],
                SyntaxLimits::DEFAULT
            )
            .is_none()
        );
    }
}

#[test]
fn explicit_setuptools_module_count_refuses_one_over_bound() {
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let modules = (0..=crate::PACKAGE_IMPORT_ENTRIES_MAX)
        .map(|index| format!("'module_{index}'"))
        .collect::<Vec<_>>()
        .join(",");
    let text = SETUPTOOLS.replace("['main','helper']", &format!("[{modules}]"));
    assert!(
        build_paths(
            PackageSource::new(&metadata_path, &text),
            &[],
            SyntaxLimits::DEFAULT
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn symlink_setuptools_module_never_establishes_root() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", SETUPTOOLS);
    write(directory.path(), "main.py", SOURCE);
    write(directory.path(), "actual.py", SOURCE);
    std::os::unix::fs::symlink("actual.py", directory.path().join("helper.py"))
        .expect("module symlink");
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let main = ProjectPath::new("main.py").expect("path");
    let helper = ProjectPath::new("helper.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, SETUPTOOLS);
    let files = [
        PackageSource::new(&main, SOURCE),
        PackageSource::new(&helper, SOURCE),
    ];
    let paths = observations(directory.path(), metadata, &files);
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
}

fn write(root: &Path, path: &str, bytes: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("file parent")).expect("directories");
    std::fs::write(path, bytes).expect("source");
}

fn observations(
    root: &Path,
    metadata: PackageSource<'_>,
    files: &[PackageSource<'_>],
) -> BTreeMap<ObservationPath, Option<ArchiveMemberKind>> {
    build_paths(metadata, files, SyntaxLimits::DEFAULT)
        .expect("request paths")
        .into_iter()
        .filter_map(|path| {
            let kind = match std::fs::symlink_metadata(root.join(&path.0)) {
                Ok(metadata) if metadata.file_type().is_symlink() => Some(ArchiveMemberKind::Link),
                Ok(metadata) if metadata.is_dir() => Some(ArchiveMemberKind::Directory),
                Ok(metadata) if metadata.is_file() => Some(ArchiveMemberKind::File),
                Ok(_) => return None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("capture failed: {error}"),
            };
            Some((path, kind))
        })
        .collect()
}

#[test]
fn verified_missing_src_establishes_default_package_root() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "beacon/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    let root = captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("root");
    assert!(root.prefix().is_none());
    assert_eq!(root.modules(), ["beacon"]);
    assert_eq!(root.origin(), PackageImportRootOrigin::Pdm);
    assert_eq!(paths.get(&ObservationPath("src".to_owned())), Some(&None));
}

#[test]
fn empty_src_directory_blocks_root_package_selection() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "beacon/__init__.py", SOURCE);
    std::fs::create_dir(directory.path().join("src")).expect("empty src");
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    let config = config(metadata, SyntaxLimits::DEFAULT).expect("config");
    assert_eq!(root(&config, &paths).expect("selected src").as_str(), "src");
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
}

#[test]
fn retained_src_package_preserves_original_path() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "src/beacon/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("src/beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    let root = captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("root");
    assert_eq!(root.prefix().expect("prefix").as_str(), "src");
    assert_eq!(root.modules(), ["beacon"]);
    assert_eq!(files[0].path().as_str(), "src/beacon/__init__.py");
}

#[test]
fn missing_observations_never_prove_absence() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "beacon/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    for missing in [
        "",
        "pyproject.toml",
        "src",
        "pdm_build.py",
        "beacon",
        "beacon/__init__.py",
    ] {
        let mut incomplete = paths.clone();
        incomplete.remove(&ObservationPath(missing.to_owned()));
        assert!(
            captured_root(metadata, &files, SyntaxLimits::DEFAULT, &incomplete).is_none(),
            "{missing}"
        );
    }
}

#[test]
fn present_default_hook_and_custom_build_settings_refuse_roots() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "beacon/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("beacon/__init__.py").expect("path");
    let files = [PackageSource::new(&path, SOURCE)];
    for setting in [
        "custom-hook='build.py'",
        "run-setuptools=true",
        "includes=['beacon']",
        "excludes=['beacon']",
    ] {
        let text = format!("{DEFAULT}\n[tool.pdm.build]\n{setting}\n");
        write(directory.path(), "pyproject.toml", &text);
        let metadata = PackageSource::new(&metadata_path, &text);
        let paths = observations(directory.path(), metadata, &files);
        assert!(
            captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none(),
            "{setting}"
        );
    }
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(
        directory.path(),
        "pdm_build.py",
        "def pdm_build_initialize(context): pass\n",
    );
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let paths = observations(directory.path(), metadata, &files);
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
}

#[test]
fn metadata_parent_and_explicit_package_directory_use_captured_paths() {
    let directory = tempfile::tempdir().expect("directory");
    let text = format!("{DEFAULT}\n[tool.pdm.build]\npackage-dir='lib'\n");
    write(directory.path(), "nested/pyproject.toml", &text);
    write(directory.path(), "nested/lib/beacon/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("nested/pyproject.toml").expect("path");
    let path = ProjectPath::new("nested/lib/beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, &text);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    let root = captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("root");
    assert_eq!(root.prefix().expect("prefix").as_str(), "nested/lib");
    assert!(paths.contains_key(&ObservationPath("nested/pdm_build.py".to_owned())));
    assert!(!paths.contains_key(&ObservationPath("pdm_build.py".to_owned())));
}

#[test]
fn fastapi_source_includes_preserve_installed_module_name() {
    let directory = tempfile::tempdir().expect("directory");
    let text = format!(
        "{DEFAULT}\n[tool.pdm.build]\nsource-includes=['tests/','docs_src/','scripts/','docs/en/docs/img/favicon.png']\n"
    );
    write(directory.path(), "pyproject.toml", &text);
    write(directory.path(), "fastapi/__init__.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("fastapi/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, &text);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    let root = captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).expect("root");
    let wheel = PackageImportRoot::new(
        None,
        vec!["fastapi".to_owned()],
        PackageImportRootOrigin::Wheel,
    )
    .expect("wheel root");
    assert_eq!(
        super::super::python_module("fastapi/applications.py", &[root]),
        super::super::python_module("fastapi/applications.pyi", &[wheel])
    );
}

#[test]
fn module_only_layout_and_unsupported_metadata_remain_unresolved() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "beacon.py", SOURCE);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("beacon.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
    for text in [
        "[broken",
        "[build-system]\nbuild-backend='other'\nrequires=['other']",
        "[build-system]\nbuild-backend='pdm.backend'\nrequires=['pdm-backend','plugin']",
    ] {
        assert!(
            build_paths(
                PackageSource::new(&metadata_path, text),
                &files,
                SyntaxLimits::DEFAULT
            )
            .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_src_and_package_paths_do_not_establish_roots() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    write(directory.path(), "actual/beacon/__init__.py", SOURCE);
    std::os::unix::fs::symlink("actual", directory.path().join("src")).expect("symlink");
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let path = ProjectPath::new("src/beacon/__init__.py").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
    std::fs::remove_file(directory.path().join("src")).expect("remove symlink");
    std::os::unix::fs::symlink("actual/beacon", directory.path().join("beacon"))
        .expect("package symlink");
    let path = ProjectPath::new("beacon/__init__.py").expect("path");
    let files = [PackageSource::new(&path, SOURCE)];
    let paths = observations(directory.path(), metadata, &files);
    assert!(captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none());
}

#[test]
fn excluded_package_directories_do_not_establish_import_roots() {
    let directory = tempfile::tempdir().expect("directory");
    write(directory.path(), "pyproject.toml", DEFAULT);
    let metadata_path = ProjectPath::new("pyproject.toml").expect("path");
    let metadata = PackageSource::new(&metadata_path, DEFAULT);
    for name in ["__pycache__", "__pypackages__", "build"] {
        let path = ProjectPath::new(format!("{name}/__init__.py")).expect("path");
        write(directory.path(), path.as_str(), SOURCE);
        let files = [PackageSource::new(&path, SOURCE)];
        let paths = observations(directory.path(), metadata, &files);
        assert!(
            captured_root(metadata, &files, SyntaxLimits::DEFAULT, &paths).is_none(),
            "{name}"
        );
    }
}
