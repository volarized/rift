//! Public component providers preserve host bytes and bound combined parsing.

use rift_core::{FileDigest, ProjectPath};
use rift_syntax::{
    AngularSyntaxProvider, ByteRange, SvelteSyntaxProvider, SyntaxDocument, SyntaxLimits,
    SyntaxNames, SyntaxProvider, SyntaxSource, TypeScriptDialect, VueSyntaxProvider,
    analyze_angular_included, append_angular_templates,
};

fn analyze(provider: &dyn SyntaxProvider, text: &str) -> SyntaxDocument {
    let path = ProjectPath::new("src/Beacon.component").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    provider
        .analyze(source, SyntaxLimits::default())
        .expect("component syntax")
}

#[test]
fn vue_setup_and_ordinary_scripts_keep_original_unicode_offsets_and_styles() {
    let text = "<template><section>λ {{ title }}</section></template>\n<script setup lang=\"ts\">\nexport function beam(value: number): number { return value; }\n</script>\n<script>const title = '灯';</script>\n<style>.beacon { --color: red; color: var(--color); }</style>";
    let document = analyze(&VueSyntaxProvider::default(), text);
    assert!(!document.has_errors());
    assert_eq!(document.language().name, "vue");
    assert_eq!(
        document.source_digest(),
        Some(&FileDigest::of(text.as_bytes()))
    );
    let beam = document
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "beam")
        .expect("embedded function");
    let start = usize::try_from(beam.range.start).expect("fixture offset");
    let end = usize::try_from(beam.range.end).expect("fixture offset");
    assert_eq!(
        &text[start..end],
        "export function beam(value: number): number { return value; }"
    );
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "title")
    );
    assert!(document.nodes().iter().any(|node| node.kind == "rule_set"));
    assert!(
        document
            .nodes()
            .iter()
            .any(|node| node.kind == "interpolation")
    );
    assert!(
        document
            .nodes()
            .iter()
            .all(|node| node.range.end <= text.len() as u64)
    );
    assert!(
        document
            .nodes()
            .iter()
            .enumerate()
            .all(|(index, node)| node.parent.is_none_or(|parent| parent < index))
    );
    let names = SyntaxNames::new(document.language()).expect("component vocabulary");
    for node in document.nodes() {
        assert!(
            names.node_kind(node.kind).is_some(),
            "stored node {} must restore",
            node.kind
        );
    }
    for symbol in document.symbols() {
        assert!(
            names.symbol_kind(symbol.kind).is_some(),
            "stored symbol {} must restore",
            symbol.kind
        );
    }
}

#[test]
fn svelte_template_control_flow_and_typescript_script_share_host_ranges() {
    let text = "<script lang=\"ts\">export const beams: string[] = ['灯'];</script>\n{#if beams.length}{#each beams as beam}<section>{beam}</section>{/each}{:else}<span>none</span>{/if}\n<style>section { color: red; }</style>";
    let document = analyze(&SvelteSyntaxProvider::default(), text);
    assert!(!document.has_errors());
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "beams")
    );
    assert!(document.nodes().iter().any(|node| node.kind.contains("if")));
    assert!(
        document
            .nodes()
            .iter()
            .any(|node| node.kind.contains("each"))
    );
    assert!(
        document
            .nodes()
            .iter()
            .any(|node| node.kind == "expression")
    );
    assert!(document.nodes().iter().any(|node| node.kind == "rule_set"));
}

#[test]
fn component_malformed_source_reports_errors_and_unknown_embedded_languages_stay_raw() {
    let malformed = analyze(
        &VueSyntaxProvider::default(),
        "<script>function beam( {</script><template><section></template>",
    );
    assert!(malformed.has_errors());
    let text = "<script type=\"application/json\">{\"beam\":1}</script><script lang=\"coffee\">beam = -> 1</script><style lang=\"scss\">$beam: red;</style>";
    let document = analyze(&VueSyntaxProvider::default(), text);
    assert!(document.symbols().is_empty());
    assert_eq!(
        document
            .nodes()
            .iter()
            .filter(|node| node.kind == "raw_text")
            .count(),
        3
    );
    assert!(
        !document
            .nodes()
            .iter()
            .any(|node| node.kind == "program" || node.kind == "stylesheet")
    );
}

