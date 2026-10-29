use std::fs;
use std::path::Path;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_error::errors;
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::dependencies::{DependenciesConfiguration, DependencyResolution};
use rift_protocol::read::{NodesParams, NodesResult, ProjectPath, SymbolId};

use super::{PageBounds, symbol_page};
use crate::read::{ReadService, RiftError, symbol_id};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A Rust module holding a function and a constant nested in it, beside a free function.
const RUST_SOURCE: &str = "pub mod lamp {\n    pub fn light() -> u8 {\n        1 + 2\n    }\n\n    pub const GLOW: u8 = 3;\n}\n\npub fn dim(level: u8) -> u8 {\n    level / 2\n}\n";

fn service(root: &Path) -> Result<ReadService, RiftError> {
    ReadService::build_with_languages(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        &LanguageFileSelections::default(),
        HistoryConfiguration::default(),
        DependenciesConfiguration {
            resolution: DependencyResolution::Static,
            ..DependenciesConfiguration::default()
        },
    )
}

fn rust_service() -> TestResult<(tempfile::TempDir, ReadService)> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"lamp\"\nversion = \"1.0.0\"\n",
    )?;
    fs::create_dir(directory.path().join("src"))?;
    fs::write(directory.path().join("src/lib.rs"), RUST_SOURCE)?;
    let service = service(directory.path())?;
    Ok((directory, service))
}

/// The canonical identity of the first declaration named `name`.
fn identity(service: &ReadService, name: &str) -> TestResult<SymbolId> {
    let matched = *service
        .index()
        .symbols(name, 10)?
        .iter()
        .find(|matched| matched.symbol.name == name)
        .ok_or("fixture declares the name")?;
    Ok(symbol_id(service.index(), matched)?.ok_or("established identity")?)
}

fn symbol_nodes(service: &ReadService, id: &SymbolId) -> Result<NodesResult, RiftError> {
    service.nodes(&NodesParams::of(id.clone()))
}

#[test]
fn a_symbol_read_lists_its_item_node_then_its_own_descendants_with_their_source() -> TestResult {
    let (_directory, service) = rust_service()?;
    let lamp = identity(&service, "lamp")?;

    let answer = symbol_nodes(&service, &lamp)?;

    let item = answer.nodes.first().ok_or("the module has nodes")?;
    assert_eq!(item.kind.0, "mod_item");
    assert_eq!(answer.symbol.as_ref(), Some(&lamp));
    assert_eq!(
        (item.declaration, item.index, item.parent),
        (Some(0), 0, None)
    );
    let module_end = RUST_SOURCE.find("}\n\npub fn dim").ok_or("module end")? + 1;
    assert_eq!(item.source, RUST_SOURCE[..module_end]);
    for node in &answer.nodes {
        let start = usize::try_from(node.range.start)?;
        let end = usize::try_from(node.range.end)?;
        assert_eq!(node.source, RUST_SOURCE[start..end]);
        assert!(
            node.range.end <= item.range.end,
            "every node lies inside the item: {node:?}"
        );
    }
    let nested_start = u64::try_from(RUST_SOURCE.find("pub fn light").ok_or("nested")?)?;
    assert!(
        answer
            .nodes
            .iter()
            .all(|node| node.range.start < nested_start || node.kind.0 == "declaration_list"),
        "nodes of the nested declarations belong to them, not to the module: {:?}",
        answer.nodes
    );
    for (offset, node) in answer.nodes.iter().enumerate() {
        assert_eq!(
            node.declaration,
            Some(0),
            "every node is of the one declaration"
        );
        assert_eq!(
            node.index,
            u64::try_from(offset)?,
            "indexes count in pre-order"
        );
    }
    assert_eq!(answer.node_cursor, None);
    Ok(())
}

