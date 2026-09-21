//! The two per-file aggregations, over matches built in the test, so no suite
//! opens an index.

use rift_core::ProjectPath;
use rift_search::{DeclarationMatch, VectorMatch, best_per_file, spread_per_file};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn declaration(
    path: &str,
    digest: &str,
    similarity: f32,
) -> Result<DeclarationMatch, Box<dyn std::error::Error + Send + Sync>> {
    let file = ProjectPath::new(path.to_owned())?;
    Ok(DeclarationMatch::new(
        file,
        VectorMatch::new(digest.to_owned(), similarity),
    ))
}

fn paths(matches: &[DeclarationMatch]) -> Vec<&str> {
    matches.iter().map(|entry| entry.file().as_str()).collect()
}

fn digests(matches: &[DeclarationMatch]) -> Vec<&str> {
    matches
        .iter()
        .map(|entry| entry.matched().digest())
        .collect()
}

#[test]
fn a_file_ranks_by_the_best_its_declarations_reached() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.2)?,
        declaration("src/index.rs", "bbb", 0.9)?,
        declaration("src/index.rs", "ccc", 0.5)?,
        declaration("src/search.rs", "ddd", 0.7)?,
    ];
    let best = best_per_file(&matches);
    assert_eq!(paths(&best), ["src/index.rs", "src/search.rs"]);
    assert_eq!(
        digests(&best),
        ["bbb", "ddd"],
        "the file carries the declaration that reached its rank"
    );
    Ok(())
}

#[test]
fn files_of_one_similarity_are_ordered_by_path() -> TestResult {
    let matches = [
        declaration("src/zebra.rs", "aaa", 0.5)?,
        declaration("src/alpha.rs", "bbb", 0.5)?,
    ];
    assert_eq!(
        paths(&best_per_file(&matches)),
        ["src/alpha.rs", "src/zebra.rs"]
    );
    Ok(())
}

#[test]
fn no_declarations_collapse_to_no_files() {
    assert!(best_per_file(&[]).is_empty());
}

#[test]
fn one_declaration_is_its_own_files_rank() -> TestResult {
    let matches = [declaration("src/only.rs", "aaa", 0.4)?];
    let best = best_per_file(&matches);
    assert_eq!(best, matches.to_vec());
    Ok(())
}

#[test]
fn a_per_file_cap_of_zero_keeps_nothing() -> TestResult {
    let matches = [declaration("src/index.rs", "aaa", 0.9)?];
    assert!(spread_per_file(&matches, 0).is_empty());
    assert!(spread_per_file(&[], 4).is_empty());
    Ok(())
}

#[test]
fn a_per_file_cap_drops_only_the_overflow_and_keeps_the_order() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.9)?,
        declaration("src/search.rs", "bbb", 0.8)?,
        declaration("src/index.rs", "ccc", 0.7)?,
        declaration("src/index.rs", "ddd", 0.6)?,
        declaration("src/search.rs", "eee", 0.5)?,
    ];
    assert_eq!(
        digests(&spread_per_file(&matches, 1)),
        ["aaa", "bbb"],
        "one large file cannot crowd the other out"
    );
    assert_eq!(
        digests(&spread_per_file(&matches, 2)),
        ["aaa", "bbb", "ccc", "eee"],
        "the input order is preserved, and only the overflow is dropped"
    );
    Ok(())
}

#[test]
fn a_per_file_cap_above_the_count_keeps_every_match() -> TestResult {
    let matches = [
        declaration("src/index.rs", "aaa", 0.9)?,
        declaration("src/search.rs", "bbb", 0.8)?,
        declaration("src/index.rs", "ccc", 0.7)?,
    ];
    assert_eq!(spread_per_file(&matches, 9), matches.to_vec());
    Ok(())
}

#[test]
fn the_debug_render_names_the_file_and_the_match_it_carries() -> TestResult {
    let matched = declaration("src/index.rs", "aaa", 0.25)?;
    let rendered = format!("{matched:?}");
    assert!(rendered.starts_with("DeclarationMatch"), "{rendered}");
    assert!(rendered.contains("src/index.rs"), "{rendered}");
    assert!(rendered.contains("VectorMatch"), "{rendered}");
    assert_eq!(matched.file().as_str(), "src/index.rs");
    assert_eq!(matched.matched().digest(), "aaa");
    Ok(())
}
