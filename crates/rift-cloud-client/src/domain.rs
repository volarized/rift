//! Domain values produced from validated global package responses.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use crate::{
    ClientError, HitLocation, PackageDocumentationHit, PackageIdentity, PackageSearchHit,
    PackageSearchHitContributingField, PackageSearchItem, PackageSymbol, SourceKind,
    SourceLocationKind, Symbol, SymbolFacet, SymbolOrigin, TextRange, TypeBinding,
    TypeBindingOrigin, TypeBindingRole, TypeExpression,
};

/// One package declaration returned by global search in the local read model.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageSearchCandidate {
    /// Package that owns declaration.
    pub package: rift_protocol::read::PackageIdentity,
    /// Ranking identity for declaration.
    pub identity: rift_ranking::DocumentIdentity,
    /// Search hit carrying declaration.
    pub hit: rift_protocol::read::SearchHit,
    /// Identifier match class established by global index.
    pub match_class: Option<rift_ranking::IdentifierMatchClass>,
}

/// One package declaration returned by global symbol lookup in the local read model.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageSymbolCandidate {
    /// Package that owns declaration.
    pub package: rift_protocol::read::PackageIdentity,
    /// Stable identity of declaration symbol.
    pub symbol_identity: rift_protocol::read::SymbolId,
    /// Symbol hit carrying declaration.
    pub hit: rift_protocol::read::GetSymbolHit,
    /// Identifier match class established by global index.
    pub match_class: rift_ranking::IdentifierMatchClass,
}

impl TryFrom<PackageSearchHit> for PackageSearchCandidate {
    type Error = ClientError;

    fn try_from(value: PackageSearchHit) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<PackageSearchItem> for PackageSearchCandidate {
    type Error = ClientError;

    fn try_from(value: PackageSearchItem) -> Result<Self, Self::Error> {
        match value {
            PackageSearchItem::Search(hit) => Self::try_from(hit),
            PackageSearchItem::Documentation(hit) => documentation_candidate(hit),
        }
    }
}

impl TryFrom<&PackageSearchHit> for PackageSearchCandidate {
    type Error = ClientError;

    fn try_from(value: &PackageSearchHit) -> Result<Self, Self::Error> {
        let (qualified_name, unit, line) = validate_location(
            &value.package,
            &value.symbol,
            &value.unit,
            &value.range,
            value.line,
            value.source.as_deref(),
        )?;
        let package = package_identity(&value.package);
        let symbol = convert_symbol(&value.symbol)?;
        let identity = rift_ranking::DocumentIdentity::for_unit(&unit, &qualified_name)
            .map_err(|_| invalid("symbol_identity"))?;
        let match_class = crate::ranking_match_class(&value.match_class)?;
        let hit = rift_protocol::read::SearchHit {
            hit: rift_protocol::read::SearchHitTarget::Symbol {
                symbol: Box::new(symbol),
            },
            score: None,
            matched_by: matched_fields(&value.contributing_fields),
            source: value.source.clone(),
            range: Some(text_range(&value.range)),
            line: Some(line),
            path: None,
            unit: Some(protocol_unit(&value.unit)),
            traversal_path: None,
            distance: None,
            change: None,
        };
        Ok(Self {
            package,
            identity,
            hit,
            match_class: Some(match_class),
        })
    }
}

fn documentation_candidate(
    value: PackageDocumentationHit,
) -> Result<PackageSearchCandidate, ClientError> {
    if value.target != "documentation" {
        return Err(invalid("target"));
    }
    let documentation = protocol_documentation_hit(value.documentation)?;
    let package = package_identity(&value.package);
    rift_analysis::documentation::validate_documentation_hit(&documentation)
        .map_err(|_| invalid("documentation"))?;
    if documentation.source.origin.package.as_ref() != Some(&package)
        || value.contributing_fields.is_empty()
        || value.contributing_fields.len() > 16
        || value.source.as_ref().is_some_and(|source| {
            source.len() > rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize
                || source.len() as u64
                    > documentation.block.range.end - documentation.block.range.start
        })
    {
        return Err(invalid("documentation"));
    }
    let (path, unit) = match &documentation.block.source.source {
        rift_protocol::documentation::DocumentationSourceIdentity::Project { .. } => {
            return Err(invalid("source_identity"));
        }
        rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } => {
            package_source_unit_prefix(&package, unit)?;
            (None, Some(unit.clone()))
        }
    };
    let identity =
        rift_ranking::DocumentIdentity::for_documentation_block(&documentation.block.identity.0)
            .map_err(|_| invalid("documentation.identity"))?;
    let matched_by = value
        .contributing_fields
        .iter()
        .map(|field| match field {
            crate::PackageDocumentationHitContributingField::Documentation => {
                rift_protocol::read::MatchedField::Documentation
            }
            crate::PackageDocumentationHitContributingField::Content => {
                rift_protocol::read::MatchedField::Content
            }
            crate::PackageDocumentationHitContributingField::Unknown => {
                rift_protocol::read::MatchedField::Ranked
            }
        })
        .collect();
    let hit = rift_protocol::read::SearchHit {
        hit: rift_protocol::read::SearchHitTarget::Documentation {
            documentation: Box::new(documentation.clone()),
        },
        score: None,
        matched_by,
        source: value.source,
        range: Some(protocol_range(&documentation.block.range)),
        line: Some(documentation.block.line),
        path,
        unit,
        traversal_path: None,
        distance: None,
        change: None,
    };
    Ok(PackageSearchCandidate {
        package,
        identity,
        hit,
        match_class: None,
    })
}

