use super::{PackagePublication, decode_package_records, encode_package_records};
use serde_json::{Value, json};

use crate::canonical::package_record_digest;

const UNIT: &str = "rift://source/pypi/pypi.org/demo/demo/__init__.py";
const MODULE: &str = "rift://symbol/pypi/pypi.org/demo/python/demo";
const LEAF: &str = "rift://symbol/pypi/pypi.org/demo/python/demo/leaf";
const EXTERNAL: &str = "rift://symbol/stdlib/cpython/python/builtins/len";
const SOURCE: &str = "def leaf():\n    return len(())\n";

fn publication() -> PackagePublication {
    let owner = json!({
        "kind": "package", "manager": "pypi", "registry": "pypi.org", "name": "demo"
    });
    let authored = json!({
        "location": "dependency",
        "package": {
            "manager": "pypi", "registry": "pypi.org", "name": "demo", "version": "1.0.0"
        },
        "source_kind": "authored"
    });
    let declaration = |symbol, origin: Value, source: &str| {
        json!({
            "symbol": symbol, "origin": origin, "unit": UNIT,
            "range": {"start": 0, "end": source.len()},
            "line": 1, "signature_indices": [], "type_indices": [],
            "documentation_indices": [], "source": source, "public": true,
            "source_complete": true, "digest": "00000000"
        })
    };
    let call = SOURCE.find("len").expect("call occurrence");
    let mut publication = serde_json::from_value(json!({
        "format_revision": 6, "identity_format": crate::identity::SYMBOL_IDENTITY_FORMAT_REVISION,
        "analyzer_revision": "00000000", "owner": owner, "source_digest": "00000000",
        "revision": "rift://revision/pypi/pypi.org/demo@1.0.0",
        "units": [{
            "unit": UNIT, "path": "demo/__init__.py", "language": "python",
            "content_digest": "00000000", "source": SOURCE, "source_complete": true,
            "digest": "00000000"
        }],
        "objects": [
            {"id": MODULE, "name": "demo", "language": "python", "kind": "module",
             "kinds": ["module"], "origin": authored},
            {"id": LEAF, "name": "leaf", "language": "python", "kind": "function",
             "kinds": ["callable"], "container": MODULE}
        ],
        "declarations": [
            declaration(MODULE, authored.clone(), SOURCE),
            declaration(LEAF, authored, SOURCE)
        ],
        "edges": [{
            "from": LEAF, "to": EXTERNAL, "kind": "calls", "kinds": ["calls"], "provider": "static",
            "derivation": "resolution",
            "occurrence": {"unit": UNIT, "range": {"start": call, "end": call + 3}}
        }],
        "coverage": [{
            "language": "python", "inventory_complete": true, "identity_complete": true,
            "applicability_complete": false, "edges": [], "facts": [{"kind": "edges"}]
        }],
        "documents": [{
            "identity": UNIT, "kind": "file", "unit": UNIT, "language": "python",
            "owner": owner, "content_digest": "00000000", "name": "__init__.py",
            "file_content": SOURCE, "digest": "00000000"
        }],
        "documentation": {
            "documentation_revision": "00000000", "selection_digest": "0".repeat(64),
            "sources": [], "blocks": [], "links": [], "references": [],
            "unresolved_references": [], "warnings": [],
            "coverage": crate::documentation::DocumentationCoverage::default()
        }
    }))
    .expect("fixture publication");
    refresh_digests(&mut publication);
    publication
}

fn refresh_digests(publication: &mut PackagePublication) {
    for unit in &mut publication.units {
        unit.digest = package_record_digest(unit).expect("unit digest");
    }
    for declaration in &mut publication.declarations {
        declaration.digest = package_record_digest(declaration).expect("declaration digest");
    }
    for document in &mut publication.documents {
        document.digest = package_record_digest(document).expect("document digest");
    }
}

#[test]
fn package_records_restore_unit_paths_origins_and_source() {
    let mut expected = publication();
    let bytes = encode_package_records(&expected).expect("write publication");
    let compact: Value = serde_json::from_str(&bytes).expect("record form");
    let unit = &compact["records"]["units"][0];
    assert_eq!(unit["base"], "python/demo");
    assert!(unit.get("unit").is_none());
    assert_eq!(unit["declarations"][0]["symbol"], "~");
    assert!(unit["declarations"][0].get("origin").is_none());
    assert!(unit["declarations"][0].get("source").is_none());
    assert_eq!(unit["edges"][0]["from"], "~/leaf");
    assert_eq!(unit["edges"][0]["to"], EXTERNAL);
    assert_eq!(unit["documents"][0]["file_content_from_unit"], true);
    let actual = decode_package_records(bytes.as_bytes()).expect("restore publication");
    expected.format_revision = 7;
    assert_eq!(actual, expected);
    assert_eq!(
        encode_package_records(&actual).expect("write restored rows"),
        bytes
    );
}

#[test]
fn package_records_preserve_explicit_cuts_and_unit_origin() {
    let mut expected = publication();
    expected.target = Some("x86_64-unknown-linux-gnu".into());
    expected.units[0].source_complete = false;
    expected.documents[0].file_content = None;
    for declaration in &mut expected.declarations {
        declaration.origin.source_kind = crate::read::SourceKind::Generated;
    }
    refresh_digests(&mut expected);
    let bytes = encode_package_records(&expected).expect("write publication");
    let compact: Value = serde_json::from_str(&bytes).expect("record form");
    let unit = &compact["records"]["units"][0];
    assert_eq!(unit["origin"]["source_kind"], "generated");
    assert!(unit["declarations"][0].get("origin").is_none());
    assert_eq!(unit["declarations"][0]["source"], SOURCE);
    assert!(unit["documents"][0].get("file_content_from_unit").is_none());
    expected.format_revision = 7;
    assert_eq!(
        decode_package_records(bytes.as_bytes()).expect("restore publication"),
        expected
    );
}

