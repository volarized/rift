//! Exact package calls and property reads from direct `CommonJS` export literals.

use super::*;
use rift_protocol::index::StoredEdge;
use rift_protocol::read::{EdgeDerivation, EdgeKind, EdgeProvider};

const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","exports":{".":"./index.js","./literal":"./literal.js","./values":"./values.js","./forward":"./forward.js"}}"#;
const ROOT_ONLY_MANIFEST: &str =
    r#"{"name":"beacon","version":"1.0.0","type":"commonjs","main":"index.js"}"#;
const OWNER: &str = "function checkExistence() { return 1 }
module.exports = { exist: checkExistence }
";
const USER: &str = "const library = require('./literal')
const { exist } = require('./literal')
const { kDirect } = require('./values')
function use() {
    library.exist()
    exist()
    return kDirect
}
module.exports = use
";
const VALUES: &str = "module.exports = { kDirect: Symbol('direct') }
";

fn analyze(user: &str, owner: &str) -> PackageAnalysis {
    analyze_with_manifest(user, owner, MANIFEST)
}

fn analyze_with_manifest(user: &str, owner: &str, manifest: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![
            ("index.js", user),
            ("literal.js", owner),
            ("values.js", VALUES),
            ("forward.js", "module.exports = require('./literal')"),
        ],
        None,
        Some(("package.json", manifest)),
    )
    .expect("selected CommonJS package")
}

#[test]
fn direct_literal_of_a_file_no_entry_names_keeps_its_path_module() {
    let analyzed = analyze_with_manifest(USER, OWNER, ROOT_ONLY_MANIFEST);
    for (kind, written, target) in [
        (EdgeKind::Calls, "library.exist()", "/checkExistence"),
        (EdgeKind::Calls, "exist()", "/checkExistence"),
        (EdgeKind::Reads, "library.exist", "/default/exist"),
    ] {
        assert!(
            analyzed.publication().edges.iter().any(|edge| {
                edge.kinds == [kind]
                    && occurrence(edge, USER, written)
                    && edge
                        .to
                        .as_str()
                        .ends_with(&format!("/beacon%2Fliteral.js{target}"))
            }),
            "{kind:?} {written}"
        );
    }
}

fn occurrence(edge: &StoredEdge, source: &str, written: &str) -> bool {
    edge.occurrence.as_ref().is_some_and(|occurrence| {
        occurrence.unit.as_str().ends_with("/index.js")
            && source.get(offset_in(occurrence.range.start)..offset_in(occurrence.range.end))
                == Some(written)
    })
}

#[test]
fn final_commonjs_literal_pair_keeps_its_property_and_calls_its_local_function() {
    let (publication, files, semantics, _) = analyze(USER, OWNER).into_parts();
    let owner = files
        .iter()
        .find(|file| file.file().path().as_str() == "literal.js")
        .expect("checked literal provider");
    let syntax = owner.file().syntax();
    let property = syntax
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == "default.exist")
        .expect("physical property declaration");
    assert_eq!(property.syntax_element_kind, Some("pair"));
    let item = property.item_range;
    assert_eq!(
        OWNER.get(offset_in(item.start)..offset_in(item.end)),
        Some("exist: checkExistence")
    );
    let proof = syntax
        .static_facts()
        .and_then(rift_syntax::StaticFacts::commonjs_export)
        .expect("closed module literal");
    assert!(proof.assignment.encloses(item));
    assert!(proof.initialized.is_empty());
    assert!(proof.literal.is_some_and(|literal| literal.encloses(item)));
    let property = owner
        .placement()
        .logical_identity(&property.qualified_name)
        .expect("same retained member identity");
    let callable = owner
        .placement()
        .logical_identity("checkExistence")
        .expect("exact lexical function");
    assert_ne!(property, callable);

    for (kind, text, target) in [
        (EdgeKind::Calls, "library.exist()", callable.as_str()),
        (EdgeKind::Calls, "exist()", callable.as_str()),
        (EdgeKind::Reads, "library.exist", property.as_str()),
    ] {
        let edge = publication
            .edges
            .iter()
            .find(|edge| {
                edge.kinds == [kind] && edge.to.as_str() == target && occurrence(edge, USER, text)
            })
            .expect("exact static edge at the written occurrence");
        assert!(edge.from.as_str().ends_with("/use"));
        assert_eq!(edge.provider, EdgeProvider::Static);
        assert_eq!(edge.derivation, EdgeDerivation::Resolution);
        assert!(
            semantics
                .edges()
                .edges()
                .any(|stored| &stored.to_protocol() == edge)
        );
    }
    let value = publication
        .edges
        .iter()
        .find(|edge| edge.kinds == [EdgeKind::Imports] && occurrence(edge, USER, "kDirect"))
        .expect("destructuring imports the donor property");
    assert!(value.to.as_str().ends_with("/default/kDirect"));
    assert!(
        semantics
            .edges()
            .edges()
            .any(|stored| &stored.to_protocol() == value)
    );
    let local = publication
        .edges
        .iter()
        .find(|edge| edge.kinds == [EdgeKind::Reads] && occurrence(edge, USER, "kDirect"))
        .expect("the later occurrence reads the destructured local binding");
    assert_eq!(local.to, value.from);
}

