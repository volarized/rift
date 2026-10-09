//! Tailwind source facts retain authored ranges and checked parser identity.

use rift_core::ProjectPath;
use rift_syntax::{
    CssSyntaxProvider, HtmlSyntaxProvider, SvelteSyntaxProvider, SyntaxFacts, SyntaxFactsParts,
    SyntaxLimits, SyntaxProvider, SyntaxSource, TypeScriptDialect, VueSyntaxProvider,
    append_framework_symbols, tailwind_symbols,
};

#[test]
fn tailwind_static_classes_variants_and_dynamic_expressions_across_templates() {
    let html = HtmlSyntaxProvider::default();
    let vue = VueSyntaxProvider::default();
    let svelte = SvelteSyntaxProvider::default();
    let tsx = TypeScriptDialect::Tsx.provider();
    for (provider, text) in [
        (
            &html as &dyn SyntaxProvider,
            "<section class=\"flex md:hover:text-red-500 bg-[url(http://light)]\"></section>",
        ),
        (
            &vue,
            "<template><section class=\"flex md:hover:text-red-500 bg-[url(http://light)]\" :class=\"selected\" /></template>",
        ),
        (
            &svelte,
            "<section class=\"flex md:hover:text-red-500 bg-[url(http://light)]\" /><span class={selected}/>",
        ),
        (
            &tsx,
            "const panel = <section className=\"flex md:hover:text-red-500 bg-[url(http://light)]\"/>; const other=<span className={selected}/>;",
        ),
    ] {
        let path = ProjectPath::new("src/beacon.component").expect("fixture path");
        let source = SyntaxSource { path: &path, text };
        let document = provider
            .analyze(source, SyntaxLimits::default())
            .expect("template syntax");
        let facts = tailwind_symbols(source, &document, 4, SyntaxLimits::default())
            .expect("Tailwind source facts");
        for (kind, name) in [
            ("utility", "flex"),
            ("variant", "md"),
            ("variant", "hover"),
            ("utility", "text-red-500"),
            ("utility", "bg-[url(http://light)]"),
        ] {
            assert!(
                facts
                    .symbols
                    .iter()
                    .any(|symbol| symbol.kind == kind && symbol.name == name),
                "{name}: {text}; nodes={:?}",
                document.nodes()
            );
        }
        if text.contains("selected") {
            assert!(
                !facts.unresolved.is_empty(),
                "dynamic classes: {text}; nodes={:?}; facts={facts:?}",
                document.nodes()
            );
        }
        assert!(facts.symbols.iter().all(|symbol| text
            [usize::try_from(symbol.range.start).expect("bounded source bytes")
                ..usize::try_from(symbol.range.end).expect("bounded source bytes")]
            == symbol.name));
        let enriched =
            append_framework_symbols(source, SyntaxLimits::default(), &document, facts.symbols)
                .expect("enriched source");
        let parts = SyntaxFactsParts {
            origin: enriched.facts().origin(),
            language: enriched.language().clone(),
            symbols: enriched.symbols().to_vec(),
            has_errors: enriched.has_errors(),
            left_out_declarations: enriched.left_out_declaration_count(),
            markdown_facts: None,
            source_digest: *enriched.source_digest().expect("digest"),
        };
        assert_eq!(
            SyntaxFacts::from_parts(text, SyntaxLimits::default(), parts)
                .expect("restored context"),
            *enriched.facts()
        );
    }
}

