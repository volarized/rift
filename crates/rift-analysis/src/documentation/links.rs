//! Resolves authored local destinations without fetching source bytes.

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;
use rift_protocol::documentation::{
    DocumentationBlock, DocumentationContentIdentity, DocumentationDigest, DocumentationLink,
    DocumentationLinkResolution, DocumentationSource, DocumentationSourceIdentity,
    DocumentationTarget, DocumentationUnresolvedReason,
};
use rift_protocol::read::{ProjectPath, SourceUnitId, TextRange};
use url::Url;

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::input::source_path;

/// One explicit fragment supplied by a format parser, never a generated heading slug.
#[derive(Clone, Debug)]
pub struct DocumentationFragment {
    /// Content owner containing this target.
    pub source: DocumentationContentIdentity,
    /// Authored fragment name.
    pub name: String,
    /// Exact target range.
    pub range: TextRange,
}

/// Resolves bounded authored destinations against the accepted source set.
///
/// `url::Url::join` handles relative URL references. Decoded paths then pass the
/// existing project-path validator; a synthetic root prevents references from
/// escaping the project or exact package. No URL is fetched.
///
/// # Errors
///
/// Refuses invalid input identities or collections beyond protocol bounds.
pub fn resolve_links(
    sources: &[DocumentationSource],
    blocks: &[DocumentationBlock],
    links: &mut [DocumentationLink],
    fragments: &[DocumentationFragment],
) -> Result<(), DocumentationError> {
    use rift_protocol::documentation::{
        DOCUMENTATION_BLOCKS_MAX, DOCUMENTATION_REFERENCES_MAX, DOCUMENTATION_SOURCES_MAX,
    };
    if sources.len() > DOCUMENTATION_SOURCES_MAX as usize
        || blocks.len() > DOCUMENTATION_BLOCKS_MAX as usize
        || links.len() > DOCUMENTATION_REFERENCES_MAX as usize
        || fragments.len() > DOCUMENTATION_REFERENCES_MAX as usize
    {
        return Err(refused(DocumentationViolation::LimitExceeded, "links"));
    }
    let sources: BTreeMap<_, _> = sources
        .iter()
        .map(|source| (&source.identity, source))
        .collect();
    let blocks: BTreeMap<_, _> = blocks
        .iter()
        .map(|block| (&block.identity, block))
        .collect();
    let fragments = fragment_index(fragments)?;
    for link in links {
        let block = blocks
            .get(&link.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "link.block"))?;
        let source = sources
            .get(&block.source)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.source"))?;
        link.resolution = resolve_destination(source, &link.authored, &sources, &fragments)?;
    }
    Ok(())
}

pub(super) type Fragments<'a> =
    BTreeMap<(&'a DocumentationContentIdentity, &'a str), Option<&'a TextRange>>;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "collector")]
pub(super) enum DeclarationLinkMatch {
    Symbol(rift_protocol::read::SymbolId),
    Ambiguous,
    Missing,
}

#[cfg(feature = "collector")]
pub(super) struct DeclarationLinkNames<'declaration> {
    symbols: BTreeMap<&'declaration str, &'declaration rift_protocol::read::SymbolId>,
    qualified: BTreeMap<
        (&'declaration DocumentationContentIdentity, String),
        Option<&'declaration rift_protocol::read::SymbolId>,
    >,
}

#[cfg(feature = "collector")]
impl<'declaration> DeclarationLinkNames<'declaration> {
    pub(super) fn new(
        declarations: &'declaration [super::DocumentationDeclaration<'_>],
    ) -> Result<Self, DocumentationError> {
        let symbols = declarations
            .iter()
            .map(|declaration| (declaration.symbol().0.as_str(), declaration.symbol()))
            .collect();
        let mut qualified = BTreeMap::new();
        for declaration in declarations {
            let parsed = rift_core::parse_symbol_identity(&declaration.symbol().0)
                .map_err(|_| refused(DocumentationViolation::Identity, "declaration.symbol"))?;
            qualified
                .entry((declaration.source(), parsed.qualified_name().to_owned()))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(declaration.symbol()));
        }
        Ok(Self { symbols, qualified })
    }

    pub(super) fn direct(
        &self,
        authored: &str,
    ) -> Option<&'declaration rift_protocol::read::SymbolId> {
        self.symbols.get(authored).copied()
    }

    pub(super) fn qualified(
        &self,
        source: &DocumentationContentIdentity,
        name: &str,
    ) -> DeclarationLinkMatch {
        match self.qualified.get(&(source, name.to_owned())) {
            Some(Some(symbol)) => DeclarationLinkMatch::Symbol((*symbol).clone()),
            Some(None) => DeclarationLinkMatch::Ambiguous,
            None => DeclarationLinkMatch::Missing,
        }
    }

    pub(super) fn lookup(
        &self,
        source: &DocumentationSource,
        authored: &str,
    ) -> Result<DeclarationLinkMatch, DocumentationError> {
        if let Some(symbol) = self.direct(authored) {
            return Ok(DeclarationLinkMatch::Symbol(symbol.clone()));
        }
        let Destination::Local {
            identity,
            fragment: Some(fragment),
        } = local_destination(source, authored)?
        else {
            return Ok(DeclarationLinkMatch::Missing);
        };
        let Ok(fragment) = percent_decode_str(&fragment).decode_utf8() else {
            return Ok(DeclarationLinkMatch::Missing);
        };
        Ok(self.qualified(&identity, &fragment))
    }
}

