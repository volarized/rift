//! Framework context follows package ownership and original source bytes.
#![cfg(feature = "collector")]

use std::sync::Arc;

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, FrameworkContext, PackageAnalysis, PackageAnalyzer,
    PackageSource, PackageSyntax, PackageSyntaxSource,
};
use rift_core::{ContributionOrigin, FileDigest, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::canonical::canonical_json;
use rift_protocol::configuration::{SyntaxFrameworkConfiguration, TailwindVersion};
use rift_protocol::read::{PackageIdentity, PathPattern, ReadWarning, SyntaxFramework};
use rift_syntax::{
    ShippedLanguage, SyntaxFacts, SyntaxFactsParts, SyntaxLimits, SyntaxNames, SyntaxSource,
};

fn sources<'a>(paths: &'a [ProjectPath], files: &'a [(&str, &str)]) -> Vec<PackageSource<'a>> {
    paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect()
}

fn paths(files: &[(&str, &str)]) -> Vec<ProjectPath> {
    files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("fixture path"))
        .collect()
}

fn has_warning(warnings: &[ReadWarning], framework: SyntaxFramework) -> bool {
    warnings.iter().any(|warning| matches!(warning, ReadWarning::FrameworkContextUnresolved { framework: found, .. } if *found == framework))
}

#[test]
fn angular_component_ownership_keeps_plain_html_and_package_boundaries() {
    let files = [
        ("apps/beacon/package.json", "{}"),
        (
            "apps/beacon/component.ts",
            "import {Component} from '@angular/core'; @Component({templateUrl:'./owned.html', template:`<p>{{ title | uppercase }}</p>`}) export class Beacon {}",
        ),
        ("apps/beacon/owned.html", "@if (ready) {<p>{{ title }}</p>}"),
        ("apps/beacon/plain.html", "<p>{{ title }}</p>"),
        ("apps/other/package.json", "{}"),
        ("apps/other/plain.html", "<p>{{ title }}</p>"),
        (
            "apps/beacon/unresolved.ts",
            "import {Component} from '@angular/core'; @Component({template: selected, templateUrl:'../other/plain.html'}) class Other {} @Component({templateUrl:'./stylesheet.css'}) class Style {} @Component({templateUrl:'./component.ts'}) class Script {}",
        ),
        (
            "apps/beacon/module.mts",
            "import {Component} from '@angular/core'; @Component({template:`<p>{{title}}</p>`}) export class Module {}",
        ),
        (
            "apps/beacon/module.cts",
            "import {Component} from '@angular/core'; @Component({template:`<p>{{title}}</p>`}) export class Module {}",
        ),
        ("apps/beacon/stylesheet.css", "p {color:red;}"),
    ];
    let paths = paths(&files);
    let sources = sources(&paths, &files);
    let context = FrameworkContext::resolve(&sources, &[], SyntaxLimits::default(), &|| false)
        .expect("component context");
    assert!(context.for_path(&paths[2]).expect("owned template").angular);
    assert_eq!(
        context
            .for_path(&paths[1])
            .expect("inline template")
            .templates
            .len(),
        1
    );
    assert!(context.for_path(&paths[3]).is_none());
    assert!(context.for_path(&paths[5]).is_none());
    assert!(context.for_path(&paths[9]).is_none());
    for path in &paths[7..9] {
        assert_eq!(
            context
                .for_path(path)
                .expect("module template ownership")
                .templates
                .len(),
            1
        );
    }
    assert!(has_warning(context.warnings(), SyntaxFramework::Angular));
    for index in [1, 2, 3] {
        let source = SyntaxSource {
            path: &paths[index],
            text: files[index].1,
        };
        let provider =
            rift_syntax::registry::provider_for_extension(if index == 1 { "ts" } else { "html" })
                .expect("source provider");
        let raw = provider
            .analyze(source, SyntaxLimits::default())
            .expect("raw syntax");
        let (document, _, _) = context
            .apply(source, SyntaxLimits::default(), raw)
            .expect("component syntax");
        assert_eq!(
            document.source_digest(),
            Some(&FileDigest::of(source.text.as_bytes()))
        );
        assert_eq!(
            document.language().identity_segment(),
            if index == 2 {
                "html:angular"
            } else if index == 1 {
                "typescript"
            } else {
                "html"
            }
        );
        if index != 3 {
            assert!(
                document
                    .nodes()
                    .iter()
                    .any(|node| node.kind == "interpolation")
            );
        }
    }
}

