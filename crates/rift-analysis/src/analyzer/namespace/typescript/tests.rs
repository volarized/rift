use std::collections::BTreeMap;

use rift_core::{ContributionOrigin, ProjectPath, SourceUnitId, SymbolId};
use rift_protocol::{
    identity::SymbolIdentity,
    read::{PackageIdentity, SourceKind, SourceLocation},
};
use rift_syntax::{
    JavaScriptSyntaxProvider, ShippedLanguage, SyntaxProvider, SyntaxSource, TypeScriptDialect,
    TypeScriptSyntaxProvider,
};

use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};

type PathStates = BTreeMap<rift_protocol::read::ProjectPath, Option<crate::ArchiveMemberKind>>;

const TYPES: &str = r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts"}"#;

#[test]
fn selected_conditions_keep_runtime_and_types_projections_separate() {
    let metadata: super::PackageMetadata = serde_json::from_str(
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":"./index.d.ts","require":"./index.cjs","browser":{"import":"./standalone.mjs","default":"./standalone.js"},"default":"./index.mjs"}}}"#,
    ).expect("captured metadata");
    let exports = metadata.exports.expect("exports");
    for (conditions, implementation) in [
        (vec!["require"], "./index.cjs"),
        (vec!["default"], "./index.mjs"),
        (vec!["browser", "import"], "./standalone.mjs"),
    ] {
        assert_eq!(
            exports.observed_targets("prettier", "prettier", &conditions),
            Some(("./index.d.ts", implementation))
        );
    }
    assert!(
        exports
            .observed_targets("prettier", "foreign", &["require"])
            .is_none()
    );
}

#[test]
fn nested_conditions_preserve_json_order_and_branch_specific_declarations() {
    let metadata: super::PackageMetadata = serde_json::from_str(
        r#"{"name":"beacon","version":"1.0.0","exports":{".":{"import":{"types":"./index.d.mts","default":"./index.mjs"},"require":{"types":"./index.d.cts","default":"./index.cjs"}}}}"#,
    ).expect("captured metadata");
    let exports = metadata.exports.expect("exports");
    assert_eq!(
        exports.observed_targets("beacon", "beacon", &["import"]),
        Some(("./index.d.mts", "./index.mjs"))
    );
    assert_eq!(
        exports.observed_targets("beacon", "beacon", &["require"]),
        Some(("./index.d.cts", "./index.cjs"))
    );
    assert!(exports.observed_targets("beacon", "beacon", &[]).is_none());
    let versioned: super::ExportTarget = serde_json::from_str(
        r#"{"types@>=5.7":"./new.d.ts","types":"./old.d.ts","default":"./index.js"}"#,
    )
    .expect("versioned metadata");
    assert!(versioned.selected_target(true, &["import"]).is_none());
}

fn prepared(
    language: ShippedLanguage,
    metadata: &[(&str, &str)],
    files: &[(&str, &str)],
) -> super::PreparedNamespace {
    prepared_languages(language, metadata, files, None)
}

fn prepared_languages(
    language: ShippedLanguage,
    metadata: &[(&str, &str)],
    files: &[(&str, &str)],
    languages: Option<&[ShippedLanguage]>,
) -> super::PreparedNamespace {
    prepared_observed(language, metadata, files, languages, None)
}

/// The same, with the path states a caller observed for the package.
fn prepared_observed(
    language: ShippedLanguage,
    metadata: &[(&str, &str)],
    files: &[(&str, &str)],
    languages: Option<&[ShippedLanguage]>,
    observed: Option<&PathStates>,
) -> super::PreparedNamespace {
    let owner =
        SymbolIdentity::parse("rift://symbol/npm/npmjs.org/prettier/typescript/prettier/format")
            .expect("exact package owner")
            .owner()
            .clone();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "npm".to_owned(),
                registry: "npmjs.org".to_owned(),
                name: "prettier".to_owned(),
                version: "3.8.5".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("captured package origin");
    let language = language.language();
    let paths = files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("selected path"))
        .collect::<Vec<_>>();
    let sources = paths
        .iter()
        .zip(files)
        .map(|(path, (_, source))| PackageSource::new(path, source))
        .collect::<Vec<_>>();
    let context_paths = metadata
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("context path"))
        .collect::<Vec<_>>();
    let context = context_paths
        .iter()
        .zip(metadata)
        .map(|(path, (_, source))| PackageSource::new(path, source))
        .collect::<Vec<_>>();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(20, 16 * 1024 * 1024),
    )
    .expect("bounded captured input")
    .with_framework_context(&context, &[])
    .expect("captured package metadata");
    let input = match observed {
        Some(observed) => input.with_build_paths(observed).expect("observed paths"),
        None => input,
    };
    let facts = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let source = SyntaxSource {
                path: source.path(),
                text: source.text(),
            };
            let provider_language =
                languages.map_or_else(|| language.clone(), |languages| languages[index].language());
            if provider_language.name == "javascript" {
                JavaScriptSyntaxProvider::default().analyze(source, input.limits().syntax())
            } else {
                TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript)
                    .analyze(source, input.limits().syntax())
            }
            .expect("captured source parses")
            .into_facts()
        })
        .collect::<Vec<_>>();
    let selected = sources
        .iter()
        .zip(&facts)
        .map(|(source, syntax)| super::SelectedFile {
            path: source.path(),
            source: source.text(),
            syntax,
        })
        .collect::<Vec<_>>();
    let before = paths
        .iter()
        .map(|path| {
            SourceUnitId::for_owner(owner.clone(), path.as_str()).expect("physical source unit")
        })
        .collect::<Vec<_>>();
    let found = super::prepare(&input.namespace_input(), &selected);
    let after = paths
        .iter()
        .map(|path| {
            SourceUnitId::for_owner(owner.clone(), path.as_str())
                .expect("unchanged physical source unit")
        })
        .collect::<Vec<_>>();
    assert_eq!(before, after);
    found
}

fn anchors(
    language: ShippedLanguage,
    metadata: &[(&str, &str)],
    files: &[(&str, &str)],
) -> super::super::Anchors {
    prepared(language, metadata, files).anchors
}

fn one(metadata: &str, path: &str, source: &str) -> BTreeMap<String, SymbolId> {
    anchors(
        ShippedLanguage::TypeScript,
        &[("package.json", metadata)],
        &[(path, source)],
    )
    .remove(path)
    .unwrap_or_default()
    .into_iter()
    .filter(|(name, _)| name != ".")
    .collect()
}

/// Whether `anchors` names its declarations, every one by the module of a package path.
fn in_path_module(anchors: &BTreeMap<String, SymbolId>, module: &str) -> bool {
    !anchors.is_empty()
        && anchors.values().all(|identity| {
            SymbolIdentity::parse(identity.as_str())
                .expect("canonical identity")
                .qualified_path()
                .first()
                .is_some_and(|first| first == module)
        })
}

