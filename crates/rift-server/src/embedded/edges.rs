//! ty type definitions and inheritance behind standard LSP requests.

use ruff_db::{files::File, source::source_text};
use ruff_text_size::TextSize;
use serde_json::{Value, json};
use ty_project::{ProjectDatabase, SemanticDb as _};

use super::{
    AnswerRefusal, Exchange, call_hierarchy_anchor, file_at, offset_at, range_at, target_location,
};

fn document_position(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<(File, TextSize), AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer(super::DOCUMENT_URI).unwrap_or(&Value::Null),
    )?;
    super::open_file(database, file);
    let text = source_text(database, file);
    let offset = offset_at(
        exchange.conversion_text(text.as_str()),
        params.pointer("/position").unwrap_or(&Value::Null),
    )?;
    Ok((file, offset))
}

pub(super) fn definitions(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let targets = ty_ide::goto_definition(database, database.program_file(file), offset);
    navigation_locations(database, exchange, targets)
}

pub(super) fn type_definitions(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let targets = ty_ide::goto_type_definition(database, database.program_file(file), offset);
    navigation_locations(database, exchange, targets)
}

pub(super) fn implementations(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let targets = ty_ide::goto_implementation(database, database.program_file(file), offset);
    navigation_locations(database, exchange, targets)
}

fn navigation_locations(
    database: &ProjectDatabase,
    exchange: &Exchange<'_>,
    targets: Option<ty_ide::RangedValue<ty_ide::NavigationTargets>>,
) -> Result<Value, AnswerRefusal> {
    let mut locations = Vec::new();
    for target in targets.into_iter().flat_map(|targets| targets.value) {
        if let Some((uri, range)) =
            target_location(database, exchange, target.file(), target.focus_range())?
        {
            locations.push(json!({"uri": uri, "range": range}));
        }
    }
    Ok(Value::Array(locations))
}

pub(super) fn prepare_types(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let item = ty_ide::prepare_type_hierarchy(database, database.program_file(file), offset);
    type_items(database, exchange, item)
}

pub(super) fn supertypes(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = call_hierarchy_anchor(database, exchange, params)?;
    let items = ty_ide::type_hierarchy_supertypes(database, database.program_file(file), offset);
    type_items(database, exchange, items)
}

pub(super) fn subtypes(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = call_hierarchy_anchor(database, exchange, params)?;
    let items = ty_ide::type_hierarchy_subtypes(database, database.program_file(file), offset);
    type_items(database, exchange, items)
}

fn type_items(
    database: &ProjectDatabase,
    exchange: &Exchange<'_>,
    items: impl IntoIterator<Item = ty_ide::TypeHierarchyItem>,
) -> Result<Value, AnswerRefusal> {
    let mut prepared = Vec::new();
    for item in items {
        let Some((uri, range)) = target_location(database, exchange, item.file, item.full_range)?
        else {
            continue;
        };
        let text = source_text(database, item.file);
        prepared.push(json!({
            "name": item.name.as_str(), "kind": lsp_types::SymbolKind::CLASS,
            "uri": uri, "range": range,
            "selectionRange": range_at(text.as_str(), item.selection_range)?,
        }));
    }
    Ok(Value::Array(prepared))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keeps_standard_library_definition_locations() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        let file = root.join("app.py");
        std::fs::write(&file, "def size():\n    return len([])\n").expect("source");
        let params = json!({"textDocument":{"uri":super::super::path_to_uri(&file).expect("URI")},"position":{"line":1,"character":12}});
        let answer = super::super::with_database(root, |database, canonical_root| {
            definitions(
                database,
                &Exchange {
                    spelled_root: root,
                    canonical_root,
                    sent_text: None,
                },
                &params,
            )
        })
        .expect("definition");
        assert_eq!(
            answer[0]["uri"], "vendored://stdlib/builtins.pyi",
            "{answer}"
        );
        assert_ne!(answer[0]["range"]["start"], Value::Null, "{answer}");
    }

    #[test]
    fn inferred_instance_type_names_its_local_class() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='types'\nversion='0.1.0'\n",
        )
        .expect("metadata");
        std::fs::write(root.join("graph.py"), "class Parent:\n    pass\n\nclass Child(Parent):\n    pass\n\ndef make() -> Child:\n    return Child()\n\nvalue = make()\n").expect("source");
        let params = json!({"textDocument":{"uri":super::super::path_to_uri(&root.join("graph.py")).expect("URI")},"position":{"line":9,"character":0}});
        let answer = super::super::with_database(root, |database, canonical_root| {
            type_definitions(
                database,
                &Exchange {
                    spelled_root: root,
                    canonical_root,
                    sent_text: None,
                },
                &params,
            )
        })
        .expect("type definition");
        assert_eq!(
            answer[0]["range"]["start"],
            json!({"line":3,"character":6}),
            "{answer}"
        );
    }
}
