use serde_json::json;

use super::{REVISION_BYTES_MAX, Revision, RevisionPoint, RevisionViolation};
use crate::identity::SymbolOwner;

const COMMIT: &str = "1f2080e49da12fee4431e6872630509355cd62d1";
const TREE: &str = "4e1b6c1ab5d6ddbdd1d7e3a7e38fc1bf1d2a35c6a1b0a7c8a4e0bd0c7a9b6f11";

fn violation(value: &str) -> String {
    let error = Revision::parse(value).expect_err("the spelling breaks the revision form");
    error.to_string()
}

#[test]
fn every_form_round_trips_through_its_iri() {
    let accepted = [
        format!("rift://revision/git/{COMMIT}"),
        format!("rift://revision/git/{TREE}"),
        "rift://revision/git/main".to_owned(),
        "rift://revision/git/origin/release-1.2".to_owned(),
        "rift://revision/git/HEAD~2".to_owned(),
        format!("rift://revision/tree/{TREE}"),
        "rift://revision/cargo/crates.io/serde@1.0.228".to_owned(),
        "rift://revision/npm/npmjs.org/@types/node@26.6.4".to_owned(),
        "rift://revision/stdlib/cpython@3.12.9".to_owned(),
    ];
    for value in accepted {
        let revision = Revision::parse(&value).expect("accepted revision");
        assert_eq!(revision.to_string(), value);
    }
}

#[test]
fn a_git_revision_names_a_commit_only_with_a_complete_lowercase_id() {
    let commit = Revision::parse(&format!("rift://revision/git/{COMMIT}")).expect("commit");
    assert_eq!(commit.point(), RevisionPoint::Commit(COMMIT));
    assert_eq!(commit, Revision::commit(COMMIT).expect("commit"));
    let short = Revision::parse("rift://revision/git/1f2080e").expect("short id");
    assert_eq!(short.point(), RevisionPoint::Reference("1f2080e"));
    let upper = COMMIT.to_uppercase();
    let reference = Revision::parse(&format!("rift://revision/git/{upper}")).expect("reference");
    assert_eq!(reference.point(), RevisionPoint::Reference(upper.as_str()));
    assert!(Revision::commit("main").is_err());
    assert_eq!(
        reference.git_revision().map(|revision| revision.0),
        Some(upper)
    );
}

#[test]
fn release_and_tree_forms_carry_their_coordinates_and_no_git_spelling() {
    let release = Revision::parse("rift://revision/cargo/crates.io/serde@1.0.228").expect("ok");
    let owner = SymbolOwner::Package {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "serde".to_owned(),
        version: "1.0.228".to_owned(),
    };
    assert_eq!(release.point(), RevisionPoint::Release(&owner));
    assert_eq!(release, Revision::release(&owner).expect("release"));
    assert_eq!(release.git_revision(), None);
    let tree = Revision::tree(TREE).expect("tree");
    assert_eq!(tree.point(), RevisionPoint::Tree(TREE));
    assert_eq!(tree.git_revision(), None);
    assert!(Revision::release(&SymbolOwner::Local).is_err());
    assert!(Revision::tree(COMMIT).is_err());
}

#[test]
fn each_broken_spelling_names_its_violation() {
    let long = format!("rift://revision/git/{}", "a".repeat(REVISION_BYTES_MAX));
    let refused = [
        (long.as_str(), RevisionViolation::Length),
        ("rift://symbol/git/main", RevisionViolation::Structure),
        ("rift://revision/git", RevisionViolation::Structure),
        ("rift://revision/git/main^2", RevisionViolation::Reference),
        ("rift://revision/git/HEAD@{1}", RevisionViolation::Reference),
        ("rift://revision/git//main", RevisionViolation::Reference),
        ("rift://revision/tree/abc", RevisionViolation::Digest),
        (
            "rift://revision/cargo/crates.io/serde",
            RevisionViolation::Release,
        ),
        (
            "rift://revision/local/app@1.0.0",
            RevisionViolation::Release,
        ),
        (
            "rift://revision/stdlib/cpython@3.12",
            RevisionViolation::Release,
        ),
    ];
    for (value, expected) in refused {
        let message = violation(value);
        assert!(
            message.contains("field revision") && message.contains(expected.as_str()),
            "refusal must name the revision field and its violation: value={value}, message={message}"
        );
    }
}

#[test]
fn serde_spells_the_iri_and_refuses_a_broken_one() {
    let revision = Revision::tree(TREE).expect("tree");
    let value = serde_json::to_value(&revision).expect("serialize");
    assert_eq!(value, json!(format!("rift://revision/tree/{TREE}")));
    let decoded: Revision = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded, revision);
    let refused = serde_json::from_value::<Revision>(json!("rift://revision/tree/0"));
    assert!(refused.is_err());
}

/// The advertised pattern accepts every schema example and the codec accepts them too, so
/// the schema and the codec describe one form.
#[test]
fn the_schema_pattern_and_the_codec_accept_the_same_examples() {
    let schema = serde_json::to_value(schemars::schema_for!(Revision)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("validator");
    let examples = schema["examples"].as_array().expect("examples");
    for example in examples {
        assert!(validator.is_valid(example), "pattern accepts {example}");
        let spelling = example.as_str().expect("string example");
        assert!(
            Revision::parse(spelling).is_ok(),
            "codec accepts {spelling}"
        );
    }
    assert!(!validator.is_valid(&json!("rift://revision/git/main^2")));
}
