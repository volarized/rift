//! SCIP snapshot conversion into immutable Contributions.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ContributionReference,
    ContributionRelationship, DeclarationBinding, Documentation, DocumentationFormat, ExactKind,
    ExtensionKey, ExtensionValue, Extensions, Language, PortableSymbolFacts, ProviderId,
    ProviderRevision, ProviderSymbolId, ReferenceRole, RelationshipKind, SemanticReference,
    Signature, SourceApplicability, SourceKind, SourceLocation, SourcePath, SourceRange,
    SourceResolverId, SourceRevision, SourceUnitId, SymbolFacet, TreeRevision,
};
use scip::types::{
    Document, Index, Occurrence, PositionEncoding, Relationship, SymbolInformation, SymbolRole,
    occurrence, symbol_information,
};
use serde_json::{Value, json};

use crate::{
    AdapterPublication, AdapterViolation, ProviderInputMode, ProviderPublication,
    PublicationCoverage, PublicationLimits,
};

/// SCIP conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScipViolation {
    /// SCIP symbol string is empty or invalid for provider identity.
    InvalidSymbol,
    /// Document path cannot identify one source unit.
    InvalidPath,
    /// Occurrence range cannot be converted to source bytes.
    InvalidRange,
    /// SCIP snapshot repeats one provider-local symbol.
    DuplicateSymbol,
    /// Contribution validation failed.
    InvalidContribution,
    /// Provider publication validation failed.
    InvalidPublication,
    /// Snapshot coverage validation failed.
    InvalidCoverage,
}

/// Error returned by SCIP snapshot conversion.
#[derive(Debug)]
pub struct ScipAdapterError {
    violation: ScipViolation,
    detail: String,
}

impl ScipAdapterError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> ScipViolation {
        self.violation
    }

    /// Returns failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ScipAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SCIP adapter rejected {:?}: {}",
            self.violation, self.detail
        )
    }
}

impl Error for ScipAdapterError {}

/// Converts one SCIP index snapshot into one provider publication.
#[derive(Debug, Clone)]
pub struct ScipAdapter {
    provider: ProviderId,
    publication_revision: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    resolver: SourceResolverId,
    limits: PublicationLimits,
}

impl ScipAdapter {
    /// Creates one snapshot adapter.
    #[must_use]
    pub const fn new(
        provider: ProviderId,
        publication_revision: ProviderRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        resolver: SourceResolverId,
        limits: PublicationLimits,
    ) -> Self {
        Self {
            provider,
            publication_revision,
            source_revision,
            tree_revision,
            resolver,
            limits,
        }
    }

    /// Converts one deterministic SCIP snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ScipAdapterError`] when SCIP identity, source coordinates, Contributions,
    /// publication, or coverage are invalid.
    pub fn convert(&self, index: &Index) -> Result<AdapterPublication, ScipAdapterError> {
        let mut units = Vec::with_capacity(index.documents.len());
        let mut drafts = BTreeMap::<ProviderSymbolId, Draft>::new();
        self.collect_information(index, &mut units, &mut drafts)?;
        self.collect_occurrences(index, &mut drafts)?;
        self.finish_publication(units, drafts)
    }

    fn collect_information(
        &self,
        index: &Index,
        units: &mut Vec<SourceUnitId>,
        drafts: &mut BTreeMap<ProviderSymbolId, Draft>,
    ) -> Result<(), ScipAdapterError> {
        for document in &index.documents {
            let unit = self.source_unit(document)?;
            units.push(unit.clone());
            let context = DocumentContext { document, unit };
            for information in &document.symbols {
                self.insert_information(drafts, information, Some(&context))?;
            }
        }
        for information in &index.external_symbols {
            self.insert_information(drafts, information, None)?;
        }
        Ok(())
    }