fn package_source_unit_prefix(
    package: &rift_protocol::read::PackageIdentity,
    unit: &rift_protocol::read::SourceUnitId,
) -> Result<String, ClientError> {
    let parsed = rift_core::SourceUnitId::parse(&unit.0).map_err(|_| invalid("source_identity"))?;
    let package_prefix = format!("{}@{}/", package.name, package.version);
    if parsed.resolver().as_str() != package.manager
        || !parsed.key().as_str().starts_with(&package_prefix)
    {
        return Err(invalid("source_identity"));
    }
    Ok(unit.0.clone())
}

fn protocol_documentation_hit(
    value: crate::DocumentationHit,
) -> Result<rift_protocol::documentation::DocumentationHit, ClientError> {
    Ok(rift_protocol::documentation::DocumentationHit {
        block: protocol_documentation_block(value.block)?,
        source: protocol_documentation_source(value.source)?,
        documentation_revision: rift_protocol::read::Digest(value.documentation_revision),
    })
}

fn protocol_documentation_block(
    value: crate::DocumentationBlock,
) -> Result<rift_protocol::documentation::DocumentationBlock, ClientError> {
    use rift_protocol::documentation as protocol;
    Ok(protocol::DocumentationBlock {
        identity: rift_protocol::documentation::DocumentationDigest(value.identity),
        source: protocol_content_identity(value.source)?,
        content_digest: rift_protocol::documentation::DocumentationDigest(value.content_digest),
        heading_path: value
            .heading_path
            .unwrap_or_default()
            .into_iter()
            .map(|heading| protocol::DocumentationHeading {
                level: heading.level,
                name: heading.name,
            })
            .collect(),
        range: text_range(&value.range),
        line: value.line,
        kind: match value.kind {
            crate::DocumentationBlockKind::Prose => protocol::DocumentationBlockKind::Prose,
            crate::DocumentationBlockKind::Code => protocol::DocumentationBlockKind::Code,
        },
        language: value.language,
        chunks: value
            .chunks
            .unwrap_or_default()
            .into_iter()
            .map(|chunk| protocol::DocumentationChunk {
                identity: chunk.identity,
                range: text_range(&chunk.range),
            })
            .collect(),
        symbol: value.symbol.map(rift_protocol::read::SymbolId),
    })
}

fn protocol_documentation_source(
    value: crate::DocumentationSource,
) -> Result<rift_protocol::documentation::DocumentationSource, ClientError> {
    use rift_protocol::documentation as protocol;
    Ok(protocol::DocumentationSource {
        identity: protocol_content_identity(value.identity)?,
        revision: rift_protocol::documentation::DocumentationDigest(value.revision),
        content_digest: rift_protocol::documentation::DocumentationDigest(value.content_digest),
        origin: convert_origin(Some(&value.origin)),
        format: match value.format {
            crate::DocumentationSourceFormat::Markdown => {
                protocol::DocumentationSourceFormat::Markdown
            }
            crate::DocumentationSourceFormat::Mdx => protocol::DocumentationSourceFormat::Mdx,
            crate::DocumentationSourceFormat::RestructuredText => {
                protocol::DocumentationSourceFormat::RestructuredText
            }
            crate::DocumentationSourceFormat::Text => protocol::DocumentationSourceFormat::Text,
            crate::DocumentationSourceFormat::Notebook => {
                protocol::DocumentationSourceFormat::Notebook
            }
            crate::DocumentationSourceFormat::AttachedComment => {
                protocol::DocumentationSourceFormat::AttachedComment
            }
        },
        media_type: value.media_type,
        selection: match value.selection {
            crate::DocumentationSelectionReason::Workspace => {
                protocol::DocumentationSelectionReason::Workspace
            }
            crate::DocumentationSelectionReason::PackageArchive => {
                protocol::DocumentationSelectionReason::PackageArchive
            }
            crate::DocumentationSelectionReason::AttachedComment => {
                protocol::DocumentationSelectionReason::AttachedComment
            }
            crate::DocumentationSelectionReason::CloudResolver => {
                protocol::DocumentationSelectionReason::CloudResolver
            }
        },
        byte_length: value.byte_length,
        language: value
            .language
            .map(|source_language| language(source_language.as_str()))
            .transpose()?,
        physical_ranges: value
            .physical_ranges
            .unwrap_or_default()
            .iter()
            .map(text_range)
            .collect(),
        license: value
            .license
            .map(protocol_documentation_license)
            .transpose()?,
    })
}