#[test]
fn final_commonjs_literal_pair_refuses_uncertain_or_mutated_values() {
    let direct = "const library = require('./literal')
function use() { return library.exist() }
module.exports = use";
    let literal_negatives = [
        "function local() { return 1 }\nmodule.exports = { exist: local, [key]: 1 }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, 'exist': 1 }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, exist() {} }".to_owned(),
        "function local() { return 1 }\nmodule.exports = { exist: local, ...unknown }".to_owned(),
        format!("const module = {{ exports: {{}} }}\n{OWNER}"),
    ];
    for owner in literal_negatives.into_iter().chain(
        [
            "module.exports.exist = unknown",
            "module.exports[key] = unknown",
            "const alias = module.exports\nalias.exist = unknown",
            "module.exports = unknown",
        ]
        .map(|effect| format!("{OWNER}{effect}")),
    ) {
        assert_no_literal_member_edges(direct, &owner);
    }
    let rebound =
        "let local = function () { return 1 }\nlocal = unknown\nmodule.exports = { exist: local }";
    let analyzed = analyze(direct, rebound);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls] && occurrence(edge, direct, "library.exist()")
    }));
    for effect in [
        "library = unknown",
        "library.exist = unknown",
        "library[key] = unknown",
        "const alias = library\nalias.exist = unknown",
        "delete library.exist",
        "mutate(library)",
        "library[key]()",
        "const other = require('./literal.js')\nother.exist = unknown",
        "require('./literal').exist = unknown",
    ] {
        let user = format!(
            "let library = require('./literal')\n{effect}\nfunction use() {{ return library.exist() }}"
        );
        assert_no_literal_member_edges(&user, OWNER);
    }
}

#[test]
fn literal_pair_refuses_standalone_member_reads_without_a_checked_use() {
    let user = "const library = require('./literal')\nfunction use() { return library.exist }\nmodule.exports = use";
    let analyzed = analyze(user, OWNER);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && occurrence(edge, user, "library.exist")
            && edge.to.as_str().ends_with("/default/exist")
    }));
    let spread = "function checkExistence() { return 1 }\nmodule.exports = { exist: checkExistence, ...unknown }";
    let analyzed = analyze(user, spread);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && occurrence(edge, user, "library.exist")
            && edge.to.as_str().ends_with("/default/exist")
    }));
}

fn assert_no_literal_member_edges(user: &str, owner: &str) {
    let analyzed = analyze(user, owner);
    assert!(
        !analyzed.publication().edges.iter().any(|edge| {
            (edge.kinds == [EdgeKind::Calls] && occurrence(edge, user, "library.exist()"))
                || (edge.kinds == [EdgeKind::Reads]
                    && occurrence(edge, user, "library.exist")
                    && edge.to.as_str().ends_with("/default/exist"))
        }),
        "{user}\n{owner}"
    );
}

#[test]
fn literal_pair_keeps_unproved_forwarded_members_unresolved() {
    let user = "const library = require('./forward')
function use() { return [library.exist(), library.exist] }
module.exports = use";
    let analyzed = analyze(user, OWNER);
    assert!(!analyzed.publication().edges.iter().any(|edge| {
        (edge.kinds == [EdgeKind::Calls] && occurrence(edge, user, "library.exist()"))
            || (edge.kinds == [EdgeKind::Reads]
                && occurrence(edge, user, "library.exist")
                && edge.to.as_str().ends_with("/default/exist"))
    }));
}

#[test]
fn literal_pair_before_other_written_properties_keeps_its_call_and_read() {
    let user = "const library = require('./literal')
function use() { return library.exist() }
module.exports = use";
    for owner in [
        "function local() { return 1 }\nmodule.exports = { exist: local, other: 1 }",
        "function local() { return 1 }\nmodule.exports = { ...base, exist: local, 'other': 1, last() {} }",
    ] {
        let analyzed = analyze(user, owner);
        for (kind, text, target) in [
            (EdgeKind::Calls, "library.exist()", "/local"),
            (EdgeKind::Reads, "library.exist", "/default/exist"),
        ] {
            assert!(
                analyzed.publication().edges.iter().any(|edge| {
                    edge.kinds == [kind]
                        && occurrence(edge, user, text)
                        && edge.to.as_str().ends_with(target)
                }),
                "{kind:?} {owner}"
            );
        }
    }
}

