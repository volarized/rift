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
        r#"{"./hub":{"import":"./hub.ts"}}"#,
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
        assert!(found.anchors.is_empty(), "{metadata}");
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
fn raw_anonymous_export_bindings_need_module_export_context_before_identity() {
    let metadata = [(
        "package.json",
        r#"{"name":"prettier","version":"3.8.5","main":"./index.js"}"#,
    )];
    for source in [
        "exports.open = function () {}; function kept() {}",
        "function outer(exports) { exports.open = function () {}; } function kept() {}",
        "(function (exports) { exports.open = function () {}; })(other); function kept() {}",
    ] {
        let prepared = prepared(
            ShippedLanguage::JavaScript,
            &metadata,
            &[("index.js", source)],
        );
        let anchors = prepared.anchors.get("index.js").expect("captured module");
        assert!(anchors.contains_key("kept"));
        assert!(!anchors.contains_key("open"));
    }
}

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
            version,
        } => SourceLocation::Dependency {
            package: PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: version.clone(),
            },
        },
        SymbolOwner::Runtime { runtime, version } => SourceLocation::Stdlib {
            runtime: Some(RuntimeIdentity {
                runtime: runtime.clone(),
                version: version.clone(),
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
    SymbolIdentity::parse(
        "rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node:buffer/isUtf8",
    )
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
fn package_ambient_missing_wrong_metadata_and_other_owners_remain_unknown() {
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
            version: "26.6.4".to_owned(),
        },
    ] {
        let (found, facts) = ambient_prepared(&owner, Some(original), source);
        assert!(facts.symbols().iter().any(|symbol| symbol.name == "isUtf8"));
        assert!(found.anchors.is_empty(), "{owner:?}");
        assert!(found.mappings.is_empty());
    }
}

#[test]
fn package_ambient_global_unquoted_outside_and_malformed_remain_unknown() {
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    for source in [
        "declare namespace NodeJS { function isUtf8(): boolean; }",
        "declare global { function isUtf8(): boolean; }",
        "export function isUtf8(): boolean;",
        "declare module \"node:buffer\" { export function isUtf8(input: ): boolean; }",
        "declare module \"node:buffer\" { global { interface Broken { value: } } }",
    ] {
        let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), source);
        assert!(found.anchors.is_empty(), "{source}");
    }
    let source = "export function outside(): boolean;\ndeclare module \"node:buffer\" { export function isUtf8(): boolean; global { interface GlobalMember {} } }\n";
    let (found, facts) = ambient_prepared(&types_node_owner(), Some(metadata), source);
    assert!(!facts.has_errors());
    let anchors = &found.anchors["buffer.d.ts"];
    assert!(anchors.contains_key("\"node:buffer\".isUtf8"));
    assert!(!anchors.contains_key("outside"));
    assert!(!anchors.keys().any(|name| name.starts_with("global.")));
}

#[test]
fn package_ambient_reuses_overload_and_occurrence_identity() {
    let metadata = include_str!("../../../../tests/fixtures/types-node-package.json");
    let overloads = "declare module \"node:buffer\" { export function read(value: string): string; export function read(value: number): string; }";
    let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), overloads);
    let identities = found.anchors["buffer.d.ts"]
        .iter()
        .filter(|(name, _)| name.contains(".read"))
        .map(|(_, identity)| identity)
        .collect::<Vec<_>>();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0], identities[1]);
    let repeated =
        "declare module \"node:buffer\" { export function read() {} export function read() {} }";
    let (found, _) = ambient_prepared(&types_node_owner(), Some(metadata), repeated);
    let identities = found.anchors["buffer.d.ts"]
        .iter()
        .filter(|(name, _)| name.contains(".read"))
        .map(|(_, identity)| identity)
        .collect::<Vec<_>>();
    assert_eq!(identities.len(), 2);
    assert_ne!(identities[0], identities[1]);
    assert!(identities[1].as_str().contains("~2?rev="));
    assert!(
        identities[1]
            .as_str()
            .ends_with(&crate::documentation::content_digest(repeated.as_bytes()).0)
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
    let owner =
        SymbolIdentity::parse("rift://symbol/npm/npmjs.org/react@19.3.0/javascript/react/open")
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
    let implementation = sources
        .iter()
        .find(|source| source.path().as_str() == entry)
        .copied()
        .expect("selected original entry");
    let observations = [crate::NamespaceModule::new(
        "react",
        implementation,
        &[],
        conditions,
    )];
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
        assert!(
            !declaration
                .facets
                .contains(&rift_protocol::read::SymbolFacet::Public)
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
fn commonjs_original_entry_conditions_owner_and_esm_context_are_required() {
    let source = "exports.open=function(){};";
    for (metadata, conditions) in [
        (
            r#"{"name":"wrong","version":"19.3.0","main":"index.js"}"#,
            &["node", "require"][..],
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"other.js"}"#,
            &["node", "require"][..],
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js","type":"module"}"#,
            &["node", "require"][..],
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js"}"#,
            &[][..],
        ),
        (
            r#"{"name":"react","version":"19.3.0","main":"index.js"}"#,
            &["import"][..],
        ),
    ] {
        let (prepared, _) =
            commonjs_prepared(metadata, &[("index.js", source)], "index.js", conditions);
        assert!(
            !prepared
                .anchors
                .get("index.js")
                .is_some_and(|anchors| anchors.contains_key("open"))
        );
        assert!(prepared.public_names.is_empty());
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
        assert!(!prepared.anchors.contains_key("child.js"), "{entry}");
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