/// Resolves explicit symbol addresses and exact declaration fragments against supplied facts.
/// No spelling is inferred from prose or a generated heading fragment.
#[cfg(feature = "collector")]
pub(super) fn resolve_declaration_links(
    sources: &[DocumentationSource],
    blocks: &[DocumentationBlock],
    links: &mut [DocumentationLink],
    declarations: &[super::DocumentationDeclaration<'_>],
) -> Result<Vec<rift_protocol::documentation::DocumentationReference>, DocumentationError> {
    use rift_protocol::documentation::{DocumentationReference, DocumentationReferenceEvidence};
    if declarations.len() > rift_protocol::index::PACKAGE_SYMBOLS_MAX as usize {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "declarations",
        ));
    }
    let sources: BTreeMap<_, _> = sources
        .iter()
        .map(|source| (&source.identity, source))
        .collect();
    let blocks: BTreeMap<_, _> = blocks
        .iter()
        .map(|block| (&block.identity, block))
        .collect();
    let names = DeclarationLinkNames::new(declarations)?;
    let mut references = Vec::new();
    let mut occurrences = BTreeMap::new();
    for link in links {
        let block = blocks
            .get(&link.block)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "link.block"))?;
        let source = sources
            .get(&block.source)
            .ok_or_else(|| refused(DocumentationViolation::MissingTarget, "block.source"))?;
        let symbol = match names.lookup(source, &link.authored)? {
            DeclarationLinkMatch::Symbol(symbol) => symbol,
            DeclarationLinkMatch::Ambiguous => {
                link.resolution = unresolved(DocumentationUnresolvedReason::Ambiguous);
                continue;
            }
            DeclarationLinkMatch::Missing => continue,
        };
        link.resolution = DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Symbol {
                symbol: symbol.clone(),
            },
        };
        let evidence = DocumentationReferenceEvidence::AuthoredLink;
        let key = (
            link.block.clone(),
            symbol.clone(),
            evidence,
            link.authored.clone(),
        );
        let ordinal = occurrences.entry(key.clone()).or_insert(0_u32);
        let identity = super::identity::canonical_digest(&(key, *ordinal))?;
        *ordinal += 1;
        references.push(DocumentationReference {
            identity,
            block: link.block.clone(),
            target: symbol,
            range: link.range.clone(),
            authored: link.authored.clone(),
            evidence,
        });
    }
    Ok(references)
}

pub(super) fn fragment_index(
    fragments: &[DocumentationFragment],
) -> Result<Fragments<'_>, DocumentationError> {
    let mut index = BTreeMap::new();
    for fragment in fragments {
        super::input::validate_identity(&fragment.source)?;
        let valid_name = !fragment.name.is_empty()
            && fragment.name.len()
                <= rift_protocol::documentation::DOCUMENTATION_TEXT_BYTES_MAX as usize;
        if !valid_name || fragment.range.end <= fragment.range.start {
            return Err(refused(DocumentationViolation::Range, "fragment"));
        }
        index
            .entry((&fragment.source, fragment.name.as_str()))
            .and_modify(|entry| *entry = None)
            .or_insert(Some(&fragment.range));
    }
    Ok(index)
}

fn unresolved(reason: DocumentationUnresolvedReason) -> DocumentationLinkResolution {
    DocumentationLinkResolution::Unresolved { reason }
}

fn resolve_destination(
    source: &DocumentationSource,
    authored: &str,
    sources: &BTreeMap<&DocumentationContentIdentity, &DocumentationSource>,
    fragments: &Fragments<'_>,
) -> Result<DocumentationLinkResolution, DocumentationError> {
    let (identity, fragment) = match local_destination(source, authored)? {
        Destination::Local { identity, fragment } => (identity, fragment),
        Destination::Unresolved(reason) => return Ok(unresolved(reason)),
    };
    let Some(record) = sources.get(&identity) else {
        return Ok(unresolved(DocumentationUnresolvedReason::Missing));
    };
    let range = match fragment.as_deref() {
        None | Some("") => TextRange {
            start: 0,
            end: record.byte_length,
        },
        Some(fragment) => return Ok(resolve_fragment(record, fragment, fragments)),
    };
    Ok(DocumentationLinkResolution::Resolved {
        target: DocumentationTarget::Source {
            source: identity,
            range,
        },
    })
}

pub(super) enum Destination {
    Local {
        identity: DocumentationContentIdentity,
        fragment: Option<String>,
    },
    Unresolved(DocumentationUnresolvedReason),
}