#[test]
fn package_records_preserve_a_foreign_or_custom_unit_identity() {
    for id in [
        "rift://source/pypi/pypi.org/companion/demo/__init__.py",
        "rift://source/external/demo/__init__.py",
    ] {
        let mut expected = publication();
        expected.format_revision = super::PACKAGE_PUBLICATION_FORMAT_REVISION;
        expected.units[0].unit.0 = id.into();
        for declaration in &mut expected.declarations {
            declaration.unit.0 = id.into();
        }
        expected.documents[0].identity = id.into();
        expected.documents[0].unit.0 = id.into();
        expected.edges[0]
            .occurrence
            .as_mut()
            .expect("source occurrence")
            .unit
            .0 = id.into();
        refresh_digests(&mut expected);
        let bytes = encode_package_records(&expected).expect("write publication");
        let compact: Value = serde_json::from_str(&bytes).expect("record form");
        assert_eq!(compact["records"]["units"][0]["unit"], id);
        assert_eq!(
            decode_package_records(bytes.as_bytes()).expect("restore publication"),
            expected
        );

        expected.units[0].path.0 = "another.py".into();
        assert!(encode_package_records(&expected).is_err());
    }
}

#[test]
fn package_records_check_header_counts_before_decoding_rows() {
    let bytes = encode_package_records(&publication()).expect("encode fixture");
    assert!(bytes.starts_with("{\"header\":{"));

    let mut malformed: Value = serde_json::from_str(&bytes).expect("record form");
    malformed["header"]["counts"]["units"] = json!(crate::index::PACKAGE_UNITS_CEILING + 1);
    malformed["records"] = json!(false);
    let bytes = loop {
        let rendered = crate::canonical::canonical_json(&malformed).expect("frame fixture");
        let length = json!(rendered.len());
        if malformed["header"]["canonical_bytes"] == length {
            break rendered;
        }
        malformed["header"]["canonical_bytes"] = length;
    };

    let refusal = decode_package_records(bytes.as_bytes()).expect_err("count exceeds bound");
    assert!(
        refusal
            .to_string()
            .contains("publication record counts exceed their bounds"),
        "{refusal}"
    );
}

#[test]
fn package_records_refuse_modified_unit_content() {
    let bytes = encode_package_records(&publication()).expect("encode fixture");
    let mut compact: Value = serde_json::from_str(&bytes).expect("record form");
    compact["records"]["units"][0]["source"] = json!(SOURCE.replace("len", "min"));
    let modified = crate::canonical::canonical_json(&compact).expect("modified record form");
    assert_eq!(modified.len(), bytes.len());
    let refusal = decode_package_records(modified.as_bytes()).expect_err("digest differs");
    assert!(
        refusal
            .to_string()
            .contains("publication record digest differs"),
        "{refusal}"
    );
}

#[test]
fn package_records_keep_unit_bytes_across_release_only_changes() {
    let original = publication();
    let mut next = original.clone();
    next.revision =
        crate::revision::Revision::release(&next.owner, "2.0.0").expect("next package release");
    for origin in next
        .declarations
        .iter_mut()
        .map(|declaration| &mut declaration.origin)
        .chain(next.objects.iter_mut().map(|symbol| &mut symbol.origin))
    {
        if let Some(package) = &mut origin.package {
            package.version = "2.0.0".into();
        }
    }
    refresh_digests(&mut next);
    assert_ne!(original.declarations[0].digest, next.declarations[0].digest);
    let encode = |publication: &PackagePublication| {
        let bytes = encode_package_records(publication).expect("encode fixture release");
        serde_json::from_str::<Value>(&bytes).expect("record form")
    };
    let left = encode(&original);
    let right = encode(&next);
    assert_ne!(left["header"]["revision"], right["header"]["revision"]);
    assert_eq!(left["records"]["units"], right["records"]["units"]);
}

#[test]
fn package_records_conform_to_the_publication_schema() {
    let bytes = encode_package_records(&publication()).expect("encode fixture");
    let compact: Value = serde_json::from_str(&bytes).expect("record form");
    let schema: Value = serde_json::from_str(&crate::schema::package_publication_document(7))
        .expect("publication schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("publication schema");
    if let Err(error) = validator.validate(&compact) {
        panic!("encoded records must follow the schema: {error}");
    }
    let mut bad = compact;
    bad["records"]["units"][0]["edges"][0]["occurrence"] = json!([1, 2, 3]);
    assert!(!validator.is_valid(&bad));
}

#[test]
fn package_records_restore_retained_flat_bytes_and_require_the_revisioned_shape() {
    let mut expected = publication();
    let legacy = crate::canonical::canonical_json(&expected).expect("flat artifact");
    let restored = super::decode_package_publication(legacy.as_bytes()).expect("decode revision 6");
    assert_eq!(restored, expected);
    assert_eq!(
        super::encode_package_publication(&restored).expect("retain legacy bytes"),
        legacy,
    );

    expected.format_revision = crate::index::PACKAGE_PUBLICATION_FORMAT_REVISION;
    let packed = super::encode_package_publication(&expected).expect("encode revision 7");
    assert!(packed.starts_with("{\"header\":{"));
    assert_eq!(
        super::decode_package_publication(packed.as_bytes()).expect("decode revision 7"),
        expected,
    );
    let flat = crate::canonical::canonical_json(&expected).expect("flat revision 7 value");
    assert!(super::decode_package_publication(flat.as_bytes()).is_err());
}
