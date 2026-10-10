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
    let owner = SymbolIdentity::parse(
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/typescript/prettier/format",
    )
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
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/typescript/prettier/format"
    );
    assert_eq!(
        found["SemVer.format"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/typescript/prettier/SemVer/format"
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
        !found.contains_key("index.mjs"),
        "unselected conditional implementation is unresolved"
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
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/javascript/prettier/format2"
    );
    assert_eq!(
        found["SemVer.format"].as_str(),
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/javascript/prettier/SemVer/format"
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
fn typescript_overload_signatures_bind_one_function_without_occurrence_suffix() {
    let source = "export function format(value: string): string;\nexport function format(value: number): string;";
    let found = one(TYPES, "index.d.ts", source);
    assert_eq!(found.len(), 2);
    assert!(found.values().all(|identity| identity.as_str()
        == "rift://symbol/npm/npmjs.org/prettier@3.8.5/typescript/prettier/format"));
}

#[test]
fn competing_function_bodies_keep_immutable_occurrence_identity() {
    let source = "export function format() {}\nexport function format() {}";
    let found = one(TYPES, "index.d.ts", source);
    assert_eq!(found.len(), 2);
    let expected = format!(
        "rift://symbol/npm/npmjs.org/prettier@3.8.5/typescript/prettier/format~2?rev={}",
        crate::documentation::content_digest(source.as_bytes()).0
    );
    assert_eq!(found["format~2"].as_str(), expected);
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
    assert!(!member.as_str().contains("?rev="));
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
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"browser":"./browser.d.ts","types":"./index.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":["./index.d.ts"]}}"#,
        r#"{"name":"prettier","version":"3.8.5","exports":{".":{"types":"./index.d.ts","types":"./other.d.ts"}}}"#,
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","types":"./other.d.ts"}"#,
        "not JSON",
    ] {
        assert!(
            one(metadata, "index.d.ts", "export class Example {}").is_empty(),
            "{metadata}"
        );
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
    assert!(one(TYPES, "other.d.ts", source).is_empty());
    assert!(one(TYPES, "index.d.ts", "export function broken(").is_empty());
}

#[test]
fn types_target_precedence_and_export_conditions_preserve_observed_order() {
    let metadata =
        r#"{"name":"prettier","version":"3.8.5","types":"./index.d.ts","typings":"./other.d.ts"}"#;
    assert!(!one(metadata, "index.d.ts", "export class Example {}").is_empty());
    assert!(one(metadata, "other.d.ts", "export class Example {}").is_empty());
    let metadata = r#"{"name":"prettier","version":"3.8.5","exports":{".":{"default":"./other.d.ts","types":"./index.d.ts"}}}"#;
    assert!(one(metadata, "index.d.ts", "export class Example {}").is_empty());
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
    assert_eq!(mappings.len(), 2);
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
fn local_owner_without_captured_entry_metadata_keeps_identity_unresolved() {
    let result = prepared_project(&rift_protocol::identity::SymbolOwner::Local, None);
    assert!(result.anchors.is_empty());
    assert!(result.exports.is_empty());
    assert!(result.mappings.is_empty());
}