const TAILWIND_PACKAGE_FILES: [(&str, &str); 16] = [
    (
        "apps/beacon/package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"^4.0.0\"}}",
    ),
    (
        "apps/beacon/package-lock.json",
        "{\"lockfileVersion\":3,\"packages\":{\"node_modules/tailwindcss\":{\"version\":\"4.1.0\"}}}",
    ),
    (
        "apps/beacon/theme.css",
        "@import 'tailwindcss'; @reference './base.css'; @theme {--color-beacon: red;} @utility beacon {color:red;} @custom-variant selected (&:hover); @config './tailwind.config.js';",
    ),
    ("apps/beacon/view.html", "<p class=\"md:beacon\"></p>"),
    (
        "apps/beacon/view.tsx",
        "export const Beacon = <p className=\"md:beacon\"/>; const dynamic=<p className={selected}/>;",
    ),
    (
        "apps/beacon/view.vue",
        "<template><p class=\"md:beacon\" v-bind:class=\"selected\"/></template>",
    ),
    (
        "apps/beacon/view.svelte",
        "<p class=\"md:beacon\"/><p class={selected}/>",
    ),
    (
        "apps/beacon/component.ts",
        "import {Component} from '@angular/core'; @Component({templateUrl:'./angular.html'}) class Beacon {}",
    ),
    (
        "apps/beacon/angular.html",
        "<p class=\"md:beacon\" [attr.class]=\"selected\"></p>",
    ),
    ("apps/beacon/nested/package.json", "{}"),
    (
        "apps/beacon/nested/plain.html",
        "<p class=\"md:beacon\"></p>",
    ),
    (
        "apps/legacy/package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"3.4.1\"}}",
    ),
    ("apps/legacy/tailwind.config.js", "export default {};"),
    (
        "apps/legacy/theme.css",
        "@layer utilities {.beacon {color:red;}} @apply hover:flex; @reference './base.css';",
    ),
    (
        "apps/beacon/module.cjs",
        "const panel=<p className=\"md:beacon\"/>;",
    ),
    (
        "apps/beacon/module.mjs",
        "const panel=<p className=\"md:beacon\"/>;",
    ),
];