#[test]
fn captured_types_root_and_container_members_have_defining_module_names() {
    let found = one(
        TYPES,
        "index.d.ts",
        "export function format(value: string): string;\nexport class SemVer { format(): string; }",
    );
    assert_eq!(
        found["format"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/format"
    );
    assert_eq!(
        found["SemVer.format"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/SemVer/format"
    );
    assert_ne!(found["format"], found["SemVer.format"]);
}

#[test]
fn exact_prettier_export_types_target_selects_declarations_without_renaming_implementation() {
    let metadata = r#"{"name":"prettier","version":"3.8.5","main":"./index.cjs","exports":{".":{"types":"./index.d.ts","require":"./index.cjs","browser":{"import":"./standalone.mjs","default":"./standalone.js"},"default":"./index.mjs"},"./*":"./*"}}"#;
    let found = anchors(
        ShippedLanguage::TypeScript,
        &[("package.json", metadata)],
        &[
            (
                "index.d.ts",
                "export function format(value: string): string;",
            ),
            (
                "index.mjs",
                "class SemVer { format() {} }\nfunction format2() {}\nexport { format2 as format };",
            ),
        ],
    );
    assert!(found["index.d.ts"].contains_key("format"));
    assert!(
        in_path_module(&found["index.mjs"], "prettier/index.mjs"),
        "an unselected conditional implementation takes no entry module"
    );
}

#[test]
fn javascript_export_alias_keeps_defining_name_and_container_separate() {
    let metadata = r#"{"name":"prettier","version":"3.8.5","main":"./index.mjs"}"#;
    let found = anchors(
        ShippedLanguage::JavaScript,
        &[("package.json", metadata)],
        &[(
            "index.mjs",
            "class SemVer { format() {} }\nfunction format2() {}\nexport { format2 as format };",
        )],
    );
    let found = &found["index.mjs"];
    assert_eq!(
        found["format2"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier/javascript/prettier/format2"
    );
    assert_eq!(
        found["SemVer.format"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier/javascript/prettier/SemVer/format"
    );
    assert!(
        !found.contains_key("format"),
        "export alias does not rename its defining local"
    );
}

#[test]
fn explicit_alias_targets_the_defining_function_and_retains_its_export_range() {
    let source =
        "class SemVer { format() {} }\nfunction format2() {}\nexport { format2 as format };";
    let found = prepared(
        ShippedLanguage::JavaScript,
        &[(
            "package.json",
            r#"{"name":"prettier","version":"3.8.5","main":"./index.mjs"}"#,
        )],
        &[("index.mjs", source)],
    );
    assert!(!found.unresolved_exports);
    assert_eq!(found.exports.len(), 1);
    let binding = &found.exports[0];
    assert_eq!(binding.target, found.anchors["index.mjs"]["format2"]);
    assert_ne!(binding.target, found.anchors["index.mjs"]["SemVer.format"]);
    assert_eq!(binding.target_qualified_name, "format2");
    assert_eq!(binding.name, "format");
    assert_eq!(binding.qualified_name, "format");
    let alias = SymbolIdentity::parse(binding.identity.as_str()).expect("canonical alias");
    let target = SymbolIdentity::parse(binding.target.as_str()).expect("canonical target");
    assert_eq!(alias.owner(), target.owner());
    assert_eq!(alias.language(), target.language());
    assert_eq!(alias.qualified_path(), ["prettier", "format"]);
    assert_eq!(target.qualified_path(), ["prettier", "format2"]);
    let start = usize::try_from(binding.binding.range.start).expect("export start");
    let end = usize::try_from(binding.binding.range.end).expect("export end");
    assert_eq!(&source[start..end], "export { format2 as format };");
}

#[test]
fn lexical_collision_and_unestablished_reexports_keep_incomplete_export_coverage() {
    for source in [
        "function format() {} function format2() {} export { format2 as format };",
        "export { format as alias } from './other.js';",
        "export * from './other.js';",
        "export default () => 1;",
    ] {
        let found = prepared(
            ShippedLanguage::JavaScript,
            &[(
                "package.json",
                r#"{"name":"prettier","version":"3.8.5","main":"./index.mjs"}"#,
            )],
            &[("index.mjs", source)],
        );
        assert!(found.unresolved_exports, "{source}");
        assert!(found.exports.is_empty());
    }
}

#[test]
fn typescript_overload_signatures_bind_one_function() {
    let source = "export function format(value: string): string;\nexport function format(value: number): string;";
    let found = one(TYPES, "index.d.ts", source);
    assert_eq!(found.len(), 1);
    assert!(found.values().all(|identity| identity.as_str()
        == "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/format"));
}

#[test]
fn competing_function_bodies_share_one_identity() {
    let source = "export function format() {}\nexport function format() {}";
    let found = one(TYPES, "index.d.ts", source);
    assert_eq!(found.len(), 1);
    assert!(found.values().all(|identity| identity.as_str()
        == "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/format"));
}

#[test]
fn members_of_a_repeated_container_take_its_one_path() {
    let source = "export namespace Tools { export function a(): void; }\nexport namespace Tools { export function b(): void; }";
    let found = one(TYPES, "index.d.ts", source);
    let identities = found
        .values()
        .map(SymbolId::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identities,
        [
            "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/Tools",
            "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/Tools/a",
            "rift://symbol/npm/npmjs.org/prettier/typescript/prettier/Tools/b",
        ]
        .into_iter()
        .collect(),
        "{found:?}"
    );
}

#[test]
fn literal_occurrence_spelling_in_source_name_is_not_a_qualifier() {
    let found = one(
        TYPES,
        "index.d.ts",
        "export class Example { \"method~2\"(): string; }",
    );
    let member = found
        .values()
        .find(|id| {
            SymbolIdentity::parse(id.as_str())
                .expect("canonical identity")
                .qualified_path()
                .last()
                .is_some_and(|name| name.contains("method~2"))
        })
        .expect("literal named method");
    assert!(member.as_str().ends_with("/%22method%7E2%22"), "{member:?}");
}

#[test]
fn unresolved_owner_metadata_conditions_and_paths_do_not_guess_module() {
    for metadata in [
        r#"{"name":"other","version":"3.8.5","types":"./index.d.ts"}"#,
        r#"{"name":"prettier","version":"3.8.6","types":"./index.d.ts"}"#,
        r#"{"name":"prettier","version":"3.8.5"}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"../index.d.ts"}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"/index.d.ts"}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","typesVersions":{">=5":{"*": ["new/*"]}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"browser":"./index.d.ts","@prettier/source":"./index.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":null,"default":"./index.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types@>=5.7":"./new.d.ts","types":"./index.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":"../index.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":["./index.d.ts"]}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":"./index.d.ts","types":"./other.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","types":"./other.d.ts"}"#,
        "not JSON",
    ] {
        let found = one(metadata, "index.d.ts", "export class Example {}");
        // Metadata of another package or that cannot be read names no file at all.
        let described = metadata.starts_with(r#"{"name":"prettier","version":"3.8.5""#)
            && !metadata.contains(r#""types":"./other.d.ts""#);
        if described {
            assert!(in_path_module(&found, "prettier/index.d.ts"), "{metadata}");
        } else {
            assert!(found.is_empty(), "{metadata}");
        }
    }
    assert!(
        anchors(
            ShippedLanguage::TypeScript,
            &[],
            &[("index.d.ts", "export class Example {}")]
        )
        .is_empty()
    );
}

#[test]
fn unselected_physical_module_and_parser_errors_remain_unresolved() {
    let source = "export class Example {}";
    assert!(in_path_module(
        &one(TYPES, "other.d.ts", source),
        "prettier/other.d.ts"
    ));
    assert!(one(TYPES, "index.d.ts", "export function broken(").is_empty());
    assert!(one(TYPES, "other.d.ts", "export function broken(").is_empty());
}

#[test]
fn types_target_precedence_and_export_conditions_preserve_observed_order() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","typings":"./other.d.ts"}"#;
    assert!(!one(metadata, "index.d.ts", "export class Example {}").is_empty());
    assert!(in_path_module(
        &one(metadata, "other.d.ts", "export class Example {}"),
        "prettier/other.d.ts"
    ));
    let metadata = r#"{"name":"prettier","version":"3.8.5","exports":{".":{"default":"./other.d.ts","types":"./index.d.ts"}}}"#;
    assert!(in_path_module(
        &one(metadata, "index.d.ts", "export class Example {}"),
        "prettier/index.d.ts"
    ));
}

fn mixed_entries(metadata: &str, types: &str, implementation: &str) -> super::PreparedNamespace {
    prepared_languages(
        ShippedLanguage::TypeScript,
        &[("package.json", metadata)],
        &[("index.d.ts", types), ("index.mjs", implementation)],
        Some(&[ShippedLanguage::TypeScript, ShippedLanguage::JavaScript]),
    )
}

#[test]
fn captured_types_and_main_map_one_explicit_export_to_its_defining_object() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs"}"#;
    let found = mixed_entries(
        metadata,
        "export declare function format(source: string, options?: unknown): Promise<string>;",
        "class SemVer { format() { return this.version; } }\nfunction format2(text, options) { return text; }\nexport { format2 as format };",
    );
    let mapping = &found.mappings["index.d.ts"]["format"];
    assert_eq!(mapping.language.name, "javascript");
    assert_eq!(mapping.identity, found.anchors["index.mjs"]["format2"]);
    assert_ne!(
        mapping.identity,
        found.anchors["index.mjs"]["SemVer.format"]
    );
    assert_ne!(mapping.identity, found.anchors["index.d.ts"]["format"]);
    assert_eq!(found.mappings["index.d.ts"].len(), 1);
    assert_eq!(found.exports.len(), 1);
    assert_eq!(found.exports[0].target, mapping.identity);
}

#[test]
fn export_conditions_resolve_the_first_one_a_static_consumer_selects() {
    for (exports, types, implementation) in [
        // zod 4.6.7 lists a condition no published consumer resolves first.
        (
            r#"{".":{"@zod/source":"./src/index.ts","types":"./index.d.cts","import":"./index.js","require":"./index.cjs"}}"#,
            "./index.d.cts",
            "./index.js",
        ),
        (
            r#"{".":{"browser":"./browser.js","types":"./index.d.ts","default":"./index.mjs"}}"#,
            "./index.d.ts",
            "./index.mjs",
        ),
        (
            r#"{".":{"import":{"types":"./index.d.mts","default":"./index.mjs"},"require":{"types":"./index.d.cts","default":"./index.cjs"}}}"#,
            "./index.d.mts",
            "./index.mjs",
        ),
        (
            r#"{".":{"require":"./index.cjs","import":"./index.mjs"}}"#,
            "./index.mjs",
            "./index.mjs",
        ),
        (
            r#"{".":{"node":{"import":"./node.mjs"},"import":{"development":"./dev.mjs"},"default":"./index.mjs"}}"#,
            "./node.mjs",
            "./node.mjs",
        ),
        (
            r#"{".":{"default":"./index.mjs","types":"./index.d.ts"}}"#,
            "./index.mjs",
            "./index.mjs",
        ),
        (
            r#"{"types":"./index.d.ts","import":"./index.mjs"}"#,
            "./index.d.ts",
            "./index.mjs",
        ),
        (
            r#"{".":{"types":"./index.d.ts","default":"./index.mjs"},"./internal/*":null,"./list":["./list.mjs"]}"#,
            "./index.d.ts",
            "./index.mjs",
        ),
    ] {
        let metadata: super::PackageMetadata = serde_json::from_str(&format!(
            r#"{{"name":"prettier","version":"3.8.5","exports":{exports}}}"#
        ))
        .expect("captured metadata");
        for (language, expected) in [("typescript", types), ("javascript", implementation)] {
            assert_eq!(
                super::entry_path(&metadata, language)
                    .as_ref()
                    .map(ProjectPath::as_str),
                expected.strip_prefix("./"),
                "{language} {exports}"
            );
        }
    }
    let metadata: super::PackageMetadata = serde_json::from_str(
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":"./index.d.ts"}}}"#,
    )
    .expect("captured metadata");
    assert!(super::entry_path(&metadata, "javascript").is_none());
}

#[test]
fn unknown_first_conditions_establish_every_published_entry_point() {
    let files = [
        ("src/index.ts", "export class Source {}"),
        (
            "index.d.cts",
            "export declare class ZodType { parse(): void }",
        ),
        (
            "core/index.d.ts",
            "export declare function safeParse(): void;",
        ),
    ];
    let found = prepared(
        ShippedLanguage::TypeScript,
        &[(
            "package.json",
            r#"{"name":"prettier","version":"3.8.5","exports":{".":{"@zod/source":"./src/index.ts","types":"./index.d.cts","import":"./index.js","require":"./index.cjs"},"./core":{"@zod/source":"./src/core.ts","import":{"types":"./core/index.d.ts","default":"./core/index.js"}}}}"#,
        )],
        &files,
    );
    for (path, name, qualified) in [
        ("index.d.cts", "ZodType", vec!["prettier", "ZodType"]),
        (
            "index.d.cts",
            "ZodType.parse",
            vec!["prettier", "ZodType", "parse"],
        ),
        (
            "core/index.d.ts",
            "safeParse",
            vec!["prettier/core", "safeParse"],
        ),
    ] {
        let identity = SymbolIdentity::parse(found.anchors[path][name].as_str())
            .expect("published entry identity");
        assert_eq!(identity.qualified_path(), qualified);
    }
    assert!(
        in_path_module(&found.anchors["src/index.ts"], "prettier/src/index.ts"),
        "no published consumer resolves the source condition"
    );
}

#[test]
fn conditions_map_declared_types_only_to_one_implementation() {
    let types = "export declare function format(source: string): string;";
    let implementation = "export function format(source) { return source; }";
    let found = mixed_entries(
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"@prettier/source":"./src/index.ts","types":"./index.d.ts","default":"./index.mjs"}}}"#,
        types,
        implementation,
    );
    assert!(
        found.mappings.is_empty(),
        "the source condition names a second implementation"
    );
    let found = mixed_entries(
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"import":{"types":"./index.d.ts","default":"./index.mjs"},"default":"./index.mjs"}}}"#,
        types,
        implementation,
    );
    assert_eq!(
        found.mappings["index.d.ts"]["format"].identity,
        found.anchors["index.mjs"]["format"]
    );
}