#[test]
fn a_nested_declaration_lists_its_own_subtree() -> TestResult {
    let (_directory, service) = rust_service()?;
    let light = identity(&service, "light")?;

    let answer = symbol_nodes(&service, &light)?;

    let kinds = answer
        .nodes
        .iter()
        .map(|node| node.kind.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(kinds.first(), Some(&"function_item"));
    assert!(kinds.contains(&"binary_expression"), "{kinds:?}");
    assert!(answer.nodes.iter().any(|node| node.source == "1 + 2"));
    Ok(())
}

#[test]
fn a_symbol_with_several_declarations_lists_each_outermost_first() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("package.json"),
        r#"{"name":"beacon","version":"1.0.0","main":"index.js","types":"index.d.ts"}"#,
    )?;
    fs::write(
        directory.path().join("index.js"),
        "/** Implementation. */\nexport function open(path) {}\n",
    )?;
    let declarations = "/** First overload. */\nexport declare function open(path: string): void;\n/** Second overload. */\nexport declare function open(path: number): void;\n";
    fs::write(directory.path().join("index.d.ts"), declarations)?;
    let service = service(directory.path())?;
    let open = identity(&service, "open")?;
    let key = rift_core::SymbolId::new(open.as_str())?;
    let (bindings, total) = service.index().symbols_by_identity_page(&key, 0, 10)?;
    assert_eq!(total, 3, "one symbol, three declarations");

    let answer = symbol_nodes(&service, &open)?;

    let items = answer
        .nodes
        .iter()
        .filter(|node| node.index == 0)
        .collect::<Vec<_>>();
    assert_eq!(items.len(), 3, "one item node per declaration");
    assert_eq!(answer.symbol.as_ref(), Some(&open));
    assert_eq!(
        items
            .iter()
            .map(|item| item.declaration)
            .collect::<Vec<_>>(),
        [Some(0), Some(1), Some(2)],
        "each declaration is numbered in binding order"
    );
    for (item, binding) in items.iter().zip(&bindings) {
        assert_eq!(
            item.unit,
            rift_protocol::read::NodeUnit::File(crate::read::file_id(binding.file.path())),
            "item nodes follow binding order"
        );
        assert_eq!(item.range.start, binding.symbol.item_range.start);
    }
    let sources = items
        .iter()
        .map(|item| item.source.as_str())
        .collect::<Vec<_>>();
    let spellings = ["open(path)", "open(path: string)", "open(path: number)"];
    assert_eq!(sources.len(), spellings.len());
    for spelling in spellings {
        assert!(
            sources.iter().any(|source| source.contains(spelling)),
            "an item node carries `{spelling}`: {sources:?}"
        );
    }
    for item in &items {
        let start = usize::try_from(item.range.start)?;
        let end = usize::try_from(item.range.end)?;
        if item.unit.as_str() == "rift://file/index.d.ts" {
            assert_eq!(item.source, declarations[start..end]);
        }
    }
    Ok(())
}

#[test]
fn pages_of_a_symbol_read_resume_through_the_cursor_until_the_last_node() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;
    let whole = symbol_nodes(&service, &dim)?;
    assert!(
        whole.nodes.len() > 3,
        "the fixture function has several nodes"
    );
    let key = rift_core::SymbolId::new(dim.as_str())?;
    let revision = "tree";
    let bounds = PageBounds {
        nodes_max: 2,
        source_bytes_max: u64::MAX,
    };

    let mut listed = Vec::new();
    let mut offset = 0;
    loop {
        let page = symbol_page(service.index(), (&dim, &key), (offset, revision), bounds)?
            .ok_or("the symbol has a declaration")?;
        assert!(page.nodes.len() <= 2);
        listed.extend(page.nodes);
        let Some(cursor) = page.node_cursor else {
            break;
        };
        offset = super::cursor_offset(&cursor, dim.as_str(), revision)?;
        assert_eq!(offset, listed.len());
    }

    assert_eq!(listed, whole.nodes);
    Ok(())
}

#[test]
fn a_page_past_its_source_bound_still_answers_one_node() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;
    let key = rift_core::SymbolId::new(dim.as_str())?;
    let bounds = PageBounds {
        nodes_max: 100,
        source_bytes_max: 1,
    };

    let page = symbol_page(service.index(), (&dim, &key), (0, "tree"), bounds)?
        .ok_or("the symbol has a declaration")?;

    assert_eq!(page.nodes.len(), 1, "the item node alone crosses the bound");
    assert_eq!(page.symbol.as_ref(), Some(&dim));
    assert_eq!(page.nodes[0].index, 0);
    assert!(page.node_cursor.is_some());
    Ok(())
}

#[test]
fn a_served_cursor_continues_the_read_and_refuses_another_symbol() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;
    let lamp = identity(&service, "lamp")?;
    let revision = service.revisions.index_tree_revision.clone();
    let cursor = super::cursor(1, dim.as_str(), &revision);

    let rest = service.nodes(&NodesParams {
        node_cursor: Some(cursor.clone()),
        ..NodesParams::of(dim.clone())
    })?;
    let whole = symbol_nodes(&service, &dim)?;
    assert_eq!(rest.nodes, whole.nodes[1..]);

    let foreign = service
        .nodes(&NodesParams {
            node_cursor: Some(cursor),
            ..NodesParams::of(lamp)
        })
        .expect_err("a cursor minted for another symbol refuses");
    assert_eq!(foreign.slug(), errors::server::read_invalid::SLUG);
    let malformed = service
        .nodes(&NodesParams {
            node_cursor: Some("seven".to_owned()),
            ..NodesParams::of(dim)
        })
        .expect_err("a cursor no read returned refuses");
    assert_eq!(malformed.slug(), errors::server::read_invalid::SLUG);
    Ok(())
}