#[test]
fn tailwind_versions_follow_nearest_package_and_all_template_hosts() {
    let files = TAILWIND_PACKAGE_FILES;
    let paths = paths(&files);
    let sources = sources(&paths, &files);
    let context = FrameworkContext::resolve(&sources, &[], SyntaxLimits::default(), &|| false)
        .expect("package framework context");
    assert!(context.for_path(&paths[10]).is_none());
    assert_eq!(
        context
            .for_path(&paths[13])
            .expect("legacy context")
            .tailwind,
        Some(TailwindVersion::V3)
    );
    for index in [2, 3, 4, 5, 6, 8, 13, 14, 15] {
        let source = SyntaxSource {
            path: &paths[index],
            text: files[index].1,
        };
        let extension = paths[index]
            .as_str()
            .rsplit('.')
            .next()
            .expect("source extension");
        let raw = rift_syntax::registry::provider_for_extension(extension)
            .expect("source provider")
            .analyze(source, SyntaxLimits::default())
            .expect("raw syntax");
        let raw_language = raw.language().clone();
        let (document, warnings, _) = context
            .apply(source, SyntaxLimits::default(), raw)
            .expect("Tailwind syntax");
        assert!(
            document
                .symbols()
                .iter()
                .any(|symbol| symbol.kind == "utility" && symbol.name == "beacon"),
            "{}",
            paths[index].as_str()
        );
        assert_eq!(
            document.source_digest(),
            Some(&FileDigest::of(source.text.as_bytes()))
        );
        if index != 8 {
            assert_eq!(document.language(), &raw_language);
        }
        if [4, 5, 6, 8].contains(&index) {
            assert!(has_warning(&warnings, SyntaxFramework::Tailwind));
        }
        if index == 13 {
            assert!(
                !document
                    .symbols()
                    .iter()
                    .any(|symbol| symbol.kind == "configuration" && symbol.name == "./base.css")
            );
        }
        if index == 2 {
            let reference = document
                .symbols()
                .iter()
                .find(|symbol| symbol.kind == "configuration" && symbol.name == "./base.css")
                .expect("contextual stylesheet reference");
            let start = usize::try_from(reference.range.start).expect("reference offset");
            let end = usize::try_from(reference.range.end).expect("reference offset");
            assert_eq!(&source.text[start..end], "./base.css");
            for (kind, name) in [
                ("variant", "selected"),
                ("theme", "--color-beacon"),
                ("configuration", "./tailwind.config.js"),
                ("configuration", "./base.css"),
            ] {
                assert!(
                    document
                        .symbols()
                        .iter()
                        .any(|symbol| symbol.kind == kind && symbol.name == name),
                    "{kind}: {name}"
                );
            }
        }
    }
}

#[test]
fn explicit_context_and_unresolved_versions_keep_original_facts() {
    let files = [
        (
            "package.json",
            "{\"devDependencies\":{\"tailwindcss\":\"^4.0.0\"}}",
        ),
        ("theme.css", "@import 'tailwindcss';"),
        ("view.html", "<p class=\"flex\">{{ title }}</p>"),
    ];
    let paths = paths(&files);
    let sources = sources(&paths, &files);
    let unresolved = FrameworkContext::resolve(&sources, &[], SyntaxLimits::default(), &|| false)
        .expect("unresolved context");
    assert!(unresolved.for_path(&paths[2]).is_none());
    assert!(has_warning(
        unresolved.warnings(),
        SyntaxFramework::Tailwind
    ));
    let selections = [SyntaxFrameworkConfiguration {
        include: vec![PathPattern("view.html".to_owned())],
        angular: true,
        tailwind: Some(TailwindVersion::V4),
    }];
    let explicit =
        FrameworkContext::resolve(&sources, &selections, SyntaxLimits::default(), &|| false)
            .expect("explicit context");
    let context = explicit.for_path(&paths[2]).expect("selected source");
    assert!(context.angular);
    assert_eq!(context.tailwind, Some(TailwindVersion::V4));
    let conflict = [selections[0].clone(), selections[0].clone()];
    let conflicted =
        FrameworkContext::resolve(&sources, &conflict, SyntaxLimits::default(), &|| false)
            .expect("conflicting context warning");
    assert!(conflicted.for_path(&paths[2]).is_none());
    assert!(has_warning(conflicted.warnings(), SyntaxFramework::Angular));
    assert!(FrameworkContext::resolve(&sources, &[], SyntaxLimits::default(), &|| true).is_err());
}

fn analyze_package(
    files: &[(&str, &str)],
    metadata: &[(&str, &str)],
    supplied: impl FnMut(&PackageSyntaxSource<'_>) -> Option<PackageSyntax>,
) -> PackageAnalysis {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let language = ShippedLanguage::TypeScript.language();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let paths = paths(files);
    let metadata_paths = self::paths(metadata);
    let sources = sources(&paths, files);
    let metadata_sources = self::sources(&metadata_paths, metadata);
    let bytes = files
        .iter()
        .chain(metadata)
        .map(|(_, text)| u64::try_from(text.len()).expect("fixture bytes"))
        .sum();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(
            u32::try_from(files.len() + metadata.len()).expect("fixture count"),
            bytes,
        ),
    )
    .expect("package input")
    .with_framework_context(&metadata_sources, &[])
    .expect("package context input");
    PackageAnalyzer::analyze_with_syntax(input, 1, supplied).expect("package framework analysis")
}

