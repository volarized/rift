//! Adapts the logical publication schema to the package record form.

use serde_json::{Map, Value, json};

use crate::index::{PACKAGE_DOCUMENTS_CEILING, PACKAGE_SYMBOLS_CEILING, PACKAGE_UNITS_CEILING};
use crate::read::{IDENTITY_PATH_CHARACTER, LANGUAGE_IDENTITY_PATTERN};
use crate::source::SOURCE_EDGES_MAX;

type Object = Map<String, Value>;

pub(super) fn document(mut logical: Value, revision: u32) -> Value {
    let properties = logical["properties"]
        .as_object()
        .expect("publication schema properties");
    let header = header(properties, &logical["required"], revision);
    let mut definitions = logical["$defs"]
        .as_object()
        .expect("publication definitions")
        .clone();
    let records = records(properties, &mut definitions);
    logical["properties"] = json!({"header": header, "records": records});
    logical["required"] = json!(["header", "records"]);
    logical["$defs"] = Value::Object(definitions);
    logical
}

fn header(properties: &Object, required: &Value, revision: u32) -> Value {
    let row_lists = [
        "units",
        "objects",
        "declarations",
        "edges",
        "occurrences",
        "documents",
    ];
    let mut header = object(properties.clone(), required.clone());
    for name in row_lists {
        omit(&mut header, name);
    }
    header["properties"]["format_revision"]["const"] = json!(revision);
    header["properties"]["canonical_bytes"] = json!({"type": "integer", "minimum": 1});
    header["properties"]["source_bytes"] = json!({"type": "integer", "minimum": 0});
    let counts = [
        ("units", u64::from(PACKAGE_UNITS_CEILING)),
        ("objects", u64::from(PACKAGE_SYMBOLS_CEILING)),
        ("declarations", u64::from(PACKAGE_SYMBOLS_CEILING)),
        ("edges", SOURCE_EDGES_MAX),
        ("occurrences", SOURCE_EDGES_MAX),
        ("documents", u64::from(PACKAGE_DOCUMENTS_CEILING)),
    ];
    header["properties"]["counts"] = object(
        counts
            .into_iter()
            .map(|(name, maximum)| {
                (
                    name.to_owned(),
                    json!({"type": "integer", "minimum": 0, "maximum": maximum}),
                )
            })
            .collect(),
        json!(counts.map(|(name, _)| name)),
    );
    header["required"]
        .as_array_mut()
        .expect("header required fields")
        .extend(
            ["canonical_bytes", "source_bytes", "counts"]
                .into_iter()
                .map(|name| json!(name)),
        );
    header
}

