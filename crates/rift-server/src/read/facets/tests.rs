use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_error::errors;
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::dependencies::{DependenciesConfiguration, DependencyResolution};
use rift_protocol::read::{FacetsParams, FacetsResult, SymbolId};

use super::{PageBounds, symbol_page};
use crate::read::{ReadService, RiftError, symbol_id};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A Rust module holding a function and a constant nested in it, beside a free function.
const RUST_SOURCE: &str = "pub mod lamp {\n    pub fn light() -> u8 {\n        1 + 2\n    }\n\n    pub const GLOW: u8 = 3;\n}\n\npub fn dim(level: u8) -> u8 {\n    level / 2\n}\n";

fn service(root: &Path) -> Result<ReadService, RiftError> {
    service_with_limits(root, WorkspaceIndexLimits::default())
}

fn service_with_limits(
    root: &Path,
    limits: WorkspaceIndexLimits,
) -> Result<ReadService, RiftError> {
    ReadService::build_with_languages(
        root,
        limits,
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
        .symbols(name, service.index().limits().results_max().min(10))?
        .iter()
        .find(|matched| matched.symbol.name == name)
        .ok_or("fixture declares the name")?;
    Ok(symbol_id(service.index(), matched)?.ok_or("established identity")?)
}

fn symbol_facets(service: &ReadService, id: &SymbolId) -> Result<FacetsResult, RiftError> {
    service.facets(&FacetsParams::of(id.clone()))
}

#[test]
fn each_facet_carries_declaration_source_and_syntax_structure() -> TestResult {
    let (_directory, service) = rust_service()?;
    let id = identity(&service, "dim")?;
    let answer = symbol_facets(&service, &id)?;
    assert_eq!(answer.symbol, id);
    assert_eq!(answer.declarations.len(), 1);
    let declaration = &answer.declarations[0];
    assert_eq!(declaration.declaration, 0);
    assert_eq!(declaration.kind.0, "function_item");
    let range = &declaration.range;
    assert_eq!(
        declaration.source.text,
        RUST_SOURCE[usize::try_from(range.start)?..usize::try_from(range.end)?]
    );
    assert_eq!(declaration.source.span.range, *range);
    assert!(
        declaration
            .source
            .span
            .unit
            .as_str()
            .ends_with("src/lib.rs")
    );
    assert!(
        declaration
            .syntax
            .iter()
            .any(|element| element.kind.0 == "binary_expression" && element.source == "level / 2")
    );
    for (index, element) in declaration.syntax.iter().enumerate() {
        assert_eq!(element.declaration, Some(0));
        assert_eq!(element.index, index as u64);
        assert_eq!(
            element.source,
            RUST_SOURCE[usize::try_from(element.range.start)?..usize::try_from(element.range.end)?]
        );
        assert!(element.parent.is_none_or(|parent| parent < element.index));
    }
    Ok(())
}

#[test]
fn nested_declarations_keep_their_own_syntax_structure() -> TestResult {
    let (_directory, service) = rust_service()?;
    let module = symbol_facets(&service, &identity(&service, "lamp")?)?;
    let function = symbol_facets(&service, &identity(&service, "light")?)?;
    assert!(module.declarations[0].source.text.contains("pub fn light"));
    assert!(
        !module.declarations[0]
            .syntax
            .iter()
            .any(|element| element.kind.0 == "function_item")
    );
    assert_eq!(function.declarations[0].kind.0, "function_item");
    assert!(
        function.declarations[0]
            .syntax
            .iter()
            .any(|element| element.kind.0 == "binary_expression" && element.source == "1 + 2")
    );
    Ok(())
}

#[test]
fn declaration_page_keeps_syntax_and_refuses_a_source_byte_breach() -> TestResult {
    let (_directory, service) = rust_service()?;
    let id = identity(&service, "dim")?;
    let key = rift_core::SymbolId::new(id.as_str())?;
    let error = symbol_page(
        service.index(),
        (&id, &key),
        0,
        PageBounds {
            facets_max: 1,
            source_bytes_max: 1,
        },
    )
    .expect_err("source byte bound");
    assert_eq!(error.slug(), errors::server::read_unavailable::SLUG);
    let rest = service.facets(&FacetsParams {
        page: 2,
        max_results: 1,
        ..FacetsParams::of(id)
    })?;
    assert!(rest.declarations.is_empty());
    assert_eq!(
        rest.pagination,
        rift_protocol::read::Pagination::new(2, 1, 1)
    );
    Ok(())
}

#[test]
fn missing_symbol_refuses() -> TestResult {
    let (_directory, service) = rust_service()?;
    let error = service
        .facets(&FacetsParams::of(SymbolId(
            "rift://symbol/local/rust/lamp/absent".to_owned(),
        )))
        .expect_err("missing symbol refuses");
    assert_eq!(error.slug(), errors::server::read_symbol_not_found::SLUG);
    Ok(())
}

#[test]
fn declaration_pages_keep_every_syntax_tree() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"lamp\"\nversion = \"1.0.0\"\n",
    )?;
    fs::create_dir(directory.path().join("src"))?;
    let mut source = String::new();
    for value in 0..6 {
        writeln!(source, "pub fn tick() -> u8 {{ {value} }}")?;
    }
    fs::write(directory.path().join("src/lib.rs"), &source)?;
    let limits = WorkspaceIndexLimits::default().with_read_bounds(32, 4)?;
    let service = service_with_limits(directory.path(), limits)?;
    let id = identity(&service, "tick")?;
    let first = service.facets(&FacetsParams {
        max_results: 4,
        ..FacetsParams::of(id.clone())
    })?;
    assert_eq!(first.declarations.len(), 4);
    let second = service.facets(&FacetsParams {
        page: 2,
        max_results: 4,
        revision: Some(first.revision.clone()),
        ..FacetsParams::of(id)
    })?;
    assert_eq!(second.declarations.len(), 2);
    assert_eq!(
        second.pagination,
        rift_protocol::read::Pagination::new(2, 4, 6)
    );
    assert_eq!(first.revision, second.revision);
    let listed = first
        .declarations
        .iter()
        .chain(&second.declarations)
        .collect::<Vec<_>>();
    for (ordinal, declaration) in listed.iter().enumerate() {
        assert_eq!(declaration.declaration, ordinal as u64);
        assert_eq!(declaration.syntax[0].index, 0);
        assert_eq!(
            declaration.source.text,
            source[usize::try_from(declaration.range.start)?
                ..usize::try_from(declaration.range.end)?]
        );
        assert!(
            declaration
                .syntax
                .iter()
                .all(|element| element.declaration == Some(ordinal as u64))
        );
    }
    Ok(())
}

/// Complete syntax is refused before projection when repeated nested source crosses the byte bound.
#[test]
fn declaration_source_past_the_accepted_byte_bound_refuses() -> TestResult {
    let (directory, _) = rust_service()?;
    let source = format!(
        "pub fn lantern() {{ {} let value = \"{}\"; {} }}",
        "{".repeat(24),
        "x".repeat(65_536),
        "}".repeat(24),
    );
    fs::write(directory.path().join("src/lib.rs"), source)?;
    let service = service(directory.path())?;
    let id = identity(&service, "lantern")?;
    let error = service
        .facets(&FacetsParams::of(id))
        .expect_err("nested syntax source exceeds the accepted byte bound");
    assert_eq!(error.slug(), errors::server::read_unavailable::SLUG);
    assert!(error.to_string().contains("source byte bound"));
    Ok(())
}
