use super::*;
use rift_core::{
    ContributionEdge, ContributionOrigin, ContributionReference, DeclarationBinding, EdgeKind,
    EquivalenceEvidence, ExactKind, PortableSymbolFacts, ProviderId, ProviderRevision,
    ProviderSymbolId, SourceKind, SourceLocation, SourcePath, SourceResolverId, SymbolId,
};
use rift_protocol::read::Language;

const SHARED: &str = "rift://symbol/local/rust/edited/Shared";
const MARKER: &str = "rift://symbol/local/rust/edited/Marker";

fn unit(path: &str) -> SourceUnitId {
    SourceUnitId::new(
        SourceResolverId::new("project").unwrap(),
        SourcePath::new(path).unwrap(),
    )
    .unwrap()
}

fn local(name: &str) -> ContributionReference {
    ContributionReference::new(
        ProviderId::new("syntax").unwrap(),
        ProviderSymbolId::new(name).unwrap(),
    )
}

fn revisions(revision: u64) -> (IndexRevision, SourceRevision, TreeRevision) {
    (
        IndexRevision::new(revision).unwrap(),
        SourceRevision::new(revision).unwrap(),
        TreeRevision::new(revision).unwrap(),
    )
}

fn publication(
    revision: u64,
    descriptions: &[(&str, &str, u64, Option<&str>)],
    extra_claim: Option<&str>,
) -> Arc<PublicationSet> {
    let provider = ProviderId::new("syntax").unwrap();
    let publication_revision = ProviderRevision::new(revision).unwrap();
    let (_, source_revision, tree_revision) = revisions(revision);
    let contributions = descriptions
        .iter()
        .map(|&(path, name, start, identity)| {
            let mut row = Contribution::builder(
                ContributionKey::new(
                    provider.clone(),
                    publication_revision,
                    local(name).symbol().clone(),
                ),
                SourceApplicability::Exact {
                    source_revision,
                    tree_revision,
                },
                PortableSymbolFacts::new(
                    Language::from_identity_segment("rust").unwrap(),
                    name,
                    name,
                    ExactKind("struct".to_owned()),
                ),
                ContributionOrigin::new(
                    Some(SourceLocation::Project { package: None }),
                    SourceKind::Authored,
                )
                .unwrap(),
            )
            .source(DeclarationBinding::new(
                unit(path),
                SourceRange::new(start, start + 1).unwrap(),
            ));
            if let Some(identity) = identity {
                row = row.identity_anchor(SymbolId::new(identity).unwrap());
            }
            if name == "incoming" {
                row = row.edges(vec![ContributionEdge::new(
                    EdgeKind::Alias,
                    local("marker"),
                )]);
            }
            if extra_claim == Some(name) {
                row = row.equivalence(vec![EquivalenceEvidence::Explicit(local("first"))]);
            }
            row.build().unwrap()
        })
        .collect();
    let limits = crate::PublicationLimits::default();
    let publication =
        ProviderPublication::new(provider, publication_revision, contributions, limits).unwrap();
    Arc::new(PublicationSet::empty(limits).replaced(publication).unwrap())
}

const ORIGINAL: &[(&str, &str, u64, Option<&str>)] = &[
    ("edited.rs", "first", 4, Some(SHARED)),
    ("edited.rs", "middle", 6, None),
    ("edited.rs", "last", 4, None),
    ("other.rs", "other", 9, Some(SHARED)),
    ("caller.rs", "incoming", 10, None),
];

const WITH_MARKER: &[(&str, &str, u64, Option<&str>)] = &[
    ("edited.rs", "first", 5, Some(SHARED)),
    ("edited.rs", "marker", 8, Some(MARKER)),
    ("edited.rs", "middle", 7, None),
    ("edited.rs", "last", 5, None),
    ("other.rs", "other", 9, Some(SHARED)),
    ("caller.rs", "incoming", 10, None),
];

fn compare(current: &NormalizedGraph, cold: &NormalizedGraph) {
    assert_eq!(current.records(), cold.records());
    assert_eq!(current.project_roots, cold.project_roots);
    assert_eq!(
        current.records_by_contribution,
        cold.records_by_contribution
    );
    assert_eq!(current.candidates(), cold.candidates());
    assert_eq!(current.references(), cold.references());
    assert_eq!(current.edges(), cold.edges());
}

