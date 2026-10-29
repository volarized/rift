use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ContributionReference, ExactKind,
    PortableSymbolFacts, ProjectPath, ProviderId, ProviderRevision, ProviderSymbolId,
    SourceApplicability, SourceKind, SourceLocation, SourceRange, SourceRevision, SourceUnitId,
    SymbolId, TreeRevision, declaration_address, encode_path, symbol_identity,
};
use rift_error::RiftError;
use rift_provider::{ProviderPublication, PublicationLimits};

use crate::{SyntaxDocument, SyntaxFacts};

/// Stable identity of built-in syntax Contribution provider.
pub const SYNTAX_PROVIDER_ID: &str = "syntax";

/// One established export alias and its defining provider identity.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedAlias {
    /// Exported name.
    pub name: String,
    /// Provider-local qualified exported name.
    pub qualified_name: String,
    /// Established logical alias identity.
    pub identity: SymbolId,
    /// Defining declaration identity supplied to normalization.
    pub target_identity: String,
    /// Original export statement range.
    pub range: crate::ByteRange,
}

/// Logical identity and language supplied by an established declaration mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalDeclaration {
    /// Established target identity.
    pub identity: SymbolId,
    /// Language of that logical target.
    pub language: rift_protocol::read::Language,
}

/// Where one document's declarations are filed: origin, source unit, and identity path.
///
/// `unit` is the [`SourceUnitId`] every declaration's binding names, and
/// `identity_path` is the path segment each [`SymbolId`] embeds after the
/// language. The project placement files a document under
/// `rift://source/project/<path>` with the path itself as the identity path;
/// a dependency placement names the package's own unit and embeds
/// `<manager>/<name>@<version>/<path>` instead.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentPlacement {
    origin: ContributionOrigin,
    unit: SourceUnitId,
    identity_path: String,
    identity_anchors: BTreeMap<String, SymbolId>,
    aliases: Vec<PlacedAlias>,
    logical_declarations: BTreeMap<String, LogicalDeclaration>,
}

impl DocumentPlacement {
    /// Files declarations under `unit`, embedding `identity_path` in every symbol identity.
    #[must_use]
    pub fn new(
        origin: ContributionOrigin,
        unit: SourceUnitId,
        identity_path: impl Into<String>,
    ) -> Self {
        Self {
            origin,
            unit,
            identity_path: identity_path.into(),
            identity_anchors: BTreeMap::new(),
            aliases: Vec::new(),
            logical_declarations: BTreeMap::new(),
        }
    }

    /// The project placement: `rift://source/project/<path>` with the path itself.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the document's path breaks
    /// source-unit rules.
    pub fn project(document: &SyntaxDocument) -> Result<Self, RiftError> {
        Self::project_path(document.path())
    }

    /// The project placement for one project-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the path breaks source-unit rules.
    pub fn project_path(path: &ProjectPath) -> Result<Self, RiftError> {
        let location = SourceLocation::Project { package: None };
        let origin = ContributionOrigin::new(Some(location), SourceKind::Authored)?;
        Ok(Self::new(
            origin,
            source_unit_for_path(path)?,
            path.as_str(),
        ))
    }

    /// Where the declarations came from.
    #[must_use]
    pub const fn origin(&self) -> &ContributionOrigin {
        &self.origin
    }

    /// The source unit every declaration's binding names.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// The path segment every symbol identity embeds after the language.
    #[must_use]
    pub fn identity_path(&self) -> &str {
        &self.identity_path
    }

    /// Supplies established logical identities separately from physical provider keys.
    ///
    /// A declaration the map does not name stays an unresolved fact: a physical provider
    /// key never becomes a public identity.
    #[must_use]
    pub fn with_identity_anchors(mut self, anchors: BTreeMap<String, SymbolId>) -> Self {
        self.identity_anchors = anchors;
        self
    }

    /// Returns the current established identity for one qualified name, which every
    /// declaration spelling it shares.
    #[must_use]
    pub fn logical_identity(&self, qualified_name: &str) -> Option<&SymbolId> {
        self.logical_declarations
            .get(qualified_name)
            .map(|declaration| &declaration.identity)
            .or_else(|| self.identity_anchors.get(qualified_name))
    }

