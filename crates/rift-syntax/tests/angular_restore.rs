//! Nested Angular template nodes use declared included grammar names.

use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath};
use rift_syntax::{
    AngularTemplate, ShippedLanguage, SyntaxDocument, SyntaxFacts, SyntaxFactsParts, SyntaxLimits,
    SyntaxNames, SyntaxSource, append_angular_templates,
};

#[test]
fn typescript_inline_angular_styles_restore_nested_css_node_names() {
    for shipped in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
        assert_nested_template(shipped);
    }
}

fn assert_nested_template(shipped: ShippedLanguage) {
    let path = ProjectPath::new("component.ts").expect("source path");
    let text = "// 灯\nimport {Component} from '@angular/core'; function open(): number { return 1; } @Component({template:`<style>.beacon { color:red; }</style><script>const lamp = 1;</script>`}) class Beacon {}";
    let source = SyntaxSource { path: &path, text };
    let limits = SyntaxLimits::default();
    let raw = shipped
        .definition()
        .syntax_provider()
        .analyze(source, limits)
        .expect("TypeScript source");
    let ranges = rift_syntax::angular_components(source, &raw)
        .into_iter()
        .flat_map(|component| component.templates)
        .filter_map(|template| match template {
            AngularTemplate::Inline { range } => Some(range),
            _ => None,
        })
        .collect::<Vec<_>>();
    let document =
        append_angular_templates(source, limits, &raw, &ranges).expect("nested template syntax");
    assert_eq!(document.language(), &shipped.language());
    assert_eq!(
        document.source_digest(),
        Some(&FileDigest::of(text.as_bytes()))
    );
    assert!(document.nodes().iter().any(|node| node.kind == "rule_set"));
    let names = SyntaxNames::new(document.language()).expect("TypeScript names");
    for node in document.nodes() {
        assert!(names.node_kind(node.kind).is_some(), "{}", node.kind);
    }
    assert!(names.node_kind("function_item").is_none());
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "selector")
    );
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "lamp")
    );
    let restored = SyntaxFacts::from_parts(
        text,
        limits,
        SyntaxFactsParts {
            origin: document.facts().origin(),
            language: document.language().clone(),
            symbols: document.symbols().to_vec(),
            has_errors: document.has_errors(),
            left_out_declarations: document.left_out_declaration_count(),
            markdown_facts: document.facts().markdown_facts().cloned(),
            source_digest: FileDigest::of(text.as_bytes()),
        },
    )
    .expect("checked nested template facts");
    assert_eq!(&restored, document.facts());
    assert_aggregate_bounds(source, &document, &ranges, shipped);
    let mut symbols = document.symbols().to_vec();
    let symbol = symbols
        .iter_mut()
        .find(|symbol| !symbol.signatures.is_empty())
        .expect("TypeScript signature");
    Arc::make_mut(&mut symbol.signatures)[0].language = ShippedLanguage::Rust.language();
    let restored = SyntaxFacts::from_parts(
        text,
        limits,
        SyntaxFactsParts {
            origin: document.facts().origin(),
            language: document.language().clone(),
            symbols,
            has_errors: document.has_errors(),
            left_out_declarations: document.left_out_declaration_count(),
            markdown_facts: document.facts().markdown_facts().cloned(),
            source_digest: FileDigest::of(text.as_bytes()),
        },
    );
    assert_eq!(
        restored
            .expect_err("foreign signature language")
            .slug()
            .as_str(),
        "rift.syntax.facts_structure_invalid"
    );
}

fn assert_aggregate_bounds(
    source: SyntaxSource<'_>,
    document: &SyntaxDocument,
    ranges: &[rift_syntax::ByteRange],
    shipped: ShippedLanguage,
) {
    let limits = SyntaxLimits::default();
    let exact = SyntaxLimits::new(
        source.text.len(),
        document.nodes().len(),
        limits.syntax_depth_max(),
    )
    .expect("exact aggregate bounds");
    let exact_raw = shipped
        .definition()
        .syntax_provider()
        .analyze(source, exact)
        .expect("host fits exact aggregate bound");
    let exact_document = append_angular_templates(source, exact, &exact_raw, ranges)
        .expect("nested template fits exact aggregate bound");
    assert_eq!(exact_document.nodes().len(), document.nodes().len());
    let below = SyntaxLimits::new(
        source.text.len(),
        document.nodes().len() - 1,
        limits.syntax_depth_max(),
    )
    .expect("lower aggregate bound");
    let below_raw = shipped
        .definition()
        .syntax_provider()
        .analyze(source, below)
        .expect("host fits lower aggregate bound");
    assert_eq!(
        append_angular_templates(source, below, &below_raw, ranges)
            .expect_err("nested template exceeds aggregate bound")
            .slug()
            .as_str(),
        "rift.syntax.too_many_nodes"
    );
}
#[test]
fn shadowed_component_import_does_not_own_local_class_template() {
    for text in [
        "import {Component} from '@angular/core'; function build(Component:any) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
        "import {Component as Own} from '@angular/core'; function build(Own:any) { @Own({template:'<p>{{title}}</p>'}) class Local {} }",
        "import * as ng from '@angular/core'; function build(ng:any) { @ng.Component({template:'<p>{{title}}</p>'}) class Local {} }",
        "import {Component} from '@angular/core'; function build({Component}:any) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
        "import {Component} from '@angular/core'; function build({item: Component}:any) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
    ] {
        assert_component_count(text, 0);
    }
}

#[test]
fn unshadowed_nested_component_import_preserves_template_ownership() {
    for text in [
        "import {Component} from '@angular/core'; function build(value:any) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
        "import {Component} from '@angular/core'; function build(value:Component = Component) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
        "import {Component} from '@angular/core'; function build({Component: value}:any) { @Component({template:'<p>{{title}}</p>'}) class Local {} }",
    ] {
        assert_component_count(text, 1);
    }
}

fn assert_component_count(text: &str, count: usize) {
    let path = ProjectPath::new("component.ts").expect("source path");
    let source = SyntaxSource { path: &path, text };
    let document = ShippedLanguage::TypeScript
        .definition()
        .syntax_provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript source");
    assert!(document.nodes().iter().any(|node| node.kind == "decorator"));
    let components = rift_syntax::angular_components(source, &document);
    assert_eq!(components.len(), count, "parameter binding: {text}");
}

#[test]
fn quoted_component_template_keys_keep_import_ownership_and_original_ranges() {
    let text = r#"import {Component} from '@angular/core'; @Component({'template':'<p>{{title}}</p>', "templateUrl": './beacon.html'}) class Beacon {}"#;
    let path = ProjectPath::new("component.ts").expect("source path");
    let source = SyntaxSource { path: &path, text };
    for shipped in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
        let document = shipped
            .definition()
            .syntax_provider()
            .analyze(source, SyntaxLimits::default())
            .expect("TypeScript source");
        assert!(!document.has_errors());
        let components = rift_syntax::angular_components(source, &document);
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].templates.len(), 2);
        assert!(components[0].templates.iter().any(|template| {
            let AngularTemplate::Inline { range } = template else {
                return false;
            };
            let start = usize::try_from(range.start).expect("bounded source bytes");
            let end = usize::try_from(range.end).expect("bounded source bytes");
            &text[start..end] == "<p>{{title}}</p>"
        }));
        assert!(components[0].templates.iter().any(|template| {
            matches!(template, AngularTemplate::External { path } if path == "./beacon.html")
        }));
    }
}