pub(super) fn local_destination(
    source: &DocumentationSource,
    authored: &str,
) -> Result<Destination, DocumentationError> {
    use DocumentationUnresolvedReason as Reason;
    let authored_valid = !authored.is_empty()
        && authored.len() <= rift_protocol::documentation::DOCUMENTATION_TEXT_BYTES_MAX as usize
        && !authored.chars().any(char::is_control);
    if !authored_valid {
        return Ok(Destination::Unresolved(Reason::Invalid));
    }
    if authored.contains('\\') {
        return Ok(Destination::Unresolved(Reason::Invalid));
    }
    if Url::parse(authored).is_ok() || authored.starts_with("//") {
        return Ok(Destination::Unresolved(Reason::External));
    }
    let base = source_base(source)?;
    let Ok(target) = base.join(authored) else {
        return Ok(Destination::Unresolved(Reason::Invalid));
    };
    let local_origin = target.scheme() == base.scheme()
        && target.host_str() == base.host_str()
        && target.port() == base.port()
        && target.username().is_empty()
        && target.password().is_none();
    if !local_origin {
        return Ok(Destination::Unresolved(Reason::External));
    }
    if target.query().is_some() {
        return Ok(Destination::Unresolved(Reason::Invalid));
    }
    let Some(encoded) = target.path().strip_prefix("/root/") else {
        return Ok(Destination::Unresolved(Reason::Invalid));
    };
    let Ok(decoded) = percent_decode_str(encoded).decode_utf8() else {
        return Ok(Destination::Unresolved(Reason::Invalid));
    };
    let Ok(path) = rift_core::ProjectPath::new(decoded.as_ref()) else {
        return Ok(Destination::Unresolved(Reason::Invalid));
    };
    if path.as_str().is_empty() {
        return Ok(Destination::Unresolved(Reason::Invalid));
    }
    let identity = target_identity(source, &path)?;
    Ok(Destination::Local {
        identity,
        fragment: target.fragment().map(str::to_owned),
    })
}

fn resolve_fragment(
    source: &DocumentationSource,
    fragment: &str,
    fragments: &Fragments<'_>,
) -> DocumentationLinkResolution {
    let Ok(name) = percent_decode_str(fragment).decode_utf8() else {
        return unresolved(DocumentationUnresolvedReason::Invalid);
    };
    match fragments.get(&(&source.identity, name.as_ref())) {
        Some(Some(range)) if range.end <= source.byte_length => {
            DocumentationLinkResolution::Resolved {
                target: DocumentationTarget::Source {
                    source: source.identity.clone(),
                    range: (*range).clone(),
                },
            }
        }
        Some(None) => unresolved(DocumentationUnresolvedReason::Ambiguous),
        _ => unresolved(DocumentationUnresolvedReason::Fragment),
    }
}

fn source_base(source: &DocumentationSource) -> Result<Url, DocumentationError> {
    let mut base = Url::parse("https://rift.invalid/root/")
        .map_err(|_| refused(DocumentationViolation::Identity, "source.base"))?;
    let full_path = source_path(&source.identity)?;
    let path = match &source.identity.source {
        DocumentationSourceIdentity::Project { .. } => full_path.as_str(),
        DocumentationSourceIdentity::Package { .. } => {
            let package = source
                .origin
                .package
                .as_ref()
                .ok_or_else(|| refused(DocumentationViolation::Origin, "origin.package"))?;
            full_path
                .strip_prefix(&format!("{}@{}/", package.name, package.version))
                .ok_or_else(|| refused(DocumentationViolation::Origin, "origin.package"))?
        }
    };
    base.path_segments_mut()
        .map_err(|()| refused(DocumentationViolation::Identity, "source.base"))?
        .pop_if_empty()
        .extend(path.split('/'));
    Ok(base)
}

fn target_identity(
    source: &DocumentationSource,
    path: &rift_core::ProjectPath,
) -> Result<DocumentationContentIdentity, DocumentationError> {
    let target = match &source.identity.source {
        DocumentationSourceIdentity::Project { .. } => DocumentationSourceIdentity::Project {
            path: ProjectPath(path.to_string()),
        },
        DocumentationSourceIdentity::Package { .. } => {
            let package = source
                .origin
                .package
                .as_ref()
                .ok_or_else(|| refused(DocumentationViolation::Origin, "origin.package"))?;
            let unit = rift_core::SourceUnitId::for_package(package, path)
                .map_err(|_| refused(DocumentationViolation::Identity, "source.unit"))?;
            DocumentationSourceIdentity::Package {
                unit: SourceUnitId(unit.to_string()),
            }
        }
    };
    let cell = (target == source.identity.source)
        .then(|| source.identity.cell.clone())
        .flatten();
    Ok(DocumentationContentIdentity {
        source: target,
        cell,
    })
}

/// Returns source blocks whose authored links may change after sources or fragments change.
///
/// The caller may rerun this bounded resolution pass without repeating syntax extraction.
#[must_use]
pub fn linked_blocks(links: &[DocumentationLink]) -> BTreeMap<&str, Vec<&DocumentationDigest>> {
    let mut reverse: BTreeMap<&str, Vec<&DocumentationDigest>> = BTreeMap::new();
    for link in links {
        let blocks = reverse.entry(&link.authored).or_default();
        blocks.push(&link.block);
    }
    for blocks in reverse.values_mut() {
        blocks.sort();
        blocks.dedup();
    }
    reverse
}