    fn collect_occurrences(
        &self,
        index: &Index,
        drafts: &mut BTreeMap<ProviderSymbolId, Draft>,
    ) -> Result<(), ScipAdapterError> {
        for document in &index.documents {
            let unit = self.source_unit(document)?;
            for occurrence in &document.occurrences {
                if occurrence.symbol.is_empty() {
                    continue;
                }
                let symbol = provider_symbol(&occurrence.symbol, Some(document))?;
                let draft = match drafts.entry(symbol.clone()) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(self.occurrence_draft(document, occurrence)?)
                    }
                };
                self.record_occurrence(document, &unit, occurrence, symbol, draft)?;
            }
        }
        Ok(())
    }

    fn occurrence_draft(
        &self,
        document: &Document,
        occurrence: &Occurrence,
    ) -> Result<Draft, ScipAdapterError> {
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .map_err(|error| scip_error(ScipViolation::InvalidContribution, error.to_string()))?;
        Ok(Draft {
            language: language(document),
            name: fallback_name(&occurrence.symbol),
            qualified_name: occurrence.symbol.clone(),
            kind: "scip.symbol".to_owned(),
            facets: Vec::new(),
            documentation: Vec::new(),
            signature: None,
            container: None,
            source: None,
            origin,
            applicability: SourceApplicability::Exact {
                source_revision: self.source_revision,
                tree_revision: self.tree_revision,
            },
            relationships: Vec::new(),
            references: Vec::new(),
            occurrences: Vec::new(),
            information: json!({"symbol": occurrence.symbol}),
        })
    }

    fn record_occurrence(
        &self,
        document: &Document,
        unit: &SourceUnitId,
        occurrence: &Occurrence,
        symbol: ProviderSymbolId,
        draft: &mut Draft,
    ) -> Result<(), ScipAdapterError> {
        draft.occurrences.push(occurrence_json(occurrence));
        if document.text.is_empty() {
            return Ok(());
        }
        let range = occurrence_range(document, occurrence)?;
        let binding = DeclarationBinding::new(unit.clone(), range, None);
        let target = ContributionReference::new(self.provider.clone(), symbol);
        draft.references.push(
            SemanticReference::new(binding.clone(), reference_role(occurrence), vec![target])
                .map_err(|error| {
                    scip_error(ScipViolation::InvalidContribution, error.to_string())
                })?,
        );
        if occurrence.symbol_roles & SymbolRole::Definition as i32 != 0 && draft.source.is_none() {
            draft.source = Some(binding);
        }
        Ok(())
    }

    fn finish_publication(
        &self,
        mut units: Vec<SourceUnitId>,
        drafts: BTreeMap<ProviderSymbolId, Draft>,
    ) -> Result<AdapterPublication, ScipAdapterError> {
        let mut contributions = Vec::with_capacity(drafts.len());
        for (symbol, draft) in drafts {
            contributions.push(self.build_contribution(symbol, draft)?);
        }
        let publication = ProviderPublication::new(
            self.provider.clone(),
            self.publication_revision,
            contributions,
            self.limits,
        )
        .map_err(|error| scip_error(ScipViolation::InvalidPublication, error.to_string()))?;
        units.sort();
        units.dedup();
        let coverage = if units.is_empty() {
            PublicationCoverage::Workspace
        } else {
            PublicationCoverage::SourceUnits(units)
        };
        AdapterPublication::new(ProviderInputMode::Snapshot, coverage, publication).map_err(
            |error| {
                let detail = match error.violation() {
                    AdapterViolation::ContributionOutsideCoverage => {
                        "Contribution lies outside SCIP document set".to_owned()
                    }
                    violation => format!("{violation:?}"),
                };
                scip_error(ScipViolation::InvalidCoverage, detail)
            },
        )
    }

    fn source_unit(&self, document: &Document) -> Result<SourceUnitId, ScipAdapterError> {
        let path = SourcePath::new(&document.relative_path)
            .map_err(|error| scip_error(ScipViolation::InvalidPath, error.to_string()))?;
        SourceUnitId::new(self.resolver.clone(), path)
            .map_err(|error| scip_error(ScipViolation::InvalidPath, error.to_string()))
    }

    fn insert_information(
        &self,
        drafts: &mut BTreeMap<ProviderSymbolId, Draft>,
        information: &SymbolInformation,
        context: Option<&DocumentContext<'_>>,
    ) -> Result<(), ScipAdapterError> {
        let document = context.map(|value| value.document);
        let symbol = provider_symbol(&information.symbol, document)?;
        let (language, source, origin, applicability) = if let Some(context) = context {
            (
                language(context.document),
                definition_binding(context.document, &context.unit, &information.symbol)?,
                ContributionOrigin::new(
                    Some(SourceLocation::Project { package: None }),
                    SourceKind::Authored,
                )
                .map_err(|error| {
                    scip_error(ScipViolation::InvalidContribution, error.to_string())
                })?,
                SourceApplicability::Exact {
                    source_revision: self.source_revision,
                    tree_revision: self.tree_revision,
                },
            )
        } else {
            (
                Language {
                    name: "unknown".to_owned(),
                    dialect: None,
                },
                None,
                ContributionOrigin::new(None, SourceKind::Synthetic).map_err(|error| {
                    scip_error(ScipViolation::InvalidContribution, error.to_string())
                })?,
                SourceApplicability::Independent,
            )
        };
        let name = if information.display_name.is_empty() {
            fallback_name(&information.symbol)
        } else {
            information.display_name.clone()
        };
        let kind = information.kind.enum_value().map_or_else(
            |_| "unspecified_kind".to_owned(),
            |kind| snake_case(&format!("{kind:?}")),
        );
        let facets = portable_facets(information.kind.enum_value().ok());
        let documentation = information
            .documentation
            .iter()
            .map(|text| Documentation {
                format: DocumentationFormat::Markdown,
                text: text.clone(),
            })
            .collect();
        let signature = information
            .signature_documentation
            .as_ref()
            .map(|signature| Signature {
                display: signature.text.clone(),
                links: Vec::new(),
                language: Language {
                    name: canonical_language(&signature.language),
                    dialect: None,
                },
                receiver: None,
                parameters: Vec::new(),
                returns: Vec::new(),
                type_parameters: Vec::new(),
                throws: Vec::new(),
                effects: Vec::new(),
                extensions: Extensions(BTreeMap::new()),
            });
        let container = (!information.enclosing_symbol.is_empty())
            .then(|| provider_symbol(&information.enclosing_symbol, document))
            .transpose()?
            .map(|target| ContributionReference::new(self.provider.clone(), target));
        let relationships = relationship_facts(&self.provider, information, document)?;
        let draft = Draft {
            language,
            name,
            qualified_name: information.symbol.clone(),
            kind: format!("scip.{kind}"),
            facets,
            documentation,
            signature,
            container,
            source,
            origin,
            applicability,
            relationships,
            references: Vec::new(),
            occurrences: Vec::new(),
            information: information_json(information),
        };
        if drafts.insert(symbol, draft).is_some() {
            return Err(scip_error(
                ScipViolation::DuplicateSymbol,
                information.symbol.clone(),
            ));
        }
        Ok(())
    }

    fn build_contribution(
        &self,
        symbol: ProviderSymbolId,
        draft: Draft,
    ) -> Result<Contribution, ScipAdapterError> {
        let mut facts = PortableSymbolFacts::new(
            draft.language,
            draft.name,
            draft.qualified_name,
            ExactKind(draft.kind),
        )
        .facets(draft.facets)
        .documentation(draft.documentation);
        if let Some(signature) = draft.signature {
            facts = facts.signatures(vec![signature]);
        }
        if let Some(container) = draft.container {
            facts = facts.container(container);
        }
        let mut extension_map = BTreeMap::new();
        extension_map.insert(
            ExtensionKey("io.scip.symbol".to_owned()),
            ExtensionValue {
                version: 1,
                data: json!({
                    "information": draft.information,
                    "occurrences": draft.occurrences,
                }),
            },
        );
        let mut builder = Contribution::builder(
            ContributionKey::new(self.provider.clone(), self.publication_revision, symbol),
            draft.applicability,
            facts,
            draft.origin,
        )
        .references(draft.references)
        .relationships(draft.relationships)
        .namespaced(Extensions(extension_map));
        if let Some(source) = draft.source {
            builder = builder.source(source);
        }
        builder
            .build()
            .map_err(|error| scip_error(ScipViolation::InvalidContribution, error.to_string()))
    }
}

