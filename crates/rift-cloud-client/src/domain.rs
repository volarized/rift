//! Domain values produced from validated global package responses.

use rift_error::RiftError;

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use crate::{
    HitLocation, PackageDocumentationHit, PackageIdentity, PackagePatternDeclaration,
    PackagePatternHit, PackageSearchHit, PackageSearchHitContributingField, PackageSearchItem,
    SourceKind, SourceLocationKind, Symbol, SymbolFacet, SymbolOrigin, TextRange, TypeBinding,
    TypeBindingOrigin, TypeBindingRole, TypeExpression, failure,
};

/// One package declaration returned by global search in the local read model.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageSearchCandidate {
    /// Defining package, when the canonical owner is a package. Runtime owners carry none.
    pub package: Option<rift_protocol::read::PackageIdentity>,
    /// Ranking identity for declaration.
    pub identity: rift_ranking::DocumentIdentity,
    /// Search hit carrying declaration.
    pub hit: rift_protocol::read::SearchHit,
    /// Identifier match class established by global index. Absent for a documentation
    /// block, and for a declaration matching none of the requested identifiers.
    pub match_class: Option<rift_ranking::IdentifierMatchClass>,
}

/// One package pattern match in the local read model: the file hit for the match, and the
/// symbol hit for the smallest declaration holding it, when one does.
#[derive(Clone, Debug, PartialEq)]
pub struct PackagePatternMatch {
    /// Package that owns the file.
    pub package: rift_protocol::read::PackageIdentity,
    /// File hit addressed by `unit`, whose `range` is the match's bytes.
    pub file: rift_protocol::read::SearchHit,
    /// Symbol hit at the range of the declaration holding the match.
    pub declaration: Option<rift_protocol::read::SearchHit>,
}

impl TryFrom<&PackagePatternHit> for PackagePatternMatch {
    type Error = RiftError;

    fn try_from(value: &PackagePatternHit) -> Result<Self, Self::Error> {
        crate::package_source_path(&value.package, &value.unit)?;
        let line =
            u64::try_from(value.line).map_err(|source| invalid_source("location", source))?;
        let size =
            u64::try_from(value.size).map_err(|source| invalid_source("location", source))?;
        if line == 0 || value.range.end < value.range.start || size < value.range.end {
            return Err(invalid("location"));
        }
        let file = rift_protocol::read::SearchHit {
            view: None,
            hit: rift_protocol::read::SearchHitTarget::File {
                size,
                languages: Vec::new(),
            },
            score: None,
            matched_by: vec![rift_protocol::read::MatchedField::Content],
            source: value.source.clone(),
            range: Some(text_range(&value.range)),
            line: Some(line),
            path: None,
            unit: Some(protocol_unit(&value.unit)),
            traversal_path: None,
            distance: None,
            change: None,
        };
        let declaration = value
            .declaration
            .as_ref()
            .map(|declaration| declaration_hit(&value.package, &value.unit, declaration))
            .transpose()?;
        Ok(Self {
            package: package_identity(&value.package),
            file,
            declaration,
        })
    }
}

/// The symbol hit for the declaration holding one pattern match, tagged `content` as a
/// project declaration holding a match is.
fn declaration_hit(
    package: &PackageIdentity,
    unit: &str,
    declaration: &PackagePatternDeclaration,
) -> Result<rift_protocol::read::SearchHit, RiftError> {
    let source_bytes_max = usize::try_from(rift_protocol::configuration::GLOBAL_SOURCE_BYTES_MAX)
        .map_err(|source| invalid_source("source", source))?;
    crate::validate_canonical_hit(
        package,
        &declaration.symbol,
        HitLocation {
            unit,
            range: &declaration.range,
            line: declaration.line,
            source: declaration.source.as_deref(),
        },
        None,
        source_bytes_max,
    )?;
    let line =
        u64::try_from(declaration.line).map_err(|source| invalid_source("location", source))?;
    Ok(rift_protocol::read::SearchHit {
        view: None,
        hit: rift_protocol::read::SearchHitTarget::Symbol {
            symbol: Box::new(convert_symbol(&declaration.symbol)?),
        },
        score: None,
        matched_by: vec![rift_protocol::read::MatchedField::Content],
        source: declaration.source.clone(),
        range: Some(text_range(&declaration.range)),
        line: Some(line),
        path: None,
        unit: Some(protocol_unit(unit)),
        traversal_path: None,
        distance: None,
        change: None,
    })
}

impl TryFrom<PackageSearchHit> for PackageSearchCandidate {
    type Error = RiftError;

