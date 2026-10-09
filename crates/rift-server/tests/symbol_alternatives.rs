//! Symbol misses propose existing declarations without changing ordinary lookup pages.

use std::error::Error;
use std::fs;

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::{GetSymbolParams, GetSymbolResult, ReadWarning, SYMBOL_ALTERNATIVES_MAX};
use rift_server::ReadService;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn fixture(source: &str) -> TestResult<(tempfile::TempDir, ReadService)> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), source)?;
    fs::write(
        directory.path().join("other.ts"),
        "export function beacpn() {}\n",
    )?;
    let service = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )?;
    Ok((directory, service))
}

fn lookup(service: &ReadService, request: Value) -> TestResult<GetSymbolResult> {
    let params: GetSymbolParams = serde_json::from_value(request)?;
    Ok(service.get_symbol(&params)?)
}

fn alternatives(result: &GetSymbolResult) -> TestResult<Vec<&str>> {
    let alternatives = result
        .warnings
        .iter()
        .find_map(|warning| match warning {
            ReadWarning::SymbolNotFound { alternatives, .. } => Some(alternatives),
            _ => None,
        })
        .ok_or("symbol_not_found absent")?;
    Ok(alternatives
        .iter()
        .map(|identity| identity.0.as_str())
        .collect())
}

#[test]
fn miss_orders_three_alternatives_by_distance_and_qualified_name() -> TestResult {
    let (_directory, service) = fixture(
        "pub fn beacon_c() {}\npub fn beacon_b() {}\npub fn beacon() {}\npub fn beacon_a() {}\n",
    )?;
    let result = lookup(&service, json!({"name": "beacpn", "language": "rust"}))?;
    assert!(
        result.hits.is_empty(),
        "alternatives must not become matched declarations"
    );
    assert_eq!(
        alternatives(&result)?,
        [
            "rift://symbol/rust/lib.rs/beacon",
            "rift://symbol/rust/lib.rs/beacon_a",
            "rift://symbol/rust/lib.rs/beacon_b",
        ]
    );
    assert_eq!(alternatives(&result)?.len(), SYMBOL_ALTERNATIVES_MAX);
    let exact = lookup(
        &service,
        json!({"name": "beacpn", "language": "typescript"}),
    )?;
    assert_eq!(exact.hits.len(), 1);
    assert!(
        !exact
            .warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::SymbolNotFound { .. }))
    );
    Ok(())
}

#[test]
fn miss_uses_unicode_characters_and_case_insensitive_names() -> TestResult {
    let (_directory, service) = fixture("pub fn cafè() {}\npub fn café() {}\npub fn cafaa() {}\n")?;
    let result = lookup(&service, json!({"name": "CAFÊ", "language": "rust"}))?;
    assert_eq!(
        alternatives(&result)?,
        [
            "rift://symbol/rust/lib.rs/caf%C3%A8",
            "rift://symbol/rust/lib.rs/caf%C3%A9",
            "rift://symbol/rust/lib.rs/cafaa",
        ]
    );
    Ok(())
}

#[test]
fn miss_compares_qualified_names_and_orders_equal_names_by_path() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("z.rs"), "pub fn beacon() {}\n")?;
    fs::write(directory.path().join("a.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("lib.rs"),
        "pub struct Tower;\nimpl Tower { pub fn load() {} }\npub fn load() {}\n",
    )?;
    let service = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )?;
    let tied = lookup(&service, json!({"name": "beacpn"}))?;
    assert_eq!(
        &alternatives(&tied)?[..2],
        [
            "rift://symbol/rust/a.rs/beacon",
            "rift://symbol/rust/z.rs/beacon",
        ]
    );
    let qualified = lookup(&service, json!({"name": "Tower::lpad"}))?;
    let exact = lookup(&service, json!({"name": "Tower::load"}))?;
    let identity = exact.hits[0]
        .symbol
        .id
        .as_ref()
        .ok_or("declaration identity absent")?;
    assert_eq!(alternatives(&qualified)?[0], identity.0);
    Ok(())
}

#[test]
fn empty_project_keeps_missed_name_and_empty_alternatives() -> TestResult {
    let (_directory, service) = fixture("")?;
    let result = lookup(&service, json!({"name": "beacpn", "language": "rust"}))?;
    assert!(result.hits.is_empty());
    assert!(alternatives(&result)?.is_empty());
    assert_eq!(result.pagination.total_pages, 0);
    Ok(())
}

#[test]
fn miss_with_one_declaration_or_no_selected_language_keeps_the_requested_name() -> TestResult {
    let (_directory, service) = fixture("pub fn beacon() {}\n")?;
    let one = lookup(&service, json!({"name": "beacpn", "language": "rust"}))?;
    assert_eq!(alternatives(&one)?.len(), 1);
    let empty = lookup(&service, json!({"name": "beacpn", "language": "go"}))?;
    assert!(alternatives(&empty)?.is_empty());
    assert!(empty.warnings.iter().any(|warning| matches!(warning,
        ReadWarning::SymbolNotFound { name, .. } if name == "beacpn"
    )));
    let global = lookup(&service, json!({"name": "beacpn", "scope": "global"}))?;
    assert!(
        alternatives(&global)?.is_empty(),
        "global misses must not propose project declarations"
    );
    Ok(())
}

#[test]
fn exact_prefix_and_substring_matches_and_exhausted_pages_have_no_alternatives() -> TestResult {
    let (_directory, service) =
        fixture("pub fn beacon() {}\npub fn beacon_a() {}\npub fn other_beacon() {}\n")?;
    for request in [
        json!({"name": "beacon", "language": "rust"}),
        json!({"name": "beac", "language": "rust"}),
        json!({"name": "acon", "language": "rust"}),
        json!({"name": "beacon", "language": "rust", "limit": 1, "page_index": 20}),
    ] {
        let result = lookup(&service, request)?;
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::SymbolNotFound { .. }))
        );
        assert!(result.pagination.total_pages > 0);
    }
    Ok(())
}

#[test]
fn lookup_name_enforces_its_advertised_character_bound() -> TestResult {
    let (_directory, service) = fixture("pub fn beacon() {}\n")?;
    let maximum = rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX;
    lookup(
        &service,
        json!({"name": "é".repeat(maximum), "language": "rust"}),
    )?;
    for name in [String::new(), "é".repeat(maximum + 1)] {
        let params: GetSymbolParams = serde_json::from_value(json!({"name": name}))?;
        let error = service
            .get_symbol(&params)
            .expect_err("a name outside its bound must refuse");
        assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "field" && value == "name")
        );
    }
    Ok(())
}