#[test]
fn package_framework_publication_matches_fresh_supplied_and_restored_syntax() {
    let files = [
        (
            "component.ts",
            "import {Component} from '@angular/core'; @Component({templateUrl:'./view.html',template:`<p class=\"flex\">{{title}}</p>`}) export class Beacon {}",
        ),
        (
            "view.html",
            "<p class=\"md:flex\" [className]=\"selected\">{{title}}</p>",
        ),
        (
            "view.vue",
            "<template><p class=\"flex\"/></template><script setup lang=\"ts\">const beacon = 1;</script>",
        ),
    ];
    let metadata = [
        (
            "package.json",
            "{\"devDependencies\":{\"tailwindcss\":\"4.1.0\"}}",
        ),
        ("theme.css", "@import 'tailwindcss';"),
    ];
    let fresh = analyze_package(&files, &metadata, |_| None);
    let mut retained = Vec::new();
    let supplied = analyze_package(&files, &metadata, |source| {
        let syntax = source.parse().expect("raw supplied syntax");
        retained.push(syntax.clone());
        Some(syntax)
    });
    let restored = retained
        .iter()
        .map(|syntax| {
            let text = files
                .iter()
                .find(|(_, text)| {
                    FileDigest::of(text.as_bytes()) == syntax.identity().source_digest
                })
                .expect("source witness")
                .1;
            let facts = syntax.facts();
            let names = SyntaxNames::new(facts.language()).expect("syntax names");
            let mut symbols = facts.symbols().to_vec();
            for symbol in &mut symbols {
                symbol.kind = names.symbol_kind(symbol.kind).expect("symbol kind");
                symbol.node_kind = symbol
                    .node_kind
                    .map(|kind| names.node_kind(kind).expect("node kind"));
            }
            let facts = SyntaxFacts::from_parts(
                text,
                syntax.identity().limits,
                SyntaxFactsParts {
                    origin: facts.origin(),
                    language: facts.language().clone(),
                    symbols,
                    has_errors: facts.has_errors(),
                    left_out_declarations: facts.left_out_declaration_count(),
                    markdown_facts: facts.markdown_facts().cloned(),
                    source_digest: syntax.identity().source_digest,
                },
            )
            .expect("restored raw facts");
            PackageSyntax::new(syntax.identity().clone(), Arc::new(facts))
        })
        .collect::<Vec<_>>();
    let reused = analyze_package(&files, &metadata, |source| {
        restored
            .iter()
            .find(|syntax| syntax.identity() == source.identity())
            .cloned()
    });
    let canonical = |analysis: &PackageAnalysis| {
        canonical_json(analysis.publication()).expect("canonical publication")
    };
    assert_eq!(canonical(&fresh), canonical(&supplied));
    assert_eq!(canonical(&fresh), canonical(&reused));
    assert_eq!(fresh.warnings(), supplied.warnings());
    assert_eq!(fresh.warnings(), reused.warnings());
    assert!(has_warning(fresh.warnings(), SyntaxFramework::Tailwind));
    assert_eq!(fresh.publication().units.len(), files.len());
    for unit in &fresh.publication().units {
        let (_, text) = files
            .iter()
            .find(|(path, _)| unit.path.0.as_str() == *path)
            .expect("published source");
        assert_eq!(unit.source, *text);
        assert!(unit.source_complete);
    }
}