#[test]
fn missing_or_conditional_entry_association_keeps_language_identities_distinct() {
    for metadata in [
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts"}"#,
        r#"{"name":"prettier","version":"3.8.5","main":"./index.mjs"}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs","module":"./other.mjs"}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"browser":"./browser.mjs","types":"./index.d.ts","default":"./index.mjs"}}}"#,
    ] {
        let found = mixed_entries(
            metadata,
            "export function format(value: string): string;",
            "export function format(value) { return value; }",
        );
        assert!(found.mappings.is_empty(), "{metadata}");
    }
}

#[test]
fn competing_export_or_category_does_not_establish_a_type_mapping() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs"}"#;
    for (types, implementation) in [
        (
            "export function format(value: string): string;",
            "function first(value) { return value; } function second(value) { return value; } export { first as format, second as format };",
        ),
        (
            "function first(value: string): string; function second(value: number): string; export { first as format, second as format };",
            "export function format(value) { return value; }",
        ),
        (
            "export function format(value: string): string;",
            "export const format = 1;",
        ),
        (
            "export function format(value: string): string;",
            "function format(value) { return value; }",
        ),
    ] {
        let found = mixed_entries(metadata, types, implementation);
        assert!(found.mappings.is_empty(), "{types} / {implementation}");
    }
}

#[test]
fn repeated_declarations_of_one_export_keep_one_canonical_target() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs"}"#;
    let found = mixed_entries(
        metadata,
        "export function format(value: string): string; export function format(value: number): string;",
        "export function format(value) { return value; }",
    );
    let mappings = &found.mappings["index.d.ts"];
    assert_eq!(mappings.len(), 1);
    let target = &found.anchors["index.mjs"]["format"];
    assert!(mappings.values().all(|mapping| &mapping.identity == target));
}

#[test]
fn captured_callable_variable_maps_to_one_defining_function() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs"}"#;
    let found = mixed_entries(
        metadata,
        "export declare const parse: (text: string) => Node;",
        "export function parse(text) { return text; }",
    );
    assert_eq!(
        found.mappings["index.d.ts"]["parse"].identity,
        found.anchors["index.mjs"]["parse"]
    );
    for source in [
        "export declare const parse: string;",
        "export declare const parse: { call: (text: string) => Node };",
    ] {
        let found = mixed_entries(
            metadata,
            source,
            "export function parse(text) { return text; }",
        );
        assert!(found.mappings.is_empty(), "{source}");
    }
}

#[test]
fn captured_variable_contract_keeps_category_and_unique_export_proof() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","main":"./index.mjs"}"#;
    let found = mixed_entries(
        metadata,
        "export declare const version: string;",
        "export const version = '3.8.5';",
    );
    assert_eq!(
        found.mappings["index.d.ts"]["version"].identity,
        found.anchors["index.mjs"]["version"]
    );
    for implementation in [
        "export function version() { return '3.8.5'; }",
        "const first = '3.8.5', second = 'other'; export { first as version, second as version };",
    ] {
        let found = mixed_entries(
            metadata,
            "export declare const version: string;",
            implementation,
        );
        assert!(found.mappings.is_empty(), "{implementation}");
    }
    let found = mixed_entries(
        metadata,
        "export declare const version: () => string;",
        "export const version = '3.8.5';",
    );
    assert!(found.mappings.is_empty());
}

fn prepared_project(
    owner: &rift_protocol::identity::SymbolOwner,
    metadata: Option<&str>,
) -> super::PreparedNamespace {
    let path = ProjectPath::new("index.d.ts").expect("selected path");
    let metadata_path = ProjectPath::new("package.json").expect("metadata path");
    let source = "export declare function format(text: string): string;";
    let files = [PackageSource::new(&path, source)];
    let context = metadata
        .map(|text| PackageSource::new(&metadata_path, text))
        .into_iter()
        .collect::<Vec<_>>();
    let language = ShippedLanguage::TypeScript.language();
    let input = crate::NamespaceInput::project(
        owner,
        &language,
        &files,
        &context,
        &[],
        ExactPackageLimits::new(20, 16 * 1024 * 1024),
    )
    .expect("captured project input");
    let facts = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript)
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            input.syntax(),
        )
        .expect("selected project syntax")
        .into_facts();
    let selected = [super::SelectedFile {
        path: &path,
        source,
        syntax: &facts,
    }];
    let unit = rift_syntax::source_unit_for_path(&path).expect("physical project unit");
    let result = super::prepare(&input, &selected);
    assert_eq!(
        rift_syntax::source_unit_for_path(&path).expect("unchanged unit"),
        unit
    );
    result
}

#[test]
fn captured_project_metadata_keeps_local_and_named_owners_distinct() {
    use rift_protocol::identity::SymbolOwner;
    let local = prepared_project(&SymbolOwner::Local, Some(TYPES));
    let owner = SymbolOwner::NamedLocal {
        name: "cloud".to_owned(),
    };
    let named = prepared_project(&owner, Some(TYPES));
    let local_id = &local.anchors["index.d.ts"]["format"];
    let named_id = &named.anchors["index.d.ts"]["format"];
    assert_ne!(local_id, named_id);
    assert_eq!(
        SymbolIdentity::parse(local_id.as_str())
            .expect("local ID")
            .owner(),
        &SymbolOwner::Local
    );
    assert_eq!(
        SymbolIdentity::parse(named_id.as_str())
            .expect("named ID")
            .owner(),
        &owner
    );
}

#[test]
fn local_owner_without_captured_entry_metadata_uses_relative_module() {
    let result = prepared_project(&rift_protocol::identity::SymbolOwner::Local, None);
    assert_eq!(
        result.anchors["index.d.ts"]["format"].as_str(),
        "rift://symbol/local/typescript/.%2Findex.d.ts/format"
    );
    assert!(result.exports.is_empty());
    assert!(result.mappings.is_empty());
}

#[test]
fn explicit_export_subpaths_establish_independent_captured_modules() {
    let found = prepared(
        ShippedLanguage::TypeScript,
        &[(
            "package.json",
            r#"{"name":"prettier","version":"3.8.5","exports":{".":"./index.ts","./hub":"./hub.ts","./walk":"./walk.ts"}}"#,
        )],
        &[
            ("index.ts", "export function format() {}"),
            ("hub.ts", "export function beacon() {}"),
            (
                "walk.ts",
                "import { beacon } from './hub'; export function larger() { beacon(); }",
            ),
        ],
    );
    for (path, name, module) in [
        ("index.ts", "format", "prettier"),
        ("hub.ts", "beacon", "prettier/hub"),
        ("walk.ts", "larger", "prettier/walk"),
    ] {
        let identity = SymbolIdentity::parse(found.anchors[path][name].as_str())
            .expect("captured export identity");
        assert_eq!(
            identity.qualified_path(),
            &[module.to_owned(), name.to_owned()]
        );
    }
    assert!(!found.unresolved_exports);
}