    fn try_from(value: PackageSearchHit) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<PackageSearchItem> for PackageSearchCandidate {
    type Error = RiftError;

    fn try_from(value: PackageSearchItem) -> Result<Self, Self::Error> {
        match value {
            PackageSearchItem::Search(hit) => Self::try_from(*hit),
            PackageSearchItem::Documentation(hit) => documentation_candidate(*hit),
            PackageSearchItem::Symbol(hit) => canonical_candidate(*hit),
        }
    }
}

impl TryFrom<&PackageSearchHit> for PackageSearchCandidate {
    type Error = RiftError;

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
            .map_err(|source| invalid_source("symbol_identity", source))?;
        let match_class = crate::search_match_class(&value.match_class)?;
        let hit = rift_protocol::read::SearchHit {
            view: None,
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
            package: Some(package),
            identity,
            hit,
            match_class,
        })
    }
}

fn documentation_candidate(
    value: PackageDocumentationHit,
) -> Result<PackageSearchCandidate, RiftError> {
    if value.target != "documentation" {
        return Err(invalid("target"));
    }
    let documentation = protocol_documentation_hit(value.documentation)?;
    let package = package_identity(&value.package);
    rift_analysis::documentation::validate_documentation_hit(&documentation)
        .map_err(|source| invalid_source("documentation", source))?;
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
            .map_err(|source| invalid_source("documentation.identity", source))?;
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
        view: None,
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
        package: Some(package),
        identity,
        hit,
        match_class: None,
    })
}

fn canonical_candidate(
    hit: rift_protocol::read::SearchHit,
) -> Result<PackageSearchCandidate, RiftError> {
    use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
    let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &hit.hit else {
        return Err(invalid("target"));
    };
    let id = symbol.id.as_ref().ok_or_else(|| invalid("symbol.id"))?;
    let parsed =
        SymbolIdentity::parse(&id.0).map_err(|source| invalid_source("symbol.id", source))?;
    if !symbol.name_is_public() || parsed.language() != &symbol.language {
        return Err(invalid("symbol"));
    }
    let package = match parsed.owner() {
        SymbolOwner::Package {
            manager,
            registry,
            name,
            version,
        } => {
            let package = rift_protocol::read::PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: version.clone(),
            };
            if symbol.origin.package.as_ref() != Some(&package) || symbol.origin.runtime.is_some() {
                return Err(invalid("symbol.origin"));
            }
            Some(package)
        }
        SymbolOwner::Runtime { runtime, version } => {
            if symbol.origin.package.is_some()
                || symbol
                    .origin
                    .runtime
                    .as_ref()
                    .is_none_or(|owner| &owner.runtime != runtime || &owner.version != version)
            {
                return Err(invalid("symbol.origin"));
            }
            None
        }
        SymbolOwner::Local
        | SymbolOwner::NamedLocal { .. }
        | SymbolOwner::ProfileLocal { .. }
        | SymbolOwner::NamedProfileLocal { .. } => {
            return Err(invalid("symbol.id"));
        }
    };
    let identity = rift_ranking::DocumentIdentity::new(id.0.clone())
        .map_err(|source| invalid_source("symbol.id", source))?;
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
) -> Result<String, RiftError> {
    let parsed = rift_core::SourceUnitId::parse(&unit.0)
        .map_err(|source| invalid_source("source_identity", source))?;
    let owner = package
        .owner()
        .map_err(|source| invalid_source("source_identity", source))?;
    if parsed.source_owner() != Some(&owner) {
        return Err(invalid("source_identity"));
    }
    Ok(unit.0.clone())
}

fn protocol_documentation_hit(
    value: crate::DocumentationHit,
) -> Result<rift_protocol::documentation::DocumentationHit, RiftError> {
    Ok(rift_protocol::documentation::DocumentationHit {
        block: protocol_documentation_block(value.block)?,
        source: protocol_documentation_source(value.source)?,
        documentation_revision: rift_protocol::read::Digest(value.documentation_revision),
    })
}

fn protocol_documentation_block(
    value: crate::DocumentationBlock,
) -> Result<rift_protocol::documentation::DocumentationBlock, RiftError> {
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
) -> Result<rift_protocol::documentation::DocumentationSource, RiftError> {
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

fn protocol_documentation_license(
    value: crate::DocumentationLicense,
) -> Result<rift_protocol::documentation::DocumentationLicense, RiftError> {
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
            .collect::<Result<_, RiftError>>()?,
    })
}