#[test]
fn an_extensionless_nested_main_can_use_its_observed_outer_index() {
    use rift_protocol::read::{ProjectPath as ObservedPath, SourceLocation};

    const CALLER: &str =
        "const { work } = require(\"./lib\");\nexports.run = function run() { return work(); };\n";
    const INDEX: &str = "exports.work = function work() { return 42; };\n";
    const NESTED: &str = r#"{"main":"entry"}"#;

    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("npm owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("authored npm source");
    let language = ShippedLanguage::JavaScript.language();
    let entry = ProjectPath::new("index.js").expect("entry path");
    let index = ProjectPath::new("lib/index.js").expect("fallback path");
    let manifest = ProjectPath::new("package.json").expect("root manifest path");
    let nested = ProjectPath::new("lib/package.json").expect("nested manifest path");
    let selected = [
        PackageSource::new(&entry, CALLER),
        PackageSource::new(&index, INDEX),
    ];
    let context = [
        PackageSource::new(&manifest, ROOT_ONLY_MANIFEST),
        PackageSource::new(&nested, NESTED),
    ];

    let present = Some(Some(crate::ArchiveMemberKind::File));
    for (observed_json, sibling_json, found) in [
        (Some(None), Some(None), true),
        (None, Some(None), false),
        (present, Some(None), false),
        (Some(None), None, false),
        (Some(None), present, false),
    ] {
        let mut observed = BTreeMap::from([
            (ObservedPath("lib.js".to_owned()), None),
            (ObservedPath("lib.node".to_owned()), None),
            (ObservedPath("lib/entry".to_owned()), None),
            (ObservedPath("lib/entry.js".to_owned()), None),
            (ObservedPath("lib/entry.node".to_owned()), None),
        ]);
        if let Some(kind) = observed_json {
            observed.insert(ObservedPath("lib/entry.json".to_owned()), kind);
        }
        if let Some(kind) = sibling_json {
            observed.insert(ObservedPath("lib.json".to_owned()), kind);
        }
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &selected,
            ExactPackageLimits::new(16, 16_384),
        )
        .expect("captured byte bounds")
        .with_framework_context(&context, &[])
        .expect("both written manifests")
        .with_build_paths(&observed)
        .expect("observed paths");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("package analysis");
        let publication = analysis.publication();
        publication.validate_facts().expect("valid stored edges");
        let edges = publication
            .edges
            .iter()
            .filter(|row| {
                row.to.as_str().ends_with("/beacon%2Flib%2Findex.js/work")
                    && matches!(row.kinds.as_slice(), [EdgeKind::Imports | EdgeKind::Calls])
            })
            .collect::<Vec<_>>();
        if found {
            assert_eq!(edges.len(), 2);
            for (kind, source) in [(EdgeKind::Imports, "work"), (EdgeKind::Calls, "work()")] {
                let edge = edges
                    .iter()
                    .find(|row| row.kinds == [kind])
                    .expect("authored edge");
                assert!(occurrence(edge, CALLER, source));
            }
        } else {
            assert!(edges.is_empty(), "{observed_json:?} {sibling_json:?}");
        }
    }
}