#[derive(Clone)]
struct DocumentContext<'a> {
    document: &'a Document,
    unit: SourceUnitId,
}

struct Draft {
    language: Language,
    name: String,
    qualified_name: String,
    kind: String,
    facets: Vec<SymbolFacet>,
    documentation: Vec<Documentation>,
    signature: Option<Signature>,
    container: Option<ContributionReference>,
    source: Option<DeclarationBinding>,
    origin: ContributionOrigin,
    applicability: SourceApplicability,
    relationships: Vec<ContributionRelationship>,
    references: Vec<SemanticReference>,
    occurrences: Vec<Value>,
    information: Value,
}

fn relationship_facts(
    provider: &ProviderId,
    information: &SymbolInformation,
    document: Option<&Document>,
) -> Result<Vec<ContributionRelationship>, ScipAdapterError> {
    let mut relationships = Vec::new();
    for relationship in &information.relationships {
        let target = provider_symbol(&relationship.symbol, document)?;
        relationships.extend(relationship_kinds(relationship).map(|kind| {
            ContributionRelationship::new(
                kind,
                ContributionReference::new(provider.clone(), target.clone()),
            )
        }));
    }
    Ok(relationships)
}

fn provider_symbol(
    symbol: &str,
    document: Option<&Document>,
) -> Result<ProviderSymbolId, ScipAdapterError> {
    if symbol.is_empty() {
        return Err(scip_error(ScipViolation::InvalidSymbol, "empty symbol"));
    }
    let value = if scip::symbol::is_local_symbol(symbol) {
        let path = document
            .map(|document| document.relative_path.as_str())
            .ok_or_else(|| {
                scip_error(
                    ScipViolation::InvalidSymbol,
                    "external local symbol has no document",
                )
            })?;
        format!("{path}::{symbol}")
    } else {
        symbol.to_owned()
    };
    ProviderSymbolId::new(value)
        .map_err(|error| scip_error(ScipViolation::InvalidSymbol, error.to_string()))
}