fn protocol_content_identity(
    value: crate::DocumentationContentIdentity,
) -> Result<rift_protocol::documentation::DocumentationContentIdentity, ClientError> {
    use rift_protocol::documentation as protocol;
    let source = match value.source {
        crate::DocumentationSourceIdentity::ProjectPath(project) => {
            protocol::DocumentationSourceIdentity::Project {
                path: protocol_project_path(project.path, "documentation.source.path")?,
            }
        }
        crate::DocumentationSourceIdentity::SourceUnitId(unit) => {
            rift_core::SourceUnitId::parse(&unit.unit)
                .map_err(|_| invalid("documentation.source.unit"))?;
            protocol::DocumentationSourceIdentity::Package {
                unit: protocol_unit(&unit.unit),
            }
        }
    };
    let cell = value.cell.map(|cell| protocol::NotebookCell {
        identity: match cell.identity {
            crate::NotebookCellIdentity::Object(identity) => {
                protocol::NotebookCellIdentity::Authored { id: identity.id }
            }
            crate::NotebookCellIdentity::Object2(identity) => {
                protocol::NotebookCellIdentity::Indexed {
                    index: identity.index,
                }
            }
        },
        kind: match cell.kind {
            crate::NotebookCellKind::Markdown => protocol::NotebookCellKind::Markdown,
            crate::NotebookCellKind::Code => protocol::NotebookCellKind::Code,
        },
    });
    Ok(protocol::DocumentationContentIdentity { source, cell })
}

pub(crate) fn protocol_documentation_context(
    value: crate::DocumentationContext,
) -> Result<rift_protocol::documentation::DocumentationContext, ClientError> {
    use rift_protocol::documentation as protocol;
    Ok(protocol::DocumentationContext {
        documentation_revision: rift_protocol::read::Digest(value.documentation_revision),
        references: value
            .references
            .into_iter()
            .map(|hit| {
                Ok(protocol::DocumentationReferenceHit {
                    reference: protocol::DocumentationReference {
                        identity: rift_protocol::documentation::DocumentationDigest(
                            hit.reference.identity,
                        ),
                        block: rift_protocol::documentation::DocumentationDigest(
                            hit.reference.block,
                        ),
                        target: rift_protocol::read::SymbolId(hit.reference.target),
                        range: text_range(&hit.reference.range),
                        authored: hit.reference.authored,
                        evidence: match hit.reference.evidence {
                            crate::DocumentationReferenceEvidence::Provider => {
                                protocol::DocumentationReferenceEvidence::Provider
                            }
                            crate::DocumentationReferenceEvidence::AuthoredLink => {
                                protocol::DocumentationReferenceEvidence::AuthoredLink
                            }
                            crate::DocumentationReferenceEvidence::QualifiedName => {
                                protocol::DocumentationReferenceEvidence::QualifiedName
                            }
                            crate::DocumentationReferenceEvidence::UniqueName => {
                                protocol::DocumentationReferenceEvidence::UniqueName
                            }
                        },
                    },
                    documentation: protocol_documentation_hit(hit.documentation)?,
                    excerpt: hit.excerpt,
                })
            })
            .collect::<Result<_, ClientError>>()?,
        truncated: value.truncated,
        warnings: value
            .warnings
            .unwrap_or_default()
            .into_iter()
            .map(|warning| {
                Ok(protocol::DocumentationWarning {
                    source: protocol_content_identity(warning.source)?,
                    stage: match warning.stage {
                        crate::DocumentationStage::Source => protocol::DocumentationStage::Source,
                        crate::DocumentationStage::Extract => protocol::DocumentationStage::Extract,
                        crate::DocumentationStage::Resolve => protocol::DocumentationStage::Resolve,
                        crate::DocumentationStage::Index => protocol::DocumentationStage::Index,
                    },
                    kind: match warning.kind {
                        crate::DocumentationWarningKind::SourceUnavailable => {
                            protocol::DocumentationWarningKind::SourceUnavailable
                        }
                        crate::DocumentationWarningKind::SourceTruncated => {
                            protocol::DocumentationWarningKind::SourceTruncated
                        }
                        crate::DocumentationWarningKind::UnsupportedFormat => {
                            protocol::DocumentationWarningKind::UnsupportedFormat
                        }
                        crate::DocumentationWarningKind::MalformedSource => {
                            protocol::DocumentationWarningKind::MalformedSource
                        }
                        crate::DocumentationWarningKind::OmittedRange => {
                            protocol::DocumentationWarningKind::OmittedRange
                        }
                        crate::DocumentationWarningKind::LimitExceeded => {
                            protocol::DocumentationWarningKind::LimitExceeded
                        }
                    },
                    count: warning.count,
                })
            })
            .collect::<Result<_, ClientError>>()?,
    })
}

fn protocol_documentation_license(
    value: crate::DocumentationLicense,
) -> Result<rift_protocol::documentation::DocumentationLicense, ClientError> {
    Ok(rift_protocol::documentation::DocumentationLicense {
        expression: value.expression,
        files: value
            .files
            .unwrap_or_default()
            .into_iter()
            .map(|file| {
                Ok(rift_protocol::documentation::DocumentationLicenseFile {
                    path: protocol_project_path(file.path, "documentation.license.path")?,
                    digest: rift_protocol::documentation::DocumentationDigest(file.digest),
                })
            })
            .collect::<Result<_, ClientError>>()?,
    })
}

impl TryFrom<PackageSymbol> for PackageSymbolCandidate {
    type Error = ClientError;