#[test]
fn a_request_selects_exactly_one_of_symbol_and_path() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;
    let path = ProjectPath("src/lib.rs".to_owned());
    let refusals = [
        (
            NodesParams {
                symbol: Some(dim.clone()),
                ..NodesParams::at(path.clone(), 0)
            },
            "symbol",
        ),
        (NodesParams::default(), "path"),
        (
            NodesParams {
                path: Some(path.clone()),
                ..NodesParams::default()
            },
            "position",
        ),
    ];
    for (params, field) in refusals {
        let error = service
            .nodes(&params)
            .expect_err("the selector combination refuses");
        assert_eq!(error.slug(), errors::server::read_invalid::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "field" && value == field),
            "{field}: {error}"
        );
    }
    Ok(())
}

#[test]
fn a_symbol_without_a_declaration_refuses_naming_it() -> TestResult {
    let (_directory, service) = rust_service()?;
    let absent = SymbolId("rift://symbol/local/rust/src/lib.rs/absent".to_owned());

    let error = symbol_nodes(&service, &absent).expect_err("no declaration answers");

    assert_eq!(error.slug(), errors::server::read_symbol_not_found::SLUG);
    assert!(
        error
            .context()
            .any(|(key, value)| key == "id" && value == absent.as_str())
    );
    Ok(())
}

#[test]
fn a_cursor_beside_a_position_continues_the_answer_it_came_with() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;
    let path = ProjectPath("src/lib.rs".to_owned());
    let position = u64::try_from(RUST_SOURCE.find("level / 2").ok_or("body")?)?;
    let revision = service.revisions.index_tree_revision.clone();

    let rest = service.nodes(&NodesParams {
        node_cursor: Some(super::cursor(1, dim.as_str(), &revision)),
        ..NodesParams::at(path.clone(), position)
    })?;

    assert_eq!(rest.symbol.as_ref(), Some(&dim));
    assert_eq!(rest.nodes, symbol_nodes(&service, &dim)?.nodes[1..]);
    let foreign = service
        .nodes(&NodesParams {
            node_cursor: Some(super::cursor(1, "rift://file/src/lib.rs", &revision)),
            ..NodesParams::at(path, position)
        })
        .expect_err("a cursor minted for the unit refuses inside a declaration");
    assert_eq!(foreign.slug(), errors::server::read_invalid::SLUG);
    Ok(())
}

#[test]
fn a_position_outside_every_declaration_answers_the_unit_from_its_file_node() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n",
    )?;
    fs::create_dir(directory.path().join("src"))?;
    let source = "use std::fmt;\n\npub fn dim() {}\n";
    fs::write(directory.path().join("src/lib.rs"), source)?;
    let service = service(directory.path())?;

    let answer = service.nodes(&NodesParams::at(ProjectPath("src/lib.rs".to_owned()), 4))?;

    assert_eq!(answer.symbol, None);
    let file = answer.nodes.first().ok_or("the unit holds its file node")?;
    assert_eq!(
        (file.kind.0.as_str(), file.index, file.parent),
        (rift_protocol::read::FILE_NODE_KIND, 0, None)
    );
    assert_eq!(file.source, source);
    assert!(answer.nodes.iter().all(|node| node.declaration.is_none()));
    assert!(
        answer
            .nodes
            .iter()
            .all(|node| node.range.start < 15 || node.range.start == 0),
        "the function's nodes belong to its declaration: {:?}",
        answer.nodes
    );
    let root = answer.nodes.get(1).ok_or("the grammar's root follows")?;
    assert_eq!((root.index, root.parent), (1, Some(0)));
    Ok(())
}

#[test]
fn a_symbol_read_during_preparation_answers_as_the_settled_read() -> TestResult {
    let (_directory, service) = rust_service()?;
    let dim = identity(&service, "dim")?;

    let preparing = service.nodes_for_preparation(&NodesParams::of(dim.clone()))?;

    assert_eq!(preparing, symbol_nodes(&service, &dim)?);
    Ok(())
}
