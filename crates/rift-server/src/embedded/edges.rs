//! ty type definitions and inheritance behind standard LSP requests.

use ruff_db::{files::File, source::source_text};
use ruff_text_size::TextSize;
use serde_json::{Value, json};
use ty_ide::NavigationTarget;
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
    navigation_locations(database, exchange, targets.into_iter().flatten())
}

pub(super) fn type_definitions(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let targets = ty_ide::goto_type_definition(database, database.program_file(file), offset);
    let targets = targets
        .into_iter()
        .flatten()
        .filter(|target| !names_inference_marker(database, target));
    navigation_locations(database, exchange, targets)
}

/// The vendored stub where ty declares its dynamic markers.
const MARKER_STUB: [&str; 3] = ["stdlib", "ty_extensions", "_internal.pyi"];

/// Whether the target names a marker for incomplete type inference.
fn names_inference_marker(database: &ProjectDatabase, target: &NavigationTarget) -> bool {
    let file = target.file();
    let path = file.path(database);
    if path.as_vendored_path().is_none_or(|path| {
        !path
            .components()
            .map(|component| component.as_str())
            .eq(MARKER_STUB)
    }) {
        return false;
    }
    let text = source_text(database, file);
    let range = target.focus_range();
    let name = text
        .as_str()
        .get(usize::from(range.start())..usize::from(range.end()));
    matches!(name, Some("Divergent" | "Unknown" | "Todo"))
}

pub(super) fn implementations(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let (file, offset) = document_position(database, exchange, params)?;
    let targets = ty_ide::goto_implementation(database, database.program_file(file), offset);
    navigation_locations(database, exchange, targets.into_iter().flatten())
}