#[test]
fn projects_current_order_and_resolves_incoming_alias_on_insert_and_restore() {
    let baseline = publication(1, ORIGINAL, None);
    let held = Normalizer::normalize_project(revisions(1), &baseline).unwrap();
    assert!(matches!(
        held.edges()[0].target(),
        super::super::NormalizedTarget::Contribution(_)
    ));
    let next = publication(2, WITH_MARKER, None);
    let insert = ModifiedContributionRun {
        unit: unit("edited.rs"),
        previous: 0..3,
        current: 0..4,
    };
    let current = Normalizer::project_records_after_edit(revisions(2), &next, &held, &[insert])
        .unwrap()
        .expect("isolated insertion");
    compare(
        &current,
        &Normalizer::normalize_project(revisions(2), &next).unwrap(),
    );
    assert!(matches!(
        current.edges()[0].target(),
        super::super::NormalizedTarget::Symbol(id) if id.as_str() == MARKER
    ));

    let restored = publication(3, ORIGINAL, None);
    let remove = ModifiedContributionRun {
        unit: unit("edited.rs"),
        previous: 0..4,
        current: 0..3,
    };
    let restored =
        Normalizer::project_records_after_edit(revisions(3), &restored, &current, &[remove])
            .unwrap()
            .expect("isolated removal");
    compare(
        &restored,
        &Normalizer::normalize_project(revisions(3), restored.publications()).unwrap(),
    );
    assert_eq!(held.source_revision(), revisions(1).1);
    assert!(matches!(
        held.edges()[0].target(),
        super::super::NormalizedTarget::Contribution(_)
    ));
}

#[test]
fn refuses_anchor_and_binding_changes_order_and_extra_association_evidence() {
    let baseline = publication(1, ORIGINAL, None);
    let held = Normalizer::normalize_project(revisions(1), &baseline).unwrap();
    let check = |rows: &[(&str, &str, u64, Option<&str>)], claim| {
        let current = publication(2, rows, claim);
        let edit = ModifiedContributionRun {
            unit: unit("edited.rs"),
            previous: 0..3,
            current: 0..4,
        };
        assert!(
            Normalizer::project_records_after_edit(revisions(2), &current, &held, &[edit])
                .unwrap()
                .is_none(),
            "{rows:?} {claim:?}"
        );
    };
    let mut rows = WITH_MARKER.to_vec();
    rows[1].3 = Some(SHARED);
    check(&rows, None);
    rows[1].3 = Some(MARKER);
    rows[1].2 = rows[0].2;
    check(&rows, None);
    rows[1].2 = WITH_MARKER[1].2;
    rows[3].2 = rows[2].2;
    check(&rows, None);
    rows[3].2 = WITH_MARKER[3].2;
    rows.swap(0, 2);
    check(&rows, None);
    check(WITH_MARKER, Some("other"));
    check(WITH_MARKER, Some("marker"));
    rows = WITH_MARKER.to_vec();
    rows[4].0 = "changed-unit.rs";
    check(&rows, None);
}

#[test]
fn refuses_unproven_run_and_declines_the_selected_row_bound() {
    let baseline = publication(1, ORIGINAL, None);
    let held = Normalizer::normalize_project(revisions(1), &baseline).unwrap();
    let current = publication(2, WITH_MARKER, None);
    let split = ModifiedContributionRun {
        unit: unit("edited.rs"),
        previous: 1..3,
        current: 1..4,
    };
    assert!(
        Normalizer::project_records_after_edit(revisions(2), &current, &held, &[split])
            .unwrap()
            .is_none()
    );
    let foreign = ModifiedContributionRun {
        unit: unit("other.rs"),
        previous: 0..3,
        current: 0..4,
    };
    assert!(
        Normalizer::project_records_after_edit(revisions(2), &current, &held, &[foreign])
            .unwrap()
            .is_none()
    );

    let names = (0..4_096)
        .map(|index| format!("entry{index}"))
        .collect::<Vec<_>>();
    let rows = names
        .iter()
        .map(|name| ("edited.rs", name.as_str(), 0, None))
        .collect::<Vec<_>>();
    let baseline = publication(1, &rows, None);
    let held = Normalizer::normalize_project(revisions(1), &baseline).unwrap();
    let mut added = rows.clone();
    added.push(("edited.rs", "marker", 8, Some(MARKER)));
    let current = publication(2, &added, None);
    let edit = ModifiedContributionRun {
        unit: unit("edited.rs"),
        previous: 0..4_096,
        current: 0..4_097,
    };
    assert!(
        Normalizer::project_records_after_edit(revisions(2), &current, &held, &[edit])
            .unwrap()
            .is_none()
    );
}