fn protocol_content_identity(
    value: crate::DocumentationContentIdentity,
) -> Result<rift_protocol::documentation::DocumentationContentIdentity, RiftError> {
    use rift_protocol::documentation as protocol;
    let source = match value.source {
        crate::DocumentationSourceIdentity::ProjectPath(project) => {
            protocol::DocumentationSourceIdentity::Project {
                path: protocol_project_path(project.path, "documentation.source.path")?,
            }
        }
        crate::DocumentationSourceIdentity::SourceUnitId(unit) => {
            rift_core::SourceUnitId::parse(&unit.unit)
                .map_err(|source| invalid_source("documentation.source.unit", source))?;
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

pub(crate) fn validate_documentation_context(
    context: &rift_protocol::documentation::DocumentationContext,
    target: &rift_protocol::read::SymbolId,
    package: &rift_protocol::read::PackageIdentity,
) -> Result<(), RiftError> {
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
) -> Result<(), RiftError> {
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
) -> Result<(String, rift_core::SourceUnitId, u64), RiftError> {
    let packages = HashSet::from([crate::package_key(package)]);
    let source_bytes_max = usize::try_from(rift_protocol::configuration::GLOBAL_SOURCE_BYTES_MAX)
        .map_err(|source| invalid_source("source", source))?;
    let qualified_name = crate::validate_hit_common(
        package,
        symbol,
        HitLocation {
            unit,
            range,
            line,
            source,
        },
        Some(&packages),
        source_bytes_max,
    )?;
    let source_unit = rift_core::SourceUnitId::parse(unit)
        .map_err(|source| invalid_source("source_identity", source))?;
    let line = u64::try_from(line).map_err(|source| invalid_source("location", source))?;
    Ok((qualified_name, source_unit, line))
}

pub(crate) fn package_identity(value: &PackageIdentity) -> rift_protocol::read::PackageIdentity {
    rift_protocol::read::PackageIdentity {
        manager: value.manager.clone(),
        registry: value.registry.clone(),
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
) -> Result<rift_protocol::read::ProjectPath, RiftError> {
    let validated =
        rift_core::ProjectPath::new(value).map_err(|source| invalid_source(field, source))?;
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
            PackageSearchHitContributingField::FileContent => {
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

fn convert_symbol(value: &Symbol) -> Result<rift_protocol::read::Symbol, RiftError> {
    Ok(rift_protocol::read::Symbol {
        id: value.id.clone().map(rift_protocol::read::SymbolId),
        language: language(&value.language)?,
        name: value.name.clone(),
        kind: rift_protocol::read::ExactKind::try_from(value.kind.clone())
            .map_err(|source| invalid_source("kind", source))?,
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
            runtime: None,
            source_kind: rift_protocol::read::SourceKind::Authored,
        };
    };
    rift_protocol::read::SymbolOrigin {
        location: value.location.as_ref().map(convert_location),
        package: value.package.as_ref().map(package_identity),
        runtime: value
            .runtime
            .as_ref()
            .map(|runtime| rift_protocol::read::RuntimeIdentity {
                runtime: runtime.runtime.clone(),
                version: runtime.version.clone(),
            }),
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
) -> Result<rift_protocol::read::Signature, RiftError> {
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
) -> Result<rift_protocol::read::Parameter, RiftError> {
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
) -> Result<rift_protocol::read::TypeBinding, RiftError> {
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
) -> Result<rift_protocol::read::TypeExpression, RiftError> {
    Ok(rift_protocol::read::TypeExpression {
        language: language(&value.language)?,
        source: value.source.clone(),
        resolved: value.resolved.clone().map(rift_protocol::read::SymbolId),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn language(value: &str) -> Result<rift_protocol::read::Language, RiftError> {
    rift_protocol::read::Language::from_identity_segment(value)
        .map_err(|source| invalid_source("language", source))
}

fn convert_extensions(value: Option<&Value>) -> Result<rift_protocol::read::Extensions, RiftError> {
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

fn invalid(field: &'static str) -> RiftError {
    failure::response_invalid("invalid", field, "field failed validation", None)
}

fn invalid_source(
    field: &'static str,
    source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> RiftError {
    failure::response_source("invalid", field, "field failed validation", None, source)
}

#[cfg(test)]
mod tests {
    fn assert_response_field(error: &rift_error::RiftError, expected: &str) {
        assert_eq!(error.slug().as_str(), "rift.client.response_invalid");
        assert!(error.fields().iter().any(|field| {
            field.key() == "field"
                && matches!(field.value(), rift_error::ErrorValue::Display(value) if value == expected)
        }));
    }

    use std::collections::HashMap;

    use super::*;

    fn package() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "helper".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn symbol(package: &PackageIdentity) -> Symbol {
        let id =
            rift_core::symbol_identity("rust", "cargo/helper@1.0.0/src/lib.rs", "helper_beacon");
        Symbol {
            id: Some(id),
            language: "rust".to_owned(),
            name: "helper_beacon".to_owned(),
            kind: "function".to_owned(),
            origin: Some(SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                runtime: None,
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
        "rift://source/cargo/crates.io/helper@1.0.0/src/lib.rs".to_owned()
    }

    fn documentation() -> rift_protocol::documentation::DocumentationHit {
        serde_json::from_value(serde_json::json!({
            "documentation_revision":"0123abcd",
            "block": {
                "identity":"1".repeat(64), "content_digest":"2".repeat(64),
                "source":{"source":{"kind":"package","unit":"rift://source/cargo/crates.io/helper@1.0.0/README.md"}},
                "range":{"start":0,"end":40}, "line":1, "kind":"prose"
            },
            "source": {
                "identity":{"source":{"kind":"package","unit":"rift://source/cargo/crates.io/helper@1.0.0/README.md"}},
                "revision":"3".repeat(64), "content_digest":"4".repeat(64),
                "origin":{"location":"dependency","source_kind":"authored","package":{"manager":"cargo","registry":"crates.io","name":"helper","version":"1.0.0"}},
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
                        "rift://source/cargo/crates.io/other@1.0.0/README.md".to_owned(),
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

        assert_response_field(
            &(PackageSearchCandidate::try_from(item)).expect_err("registered refusal"),
            "target",
        );
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
                "rift://source/cargo/crates.io/other@1.0.0/README.md".to_owned(),
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
            registry: "crates.io".to_owned(),
            name: "other".to_owned(),
            version: "1.0.0".to_owned(),
        };

        assert_response_field(
            &(validate_documentation_context(&context, &target, &other_package))
                .expect_err("registered refusal"),
            "documentation",
        );
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

        assert_response_field(
            &(validate_documentation_context(&context, &target, &package))
                .expect_err("registered refusal"),
            "source_identity",
        );
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
                PackageSearchHitContributingField::Signature,
            ],
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: Some("pub fn helper_beacon() {}".to_owned()),
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSearchCandidate::try_from(hit).expect("valid package hit");
        assert_eq!(
            candidate.package.as_ref().expect("package owner").name,
            "helper"
        );
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

    /// A declaration matched inside its body reports `file_content` and the `unknown`
    /// match class: it answers as a content match claiming no identifier class.
    #[test]
    fn search_candidate_maps_a_body_match_to_content() {
        let package = package();
        let hit = PackageSearchHit {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            contributing_fields: vec![PackageSearchHitContributingField::FileContent],
            match_class: crate::generated::IdentifierMatchClass::Unknown,
            source: None,
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSearchCandidate::try_from(hit).expect("valid body match");
        assert_eq!(
            candidate.hit.matched_by,
            [rift_protocol::read::MatchedField::Content]
        );
        assert_eq!(candidate.match_class, None);
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
        let hit = PackageSearchHit {
            package,
            symbol,
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::QualifiedExact,
            source: None,
            contributing_fields: vec![PackageSearchHitContributingField::QualifiedName],
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSearchCandidate::try_from(hit).expect("valid package symbol");
        assert_eq!(
            candidate.match_class,
            Some(rift_ranking::IdentifierMatchClass::QualifiedExact)
        );
        let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &candidate.hit.hit else {
            panic!("symbol candidate");
        };
        assert_eq!(
            symbol.id.as_ref().expect("id").0,
            rift_core::symbol_identity("rust", "cargo/helper@1.0.0/src/lib.rs", "helper_beacon")
        );
        assert_eq!(symbol.documentation[0].text, "Beacon docs");
        assert_eq!(
            symbol.signatures[0].parameters[0].name.as_deref(),
            Some("value")
        );
        assert_eq!(symbol.extensions.0.len(), 1);
    }

    #[test]
    fn candidate_rejects_a_malformed_kind() {
        let package = package();
        let mut symbol = symbol(&package);
        symbol.kind = "9struct".to_owned();
        let hit = PackageSearchHit {
            package,
            symbol,
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: None,
            contributing_fields: vec![PackageSearchHitContributingField::QualifiedName],
            additional_properties: HashMap::new(),
        };
        assert_response_field(
            &PackageSearchCandidate::try_from(hit).expect_err("registered refusal"),
            "kind",
        );
    }

    #[test]
    fn candidate_rejects_invalid_source_identity() {
        let package = package();
        let mut hit = PackageSearchHit {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: None,
            contributing_fields: vec![PackageSearchHitContributingField::QualifiedName],
            additional_properties: HashMap::new(),
        };
        hit.unit = "rift://source/cargo/crates.io/other@1.0.0/src/lib.rs".to_owned();
        assert_response_field(
            &(PackageSearchCandidate::try_from(hit)).expect_err("registered refusal"),
            "source_identity",
        );
    }
}