fn navigation_locations(
    database: &ProjectDatabase,
    exchange: &Exchange<'_>,
    targets: impl IntoIterator<Item = NavigationTarget>,
) -> Result<Value, AnswerRefusal> {
    let mut locations = Vec::new();
    for target in targets {
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

    #[test]
    fn type_definition_omits_tys_unknown_marker() {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        let file = root.join("app.py");
        std::fs::write(
            &file,
            "x: \"foobar\"\nclass Unknown:\n    pass\nvalue = Unknown()\n",
        )
        .expect("source");
        let uri = super::super::path_to_uri(&file).expect("URI");
        let at = |line, character| json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}});
        let (marker, local) = super::super::with_database(root, |database, canonical_root| {
            let exchange = Exchange {
                spelled_root: root,
                canonical_root,
                sent_text: None,
            };
            Ok((
                type_definitions(database, &exchange, &at(0, 6))?,
                type_definitions(database, &exchange, &at(3, 0))?,
            ))
        })
        .expect("type definitions");
        assert_eq!(marker, json!([]), "{marker}");
        assert_eq!(local[0]["uri"], uri, "{local}");
        assert_eq!(
            local[0]["range"]["start"],
            json!({"line": 1, "character": 6}),
            "{local}"
        );
    }

    /// A pair of implicit type aliases that name each other, and a caller of the function
    /// that returns one of them.
    const RECURSIVE_ALIASES: &str = "from typing import Dict, Tuple, Union\n\nJsonValue = Union[str, \"JsonDict\", Tuple[\"JsonValue\"]]\nJsonDict = Dict[str, JsonValue]\n\n\ndef as_json() -> JsonDict:\n    return {}\n";
    const RECURSIVE_ALIASES_CALLER: &str =
        "from a import as_json\n\n\ndef submit():\n    return as_json()\n";

    /// The variable that names the file, the line and the character the probe asks about.
    const RECURSIVE_ALIASES_POSITION: &str = "RIFT_TEST_RECURSIVE_ALIASES_POSITION";

    /// The wall limit of one definition request, its database build included.
    const RECURSIVE_ALIASES_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

    #[test]
    #[ignore = "a child process of the test that bounds it"]
    fn recursive_aliases_end_a_definition_probe() {
        let position = std::env::var(RECURSIVE_ALIASES_POSITION).expect("the probed position");
        let [file, line, character] = position.split(':').collect::<Vec<_>>()[..] else {
            panic!("a position is `file:line:character`: {position}");
        };
        let line: u64 = line.parse().expect("a line");
        let character: u64 = character.parse().expect("a character");
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        std::fs::write(root.join("a.py"), RECURSIVE_ALIASES).expect("aliases");
        std::fs::write(root.join("b.py"), RECURSIVE_ALIASES_CALLER).expect("caller");
        let params = json!({
            "textDocument": {"uri": super::super::path_to_uri(&root.join(file)).expect("URI")},
            "position": {"line": line, "character": character},
        });
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
            answer[0]["uri"],
            json!(super::super::path_to_uri(&root.join("a.py")).expect("URI")),
            "{answer}"
        );
        assert_eq!(
            answer[0]["range"]["start"],
            json!({"line": 6, "character": 4}),
            "{answer}"
        );
    }

    /// The alias module of [`RECURSIVE_ALIASES`] with a function that returns the other alias.
    const ORDERED_ALIASES: &str = "from typing import Dict, Tuple, Union\n\nJsonValue = Union[str, \"JsonDict\", Tuple[\"JsonValue\"]]\nJsonDict = Dict[str, JsonValue]\n\n\ndef as_json() -> JsonDict:\n    return {}\n\n\ndef element() -> JsonValue:\n    return \"\"\n";
    const ORDERED_ALIASES_CALLER: &str = "from a import as_json, element\n\nvalue = as_json()\nitem = value[\"k\"]\nleaf = element()\n";

    /// The type targets of one position of `b.py` in a tree of [`ORDERED_ALIASES`], asked after
    /// the type of `first` on a new database.
    fn type_targets_after(first: (&str, u64, u64), asked: (u64, u64)) -> Vec<String> {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        std::fs::write(root.join("a.py"), ORDERED_ALIASES).expect("aliases");
        std::fs::write(root.join("b.py"), ORDERED_ALIASES_CALLER).expect("caller");
        let at = |file: &str, line: u64, character: u64| {
            json!({
                "textDocument": {"uri": super::super::path_to_uri(&root.join(file)).expect("URI")},
                "position": {"line": line, "character": character},
            })
        };
        super::super::with_database(root, |database, canonical_root| {
            let exchange = Exchange {
                spelled_root: root,
                canonical_root,
                sent_text: None,
            };
            type_definitions(database, &exchange, &at(first.0, first.1, first.2))?;
            type_definitions(database, &exchange, &at("b.py", asked.0, asked.1))
        })
        .expect("type definition")
        .as_array()
        .expect("locations")
        .iter()
        .map(|location| location["uri"].as_str().expect("a URI").to_owned())
        .collect()
    }

    /// The vendored ty cuts the cycle of these aliases only when the type of a function that
    /// returns one is asked before the type of an alias itself
    /// (`vendor/ty_python_semantic/ORIGIN.md`), so a type definition depends on the first
    /// type request. This states today's answers: after the alias's own type, a value of the
    /// alias has no type target; after the function's, it has the three classes of the cut
    /// union. A value the cut leaves as ty's `Divergent` marker names no type in either order.
    #[test]
    fn recursive_alias_types_depend_on_the_first_request_and_never_name_the_marker() {
        const ALIAS_FIRST: (&str, u64, u64) = ("a.py", 2, 0);
        const FUNCTION_FIRST: (&str, u64, u64) = ("b.py", 2, 8);
        const LEAF: (u64, u64) = (4, 0);
        const ITEM: (u64, u64) = (3, 0);
        const BUILTINS: &str = "vendored://stdlib/builtins.pyi";

        assert_eq!(type_targets_after(ALIAS_FIRST, LEAF), [""; 0]);
        assert_eq!(type_targets_after(FUNCTION_FIRST, LEAF), [BUILTINS; 3]);
        assert_eq!(type_targets_after(ALIAS_FIRST, ITEM), [""; 0]);
        assert_eq!(type_targets_after(FUNCTION_FIRST, ITEM), [""; 0]);
    }

    /// A request ty does not end cannot be canceled from inside the process, so each
    /// position runs in a child the test kills at its wall limit.
    #[test]
    fn recursive_aliases_end_a_definition() {
        for position in ["b.py:4:11", "b.py:0:14", "a.py:6:4"] {
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("the test binary"))
                    .args([
                        "--exact",
                        "embedded::edges::tests::recursive_aliases_end_a_definition_probe",
                        "--ignored",
                    ])
                    .env(RECURSIVE_ALIASES_POSITION, position)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .expect("the probe starts");
            let deadline = std::time::Instant::now() + RECURSIVE_ALIASES_WAIT;
            while child.try_wait().expect("the probe's state").is_none() {
                if std::time::Instant::now() >= deadline {
                    child.kill().expect("the probe is killed");
                    child.wait().expect("the probe is reaped");
                    panic!("{position} did not end within {RECURSIVE_ALIASES_WAIT:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let output = child.wait_with_output().expect("the probe's output");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{position}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
