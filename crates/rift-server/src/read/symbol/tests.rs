use super::*;
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::{GetSymbolInclude, SymbolId};
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
        include: GetSymbolInclude {
            documentation: true,
            ..GetSymbolInclude::default()
        },
        revision: None,
        max_results: 1,
        page: 1,
    }
}

#[test]
fn current_singleton_reads_its_revision_without_initializing_the_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("index.ts");
    std::fs::write(&path, "export function known(x: number) { return x; }\n")?;
    let held = build(directory.path())?;
    let id = SymbolId::parse("rift://symbol/local/typescript/.%2Findex.ts/known")?;
    let key = rift_core::SymbolId::new(id.as_str())?;
    let mut params = request(id);
    params.include = GetSymbolInclude::default();

    let first = held.exact_current_symbol(&params)?;
    assert!(matches!(first, GetSymbolResult::Found { .. }));
    assert!(held.index.exact_relative_singleton(&key).is_some());
    assert_eq!(first, held.exact_symbol(&params, true)?);
    assert!(held.index.exact_relative_singleton(&key).is_none());
    assert_eq!(first, held.exact_current_symbol(&params)?);

    std::fs::write(&path, "\nexport function known(x: number) { return x; }\n")?;
    let cold = build(directory.path())?;
    let changes =
        rift_index::PathChanges::between(&held.workspace_digests(), &cold.workspace_digests());
    let current = held.rebuilt(&changes)?;
    let fresh = current.exact_current_symbol(&params)?;
    assert_ne!(first, fresh);
    assert_eq!(fresh, cold.exact_symbol(&params, true)?);
    assert!(current.index.exact_relative_singleton(&key).is_some());
    assert_eq!(first, held.exact_current_symbol(&params)?);

    params.revision = Some(held.served_revision()?);
    assert_eq!(
        current
            .exact_current_symbol(&params)
            .expect_err("the old tree is not served")
            .slug(),
        errors::server::read_revision_unserved::SLUG
    );
    assert!(current.index.exact_relative_singleton(&key).is_some());
    params.revision = Some(current.served_revision()?);
    assert_eq!(fresh, current.exact_current_symbol(&params)?);
    assert!(current.index.exact_relative_singleton(&key).is_none());
    Ok(())
}