fn language(document: &Document) -> Language {
    Language {
        name: canonical_language(&document.language),
        dialect: None,
    }
}

fn canonical_language(value: &str) -> String {
    let lowered = value.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        "unknown".to_owned()
    } else {
        lowered
    }
}

fn definition_binding(
    document: &Document,
    unit: &SourceUnitId,
    symbol: &str,
) -> Result<Option<DeclarationBinding>, ScipAdapterError> {
    if document.text.is_empty() {
        return Ok(None);
    }
    document
        .occurrences
        .iter()
        .find(|occurrence| {
            occurrence.symbol == symbol
                && occurrence.symbol_roles & SymbolRole::Definition as i32 != 0
        })
        .map(|occurrence| {
            occurrence_range(document, occurrence)
                .map(|range| DeclarationBinding::new(unit.clone(), range, None))
        })
        .transpose()
}

fn occurrence_range(
    document: &Document,
    occurrence: &Occurrence,
) -> Result<SourceRange, ScipAdapterError> {
    let (start_line, start_character, end_line, end_character) =
        raw_occurrence_range(occurrence)
            .ok_or_else(|| scip_error(ScipViolation::InvalidRange, "missing occurrence range"))?;
    let encoding = document
        .position_encoding
        .enum_value()
        .unwrap_or(PositionEncoding::UTF8CodeUnitOffsetFromLineStart);
    let start = position_to_byte(&document.text, start_line, start_character, encoding)
        .ok_or_else(|| scip_error(ScipViolation::InvalidRange, "invalid range start"))?;
    let end = position_to_byte(&document.text, end_line, end_character, encoding)
        .ok_or_else(|| scip_error(ScipViolation::InvalidRange, "invalid range end"))?;
    SourceRange::new(start, end)
        .map_err(|error| scip_error(ScipViolation::InvalidRange, error.to_string()))
}

fn raw_occurrence_range(occurrence: &Occurrence) -> Option<(i32, i32, i32, i32)> {
    match &occurrence.typed_range {
        Some(occurrence::Typed_range::SingleLineRange(range)) => Some((
            range.line,
            range.start_character,
            range.line,
            range.end_character,
        )),
        Some(occurrence::Typed_range::MultiLineRange(range)) => Some((
            range.start_line,
            range.start_character,
            range.end_line,
            range.end_character,
        )),
        Some(_) => None,
        None => match occurrence.range.as_slice() {
            [line, start, end] => Some((*line, *start, *line, *end)),
            [start_line, start, end_line, end] => Some((*start_line, *start, *end_line, *end)),
            _ => None,
        },
    }
}

