//! Exact declaration matching for authored inline code candidates.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use rift_core::symbol_identity;
use rift_protocol::documentation::{
    DOCUMENTATION_REFERENCES_CEILING, DOCUMENTATION_TEXT_BYTES_CEILING,
    DocumentationContentIdentity, DocumentationReference, DocumentationReferenceCandidate,
    DocumentationReferenceEvidence, DocumentationSourceIdentity, DocumentationUnresolvedReason,
};
use rift_protocol::index::PACKAGE_SYMBOLS_CEILING;
use rift_protocol::read::{Language, SymbolId, TextRange};

use super::identity::canonical_digest;
use super::input::{source_path, validate_identity};
use rift_error::{RiftError, errors};

/// One declaration spelling and the owner and language established by the canonical codec.
#[derive(Debug)]
pub struct DocumentationSymbol {
    wire: SymbolId,
    scope: Option<(rift_protocol::identity::SymbolOwner, Language)>,
}

impl DocumentationSymbol {
    /// Reads the canonical scope once. Legacy spellings retain their original wire value.
    #[must_use]
    pub fn new(wire: SymbolId) -> Self {
        let scope = rift_protocol::identity::SymbolIdentity::parse(&wire.0)
            .ok()
            .map(|id| (id.owner().clone(), id.language().clone()));
        Self { wire, scope }
    }

    /// Returns the unmodified wire identity.
    #[must_use]
    pub const fn wire(&self) -> &SymbolId {
        &self.wire
    }

    /// Returns the canonical owner and language when established.
    #[must_use]
    pub fn scope(&self) -> Option<(&rift_protocol::identity::SymbolOwner, &Language)> {
        self.scope
            .as_ref()
            .map(|(owner, language)| (owner, language))
    }

    /// Validates declaration facts using this spelling's established canonical scope.
    ///
    /// # Errors
    /// Returns the same source, name, identity or range refusal as raw declaration admission.
    pub fn declaration<'a>(
        &'a self,
        language: &'a Language,
        name: &'a str,
        qualified_name: &'a str,
        source: &'a DocumentationContentIdentity,
        range: TextRange,
    ) -> Result<DocumentationDeclaration<'a>, RiftError> {
        validate_declaration(
            &self.wire,
            language,
            name,
            qualified_name,
            source,
            &range,
            self.scope().map(|(_, language)| language),
        )?;
        Ok(DocumentationDeclaration {
            symbol: &self.wire,
            language,
            name,
            qualified_name,
            source: Cow::Borrowed(source),
            range,
        })
    }
}

/// A borrowed declaration's identity and name-resolution facts.
#[derive(Clone, Debug)]
pub struct DocumentationDeclaration<'declaration> {
    symbol: &'declaration SymbolId,
    language: &'declaration Language,
    name: &'declaration str,
    qualified_name: &'declaration str,
    source: Cow<'declaration, DocumentationContentIdentity>,
    range: TextRange,
}

impl<'declaration> DocumentationDeclaration<'declaration> {
    /// Validates a declaration against its canonical source and qualified name.
    ///
    /// # Errors
    ///
    /// Returns a typed refusal for invalid names, ranges, or a mismatched symbol identity.
    pub fn new(
        symbol: &'declaration SymbolId,
        language: &'declaration Language,
        name: &'declaration str,
        qualified_name: &'declaration str,
        source: &DocumentationContentIdentity,
        range: TextRange,
    ) -> Result<Self, RiftError> {
        validate_declaration(symbol, language, name, qualified_name, source, &range, None)?;
        Ok(Self {
            symbol,
            language,
            name,
            qualified_name,
            source: Cow::Owned(source.clone()),
            range,
        })
    }

    /// Returns the exact declaration identity.
    #[must_use]
    pub const fn symbol(&self) -> &SymbolId {
        self.symbol
    }

    #[cfg(feature = "collector")]
    pub(super) const fn qualified_name(&self) -> &str {
        self.qualified_name
    }

    /// Returns the source containing the declaration.
    #[must_use]
    pub const fn source(&self) -> &DocumentationContentIdentity {
        match &self.source {
            Cow::Borrowed(source) => source,
            Cow::Owned(source) => source,
        }
    }

