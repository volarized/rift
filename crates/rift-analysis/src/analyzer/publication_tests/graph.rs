//! Captured graph projection, source occurrences, and independent coverage.

use super::*;
use rift_core::{
    Contribution, ContributionKey, ContributionReference, ContributionRelationship,
    DeclarationBinding, IndexRevision, PortableSymbolFacts, ProviderId, ProviderRevision,
    ProviderSymbolId, SourceApplicability, SourceRange, SourceRevision, TreeRevision,
};
use rift_protocol::read::{ExactKind, RelationshipDerivation, RelationshipFacet};
use rift_provider::{Normalizer, ProviderPublication, PublicationLimits, PublicationSet};

const SOURCE: &str = "// é\npub fn start() { finish(); finish(); }\npub fn finish() {}\n";

struct GraphFixture {
    analysis: PackageAnalysis,
    owner: SymbolOwner,
    origin: ContributionOrigin,
    language: Language,
    anchor_language: Option<Language>,
}

impl GraphFixture {
    fn new() -> Self {
        let package = fixture::identity();
        Self {
            analysis: fixture::package_result(
                rift_syntax::ShippedLanguage::Rust,
                vec![("src/lib.rs", SOURCE)],
                None,
            )
            .expect("captured physical source"),
            owner: package.owner().expect("canonical package owner"),
            origin: fixture::origin(&package),
            language: Language::from_identity_segment("rust").expect("language"),
            anchor_language: None,
        }
    }

    fn identity(&self, name: &str) -> rift_core::SymbolId {
        let identity = rift_protocol::identity::SymbolIdentity::new(
            self.owner.clone(),
            self.anchor_language
                .as_ref()
                .unwrap_or(&self.language)
                .clone(),
            vec!["beacon".to_owned(), name.to_owned()],
        )
        .expect("current logical identity");
        rift_core::SymbolId::new(identity.wire_identity()).expect("symbol address")
    }

    fn reference(&self, name: &str) -> ContributionReference {
        ContributionReference::new(
            Self::provider(),
            ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
        )
    }

    fn provider() -> ProviderId {
        ProviderId::new("publication.fixture").expect("provider")
    }

    fn contribution(
        &self,
        name: &str,
        relationships: Vec<ContributionRelationship>,
    ) -> Contribution {
        Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity(name).as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            PortableSymbolFacts::new(
                self.language.clone(),
                name,
                format!("beacon::{name}"),
                ExactKind::try_from("function".to_owned()).expect("kind"),
            ),
            self.origin.clone(),
        )
        .identity_anchor(self.identity(name))
        .relationships(relationships)
        .build()
        .expect("source-less logical contribution")
    }

    fn graph(&self, relationships: Vec<ContributionRelationship>) -> WorkspaceSemantics {
        Self::graph_with_contributions(vec![
            self.contribution("start", relationships),
            self.contribution("finish", Vec::new()),
        ])
    }

    fn graph_with_contributions(contributions: Vec<Contribution>) -> WorkspaceSemantics {
        let limits = PublicationLimits::new(1, 2, 2).expect("graph bounds");
        let publication = ProviderPublication::new(
            Self::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            contributions,
            limits,
        )
        .expect("captured contributions");
        let publications = std::sync::Arc::new(
            PublicationSet::empty(limits)
                .replaced(publication)
                .expect("bounded publication set"),
        );
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            &publications,
            None,
        )
        .expect("normalized graph");
        WorkspaceSemantics::from_graph(graph, Self::provider(), 16)
    }

    fn named_contribution(
        &self,
        name: &str,
        relationships: Vec<ContributionRelationship>,
        established: bool,
    ) -> Result<Contribution, RiftError> {
        use rift_protocol::read::{Documentation, DocumentationFormat, Extensions, Signature};

        let signatures = ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
            .into_iter()
            .map(|display| Signature {
                display: display.to_owned(),
                links: Vec::new(),
                language: self.language.clone(),
                receiver: None,
                parameters: Vec::new(),
                returns: Vec::new(),
                type_parameters: Vec::new(),
                throws: Vec::new(),
                effects: Vec::new(),
                extensions: Extensions::default(),
            })
            .collect();
        let facts = PortableSymbolFacts::new(
            self.language.clone(),
            name,
            "beacon::wide",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        )
        .signatures(signatures)
        .documentation(vec![Documentation {
            format: DocumentationFormat::Markdown,
            text: "Both callable forms remain in the captured object.".to_owned(),
        }]);
        let mut builder = Contribution::builder(
            ContributionKey::new(
                Self::provider(),
                ProviderRevision::new(1).expect("provider revision"),
                ProviderSymbolId::for_symbol(self.identity("wide").as_str()).expect("provider key"),
            ),
            SourceApplicability::Independent,
            facts,
            self.origin.clone(),
        )
        .relationships(relationships);
        if established {
            builder = builder.identity_anchor(self.identity("wide"));
        }
        builder.build()
    }

    fn occurrence(&self, start: usize, end: usize) -> DeclarationBinding {
        DeclarationBinding::new(
            self.analysis.files()[0].placement.unit().clone(),
            SourceRange::new(
                u64::try_from(start).expect("source start"),
                u64::try_from(end).expect("source end"),
            )
            .expect("nonempty source range"),
            None,
        )
    }

    fn edge(&self, occurrence: DeclarationBinding) -> ContributionRelationship {
        ContributionRelationship::new(
            rift_core::RelationshipKind::Reference,
            self.reference("finish"),
        )
        .with_derivation(RelationshipDerivation::Syntax)
        .with_occurrence(occurrence)
    }

    fn publication(
        &self,
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
    ) -> Result<PackagePublication, RiftError> {
        let sources = self
            .analysis
            .files()
            .iter()
            .map(|held| {
                crate::PackageSource::new(held.file.path(), held.file.source())
                    .with_source_unit(held.placement.unit(), held.placement.origin())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let input =
            ExactPackageInput::new(&self.owner, &self.language, &self.origin, &sources, limits)?;
        publish(&input, self.analysis.files(), semantics, false).map(|(publication, _)| publication)
    }
}

#[test]
fn artifact_name_bytes_and_public_name_characters_keep_distinct_bounds() {
    let fixture = GraphFixture::new();
    for (name, public) in [
        ("n".repeat(4096), true),
        ("n".repeat(4097), false),
        ("é".repeat(4096), true),
        ("n".repeat(8192), false),
    ] {
        assert!(rift_core::is_portable_name(&name));
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, Vec::new(), true)
                .expect("provider name within byte bound"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("full artifact name retained");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("original normalized object");
        assert_eq!(object.name_is_public(), public);
        assert!(object.id.is_some());
        assert_eq!(object.signatures.len(), 2);
        assert_eq!(
            publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, field, bound, .. }
                if field == "name" && *bound == 4096)),
            !public
        );
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| coverage.identity_complete == public)
        );
        let schema: serde_json::Value =
            serde_json::from_str(&rift_protocol::schema::package_index_schema_document())
                .expect("artifact schema");
        let validator = jsonschema::validator_for(&schema).expect("artifact schema compiles");
        assert!(validator.is_valid(&serde_json::to_value(&publication).expect("artifact")));
    }
    for name in ["n".repeat(8193), "é".repeat(4097)] {
        assert!(!rift_core::is_portable_name(&name));
        assert!(fixture.named_contribution(&name, Vec::new(), true).is_err());
    }
}