fn position_to_byte(
    text: &str,
    line: i32,
    character: i32,
    encoding: PositionEncoding,
) -> Option<u64> {
    let line = usize::try_from(line).ok()?;
    let character = usize::try_from(character).ok()?;
    let start = text
        .split_inclusive('\n')
        .take(line)
        .map(str::len)
        .sum::<usize>();
    let line_text = text.get(start..)?.split('\n').next()?;
    let offset = match encoding {
        PositionEncoding::UTF16CodeUnitOffsetFromLineStart => {
            encoded_offset(line_text, character, char::len_utf16)?
        }
        PositionEncoding::UTF32CodeUnitOffsetFromLineStart => {
            encoded_offset(line_text, character, |_| 1)?
        }
        PositionEncoding::UnspecifiedPositionEncoding
        | PositionEncoding::UTF8CodeUnitOffsetFromLineStart => {
            line_text.get(..character)?;
            character
        }
    };
    u64::try_from(start.checked_add(offset)?).ok()
}

fn encoded_offset(text: &str, expected: usize, width: impl Fn(char) -> usize) -> Option<usize> {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        if units == expected {
            return Some(byte);
        }
        units = units.checked_add(width(character))?;
        if units > expected {
            return None;
        }
    }
    (units == expected).then_some(text.len())
}

fn reference_role(occurrence: &Occurrence) -> ReferenceRole {
    let roles = occurrence.symbol_roles;
    if roles & SymbolRole::Definition as i32 != 0 {
        ReferenceRole::Definition
    } else if roles & SymbolRole::Import as i32 != 0 {
        ReferenceRole::Import
    } else if roles & SymbolRole::WriteAccess as i32 != 0 {
        ReferenceRole::Write
    } else if roles & SymbolRole::ReadAccess as i32 != 0 {
        ReferenceRole::Read
    } else {
        ReferenceRole::Unknown
    }
}

fn relationship_kinds(relationship: &Relationship) -> impl Iterator<Item = RelationshipKind> {
    [
        relationship
            .is_reference
            .then_some(RelationshipKind::Reference),
        relationship
            .is_definition
            .then_some(RelationshipKind::Definition),
        relationship
            .is_implementation
            .then_some(RelationshipKind::Implementation),
        relationship
            .is_type_definition
            .then_some(RelationshipKind::TypeDefinition),
    ]
    .into_iter()
    .flatten()
}

fn portable_facets(kind: Option<symbol_information::Kind>) -> Vec<SymbolFacet> {
    use symbol_information::Kind;
    match kind {
        Some(
            Kind::Class
            | Kind::Enum
            | Kind::Interface
            | Kind::Struct
            | Kind::Trait
            | Kind::Type
            | Kind::TypeAlias,
        ) => vec![SymbolFacet::Type],
        Some(
            Kind::AbstractMethod
            | Kind::Constructor
            | Kind::Function
            | Kind::Method
            | Kind::MethodAlias
            | Kind::MethodSpecification,
        ) => vec![SymbolFacet::Callable, SymbolFacet::Value],
        Some(Kind::Module | Kind::Namespace | Kind::Package | Kind::PackageObject) => {
            vec![SymbolFacet::Namespace]
        }
        Some(_) => vec![SymbolFacet::Value],
        None => Vec::new(),
    }
}

fn fallback_name(symbol: &str) -> String {
    scip::symbol::parse_symbol(symbol)
        .ok()
        .and_then(|symbol| {
            symbol
                .descriptors
                .last()
                .map(|descriptor| descriptor.name.clone())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| symbol.to_owned())
}

fn snake_case(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character.to_ascii_lowercase());
        }
    }
    output
}

fn information_json(information: &SymbolInformation) -> Value {
    json!({
        "symbol": information.symbol,
        "display_name": information.display_name,
        "kind": information.kind.value(),
        "documentation": information.documentation,
        "enclosing_symbol": information.enclosing_symbol,
        "signature": information.signature_documentation.as_ref().map(|signature| {
            json!({
                "language": signature.language,
                "text": signature.text,
                "occurrence_count": signature.occurrences.len(),
            })
        }),
        "relationships": information.relationships.iter().map(|relationship| {
            json!({
                "symbol": relationship.symbol,
                "is_reference": relationship.is_reference,
                "is_definition": relationship.is_definition,
                "is_implementation": relationship.is_implementation,
                "is_type_definition": relationship.is_type_definition,
            })
        }).collect::<Vec<_>>(),
    })
}