    /// Returns the declaration range in its source.
    #[must_use]
    pub const fn range(&self) -> &TextRange {
        &self.range
    }
}

fn validate_declaration(
    symbol: &SymbolId,
    language: &Language,
    name: &str,
    qualified_name: &str,
    source: &DocumentationContentIdentity,
    range: &TextRange,
    canonical_language: Option<&Language>,
) -> Result<(), RiftError> {
    validate_identity(source)?;
    let accepted_name = |text: &str| {
        !text.is_empty()
            && text.len() <= DOCUMENTATION_TEXT_BYTES_CEILING as usize
            && !text.chars().any(char::is_control)
    };
    if !accepted_name(name) || !accepted_name(qualified_name) {
        return errors::analysis::documentation_identity_invalid()
            .field("declaration.name")
            .fail();
    }
    let accepted_symbol = if let Some(known) = canonical_language {
        known == language
    } else if let Ok(identity) = rift_protocol::identity::SymbolIdentity::parse(&symbol.0) {
        identity.language() == language
    } else {
        let path = declaration_path(source)?;
        symbol.0 == symbol_identity(&language.identity_segment(), &path, qualified_name)
    };
    if !accepted_symbol {
        return errors::analysis::documentation_identity_invalid()
            .field("declaration.symbol")
            .fail();
    }
    if range.end < range.start {
        return errors::analysis::documentation_range_invalid()
            .field("declaration.range")
            .fail();
    }
    Ok(())
}

pub(super) fn declaration_path(source: &DocumentationContentIdentity) -> Result<String, RiftError> {
    let path = source_path(source)?;
    match &source.source {
        DocumentationSourceIdentity::Project { .. } => Ok(path),
        DocumentationSourceIdentity::Package { unit } => {
            let parsed = rift_core::SourceUnitId::parse(&unit.0).map_err(|_| {
                errors::analysis::documentation_identity_invalid()
                    .field("source.unit")
                    .error()
            })?;
            parsed
                .to_string()
                .strip_prefix("rift://source/")
                .map(str::to_owned)
                .ok_or_else(|| {
                    errors::analysis::documentation_identity_invalid()
                        .field("source.unit")
                        .error()
                })
        }
    }
}

/// Exact references and unresolved candidates from one bounded resolution pass.
#[derive(Debug)]
pub struct ResolvedDocumentationReferences {
    references: Vec<DocumentationReference>,
    unresolved: Vec<DocumentationReferenceCandidate>,
}

impl ResolvedDocumentationReferences {
    /// Returns exact declaration references in evidence and block order.
    #[must_use]
    pub fn references(&self) -> &[DocumentationReference] {
        &self.references
    }

    /// Returns unresolved candidates for incremental invalidation.
    #[must_use]
    pub fn unresolved(&self) -> &[DocumentationReferenceCandidate] {
        &self.unresolved
    }

    /// Takes the resolved and unresolved records for publication.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<DocumentationReference>,
        Vec<DocumentationReferenceCandidate>,
    ) {
        (self.references, self.unresolved)
    }
}