#[test]
fn current_exact_symbol_falls_back_for_docs_continuations_and_unavailable_identity() -> TestResult {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("index.ts"),
        "export function known() { return 1; }\n",
    )?;
    std::fs::write(directory.path().join("unavailable.ts"), b"\0binary")?;
    std::fs::write(
        directory.path().join("guide.md"),
        "Use [known](rift://symbol/local/typescript/.%2Findex.ts/known).\n",
    )?;
    let id = SymbolId::parse("rift://symbol/local/typescript/.%2Findex.ts/known")?;
    let key = rift_core::SymbolId::new(id.as_str())?;

    let with_docs = build(directory.path())?;
    let mut params = request(id.clone());
    let result = with_docs.exact_current_symbol(&params)?;
    let GetSymbolResult::Found {
        documentation: Some(documentation),
        ..
    } = &result
    else {
        return Err("documentation must reach the ordinary owner".into());
    };
    assert_eq!(documentation.references.len(), 1);
    assert_eq!(result, with_docs.exact_symbol(&params, true)?);
    assert!(with_docs.index.exact_relative_singleton(&key).is_none());

    let mcp = build(directory.path())?;
    assert_eq!(
        mcp.exact_symbol_without_alternatives(&params, true, true)?,
        result
    );
    assert!(mcp.index.exact_relative_singleton(&key).is_some());

    let continued = build(directory.path())?;
    params.include = GetSymbolInclude::default();
    params.page = 2;
    let result = continued.exact_current_symbol(&params)?;
    let GetSymbolResult::Found { declarations, .. } = &result else {
        return Err("an established empty continuation is still found".into());
    };
    assert!(declarations.is_empty());
    assert_eq!(result, continued.exact_symbol(&params, true)?);
    assert!(continued.index.exact_relative_singleton(&key).is_none());

    let unavailable = build(directory.path())?;
    params.id = SymbolId::parse("rift://symbol/local/typescript/.%2Funavailable.ts/unknown")?;
    params.page = 1;
    assert!(matches!(
        unavailable.exact_current_symbol(&params)?,
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
    Ok(())
}

#[test]
fn exact_hit_after_edit_matches_cold_pages_without_building_lexical_units() -> TestResult {
    let directory = tempfile::tempdir()?;
    let previous = service(directory.path(), true)?;
    let mut params = request(open_identity(&previous)?);
    std::fs::write(
        directory.path().join("index.d.ts"),
        OVERLOADS.replace("First overload", "Edited overload"),
    )?;
    let cold = build(directory.path())?;
    let changes =
        rift_index::PathChanges::between(&previous.workspace_digests(), &cold.workspace_digests());
    assert_eq!(changes.len(), 1);
    let edited = previous.rebuilt(&changes)?;
    params.revision = Some(cold.served_revision()?);
    let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
        .capture("trace")
        .install()?;
    for page in 1..=4 {
        params.page = page;
        let answer = edited.exact_symbol(&params, true)?;
        assert!(matches!(answer, GetSymbolResult::Found { .. }));
        assert_eq!(answer, cold.exact_symbol(&params, true)?);
    }
    assert!(
        drain
            .queued_records()
            .iter()
            .all(|record| record.operation() != "index.lexical_units")
    );
    assert!(!edited.index_documents().is_empty());
    drop(recorder);
    assert!(
        drain
            .queued_records()
            .iter()
            .any(|record| record.operation() == "index.lexical_units")
    );
    Ok(())
}

#[test]
fn declaration_pages_continue_at_the_revision_they_were_read_at() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path(), true)?;
    let id = open_identity(&service)?;
    let served = service.served_revision()?;
    let mut params = request(id.clone());
    let mut starts = Vec::new();
    for page in 1..=4 {
        params.page = page;
        params.revision = Some(served.clone());
        let GetSymbolResult::Found {
            symbol,
            revision,
            declarations,
            pagination,
            documentation,
            ..
        } = service.exact_symbol(&params, true)?
        else {
            return Err("an established symbol is found".into());
        };
        assert_eq!(revision, served);
        assert_eq!(symbol.signatures.len(), 3);
        assert_eq!(pagination, rift_protocol::read::Pagination::new(page, 1, 3));
        assert_eq!(declarations.len(), usize::from(page <= 3));
        assert!(documentation.is_some());
        starts.extend(
            declarations
                .iter()
                .map(|declaration| (declaration.path.clone(), declaration.range.start)),
        );
    }
    starts.sort();
    starts.dedup();
    assert_eq!(starts.len(), 3, "each page answers another declaration");

    let mut first = request(id);
    first.revision = Some(served);
    std::fs::write(
        directory.path().join("index.d.ts"),
        OVERLOADS.replace("First", "1st"),
    )?;
    let moved = build(directory.path())?;
    let refused = moved
        .exact_symbol(&first, true)
        .expect_err("another tree refuses");
    assert_eq!(refused.slug(), errors::server::read_revision_unserved::SLUG);
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

/// A Cargo package `beacon` whose library declares `config`, beside every kind of target
/// a library namespace never reaches: a binary with its own module, a second binary, an
/// example, an integration test, a benchmark, a build script, and a `[[bin]]` at a
/// manifest-named path.
fn rust_package(directory: &std::path::Path, library: &str) -> TestResult {
    let write = |path: &str, text: &str| -> TestResult {
        let path = directory.join(path);
        std::fs::create_dir_all(path.parent().ok_or("parent")?)?;
        std::fs::write(path, text)?;
        Ok(())
    };
    write(
        "Cargo.toml",
        "[package]\nname = 'beacon'\nversion = '0.1.0'\nedition = '2024'\n\n[[bin]]\nname = 'tool'\npath = 'tools/tool.rs'\n",
    )?;
    write("src/lib.rs", library)?;
    write("src/config.rs", "pub fn load_config() {}\n")?;
    write("src/main.rs", "mod cli;\n\nfn main() {}\n")?;
    write("src/cli.rs", "pub fn parse_arguments() {}\n")?;
    write("src/bin/other.rs", "fn main() {}\n")?;
    write("examples/demo/main.rs", "fn main() {}\n")?;
    write("tests/round_trip.rs", "#[test]\nfn round_trip() {}\n")?;
    write("benches/load.rs", "fn main() {}\n")?;
    write("build.rs", "fn main() {}\n")?;
    write("tools/tool.rs", "fn main() {}\n")?;
    Ok(())
}