#[test]
fn source_less_wide_objects_roundtrip_all_facts_and_relationship_occurrences() {
    let fixture = GraphFixture::new();
    let name = "n".repeat(4097);
    for established in [true, false] {
        let edges = if established {
            SOURCE
                .match_indices("finish();")
                .map(|(start, _)| fixture.edge(fixture.occurrence(start, start + "finish()".len())))
                .collect()
        } else {
            Vec::new()
        };
        let semantics = GraphFixture::graph_with_contributions(vec![
            fixture
                .named_contribution(&name, edges, established)
                .expect("full source-less facts"),
            fixture.contribution("finish", Vec::new()),
        ]);
        let publication = fixture
            .publication(&semantics, ExactPackageLimits::new(1, 1024))
            .expect("source-less artifact projection");
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("full source-less object");
        assert_eq!(object.id.is_some(), established);
        assert!(!object.name_is_public());
        assert_eq!(
            object
                .signatures
                .iter()
                .map(|signature| signature.display.as_str())
                .collect::<Vec<_>>(),
            ["fn wide(value: u8) -> u8", "fn wide(value: u16) -> u16"]
        );
        assert_eq!(
            object.documentation[0].text,
            "Both callable forms remain in the captured object."
        );
        assert!(publication.declarations.is_empty());
        assert!(
            !publication
                .documents
                .iter()
                .any(|document| document.name == name)
        );
        assert_eq!(publication.units[0].source, SOURCE);
        assert!(publication.units[0].source_complete);
        assert!(
            publication
                .coverage
                .iter()
                .all(|coverage| !coverage.identity_complete)
        );
        assert!(publication.warnings.iter().any(|warning| matches!(warning,
            PackageAnalysisWarning::ObjectUnavailable { unit: None, range: None, qualified_name, field, bound }
                if qualified_name == "beacon::wide" && field == "name" && *bound == 4096)));
        if !established {
            assert!(publication.warnings.iter().any(|warning| matches!(warning,
                PackageAnalysisWarning::IdentityUnresolved { unit: None, range: None, qualified_name }
                    if qualified_name == "beacon::wide")));
        }
        let wire = serde_json::to_value(&publication).expect("full artifact serialization");
        let restored: PackagePublication =
            serde_json::from_value(wire).expect("full artifact restore");
        assert_eq!(restored, publication);
        assert_eq!(
            semantics.graph().relationships().len(),
            if established { 2 } else { 0 }
        );
        if established {
            assert_eq!(restored.relationships.len(), 2);
            let starts = restored
                .relationships
                .iter()
                .map(|edge| {
                    let occurrence = edge.occurrence.as_ref().expect("actual source occurrence");
                    assert_eq!(occurrence.unit, restored.units[0].unit);
                    let start = usize::try_from(occurrence.range.start).expect("source start");
                    let end = usize::try_from(occurrence.range.end).expect("source end");
                    assert_eq!(&SOURCE[start..end], "finish()");
                    start
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(starts.len(), 2);
        }
    }
}

#[test]
fn public_name_omission_retains_original_source_binding_and_artifact_object() {
    let name = "n".repeat(4097);
    let source = format!("pub fn {name}() {{}}\n");
    let analysis = fixture::package_result(
        rift_syntax::ShippedLanguage::Rust,
        vec![("src/lib.rs", &source)],
        None,
    )
    .expect("captured Cargo root and source");
    let publication = analysis.publication();
    let object = publication
        .objects
        .iter()
        .find(|object| object.name == name)
        .expect("full artifact object");
    let id = object.id.as_ref().expect("established canonical identity");
    assert!(!object.name_is_public());
    let bindings = publication
        .declarations
        .iter()
        .filter(|binding| &binding.symbol == id)
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 1);
    let binding = bindings[0];
    assert!(binding.public);
    assert!(binding.source_complete);
    let start = usize::try_from(binding.range.start).expect("original start");
    let end = usize::try_from(binding.range.end).expect("original end");
    assert_eq!(binding.source, source[start..end]);
    assert_eq!(publication.units[0].source, source);
    assert!(
        !publication
            .documents
            .iter()
            .any(|document| document.kind == PackageDocumentKind::Symbol)
    );
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::ObjectUnavailable { unit: Some(unit), range: Some(range), qualified_name, field, bound }
            if unit == &binding.unit && range == &binding.range && qualified_name == &name
                && field == "name" && *bound == 4096)));
}

