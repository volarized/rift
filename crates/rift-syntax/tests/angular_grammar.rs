//! Angular grammar updates retain expressions and template control flow.

use rift_core::ProjectPath;
use rift_syntax::{AngularSyntaxProvider, SyntaxLimits, SyntaxNames, SyntaxProvider, SyntaxSource};

#[test]
fn angular_latest_grammar_rules_remain_available_and_restore() {
    let cases = [
        ("<div [class.active]=\"active\"></div>", "class_binding"),
        ("<div [style.width.px]=\"width\"></div>", "style_unit"),
        ("<div [title]=\"`Hello ${name}`\"></div>", "template_string"),
        (
            "<div [hidden]=\"/^ok$/i.test(name)\"></div>",
            "regular_expression",
        ),
        (
            "<div (click)=\"items.map(item => item.id)\"></div>",
            "arrow_function",
        ),
        (
            "@let label = title | uppercase; <p>{{ label }}</p>",
            "let_statement",
        ),
        (
            "@for (item of items; track item.id; let i = $index) {<p>{{i}} {{item.name}}</p>} @empty {<p>none</p>}",
            "for_statement",
        ),
        (
            "@defer (on viewport) {<p>ready</p>} @placeholder {<p>wait</p>}",
            "defer_statement",
        ),
        (
            "@switch (state) {@case ('on') {<p>on</p>} @default {<p>off</p>}}",
            "switch_statement",
        ),
    ];
    let path = ProjectPath::new("component.html").expect("fixture path");
    for (text, kind) in cases {
        let source = SyntaxSource { path: &path, text };
        let document = AngularSyntaxProvider::default()
            .analyze(source, SyntaxLimits::default())
            .expect("Angular template");
        assert!(!document.has_errors(), "{kind}: {text}");
        assert!(
            document.nodes().iter().any(|node| node.kind == kind),
            "{kind}: {text}"
        );
        let names = SyntaxNames::new(document.language()).expect("Angular vocabulary");
        assert!(names.node_kind(kind).is_some(), "{kind} restores");
    }
}
