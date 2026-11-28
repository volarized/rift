use super::{ErrorDefinition, REGISTERED_SLUGS, REGISTRY_NAMESPACE, registry};

#[test]
fn registry_definitions_match_ordered_builder_slugs() {
    assert!(!REGISTERED_SLUGS.is_empty());
    assert!(REGISTERED_SLUGS.is_sorted_by(|left, right| left < right));
    assert!(
        registry()
            .map(|error| error.slug)
            .eq(REGISTERED_SLUGS.iter().copied())
    );

    for error in registry() {
        let mut parts = error.slug.split('.');
        assert_eq!(parts.next(), Some(REGISTRY_NAMESPACE));
        assert!(parts.next().is_some_and(valid_name), "{}", error.slug);
        assert!(parts.next().is_some_and(valid_name), "{}", error.slug);
        assert!(parts.next().is_none(), "{}", error.slug);
        assert!(
            error
                .fields
                .is_sorted_by(|left, right| left.name < right.name),
            "{}",
            error.slug
        );
        for field in error.fields {
            assert!(valid_name(field.name), "{}: {}", error.slug, field.name);
            assert_ne!(field.name, "mcp", "{}", error.slug);
        }
        for kind in ["source", "cause"] {
            assert!(
                error
                    .fields
                    .iter()
                    .filter(|field| field.kind == kind)
                    .count()
                    <= 1,
                "{}: duplicate {kind} fields",
                error.slug,
            );
        }
    }
}

#[test]
fn registry_templates_reference_visible_fields() {
    for error in registry() {
        check_template(error, error.message);
        check_template(error, error.action);
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__")
}

fn check_template(error: &ErrorDefinition, mut text: &str) {
    assert!(!text.trim().is_empty(), "{}: empty template", error.slug);
    while let Some(start) = text.find(['{', '}']) {
        let rest = &text[start..];
        if rest.starts_with("{{") || rest.starts_with("}}") {
            text = &rest[2..];
            continue;
        }
        let field = rest.strip_prefix('{').and_then(|tail| tail.split_once('}'));
        let Some((name, remaining)) = field else {
            panic!("{}: malformed placeholder in {rest:?}", error.slug);
        };
        assert!(
            error
                .fields
                .iter()
                .any(|field| field.name == name && field.display),
            "{}: placeholder {{{name}}} requires a visible field",
            error.slug,
        );
        text = remaining;
    }
}