fn rust_miss(service: &ReadService) -> TestResult<GetSymbolResult> {
    let params = request(SymbolId::parse(
        "rift://symbol/local/rust/beacon/config/load_confg",
    )?);
    Ok(service.exact_symbol(&params, true)?)
}

// Declarations in binary targets carry no identity; they used to mark Rust coverage
// incomplete and turn every exact miss in such a package into `InsufficientCoverage`.
#[test]
fn a_rust_miss_beside_binary_targets_answers_missing_with_alternatives() -> TestResult {
    let directory = tempfile::tempdir()?;
    rust_package(directory.path(), "pub mod config;\n")?;
    let service = build(directory.path())?;
    let GetSymbolResult::Missing {
        symbol_not_found, ..
    } = rust_miss(&service)?
    else {
        return Err("an absent library identity is a definite miss".into());
    };
    assert!(
        symbol_not_found
            .alternatives
            .iter()
            .any(|alternative| alternative.id.as_str()
                == "rift://symbol/local/rust/beacon/config/load_config"),
        "{symbol_not_found:?}"
    );
    Ok(())
}

#[test]
fn a_rust_miss_with_a_library_file_that_failed_analysis_stays_unavailable() -> TestResult {
    let directory = tempfile::tempdir()?;
    rust_package(directory.path(), "pub mod config;\npub fn broken( {}\n")?;
    let service = build(directory.path())?;
    assert!(matches!(
        rust_miss(&service)?,
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
    Ok(())
}

// A module file whose declaring library file failed analysis is reached by no library
// namespace. The binary declares that same library file, so the binary does not reach the
// module either: its declarations may be library items.
#[test]
fn a_rust_miss_with_a_module_its_library_parent_hides_stays_unavailable() -> TestResult {
    let directory = tempfile::tempdir()?;
    rust_package(directory.path(), "pub mod config;\npub mod nested;\n")?;
    std::fs::write(
        directory.path().join("src/main.rs"),
        "mod cli;\nmod nested;\n\nfn main() {}\n",
    )?;
    std::fs::write(
        directory.path().join("src/nested.rs"),
        "pub mod inner;\npub fn broken( {}\n",
    )?;
    std::fs::create_dir_all(directory.path().join("src/nested"))?;
    std::fs::write(
        directory.path().join("src/nested/inner.rs"),
        "pub fn hidden() {}\n",
    )?;
    let service = build(directory.path())?;
    assert!(matches!(
        rust_miss(&service)?,
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
    Ok(())
}

#[test]
fn a_rust_file_no_target_reaches_keeps_a_miss_unavailable() -> TestResult {
    let directory = tempfile::tempdir()?;
    rust_package(directory.path(), "pub mod config;\n")?;
    std::fs::write(
        directory.path().join("src/orphan.rs"),
        "pub fn unreached() {}\n",
    )?;
    let service = build(directory.path())?;
    assert!(matches!(
        rust_miss(&service)?,
        GetSymbolResult::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            ..
        }
    ));
    Ok(())
}

// A module both the library and a binary declare is a library module: its declarations
// keep their library identities, and the binary does not hide them.
#[test]
fn a_module_a_library_and_a_binary_share_keeps_its_library_identity() -> TestResult {
    let directory = tempfile::tempdir()?;
    rust_package(directory.path(), "pub mod config;\n")?;
    std::fs::write(
        directory.path().join("src/main.rs"),
        "mod cli;\nmod config;\n\nfn main() {}\n",
    )?;
    let service = build(directory.path())?;
    let params = request(SymbolId::parse(
        "rift://symbol/local/rust/beacon/config/load_config",
    )?);
    assert!(matches!(
        service.exact_symbol(&params, true)?,
        GetSymbolResult::Found { .. }
    ));
    assert!(matches!(
        rust_miss(&service)?,
        GetSymbolResult::Missing { .. }
    ));
    Ok(())
}