#[test]
fn tailwind_version_specific_directives_and_theme_configuration_references() {
    let text = "@import \"tailwindcss\"; @reference './base.css'; @config './tailwind.config.js'; @theme { --color-beacon: red; } @utility beacon { display:flex; } @custom-variant night (&:where(.night *)); .plain { color:blue; @apply flex hover:beacon !important; margin:theme(spacing.4); }";
    let path = ProjectPath::new("src/beacon.css").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = CssSyntaxProvider::default()
        .analyze(source, SyntaxLimits::default())
        .expect("CSS syntax");
    let facts = tailwind_symbols(source, &document, 4, SyntaxLimits::default()).expect("Tailwind4");
    for (kind, name) in [
        ("configuration", "tailwindcss"),
        ("configuration", "./tailwind.config.js"),
        ("configuration", "./base.css"),
        ("theme", "--color-beacon"),
        ("utility", "beacon"),
        ("variant", "night"),
        ("theme", "spacing.4"),
    ] {
        assert!(
            facts
                .symbols
                .iter()
                .any(|symbol| symbol.kind == kind && symbol.name == name),
            "{kind}:{name}; facts={facts:?}; nodes={:?}",
            document.nodes()
        );
    }
    let reference = facts
        .symbols
        .iter()
        .find(|symbol| symbol.kind == "configuration" && symbol.name == "./base.css")
        .expect("authored stylesheet reference");
    let start = usize::try_from(reference.range.start).expect("reference offset");
    let end = usize::try_from(reference.range.end).expect("reference offset");
    assert_eq!(&text[start..end], "./base.css");
    assert!(
        !facts
            .symbols
            .iter()
            .any(|symbol| symbol.name == "!important")
    );
    let facts3 =
        tailwind_symbols(source, &document, 3, SyntaxLimits::default()).expect("Tailwind3");
    assert!(!facts3.symbols.iter().any(|symbol| matches!(
        symbol.name.as_str(),
        "--color-beacon" | "night" | "./base.css"
    )));
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "color")
    );
}

#[test]
fn tailwind_dynamic_class_strings_preserve_ranges_and_context_bound_refuses() {
    let text = "const panel=<section className={`text-${color}-500`}/>;";
    let path = ProjectPath::new("src/beacon.tsx").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = TypeScriptDialect::Tsx
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TSX syntax");
    let facts =
        tailwind_symbols(source, &document, 4, SyntaxLimits::default()).expect("dynamic context");
    assert!(facts.symbols.is_empty());
    assert_eq!(facts.unresolved.len(), 1);
    let limits = SyntaxLimits::new(text.len(), document.nodes().len(), 128).expect("bounds");
    assert_eq!(
        tailwind_symbols(source, &document, 4, limits)
            .expect_err("combined context bound")
            .slug(),
        rift_error::errors::syntax::too_many_nodes::SLUG
    );
}

#[test]
fn tailwind_conditional_class_expression_never_selects_one_branch() {
    let text = "const panel=<section className={selected ? 'flex' : 'block'}/>;";
    let path = ProjectPath::new("src/beacon.tsx").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = TypeScriptDialect::Tsx
        .provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TSX syntax");
    let facts =
        tailwind_symbols(source, &document, 4, SyntaxLimits::default()).expect("dynamic context");
    assert!(facts.symbols.is_empty());
    assert_eq!(facts.unresolved.len(), 1);
    let range = facts.unresolved[0];
    assert_eq!(
        &text[usize::try_from(range.start).expect("bounded source bytes")
            ..usize::try_from(range.end).expect("bounded source bytes")],
        "className={selected ? 'flex' : 'block'}"
    );
}

#[test]
fn tailwind_many_classes_refuse_before_exceeding_available_fact_slots() {
    let text = format!("<section class=\"{}\"/>", "flex ".repeat(1000));
    let path = ProjectPath::new("src/beacon.html").expect("fixture path");
    let source = SyntaxSource {
        path: &path,
        text: &text,
    };
    let document = HtmlSyntaxProvider::default()
        .analyze(source, SyntaxLimits::default())
        .expect("HTML syntax");
    let limits = SyntaxLimits::new(text.len(), document.nodes().len() + 1, 128).expect("bounds");
    assert_eq!(
        tailwind_symbols(source, &document, 4, limits)
            .expect_err("one fact slot")
            .slug(),
        rift_error::errors::syntax::too_many_nodes::SLUG
    );
}

