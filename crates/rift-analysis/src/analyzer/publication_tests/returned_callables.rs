//! Calls and reads through a JavaScript factory's returned object.

use super::*;
use rift_protocol::index::StoredEdge;
use rift_protocol::read::{EdgeDerivation, EdgeKind, EdgeProvider};

const MANIFEST: &str = r#"{"name":"beacon","version":"1.0.0","type":"commonjs","exports":{".":"./index.js","./route":"./route.js"}}"#;
const ORIGINAL_ROUTE: &str = "function buildRouting (options) {\n\
    function prepareRoute (url) { return url }\n\
    function routeHandler () { return options }\n\
    return { prepareRoute, routeHandler }\n}\n\
    module.exports = { buildRouting }\n";
const ORIGINAL_INDEX: &str = "const { buildRouting } = require('./route')\n\
    function local () {\nfunction inner () { return 1 }\nreturn { inner }\n}\n\
    function app () {\nconst router = buildRouting({})\nconst made = local()\n\
    return [router.prepareRoute('/'), router.routeHandler(), made.inner()]\n}\n\
    module.exports = app\n";
const ALIAS_ROUTE: &str = "function buildRouting () {\n\
    function prepareRoute (url) { return url }\n\
    return { prepareRoute, explicit: prepareRoute }\n}\n\
    module.exports = { buildRouting }\n";
const ALIAS_INDEX: &str = "const { buildRouting } = require('./route')\n\
    function app (value) {\nconst router = buildRouting(value)\n\
    return [router.prepareRoute('/'), router.explicit('/'), router.explicit]\n}\n\
    module.exports = app\n";
const MUTATED_INDEX: &str = "const { buildRouting } = require('./route')\n\
    function app (replacement) {\nconst router = buildRouting()\n\
    router.explicit = replacement\nreturn router.explicit('/')\n}\n\
    module.exports = app\n";
const ALIAS_CALL: &str = "router.explicit('/')";

fn analyze_returned_calls(index: &str, route: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::JavaScript,
        vec![("index.js", index), ("route.js", route)],
        None,
        Some(("package.json", MANIFEST)),
    )
    .expect("captured JavaScript package")
}

fn occurrence_in_index(edge: &StoredEdge, source: &str, written: &str) -> bool {
    edge.occurrence.as_ref().is_some_and(|occurrence| {
        occurrence.unit.as_str().ends_with("/index.js")
            && source.get(offset_in(occurrence.range.start)..offset_in(occurrence.range.end))
                == Some(written)
    })
}

#[test]
fn returned_member_calls_keep_shorthand_and_resolve_an_explicit_callable_value() {
    let original = analyze_returned_calls(ORIGINAL_INDEX, ORIGINAL_ROUTE);
    for (written, path) in [
        ("router.prepareRoute('/')", "/buildRouting/prepareRoute"),
        ("router.routeHandler()", "/buildRouting/routeHandler"),
        ("made.inner()", "/local/inner"),
    ] {
        assert!(original.publication().edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && occurrence_in_index(edge, ORIGINAL_INDEX, written)
                && edge.to.as_str().ends_with(path)
        }));
    }

    let (publication, files, semantics, _) =
        analyze_returned_calls(ALIAS_INDEX, ALIAS_ROUTE).into_parts();
    let route = files
        .iter()
        .find(|file| file.file().path().as_str() == "route.js")
        .expect("factory source");
    let alias = route
        .file()
        .syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.name == "explicit")
        .expect("explicit field declaration");
    assert_eq!(alias.syntax_element_kind, Some("pair"));
    let value = alias.body_range.expect("written field value");
    assert_eq!(
        ALIAS_ROUTE.get(offset_in(value.start)..offset_in(value.end)),
        Some("prepareRoute")
    );
    let call = publication
        .edges
        .iter()
        .find(|edge| {
            edge.kinds == [EdgeKind::Calls] && occurrence_in_index(edge, ALIAS_INDEX, ALIAS_CALL)
        })
        .expect("explicit alias call");
    let index = files
        .iter()
        .find(|file| file.file().path().as_str() == "index.js")
        .expect("caller source");
    let app = index
        .placement()
        .logical_identity("app")
        .expect("authored app identity");
    assert_eq!(call.from.as_str(), app.as_str());
    assert!(call.to.as_str().ends_with("/buildRouting/prepareRoute"));
    assert_eq!(call.provider, EdgeProvider::Static);
    assert_eq!(call.derivation, EdgeDerivation::Resolution);
    assert!(
        semantics
            .edges()
            .edges()
            .any(|edge| &edge.to_protocol() == call)
    );
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Reads]
            && occurrence_in_index(edge, ALIAS_INDEX, "router.explicit")
            && edge.to.as_str().ends_with("/buildRouting/explicit")
    }));
}

#[test]
fn returned_explicit_calls_refuse_uncertain_or_replaced_values() {
    let other_returns = "function buildRouting (flag) { function a () {} function b () {}\n\
        if (flag) return { explicit: a }; return { explicit: b }; }\n\
        module.exports = { buildRouting }";
    let mutated_literal = "function buildRouting (replacement) { function a () {}\n\
        const result = { explicit: a }; result.explicit = replacement; return result; }\n\
        module.exports = { buildRouting }";
    let dynamic_after = "function buildRouting (replacement) { function a () {}\n\
        return { explicit: a, ...replacement }; }\nmodule.exports = { buildRouting }";
    let rebound = "function buildRouting (replacement) { function a () {}\n\
        a = replacement; return { explicit: a }; }\nmodule.exports = { buildRouting }";
    for (index, route) in [
        (ALIAS_INDEX, other_returns),
        (ALIAS_INDEX, mutated_literal),
        (ALIAS_INDEX, dynamic_after),
        (ALIAS_INDEX, rebound),
        (MUTATED_INDEX, ALIAS_ROUTE),
    ] {
        let (publication, _, semantics, _) = analyze_returned_calls(index, route).into_parts();
        assert!(publication.edges.iter().all(|edge| {
            edge.kinds != [EdgeKind::Calls] || !occurrence_in_index(edge, index, ALIAS_CALL)
        }));
        assert!(semantics.edges().edges().all(|edge| {
            edge.kind() != EdgeKind::Calls
                || !occurrence_in_index(&edge.to_protocol(), index, ALIAS_CALL)
        }));
    }
}