#[test]
fn missing_conditional_and_pattern_exports_leave_sources_unresolved() {
    for target in [
        r#"{"./hub":{"browser":"./hub.ts"}}"#,
        r#"{"./*":"./hub.ts"}"#,
        r#"{"./hub":"./missing.ts"}"#,
        r#"{"./hub":"../hub.ts"}"#,
        r#"{"./hub":"./node_modules/hub.ts"}"#,
    ] {
        let metadata = format!(r#"{{"name":"prettier","version":"3.8.5","exports":{target}}}"#);
        let found = prepared(
            ShippedLanguage::TypeScript,
            &[("package.json", metadata.as_str())],
            &[("hub.ts", "export function beacon() {}")],
        );
        assert!(
            in_path_module(&found.anchors["hub.ts"], "prettier/hub.ts"),
            "{metadata}"
        );
        assert!(found.unresolved_exports, "{metadata}");
    }
}

#[test]
fn conflicting_export_subpaths_refuse_one_physical_module_assignment() {
    let found = prepared(
        ShippedLanguage::TypeScript,
        &[(
            "package.json",
            r#"{"name":"prettier","version":"3.8.5","exports":{"./hub":"./hub.ts","./other":"./hub.ts"}}"#,
        )],
        &[("hub.ts", "export function beacon() {}")],
    );
    assert!(found.anchors.is_empty());
    assert!(found.unresolved_exports);
    let duplicate: Result<super::PackageMetadata, _> = serde_json::from_str(
        r#"{"name":"prettier","version":"3.8.5","exports":{"./hub":"./hub.ts","./hub":"./other.ts"}}"#,
    );
    assert!(duplicate.is_err(), "duplicate captured export key refuses");
}

#[test]
fn raw_anonymous_export_bindings_keep_checked_physical_identities() {
    let metadata = [(
        "package.json",
        r#"{"name":"prettier","version":"3.8.5","main":"./index.js"}"#,
    )];
    for (source, established) in [
        ("exports.open = function () {}; function kept() {}", true),
        (
            "function outer(exports) { exports.open = function () {}; } function kept() {}",
            false,
        ),
        (
            "(function (exports) { exports.open = function () {}; })(other); function kept() {}",
            false,
        ),
    ] {
        let prepared = prepared(
            ShippedLanguage::JavaScript,
            &metadata,
            &[("index.js", source)],
        );
        let anchors = prepared.anchors.get("index.js").expect("captured module");
        assert!(anchors.contains_key("kept"));
        assert_eq!(anchors.contains_key("open"), established);
        assert!(
            !prepared
                .public_names
                .get("index.js")
                .is_some_and(|names| names.contains("open"))
        );
    }
}

/// The release every ambient fixture analyzes, matching the captured `package.json`.
const AMBIENT_RELEASE: &str = "26.6.4";

fn ambient_prepared(
    owner: &rift_protocol::identity::SymbolOwner,
    metadata: Option<&str>,
    source: &str,
) -> (
    super::PreparedNamespace,
    std::sync::Arc<rift_syntax::SyntaxFacts>,
) {
    use rift_protocol::{identity::SymbolOwner, read::RuntimeIdentity};
    let path = ProjectPath::new("buffer.d.ts").expect("original path");
    let metadata_path = ProjectPath::new("package.json").expect("metadata path");
    let files = [PackageSource::new(&path, source)];
    let context = metadata
        .map(|text| PackageSource::new(&metadata_path, text))
        .into_iter()
        .collect::<Vec<_>>();
    let location = match owner {
        SymbolOwner::Package {
            manager,
            registry,
            name,
        } => SourceLocation::Dependency {
            package: PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: AMBIENT_RELEASE.to_owned(),
            },
        },
        SymbolOwner::Runtime { runtime } => SourceLocation::Stdlib {
            runtime: Some(RuntimeIdentity {
                runtime: runtime.clone(),
                version: AMBIENT_RELEASE.to_owned(),
            }),
        },
        _ => SourceLocation::Project { package: None },
    };
    let origin = ContributionOrigin::new(Some(location), SourceKind::Authored)
        .expect("captured owner origin");
    let language = ShippedLanguage::TypeScript.language();
    let limits = ExactPackageLimits::new(20, 16 * 1024 * 1024);
    let input = if matches!(
        owner,
        SymbolOwner::Local | SymbolOwner::NamedLocal { .. } | SymbolOwner::ProfileLocal { .. }
    ) {
        crate::NamespaceInput::project(owner, &language, &files, &context, &[], limits)
            .expect("captured local input")
    } else {
        ExactPackageInput::new(owner, &language, &origin, &files, limits)
            .expect("captured release input")
            .with_framework_context(&context, &[])
            .expect("original metadata")
            .namespace_input()
    };
    let facts = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript)
        .analyze(
            SyntaxSource {
                path: &path,
                text: source,
            },
            input.syntax(),
        )
        .expect("original source parses")
        .into_facts();
    let selected = [super::SelectedFile {
        path: &path,
        source,
        syntax: &facts,
    }];
    (super::prepare(&input, &selected), facts)
}

fn types_node_owner() -> rift_protocol::identity::SymbolOwner {
    SymbolIdentity::parse("rift://symbol/npm/npmjs.org/@types/node/typescript/node:buffer/isUtf8")
        .expect("exact declaration package owner")
        .owner()
        .clone()
}

#[test]
fn package_ambient_original_buffer_keeps_owner_ranges_and_unknown_entry() {
    let source = include_str!("../../../../tests/fixtures/types-node-buffer.d.ts");
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    let owner = types_node_owner();
    let (found, facts) = ambient_prepared(&owner, Some(metadata), source);
    assert!(!facts.has_errors());
    let declaration = facts
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "isUtf8")
        .expect("original callable declaration");
    let statement = "function isUtf8(input: ArrayBuffer | NodeJS.TypedArray): boolean;";
    let start = source.find(statement).expect("original callable source");
    let end = start + statement.len();
    assert_eq!(
        (declaration.item_range.start, declaration.item_range.end),
        (
            u64::try_from(start).expect("fixture offset fits"),
            u64::try_from(end).expect("fixture offset fits")
        )
    );
    assert_eq!(source.get(start..end), Some(statement));
    assert_eq!(declaration.container.as_deref(), Some("\"node:buffer\""));
    let identity = &found.anchors["buffer.d.ts"][&declaration.qualified_name];
    let parsed = SymbolIdentity::parse(identity.as_str()).expect("Package ambient identity");
    assert_eq!(parsed.owner(), &owner);
    assert_eq!(parsed.language().name, "typescript");
    assert_eq!(parsed.qualified_path(), ["node:buffer", "isUtf8"]);
    assert_eq!(
        facts.source_digest(),
        Some(&rift_core::FileDigest::of(source.as_bytes()))
    );
    assert!(
        found.mappings.is_empty(),
        "no Runtime or JavaScript association"
    );
    assert!(
        !found.anchors["buffer.d.ts"]
            .keys()
            .any(|name| name.starts_with("global."))
    );
    let metadata: super::PackageMetadata =
        serde_json::from_str(metadata).expect("original metadata");
    assert!(
        super::entry_path(&metadata, "typescript").is_none(),
        "compiler applicability remains unknown"
    );
}

#[test]
fn package_ambient_requires_metadata_and_local_declarations_keep_their_owner() {
    use rift_protocol::identity::SymbolOwner;
    let source =
        "declare module \"node:buffer\" { export function isUtf8(input: unknown): boolean; }";
    let original = include_str!("../../../../tests/fixtures/types-node-package.json");
    for metadata in [
        None,
        Some("not JSON"),
        Some(r#"{"name":"other","version":"26.6.4"}"#),
        Some(r#"{"name":"@types/node","version":"26.6.3"}"#),
    ] {
        let (found, facts) = ambient_prepared(&types_node_owner(), metadata, source);
        assert!(facts.symbols().iter().any(|symbol| symbol.name == "isUtf8"));
        assert!(found.anchors.is_empty(), "{metadata:?}");
    }
    for owner in [
        SymbolOwner::Local,
        SymbolOwner::NamedLocal {
            name: "node".to_owned(),
        },
        SymbolOwner::Runtime {
            runtime: "node".to_owned(),
        },
    ] {
        let (found, facts) = ambient_prepared(&owner, Some(original), source);
        assert!(facts.symbols().iter().any(|symbol| symbol.name == "isUtf8"));
        if owner.is_local() {
            let identity = &found.anchors["buffer.d.ts"]["\"node:buffer\".isUtf8"];
            assert_eq!(
                SymbolIdentity::parse(identity.as_str())
                    .expect("local declaration")
                    .owner(),
                &owner
            );
        } else {
            assert!(found.anchors.is_empty(), "{owner:?}");
        }
        assert!(found.mappings.is_empty());
    }
}

#[test]
fn package_ambient_global_unquoted_and_outside_remain_unknown() {
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    for source in [
        "declare namespace NodeJS { function isUtf8(): boolean; }",
        "declare global { function isUtf8(): boolean; }",
    ] {
        let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), source);
        assert!(found.anchors.is_empty(), "{source}");
    }
    let module = "@types/node/buffer.d.ts";
    let source = "export function isUtf8(): boolean;";
    let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), source);
    assert!(in_path_module(&found.anchors["buffer.d.ts"], module));
    let source = "export function outside(): boolean;\ndeclare module \"node:buffer\" { export function isUtf8(): boolean; global { interface GlobalMember {} } }\n";
    let (found, facts) = ambient_prepared(&types_node_owner(), Some(metadata), source);
    assert!(!facts.has_errors());
    let anchors = &found.anchors["buffer.d.ts"];
    assert!(anchors.contains_key("\"node:buffer\".isUtf8"));
    assert!(in_path_module(
        &BTreeMap::from([("outside".to_owned(), anchors["outside"].clone())]),
        module
    ));
    assert!(!anchors.keys().any(|name| name.starts_with("global.")));
}