#[test]
fn combined_component_nodes_cross_one_aggregate_bound() {
    let text = "<script>export const beam = 1;</script><script>export const light = 2;</script><style>.beam { color:red; }</style>";
    let provider = VueSyntaxProvider::default();
    let complete = analyze(&provider, text);
    let path = ProjectPath::new("src/Beacon.vue").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let limits =
        SyntaxLimits::new(text.len(), complete.nodes().len() - 1, 128).expect("positive bounds");
    let error = provider
        .analyze(source, limits)
        .expect_err("combined syntax exceeds bound");
    assert_eq!(
        error.slug(),
        rift_error::errors::syntax::too_many_nodes::SLUG
    );
    let exact = SyntaxLimits::new(text.len(), complete.nodes().len(), 128).expect("exact bounds");
    assert_eq!(
        provider
            .analyze(source, exact)
            .expect("exact bound")
            .nodes(),
        complete.nodes()
    );
    let small_source = SyntaxLimits::new(text.len() - 1, 1000, 128).expect("positive bounds");
    assert_eq!(
        provider
            .analyze(source, small_source)
            .expect_err("source bytes")
            .slug(),
        rift_error::errors::syntax::source_too_large::SLUG
    );
}

#[test]
fn angular_external_template_parses_bindings_pipes_and_control_flow() {
    let text = "@if (ready) { <section [title]=\"title\" (click)=\"beam()\">{{ title | uppercase }}</section> } @else { <span>none</span> }";
    let document = analyze(&AngularSyntaxProvider::default(), text);
    assert!(!document.has_errors());
    assert_eq!(document.language().name, "html");
    assert_eq!(document.language().dialect.as_deref(), Some("angular"));
    for kind in [
        "property_binding",
        "event_binding",
        "interpolation",
        "pipe_call",
        "if_statement",
    ] {
        assert!(
            document.nodes().iter().any(|node| node.kind == kind),
            "Angular grammar must expose {kind}"
        );
    }
}

#[test]
fn angular_inline_template_preserves_typescript_identity_and_unicode_offsets() {
    let text = "const heading = '灯';\n@Component({ template: `@if (ready) { <section>{{ title | uppercase }}</section> }` })\nexport class Beacon {}";
    let path = ProjectPath::new("src/beacon.ts").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let start = text.find('`').expect("opening template") + 1;
    let end = text.rfind('`').expect("closing template");
    let range = ByteRange {
        start: start as u64,
        end: end as u64,
    };
    let template =
        analyze_angular_included(source, SyntaxLimits::default(), range).expect("inline template");
    assert!(!template.has_errors());
    assert!(
        template
            .nodes()
            .iter()
            .all(|node| node.range.start >= range.start && node.range.end <= range.end)
    );
    let host = TypeScriptDialect::TypeScript
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript component");
    let host_count = host.nodes().len();
    let document = append_angular_templates(source, SyntaxLimits::default(), &host, &[range])
        .expect("combined templates");
    assert_eq!(document.language(), host.language());
    assert_eq!(document.symbols(), host.symbols());
    assert_eq!(document.nodes().len(), host_count + template.nodes().len());
    assert!(document.nodes().iter().any(|node| node.kind == "pipe_call"));
    let limits = SyntaxLimits::new(text.len(), document.nodes().len() - 1, 128).expect("bounds");
    assert_eq!(
        append_angular_templates(source, limits, &host, &[range])
            .expect_err("aggregate nodes")
            .slug(),
        rift_error::errors::syntax::too_many_nodes::SLUG
    );
}

#[test]
fn angular_component_ownership_requires_runtime_import_and_class_decorator() {
    use rift_syntax::{AngularTemplate, angular_components};
    let text = "import { Component as Panel } from '@angular/core';\nimport * as ng from '@angular/core';\n@Panel({ template: `<section>灯 {{ title }}</section>`, templateUrl: './beacon.html' }) export class Beacon {}\n@ng . Component({ template: makeTemplate() }) class Dynamic {}\n@Panel(settings) class Settings {}\n@Other({ template: '<section />' }) class Unrelated {}";
    let path = ProjectPath::new("src/beacon.ts").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = TypeScriptDialect::TypeScript
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("component syntax");
    let components = angular_components(source, &document);
    assert_eq!(
        components.len(),
        3,
        "components={components:?}; nodes={:?}",
        document.nodes()
    );
    assert_eq!(components[0].templates.len(), 2);
    let AngularTemplate::Inline { range } = components[0].templates[0] else {
        panic!("literal template")
    };
    assert_eq!(
        &text[usize::try_from(range.start).expect("bounded source bytes")
            ..usize::try_from(range.end).expect("bounded source bytes")],
        "<section>灯 {{ title }}</section>"
    );
    assert_eq!(
        components[0].templates[1],
        AngularTemplate::External {
            path: "./beacon.html".into()
        }
    );
    assert!(matches!(
        components[1].templates[0],
        AngularTemplate::Unresolved { .. }
    ));
    assert!(matches!(
        components[2].templates[0],
        AngularTemplate::Unresolved { .. }
    ));
    for text in [
        "import { Component } from './local'; @Component({template:'x'}) class Beacon {}",
        "import type { Component } from '@angular/core'; @Component({template:'x'}) class Beacon {}",
        "import { type Component } from '@angular/core'; @Component({template:'x'}) class Beacon {}",
    ] {
        let source = SyntaxSource { path: &path, text };
        let document = TypeScriptDialect::TypeScript
            .provider()
            .analyze(source, SyntaxLimits::default())
            .expect("TypeScript syntax");
        assert!(
            angular_components(source, &document).is_empty(),
            "unrelated or type import: {text}"
        );
    }
}