fn records(properties: &Object, definitions: &mut Object) -> Value {
    let mut objects = properties["objects"].clone();
    for name in ["id", "container"] {
        objects["items"]["properties"][name] = identity(false, true);
    }
    objects["items"]["properties"]["origin"]
        .as_object_mut()
        .expect("symbol origin property")
        .remove("default");

    let mut unit = definitions["PackageSourceUnit"].clone();
    omit(&mut unit, "unit");
    unit["properties"]["base"] = identity(false, false);
    unit["properties"]["origin"] = json!({"$ref": "#/$defs/SymbolOrigin"});
    unit["properties"]["declarations"] = array(
        "PackageDeclarationRecord",
        u64::from(PACKAGE_SYMBOLS_CEILING),
    );
    unit["properties"]["edges"] = array("PackageEdgeRecord", SOURCE_EDGES_MAX);
    unit["properties"]["occurrences"] = array("PackageOccurrenceRecord", SOURCE_EDGES_MAX);
    unit["properties"]["documents"] = array(
        "PackageDocumentRecord",
        u64::from(PACKAGE_DOCUMENTS_CEILING),
    );
    definitions.insert("PackageUnitRecord".into(), unit);

    let mut declaration = definitions["PackageSymbol"].clone();
    omit(&mut declaration, "unit");
    optional(&mut declaration, "origin");
    optional(&mut declaration, "source");
    declaration["properties"]["symbol"] = identity(true, true);
    declaration["properties"]["range"] = range();
    definitions.insert("PackageDeclarationRecord".into(), declaration);

    let mut occurrence = definitions["PackageOccurrence"].clone();
    omit(&mut occurrence, "unit");
    occurrence["properties"]["symbol"] = identity(true, true);
    occurrence["properties"]["range"] = range();
    definitions.insert("PackageOccurrenceRecord".into(), occurrence);

    let mut loose_edge = definitions["StoredEdge"].clone();
    omit(&mut loose_edge, "occurrence");
    for name in ["from", "to"] {
        loose_edge["properties"][name] = identity(false, true);
    }
    let mut edge = loose_edge.clone();
    edge["properties"]["occurrence"] = range();
    edge["required"]
        .as_array_mut()
        .expect("edge required fields")
        .push(json!("occurrence"));
    for name in ["from", "to"] {
        edge["properties"][name] = identity(true, true);
    }
    definitions.insert("PackageEdgeRecord".into(), edge);
    definitions.insert("PackageEdgeWithoutSource".into(), loose_edge);
    definitions.insert("PackageDocumentRecord".into(), document_record(definitions));
    object(
        [
            (
                "units".to_owned(),
                array("PackageUnitRecord", u64::from(PACKAGE_UNITS_CEILING)),
            ),
            ("objects".to_owned(), objects),
            (
                "edges".to_owned(),
                array("PackageEdgeWithoutSource", SOURCE_EDGES_MAX),
            ),
        ]
        .into_iter()
        .collect(),
        json!(["units", "objects", "edges"]),
    )
}

fn document_record(definitions: &Object) -> Value {
    let mut symbol = definitions["PackageDocument"].clone();
    omit(&mut symbol, "unit");
    let mut file = symbol.clone();
    omit(&mut file, "identity");
    file["properties"]["kind"] = json!({"const": "file"});
    file["properties"]["file_content_from_unit"] = json!({"const": true});
    file["not"] = json!({"required": ["file_content", "file_content_from_unit"]});
    symbol["properties"]["kind"] = json!({"const": "symbol"});
    symbol["properties"]["identity"] = identity(true, true);
    json!({"oneOf": [symbol, file]})
}

fn range() -> Value {
    json!({"type": "array", "minItems": 2, "maxItems": 2,
           "items": {"type": "integer", "minimum": 0}})
}

fn identity(unit: bool, foreign: bool) -> Value {
    let language = LANGUAGE_IDENTITY_PATTERN
        .strip_prefix('^')
        .and_then(|p| p.strip_suffix('$'))
        .expect("language pattern anchors");
    let path = format!("{IDENTITY_PATH_CHARACTER}+(?:/{IDENTITY_PATH_CHARACTER}+)*");
    let owner = json!({"type": "string", "pattern": format!("^{language}/{path}$")});
    let mut forms = vec![owner];
    if unit {
        forms.push(json!({"type": "string", "pattern": format!("^~(?:/{path})?$")}));
    }
    if foreign {
        forms.push(json!({"$ref": "#/$defs/SymbolId"}));
    }
    if forms.len() == 1 {
        forms.pop().expect("owner-relative spelling")
    } else {
        json!({"anyOf": forms})
    }
}

fn object(properties: Object, required: Value) -> Value {
    Value::Object(
        [
            ("type".into(), "object".into()),
            ("properties".into(), Value::Object(properties)),
            ("additionalProperties".into(), Value::Bool(false)),
            ("required".into(), required),
        ]
        .into_iter()
        .collect(),
    )
}

fn array(item: &str, maximum: u64) -> Value {
    json!({"type": "array", "maxItems": maximum, "items": {"$ref": format!("#/$defs/{item}")}})
}

fn omit(record: &mut Value, field: &str) {
    record["properties"]
        .as_object_mut()
        .expect("record properties")
        .remove(field);
    optional(record, field);
}

fn optional(record: &mut Value, field: &str) {
    record["required"]
        .as_array_mut()
        .expect("record required fields")
        .retain(|name| name.as_str() != Some(field));
}