#[test]
fn package_ambient_module_keeps_id_with_malformed_members() {
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    for source in [
        "declare module \"node:buffer\" { export function isUtf8(input: ): boolean; }",
        "declare module \"node:buffer\" { global { interface Broken { value: } } }",
    ] {
        let (found, facts) = ambient_prepared(&types_node_owner(), Some(metadata), source);
        assert!(facts.has_errors());
        let anchors = &found.anchors["buffer.d.ts"];
        assert_eq!(
            anchors.keys().map(String::as_str).collect::<Vec<_>>(),
            ["\"node:buffer\""],
            "{source}"
        );
        let identity =
            SymbolIdentity::parse(anchors["\"node:buffer\""].as_str()).expect("module identity");
        assert_eq!(identity.qualified_path(), ["node:buffer"]);
    }
}

#[test]
fn package_ambient_overloads_and_repeated_bodies_share_one_identity() {
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    let overloads = "declare module \"node:buffer\" { export function read(value: string): string; export function read(value: number): string; }";
    let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), overloads);
    let identities = found.anchors["buffer.d.ts"]
        .iter()
        .filter(|(name, _)| name.contains(".read"))
        .map(|(_, identity)| identity)
        .collect::<Vec<_>>();
    assert_eq!(
        identities.len(),
        1,
        "every declaration spells one qualified name"
    );
    let repeated =
        "declare module \"node:buffer\" { export function read() {} export function read() {} }";
    let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), repeated);
    let identities = found.anchors["buffer.d.ts"]
        .iter()
        .filter(|(name, _)| name.contains(".read"))
        .map(|(_, identity)| identity)
        .collect::<Vec<_>>();
    assert_eq!(
        identities.len(),
        1,
        "every declaration spells one qualified name"
    );
}

fn commonjs_prepared(
    metadata: &str,
    files: &[(&str, &str)],
    entry: &str,
    conditions: &[&str],
) -> (
    super::PreparedNamespace,
    Vec<std::sync::Arc<rift_syntax::SyntaxFacts>>,
) {
    commonjs_observed(metadata, files, &[(entry, conditions)])
}

fn commonjs_observed(
    metadata: &str,
    files: &[(&str, &str)],
    observed: &[(&str, &[&str])],
) -> (
    super::PreparedNamespace,
    Vec<std::sync::Arc<rift_syntax::SyntaxFacts>>,
) {
    let owner = SymbolIdentity::parse("rift://symbol/npm/npmjs.org/react/javascript/react/open")
        .expect("captured package owner")
        .owner()
        .clone();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "npm".to_owned(),
                registry: "npmjs.org".to_owned(),
                name: "react".to_owned(),
                version: "19.3.0".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("original package origin");
    let paths = files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("original path"))
        .collect::<Vec<_>>();
    let sources = paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let metadata_path = ProjectPath::new("package.json").expect("metadata path");
    let context = [PackageSource::new(&metadata_path, metadata)];
    let observations = observed
        .iter()
        .map(|(entry, conditions)| {
            let implementation = sources
                .iter()
                .find(|source| source.path().as_str() == *entry)
                .copied()
                .expect("selected original entry");
            crate::NamespaceModule::new("react", implementation, &[], conditions)
        })
        .collect::<Vec<_>>();
    let language = ShippedLanguage::JavaScript.language();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(20, 16 * 1024 * 1024),
    )
    .expect("bounded original sources")
    .with_framework_context(&context, &[])
    .expect("original metadata")
    .with_modules(&observations)
    .expect("captured resolver conditions");
    let facts = sources
        .iter()
        .map(|source| {
            JavaScriptSyntaxProvider::default()
                .analyze(
                    SyntaxSource {
                        path: source.path(),
                        text: source.text(),
                    },
                    input.limits().syntax(),
                )
                .expect("original JavaScript parses")
                .into_facts()
        })
        .collect::<Vec<_>>();
    let selected = sources
        .iter()
        .zip(&facts)
        .map(|(source, syntax)| super::SelectedFile {
            path: source.path(),
            source: source.text(),
            syntax,
        })
        .collect::<Vec<_>>();
    (super::prepare(&input.namespace_input(), &selected), facts)
}

#[test]
fn commonjs_original_react_builds_keep_distinct_modules_and_public_bindings() {
    let files = [
        (
            "index.js",
            include_str!("../../../../tests/fixtures/react/index.js"),
        ),
        (
            "cjs/react.development.js",
            include_str!("../../../../tests/fixtures/react/cjs/react.development.js"),
        ),
        (
            "cjs/react.production.js",
            include_str!("../../../../tests/fixtures/react/cjs/react.production.js"),
        ),
    ];
    let (prepared, facts) = commonjs_prepared(
        include_str!("../../../../tests/fixtures/react/package.json"),
        &files,
        "index.js",
        &["node", "require"],
    );
    assert!(
        prepared.unresolved_exports,
        "conditional facade remains unknown"
    );
    let mut identities = Vec::new();
    for (index, path) in ["cjs/react.development.js", "cjs/react.production.js"]
        .iter()
        .enumerate()
    {
        let declaration = facts[index + 1]
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "useState")
            .expect("original raw useState");
        // The production build assigns in the module body; the development build assigns
        // inside a conditional function expression.
        assert_eq!(
            declaration
                .kinds
                .contains(&rift_protocol::read::SymbolKind::Public),
            path.ends_with("production.js"),
            "{path}"
        );
        let anchors = prepared
            .anchors
            .get(*path)
            .expect("resolved defining module");
        identities.push(
            anchors
                .get(&declaration.qualified_name)
                .expect("checked useState identity")
                .clone(),
        );
        assert!(prepared.public_names[*path].contains(&declaration.qualified_name));
        assert!(
            facts[index + 1]
                .export_bindings()
                .expect("original bindings")
                .iter()
                .any(|binding| binding.range == declaration.item_range
                    && binding.local == declaration.name_range)
        );
    }
    assert_ne!(
        identities[0], identities[1],
        "different resolved CommonJS files stay distinct"
    );
    assert!(!prepared.anchors["index.js"].contains_key("useState"));
}

#[test]
fn commonjs_receiver_shadow_reassignment_qualified_and_ordinary_functions_remain_unknown() {
    let metadata = r#"{"name":"react","version":"19.3.0","main":"index.js"}"#;
    for source in [
        "const exports={}; exports.open=function(){};",
        "exports={}; exports.open=function(){};",
        "module={exports:{}}; module.exports.open=function(){};",
        "module.exports={}; exports.open=function(){};",
        "exports.open=function(){}; module.exports={};",
        "module.exports=require('./other.js'); exports.open=function(){};",
        "const {exports}=other; exports.open=function(){};",
        "function outer(exports){exports.open=function(){};}",
        "(function(exports){exports.open=function(){};})(other);",
        "function outer(){exports.open=function(){};}",
        "window.exports.open=function(){};",
        "eval('exports = other'); exports.open=function(){};",
    ] {
        let (prepared, _) = commonjs_prepared(
            metadata,
            &[("index.js", source)],
            "index.js",
            &["node", "require"],
        );
        assert!(
            !prepared
                .anchors
                .get("index.js")
                .is_some_and(|anchors| anchors.contains_key("open")),
            "{source}"
        );
        assert!(
            !prepared
                .public_names
                .get("index.js")
                .is_some_and(|names| names.contains("open")),
            "{source}"
        );
    }
    for source in [
        "exports.open=function(){};",
        "(function(){exports.open=function(){};})();",
        "if(condition){exports.open=function(){};}",
    ] {
        let (prepared, _) = commonjs_prepared(
            metadata,
            &[("index.js", source)],
            "index.js",
            &["node", "require"],
        );
        assert!(
            prepared.anchors["index.js"].contains_key("open"),
            "{source}"
        );
        assert!(
            prepared.public_names["index.js"].contains("open"),
            "{source}"
        );
    }
}

#[test]
fn commonjs_public_exports_require_entry_conditions_owner_and_mode() {
    let source = "exports.open=function(){};";
    for (metadata, conditions, module) in [
        (
            r#"{"name":"wrong","version":"19.3.0","main":"index.js"}"#,
            &["node", "require"][..],
            None,
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"other.js"}"#,
            &["node", "require"][..],
            Some("react/index.js"),
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js","type":"module"}"#,
            &["node", "require"][..],
            Some("react"),
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js"}"#,
            &[][..],
            Some("react"),
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js"}"#,
            &["import"][..],
            Some("react"),
        ),
    ] {
        let (prepared, _) =
            commonjs_prepared(metadata, &[("index.js", source)], "index.js", conditions);
        match module {
            Some(module) => {
                let anchors = &prepared.anchors["index.js"];
                assert!(anchors.contains_key("open"), "{metadata}");
                assert!(in_path_module(anchors, module), "{metadata}");
            }
            None => assert!(prepared.anchors.is_empty(), "{metadata}"),
        }
        assert!(prepared.public_names.is_empty());
        assert!(prepared.exports.is_empty());
        assert!(prepared.unresolved_exports);
    }
}