fn diagnostic_tags(diagnostic: &scip::types::Diagnostic) -> Vec<i32> {
    let mut tags = Vec::with_capacity(diagnostic.tags.len());
    for tag in &diagnostic.tags {
        tags.push(tag.value());
    }
    tags
}

fn occurrence_json(occurrence: &Occurrence) -> Value {
    json!({
        "range": occurrence.range,
        "symbol": occurrence.symbol,
        "symbol_roles": occurrence.symbol_roles,
        "override_documentation": occurrence.override_documentation,
        "syntax_kind": occurrence.syntax_kind.value(),
        "diagnostics": occurrence.diagnostics.iter().map(|diagnostic| {
            json!({
                "severity": diagnostic.severity.value(),
                "code": diagnostic.code,
                "message": diagnostic.message,
                "source": diagnostic.source,
                "tags": diagnostic_tags(diagnostic),
            })
        }).collect::<Vec<_>>(),
        "enclosing_range": occurrence.enclosing_range,
        "typed_range": raw_occurrence_range(occurrence),
    })
}

fn scip_error(violation: ScipViolation, detail: impl Into<String>) -> ScipAdapterError {
    ScipAdapterError {
        violation,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use scip::types::{
        Diagnostic, Document, Index, Occurrence, PositionEncoding, Relationship, Severity,
        SymbolInformation, SymbolRole, symbol_information,
    };

    use super::{ScipAdapter, ScipViolation, position_to_byte};
    use crate::{ProviderInputMode, PublicationLimits};
    use rift_core::{
        DocumentationFormat, ProviderId, ProviderRevision, SourceResolverId, SourceRevision,
        TreeRevision,
    };

    fn adapter() -> ScipAdapter {
        ScipAdapter::new(
            ProviderId::new("scip").expect("provider"),
            ProviderRevision::new(1).expect("publication"),
            SourceRevision::new(2).expect("source"),
            TreeRevision::new(3).expect("tree"),
            SourceResolverId::new("project").expect("resolver"),
            PublicationLimits::default(),
        )
    }

    fn fixture() -> Index {
        let symbol = "rust cargo app 1 main().";
        let target = "rust cargo app 1 Runnable#";

        let mut relationship = Relationship::new();
        relationship.symbol = target.to_owned();
        relationship.is_reference = true;
        relationship.is_implementation = true;

        let mut information = SymbolInformation::new();
        information.symbol = symbol.to_owned();
        information.display_name = "main".to_owned();
        information.kind = symbol_information::Kind::Function.into();
        information.documentation = vec!["Runs application.".to_owned()];
        information.relationships = vec![relationship];

        let mut diagnostic = Diagnostic::new();
        diagnostic.severity = Severity::Warning.into();
        diagnostic.code = "unused".to_owned();
        diagnostic.message = "unused result".to_owned();
        diagnostic.source = "rust".to_owned();

        let mut definition = Occurrence::new();
        definition.range = vec![0, 3, 7];
        definition.symbol = symbol.to_owned();
        definition.symbol_roles = SymbolRole::Definition as i32;

        let mut reference = Occurrence::new();
        reference.range = vec![1, 0, 4];
        reference.symbol = symbol.to_owned();
        reference.symbol_roles = SymbolRole::ReadAccess as i32;
        reference.override_documentation = vec!["Call site.".to_owned()];
        reference.diagnostics = vec![diagnostic];

        let mut document = Document::new();
        document.language = "rust".to_owned();
        document.relative_path = "src/main.rs".to_owned();
        document.text = "fn main() {}\nmain();\n".to_owned();
        document.position_encoding = PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into();
        document.symbols = vec![information];
        document.occurrences = vec![definition, reference];

        let mut external = SymbolInformation::new();
        external.symbol = target.to_owned();
        external.display_name = "Runnable".to_owned();
        external.kind = symbol_information::Kind::Trait.into();

        let mut index = Index::new();
        index.documents = vec![document];
        index.external_symbols = vec![external];
        index
    }

    #[test]
    fn snapshot_preserves_symbols_occurrences_docs_diagnostics_and_relationships() {
        let output = adapter().convert(&fixture()).expect("SCIP snapshot");
        assert_eq!(output.mode(), ProviderInputMode::Snapshot);
        let publication = output.publication();
        assert_eq!(publication.contributions().len(), 2);
        let main = publication
            .contributions()
            .iter()
            .find(|contribution| contribution.facts().name() == "main")
            .expect("main");
        assert!(main.identity_anchor().is_none());
        assert_eq!(main.references().len(), 2);
        assert_eq!(main.relationships().len(), 2);
        assert_eq!(
            main.facts().documentation_blocks()[0].format,
            DocumentationFormat::Markdown
        );
        let data = &main
            .namespaced()
            .0
            .get(&rift_core::ExtensionKey("io.scip.symbol".to_owned()))
            .expect("SCIP namespace")
            .data;
        assert_eq!(data["occurrences"][1]["diagnostics"][0]["code"], "unused");
    }

    #[test]
    fn utf16_positions_convert_to_utf8_bytes() {
        assert_eq!(
            position_to_byte(
                "a😀b",
                0,
                3,
                PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
            ),
            Some(5)
        );
        assert_eq!(
            position_to_byte(
                "a😀b",
                0,
                2,
                PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
            ),
            None
        );
    }

    #[test]
    fn duplicate_provider_symbol_is_refused() {
        let mut index = fixture();
        let duplicate = index.documents[0].symbols[0].clone();
        index.documents[0].symbols.push(duplicate);
        let error = adapter().convert(&index).expect_err("duplicate symbol");
        assert_eq!(error.violation(), ScipViolation::DuplicateSymbol);
    }

    #[test]
    fn helper_mappings_cover_range_role_relationship_and_name_forms() {
        let mut occurrence = Occurrence::new();
        occurrence.range = vec![1, 2, 3];
        assert_eq!(super::raw_occurrence_range(&occurrence), Some((1, 2, 1, 3)));
        occurrence.range = vec![1, 2, 3, 4];
        assert_eq!(super::raw_occurrence_range(&occurrence), Some((1, 2, 3, 4)));
        occurrence.range.clear();
        assert_eq!(super::raw_occurrence_range(&occurrence), None);

        let roles = [
            (SymbolRole::Definition, rift_core::ReferenceRole::Definition),
            (SymbolRole::Import, rift_core::ReferenceRole::Import),
            (SymbolRole::WriteAccess, rift_core::ReferenceRole::Write),
            (SymbolRole::ReadAccess, rift_core::ReferenceRole::Read),
        ];
        for (role, expected) in roles {
            occurrence.symbol_roles = role as i32;
            assert_eq!(super::reference_role(&occurrence), expected);
        }
        occurrence.symbol_roles = 0;
        assert_eq!(
            super::reference_role(&occurrence),
            rift_core::ReferenceRole::Unknown
        );

        let mut relationship = Relationship::new();
        relationship.is_reference = true;
        relationship.is_definition = true;
        relationship.is_implementation = true;
        relationship.is_type_definition = true;
        assert_eq!(
            super::relationship_kinds(&relationship).collect::<Vec<_>>(),
            vec![
                rift_core::RelationshipKind::Reference,
                rift_core::RelationshipKind::Definition,
                rift_core::RelationshipKind::Implementation,
                rift_core::RelationshipKind::TypeDefinition,
            ]
        );

        assert_eq!(
            super::portable_facets(Some(symbol_information::Kind::Class)),
            vec![rift_core::SymbolFacet::Type]
        );
        assert_eq!(
            super::portable_facets(Some(symbol_information::Kind::Function)),
            vec![
                rift_core::SymbolFacet::Callable,
                rift_core::SymbolFacet::Value
            ]
        );
        assert_eq!(
            super::portable_facets(Some(symbol_information::Kind::Module)),
            vec![rift_core::SymbolFacet::Namespace]
        );
        assert_eq!(
            super::portable_facets(Some(symbol_information::Kind::Field)),
            vec![rift_core::SymbolFacet::Value]
        );
        assert!(super::portable_facets(None).is_empty());

        assert_eq!(super::canonical_language(" Rust "), "rust");
        assert_eq!(super::canonical_language("  "), "unknown");
        assert_eq!(
            super::fallback_name("rust cargo app 1 Runnable#"),
            "Runnable"
        );
        assert_eq!(
            super::fallback_name("not a SCIP symbol"),
            "not a SCIP symbol"
        );
        assert_eq!(super::snake_case("TypeParameter"), "type_parameter");
    }
}
