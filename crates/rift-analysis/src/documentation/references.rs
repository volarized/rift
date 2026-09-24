//! Exact declaration matching for authored inline code candidates.

use std::collections::BTreeMap;

use rift_core::symbol_identity;
use rift_protocol::documentation::{
    DOCUMENTATION_REFERENCES_MAX, DOCUMENTATION_TEXT_BYTES_MAX, DocumentationContentIdentity,
    DocumentationReference, DocumentationReferenceCandidate, DocumentationReferenceEvidence,
    DocumentationSourceIdentity, DocumentationUnresolvedReason,
};
use rift_protocol::index::PACKAGE_SYMBOLS_MAX;
use rift_protocol::read::{Language, SymbolId, TextRange};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::canonical_digest;
use super::input::{source_path, validate_identity};

/// A borrowed declaration's identity and name-resolution facts.
#[derive(Clone, Debug)]
pub struct DocumentationDeclaration<'declaration> {
    symbol: &'declaration SymbolId,
    language: &'declaration Language,
    name: &'declaration str,
    qualified_name: &'declaration str,
    source: DocumentationContentIdentity,
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
    ) -> Result<Self, DocumentationError> {
        validate_identity(source)?;
        let accepted_name = |text: &str| {
            !text.is_empty()
                && text.len() <= DOCUMENTATION_TEXT_BYTES_MAX as usize
                && !text.chars().any(char::is_control)
        };
        if !accepted_name(name) || !accepted_name(qualified_name) {
            return Err(refused(
                DocumentationViolation::Identity,
                "declaration.name",
            ));
        }
        let path = declaration_path(source)?;
        let expected = symbol_identity(&language.identity_segment(), &path, qualified_name);
        if symbol.0 != expected {
            return Err(refused(
                DocumentationViolation::Identity,
                "declaration.symbol",
            ));
        }
        if range.end < range.start {
            return Err(refused(DocumentationViolation::Range, "declaration.range"));
        }
        Ok(Self {
            symbol,
            language,
            name,
            qualified_name,
            source: source.clone(),
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
        &self.source
    }

    /// Returns the declaration range in its source.
    #[must_use]
    pub const fn range(&self) -> &TextRange {
        &self.range
    }
}

pub(super) fn declaration_path(
    source: &DocumentationContentIdentity,
) -> Result<String, DocumentationError> {
    let path = source_path(source)?;
    match &source.source {
        DocumentationSourceIdentity::Project { .. } => Ok(path),
        DocumentationSourceIdentity::Package { unit } => {
            let parsed = rift_core::SourceUnitId::parse(&unit.0)
                .map_err(|_| refused(DocumentationViolation::Identity, "source.unit"))?;
            Ok(format!("{}/{path}", parsed.resolver()))
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
) -> Result<ResolvedDocumentationReferences, DocumentationError> {
    if declarations.len() > PACKAGE_SYMBOLS_MAX as usize
        || candidates.len() > DOCUMENTATION_REFERENCES_MAX as usize
    {
        return Err(refused(DocumentationViolation::LimitExceeded, "references"));
    }
    let index = DeclarationNames::new(declarations);
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

pub(super) fn validate_candidate(
    candidate: &DocumentationReferenceCandidate,
) -> Result<(), DocumentationError> {
    let spelling_accepted = !candidate.authored.is_empty()
        && candidate.authored.len() <= DOCUMENTATION_TEXT_BYTES_MAX as usize;
    let range_accepted = candidate.range.end > candidate.range.start;
    if !spelling_accepted || !range_accepted || !super::identity::is_digest(&candidate.block) {
        return Err(refused(DocumentationViolation::Identity, "reference"));
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
    pub(super) fn new(declarations: &'declaration [DocumentationDeclaration<'_>]) -> Self {
        let mut index = Self {
            qualified: BTreeMap::new(),
            bare: BTreeMap::new(),
        };
        for declaration in declarations {
            index.insert(declaration, None);
            index.insert(declaration, Some(language_key(declaration.language)));
        }
        index
    }

    fn insert(
        &mut self,
        declaration: &'declaration DocumentationDeclaration<'_>,
        language: Option<LanguageKey<'declaration>>,
    ) {
        add_name(
            &mut self.bare,
            (declaration.name, language),
            declaration.symbol,
        );
        if declaration.qualified_name != declaration.name {
            add_name(
                &mut self.qualified,
                (declaration.qualified_name, language),
                declaration.symbol,
            );
        }
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
    fn test_unique_bare_and_qualified_references_resolve_exact_declarations() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = SymbolId(rift_core::symbol_identity(
            "rust",
            "src/lib.rs",
            "Client::open",
        ));
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
        let first_id = SymbolId(rift_core::symbol_identity("rust", "src/first.rs", "open"));
        let second_id = SymbolId(rift_core::symbol_identity("rust", "src/second.rs", "open"));
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
        let rust_id = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "open"));
        let python_id = SymbolId(rift_core::symbol_identity("python", "src/main.py", "open"));
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
        let identity = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "open"));
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
        assert_eq!(error.fault().violation(), DocumentationViolation::Identity);
    }

    #[test]
    fn test_empty_and_invalid_candidates_do_not_create_references() {
        let empty = resolve_references(&[], &[]).expect("empty");
        assert!(empty.references().is_empty());
        let error = resolve_references(&[], &[candidate("", 0)]).expect_err("empty spelling");
        assert_eq!(error.fault().violation(), DocumentationViolation::Identity);
    }

    #[test]
    fn test_declaration_name_range_and_candidate_count_bounds() {
        let source = source("src/lib.rs");
        let language = language();
        let identity = SymbolId(rift_core::symbol_identity("rust", "src/lib.rs", "open"));
        let control_name = DocumentationDeclaration::new(
            &identity,
            &language,
            "open\n",
            "open",
            &source,
            TextRange { start: 0, end: 1 },
        )
        .expect_err("control character in declaration name");
        assert_eq!(control_name.fault().field(), "declaration.name");

        let reversed_range = DocumentationDeclaration::new(
            &identity,
            &language,
            "open",
            "open",
            &source,
            TextRange { start: 2, end: 1 },
        )
        .expect_err("reversed declaration range");
        assert_eq!(reversed_range.fault().field(), "declaration.range");

        let candidates =
            vec![candidate("open", 0); super::DOCUMENTATION_REFERENCES_MAX as usize + 1];
        let error = resolve_references(&[], &candidates).expect_err("candidate bound");
        assert_eq!(error.fault().field(), "references");
    }
}