#[test]
fn source_less_objects_keep_logical_runtime_origin() {
    let mut fixture = GraphFixture::new();
    let runtime = rift_protocol::read::RuntimeIdentity {
        runtime: "rust".to_owned(),
        version: "1.98.1".to_owned(),
    };
    fixture.owner = runtime.owner().expect("logical runtime owner");
    fixture.origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime.clone()),
        }),
        rift_core::SourceKind::Authored,
    )
    .expect("runtime origin");
    let semantics = fixture.graph(Vec::new());
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("source-less object projection");
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.declarations.is_empty());
    for object in &publication.objects {
        let identity = rift_protocol::identity::SymbolIdentity::parse(
            object.id.as_ref().expect("established identity").as_str(),
        )
        .expect("canonical runtime identity");
        assert_eq!(identity.owner(), &fixture.owner);
        assert_eq!(object.origin.runtime.as_ref(), Some(&runtime));
        assert!(object.origin.package.is_none());
    }
    assert!(
        publication
            .documents
            .iter()
            .all(|document| { document.kind != rift_protocol::index::PackageDocumentKind::Symbol })
    );
    assert_eq!(semantics.graph().records().len(), 2);
}

#[test]
fn foreign_logical_owner_cannot_receive_publication_origin() {
    let fixture = GraphFixture::new();
    let mut foreign = GraphFixture::new();
    foreign.owner = SymbolOwner::Local;
    let local = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&local, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign owner retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(local.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    let SymbolOwner::Package { registry, .. } = &mut foreign.owner else {
        panic!("fixture requires a package owner");
    };
    *registry = "private.example".to_owned();
    foreign
        .owner
        .validate()
        .expect("canonical foreign registry");
    let private = foreign.graph(Vec::new());
    let publication = fixture
        .publication(&private, ExactPackageLimits::new(1, 1024))
        .expect("unresolved foreign registry retained");
    assert_foreign_facts_retained(&publication);
    assert_eq!(private.graph().records().len(), 2);
    foreign.owner = fixture.owner.clone();
    foreign.anchor_language = Some(Language::from_identity_segment("python").expect("language"));
    let invalid = Contribution::builder(
        ContributionKey::new(
            GraphFixture::provider(),
            ProviderRevision::new(1).expect("provider revision"),
            ProviderSymbolId::for_symbol(foreign.identity("start").as_str()).expect("provider key"),
        ),
        SourceApplicability::Independent,
        PortableSymbolFacts::new(
            foreign.language.clone(),
            "start",
            "beacon::start",
            ExactKind::try_from("function".to_owned()).expect("kind"),
        ),
        foreign.origin.clone(),
    )
    .identity_anchor(foreign.identity("start"))
    .build();
    assert!(invalid.is_err());
}

fn assert_foreign_facts_retained(publication: &PackagePublication) {
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.objects.iter().all(|object| object.id.is_none()));
    assert_eq!(
        publication
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["start", "finish"])
    );
    assert!(publication.declarations.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(
        publication
            .warnings
            .iter()
            .filter(|warning| matches!(
                warning,
                PackageAnalysisWarning::IdentityUnresolved {
                    unit: None,
                    range: None,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        publication
            .warnings
            .iter()
            .all(PackageAnalysisWarning::is_valid)
    );
}

#[test]
fn portable_relationships_keep_two_original_call_sites_without_nodes() {
    let fixture = GraphFixture::new();
    let first = SOURCE.find("finish();").expect("first call");
    let second = SOURCE.rfind("finish();").expect("second call");
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(first, first + "finish()".len())),
        fixture.edge(fixture.occurrence(second, second + "finish()".len())),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_retained_source_bytes(8, 1024)
                .expect("bounded retained excerpts"),
        )
        .expect("portable occurrence projection");
    assert_eq!(publication.relationships.len(), 2);
    assert!(!publication.units[0].source_complete);
    let starts = publication
        .relationships
        .iter()
        .map(|edge| {
            assert_eq!(edge.from.0, fixture.identity("start").as_str());
            assert_eq!(edge.to.0, fixture.identity("finish").as_str());
            assert_eq!(edge.derivation, RelationshipDerivation::Syntax);
            assert_eq!(edge.facets, [RelationshipFacet::References]);
            assert!(edge.evidence.is_empty());
            assert!(edge.confidence.is_none());
            let occurrence = edge.occurrence.as_ref().expect("original call site");
            assert_eq!(occurrence.unit, publication.units[0].unit);
            let start = usize::try_from(occurrence.range.start).expect("source start");
            let end = usize::try_from(occurrence.range.end).expect("source end");
            assert_eq!(&SOURCE[start..end], "finish()");
            start
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(starts, BTreeSet::from([first, second]));
    assert_eq!(semantics.graph().relationships().len(), 2);
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.relationships.is_empty())
    );
}

#[test]
fn absent_derivation_keeps_graph_fact_without_portable_relationship() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![ContributionRelationship::new(
        rift_core::RelationshipKind::Reference,
        fixture.reference("finish"),
    )]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unresolved relationship projection");
    assert_eq!(semantics.graph().relationships().len(), 1);
    assert!(publication.relationships.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| coverage.relationships.is_empty())
    );
}

