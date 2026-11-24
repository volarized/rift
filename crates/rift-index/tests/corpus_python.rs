//! Static collection floors for pinned Python packages.
//!
//! Each case reads one released package checkout named by its environment variable, builds the
//! index over the package sources and `pyproject.toml`, and asserts the declaration and distinct
//! edge-pair counts this collector reached on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
use rift_protocol::read::EdgeKind;

struct Package {
    variable: &'static str,
    version_file: &'static str,
    version_line: &'static str,
    include: &'static str,
    files: usize,
    declarations: &'static [(&'static str, usize)],
    edges: &'static [(EdgeKind, usize)],
}

fn check(package: &Package) -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os(package.variable).ok_or("missing corpus")?);
    let version = std::fs::read_to_string(root.join(package.version_file))?;
    assert!(
        version
            .lines()
            .any(|line| line.starts_with(package.version_line)),
        "{} does not name the pinned release",
        package.version_file
    );
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec![package.include.into(), "pyproject.toml".into()],
            vec![],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    let mut declarations = BTreeMap::<_, usize>::new();
    let mut files = 0;
    for file in index
        .files()
        .filter(|file| file.syntax().language().name == "python")
    {
        files += 1;
        for symbol in file.syntax().symbols() {
            *declarations.entry(symbol.kind).or_default() += 1;
        }
    }
    let mut pairs = BTreeMap::<_, BTreeSet<_>>::new();
    for edge in index.edges().edges() {
        pairs
            .entry(edge.kind())
            .or_default()
            .insert((edge.from(), edge.to()));
    }
    let counts = pairs
        .iter()
        .map(|(kind, pairs)| (*kind, pairs.len()))
        .collect::<BTreeMap<_, _>>();
    eprintln!("files {files}; declarations {declarations:?}; edge pairs {counts:?}");
    assert_eq!(files, package.files);
    for (kind, floor) in package.declarations {
        let count = declarations.get(kind).copied().unwrap_or_default();
        assert!(count >= *floor, "{kind}: {count} < {floor}");
    }
    for (kind, floor) in package.edges {
        let count = counts.get(kind).copied().unwrap_or_default();
        assert!(count >= *floor, "{kind:?}: {count} < {floor}");
    }
    assert!(index.edges().is_complete());
    assert_eq!(index.edges().dropped_edges(), 0);
    Ok(())
}

#[test]
#[ignore = "requires httpx 0.28.1 in RIFT_CORPUS_HTTPX"]
fn test_httpx_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_HTTPX",
        version_file: "httpx/__version__.py",
        version_line: "__version__ = \"0.28.1\"",
        include: "httpx/**/*.py",
        files: 23,
        declarations: &[
            ("class", 87),
            ("function", 73),
            ("method", 373),
            ("module", 23),
            ("variable", 942),
        ],
        edges: &[
            (EdgeKind::Calls, 842),
            (EdgeKind::Contains, 1284),
            (EdgeKind::Extends, 56),
            (EdgeKind::HasType, 620),
            (EdgeKind::Imports, 204),
            (EdgeKind::Overrides, 55),
            (EdgeKind::Reads, 688),
            (EdgeKind::Writes, 192),
        ],
    })
}

#[test]
#[ignore = "requires fastapi 0.163.2 in RIFT_CORPUS_FASTAPI_STATIC"]
fn test_fastapi_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_FASTAPI_STATIC",
        version_file: "fastapi/__init__.py",
        version_line: "__version__ = \"0.163.2\"",
        include: "fastapi/**/*.py",
        files: 52,
        declarations: &[
            ("class", 122),
            ("function", 227),
            ("method", 218),
            ("module", 52),
            ("variable", 1674),
        ],
        edges: &[
            (EdgeKind::Aliases, 61),
            (EdgeKind::Calls, 601),
            (EdgeKind::Contains, 1993),
            (EdgeKind::Extends, 58),
            (EdgeKind::HasType, 539),
            (EdgeKind::Imports, 283),
            (EdgeKind::Overrides, 24),
            (EdgeKind::Reads, 585),
            (EdgeKind::Writes, 212),
        ],
    })
}