#[cfg(feature = "archive")]
#[test]
fn an_explicit_nested_main_does_not_fall_through_an_unavailable_runtime_file() {
    use rift_protocol::read::SourceLocation;

    const CALLER: &str =
        "const { work } = require(\"./lib\");\nexports.run = function run() { return work(); };\n";
    const INDEX: &str = "exports.work = function work() { return 42; };\n";
    const NESTED: &str = r#"{"main":"entry.js"}"#;
    let package = rift_protocol::read::PackageIdentity {
        manager: "npm".to_owned(),
        registry: "registry.npmjs.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("npm owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("authored npm source");
    let language = ShippedLanguage::JavaScript.language();
    for (extra, keep, unobserved, expected) in [
        (None, false, None, Some("lib/index.js")),
        (None, false, Some("lib/entry.js"), None),
        (Some("lib/entry.js"), false, None, None),
        (Some("lib/entry.js"), true, None, Some("lib/entry.js")),
        (
            Some("lib/entry.js"),
            true,
            Some("lib.js"),
            Some("lib/entry.js"),
        ),
        (Some("lib/entry.js.js"), false, None, None),
        (Some("lib/entry.js.json"), false, None, None),
        (Some("lib/entry.js.node"), false, None, None),
        (Some("lib.node"), false, None, None),
    ] {
        let archive = nested_main_archive(
            [
                ("index.js", CALLER),
                ("lib/index.js", INDEX),
                ("package.json", ROOT_ONLY_MANIFEST),
                ("lib/package.json", NESTED),
            ]
            .into_iter()
            .chain(extra.map(|path| (path, INDEX))),
            extra.filter(|_| !keep),
        );
        let (context, selected): (Vec<_>, Vec<_>) = archive
            .files()
            .iter()
            .map(|(path, bytes)| {
                PackageSource::new(path, std::str::from_utf8(bytes).expect("authored source"))
            })
            .partition(|source| source.path().as_str().ends_with("package.json"));
        let observed = nested_main_observations(&archive, &context, &selected, unobserved);
        if let Some(extra) = extra {
            let member = rift_protocol::read::ProjectPath(extra.to_owned());
            assert_eq!(observed[&member], Some(crate::ArchiveMemberKind::File));
            assert_eq!(
                selected.iter().any(|file| file.path().as_str() == extra),
                keep
            );
        }
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &selected,
            ExactPackageLimits::new(32, 16_384),
        )
        .expect("captured byte bounds")
        .with_framework_context(&context, &[])
        .expect("both written manifests")
        .with_build_paths(&observed)
        .expect("observed paths");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("package analysis");
        let publication = analysis.publication();
        publication.validate_facts().expect("valid stored edges");
        for (kind, written) in [(EdgeKind::Imports, "work"), (EdgeKind::Calls, "work()")] {
            let edges = publication
                .edges
                .iter()
                .filter(|edge| edge.kinds == [kind] && occurrence(edge, CALLER, written))
                .map(|edge| edge.to.as_str())
                .collect::<Vec<_>>();
            let target =
                expected.map(|path| format!("/beacon%2F{}/work", path.replace('/', "%2F")));
            assert_eq!(
                edges.len(),
                usize::from(target.is_some()),
                "{extra:?} {keep} {unobserved:?} {kind:?} {edges:?}"
            );
            assert!(
                edges
                    .iter()
                    .zip(target)
                    .all(|(edge, target)| edge.ends_with(&target))
            );
        }
    }
}

#[cfg(feature = "archive")]
fn nested_main_archive(
    entries: impl IntoIterator<Item = (&'static str, &'static str)>,
    omitted: Option<&str>,
) -> crate::archive::ArchiveFiles {
    use crate::archive::{ArchiveDigest, ArchiveFormat, ArchiveLimits, read_archive_from};
    use sha2::{Digest, Sha512};

    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gzip);
    for (path, source) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_size(u64::try_from(source.len()).expect("source length"));
        header.set_mode(0o644);
        builder
            .append_data(&mut header, format!("package/{path}"), source.as_bytes())
            .expect("authored archive entry");
    }
    let bytes = builder
        .into_inner()
        .expect("authored archive")
        .finish()
        .expect("complete compressed archive");
    read_archive_from(
        std::io::Cursor::new(&bytes),
        ArchiveFormat::TarGzip,
        &ArchiveDigest::Sha512(Sha512::digest(&bytes).into()),
        Some("package"),
        ArchiveLimits::default(),
        &|path, _| omitted != Some(path.as_str()),
    )
    .expect("verified package archive")
}

#[cfg(feature = "archive")]
fn nested_main_observations(
    archive: &crate::archive::ArchiveFiles,
    context: &[PackageSource<'_>],
    selected: &[PackageSource<'_>],
    unobserved: Option<&str>,
) -> BTreeMap<rift_protocol::read::ProjectPath, Option<crate::ArchiveMemberKind>> {
    context
        .iter()
        .flat_map(|manifest| {
            crate::build_paths(*manifest, selected, rift_syntax::SyntaxLimits::DEFAULT)
                .unwrap_or_default()
        })
        .filter(|path| unobserved != Some(path.0.as_str()))
        .map(|path| {
            let name = format!("package/{}", path.0);
            let kind = archive.members().get(&name).copied().or_else(|| {
                let prefix = format!("{name}/");
                archive
                    .members()
                    .range(prefix.clone()..)
                    .next()
                    .filter(|(name, _)| name.starts_with(&prefix))
                    .map(|_| crate::ArchiveMemberKind::Directory)
            });
            (path, kind)
        })
        .collect()
}