#[test]
fn tailwind_angular_classes_and_bindings_preserve_authored_names_and_expressions() {
    let text = "<section class=\"flex hover:text-red-500\" [ngClass]=\"classes\" [class.beacon]=\"ready\"></section>";
    let path = ProjectPath::new("src/beacon.html").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = rift_syntax::AngularSyntaxProvider::default()
        .analyze(source, SyntaxLimits::default())
        .expect("Angular syntax");
    let facts =
        tailwind_symbols(source, &document, 4, SyntaxLimits::default()).expect("Angular Tailwind");
    assert!(
        facts
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "utility" && symbol.name == "flex")
    );
    assert!(
        facts
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "utility" && symbol.name == "beacon")
    );
    assert_eq!(facts.unresolved.len(), 2);
    assert!(facts.symbols.iter().all(|symbol| text
        [usize::try_from(symbol.range.start).expect("bounded source bytes")
            ..usize::try_from(symbol.range.end).expect("bounded source bytes")]
        == symbol.name));
}

#[test]
fn tailwind_malformed_embedded_style_header_cannot_reach_other_host_bytes() {
    let text = "<template><section>灯</section></template><style>@apply flex</style><script>const title='hover:block';</script>";
    let path = ProjectPath::new("src/beacon.vue").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = VueSyntaxProvider::default()
        .analyze(source, SyntaxLimits::default())
        .expect("Vue syntax");
    let facts = tailwind_symbols(source, &document, 4, SyntaxLimits::default())
        .expect("bounded style facts");
    let style_end = text.find("</style>").expect("style end") as u64;
    assert!(
        facts
            .symbols
            .iter()
            .all(|symbol| symbol.range.end <= style_end)
    );
    assert!(
        !facts
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains("title") || symbol.name.contains("</style>"))
    );
}

#[test]
fn tailwind_v3_utility_layer_records_authored_class_names_only_in_v3_context() {
    let text = "@tailwind utilities; @layer utilities { .content-auto:hover { content-visibility:auto; } } .plain { color:blue; }";
    let path = ProjectPath::new("src/beacon.css").expect("fixture path");
    let source = SyntaxSource { path: &path, text };
    let document = CssSyntaxProvider::default()
        .analyze(source, SyntaxLimits::default())
        .expect("CSS syntax");
    assert!(
        !document
            .symbols()
            .iter()
            .any(|symbol| symbol.kind == "utility")
    );
    let facts = tailwind_symbols(source, &document, 3, SyntaxLimits::default()).expect("Tailwind3");
    assert!(
        facts
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "utility" && symbol.name == "content-auto")
    );
    assert!(
        !facts
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "utility" && symbol.name == "plain")
    );
    let facts4 =
        tailwind_symbols(source, &document, 4, SyntaxLimits::default()).expect("Tailwind4");
    assert!(
        !facts4
            .symbols
            .iter()
            .any(|symbol| symbol.name == "content-auto")
    );
}

#[test]
fn tailwind_vue_and_angular_dynamic_class_bindings_preserve_expression_ranges() {
    let vue = VueSyntaxProvider::default();
    let angular = rift_syntax::AngularSyntaxProvider::default();
    for (provider, text) in [
        (
            &vue as &dyn SyntaxProvider,
            "<template><section v-bind:class=\"classes\" /></template>",
        ),
        (
            &angular,
            "<section [attr.class]=\"classes\" [className]=\"classes\"></section>",
        ),
    ] {
        let path = ProjectPath::new("src/beacon.component").expect("fixture path");
        let source = SyntaxSource { path: &path, text };
        let document = provider
            .analyze(source, SyntaxLimits::default())
            .expect("template syntax");
        let facts = tailwind_symbols(source, &document, 4, SyntaxLimits::default())
            .expect("Tailwind context");
        assert!(facts.symbols.is_empty());
        assert_eq!(
            facts.unresolved.len(),
            if text.contains("className") { 2 } else { 1 }
        );
        assert!(
            facts
                .unresolved
                .iter()
                .all(|range| range.end <= text.len() as u64)
        );
        assert_eq!(document.language(), provider.language());
    }
}