/// Resolves inline code through exact qualified names and unique bare names.
///
/// The caller supplies candidates extracted from inline code, never ordinary prose.
/// Work is O((declarations + candidates) log declarations), bounded by publication limits.
/// Language filters use the same index as unrestricted matching.
///
/// # Errors
///
/// Returns a typed refusal for oversized input, invalid candidates, or canonical encoding failure.
pub fn resolve_references(
    declarations: &[DocumentationDeclaration<'_>],
    candidates: &[DocumentationReferenceCandidate],
) -> Result<ResolvedDocumentationReferences, RiftError> {
    validate_reference_counts(declarations.len(), candidates.len())?;
    let names = candidates
        .iter()
        .filter(|candidate| candidate.authored.len() <= DOCUMENTATION_TEXT_BYTES_CEILING as usize)
        .map(|candidate| candidate.authored.as_str())
        .collect();
    let index = DeclarationNames::requested(declarations, &names);
    resolve_indexed_references(&index, candidates)
}

pub(super) fn resolve_indexed_references(
    index: &DeclarationNames<'_>,
    candidates: &[DocumentationReferenceCandidate],
) -> Result<ResolvedDocumentationReferences, RiftError> {
    let mut references = Vec::new();
    let mut unresolved = Vec::new();
    let mut occurrences = BTreeMap::new();
    for candidate in candidates {
        validate_candidate(candidate)?;
        match index.resolve(candidate) {
            Ok((symbol, evidence)) => {
                let key = (&candidate.block, symbol, evidence, &candidate.authored);
                let ordinal = occurrences.entry(key).or_insert(0_u32);
                let identity = canonical_digest(&(key, *ordinal))?;
                *ordinal += 1;
                references.push(DocumentationReference {
                    identity,
                    block: candidate.block.clone(),
                    target: symbol.clone(),
                    range: candidate.range.clone(),
                    authored: candidate.authored.clone(),
                    evidence,
                });
            }
            Err(reason) => {
                let mut candidate = candidate.clone();
                candidate.reason = reason;
                unresolved.push(candidate);
            }
        }
    }
    references.sort_by(|left, right| {
        (
            &left.evidence,
            &left.block,
            left.range.start,
            &left.identity,
        )
            .cmp(&(
                &right.evidence,
                &right.block,
                right.range.start,
                &right.identity,
            ))
    });
    Ok(ResolvedDocumentationReferences {
        references,
        unresolved,
    })
}

pub(super) fn validate_reference_counts(
    declarations: usize,
    candidates: usize,
) -> Result<(), RiftError> {
    if declarations > PACKAGE_SYMBOLS_CEILING as usize
        || candidates > DOCUMENTATION_REFERENCES_CEILING as usize
    {
        return errors::analysis::documentation_limit_exceeded()
            .field("references")
            .fail();
    }
    Ok(())
}

pub(super) fn validate_candidate(
    candidate: &DocumentationReferenceCandidate,
) -> Result<(), RiftError> {
    let spelling_accepted = !candidate.authored.is_empty()
        && candidate.authored.len() <= DOCUMENTATION_TEXT_BYTES_CEILING as usize;
    let range_accepted = candidate.range.end > candidate.range.start;
    if !spelling_accepted || !range_accepted || !super::identity::is_digest(&candidate.block) {
        return errors::analysis::documentation_identity_invalid()
            .field("reference")
            .fail();
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum NameMatch<'declaration> {
    Unique(&'declaration SymbolId),
    Ambiguous,
}

type LanguageKey<'declaration> = (&'declaration str, Option<&'declaration str>);
type NameKey<'declaration> = (&'declaration str, Option<LanguageKey<'declaration>>);

pub(super) struct DeclarationNames<'declaration> {
    qualified: BTreeMap<NameKey<'declaration>, NameMatch<'declaration>>,
    bare: BTreeMap<NameKey<'declaration>, NameMatch<'declaration>>,
}

impl<'declaration> DeclarationNames<'declaration> {
    pub(super) fn requested(
        declarations: &'declaration [DocumentationDeclaration<'_>],
        names: &HashSet<&str>,
    ) -> Self {
        let mut index = Self {
            qualified: BTreeMap::new(),
            bare: BTreeMap::new(),
        };
        for declaration in declarations {
            let bare = names.contains(declaration.name);
            let qualified = declaration.qualified_name != declaration.name
                && names.contains(declaration.qualified_name);
            if bare || qualified {
                for language in [None, Some(language_key(declaration.language))] {
                    if bare {
                        add_name(
                            &mut index.bare,
                            (declaration.name, language),
                            declaration.symbol,
                        );
                    }
                    if qualified {
                        add_name(
                            &mut index.qualified,
                            (declaration.qualified_name, language),
                            declaration.symbol,
                        );
                    }
                }
            }
        }
        index
    }

    #[cfg(feature = "collector")]
    pub(super) fn bucket_count(&self) -> usize {
        self.bare.len() + self.qualified.len()
    }

    pub(super) fn resolve(
        &self,
        candidate: &DocumentationReferenceCandidate,
    ) -> Result<
        (&'declaration SymbolId, DocumentationReferenceEvidence),
        DocumentationUnresolvedReason,
    > {
        self.resolve_name(&candidate.authored, candidate.language.as_ref())
    }

    pub(super) fn resolve_name(
        &self,
        authored: &str,
        language: Option<&Language>,
    ) -> Result<
        (&'declaration SymbolId, DocumentationReferenceEvidence),
        DocumentationUnresolvedReason,
    > {
        let key = (authored, language.map(language_key));
        if let Some(found) = self.qualified.get(&key) {
            return exact_match(*found, DocumentationReferenceEvidence::QualifiedName);
        }
        self.bare
            .get(&key)
            .ok_or(DocumentationUnresolvedReason::Missing)
            .and_then(|found| exact_match(*found, DocumentationReferenceEvidence::UniqueName))
    }
}

fn language_key(language: &Language) -> LanguageKey<'_> {
    (language.name.as_str(), language.dialect.as_deref())
}

fn add_name<'declaration>(
    index: &mut BTreeMap<NameKey<'declaration>, NameMatch<'declaration>>,
    key: NameKey<'declaration>,
    symbol: &'declaration SymbolId,
) {
    index
        .entry(key)
        .and_modify(|found| match found {
            NameMatch::Unique(previous) if *previous != symbol => *found = NameMatch::Ambiguous,
            _ => {}
        })
        .or_insert(NameMatch::Unique(symbol));
}

fn exact_match(
    found: NameMatch<'_>,
    evidence: DocumentationReferenceEvidence,
) -> Result<(&SymbolId, DocumentationReferenceEvidence), DocumentationUnresolvedReason> {
    match found {
        NameMatch::Unique(symbol) => Ok((symbol, evidence)),
        NameMatch::Ambiguous => Err(DocumentationUnresolvedReason::Ambiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::{DocumentationDeclaration, resolve_references};
    use crate::documentation::{DocumentationViolation, content_digest};
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationReferenceCandidate,
        DocumentationReferenceEvidence, DocumentationSourceIdentity, DocumentationUnresolvedReason,
    };
    use rift_protocol::read::{Language, ProjectPath, SymbolId, TextRange};

    fn source(path: &str) -> DocumentationContentIdentity {
        DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath(path.to_owned()),
            },
            cell: None,
        }
    }

    fn language() -> Language {
        Language {
            name: "rust".to_owned(),
            dialect: None,
        }
    }

    fn identity(language: &str, namespace: &str, qualified_name: &str) -> SymbolId {
        let mut path = vec![namespace.to_owned()];
        path.extend(qualified_name.split("::").map(str::to_owned));
        let identity = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            Language {
                name: language.to_owned(),
                dialect: None,
            },
            path,
        )
        .expect("canonical fixture identity");
        SymbolId::parse(&identity.wire_identity()).expect("canonical symbol")
    }

    fn candidate(spelling: &str, start: u64) -> DocumentationReferenceCandidate {
        DocumentationReferenceCandidate {
            block: content_digest(b"README.md:Usage:0"),
            range: TextRange {
                start,
                end: start + spelling.len() as u64,
            },
            authored: spelling.to_owned(),
            language: None,
            reason: DocumentationUnresolvedReason::Missing,
        }
    }

    #[test]
    fn canonical_documentation_symbol_matches_raw_identity_name_and_range_refusals() {
        let rust = language();
        let python = Language::from_identity_segment("python").expect("Python");
        let unit = source("src/lib.rs");
        let canonical = identity("rust", "app", "Café");
        for (wire, language, name, start, end, accepted) in [
            (canonical.clone(), &rust, "Café", 0, 5, true),
            (canonical.clone(), &python, "Café", 0, 5, false),
            (canonical.clone(), &rust, "", 0, 5, false),
            (canonical, &rust, "Café", 7, 5, false),
            (
                SymbolId("rift://symbol/local/rust/app/%GG".into()),
                &rust,
                "Café",
                0,
                5,
                false,
            ),
        ] {
            let symbol = super::DocumentationSymbol::new(wire.clone());
            let raw = DocumentationDeclaration::new(
                &wire,
                language,
                name,
                name,
                &unit,
                TextRange { start, end },
            );
            let typed = symbol.declaration(language, name, name, &unit, TextRange { start, end });
            assert_eq!(raw.is_ok(), accepted);
            match (raw, typed) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.symbol(), b.symbol());
                    assert_eq!(a.source(), b.source());
                    assert!(std::ptr::eq(b.source(), &raw const unit));
                    assert_eq!(a.range(), b.range());
                }
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                _ => panic!("raw and canonical admission differed"),
            }
        }
    }

    #[test]
    fn test_released_declaration_path_keeps_registry_and_runtime_owners() {
        use rift_protocol::identity::SymbolOwner;

        let owners = [
            SymbolOwner::Package {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "beacon".to_owned(),
            },
            SymbolOwner::Package {
                manager: "cargo".to_owned(),
                registry: "registry.example:8443/cargo".to_owned(),
                name: "beacon".to_owned(),
            },
            SymbolOwner::Runtime {
                runtime: "rustc".to_owned(),
            },
        ];
        let mut identities = std::collections::BTreeSet::new();
        for owner in owners {
            let unit = rift_core::SourceUnitId::for_owner(owner.clone(), "src/lib.rs")
                .expect("released source unit");
            let address = unit.to_string();
            let source = DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Package {
                    unit: rift_protocol::read::SourceUnitId::parse(&address)
                        .expect("canonical source"),
                },
                cell: None,
            };
            let identity = rift_protocol::identity::SymbolIdentity::new(
                owner.clone(),
                language(),
                vec!["fixture".to_owned(), "open".to_owned()],
            )
            .expect("released symbol owner");
            let identity = SymbolId::parse(&identity.wire_identity()).expect("canonical symbol");
            assert_eq!(
                rift_protocol::identity::SymbolIdentity::parse(&identity.0)
                    .expect("canonical owner")
                    .owner(),
                &owner,
            );
            DocumentationDeclaration::new(
                &identity,
                &language(),
                "open",
                "open",
                &source,
                TextRange { start: 0, end: 20 },
            )
            .expect("declaration retains complete source owner");
            assert!(identities.insert(identity));
        }
        assert_eq!(identities.len(), 3);
    }

    #[test]
    fn test_unique_bare_and_qualified_references_resolve_exact_declarations() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = identity("rust", "fixture", "Client::open");
        let range = TextRange { start: 0, end: 100 };
        let declaration = DocumentationDeclaration::new(
            &identity,
            &language,
            "open",
            "Client::open",
            &source,
            range,
        )
        .expect("declaration");
        assert_eq!(declaration.symbol(), &identity);
        assert_eq!(declaration.source(), &source);
        assert_eq!(declaration.range().end, 100);
        let candidates = [
            candidate("open", 2),
            candidate("Client::open", 20),
            candidate("opening", 40),
        ];
        let resolved = resolve_references(&[declaration], &candidates).expect("resolution");
        assert_eq!(resolved.references().len(), 2);
        assert_eq!(
            resolved.references()[0].evidence,
            DocumentationReferenceEvidence::QualifiedName
        );
        assert_eq!(
            resolved.references()[1].evidence,
            DocumentationReferenceEvidence::UniqueName
        );
        assert_eq!(resolved.references()[0].target, identity);
        assert_eq!(resolved.unresolved().len(), 1);
        assert_eq!(
            resolved.unresolved()[0].reason,
            DocumentationUnresolvedReason::Missing
        );
    }

    #[test]
    fn test_duplicate_names_remain_unresolved() {
        let first_source = source("src/first.rs");
        let second_source = source("src/second.rs");
        let language = language();
        let first_id = identity("rust", "first", "open");
        let second_id = identity("rust", "second", "open");
        let range = TextRange { start: 0, end: 20 };
        let first = DocumentationDeclaration::new(
            &first_id,
            &language,
            "open",
            "open",
            &first_source,
            range.clone(),
        )
        .expect("first");
        let second = DocumentationDeclaration::new(
            &second_id,
            &language,
            "open",
            "open",
            &second_source,
            range,
        )
        .expect("second");
        let resolved =
            resolve_references(&[first, second], &[candidate("open", 0)]).expect("resolution");
        assert!(resolved.references().is_empty());
        assert_eq!(
            resolved.unresolved()[0].reason,
            DocumentationUnresolvedReason::Ambiguous
        );
    }

    #[test]
    fn test_declared_language_narrows_equal_names() {
        let rust_source = source("src/lib.rs");
        let python_source = source("src/main.py");
        let rust = language();
        let python = Language {
            name: "python".to_owned(),
            dialect: None,
        };
        let rust_id = identity("rust", "fixture", "open");
        let python_id = identity("python", "fixture", "open");
        let range = TextRange { start: 0, end: 20 };
        let first = DocumentationDeclaration::new(
            &rust_id,
            &rust,
            "open",
            "open",
            &rust_source,
            range.clone(),
        )
        .expect("rust");
        let second = DocumentationDeclaration::new(
            &python_id,
            &python,
            "open",
            "open",
            &python_source,
            range,
        )
        .expect("python");
        let mut qualified = candidate("open", 0);
        qualified.language = Some(rust.clone());
        let resolved = resolve_references(&[first, second], &[qualified]).expect("resolution");
        assert_eq!(resolved.references()[0].target, rust_id);
        assert!(resolved.unresolved().is_empty());
    }

    #[test]
    fn test_reference_identity_survives_unrelated_text_insertion() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = identity("rust", "fixture", "open");
        let range = TextRange { start: 0, end: 20 };
        let declaration =
            DocumentationDeclaration::new(&identity, &language, "open", "open", &source, range)
                .expect("declaration");
        let before = resolve_references(
            std::slice::from_ref(&declaration),
            &[candidate("open", 1), candidate("open", 10)],
        )
        .expect("before");
        let after = resolve_references(
            &[declaration],
            &[candidate("open", 11), candidate("open", 20)],
        )
        .expect("after");
        assert_ne!(
            before.references()[0].identity,
            before.references()[1].identity
        );
        let identities = |resolved: &super::ResolvedDocumentationReferences| {
            resolved
                .references()
                .iter()
                .map(|reference| reference.identity.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(&before), identities(&after));
        assert_ne!(before.references()[0].range, after.references()[0].range);
    }

    #[test]
    fn test_mismatched_symbol_identity_is_refused() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "close"));
        let range = TextRange { start: 0, end: 20 };
        let error =
            DocumentationDeclaration::new(&identity, &language, "open", "open", &source, range)
                .expect_err("mismatch");
        assert_eq!(
            crate::documentation::failure::violation(&error),
            DocumentationViolation::Identity
        );
    }

    #[test]
    fn test_empty_and_invalid_candidates_do_not_create_references() {
        let empty = resolve_references(&[], &[]).expect("empty");
        assert!(empty.references().is_empty());
        let error = resolve_references(&[], &[candidate("", 0)]).expect_err("empty spelling");
        assert_eq!(
            crate::documentation::failure::violation(&error),
            DocumentationViolation::Identity
        );
    }

    #[test]
    fn test_declaration_name_range_and_candidate_count_bounds() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = identity("rust", "fixture", "open");
        let control_name = DocumentationDeclaration::new(
            &identity,
            &language,
            "open\n",
            "open",
            &source,
            TextRange { start: 0, end: 1 },
        )
        .expect_err("control character in declaration name");
        assert_eq!(
            crate::documentation::failure::context_value(&control_name, "field").as_deref(),
            Some("declaration.name")
        );

        let reversed_range = DocumentationDeclaration::new(
            &identity,
            &language,
            "open",
            "open",
            &source,
            TextRange { start: 2, end: 1 },
        )
        .expect_err("reversed declaration range");
        assert_eq!(
            crate::documentation::failure::context_value(&reversed_range, "field").as_deref(),
            Some("declaration.range")
        );

        let maximum = super::DOCUMENTATION_REFERENCES_CEILING as usize;
        assert!(super::validate_reference_counts(0, maximum).is_ok());
        let error = super::validate_reference_counts(0, maximum + 1).expect_err("candidate bound");
        assert_eq!(
            crate::documentation::failure::context_value(&error, "field").as_deref(),
            Some("references")
        );
        let maximum = super::PACKAGE_SYMBOLS_CEILING as usize;
        assert!(super::validate_reference_counts(maximum, 0).is_ok());
        assert!(super::validate_reference_counts(maximum + 1, 0).is_err());
    }
}