#[test]
fn framework_application_checks_source_witness_and_aggregate_bounds() {
    let files = [("view.html", "<p class=\"flex\"></p>")];
    let paths = paths(&files);
    let sources = sources(&paths, &files);
    let explicit = [SyntaxFrameworkConfiguration {
        include: vec![PathPattern("view.html".to_owned())],
        angular: false,
        tailwind: Some(TailwindVersion::V4),
    }];
    let limits = SyntaxLimits::default();
    let context = FrameworkContext::resolve(&sources, &explicit, limits, &|| false)
        .expect("explicit context");
    let provider = rift_syntax::registry::provider_for_extension("html").expect("HTML provider");
    let source = SyntaxSource {
        path: &paths[0],
        text: files[0].1,
    };
    let raw = provider.analyze(source, limits).expect("raw syntax");
    let limited = SyntaxLimits::new(
        source.text.len() - 1,
        limits.syntax_nodes_max(),
        limits.syntax_depth_max(),
    )
    .expect("source bound");
    assert!(context.apply(source, limited, raw).is_err());
    let changed = SyntaxSource {
        path: &paths[0],
        text: "<p class=\"grid\"></p>",
    };
    let changed_raw = provider
        .analyze(changed, limits)
        .expect("changed raw syntax");
    let changed_facts = changed_raw.facts().clone();
    let (unchanged, warnings, _) = context
        .apply(changed, limits, changed_raw)
        .expect("stale context retains source");
    assert_eq!(unchanged.facts(), &changed_facts);
    assert!(has_warning(&warnings, SyntaxFramework::Tailwind));
    assert!(
        !unchanged
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "utility")
    );
}

#[test]
fn package_context_metadata_shares_source_count_bytes_and_distinct_path_bounds() {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let language = ShippedLanguage::TypeScript.language();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let path = ProjectPath::new("view.html").expect("source path");
    let metadata_path = ProjectPath::new("package.json").expect("metadata path");
    let text = "<p class=\"flex\"></p>";
    let metadata_text = "{}";
    let selected = [PackageSource::new(&path, text)];
    let metadata = [PackageSource::new(&metadata_path, metadata_text)];
    let bytes = u64::try_from(text.len() + metadata_text.len()).expect("fixture bytes");
    for limits in [
        ExactPackageLimits::new(1, bytes),
        ExactPackageLimits::new(2, bytes - 1),
    ] {
        let input = ExactPackageInput::new(&package, &language, &origin, &selected, limits)
            .expect("selected source fits");
        assert!(input.with_framework_context(&metadata, &[]).is_err());
    }
    let limits = ExactPackageLimits::new(2, bytes);
    assert!(
        ExactPackageInput::new(&package, &language, &origin, &selected, limits)
            .expect("selected source")
            .with_framework_context(&metadata, &[])
            .is_ok()
    );
    let duplicate = [PackageSource::new(&path, metadata_text)];
    assert!(
        ExactPackageInput::new(&package, &language, &origin, &selected, limits)
            .expect("selected source")
            .with_framework_context(&duplicate, &[])
            .is_err()
    );
}

#[test]
fn tailwind_lockfile_pin_does_not_fall_back_to_conflicting_declared_version() {
    for lockfile in [
        "{\"lockfileVersion\":3,\"packages\":{\"node_modules/tailwindcss\":{\"version\":\"5.0.0\"}}}",
        "{\"lockfileVersion\":3,\"packages\":{\"node_modules/tailwindcss\":{\"version\":\"unresolved\"}}}",
    ] {
        let files = [
            (
                "package.json",
                "{\"devDependencies\":{\"tailwindcss\":\"4.1.0\"}}",
            ),
            ("package-lock.json", lockfile),
            ("theme.css", "@import 'tailwindcss';"),
            ("view.html", "<p class=\"flex\"></p>"),
        ];
        let paths = paths(&files);
        let sources = sources(&paths, &files);
        let context = FrameworkContext::resolve(&sources, &[], SyntaxLimits::default(), &|| false)
            .expect("unsupported lock pin context");
        assert!(context.for_path(&paths[3]).is_none());
        assert!(has_warning(context.warnings(), SyntaxFramework::Tailwind));
    }
}