    /// Supplies source-backed export aliases beside existing declarations.
    #[must_use]
    pub fn with_aliases(mut self, aliases: Vec<PlacedAlias>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Number of additional export alias declarations.
    #[must_use]
    pub fn aliases_count(&self) -> usize {
        self.aliases.len()
    }

    /// Supplies established logical mappings while preserving physical source bindings.
    #[must_use]
    pub fn with_logical_declarations(
        mut self,
        declarations: BTreeMap<String, LogicalDeclaration>,
    ) -> Self {
        self.logical_declarations = declarations;
        self
    }
}

/// Collects syntax documents into one atomic provider publication.
#[derive(Debug)]
pub struct SyntaxPublicationBuilder {
    provider: ProviderId,
    publication: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    limits: PublicationLimits,
    contributions: Vec<Contribution>,
}

impl SyntaxPublicationBuilder {
    /// Starts one syntax provider publication.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when built-in provider identity is
    /// invalid.
    pub fn new(
        publication: ProviderRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        limits: PublicationLimits,
    ) -> Result<Self, RiftError> {
        Ok(Self {
            provider: ProviderId::new(SYNTAX_PROVIDER_ID)?,
            publication,
            source_revision,
            tree_revision,
            limits,
            contributions: Vec::new(),
        })
    }

    /// Declarations this publication still has room for under its bound.
    ///
    /// A caller offering a document with more declarations than this would cross the
    /// per-provider bound [`ProviderPublication`] refuses at, so it can leave that
    /// document out before the publication refuses the whole set.
    #[must_use]
    pub fn declarations_remaining(&self) -> usize {
        self.limits
            .contributions_per_provider_max()
            .saturating_sub(self.contributions.len())
    }

    /// Adds every declaration from one project-tree syntax document.
    ///
    /// The project placement is [`DocumentPlacement::project`]; the document
    /// either contributes every declaration or changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when source identity or one
    /// Contribution is invalid.
    pub fn add_document(&mut self, document: &SyntaxDocument) -> Result<(), RiftError> {
        let placement = DocumentPlacement::project(document)?;
        self.add_document_placed(document, &placement)
    }

    /// Adds every declaration from one syntax document under `placement`.
    ///
    /// Each declaration's binding names the placement's unit, its identity
    /// embeds the placement's identity path, and its origin is the placement's.
    /// Document either contributes every declaration or changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when one symbol identity or one
    /// Contribution is invalid.
    pub fn add_document_placed(
        &mut self,
        document: &SyntaxDocument,
        placement: &DocumentPlacement,
    ) -> Result<(), RiftError> {
        self.add_facts_placed(document.facts(), placement)
    }

    /// Adds every declaration from path-independent syntax facts under `placement`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when one symbol identity or Contribution is invalid.
    pub fn add_facts_placed(
        &mut self,
        syntax: &SyntaxFacts,
        placement: &DocumentPlacement,
    ) -> Result<(), RiftError> {
        let language_segment = syntax.language().identity_segment();
        let mut additions = Vec::with_capacity(syntax.symbols().len());
        let mut latest: HashMap<&str, &crate::SyntaxSymbol> = HashMap::new();
        for symbol in syntax.symbols() {
            let logical = placement.logical_declarations.get(&symbol.qualified_name);
            let address = symbol.provider_address(&language_segment, placement.identity_path());
            let provider_symbol = ProviderSymbolId::for_symbol(&address)?;
            let anchor = logical.map_or_else(
                || {
                    placement
                        .identity_anchors
                        .get(&symbol.qualified_name)
                        .cloned()
                },
                |logical| Some(logical.identity.clone()),
            );
            let language = logical.map_or_else(|| syntax.language(), |logical| &logical.language);
            let mut facts = symbol.facts(language);
            if let Some(container) = &symbol.container {
                let ordinal = enclosing_ordinal(latest.get(container.as_str()).copied(), symbol);
                let container_address = declaration_address(
                    &language_segment,
                    placement.identity_path(),
                    container,
                    ordinal,
                );
                facts = facts.container(ContributionReference::new(
                    self.provider.clone(),
                    ProviderSymbolId::for_symbol(&container_address)?,
                ));
            }
            latest.insert(symbol.qualified_name.as_str(), symbol);
            let source = rift_core::DeclarationBinding::new(
                placement.unit().clone(),
                SourceRange::new(symbol.item_range.start, symbol.item_range.end)?,
                None,
            );
            let mut contribution = Contribution::builder(
                ContributionKey::new(self.provider.clone(), self.publication, provider_symbol),
                SourceApplicability::Exact {
                    source_revision: self.source_revision,
                    tree_revision: self.tree_revision,
                },
                facts,
                placement.origin().clone(),
            )
            .source(source);
            if let Some(anchor) = anchor {
                contribution = contribution.identity_anchor(anchor);
            }
            additions.push(contribution.build()?);
        }
        self.add_alias_contributions(syntax, placement, &mut additions)?;
        self.contributions.extend(additions);
        Ok(())
    }