    fn try_from(value: PackageSymbol) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<&PackageSymbol> for PackageSymbolCandidate {
    type Error = ClientError;

    fn try_from(value: &PackageSymbol) -> Result<Self, Self::Error> {
        let (_, _, line) = validate_location(
            &value.package,
            &value.symbol,
            &value.unit,
            &value.range,
            value.line,
            value.source.as_deref(),
        )?;
        let package = package_identity(&value.package);
        let symbol = convert_symbol(&value.symbol)?;
        let symbol_identity = symbol
            .id
            .clone()
            .ok_or_else(|| invalid("symbol_identity"))?;
        let match_class = crate::ranking_match_class(&value.match_class)?;
        let documentation = value
            .documentation
            .clone()
            .map(protocol_documentation_context)
            .transpose()?;
        if let Some(context) = &documentation {
            validate_documentation_context(context, &symbol_identity, &package)?;
        }
        let hit = rift_protocol::read::GetSymbolHit {
            symbol,
            path: None,
            unit: Some(protocol_unit(&value.unit)),
            range: text_range(&value.range),
            line,
            node: None,
            source: value.source.clone(),
            history: None,
            documentation,
        };
        Ok(Self {
            package,
            symbol_identity,
            hit,
            match_class,
        })
    }
}

pub(crate) fn validate_documentation_context(
    context: &rift_protocol::documentation::DocumentationContext,
    target: &rift_protocol::read::SymbolId,
    package: &rift_protocol::read::PackageIdentity,
) -> Result<(), ClientError> {
    rift_analysis::documentation::validate_documentation_context(context, target)
        .map_err(|_| invalid("documentation"))?;
    for reference in &context.references {
        let source = &reference.documentation.source;
        if source.origin.package.as_ref() != Some(package) {
            return Err(invalid("documentation"));
        }
        validate_package_content_identity(&source.identity, package)?;
    }
    for warning in &context.warnings {
        validate_package_content_identity(&warning.source, package)?;
    }
    Ok(())
}

fn validate_package_content_identity(
    identity: &rift_protocol::documentation::DocumentationContentIdentity,
    package: &rift_protocol::read::PackageIdentity,
) -> Result<(), ClientError> {
    let rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } =
        &identity.source
    else {
        return Err(invalid("source_identity"));
    };
    package_source_unit_prefix(package, unit)?;
    Ok(())
}

fn validate_location(
    package: &PackageIdentity,
    symbol: &Symbol,
    unit: &str,
    range: &TextRange,
    line: i64,
    source: Option<&str>,
) -> Result<(String, rift_core::SourceUnitId, u64), ClientError> {
    let packages = HashSet::from([crate::package_key(package)]);
    let qualified_name = crate::validate_hit_common(
        package,
        symbol,
        HitLocation {
            unit,
            range,
            line,
            source,
        },
        &packages,
        crate::SOURCE_BYTES_MAX,
    )?;
    let source_unit =
        rift_core::SourceUnitId::parse(unit).map_err(|_| invalid("source_identity"))?;
    let line = u64::try_from(line).map_err(|_| invalid("location"))?;
    Ok((qualified_name, source_unit, line))
}

pub(crate) fn package_identity(value: &PackageIdentity) -> rift_protocol::read::PackageIdentity {
    rift_protocol::read::PackageIdentity {
        manager: value.manager.clone(),
        name: value.name.clone(),
        version: value.version.clone(),
    }
}

fn protocol_unit(value: &str) -> rift_protocol::read::SourceUnitId {
    rift_protocol::read::SourceUnitId(value.to_owned())
}

fn text_range(value: &TextRange) -> rift_protocol::read::TextRange {
    rift_protocol::read::TextRange {
        start: value.start,
        end: value.end,
    }
}

fn protocol_range(value: &rift_protocol::read::TextRange) -> rift_protocol::read::TextRange {
    rift_protocol::read::TextRange {
        start: value.start,
        end: value.end,
    }
}

fn protocol_project_path(
    value: String,
    field: &'static str,
) -> Result<rift_protocol::read::ProjectPath, ClientError> {
    let validated = rift_core::ProjectPath::new(value).map_err(|_| invalid(field))?;
    Ok(rift_protocol::read::ProjectPath(validated.to_string()))
}

fn matched_fields(
    values: &[PackageSearchHitContributingField],
) -> Vec<rift_protocol::read::MatchedField> {
    let mut fields = Vec::new();
    for value in values {
        let field = match value {
            PackageSearchHitContributingField::Name
            | PackageSearchHitContributingField::QualifiedName => {
                rift_protocol::read::MatchedField::Name
            }
            PackageSearchHitContributingField::Documentation => {
                rift_protocol::read::MatchedField::Documentation
            }
            PackageSearchHitContributingField::Signature => {
                rift_protocol::read::MatchedField::Signature
            }
            PackageSearchHitContributingField::DeclarationSource => {
                rift_protocol::read::MatchedField::Content
            }
            PackageSearchHitContributingField::Unknown => rift_protocol::read::MatchedField::Ranked,
        };
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    if fields.is_empty() {
        fields.push(rift_protocol::read::MatchedField::Ranked);
    }
    fields
}

fn convert_symbol(value: &Symbol) -> Result<rift_protocol::read::Symbol, ClientError> {
    Ok(rift_protocol::read::Symbol {
        id: value.id.clone().map(rift_protocol::read::SymbolId),
        language: language(&value.language)?,
        name: value.name.clone(),
        kind: rift_protocol::read::ExactKind(value.kind.clone()),
        facets: value
            .facets
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_facet)
            .collect(),
        origin: convert_origin(value.origin.as_ref()),
        container: value.container.clone().map(rift_protocol::read::SymbolId),
        modifiers: value.modifiers.clone().unwrap_or_default(),
        visibility: value.visibility.clone(),
        types: value
            .types
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        signatures: value
            .signatures
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_signature)
            .collect::<Result<_, _>>()?,
        documentation: value
            .documentation
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_documentation)
            .collect(),
        extensions: convert_extensions(value.extensions.as_ref())?,
        document_local: value.document_local.unwrap_or(false),
    })
}