#[test]
fn invalid_occurrence_refuses_original_byte_range_and_unknown_unit() {
    let fixture = GraphFixture::new();
    for (start, end) in [(0, SOURCE.len() + 1), (4, 5)] {
        let semantics = fixture.graph(vec![fixture.edge(fixture.occurrence(start, end))]);
        assert!(
            fixture
                .publication(&semantics, ExactPackageLimits::new(1, 1024))
                .is_err()
        );
        assert_eq!(semantics.graph().relationships().len(), 1);
    }
    let unit = rift_core::SourceUnitId::for_owner(fixture.owner.clone(), "missing.rs")
        .expect("canonical unit outside selected inventory");
    let occurrence = DeclarationBinding::new(
        unit,
        SourceRange::new(0, 1).expect("nonempty source range"),
        None,
    );
    let semantics = fixture.graph(vec![fixture.edge(occurrence)]);
    let publication = fixture
        .publication(&semantics, ExactPackageLimits::new(1, 1024))
        .expect("unavailable occurrence projection");
    assert!(publication.relationships.is_empty());
    assert!(
        publication
            .coverage
            .iter()
            .all(|coverage| !coverage.identity_complete)
    );
    assert_eq!(publication.units[0].source, SOURCE);
    assert_eq!(semantics.graph().relationships().len(), 1);
}

#[test]
fn relationship_bound_keeps_captured_graph_immutable() {
    let fixture = GraphFixture::new();
    let semantics = fixture.graph(vec![
        fixture.edge(fixture.occurrence(0, 1)),
        fixture.edge(fixture.occurrence(1, 2)),
    ]);
    let publication = fixture
        .publication(
            &semantics,
            ExactPackageLimits::new(1, 1024)
                .with_relationships(1)
                .expect("portable edge bound"),
        )
        .expect("bounded portable relationships");
    assert_eq!(publication.relationships.len(), 1);
    assert_eq!(semantics.graph().relationships().len(), 2);
    assert!(publication.warnings.iter().any(|warning| matches!(warning,
        PackageAnalysisWarning::PublicationTruncated { collection, bound }
            if collection == "relationships" && *bound == 1)));
}