    fn add_alias_contributions(
        &self,
        syntax: &SyntaxFacts,
        placement: &DocumentPlacement,
        additions: &mut Vec<Contribution>,
    ) -> Result<(), RiftError> {
        let language_segment = syntax.language().identity_segment();
        for alias in &placement.aliases {
            let identity = symbol_identity(
                &language_segment,
                placement.identity_path(),
                &alias.qualified_name,
            );
            let provider_symbol = ProviderSymbolId::for_symbol(&identity)?;
            let target = ContributionReference::new(
                self.provider.clone(),
                ProviderSymbolId::for_symbol(&alias.target_identity)?,
            );
            let source = rift_core::DeclarationBinding::new(
                placement.unit().clone(),
                SourceRange::new(alias.range.start, alias.range.end)?,
                None,
            );
            let facts = PortableSymbolFacts::new(
                syntax.language().clone(),
                alias.name.clone(),
                alias.qualified_name.clone(),
                ExactKind("export".to_owned()),
            )
            .facets(vec![
                rift_protocol::read::SymbolFacet::Alias,
                rift_protocol::read::SymbolFacet::Public,
            ])
            .visibility("public");
            let relationship = rift_core::ContributionRelationship::new(
                rift_core::RelationshipKind::Alias,
                target,
            )
            .with_derivation(rift_protocol::read::RelationshipDerivation::Syntax)
            .with_occurrence(source.clone());
            additions.push(
                Contribution::builder(
                    ContributionKey::new(self.provider.clone(), self.publication, provider_symbol),
                    SourceApplicability::Exact {
                        source_revision: self.source_revision,
                        tree_revision: self.tree_revision,
                    },
                    facts,
                    placement.origin().clone(),
                )
                .source(source)
                .identity_anchor(alias.identity.clone())
                .relationships(vec![relationship])
                .build()?,
            );
        }
        Ok(())
    }

    /// Validates and returns complete syntax provider publication.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when publication bounds or keys are
    /// invalid.
    pub fn build(self) -> Result<ProviderPublication, RiftError> {
        ProviderPublication::new(
            self.provider,
            self.publication,
            self.contributions,
            self.limits,
        )
    }
}

/// The ordinal of the container declaration `latest` names for `symbol`: the latest
/// earlier declaration spelling the container's name, when it encloses `symbol`.
///
/// A qualified name never nests inside a declaration of itself, so no earlier declaration
/// of the name can enclose `symbol` when the latest one does not. A container no earlier
/// declaration encloses keeps ordinal 0, the address its name alone spells.
fn enclosing_ordinal(latest: Option<&crate::SyntaxSymbol>, symbol: &crate::SyntaxSymbol) -> usize {
    latest
        .filter(|container| container.range.encloses(symbol.range))
        .map_or(0, |container| container.declaration_ordinal)
}

/// Mints one syntax document's project source-unit identity.
///
/// The spelling is `rift://source/project/<escaped path>`, the identity every
/// project-tree Contribution carries, so a consumer joining another provider's
/// facts to this document addresses the same unit.
///
/// # Errors
///
/// Returns [`RiftError`] when the document's path breaks source-unit rules.
pub fn source_unit(document: &SyntaxDocument) -> Result<SourceUnitId, RiftError> {
    source_unit_for_path(document.path())
}

/// Mints one project source-unit identity from its canonical project path.
///
/// # Errors
///
/// Returns [`RiftError`] when the path breaks source-unit rules.
pub fn source_unit_for_path(path: &ProjectPath) -> Result<SourceUnitId, RiftError> {
    SourceUnitId::parse(&format!(
        "rift://source/project/{}",
        encode_path(path.as_str())
    ))
}

impl crate::SyntaxSymbol {
    /// The provider-local address of this declaration under `language_segment` and
    /// `path`: its qualified name and declaration ordinal, as [`declaration_address`]
    /// spells them.
    #[must_use]
    pub fn provider_address(&self, language_segment: &str, path: &str) -> String {
        declaration_address(
            language_segment,
            path,
            &self.qualified_name,
            self.declaration_ordinal,
        )
    }