#[test]
fn commonjs_literal_forwarding_missing_dynamic_shadow_and_cycles_stay_bounded() {
    let metadata = r#"{"name":"react","version":"19.3.0","main":"index.js"}"#;
    for entry in [
        "module.exports=require(target);",
        "const require=other; module.exports=require('./child.js');",
        "module.exports=require('../child.js');",
        "module.exports=require('./missing.js');",
    ] {
        let (prepared, _) = commonjs_prepared(
            metadata,
            &[
                ("index.js", entry),
                ("child.js", "exports.open=function(){};"),
            ],
            "index.js",
            &["node", "require"],
        );
        assert!(!prepared.public_names.contains_key("child.js"), "{entry}");
        assert!(
            in_path_module(&prepared.anchors["child.js"], "react/child.js"),
            "{entry}"
        );
    }
    let (prepared, _) = commonjs_prepared(
        metadata,
        &[
            (
                "index.js",
                "module.exports=require('./child.js'); if(condition){module.exports=require('./cycle.js');}",
            ),
            ("child.js", "exports.open=function(){};"),
            ("cycle.js", "module.exports=require('./index.js');"),
        ],
        "index.js",
        &["node", "require"],
    );
    assert!(prepared.anchors["child.js"].contains_key("open"));
    assert!(prepared.unresolved_exports);
}

#[test]
fn commonjs_package_default_entry_and_extensionless_forwarder_are_checked_independently() {
    let default = r#"{"name":"react","version":"19.3.0"}"#;
    let selected = r#"{"name":"react","version":"19.3.0","main":"index.js"}"#;
    let other_entry = r#"{"name":"react","version":"19.3.0","main":"other.js"}"#;
    let exports = r#"{"name":"react","version":"19.3.0","main":"index.js","exports":{".":{"require":"./other.js"}}}"#;
    for (metadata, literal, accepted) in [
        (default, "./lib/child.js", true),
        (selected, "./lib/child", true),
        (default, "./lib/child", true),
        (other_entry, "./lib/child.js", false),
        (exports, "./lib/child", false),
    ] {
        let forward = format!("module.exports = require('{literal}');");
        let (prepared, _) = commonjs_prepared(
            metadata,
            &[
                ("index.js", &forward),
                ("lib/child.js", "exports.open = function open() {};"),
            ],
            "index.js",
            &["node", "require"],
        );
        assert_eq!(
            prepared.public_names.contains_key("lib/child.js"),
            accepted,
            "{metadata} {literal}"
        );
        assert!(
            in_path_module(&prepared.anchors["lib/child.js"], "react/lib/child.js"),
            "{metadata} {literal}"
        );
    }
}

#[test]
fn commonjs_forwarding_checks_the_exact_file_before_adding_a_javascript_extension() {
    let (prepared, _) = commonjs_prepared(
        r#"{"name":"react","version":"19.3.0","main":"index.js"}"#,
        &[
            ("index.js", "module.exports = require('./lib/child');"),
            ("lib/child", "exports.exact = function exact() {};"),
            ("lib/child.js", "exports.fallback = function fallback() {};"),
        ],
        "index.js",
        &["node", "require"],
    );
    assert!(prepared.anchors["lib/child"].contains_key("exact"));
    assert!(!prepared.public_names.contains_key("lib/child.js"));
    assert!(in_path_module(
        &prepared.anchors["lib/child.js"],
        "react/lib/child.js"
    ));
}

/// The states of the paths a manifest requests, over every member a package ships.
fn shipped_states(manifest: &str, members: &[&str]) -> PathStates {
    let path = ProjectPath::new("package.json").expect("manifest path");
    super::build_paths(PackageSource::new(&path, manifest), 16 * 1024)
        .unwrap_or_default()
        .into_iter()
        .map(|path| {
            let folder = format!("{}/", path.0);
            let kind = if members.contains(&path.0.as_str()) {
                Some(crate::ArchiveMemberKind::File)
            } else {
                members
                    .iter()
                    .any(|member| member.starts_with(&folder))
                    .then_some(crate::ArchiveMemberKind::Directory)
            };
            (path, kind)
        })
        .collect()
}

