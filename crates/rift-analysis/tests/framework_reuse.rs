//! Supplied raw syntax must exclude facts from removed framework context.
#![cfg(feature = "collector")]

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, FrameworkContext, PackageAnalyzer, PackageSource,
    PackageSyntax,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::configuration::{SyntaxFrameworkConfiguration, TailwindVersion};
use rift_protocol::read::{PackageIdentity, PathPattern};
use rift_syntax::{
    ShippedLanguage, SyntaxFacts, SyntaxFactsParts, SyntaxLimits, SyntaxOrigin, SyntaxSource,
};

#[test]
fn supplied_enriched_facts_are_refused_after_framework_context_is_removed() {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let path = ProjectPath::new("view.html").expect("fixture path");
    let text = "<p class=\"md:beacon\">灯</p>";
    let sources = [PackageSource::new(&path, text)];
    let selections = [SyntaxFrameworkConfiguration {
        include: vec![PathPattern("view.html".to_owned())],
        angular: false,
        tailwind: Some(TailwindVersion::V4),
    }];
    let context =
        FrameworkContext::resolve(&sources, &selections, SyntaxLimits::default(), &|| false)
            .expect("selected framework context");
    let source = SyntaxSource { path: &path, text };
    let raw = ShippedLanguage::Html
        .definition()
        .syntax_provider()
        .analyze(source, SyntaxLimits::default())
        .expect("raw HTML");
    let (enriched, _, _) = context
        .apply(source, SyntaxLimits::default(), raw)
        .expect("Tailwind declarations");
    assert!(
        enriched
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "utility")
    );
    let language = ShippedLanguage::TypeScript.language();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(1, u64::try_from(text.len()).expect("fixture bytes")),
    )
    .expect("raw package input");
    let analysis = PackageAnalyzer::analyze_with_syntax(input, 1, |source| {
        Some(PackageSyntax::new(
            source.identity().clone(),
            enriched.shared_facts(),
        ))
    })
    .expect("package analysis without framework context");
    assert!(
        analysis.files()[0]
            .file()
            .syntax()
            .symbols()
            .iter()
            .all(|symbol| !["utility", "variant", "theme", "configuration"].contains(&symbol.kind))
    );
    assert_eq!(analysis.syntax_work().reused_files, 0);
    assert_eq!(analysis.syntax_work().provider_calls, 1);
}

#[test]
fn supplied_angular_template_facts_are_refused_when_current_context_is_unresolved() {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let path = ProjectPath::new("view.ts").expect("fixture path");
    let text = "import {Component} from '@angular/core'; @Component({template:'<script>const lamp = 1;</script>'}) class Beacon {}";
    let sources = [PackageSource::new(&path, text)];
    let limits = SyntaxLimits::default();
    let context = FrameworkContext::resolve(&sources, &[], limits, &|| false)
        .expect("owned component context");
    let source = SyntaxSource { path: &path, text };
    let raw = ShippedLanguage::TypeScript
        .definition()
        .syntax_provider()
        .analyze(source, limits)
        .expect("raw TypeScript");
    let (enriched, _, _) = context
        .apply(source, limits, raw)
        .expect("Angular template nodes");
    assert!(
        enriched
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "lamp" && symbol.kind == "variable")
    );
    let restored = checked_facts(text, limits, enriched.facts());
    assert_eq!(restored.origin(), SyntaxOrigin::Framework);
    let selections =
        [TailwindVersion::V3, TailwindVersion::V4].map(|version| SyntaxFrameworkConfiguration {
            include: vec![PathPattern("view.ts".to_owned())],
            angular: false,
            tailwind: Some(version),
        });
    let language = ShippedLanguage::TypeScript.language();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(1, u64::try_from(text.len()).expect("fixture bytes")),
    )
    .expect("raw package input")
    .with_framework_context(&[], &selections)
    .expect("conflicting context input");
    let analysis = PackageAnalyzer::analyze_with_syntax(input, 1, |source| {
        Some(PackageSyntax::new(
            source.identity().clone(),
            std::sync::Arc::new(restored.clone()),
        ))
    })
    .expect("raw package analysis with unresolved context");
    assert!(
        analysis.files()[0]
            .file()
            .syntax()
            .symbols()
            .iter()
            .all(|symbol| symbol.kind != "selector" && symbol.name != "lamp")
    );
    assert_eq!(analysis.syntax_work().reused_files, 0);
    assert_eq!(analysis.syntax_work().provider_calls, 1);
    assert!(!analysis.warnings().is_empty());
}

fn checked_facts(text: &str, limits: SyntaxLimits, facts: &SyntaxFacts) -> SyntaxFacts {
    SyntaxFacts::from_parts(
        text,
        limits,
        SyntaxFactsParts {
            origin: facts.origin(),
            language: facts.language().clone(),
            symbols: facts.symbols().to_vec(),
            has_errors: facts.has_errors(),
            left_out_declarations: facts.left_out_declaration_count(),
            markdown_facts: facts.markdown_facts().cloned(),
            source_digest: *facts.source_digest().expect("source witness"),
        },
    )
    .expect("checked facts retain origin")
}

#[test]
fn raw_embedded_provider_facts_remain_reusable_after_checked_restoration() {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let text = "<script>const lamp = 1;</script><style>.beacon {color:red;}</style>";
    for (shipped, name) in [
        (ShippedLanguage::Html, "view.html"),
        (ShippedLanguage::Vue, "view.vue"),
        (ShippedLanguage::Svelte, "view.svelte"),
    ] {
        let path = ProjectPath::new(name).expect("source path");
        let limits = SyntaxLimits::default();
        let raw = shipped
            .definition()
            .syntax_provider()
            .analyze(SyntaxSource { path: &path, text }, limits)
            .expect("raw embedded source");
        let restored = checked_facts(text, limits, raw.facts());
        assert_eq!(restored.origin(), SyntaxOrigin::Provider);
        assert!(
            restored
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "lamp")
        );
        let sources = [PackageSource::new(&path, text)];
        let language = ShippedLanguage::TypeScript.language();
        let input = ExactPackageInput::new(
            &package,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(1, u64::try_from(text.len()).expect("source bytes")),
        )
        .expect("package input");
        let analysis = PackageAnalyzer::analyze_with_syntax(input, 1, |source| {
            Some(PackageSyntax::new(
                source.identity().clone(),
                std::sync::Arc::new(restored.clone()),
            ))
        })
        .expect("raw reuse");
        assert_eq!(analysis.syntax_work().reused_files, 1, "{shipped:?}");
        assert_eq!(analysis.syntax_work().provider_calls, 0, "{shipped:?}");
    }
}