    /// Copies retained declaration facts without admitting a portable identity.
    #[must_use]
    pub fn facts(&self, language: &rift_protocol::read::Language) -> PortableSymbolFacts {
        let mut facts = PortableSymbolFacts::new(
            language.clone(),
            self.name.clone(),
            self.qualified_name.clone(),
            ExactKind(self.kind.to_owned()),
        )
        .facets(self.facets.clone())
        .with_shared_signatures(Arc::clone(&self.signatures))
        .with_shared_documentation(Arc::clone(&self.documentation));
        if let Some(visibility) = &self.visibility {
            facts = facts.visibility(visibility.clone());
        }
        facts
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::SyntaxLimits;
    use rift_core::{ProjectPath, ProviderRevision, SourceRevision, TreeRevision};

    use super::SyntaxPublicationBuilder;
    use crate::{RustSyntaxProvider, SyntaxProvider, SyntaxSource};

    fn publication(value: u64) -> ProviderRevision {
        ProviderRevision::new(value).expect("publication")
    }

    fn source_revision(value: u64) -> SourceRevision {
        SourceRevision::new(value).expect("source revision")
    }

    fn tree_revision(value: u64) -> TreeRevision {
        TreeRevision::new(value).expect("tree revision")
    }

    #[test]
    fn syntax_documents_publish_portable_facts_and_container_reference() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "pub struct Beacon; impl Beacon { pub fn run() {} }",
                },
                SyntaxLimits::default(),
            )
            .expect("syntax document");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::default(),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("publication");
        let beacon = publication
            .contributions()
            .iter()
            .find(|contribution| {
                contribution
                    .facts()
                    .is_some_and(|facts| facts.name() == "Beacon")
            })
            .expect("Beacon");
        let run = publication
            .contributions()
            .iter()
            .find(|contribution| {
                contribution
                    .facts()
                    .is_some_and(|facts| facts.name() == "run")
            })
            .expect("run");
        assert_eq!(beacon.identity_anchor(), None);
        assert_eq!(
            beacon.key().reference().symbol().as_str(),
            "rift://symbol/rust/src/lib.rs/Beacon"
        );
        assert_eq!(
            run.facts()
                .expect("portable facts")
                .container_reference()
                .map(|reference| reference.symbol().as_str()),
            Some("rift://symbol/rust/src/lib.rs/Beacon")
        );
        assert_eq!(
            run.facts().expect("portable facts").visibility_spelling(),
            Some("pub")
        );
    }

    /// Repeated declarations of one module publish under distinct provider keys, and each
    /// member's container reference names the declaration enclosing it.
    #[test]
    fn repeated_declarations_publish_distinct_keys_and_enclosing_containers() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "#[cfg(unix)]\nmod sys { pub fn open() {} }\n#[cfg(windows)]\nmod sys { pub fn open() {} }\n",
                },
                SyntaxLimits::default(),
            )
            .expect("syntax document");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::default(),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("distinct keys publish");
        let keys = publication
            .contributions()
            .iter()
            .map(|contribution| {
                let facts = contribution.facts().expect("portable facts");
                (
                    contribution.key().reference().symbol().as_str().to_owned(),
                    facts
                        .container_reference()
                        .map(|reference| reference.symbol().as_str().to_owned()),
                )
            })
            .collect::<Vec<_>>();
        let address = |name: &str| format!("rift://symbol/rust/src/lib.rs/{name}");
        assert_eq!(
            keys,
            [
                (address("sys"), None),
                (address("sys::open"), Some(address("sys"))),
                (address("sys#1"), None),
                (address("sys::open#1"), Some(address("sys#1"))),
            ]
        );
    }

    #[test]
    fn publication_bound_accepts_exact_contribution_count() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "pub struct Beacon;",
                },
                SyntaxLimits::default(),
            )
            .expect("syntax document");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::new(1, 1, 1).expect("limits"),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("publication");
        assert_eq!(publication.contributions().len(), 1);
    }

    #[test]
    fn contribution_shares_signature_and_documentation_storage_with_syntax_facts() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "/// Beacon docs.\npub fn beacon(value: u8) -> u8 { value }\n",
                },
                SyntaxLimits::default(),
            )
            .expect("syntax document");
        let symbol = document
            .symbols()
            .first()
            .expect("syntax document has one symbol");
        assert!(!symbol.signatures.is_empty(), "fixture has signature");
        assert!(
            !symbol.documentation.is_empty(),
            "fixture has documentation"
        );

        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::new(1, 1, 1).expect("limits"),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("publication");
        let facts = publication.contributions()[0]
            .facts()
            .expect("portable facts");

        assert_eq!(facts.signatures_slice(), symbol.signatures.as_ref());
        assert_eq!(
            facts.signatures_slice().as_ptr(),
            symbol.signatures.as_ptr()
        );
        assert_eq!(facts.documentation_blocks(), symbol.documentation.as_ref());
        assert_eq!(
            facts.documentation_blocks().as_ptr(),
            symbol.documentation.as_ptr()
        );
    }

    /// A dependency placement files the unit and identity under the package,
    /// and the container reference embeds the same identity path.
    #[test]
    fn test_add_document_placed_files_declarations_under_the_supplied_unit_and_path() {
        use rift_core::{ContributionOrigin, SourceKind, SourceLocation, SourceUnitId};
        use rift_protocol::read::PackageIdentity;

        use super::DocumentPlacement;

        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "pub fn spawn() {}\npub struct Runtime; impl Runtime { pub fn new() {} }",
                },
                SyntaxLimits::default(),
            )
            .expect("syntax document");
        let package = PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "registry.example".to_owned(),
            name: "tokio".to_owned(),
            version: "1.53.1".to_owned(),
        };
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin");
        let unit = SourceUnitId::for_package(&package, &path).expect("unit");
        let placement = DocumentPlacement::new(origin, unit, "cargo/tokio@1.53.1/src/lib.rs");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::default(),
        )
        .expect("builder");
        builder
            .add_document_placed(&document, &placement)
            .expect("document");
        let publication = builder.build().expect("publication");
        let named = |name: &str| {
            publication
                .contributions()
                .iter()
                .find(|contribution| {
                    contribution
                        .facts()
                        .is_some_and(|facts| facts.name() == name)
                })
                .unwrap_or_else(|| panic!("publication holds {name}"))
        };
        let spawn = named("spawn");
        assert_eq!(spawn.identity_anchor(), None);
        assert_eq!(
            spawn.key().reference().symbol().as_str(),
            "rift://symbol/rust/cargo/tokio@1.53.1/src/lib.rs/spawn"
        );
        assert_eq!(
            spawn.source().map(|binding| binding.unit().to_string()),
            Some("rift://source/cargo/registry.example/tokio@1.53.1/src/lib.rs".to_owned())
        );
        assert_eq!(
            spawn.origin().location(),
            Some(&SourceLocation::Dependency { package })
        );
        assert_eq!(
            named("new")
                .facts()
                .expect("portable facts")
                .container_reference()
                .map(|reference| reference.symbol().as_str()),
            Some("rift://symbol/rust/cargo/tokio@1.53.1/src/lib.rs/Runtime")
        );
    }
}
