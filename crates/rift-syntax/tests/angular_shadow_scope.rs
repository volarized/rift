//! Local bindings must not inherit Angular import ownership.

use rift_core::ProjectPath;
use rift_syntax::{ShippedLanguage, SyntaxLimits, SyntaxSource};

#[test]
fn local_component_bindings_do_not_own_templates() {
    for declaration in [
        "const Component = local;",
        "function Component(value:any) {}",
    ] {
        let path = ProjectPath::new("component.ts").expect("fixture path");
        let text = format!(
            "import {{Component}} from '@angular/core'; function build() {{ {declaration} @Component({{template:'<p>灯</p>'}}) class Local {{}} }}"
        );
        let source = SyntaxSource {
            path: &path,
            text: &text,
        };
        let document = ShippedLanguage::TypeScript
            .definition()
            .syntax_provider()
            .analyze(source, SyntaxLimits::default())
            .expect("TypeScript source");
        assert!(
            rift_syntax::angular_components(source, &document).is_empty(),
            "local binding: {declaration}; nodes={:?}",
            document.nodes()
        );
    }
}

#[test]
fn single_arrow_parameter_does_not_inherit_component_import_ownership() {
    let path = ProjectPath::new("component.ts").expect("fixture path");
    let text = "import {Component} from '@angular/core'; const build = Component => { @Component({template:'<p>灯</p>'}) class Local {} };";
    let source = SyntaxSource { path: &path, text };
    let document = ShippedLanguage::TypeScript
        .definition()
        .syntax_provider()
        .analyze(source, SyntaxLimits::default())
        .expect("TypeScript source");
    assert!(
        rift_syntax::angular_components(source, &document).is_empty(),
        "single arrow parameter; nodes={:?}",
        document.nodes()
    );
}
