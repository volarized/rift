use super::*;
use crate::{PackageImportRootOrigin, PackageSource};
use rift_core::{ContributionOrigin, ProjectPath, SourcePath, SourceResolverId, SourceUnitId};
use rift_protocol::identity::SourceDigest;
use rift_protocol::read::{SourceKind, SourceLocation};
use rift_syntax::{PythonSyntaxProvider, SyntaxProvider, SyntaxSource};
use sha2::{Digest as _, Sha256};

fn anchors(text: &str, parsed: &str, captured: bool) -> BTreeMap<String, SymbolId> {
    let path = ProjectPath::new("main.py").expect("selected source");
    let original = SourcePath::new("captured/main.py").expect("original member");
    let unit = SourceUnitId::new(
        SourceResolverId::new("external").expect("resolver"),
        original.clone(),
    )
    .expect("captured unit");
    let origin = ContributionOrigin::new(Some(SourceLocation::External {}), SourceKind::Authored)
        .expect("captured origin");
    let digest =
        SourceDigest::parse(&data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes())))
            .expect("complete digest");
    let source = PackageSource::new(&path, text);
    let source = if captured {
        source
            .with_captured_source_unit(&unit, &origin, &original, &digest)
            .expect("checked immutable bytes")
    } else {
        source
    };
    let files = [source];
    let roots = [PackageImportRoot::new(
        None,
        vec!["main".to_owned()],
        PackageImportRootOrigin::PyModules,
    )
    .expect("explicit module")];
    let provider = PythonSyntaxProvider::default();
    let limits = SyntaxLimits::default();
    let syntax = provider
        .analyze(
            SyntaxSource {
                path: &path,
                text: parsed,
            },
            limits,
        )
        .expect("syntax facts")
        .into_facts();
    let owner = SymbolOwner::Local;
    let input = NamespaceInput::workspace(
        &owner,
        provider.language(),
        &files,
        &[],
        &roots,
        (
            1,
            u64::try_from(text.len()).expect("source bytes").max(1),
            limits,
        ),
    )
    .expect("captured namespace");
    identity_anchors_with_roots(&input, &syntax, path.as_str(), source, &roots)
}

#[test]
fn python_captured_and_uncaptured_digest_keep_identical_unique_identity() {
    let text = "def beacon():\n    return 1\n";
    let ordinary = anchors(text, text, false);
    assert_eq!(ordinary.len(), 1);
    assert_eq!(anchors(text, text, true), ordinary);
    let identity = SymbolIdentity::parse(ordinary["beacon"].as_str()).expect("canonical identity");
    assert_eq!(identity.owner(), &SymbolOwner::Local);
}

#[test]
fn python_cached_digest_rejects_facts_from_different_bytes() {
    let text = "def beacon():\n    return 1\n";
    let changed = "def beacon():\n    return 2\n";
    assert!(anchors(text, changed, true).is_empty());
    assert!(anchors(text, changed, false).is_empty());
    assert_eq!(anchors(changed, changed, true).len(), 1);
}

#[test]
fn python_repeated_declarations_share_one_identity() {
    let text = "def beacon():\n    return 1\ndef beacon():\n    return 2\n";
    let ordinary = anchors(text, text, false);
    let captured = anchors(text, text, true);
    assert_eq!(ordinary, captured);
    assert_eq!(ordinary.len(), 2);
    let identities = captured.values().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identities
            .into_iter()
            .map(SymbolId::as_str)
            .collect::<Vec<_>>(),
        ["rift://symbol/local/python/main/beacon"]
    );
}

#[test]
fn python_captured_digest_refuses_malformed_source_facts() {
    let malformed = "def beacon(\n";
    assert!(anchors(malformed, malformed, true).is_empty());
    assert!(anchors(malformed, malformed, false).is_empty());
}
use rift_protocol::identity::SymbolOwner;
use rift_syntax::SyntaxLimits;