/// The files published under the package entry, with the members the package ships observed.
fn entry_files(
    language: ShippedLanguage,
    metadata: &str,
    files: &[&str],
    members: &[&str],
) -> (Vec<String>, bool) {
    let manifest = format!(r#"{{"name":"prettier","version":"3.8.5"{metadata}}}"#);
    let states = shipped_states(&manifest, members);
    entry_files_observed(language, &manifest, files, Some(&states))
}

fn entry_files_observed(
    language: ShippedLanguage,
    manifest: &str,
    files: &[&str],
    observed: Option<&PathStates>,
) -> (Vec<String>, bool) {
    let text = if language == ShippedLanguage::TypeScript {
        "export declare function load(): void;"
    } else {
        "exports.load = function () {};"
    };
    let files = files.iter().map(|path| (*path, text)).collect::<Vec<_>>();
    let found = prepared_observed(
        language,
        &[("package.json", manifest)],
        &files,
        None,
        observed,
    );
    let placed = files
        .iter()
        .filter(|(path, _)| {
            found
                .anchors
                .get(*path)
                .and_then(|anchors| anchors.get("load"))
                .is_some_and(|identity| {
                    SymbolIdentity::parse(identity.as_str())
                        .is_ok_and(|identity| identity.qualified_path() == ["prettier", "load"])
                })
        })
        .map(|(path, _)| (*path).to_owned())
        .collect();
    (placed, found.unresolved_exports)
}

#[test]
fn package_entries_follow_node_resolution_over_observed_members() {
    let javascript = ShippedLanguage::JavaScript;
    for (metadata, members, entry) in [
        (r#","main":"index.js""#, vec!["index.js"], Some("index.js")),
        (
            r#","type":"commonjs","main":"index.js""#,
            vec!["index.js"],
            Some("index.js"),
        ),
        (
            r#","main":"./index.cjs""#,
            vec!["index.cjs"],
            Some("index.cjs"),
        ),
        // ms@2.1.3 writes its entry this way.
        (r#","main":"./index""#, vec!["index.js"], Some("index.js")),
        (r#","main":"index""#, vec!["index.js"], Some("index.js")),
        (r#","main":".""#, vec!["index.js"], Some("index.js")),
        ("", vec!["index.js"], Some("index.js")),
        (r#","main":"""#, vec!["index.js"], Some("index.js")),
        (
            r#","main":"lib""#,
            vec!["index.js", "lib/index.js"],
            Some("lib/index.js"),
        ),
        (
            r#","main":"./lib/""#,
            vec!["index.js", "lib/index.js"],
            Some("lib/index.js"),
        ),
        (
            r#","main":"lib//index""#,
            vec!["index.js", "lib/index.js"],
            Some("lib/index.js"),
        ),
        (
            r#","main":"./lib/./index.js""#,
            vec!["lib/index.js"],
            Some("lib/index.js"),
        ),
        (
            r#","main":"lib/../index""#,
            vec!["index.js", "lib/index.js"],
            Some("index.js"),
        ),
        (r#","main":"./gone""#, vec!["index.js"], Some("index.js")),
        (
            r#","main":"index""#,
            vec!["index", "index.js"],
            Some("index"),
        ),
        (
            r#","main":"./data.json""#,
            vec!["index.js", "data.json"],
            None,
        ),
        (
            r#","main":"./native""#,
            vec!["index.js", "native.node"],
            None,
        ),
        (r#","main":"./gone""#, vec!["other.js"], None),
        (r#","main":"../index""#, vec!["index.js"], None),
        (r#","main":"/index""#, vec!["index.js"], None),
        (r#","main":"lib""#, vec!["lib/other.js"], None),
        ("", vec!["other.js"], None),
        (
            r#","module":"./esm.js""#,
            vec!["index.js", "esm.js"],
            Some("index.js"),
        ),
        (r#","module":"./esm.js""#, vec!["esm.js"], Some("esm.js")),
        (
            r#","main":"./main.js","module":"./esm.js""#,
            vec!["main.js", "esm.js"],
            Some("main.js"),
        ),
        (
            r#","main":"./index","exports":{"./sub":"./sub.js"}"#,
            vec!["index.js"],
            None,
        ),
    ] {
        let files = members
            .iter()
            .copied()
            .filter(|path| !matches!(path.rsplit_once('.'), Some((_, "json" | "node"))))
            .collect::<Vec<_>>();
        let (placed, _) = entry_files(javascript, metadata, &files, &members);
        assert_eq!(
            placed,
            entry.into_iter().collect::<Vec<_>>(),
            "{metadata} {members:?}"
        );
    }
}

#[test]
fn package_entries_take_declared_types_in_the_compiler_order() {
    let typescript = ShippedLanguage::TypeScript;
    for (metadata, members, entry) in [
        (
            r#","types":"./index.d.ts""#,
            vec!["index.d.ts"],
            Some("index.d.ts"),
        ),
        (
            r#","types":"./index""#,
            vec!["index.d.ts"],
            Some("index.d.ts"),
        ),
        (
            r#","types":"./index""#,
            vec!["index.ts", "index.d.ts"],
            Some("index.ts"),
        ),
        (
            r#","typings":"lib""#,
            vec!["lib/index.d.ts"],
            Some("lib/index.d.ts"),
        ),
        ("", vec!["index.d.ts"], Some("index.d.ts")),
        ("", vec!["index.ts", "index.d.ts"], None),
        ("", vec!["index.tsx", "index.d.ts"], None),
        (r#","main":"""#, vec!["index.d.ts"], Some("index.d.ts")),
        (r#","main":".""#, vec!["index.d.ts"], Some("index.d.ts")),
        (
            r#","main":"./index.d.ts""#,
            vec!["index.d.ts"],
            Some("index.d.ts"),
        ),
        (
            r#","main":"lib/types.d.mts""#,
            vec!["index.d.ts", "lib/types.d.mts"],
            Some("lib/types.d.mts"),
        ),
        (r#","main":"../index.d.ts""#, vec!["index.d.ts"], None),
        (r#","main":"./lib.js""#, vec!["index.d.ts"], None),
    ] {
        let files = members
            .iter()
            .copied()
            .filter(|path| !matches!(path.rsplit_once('.'), Some((_, "tsx"))))
            .collect::<Vec<_>>();
        let (placed, _) = entry_files(typescript, metadata, &files, &members);
        assert_eq!(
            placed,
            entry.into_iter().collect::<Vec<_>>(),
            "{metadata} {members:?}"
        );
    }
}

/// A selection is no evidence of absence: an installed source captures the callee's file and
/// the manifest alone.
#[test]
fn package_entries_refuse_a_path_whose_state_is_not_captured() {
    let javascript = ShippedLanguage::JavaScript;
    let manifest = |metadata: &str| format!(r#"{{"name":"prettier","version":"3.8.5"{metadata}}}"#);
    for metadata in [
        r#","main":"dist/a.js""#,
        r#","main":"./index""#,
        r#","main":"./data.json""#,
        r#","main":"./native""#,
        r#","main":"lib""#,
    ] {
        let manifest = manifest(metadata);
        let (placed, unresolved) = entry_files_observed(javascript, &manifest, &["index.js"], None);
        assert!(placed.is_empty(), "{metadata}");
        assert!(unresolved, "{metadata}");
    }
    // One earlier path without a state is enough; a link's target is not captured either.
    let written = manifest(r#","main":"dist/a""#);
    for (path, state) in [
        ("dist/a.js", None),
        ("dist/a", Some(Some(crate::ArchiveMemberKind::Link))),
    ] {
        let mut states = shipped_states(&written, &["index.js"]);
        states.remove(&rift_protocol::read::ProjectPath("dist".to_owned()));
        let key = rift_protocol::read::ProjectPath(path.to_owned());
        match state {
            Some(state) => {
                states.insert(key, state);
            }
            None => {
                states.remove(&key);
            }
        }
        let (placed, unresolved) =
            entry_files_observed(javascript, &written, &["index.js"], Some(&states));
        assert!(placed.is_empty() && unresolved, "{path}");
    }
    let (placed, unresolved) = entry_files_observed(javascript, &manifest(""), &["other.js"], None);
    assert!(
        placed.is_empty() && unresolved,
        "an unknown index.js names no entry"
    );
    let (placed, _) = entry_files_observed(javascript, &manifest(""), &["index.js"], None);
    assert_eq!(
        placed,
        ["index.js"],
        "the first candidate needs no other evidence"
    );
    let (placed, _) = entry_files_observed(
        javascript,
        &manifest(r#","module":"./esm.js""#),
        &["esm.js"],
        None,
    );
    assert_eq!(
        placed,
        ["esm.js"],
        "a package with only module keeps its entry"
    );
}

#[test]
fn package_entry_paths_are_requested_within_the_source_bound() {
    let path = ProjectPath::new("package.json").expect("manifest path");
    let manifest = r#"{"name":"prettier","version":"3.8.5","main":"lib"}"#;
    let requested = |source_bytes_max| {
        super::build_paths(PackageSource::new(&path, manifest), source_bytes_max)
    };
    let paths = requested(manifest.len()).expect("requested paths");
    assert!(paths.iter().any(|path| path.0 == "lib/index.js"));
    assert!(
        paths.len() <= 16,
        "a manifest requests a fixed number of paths"
    );
    assert!(requested(manifest.len() - 1).is_none());
    let nested = ProjectPath::new("nested/package.json").expect("nested manifest path");
    assert!(super::build_paths(PackageSource::new(&nested, manifest), 1024).is_none());
}

/// An observed `import` build holds the module; the file `main` names takes no second copy.
#[test]
fn observed_module_builds_hold_their_module_before_main() {
    let metadata = r#"{"name":"react","version":"19.3.0","types":"./index.d.ts","main":"./main.js","module":"./esm.js"}"#;
    let files = [
        ("main.js", "export function load() {}"),
        ("esm.js", "export function load() {}"),
        ("index.d.ts", "export declare function load(): void;"),
    ];
    let holders = |observed: Option<(&str, &[&str])>| {
        let observed = observed
            .map(|(entry, conditions)| (entry, conditions, true))
            .into_iter()
            .collect::<Vec<_>>();
        let prepared = observed_declarations(metadata, &files, &observed);
        prepared
            .anchors
            .iter()
            .filter(|(path, anchors)| {
                !path.ends_with(".d.ts")
                    && anchors
                        .get(".")
                        .is_some_and(|module| module.as_str().ends_with("/javascript/react"))
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(holders(None), ["main.js"]);
    assert_eq!(holders(Some(("esm.js", &["import"]))), ["esm.js"]);
    assert_eq!(holders(Some(("main.js", &["require"]))), ["main.js"]);
    // An observation the manifest does not describe decides nothing.
    assert_eq!(holders(Some(("esm.js", &["require"]))), ["main.js"]);
}

fn observed_declarations(
    metadata: &str,
    files: &[(&str, &str)],
    observed: &[(&str, &[&str], bool)],
) -> super::PreparedNamespace {
    let owner = SymbolIdentity::parse("rift://symbol/npm/npmjs.org/react/javascript/react/load")
        .expect("package owner")
        .owner()
        .clone();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: PackageIdentity {
                manager: "npm".to_owned(),
                registry: "npmjs.org".to_owned(),
                name: "react".to_owned(),
                version: "19.3.0".to_owned(),
            },
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let paths = files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("path"))
        .collect::<Vec<_>>();
    let sources = paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let source = |wanted: &str| {
        sources
            .iter()
            .find(|source| source.path().as_str() == wanted)
            .copied()
            .expect("selected source")
    };
    let declarations = [source("index.d.ts")];
    let observations = observed
        .iter()
        .map(|(entry, conditions, joined)| {
            let declarations = if *joined { &declarations[..] } else { &[] };
            crate::NamespaceModule::new("react", source(entry), declarations, conditions)
        })
        .collect::<Vec<_>>();
    let manifest = ProjectPath::new("package.json").expect("manifest path");
    let context = [PackageSource::new(&manifest, metadata)];
    let language = ShippedLanguage::JavaScript.language();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(20, 16 * 1024 * 1024),
    )
    .expect("bounded sources")
    .with_framework_context(&context, &[])
    .expect("manifest")
    .with_modules(&observations)
    .expect("observations");
    let facts = sources
        .iter()
        .map(|source| {
            let text = SyntaxSource {
                path: source.path(),
                text: source.text(),
            };
            if source.path().as_str().ends_with(".d.ts") {
                TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript)
                    .analyze(text, input.limits().syntax())
            } else {
                JavaScriptSyntaxProvider::default().analyze(text, input.limits().syntax())
            }
            .expect("source parses")
            .into_facts()
        })
        .collect::<Vec<_>>();
    let selected = sources
        .iter()
        .zip(&facts)
        .map(|(source, syntax)| super::SelectedFile {
            path: source.path(),
            source: source.text(),
            syntax,
        })
        .collect::<Vec<_>>();
    super::prepare(&input.namespace_input(), &selected)
}

/// The modules each file of a conditional package is published under, by language.
fn published_modules(
    exports: &str,
    files: &[(&str, &str)],
) -> (BTreeMap<String, Vec<String>>, bool) {
    let metadata = format!(r#"{{"name":"prettier","version":"3.8.5","exports":{exports}}}"#);
    let found = prepared_languages(
        ShippedLanguage::TypeScript,
        &[("package.json", metadata.as_str())],
        files,
        Some(
            &files
                .iter()
                .map(|(path, _)| {
                    if path.ends_with("ts") {
                        ShippedLanguage::TypeScript
                    } else {
                        ShippedLanguage::JavaScript
                    }
                })
                .collect::<Vec<_>>(),
        ),
    );
    let mut modules = BTreeMap::<String, Vec<String>>::new();
    for (path, anchors) in &found.anchors {
        if let Some(module) = anchors.get(".") {
            modules
                .entry(module.as_str().to_owned())
                .or_default()
                .push(path.clone());
        }
    }
    (modules, found.unresolved_exports)
}

/// The exports of one package entry, the files it ships, and the files that hold its
/// JavaScript and TypeScript modules.
type ConditionalEntry = (
    &'static str,
    &'static [&'static str],
    Option<&'static str>,
    Option<&'static str>,
);

const CONDITIONAL_ENTRIES: &[ConditionalEntry] = &[
    (r#""./index.js""#, &["index.js"], Some("index.js"), None),
    (
        r#"{"require":"./a.cjs","import":"./b.mjs"}"#,
        &["a.cjs", "b.mjs"],
        Some("b.mjs"),
        None,
    ),
    (
        r#"{"import":"./b.mjs","require":"./a.cjs"}"#,
        &["a.cjs", "b.mjs"],
        Some("b.mjs"),
        None,
    ),
    (
        r#"{"node":"./n.js","default":"./browser.js"}"#,
        &["n.js", "browser.js"],
        Some("n.js"),
        None,
    ),
    (
        r#"{"browser":"./w.js","default":"./d.js"}"#,
        &["w.js", "d.js"],
        Some("d.js"),
        None,
    ),
    (
        r#"{"@zod/source":"./s.ts","types":"./i.d.cts","import":"./i.js","require":"./i.cjs"}"#,
        &["s.ts", "i.d.cts", "i.js", "i.cjs"],
        Some("i.js"),
        Some("i.d.cts"),
    ),
    (
        r#"{"custom":"./index.js","types":"./index.d.ts","import":"./index.js","require":"./index.cjs"}"#,
        &["index.d.ts", "index.js", "index.cjs"],
        Some("index.js"),
        Some("index.d.ts"),
    ),
    (
        r#"{"import":{"types":"./i.d.mts","default":"./i.mjs"},"require":{"types":"./i.d.cts","default":"./i.cjs"}}"#,
        &["i.d.mts", "i.mjs", "i.d.cts", "i.cjs"],
        Some("i.mjs"),
        Some("i.d.mts"),
    ),
    (r#"{"require":"./a.cjs"}"#, &["a.cjs"], Some("a.cjs"), None),
    (r#"{"import":"./b.mjs"}"#, &["b.mjs"], Some("b.mjs"), None),
    (
        r#"{"default":"./d.js","import":"./b.mjs"}"#,
        &["d.js", "b.mjs"],
        Some("d.js"),
        None,
    ),
    (
        r#"{"types@>=5":"./n.d.ts","types":"./o.d.ts","default":"./d.js"}"#,
        &["n.d.ts", "o.d.ts", "d.js"],
        Some("d.js"),
        None,
    ),
    (
        r#"{"import":null,"default":"./d.js"}"#,
        &["d.js"],
        Some("d.js"),
        None,
    ),
    (r#"{"browser":"./w.js"}"#, &["w.js"], None, None),
    (
        r#"{"module-sync":"./m.js","require":"./a.cjs"}"#,
        &["m.js", "a.cjs"],
        Some("m.js"),
        None,
    ),
    (
        r#"{"node":{"import":"./n.mjs","require":"./n.cjs"},"default":"./d.js"}"#,
        &["n.mjs", "n.cjs", "d.js"],
        Some("n.mjs"),
        None,
    ),
];

/// One package entry names one file for each language, whatever order its conditions have.
#[test]
fn conditional_exports_hold_one_file_for_each_module() {
    let javascript = "export function load() {}";
    let types = "export declare function load(): void;";
    let source = |path: &'static str| {
        (
            path,
            if path.ends_with("ts") {
                types
            } else {
                javascript
            },
        )
    };
    for &(exports, files, script, declared) in CONDITIONAL_ENTRIES {
        let exports = if exports.starts_with('{') {
            format!(r#"{{".":{exports}}}"#)
        } else {
            exports.to_owned()
        };
        let files = files.iter().copied().map(source).collect::<Vec<_>>();
        let (modules, _) = published_modules(&exports, &files);
        for (language, expected) in [("javascript", script), ("typescript", declared)] {
            let held = modules
                .iter()
                .filter(|(module, _)| module.ends_with(&format!("/{language}/prettier")))
                .flat_map(|(_, paths)| paths.iter().map(String::as_str))
                .collect::<Vec<_>>();
            assert_eq!(
                held,
                expected.into_iter().collect::<Vec<_>>(),
                "{language} {exports}"
            );
        }
    }
}

/// A file reached by several subpaths is published under one module.
#[test]
fn export_subpaths_of_one_file_keep_one_module() {
    let load = "export function load() {}";
    for (exports, path, expected, unresolved) in [
        (
            r#"{".":"./index.js","./main":{"import":"./index.js"}}"#,
            "index.js",
            Some("prettier"),
            false,
        ),
        (
            r#"{".":{"import":"./a.js"},"./a":{"import":"./a.js"}}"#,
            "a.js",
            Some("prettier"),
            false,
        ),
        (
            r#"{"./b":{"import":"./a.js"},"./a":{"import":"./a.js"}}"#,
            "a.js",
            Some("prettier%2Fa"),
            false,
        ),
        (
            r#"{".":"./a.js","./a":"./a.js"}"#,
            "a.js",
            Some("prettier"),
            true,
        ),
        (
            r#"{"./a":{"default":"./a.js"},".":{"default":"./a.js"}}"#,
            "a.js",
            Some("prettier"),
            true,
        ),
        (
            r#"{"./sub":{"require":"./s.cjs","import":"./s.mjs"}}"#,
            "s.mjs",
            Some("prettier%2Fsub"),
            false,
        ),
    ] {
        let mut files = vec![(path, load)];
        if path == "s.mjs" {
            files.push(("s.cjs", load));
        }
        let (modules, unresolved_exports) = published_modules(exports, &files);
        let held = modules
            .iter()
            .map(|(module, paths)| (module.rsplit('/').next().expect("module"), paths.as_slice()))
            // The other file of a dual entry is named by its path.
            .filter(|row| *row != ("prettier%2Fs.cjs", ["s.cjs".to_owned()].as_slice()))
            .collect::<Vec<_>>();
        assert_eq!(
            held.len() < modules.len(),
            path == "s.mjs",
            "{exports} {modules:?}"
        );
        match expected {
            Some(module) => assert_eq!(held, [(module, [path.to_owned()].as_slice())], "{exports}"),
            None => assert!(held.is_empty(), "{exports}"),
        }
        if unresolved {
            assert!(unresolved_exports, "{exports}");
        }
    }
}

/// The resolver's file holds the module, and no second file shares it.
#[test]
fn observed_entries_hold_their_module_before_static_conditions() {
    let dual = r#"{"name":"react","version":"19.3.0","exports":{".":{"require":"./a.cjs","import":"./b.mjs"}}}"#;
    let browser = r#"{"name":"react","version":"19.3.0","exports":{".":{"browser":"./w.js"}}}"#;
    let files = [
        ("a.cjs", "exports.open = function () {};"),
        ("b.mjs", "export function open() {}"),
    ];
    for (observed, expected) in [
        (vec![("a.cjs", vec!["node", "require"])], vec!["a.cjs"]),
        (vec![("b.mjs", vec!["node", "import"])], vec!["b.mjs"]),
        (
            vec![
                ("a.cjs", vec!["node", "require"]),
                ("b.mjs", vec!["node", "import"]),
            ],
            vec!["b.mjs"],
        ),
    ] {
        let observations = observed
            .iter()
            .map(|(path, conditions)| (*path, conditions.as_slice()))
            .collect::<Vec<_>>();
        assert_eq!(
            observed_module_files(dual, &files, &observations),
            expected,
            "{observed:?}"
        );
    }
    let window = [("w.js", "exports.open = function () {};")];
    assert!(
        observed_module_files(browser, &window, &[("w.js", &["browser"])]).is_empty(),
        "only a [node, require] observation holds a module without declarations"
    );
}

#[test]
fn a_required_build_holds_the_module_no_import_build_takes() {
    let synchronous = r#"{"name":"react","version":"19.3.0","exports":{".":{"module-sync":"./m.js","import":"./i.mjs","require":"./c.cjs"}}}"#;
    let bundled = r#"{"name":"react","version":"19.3.0","exports":{".":{"browser":{"import":"./i.mjs"},"require":"./c.cjs"}}}"#;
    let build = "exports.open = function () {};";
    let module = "export function open() {}";
    for (metadata, files, import) in [
        (
            synchronous,
            vec![("m.js", module), ("i.mjs", module), ("c.cjs", build)],
            &["node", "import"][..],
        ),
        (
            bundled,
            vec![("i.mjs", module), ("c.cjs", build)],
            &["browser", "import"][..],
        ),
        (
            synchronous,
            vec![("i.mjs", "export function open("), ("c.cjs", build)],
            &["node", "import"][..],
        ),
    ] {
        let required: (&str, &[&str]) = ("c.cjs", &["node", "require"]);
        for observations in [
            vec![required],
            vec![required, ("i.mjs", import)],
            vec![("i.mjs", import), required],
        ] {
            assert_eq!(
                observed_module_files(metadata, &files, &observations),
                ["c.cjs"],
                "{metadata} {observations:?}"
            );
        }
    }
}

#[test]
fn an_observation_with_declarations_holds_its_module_before_an_import_build() {
    let metadata = r#"{"name":"react","version":"19.3.0","exports":{".":{"require":{"types":"./index.d.ts","default":"./a.cjs"},"import":"./b.mjs"}}}"#;
    let files = [
        ("a.cjs", "exports.load = function () {};"),
        ("b.mjs", "export function load() {}"),
        ("index.d.ts", "export declare function load(): void;"),
    ];
    let joined: (&str, &[&str], bool) = ("a.cjs", &["node", "require"], true);
    let import: (&str, &[&str], bool) = ("b.mjs", &["node", "import"], false);
    let bare: (&str, &[&str], bool) = ("a.cjs", &["node", "require"], false);
    for observed in [
        vec![joined],
        vec![joined, import],
        vec![import, joined],
        vec![import, joined, bare],
    ] {
        let prepared = observed_declarations(metadata, &files, &observed);
        assert_eq!(module_holders(&prepared), ["a.cjs"], "{observed:?}");
    }
}

fn observed_module_files(
    metadata: &str,
    files: &[(&str, &str)],
    observations: &[(&str, &[&str])],
) -> Vec<String> {
    let (prepared, _) = commonjs_observed(metadata, files, observations);
    module_holders(&prepared)
}

/// The JavaScript files that hold the package's own module.
fn module_holders(prepared: &super::PreparedNamespace) -> Vec<String> {
    prepared
        .anchors
        .iter()
        .filter(|(_, anchors)| {
            anchors
                .get(".")
                .is_some_and(|module| module.as_str().ends_with("/javascript/react"))
        })
        .map(|(path, _)| path.clone())
        .collect()
}