fn convert_origin(value: Option<&SymbolOrigin>) -> rift_protocol::read::SymbolOrigin {
    let Some(value) = value else {
        return rift_protocol::read::SymbolOrigin {
            location: Some(rift_protocol::read::SourceLocationKind::Project),
            package: None,
            source_kind: rift_protocol::read::SourceKind::Authored,
        };
    };
    rift_protocol::read::SymbolOrigin {
        location: value.location.as_ref().map(convert_location),
        package: value.package.as_ref().map(package_identity),
        source_kind: convert_source_kind(&value.source_kind),
    }
}

fn convert_source_kind(value: &SourceKind) -> rift_protocol::read::SourceKind {
    match value {
        SourceKind::Authored => rift_protocol::read::SourceKind::Authored,
        SourceKind::Generated => rift_protocol::read::SourceKind::Generated,
        SourceKind::Synthetic => rift_protocol::read::SourceKind::Synthetic,
    }
}

fn convert_location(value: &SourceLocationKind) -> rift_protocol::read::SourceLocationKind {
    match value {
        SourceLocationKind::Project => rift_protocol::read::SourceLocationKind::Project,
        SourceLocationKind::Dependency => rift_protocol::read::SourceLocationKind::Dependency,
        SourceLocationKind::Stdlib => rift_protocol::read::SourceLocationKind::Stdlib,
        SourceLocationKind::External => rift_protocol::read::SourceLocationKind::External,
    }
}

fn convert_facet(value: &SymbolFacet) -> rift_protocol::read::SymbolFacet {
    use rift_protocol::read::SymbolFacet as Protocol;
    match value {
        SymbolFacet::Namespace => Protocol::Namespace,
        SymbolFacet::Module => Protocol::Module,
        SymbolFacet::TypeType => Protocol::Type,
        SymbolFacet::Value => Protocol::Value,
        SymbolFacet::Callable => Protocol::Callable,
        SymbolFacet::Member => Protocol::Member,
        SymbolFacet::MemberContainer => Protocol::MemberContainer,
        SymbolFacet::Parameter => Protocol::Parameter,
        SymbolFacet::TypeParameter => Protocol::TypeParameter,
        SymbolFacet::Constructible => Protocol::Constructible,
        SymbolFacet::Extensible => Protocol::Extensible,
        SymbolFacet::Implementable => Protocol::Implementable,
        SymbolFacet::Macro => Protocol::Macro,
        SymbolFacet::Test => Protocol::Test,
        SymbolFacet::Annotation => Protocol::Annotation,
        SymbolFacet::Extension => Protocol::Extension,
        SymbolFacet::Variant => Protocol::Variant,
        SymbolFacet::Enumeration => Protocol::Enumeration,
        SymbolFacet::Alias => Protocol::Alias,
        SymbolFacet::Property => Protocol::Property,
        SymbolFacet::Abstract => Protocol::Abstract,
        SymbolFacet::Constructor => Protocol::Constructor,
        SymbolFacet::Static => Protocol::Static,
        SymbolFacet::Mutable => Protocol::Mutable,
        SymbolFacet::Deprecated => Protocol::Deprecated,
        SymbolFacet::Public => Protocol::Public,
        SymbolFacet::Entrypoint => Protocol::Entrypoint,
        SymbolFacet::Operator => Protocol::Operator,
        SymbolFacet::Async => Protocol::Async,
        SymbolFacet::Generator => Protocol::Generator,
    }
}

fn convert_documentation(
    value: &crate::generated::Documentation,
) -> rift_protocol::read::Documentation {
    rift_protocol::read::Documentation {
        format: match value.format {
            crate::generated::DocumentationFormat::Plain => {
                rift_protocol::read::DocumentationFormat::Plain
            }
            crate::generated::DocumentationFormat::Markdown => {
                rift_protocol::read::DocumentationFormat::Markdown
            }
        },
        text: value.text.clone(),
    }
}

