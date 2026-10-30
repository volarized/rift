use super::*;
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::SymbolId;
use rift_protocol::revision::Revision;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const OVERLOADS: &str = "/** First overload. */\nexport declare function open(path: string): void;\n/** Second overload. */\nexport declare function open(path: number): void;\n";

/// A JavaScript package whose `open` has three declarations, and the service over it.
fn service(directory: &std::path::Path, metadata: bool) -> TestResult<ReadService> {
    if metadata {
        std::fs::write(
            directory.join("package.json"),
            r#"{"name":"beacon","version":"1.0.0","main":"index.js","types":"index.d.ts"}"#,
        )?;
    }
    std::fs::write(
        directory.join("index.js"),
        "/** Implementation. */\nexport function open(path) {}\n",
    )?;
    std::fs::write(directory.join("index.d.ts"), OVERLOADS)?;
    build(directory)
}

fn build(directory: &std::path::Path) -> TestResult<ReadService> {
    Ok(ReadService::build(
        directory,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )?)
}

fn open_identity(service: &ReadService) -> TestResult<SymbolId> {
    let matched = service.index.symbols("open", 1)?[0];
    let identity = service
        .index
        .assembled_symbol(matched)?
        .identity()
        .ok_or("canonical identity")?
        .as_str()
        .to_owned();
    Ok(SymbolId(identity))
}

fn request(id: SymbolId) -> GetSymbolParams {
    GetSymbolParams {
        id,
        include: vec![GetSymbolInclude::Documentation],
        revision: None,
        declaration_limit: 1,
        declaration_cursor: None,
    }
}

#[test]
fn declaration_pages_continue_at_the_revision_they_were_read_at() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path(), true)?;
    let id = open_identity(&service)?;
    let served = service.served_revision()?;
    let mut params = request(id.clone());
    let mut starts = Vec::new();
    for page in 0..3 {
        let result = service.exact_symbol(&params, true)?;
        assert!(result.is_valid_for(&id));
        let GetSymbolResult::Found {
            symbol,
            revision,
            declarations,
            declaration_cursor,
            documentation,
            ..
        } = result
        else {
            return Err("an established symbol is found".into());
        };
        assert_eq!(revision, served);
        assert_eq!(symbol.signatures.len(), 3);
        assert_eq!(declarations.len(), 1);
        assert!(documentation.is_some());
        starts.push((declarations[0].path.clone(), declarations[0].range.start));
        assert_eq!(declaration_cursor.is_some(), page < 2, "page {page}");
        params.declaration_cursor = declaration_cursor;
    }
    starts.sort();
    starts.dedup();
    assert_eq!(starts.len(), 3, "each page answers another declaration");

    let mut first = request(id.clone());
    let GetSymbolResult::Found {
        declaration_cursor: Some(cursor),
        ..
    } = service.exact_symbol(&first, true)?
    else {
        return Err("a first page leaves a cursor".into());
    };
    std::fs::write(
        directory.path().join("index.d.ts"),
        OVERLOADS.replace("First", "1st"),
    )?;
    let moved = build(directory.path())?;
    first.declaration_cursor = Some(cursor.clone());
    let refused = moved
        .exact_symbol(&first, true)
        .expect_err("a cursor minted at another tree refuses");
    assert_eq!(refused.slug(), errors::server::read_invalid::SLUG);
    assert!(
        refused
            .context()
            .any(|(name, value)| name == "field" && value == "declaration_cursor")
    );
    first.id = SymbolId::parse("rift://symbol/local/javascript/beacon/close")?;
    assert!(
        service.exact_symbol(&first, true).is_err(),
        "a cursor for another symbol refuses"
    );
    Ok(())
}

#[test]
fn a_tree_revision_the_service_no_longer_serves_refuses() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path(), true)?;
    let mut params = request(open_identity(&service)?);
    params.revision = Some(service.served_revision()?);
    assert!(matches!(
        service.exact_symbol(&params, true)?,
        GetSymbolResult::Found { .. }
    ));
    params.revision = Some(Revision::tree(&"0".repeat(64))?);
    let refused = service
        .exact_symbol(&params, true)
        .expect_err("another tree is not served");
    assert_eq!(refused.slug(), errors::server::read_revision_unserved::SLUG);
    Ok(())
}

#[test]
fn a_miss_needs_complete_identity_coverage() -> TestResult {
    for metadata in [true, false] {
        let directory = tempfile::tempdir()?;
        let service = service(directory.path(), metadata)?;
        let params = request(SymbolId::parse(
            "rift://symbol/local/javascript/beacon/missing",
        )?);
        let result = service.exact_symbol(&params, true)?;
        if metadata {
            assert!(matches!(result, GetSymbolResult::Missing { .. }));
        } else {
            assert!(matches!(
                result,
                GetSymbolResult::Unavailable {
                    reason: SymbolUnavailableReason::InsufficientCoverage,
                    ..
                }
            ));
        }
    }
    Ok(())
}

#[test]
fn an_unready_publication_answers_index_preparing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path(), true)?;
    let params = request(open_identity(&service)?);
    assert!(matches!(
        service.exact_symbol(&params, false)?,
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::IndexPreparing,
            revision: None,
            ..
        }
    ));
    Ok(())
}

#[test]
fn a_symbol_past_the_public_name_bound_answers_unavailable_with_its_warning() -> TestResult {
    let directory = tempfile::tempdir()?;
    let name = "x".repeat(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX + 1);
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"name":"beacon","version":"1.0.0","main":"index.js"}"#,
    )?;
    std::fs::write(
        directory.path().join("index.js"),
        format!("export function {name}() {{}}\n"),
    )?;
    let service = build(directory.path())?;
    let matched = service.index.symbols(&name, 1)?[0];
    let assembled = service.index.assembled_symbol(matched)?;
    let identity = assembled
        .identity()
        .ok_or("the long declaration has an established identity")?
        .as_str()
        .to_owned();
    let answer = service.exact_symbol(&request(SymbolId(identity)), true)?;
    let GetSymbolResult::Unavailable {
        reason, warnings, ..
    } = answer
    else {
        return Err(format!("a name past the bound is not served: {answer:?}").into());
    };
    assert_eq!(reason, SymbolUnavailableReason::InsufficientCoverage);
    assert!(
        warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::ObjectUnavailable { .. })),
        "{warnings:?}"
    );
    Ok(())
}