#[test]
#[ignore = "requires click 8.5.0 in RIFT_CORPUS_CLICK"]
fn test_click_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_CLICK",
        version_file: "pyproject.toml",
        version_line: "version = \"8.5.0\"",
        include: "src/click/**/*.py",
        files: 17,
        declarations: &[
            ("class", 88),
            ("function", 195),
            ("method", 384),
            ("module", 17),
            ("variable", 1493),
        ],
        edges: &[
            (EdgeKind::Aliases, 64),
            (EdgeKind::Calls, 1245),
            (EdgeKind::Contains, 1649),
            (EdgeKind::Extends, 47),
            (EdgeKind::HasType, 473),
            (EdgeKind::Imports, 195),
            (EdgeKind::Overrides, 84),
            (EdgeKind::Reads, 913),
            (EdgeKind::Writes, 285),
        ],
    })
}

#[test]
#[ignore = "requires click 8.5.0 in RIFT_CORPUS_CLICK"]
fn test_click_calls_on_container_elements_are_stored() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var_os("RIFT_CORPUS_CLICK").ok_or("missing corpus")?);
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::new(
            vec!["src/click/**/*.py".into(), "pyproject.toml".into()],
            vec![],
            true,
        ),
        &TextFileInclusion::default(),
    )?;
    for (from, to) in [
        (
            "core/Command/parse_args",
            "core/Parameter/handle_parse_result",
        ),
        (
            "core/Command/shell_complete",
            "core/Command/get_short_help_str",
        ),
        ("core/Command/to_info_dict", "core/Parameter/to_info_dict"),
        (
            "core/Group/format_commands",
            "core/Command/get_short_help_str",
        ),
        (
            "core/Group/shell_complete",
            "core/Command/get_short_help_str",
        ),
        ("core/Group/invoke", "core/Command/invoke"),
        ("core/Group/invoke", "core/Command/make_context"),
        ("types/Tuple/to_info_dict", "types/ParamType/to_info_dict"),
    ] {
        assert!(
            index.edges().edges().any(|edge| {
                edge.kind() == EdgeKind::Calls
                    && edge.from().as_str() == format!("rift://symbol/local/python/click/{from}")
                    && edge.to().as_str() == format!("rift://symbol/local/python/click/{to}")
            }),
            "{from} -> {to}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires flask 3.1.3 in RIFT_CORPUS_FLASK"]
fn test_flask_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_FLASK",
        version_file: "pyproject.toml",
        version_line: "version = \"3.1.3\"",
        include: "src/flask/**/*.py",
        files: 24,
        declarations: &[
            ("class", 47),
            ("function", 96),
            ("method", 271),
            ("module", 24),
            ("variable", 679),
        ],
        edges: &[
            (EdgeKind::Aliases, 39),
            (EdgeKind::Calls, 279),
            (EdgeKind::Contains, 958),
            (EdgeKind::Extends, 18),
            (EdgeKind::HasType, 243),
            (EdgeKind::Imports, 171),
            (EdgeKind::Overrides, 37),
            (EdgeKind::Reads, 505),
            (EdgeKind::Writes, 103),
        ],
    })
}

#[test]
#[ignore = "requires requests 2.34.2 in RIFT_CORPUS_REQUESTS"]
fn test_requests_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_REQUESTS",
        version_file: "src/requests/__version__.py",
        version_line: "__version__ = \"2.34.2\"",
        include: "src/requests/**/*.py",
        files: 19,
        declarations: &[
            ("class", 52),
            ("function", 90),
            ("method", 177),
            ("module", 19),
            ("variable", 807),
        ],
        edges: &[
            (EdgeKind::Calls, 530),
            (EdgeKind::Contains, 872),
            (EdgeKind::Extends, 37),
            (EdgeKind::HasType, 353),
            (EdgeKind::Imports, 185),
            (EdgeKind::Overrides, 8),
            (EdgeKind::Reads, 299),
            (EdgeKind::Writes, 96),
        ],
    })
}