fn convert_signature(
    value: &crate::generated::Signature,
) -> Result<rift_protocol::read::Signature, ClientError> {
    Ok(rift_protocol::read::Signature {
        display: value.display.clone(),
        links: value
            .links
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_signature_link)
            .collect(),
        language: language(&value.language)?,
        receiver: value.receiver.as_ref().map(convert_parameter).transpose()?,
        parameters: value
            .parameters
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_parameter)
            .collect::<Result<_, _>>()?,
        returns: value
            .returns
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        type_parameters: value
            .type_parameters
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .map(rift_protocol::read::SymbolId)
            .collect(),
        throws: value
            .throws
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_expression)
            .collect::<Result<_, _>>()?,
        effects: value.effects.clone().unwrap_or_default(),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn convert_signature_link(
    value: &crate::generated::SignatureLink,
) -> rift_protocol::read::SignatureLink {
    rift_protocol::read::SignatureLink {
        range: text_range(&value.range),
        symbol: rift_protocol::read::SymbolId(value.symbol.clone()),
    }
}

fn convert_parameter(
    value: &crate::generated::Parameter,
) -> Result<rift_protocol::read::Parameter, ClientError> {
    Ok(rift_protocol::read::Parameter {
        name: value.name.clone(),
        node: value.node.clone().map(rift_protocol::read::NodeId),
        types: value
            .types
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        optional: value.optional,
        variadic: value.variadic,
        default: value.default.clone(),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn convert_type_binding(
    value: &TypeBinding,
) -> Result<rift_protocol::read::TypeBinding, ClientError> {
    Ok(rift_protocol::read::TypeBinding {
        role: match value.role {
            TypeBindingRole::Receiver => rift_protocol::read::TypeBindingRole::Receiver,
            TypeBindingRole::Parameter => rift_protocol::read::TypeBindingRole::Parameter,
            TypeBindingRole::Return => rift_protocol::read::TypeBindingRole::Return,
            TypeBindingRole::Field => rift_protocol::read::TypeBindingRole::Field,
            TypeBindingRole::Bound => rift_protocol::read::TypeBindingRole::Bound,
            TypeBindingRole::Element => rift_protocol::read::TypeBindingRole::Element,
            TypeBindingRole::Key => rift_protocol::read::TypeBindingRole::Key,
            TypeBindingRole::Error => rift_protocol::read::TypeBindingRole::Error,
            TypeBindingRole::Underlying => rift_protocol::read::TypeBindingRole::Underlying,
            TypeBindingRole::Yielded => rift_protocol::read::TypeBindingRole::Yielded,
            TypeBindingRole::Awaited => rift_protocol::read::TypeBindingRole::Awaited,
            TypeBindingRole::Discriminant => rift_protocol::read::TypeBindingRole::Discriminant,
        },
        origin: match value.origin {
            TypeBindingOrigin::Declared => rift_protocol::read::TypeBindingOrigin::Declared,
            TypeBindingOrigin::Inferred => rift_protocol::read::TypeBindingOrigin::Inferred,
            TypeBindingOrigin::Expected => rift_protocol::read::TypeBindingOrigin::Expected,
        },
        r#type: convert_type_expression(&value.r#type)?,
    })
}

fn convert_type_expression(
    value: &TypeExpression,
) -> Result<rift_protocol::read::TypeExpression, ClientError> {
    Ok(rift_protocol::read::TypeExpression {
        language: language(&value.language)?,
        source: value.source.clone(),
        resolved: value.resolved.clone().map(rift_protocol::read::SymbolId),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn language(value: &str) -> Result<rift_protocol::read::Language, ClientError> {
    rift_protocol::read::Language::from_identity_segment(value).map_err(|_| invalid("language"))
}

fn convert_extensions(
    value: Option<&Value>,
) -> Result<rift_protocol::read::Extensions, ClientError> {
    static EXTENSION_KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^[a-z0-9]+(?:[.-][a-z0-9]+)+\.[A-Za-z][A-Za-z0-9_-]*$")
            .expect("extension key pattern")
    });
    let Some(value) = value else {
        return Ok(rift_protocol::read::Extensions::default());
    };
    let Value::Object(entries) = value else {
        return Err(invalid("extensions"));
    };
    let mut extensions = BTreeMap::new();
    for (key, value) in entries {
        if !EXTENSION_KEY.is_match(key) {
            return Err(invalid("extensions"));
        }
        let Value::Object(fields) = value else {
            return Err(invalid("extensions"));
        };
        let version = fields
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version > 0)
            .ok_or_else(|| invalid("extensions"))?;
        let data = fields
            .get("data")
            .cloned()
            .ok_or_else(|| invalid("extensions"))?;
        if fields
            .keys()
            .any(|field| field != "version" && field != "data")
        {
            return Err(invalid("extensions"));
        }
        extensions.insert(
            rift_protocol::read::ExtensionKey(key.clone()),
            rift_protocol::read::ExtensionValue { version, data },
        );
    }
    Ok(rift_protocol::read::Extensions(extensions))
}

fn invalid(field: &'static str) -> ClientError {
    ClientError::InvalidResponseField { field }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn package() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: "helper".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn symbol(package: &PackageIdentity) -> Symbol {
        let id = rift_core::symbol_identity("rust", "src/lib.rs", "helper_beacon");
        Symbol {
            id: Some(id),
            language: "rust".to_owned(),
            name: "helper_beacon".to_owned(),
            kind: "function".to_owned(),
            origin: Some(SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                source_kind: SourceKind::Authored,
            }),
            documentation: Some(vec![crate::generated::Documentation {
                format: crate::generated::DocumentationFormat::Markdown,
                text: "Beacon docs".to_owned(),
            }]),
            facets: Some(vec![SymbolFacet::Callable]),
            ..Default::default()
        }
    }

    fn unit() -> String {
        "rift://source/cargo/helper@1.0.0/src/lib.rs".to_owned()
    }

    fn documentation() -> rift_protocol::documentation::DocumentationHit {
        serde_json::from_value(serde_json::json!({
            "documentation_revision":"0123abcd",
            "block": {
                "identity":"1".repeat(64), "content_digest":"2".repeat(64),
                "source":{"source":{"kind":"package","unit":"rift://source/cargo/helper@1.0.0/README.md"}},
                "range":{"start":0,"end":40}, "line":1, "kind":"prose"
            },
            "source": {
                "identity":{"source":{"kind":"package","unit":"rift://source/cargo/helper@1.0.0/README.md"}},
                "revision":"3".repeat(64), "content_digest":"4".repeat(64),
                "origin":{"location":"dependency","source_kind":"authored","package":{"manager":"cargo","name":"helper","version":"1.0.0"}},
                "format":"markdown", "media_type":"text/markdown", "selection":"package_archive", "byte_length":40
            }
        })).expect("documentation fixture")
    }

    fn documentation_item(
        hit: &rift_protocol::documentation::DocumentationHit,
    ) -> PackageSearchItem {
        serde_json::from_value(serde_json::json!({
            "target":"documentation", "package":package(), "documentation":hit,
            "contributing_fields":["content"], "source":"guide"
        }))
        .expect("generated documentation fixture")
    }

    fn documentation_context_and_target() -> (
        rift_protocol::documentation::DocumentationContext,
        rift_protocol::read::SymbolId,
    ) {
        use rift_protocol::documentation as docs;
        use rift_protocol::read::{Digest, SymbolId, TextRange as Range};

        let target = SymbolId(rift_core::symbol_identity(
            "rust",
            "cargo/helper@1.0.0/src/lib.rs",
            "helper_beacon",
        ));
        let context = docs::DocumentationContext {
            documentation_revision: Digest("0123abcd".to_owned()),
            references: vec![docs::DocumentationReferenceHit {
                reference: docs::DocumentationReference {
                    identity: rift_protocol::documentation::DocumentationDigest("5".repeat(64)),
                    block: rift_protocol::documentation::DocumentationDigest("1".repeat(64)),
                    target: target.clone(),
                    evidence: docs::DocumentationReferenceEvidence::UniqueName,
                    authored: "helper_beacon".to_owned(),
                    range: Range { start: 1, end: 14 },
                },
                documentation: documentation(),
                excerpt: Some("guide".to_owned()),
            }],
            truncated: false,
            warnings: Vec::new(),
        };
        (context, target)
    }

    #[test]
    fn documentation_candidate_validates_package_ranges_and_excerpt() {
        use rift_protocol::documentation::DocumentationSourceIdentity;
        let original = documentation();
        let candidate = PackageSearchCandidate::try_from(documentation_item(&original))
            .expect("valid documentation");
        assert_eq!(candidate.hit.source.as_deref(), Some("guide"));
        assert!(candidate.hit.path.is_none());
        for invalid_hit in [
            {
                let mut hit = original.clone();
                hit.block.range.end = 41;
                hit
            },
            {
                let mut hit = original.clone();
                hit.block.line = 0;
                hit
            },
            {
                let mut hit = original.clone();
                hit.block.source.source = DocumentationSourceIdentity::Package {
                    unit: rift_protocol::read::SourceUnitId(
                        "rift://source/cargo/other@1.0.0/README.md".to_owned(),
                    ),
                };
                hit
            },
            {
                let mut hit = original.clone();
                hit.source.identity.source = DocumentationSourceIdentity::Project {
                    path: rift_protocol::read::ProjectPath("README.md".to_owned()),
                };
                hit.block.source = hit.source.identity.clone();
                hit.source.origin.location = Some(rift_protocol::read::SourceLocationKind::Project);
                hit.source.selection =
                    rift_protocol::documentation::DocumentationSelectionReason::Workspace;
                hit
            },
        ] {
            assert!(PackageSearchCandidate::try_from(documentation_item(&invalid_hit)).is_err());
        }
        let mut item = documentation_item(&original);
        let PackageSearchItem::Documentation(hit) = &mut item else {
            panic!("documentation item")
        };
        hit.source = Some("x".repeat(41));
        assert!(PackageSearchCandidate::try_from(item).is_err());
    }

    #[test]
    fn documentation_candidate_refuses_wrong_target() {
        let mut item = documentation_item(&documentation());
        let PackageSearchItem::Documentation(hit) = &mut item else {
            panic!("documentation item")
        };
        hit.target = "symbol".to_owned();

        assert!(matches!(
            PackageSearchCandidate::try_from(item),
            Err(ClientError::InvalidResponseField { field: "target" })
        ));
    }

    #[test]
    fn documentation_context_refuses_forged_reference_and_other_package_warning() {
        use rift_protocol::documentation as docs;
        let (mut context, target) = documentation_context_and_target();
        let package = package_identity(&package());
        validate_documentation_context(&context, &target, &package).expect("valid context");
        context.references[0].reference.range.end = 41;
        assert!(validate_documentation_context(&context, &target, &package).is_err());
        context.references[0].reference.range.end = 14;
        context.references[0].reference.block =
            rift_protocol::documentation::DocumentationDigest("6".repeat(64));
        assert!(validate_documentation_context(&context, &target, &package).is_err());
        context.references[0].reference.block =
            rift_protocol::documentation::DocumentationDigest("1".repeat(64));
        let mut omitted = documentation().source.identity;
        omitted.source = docs::DocumentationSourceIdentity::Package {
            unit: rift_protocol::read::SourceUnitId(
                "rift://source/cargo/other@1.0.0/README.md".to_owned(),
            ),
        };
        context.warnings.push(docs::DocumentationWarning {
            source: omitted,
            stage: docs::DocumentationStage::Index,
            kind: docs::DocumentationWarningKind::LimitExceeded,
            count: 1,
        });
        assert!(validate_documentation_context(&context, &target, &package).is_err());
    }

    #[test]
    fn documentation_context_refuses_reference_from_another_hit_package() {
        let (context, target) = documentation_context_and_target();
        let other_package = rift_protocol::read::PackageIdentity {
            manager: "cargo".to_owned(),
            name: "other".to_owned(),
            version: "1.0.0".to_owned(),
        };

        assert!(matches!(
            validate_documentation_context(&context, &target, &other_package),
            Err(ClientError::InvalidResponseField {
                field: "documentation"
            })
        ));
    }

    #[test]
    fn documentation_context_refuses_project_warning_source_for_package_hit() {
        use rift_protocol::documentation as docs;
        use rift_protocol::read::ProjectPath;

        let (mut context, target) = documentation_context_and_target();
        let package = package_identity(&package());
        context.warnings.push(docs::DocumentationWarning {
            source: docs::DocumentationContentIdentity {
                source: docs::DocumentationSourceIdentity::Project {
                    path: ProjectPath("docs/guide.md".to_owned()),
                },
                cell: None,
            },
            stage: docs::DocumentationStage::Index,
            kind: docs::DocumentationWarningKind::SourceUnavailable,
            count: 1,
        });

        assert!(matches!(
            validate_documentation_context(&context, &target, &package),
            Err(ClientError::InvalidResponseField {
                field: "source_identity"
            })
        ));
    }

    #[test]
    fn search_candidate_maps_protocol_fields_and_identity() {
        let package = package();
        let hit = PackageSearchHit {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            contributing_fields: vec![
                PackageSearchHitContributingField::QualifiedName,
                PackageSearchHitContributingField::DeclarationSource,
            ],
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: Some("pub fn helper_beacon() {}".to_owned()),
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSearchCandidate::try_from(hit).expect("valid package hit");
        assert_eq!(candidate.package.name, "helper");
        assert_eq!(
            candidate.match_class,
            Some(rift_ranking::IdentifierMatchClass::NameExact)
        );
        assert_eq!(
            candidate.identity.as_unit(),
            Some((unit().as_str(), "helper_beacon"))
        );
        assert_eq!(candidate.hit.line, Some(3));
        assert_eq!(candidate.hit.matched_by.len(), 2);
        assert!(matches!(
            candidate.hit.hit,
            rift_protocol::read::SearchHitTarget::Symbol { .. }
        ));
    }

    #[test]
    fn symbol_candidate_maps_nested_symbol_without_json_roundtrip() {
        let package = package();
        let mut symbol = symbol(&package);
        symbol.extensions = Some(serde_json::json!({
            "org.example.fact": {"version": 1, "data": {"stable": true}}
        }));
        symbol.signatures = Some(vec![crate::generated::Signature {
            display: "helper_beacon(value: Value)".to_owned(),
            language: "rust".to_owned(),
            parameters: Some(vec![crate::generated::Parameter {
                name: Some("value".to_owned()),
                optional: false,
                variadic: false,
                ..Default::default()
            }]),
            ..Default::default()
        }]);
        let hit = PackageSymbol {
            package,
            symbol,
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::QualifiedExact,
            source: None,
            documentation: None,
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSymbolCandidate::try_from(hit).expect("valid package symbol");
        assert_eq!(
            candidate.match_class,
            rift_ranking::IdentifierMatchClass::QualifiedExact
        );
        assert_eq!(
            candidate.symbol_identity.0,
            candidate.hit.symbol.id.as_ref().expect("id").0
        );
        assert_eq!(candidate.hit.symbol.documentation[0].text, "Beacon docs");
        assert_eq!(
            candidate.hit.symbol.signatures[0].parameters[0]
                .name
                .as_deref(),
            Some("value")
        );
        assert_eq!(candidate.hit.symbol.extensions.0.len(), 1);
    }

    #[test]
    fn candidate_rejects_invalid_source_identity() {
        let package = package();
        let mut hit = PackageSymbol {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: None,
            documentation: None,
            additional_properties: HashMap::new(),
        };
        hit.unit = "rift://source/cargo/other@1.0.0/src/lib.rs".to_owned();
        assert_eq!(
            PackageSymbolCandidate::try_from(hit),
            Err(ClientError::InvalidResponseField {
                field: "source_identity"
            })
        );
    }
}