#[test]
fn angular_dynamic_and_escaped_template_bytes_remain_unresolved() {
    use rift_syntax::{AngularTemplate, angular_components};
    let text = "import { Component } from '@angular/core'; @Component({ template: `<span>${title}</span>`, templateUrl: './beacon\\x2ehtml' }) class Beacon {}";
    let path = ProjectPath::new("src/beacon.ts").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = TypeScriptDialect::TypeScript
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript syntax");
    let components = angular_components(source, &document);
    assert_eq!(components.len(), 1);
    assert!(
        components[0]
            .templates
            .iter()
            .all(|template| matches!(template, AngularTemplate::Unresolved { .. }))
    );
}

#[test]
fn angular_inline_ranges_reject_duplicates_overlap_and_invalid_utf8() {
    let text = "const title = '灯'; const template = `<section>{{ title }}</section>`;";
    let path = ProjectPath::new("src/beacon.ts").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let host = TypeScriptDialect::TypeScript
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript syntax");
    let start = text.find('`').expect("opening template") + 1;
    let end = text.rfind('`').expect("closing template");
    let range = ByteRange {
        start: start as u64,
        end: end as u64,
    };
    for ranges in [
        vec![range, range],
        vec![ByteRange {
            start: range.end,
            end: range.start,
        }],
        vec![ByteRange { start: 16, end: 17 }],
    ] {
        assert_eq!(
            append_angular_templates(source, SyntaxLimits::default(), &host, &ranges)
                .expect_err("invalid ranges")
                .slug(),
            rift_error::errors::syntax::facts_range_invalid::SLUG
        );
    }
}

#[test]
fn component_host_and_embedded_depth_share_exact_configured_bound() {
    let text = "<template><section><div>{{title}}</div></section></template><script>function beam(){ return () => ({value:1}); }</script>";
    let provider = VueSyntaxProvider::default();
    let path = ProjectPath::new("src/beacon.vue").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = provider
        .analyze(source, SyntaxLimits::default())
        .expect("Vue syntax");
    let mut depths = Vec::with_capacity(document.nodes().len());
    for node in document.nodes() {
        depths.push(node.parent.map_or(0, |parent| depths[parent] + 1));
    }
    let depth = *depths.iter().max().expect("root depth");
    let admitted =
        SyntaxLimits::new(text.len(), document.nodes().len(), depth).expect("exact bounds");
    assert_eq!(
        provider
            .analyze(source, admitted)
            .expect("exact depth")
            .nodes(),
        document.nodes()
    );
    let refused =
        SyntaxLimits::new(text.len(), document.nodes().len(), depth - 1).expect("reduced depth");
    assert_eq!(
        provider
            .analyze(source, refused)
            .expect_err("combined depth")
            .slug(),
        rift_error::errors::syntax::too_deep::SLUG
    );
}

#[test]
fn component_embedded_omitted_declarations_retain_admission_count() {
    let text = format!(
        "<script>const {}=1;</script>",
        "b".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1)
    );
    let document = analyze(&VueSyntaxProvider::default(), &text);
    assert!(document.symbols().is_empty());
    assert_eq!(document.left_out_declaration_count(), 1);
}

#[test]
fn angular_empty_inline_template_retains_existing_typescript_facts() {
    let text = "import {Component} from '@angular/core'; @Component({template:''}) class Beacon {}";
    let path = ProjectPath::new("src/beacon.ts").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let host = TypeScriptDialect::TypeScript
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript syntax");
    let component = rift_syntax::angular_components(source, &host);
    let rift_syntax::AngularTemplate::Inline { range } = component[0].templates[0] else {
        panic!("inline template")
    };
    assert_eq!(range.start, range.end);
    let combined = append_angular_templates(source, SyntaxLimits::default(), &host, &[range])
        .expect("empty inline template");
    assert_eq!(
        combined.facts().origin(),
        rift_syntax::SyntaxOrigin::Framework
    );
    assert_eq!(combined.nodes(), host.nodes());
    assert_eq!(combined.symbols(), host.symbols());
    assert_eq!(combined.language(), host.language());
    assert_eq!(combined.source_digest(), host.source_digest());
    assert_eq!(
        combined.facts().syntax_limits(),
        host.facts().syntax_limits()
    );
    assert_eq!(combined.has_errors(), host.has_errors());
    assert_eq!(
        combined.left_out_declaration_count(),
        host.left_out_declaration_count()
    );
}