#[test]
#[ignore = "requires attrs 26.1.0 in RIFT_CORPUS_ATTRS"]
fn test_attrs_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_ATTRS",
        version_file: "CHANGELOG.md",
        version_line: "## [26.1.0]",
        include: "src/**/*.py",
        files: 19,
        declarations: &[
            ("class", 37),
            ("function", 127),
            ("method", 81),
            ("module", 19),
            ("variable", 575),
        ],
        edges: &[
            (EdgeKind::Calls, 207),
            (EdgeKind::Contains, 718),
            (EdgeKind::Extends, 2),
            (EdgeKind::HasType, 3),
            (EdgeKind::Imports, 107),
            (EdgeKind::Reads, 192),
            (EdgeKind::Writes, 68),
        ],
    })
}

#[test]
#[ignore = "requires jinja2 3.1.6 in RIFT_CORPUS_JINJA2"]
fn test_jinja2_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_JINJA2",
        version_file: "src/jinja2/__init__.py",
        version_line: "__version__ = \"3.1.6\"",
        include: "src/jinja2/**/*.py",
        files: 25,
        declarations: &[
            ("class", 160),
            ("function", 215),
            ("method", 560),
            ("module", 25),
            ("variable", 1702),
        ],
        edges: &[
            (EdgeKind::Aliases, 31),
            (EdgeKind::Calls, 1072),
            (EdgeKind::Contains, 2314),
            (EdgeKind::Extends, 108),
            (EdgeKind::HasType, 804),
            (EdgeKind::Imports, 192),
            (EdgeKind::Overrides, 75),
            (EdgeKind::Reads, 1834),
            (EdgeKind::Writes, 300),
        ],
    })
}

#[test]
#[ignore = "requires marshmallow 4.4.0 in RIFT_CORPUS_MARSHMALLOW"]
fn test_marshmallow_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_MARSHMALLOW",
        version_file: "pyproject.toml",
        version_line: "version = \"4.4.0\"",
        include: "src/marshmallow/**/*.py",
        files: 14,
        declarations: &[
            ("class", 65),
            ("function", 33),
            ("method", 207),
            ("module", 14),
            ("variable", 578),
        ],
        edges: &[
            (EdgeKind::Calls, 208),
            (EdgeKind::Contains, 794),
            (EdgeKind::Extends, 48),
            (EdgeKind::HasType, 210),
            (EdgeKind::Imports, 55),
            (EdgeKind::Overrides, 94),
            (EdgeKind::Reads, 415),
            (EdgeKind::Writes, 157),
        ],
    })
}

#[test]
#[ignore = "requires urllib3 2.8.0 in RIFT_CORPUS_URLLIB3"]
fn test_urllib3_static_collection_floors() -> Result<(), Box<dyn std::error::Error>> {
    check(&Package {
        variable: "RIFT_CORPUS_URLLIB3",
        version_file: "CHANGES.rst",
        version_line: "2.8.0 (",
        include: "src/urllib3/**/*.py",
        files: 35,
        declarations: &[
            ("class", 104),
            ("function", 113),
            ("method", 400),
            ("module", 35),
            ("variable", 1525),
        ],
        edges: &[
            (EdgeKind::Aliases, 3),
            (EdgeKind::Calls, 435),
            (EdgeKind::Contains, 1685),
            (EdgeKind::Extends, 59),
            (EdgeKind::HasType, 315),
            (EdgeKind::Imports, 250),
            (EdgeKind::Overrides, 67),
            (EdgeKind::Reads, 766),
            (EdgeKind::Writes, 367),
        ],
    })
}
